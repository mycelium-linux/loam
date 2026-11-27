//! WebSocket transport for `soil:streams`.
//!
//! One connection is one duplex session. The subprotocol (`Sec-WebSocket-Protocol`)
//! names the `soil:streams` protocol a Spore accepts, and each binary message is
//! one session frame. There is no framing beyond the WebSocket message boundary,
//! so the spore sees exactly what the peer sent.
//!
//! TLS is the host's: this module only needs a certificate and key, and refuses
//! to start with one without the other. `ws://` stays available for loopback and
//! tailnet peers that already have transport security underneath.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

use crate::stream::{StreamHub, StreamInfo, StreamSession};

const MAX_FRAME: usize = 1024 * 1024;
const DEFAULT_PROTOCOL: &str = "default";

#[derive(Clone, Debug, Default)]
pub struct Tls {
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Clone, Debug)]
pub struct WsOptions {
    pub listen: SocketAddr,
    pub tls: Option<Tls>,
    /// Subprotocols a peer may ask for. Empty accepts anything and falls back to
    /// `default`, which is what a single-purpose capture bridge wants.
    pub protocols: Vec<String>,
}

/// Accept WebSocket sessions and queue them on the hub.
pub async fn serve(hub: StreamHub, options: WsOptions) -> Result<()> {
    let listener = TcpListener::bind(options.listen)
        .await
        .with_context(|| format!("bind stream ws {}", options.listen))?;
    serve_on(listener, hub, options.tls, options.protocols).await
}

/// Serve on an already-bound listener, so a test can bind port 0 and learn it.
pub async fn serve_on(
    listener: TcpListener,
    hub: StreamHub,
    tls: Option<Tls>,
    protocols: Vec<String>,
) -> Result<()> {
    let acceptor = match tls {
        Some(tls) => Some(tls_acceptor(&tls)?),
        None => None,
    };
    let listen = listener.local_addr().ok();
    let protocols = Arc::new(protocols);
    tracing::info!(
        listen = ?listen,
        tls = acceptor.is_some(),
        "stream ws serving"
    );
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                tracing::warn!(error = %error, "stream ws accept");
                continue;
            }
        };
        let hub = hub.clone();
        let acceptor = acceptor.clone();
        let protocols = Arc::clone(&protocols);
        tokio::spawn(async move {
            let result = match acceptor {
                Some(acceptor) => match acceptor.accept(tcp).await {
                    Ok(tls) => session(hub, tls, Some(peer.to_string()), &protocols).await,
                    Err(error) => Err(anyhow::anyhow!("tls handshake: {error}")),
                },
                None => session(hub, tcp, Some(peer.to_string()), &protocols).await,
            };
            if let Err(error) = result {
                tracing::warn!(peer = %peer, error = %format!("{error:#}"), "stream ws session");
            }
        });
    }
}

fn tls_acceptor(tls: &Tls) -> Result<tokio_rustls::TlsAcceptor> {
    use rustls_pemfile::{certs, pkcs8_private_keys, rsa_private_keys};

    let certs = certs(&mut std::io::BufReader::new(
        std::fs::File::open(&tls.cert).with_context(|| format!("open {}", tls.cert.display()))?,
    ))
    .collect::<std::result::Result<Vec<_>, _>>()
    .context("read certificate")?;
    if certs.is_empty() {
        bail!("{} has no certificates", tls.cert.display());
    }
    let mut reader = std::io::BufReader::new(
        std::fs::File::open(&tls.key).with_context(|| format!("open {}", tls.key.display()))?,
    );
    let pkcs8 = pkcs8_private_keys(&mut reader).collect::<std::result::Result<Vec<_>, _>>()?;
    let key = match pkcs8.into_iter().next() {
        Some(key) => tokio_rustls::rustls::pki_types::PrivateKeyDer::Pkcs8(key),
        None => {
            let mut reader = std::io::BufReader::new(
                std::fs::File::open(&tls.key)
                    .with_context(|| format!("open {}", tls.key.display()))?,
            );
            let rsa = rsa_private_keys(&mut reader).collect::<std::result::Result<Vec<_>, _>>()?;
            let key = rsa.into_iter().next().context("private key not found")?;
            tokio_rustls::rustls::pki_types::PrivateKeyDer::Pkcs1(key)
        }
    };
    let config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("tls server config")?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

async fn session<S>(
    hub: StreamHub,
    stream: S,
    peer: Option<String>,
    protocols: &[String],
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let chosen = Arc::new(std::sync::Mutex::new(String::new()));
    let chosen_cb = Arc::clone(&chosen);
    let allowed: Vec<String> = protocols.to_vec();
    let ws = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &Request, mut response: Response| {
            let requested = request
                .headers()
                .get("sec-websocket-protocol")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| {
                    value.split(',').map(str::trim).find(|candidate| {
                        allowed.is_empty() || allowed.iter().any(|a| a == candidate)
                    })
                })
                .unwrap_or(DEFAULT_PROTOCOL)
                .to_string();
            if requested != DEFAULT_PROTOCOL {
                let _ = response.headers_mut().insert(
                    "sec-websocket-protocol",
                    requested.parse().expect("ascii protocol"),
                );
            }
            *chosen_cb.lock().expect("protocol lock") = requested;
            Ok(response)
        },
    )
    .await
    .context("websocket handshake")?;

    let protocol = chosen.lock().expect("protocol lock").clone();
    let protocol = if protocol.is_empty() {
        DEFAULT_PROTOCOL.to_string()
    } else {
        protocol
    };

    let (inbound_tx, inbound_rx) = mpsc::channel::<Vec<u8>>(16);
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Vec<u8>>(16);
    hub.attach(StreamSession::new(
        StreamInfo {
            protocol,
            peer,
            principal: None,
            media_type: Some("application/octet-stream".into()),
        },
        inbound_rx,
        outbound_tx,
    ));

    let (mut sink, mut source) = ws.split();
    loop {
        tokio::select! {
            message = source.next() => {
                match message {
                    Some(Ok(Message::Binary(data))) => {
                        if data.len() > MAX_FRAME { break; }
                        if inbound_tx.send(data.to_vec()).await.is_err() { break; }
                    }
                    Some(Ok(Message::Text(text))) => {
                        if text.len() > MAX_FRAME { break; }
                        if inbound_tx.send(text.into_bytes()).await.is_err() { break; }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
            outbound = outbound_rx.recv() => {
                match outbound {
                    Some(data) => {
                        if sink.send(Message::Binary(data.into())).await.is_err() { break; }
                    }
                    None => break,
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The subprotocol names the Spore protocol, and bytes round-trip through
    /// the hub in both directions. This is the contract `accept` relies on.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_websocket_session_is_queued_by_subprotocol_and_round_trips() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let hub = StreamHub::new();
        let served = {
            let hub = hub.clone();
            tokio::spawn(async move { serve_on(listener, hub, None, Vec::new()).await })
        };

        let request = tokio_tungstenite::tungstenite::handshake::client::Request::builder()
            .uri(format!("ws://{addr}/"))
            .header("Sec-WebSocket-Protocol", "capture")
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
            .header("Host", addr.to_string())
            .body(())
            .expect("request");
        let (mut ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("connect");

        let mut session = None;
        for _ in 0..100 {
            if let Some(found) = hub.accept("capture") {
                session = Some(found);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let session = session.expect("session queued under the subprotocol");
        assert_eq!(session.info.protocol, "capture");

        ws.send(Message::Binary(b"hello".to_vec().into()))
            .await
            .expect("send");
        // StreamSession is the blocking seam wasmtime calls; read it off the
        // async runtime rather than inside it.
        let (session, received) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::task::spawn_blocking(move || {
                let mut session = session;
                let bytes = session.read(1024).expect("read");
                (session, bytes)
            }),
        )
        .await
        .expect("read timeout")
        .expect("join");
        assert_eq!(received.as_deref(), Some(&b"hello"[..]));

        let session = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::task::spawn_blocking(move || {
                let mut session = session;
                session.write(b"world").expect("write");
                session
            }),
        )
        .await
        .expect("write timeout")
        .expect("join");
        let _ = session;
        let echoed = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .expect("timeout")
            .expect("frame")
            .expect("ok");
        assert_eq!(echoed, Message::Binary(b"world".to_vec().into()));

        served.abort();
    }
}
