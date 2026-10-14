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
