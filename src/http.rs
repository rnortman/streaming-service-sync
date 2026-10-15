//! The HTTP transport the service clients share: the agent, request
//! pacing, a reply read whatever its status, base URLs taken from the
//! environment, and the stderr warning line. Retry policy is each client's.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

/// How long one request may take, connect to last byte.
pub const TIMEOUT: Duration = Duration::from_secs(60);

/// An agent with [`TIMEOUT`].
pub fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(TIMEOUT).build()
}

/// `$var` when set and non-empty, else `default`; without a trailing `/`.
pub fn base_url_from_env(var: &str, default: &str) -> String {
    base_url(std::env::var(var).ok(), default)
}

fn base_url(value: Option<String>, default: &str) -> String {
    value
        .filter(|v| !v.is_empty())
        .as_deref()
        .unwrap_or(default)
        .trim_end_matches('/')
        .to_owned()
}

/// Spaces request starts at least `min_interval` apart.
#[derive(Debug)]
pub struct Pacer {
    min_interval: Duration,
    last_start: Option<Instant>,
}

impl Pacer {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last_start: None,
        }
    }

    /// Sleeps until `min_interval` has passed since the previous request
    /// started.
    pub fn pace(&mut self) {
        if let Some(last) = self.last_start {
            let elapsed = last.elapsed();
            if elapsed < self.min_interval {
                std::thread::sleep(self.min_interval - elapsed);
            }
        }
        self.last_start = Some(Instant::now());
    }
}

/// A response, whatever its status.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    /// The `Retry-After` header.
    pub retry_after: Option<String>,
    /// The body, invalid UTF-8 replaced.
    pub body: String,
}

/// The reply of one request's `result`: `Err` only when no response came
/// or its body could not be read.
pub fn read(result: Result<ureq::Response, ureq::Error>) -> Result<Reply> {
    let response = match result {
        Ok(r) | Err(ureq::Error::Status(_, r)) => r,
        Err(ureq::Error::Transport(t)) => return Err(anyhow!(t)),
    };
    let status = response.status();
    let retry_after = response.header("Retry-After").map(str::to_owned);
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .context("reading the response")?;
    Ok(Reply {
        status,
        retry_after,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// Writes `line` to stderr; a failed write is ignored.
pub fn print_warning(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    #[test]
    fn base_urls_fall_back_on_unset_or_empty_and_lose_the_trailing_slash() {
        assert_eq!(base_url(None, "https://a.example/"), "https://a.example");
        assert_eq!(
            base_url(Some(String::new()), "https://a.example"),
            "https://a.example"
        );
        assert_eq!(
            base_url(Some("http://127.0.0.1:9/v1/".into()), "https://a.example"),
            "http://127.0.0.1:9/v1"
        );
    }

    #[test]
    fn a_reply_is_read_whatever_its_status() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/busy");
            then.status(429)
                .header("Retry-After", "7")
                .body(b"no \xff".as_slice());
        });
        let reply = read(agent().get(&server.url("/busy")).call()).unwrap();
        assert_eq!(reply.status, 429);
        assert_eq!(reply.retry_after.as_deref(), Some("7"));
        assert_eq!(reply.body, "no \u{fffd}");
    }

    #[test]
    fn no_response_is_an_error() {
        let err = read(agent().get("http://127.0.0.1:9/").call()).unwrap_err();
        assert!(err.downcast_ref::<ureq::Transport>().is_some(), "{err:#}");
    }

    #[test]
    fn requests_are_spaced_by_the_minimum_interval() {
        let mut pacer = Pacer::new(Duration::from_millis(40));
        let start = Instant::now();
        pacer.pace();
        pacer.pace();
        pacer.pace();
        assert!(start.elapsed() >= Duration::from_millis(80));
    }
}
