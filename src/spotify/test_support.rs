//! Test fixtures shared by every module that tests against the Spotify
//! client: zero pacing and a client whose stored token is valid.

use std::path::Path;
use std::time::Duration;

use httpmock::MockServer;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::secrets::StateDir;
use crate::spotify::auth::Token;
use crate::spotify::client::{Spotify, Timing};

/// No pacing, no settle, and 1 ms backoffs.
pub const FAST: Timing = Timing {
    min_interval: Duration::ZERO,
    first_backoff: Duration::from_millis(1),
    retry_after_unit: Duration::from_millis(1),
    settle: Duration::ZERO,
};

/// `seconds` from now, as a token's `expires_at`.
pub fn in_seconds(seconds: i64) -> String {
    (OffsetDateTime::now_utc() + time::Duration::seconds(seconds))
        .format(&Rfc3339)
        .unwrap()
}

/// A token valid for an hour, granted `scopes`.
pub fn fresh_token(scopes: &[&str]) -> Token {
    Token {
        access_token: "access-secret".into(),
        refresh_token: "refresh-secret".into(),
        expires_at: in_seconds(3600),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
    }
}

/// Profile `alice`'s state directory under `tmp`, holding `token`.
pub fn state_with(tmp: &Path, token: &Token) -> StateDir {
    let state = StateDir::under(tmp, "alice").unwrap();
    state.save_spotify_token(token).unwrap();
    state
}

/// A [`FAST`] client against `server` whose stored token is valid for an
/// hour and was granted `scopes`.
pub fn client(tmp: &Path, server: &MockServer, scopes: &[&str]) -> Spotify {
    Spotify::new(
        state_with(tmp, &fresh_token(scopes)),
        "test-client-id",
        "",
        &server.url("/v1"),
        &server.base_url(),
        FAST,
    )
    .unwrap()
}
