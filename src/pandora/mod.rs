//! Pandora: the web API client and the `sss pandora` commands.

pub mod auth;
pub mod client;
pub mod export;
pub mod model;

#[cfg(test)]
pub(crate) mod test_support {
    use std::time::Duration;

    use crate::pandora::client::Timing;
    use crate::secrets::{PandoraSession, TokenSource};

    /// No pacing and a 1 ms first backoff.
    pub const FAST: Timing = Timing {
        min_interval: Duration::ZERO,
        first_backoff: Duration::from_millis(1),
    };

    /// A writer whose reader has gone: every write fails with `BrokenPipe`.
    pub struct ClosedPipe;

    impl std::io::Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
    }

    /// A session with fake credentials.
    pub fn session() -> PandoraSession {
        PandoraSession {
            auth_token: "tok-secret-value".into(),
            csrf_token: "0123456789abcdef".into(),
            webname: Some("listener".into()),
            source: TokenSource::Prompt,
            stored_at: "2026-10-13T00:00:00Z".into(),
        }
    }
}
