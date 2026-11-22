//! Host-issued byte streams.
//!
//! The host owns the listener, TLS, authentication and quota. A Spore polls
//! `accept` and then reads/writes a bounded duplex session. The guest never
//! sees a socket, file descriptor, TLS key or peer credential.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use tokio::sync::mpsc;
use wasmtime::component::{HasSelf, Linker};

wasmtime::component::bindgen!({
    path: "wit/stream",
    world: "host",
    with: {
        "soil:streams/duplex.session": StreamSession,
    },
});

pub use self::soil::streams::duplex;

const MAX_READ: u32 = 1024 * 1024;
const MAX_WRITE: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub protocol: String,
    pub peer: Option<String>,
    pub principal: Option<String>,
    pub media_type: Option<String>,
}

pub struct StreamSession {
    pub(crate) info: StreamInfo,
    inbound: mpsc::Receiver<Vec<u8>>,
    outbound: mpsc::Sender<Vec<u8>>,
    pending: Vec<u8>,
    closed: bool,
}

impl StreamSession {
    pub fn new(
        info: StreamInfo,
        inbound: mpsc::Receiver<Vec<u8>>,
        outbound: mpsc::Sender<Vec<u8>>,
    ) -> Self {
        Self {
            info,
            inbound,
            outbound,
            pending: Vec::new(),
            closed: false,
        }
    }

    pub(crate) fn read(&mut self, max: u32) -> Result<Option<Vec<u8>>, duplex::Error> {
        if max == 0 || max > MAX_READ {
            return Err(duplex::Error::OverBudget);
        }
        if self.closed {
            return Ok(None);
        }
        if self.pending.is_empty() {
            match self.inbound.blocking_recv() {
                Some(bytes) => self.pending = bytes,
                None => {
                    self.closed = true;
                    return Ok(None);
                }
            }
        }
        let take = self.pending.len().min(max as usize);
        let out = self.pending.drain(..take).collect();
        Ok(Some(out))
    }

    pub(crate) fn write(&mut self, data: &[u8]) -> Result<u64, duplex::Error> {
        if self.closed {
            return Err(duplex::Error::Closed);
        }
        if data.is_empty() {
            return Ok(0);
        }
        if data.len() > MAX_WRITE {
            return Err(duplex::Error::OverBudget);
        }
        self.outbound
            .blocking_send(data.to_vec())
            .map_err(|_| duplex::Error::Closed)?;
        Ok(data.len() as u64)
    }

    pub(crate) fn close(&mut self) -> Result<(), duplex::Error> {
        self.closed = true;
        self.pending.clear();
        Ok(())
    }
}

/// A bounded queue of sessions the host has accepted but a Spore has not.
#[derive(Clone, Default)]
pub struct StreamHub {
    queue: Arc<Mutex<VecDeque<StreamSession>>>,
}

impl StreamHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn attach(&self, session: StreamSession) {
        self.queue
            .lock()
            .expect("stream hub poisoned")
            .push_back(session);
    }

    pub(crate) fn accept(&self, protocol: &str) -> Option<StreamSession> {
        let mut queue = self.queue.lock().expect("stream hub poisoned");
        let index = queue
            .iter()
            .position(|session| session.info.protocol == protocol)?;
        queue.remove(index)
    }

    pub fn pending(&self) -> usize {
        self.queue.lock().expect("stream hub poisoned").len()
    }
}

pub fn add_to_linker<T>(linker: &mut Linker<T>) -> anyhow::Result<()>
where
    T: duplex::Host + duplex::HostSession + 'static,
{
    duplex::add_to_linker::<T, HasSelf<T>>(linker, |state| state)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_round_trips_bytes_and_queues_by_protocol() {
        let hub = StreamHub::new();
        let (inbound_tx, inbound_rx) = mpsc::channel(4);
        let (outbound_tx, mut outbound_rx) = mpsc::channel(4);
        inbound_tx.try_send(b"hello".to_vec()).unwrap();
        hub.attach(StreamSession::new(
            StreamInfo {
                protocol: "wss".into(),
                peer: Some("test".into()),
                principal: None,
                media_type: None,
            },
            inbound_rx,
            outbound_tx,
        ));
        assert_eq!(hub.pending(), 1);
        let mut session = hub.accept("wss").expect("session");
        assert_eq!(session.read(3).unwrap(), Some(b"hel".to_vec()));
        assert_eq!(session.read(8).unwrap(), Some(b"lo".to_vec()));
        session.write(b"world").unwrap();
        assert_eq!(outbound_rx.blocking_recv().unwrap(), b"world");
        session.close().unwrap();
        assert_eq!(session.read(8).unwrap(), None);
    }
}
