//! End-to-end fixture for `soil:streams`.
//!
//! A command's entry point is `wasi:cli/run`, so that is what this exports. It
//! polls `accept` (a poll, not a listener) until the test dials in, echoes each
//! frame back prefixed with `echo:`, and closes when the peer does.

wit_bindgen::generate!({
    path: "wit",
    world: "echo",
    generate_all,
});

use soil::streams::duplex;

struct Component;

impl exports::wasi::cli::run::Guest for Component {
    fn run() -> Result<(), ()> {
        // No clock in the world, so spin with a bound: long enough for the test
        // to dial, short enough that a broken test fails rather than hangs.
        let mut session = None;
        for _ in 0..5_000_000 {
            match duplex::accept("capture") {
                Ok(Some(found)) => {
                    session = Some(found);
                    break;
                }
                Ok(None) => continue,
                Err(_) => return Err(()),
            }
        }
        let Some(mut session) = session else {
            return Err(());
        };
        loop {
            match session.read(4096) {
                Ok(Some(bytes)) if !bytes.is_empty() => {
                    let mut reply = b"echo:".to_vec();
                    reply.extend_from_slice(&bytes);
                    session.write(&reply).map_err(|_| ())?;
                }
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => return Err(()),
            }
        }
        session.close().map_err(|_| ())?;
        Ok(())
    }
}

export!(Component);
