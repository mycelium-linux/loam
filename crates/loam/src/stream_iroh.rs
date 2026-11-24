//! Iroh (QUIC) transport for `soil:streams`.
//!
//! One bidirectional QUIC stream is one session. The first line on the stream is
//! the `soil:streams` protocol (UTF-8, no newline, at most 64 bytes); everything
//! after it is payload. Putting the protocol in-band rather than in the ALPN
//! lets one connection carry sessions for several protocols, which is what a
//! capture bridge wants: one dial, several named streams.
//!
//! Identity is the peer's endpoint id. It is an attestation, not an
//! authorization: `allow` is the host's policy and an empty list still means
//! "anyone who knows this endpoint id", not "anyone on the network".

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use iroh::{Endpoint, EndpointId, SecretKey, endpoint::presets};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;

use crate::stream::{StreamHub, StreamInfo, StreamSession};

const MAX_FRAME: usize = 1024 * 1024;
const MAX_PROTOCOL: usize = 64;

#[derive(Clone, Debug)]
pub struct IrohOptions {
    pub bind: SocketAddr,
    /// ALPNs this endpoint answers on. A client picks one; the protocol line on
    /// the stream still decides which Spore session it becomes.
    pub alpns: Vec<String>,
    /// Endpoint secret key. Created with 0600 permissions when absent, so the
    /// endpoint id (and any lease that names it) survives a restart.
    pub key: Option<PathBuf>,
    /// Peers allowed to dial. Empty accepts any peer that knows the id.
    pub allow: Vec<EndpointId>,
}

pub async fn serve(hub: StreamHub, options: IrohOptions) -> Result<()> {
    let mut builder = Endpoint::builder(presets::Minimal)
        .alpns(options.alpns.iter().map(|alpn| alpn.as_bytes().to_vec()).collect())
        .relay_mode(iroh::RelayMode::Disabled)
        .bind_addr(options.bind)
        .context("invalid iroh bind address")?;
    if let Some(path) = options.key.as_deref() {
        builder = builder.secret_key(load_or_create_key(path)?);
    }
    let endpoint = builder.bind().await.context("bind iroh endpoint")?;
    serve_endpoint(endpoint, hub, options.allow).await
}

/// Serve on an already-bound endpoint, so a test can learn its address.
pub async fn serve_endpoint(
    endpoint: Endpoint,
    hub: StreamHub,
    allow: Vec<EndpointId>,
) -> Result<()> {
    tracing::info!(endpoint = %endpoint.id(), "stream iroh serving");
    while let Some(incoming) = endpoint.accept().await {
        let hub = hub.clone();
        let allow = allow.clone();
        tokio::spawn(async move {
            if let Err(error) = connection(hub, incoming, &allow).await {
                tracing::warn!(error = %format!("{error:#}"), "stream iroh connection");
            }
        });
    }
    Ok(())
}

async fn connection(
    hub: StreamHub,
    incoming: iroh::endpoint::Incoming,
    allow: &[EndpointId],
) -> Result<()> {
    let connection = incoming.await.context("complete iroh handshake")?;
    let peer = connection.remote_id();
    if !allow.is_empty() && !allow.contains(&peer) {
        bail!("peer {peer} is not in the allow list");
    }
    loop {
        let (send, recv) = match connection.accept_bi().await {
            Ok(stream) => stream,
            Err(error) => {
                tracing::debug!(peer = %peer, error = %error, "iroh stream closed");
                return Ok(());
            }
        };
        let hub = hub.clone();
        tokio::spawn(async move {
            if let Err(error) = session(hub, send, recv, peer.to_string()).await {
                tracing::warn!(peer = %peer, error = %format!("{error:#}"), "stream iroh session");
            }
        });
    }
}

async fn session(
    hub: StreamHub,
    mut send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
    peer: String,
) -> Result<()> {
    let mut reader = BufReader::new(recv);
    let mut line = Vec::new();
    let read = reader
        .read_until(b'\n', &mut line)
        .await
        .context("read protocol line")?;
    ensure!(read > 0, "stream closed before the protocol line");
    ensure!(line.len() <= MAX_PROTOCOL + 1, "protocol line too long");
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    let protocol = String::from_utf8(line).context("protocol line is not UTF-8")?;

    let (inbound_tx, inbound_rx) = mpsc::channel::<Vec<u8>>(16);
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Vec<u8>>(16);
    hub.attach(StreamSession::new(
        StreamInfo {
            protocol,
            peer: Some(peer),
            principal: None,
            media_type: Some("application/octet-stream".into()),
        },
        inbound_rx,
        outbound_tx,
    ));

    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        tokio::select! {
            read = reader.read(&mut chunk) => {
                match read {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = chunk[..n].to_vec();
                        if inbound_tx.send(data).await.is_err() { break; }
                    }
                    Err(_) => break,
                }
            }
            outbound = outbound_rx.recv() => {
                match outbound {
                    Some(data) => {
                        if data.len() > MAX_FRAME { break; }
                        if send.write_all(&data).await.is_err() { break; }
                    }
                    None => break,
                }
            }
        }
    }
    let _ = send.finish();
    Ok(())
}

fn load_or_create_key(path: &Path) -> Result<SecretKey> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let bytes = hex::decode(text.trim()).context("decode endpoint key")?;
        ensure!(bytes.len() == 32, "endpoint key must be 32 bytes");
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        return Ok(SecretKey::from_bytes(&key));
    }
    let key = SecretKey::generate();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, hex::encode(key.to_bytes()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn endpoint() -> Endpoint {
        Endpoint::builder(presets::Minimal)
            .alpns(vec![b"soil/streams/1".to_vec()])
            .relay_mode(iroh::RelayMode::Disabled)
            .bind_addr("127.0.0.1:0".parse::<SocketAddr>().expect("addr"))
            .expect("bind addr")
            .bind()
            .await
            .expect("bind")
    }

    /// A dialed bi-stream carries its protocol line, then bytes; the hub queues
    /// it under that protocol, with the peer's endpoint id.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_iroh_stream_carries_its_protocol_line() {
        let server = endpoint().await;
        let addr = iroh::EndpointAddr::new(server.id()).with_addrs(
            server
                .bound_sockets()
                .into_iter()
                .map(iroh::TransportAddr::Ip),
        );
        let hub = StreamHub::new();
        {
            let hub = hub.clone();
            tokio::spawn(async move { serve_endpoint(server, hub, Vec::new()).await });
        }

        let client = endpoint().await;
        let connection = client
            .connect(addr, b"soil/streams/1")
            .await
            .expect("connect");
        let (mut send, mut recv) = connection.open_bi().await.expect("open_bi");
        send.write_all(b"capture\n").await.expect("protocol");
        send.write_all(b"hello").await.expect("payload");

        let mut session = None;
        for _ in 0..100 {
            if let Some(found) = hub.accept("capture") {
                session = Some(found);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let session = session.expect("session queued under protocol line");
        assert!(session.info.peer.is_some(), "peer is the endpoint id");

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

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::task::spawn_blocking(move || {
                let mut session = session;
                session.write(b"world").expect("write");
            }),
        )
        .await
        .expect("write timeout")
        .expect("join");

        let back = recv.read_to_end(1024).await.expect("read back");
        assert_eq!(back, b"world");
    }
}
