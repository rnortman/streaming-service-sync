//! The Pandora web API client: `POST <base>/api/<path>` with a JSON body,
//! the session's `X-AuthToken`, and a CSRF header/cookie pair. Requests are
//! paced, retried on 429 and outage-like 5xx, recorded raw through a
//! caller-supplied sink before anything inspects them, and classified as
//! success, a failed step, or a rejected session.

use std::fmt;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::Serialize;
use serde_json::Value;
use serde_json::value::RawValue;

use crate::error::{Kind, WithKind};
use crate::http::{self, Pacer};
use crate::repo;
use crate::secrets::PandoraSession;

/// The live API origin.
pub const DEFAULT_BASE_URL: &str = "https://www.pandora.com";

/// Environment variable that replaces [`DEFAULT_BASE_URL`] (used to point
/// the binary at a test server).
pub const BASE_URL_ENV: &str = "SSS_PANDORA_BASE_URL";

/// Attempts per request, the first included, for retryable responses.
pub const MAX_TRIES: u32 = 5;

/// The API paths `sss` requests; the same strings are each raw exchange's
/// `path`, which normalization dispatches on.
pub mod endpoint {
    pub const GET_STATIONS: &str = "v1/station/getStations";
    pub const GET_STATION_DETAILS: &str = "v1/station/getStationDetails";
    pub const GET_STATION_FEEDBACK: &str = "v1/station/getStationFeedback";
    pub const GET_SORTED_PLAYLISTS: &str = "v6/collections/getSortedPlaylists";
    pub const GET_PLAYLIST_TRACKS: &str = "v7/playlists/getTracks";
    pub const GET_COLLECTION_ITEMS: &str = "v6/collections/getItems";
    pub const ANNOTATE_OBJECTS: &str = "v4/catalog/annotateObjects";
    pub const GET_BOOKMARKS: &str = "v1/bookmark/getBookmarks";
}

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

/// Request pacing and retry backoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Minimum time between the starts of two requests.
    pub min_interval: Duration,
    /// Wait before the second attempt; doubled before each further one.
    pub first_backoff: Duration,
}

impl Timing {
    /// At most 2 requests/s; backoff 1 s, 2 s, 4 s, 8 s.
    pub const LIVE: Timing = Timing {
        min_interval: Duration::from_millis(500),
        first_backoff: Duration::from_secs(1),
    };
}

/// One request and its response, as stored in a raw run. It holds no
/// headers, so no credential ever reaches it.
#[derive(Debug, Serialize)]
pub struct Exchange {
    /// When the request was sent, RFC 3339 UTC.
    pub at: String,
    /// The API path, e.g. `v1/station/getStations`.
    pub path: String,
    /// The JSON body sent.
    pub request: Value,
    /// The HTTP status.
    pub status: u16,
    /// The response body verbatim when it is JSON, else the body as a JSON
    /// string.
    pub response: Box<RawValue>,
}

/// The outcome of a request that did not reject the session.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// A 2xx JSON response without an `errorCode`.
    Ok(Value),
    /// Pandora refused the request; the text says how. The step the
    /// request belongs to has failed.
    Failed(String),
    /// Every attempt got a retryable answer (HTTP 429, or a 5xx without an
    /// `errorCode`); the text describes the last one. The step the request
    /// belongs to has failed.
    Unavailable(String),
}

/// Pandora no longer accepts the session's token (HTTP 401/403 or
/// `errorCode` 1001). The run cannot continue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRejected {
    pub detail: String,
}

impl fmt::Display for SessionRejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Pandora rejected the session ({}); re-run `sss pandora auth`",
            self.detail
        )
    }
}

impl std::error::Error for SessionRejected {}

/// How one response is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Success,
    Retry,
    Stop,
    Fail,
}

/// The `errorCode` of a JSON object body, when it has that key.
fn error_code(body: Option<&Value>) -> Option<&Value> {
    body.and_then(Value::as_object)
        .and_then(|o| o.get("errorCode"))
}

fn classify(status: u16, body: Option<&Value>) -> Class {
    let code = error_code(body);
    if status == 401 || status == 403 || code.and_then(Value::as_i64) == Some(1001) {
        Class::Stop
    } else if status == 429 || ((500..600).contains(&status) && code.is_none()) {
        Class::Retry
    } else if !(200..300).contains(&status) || code.is_some() || body.is_none() {
        Class::Fail
    } else {
        Class::Success
    }
}

/// Whether a stored exchange (`status`, and `response` as written to a raw
/// run) was a successful reply. A non-JSON body is stored as a JSON string,
/// so a string `response` counts as not JSON.
pub fn succeeded(status: u16, response: &Value) -> bool {
    !response.is_string() && classify(status, Some(response)) == Class::Success
}

/// `HTTP <status>` plus whatever error fields the body carries.
fn describe(status: u16, body: Option<&Value>) -> String {
    let mut text = format!("HTTP {status}");
    let Some(obj) = body.and_then(Value::as_object) else {
        if body.is_none() {
            text.push_str(", response is not JSON");
        }
        return text;
    };
    if let Some(code) = obj.get("errorCode") {
        text.push_str(&format!(", errorCode {code}"));
    }
    for key in ["errorString", "message"] {
        if let Some(Value::String(s)) = obj.get(key) {
            text.push_str(&format!(", {key} {s:?}"));
        }
    }
    text
}

/// The stderr line for an attempt at `path` that got no HTTP response and
/// will be retried.
fn transport_warning(path: &str, attempt: u32, error: &anyhow::Error) -> String {
    format!(
        "warning: POST {path}: no response on attempt {attempt}/{MAX_TRIES} ({error:#}); retrying"
    )
}

/// A paced Pandora API client for one session.
pub struct Client {
    agent: ureq::Agent,
    base: String,
    auth_token: String,
    csrf_token: String,
    timing: Timing,
    pacer: Pacer,
    requests: u32,
    /// Receives each warning line; [`http::print_warning`] writes it to
    /// stderr.
    warn: fn(&str),
}

impl Client {
    /// A client for `base_url` (an origin such as [`DEFAULT_BASE_URL`]).
    pub fn new(base_url: &str, session: &PandoraSession, timing: Timing) -> Self {
        Self {
            agent: http::agent(),
            base: base_url.trim_end_matches('/').to_owned(),
            auth_token: session.auth_token.clone(),
            csrf_token: session.csrf_token.clone(),
            timing,
            pacer: Pacer::new(timing.min_interval),
            requests: 0,
            warn: http::print_warning,
        }
    }

    /// A live-paced client for `$SSS_PANDORA_BASE_URL`, else the live API.
    pub fn from_env(session: &PandoraSession) -> Self {
        let base = http::base_url_from_env(BASE_URL_ENV, DEFAULT_BASE_URL);
        Self::new(&base, session, Timing::LIVE)
    }

    /// Responses received so far, retried attempts included.
    pub fn requests(&self) -> u32 {
        self.requests
    }

    /// Sends `body` to `path` and returns the classified reply. Every
    /// response, retried ones included, is passed to `record` before it is
    /// classified. A rejected session is an error that downcasts to
    /// [`SessionRejected`]. A transport failure (no HTTP response) is
    /// retried like an outage and is an error once every attempt has failed.
    pub fn post(
        &mut self,
        path: &str,
        body: &Value,
        record: &mut dyn FnMut(&Exchange) -> Result<()>,
    ) -> Result<Reply> {
        let mut backoff = self.timing.first_backoff;
        for attempt in 1..=MAX_TRIES {
            if attempt > 1 {
                std::thread::sleep(backoff);
                backoff = backoff.saturating_mul(2);
            }
            self.pacer.pace();
            let at = repo::now_rfc3339()?;
            let (status, text) = match self.send(path, body) {
                Ok(reply) => reply,
                Err(e) if attempt < MAX_TRIES => {
                    (self.warn)(&transport_warning(path, attempt, &e));
                    continue;
                }
                Err(e) => {
                    return Err(e).with_kind(Kind::Unavailable, || {
                        format!("POST {path}: no response after {MAX_TRIES} tries")
                    });
                }
            };
            self.requests += 1;
            let (parsed, response) = http::stored_body(&text)?;
            record(&Exchange {
                at,
                path: path.to_owned(),
                request: body.clone(),
                status,
                response,
            })?;
            match classify(status, parsed.as_ref()) {
                Class::Success => return Ok(Reply::Ok(parsed.unwrap_or(Value::Null))),
                Class::Stop => {
                    return Err(anyhow!(SessionRejected {
                        detail: describe(status, parsed.as_ref()),
                    }));
                }
                Class::Fail => return Ok(Reply::Failed(describe(status, parsed.as_ref()))),
                Class::Retry if attempt < MAX_TRIES => {}
                Class::Retry => {
                    return Ok(Reply::Unavailable(format!(
                        "{} after {MAX_TRIES} tries",
                        describe(status, parsed.as_ref())
                    )));
                }
            }
        }
        unreachable!("the last attempt always returns")
    }

    /// One HTTP exchange: the status and the body text, whatever the status.
    fn send(&self, path: &str, body: &Value) -> Result<(u16, String)> {
        let url = format!("{}/api/{path}", self.base);
        let result = self
            .agent
            .post(&url)
            .set("Content-Type", "application/json;charset=utf-8")
            .set("Accept", "application/json, text/plain, */*")
            .set("User-Agent", USER_AGENT)
            .set("Origin", &self.base)
            .set("Referer", &format!("{}/", self.base))
            .set("X-AuthToken", &self.auth_token)
            .set("X-CsrfToken", &self.csrf_token)
            .set("Cookie", &format!("csrftoken={}", self.csrf_token))
            .send_string(&body.to_string());
        let reply = http::read(result)?;
        Ok((reply.status, reply.body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pandora::test_support::{FAST, session};
    use httpmock::prelude::*;
    use serde_json::json;
    use std::time::Instant;

    /// Posts once to `path` on `server`, returning the reply and the
    /// serialized exchanges recorded.
    fn post(server: &MockServer, path: &str, body: Value) -> (Result<Reply>, Vec<String>) {
        let mut client = Client::new(&server.base_url(), &session(), FAST);
        let mut seen = Vec::new();
        let reply = client.post(path, &body, &mut |ex| {
            seen.push(serde_json::to_string(ex)?);
            Ok(())
        });
        (reply, seen)
    }

    fn rejected(reply: Result<Reply>) -> SessionRejected {
        reply
            .unwrap_err()
            .downcast::<SessionRejected>()
            .expect("a SessionRejected error")
    }

    #[test]
    fn success_sends_the_credentials_and_records_the_exchange() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/v1/station/getStations")
                .header("X-AuthToken", "tok-secret-value")
                .header("X-CsrfToken", "0123456789abcdef")
                .header("Cookie", "csrftoken=0123456789abcdef")
                .json_body(json!({"pageSize": 1}));
            then.status(200)
                .body(r#"{"totalStations": 2, "stations": [{"z": 1, "a": 2}]}"#);
        });
        let (reply, seen) = post(&server, "v1/station/getStations", json!({"pageSize": 1}));
        mock.assert_calls(1);
        assert_eq!(
            reply.unwrap(),
            Reply::Ok(json!({"totalStations": 2, "stations": [{"z": 1, "a": 2}]}))
        );
        assert_eq!(seen.len(), 1);
        let ex: Value = serde_json::from_str(&seen[0]).unwrap();
        assert_eq!(ex["path"], "v1/station/getStations");
        assert_eq!(ex["request"], json!({"pageSize": 1}));
        assert_eq!(ex["status"], 200);
        // Stored verbatim: the response's own key order survives.
        assert!(
            seen[0].contains(r#""response":{"totalStations": 2, "stations": [{"z": 1, "a": 2}]}"#),
            "{}",
            seen[0]
        );
        assert!(!seen[0].contains("tok-secret-value"));
        assert!(!seen[0].contains("0123456789abcdef"));
    }

    #[test]
    fn http_401_and_403_reject_the_session() {
        for status in [401, 403] {
            let server = MockServer::start();
            let mock = server.mock(|_, then| {
                then.status(status)
                    .json_body(json!({"errorCode": 1001, "errorString": "INVALID_REQUEST"}));
            });
            let (reply, seen) = post(&server, "v1/x", json!({}));
            let err = rejected(reply);
            assert!(
                err.to_string().contains("re-run `sss pandora auth`"),
                "{err}"
            );
            assert!(err.detail.contains(&format!("HTTP {status}")), "{err}");
            mock.assert_calls(1);
            assert_eq!(seen.len(), 1, "the refusal is recorded too");
        }
    }

    #[test]
    fn error_code_1001_on_200_rejects_the_session() {
        let server = MockServer::start();
        server.mock(|_, then| {
            then.status(200)
                .json_body(json!({"errorCode": 1001, "errorString": "INVALID_REQUEST"}));
        });
        let err = rejected(post(&server, "v1/x", json!({})).0);
        assert!(err.detail.contains("errorCode 1001"), "{err}");
    }

    #[test]
    fn error_code_body_on_200_fails_the_step_without_retry() {
        let server = MockServer::start();
        let mock = server.mock(|_, then| {
            then.status(200)
                .json_body(json!({"errorCode": 99000, "errorString": "PLAYLIST_VERSION_MISMATCH"}));
        });
        let (reply, seen) = post(&server, "v7/playlists/getTracks", json!({}));
        assert_eq!(
            reply.unwrap(),
            Reply::Failed(
                "HTTP 200, errorCode 99000, errorString \"PLAYLIST_VERSION_MISMATCH\"".into()
            )
        );
        mock.assert_calls(1);
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn server_error_with_error_code_fails_the_step_without_retry() {
        let server = MockServer::start();
        let mock = server.mock(|_, then| {
            then.status(502)
                .json_body(json!({"errorCode": 0, "message": "bad request"}));
        });
        let reply = post(&server, "v1/x", json!({})).0.unwrap();
        assert_eq!(
            reply,
            Reply::Failed("HTTP 502, errorCode 0, message \"bad request\"".into())
        );
        mock.assert_calls(1);
    }

    #[test]
    fn plain_server_errors_and_429_are_retried_then_fail_the_step() {
        for status in [503, 429] {
            let server = MockServer::start();
            let mock = server.mock(|_, then| {
                then.status(status).body("<html>busy</html>");
            });
            let (reply, seen) = post(&server, "v1/x", json!({}));
            assert_eq!(
                reply.unwrap(),
                Reply::Unavailable(format!(
                    "HTTP {status}, response is not JSON after {MAX_TRIES} tries"
                ))
            );
            mock.assert_calls(MAX_TRIES as usize);
            assert_eq!(seen.len(), MAX_TRIES as usize);
            assert!(
                seen[0].contains(r#""response":"<html>busy</html>""#),
                "{}",
                seen[0]
            );
        }
    }

    #[test]
    fn other_non_2xx_fails_the_step_without_retry() {
        let server = MockServer::start();
        let mock = server.mock(|_, then| {
            then.status(404).body("not found");
        });
        let reply = post(&server, "v9/nope", json!({})).0.unwrap();
        assert_eq!(
            reply,
            Reply::Failed("HTTP 404, response is not JSON".into())
        );
        mock.assert_calls(1);
    }

    #[test]
    fn a_2xx_body_that_is_not_json_fails_the_step() {
        let server = MockServer::start();
        server.mock(|_, then| {
            then.status(200).body("<html></html>");
        });
        let reply = post(&server, "v1/x", json!({})).0.unwrap();
        assert_eq!(
            reply,
            Reply::Failed("HTTP 200, response is not JSON".into())
        );
    }

    thread_local! {
        static WARNINGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    fn collect_warning(line: &str) {
        WARNINGS.with(|w| w.borrow_mut().push(line.to_owned()));
    }

    #[test]
    fn unreachable_server_is_an_error_after_every_try() {
        let mut client = Client::new("http://127.0.0.1:1", &session(), FAST);
        client.warn = collect_warning;
        let err = client
            .post("v1/x", &json!({}), &mut |_| Ok(()))
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("no response after 5 tries"),
            "{err:#}"
        );
        assert!(err.downcast_ref::<SessionRejected>().is_none());
        assert_eq!(crate::error::kind(&err), Kind::Unavailable);
        assert!(
            err.chain().nth(1).is_some(),
            "the transport error stays in the chain"
        );
        assert_eq!(client.requests(), 0);
        let warnings = WARNINGS.with(|w| w.take());
        assert_eq!(warnings.len(), MAX_TRIES as usize - 1, "{warnings:?}");
        for (n, line) in (1..).zip(&warnings) {
            assert!(
                line.starts_with(&format!(
                    "warning: POST v1/x: no response on attempt {n}/{MAX_TRIES} ("
                )) && line.ends_with("); retrying"),
                "{line}"
            );
        }
    }

    #[test]
    fn transport_warning_names_the_path_attempt_and_cause() {
        let e = anyhow!("connection refused").context("connecting");
        assert_eq!(
            transport_warning("v1/x", 2, &e),
            "warning: POST v1/x: no response on attempt 2/5 (connecting: connection refused); \
             retrying"
        );
    }

    #[test]
    fn requests_are_paced() {
        let server = MockServer::start();
        server.mock(|_, then| {
            then.status(200).json_body(json!({}));
        });
        let timing = Timing {
            min_interval: Duration::from_millis(60),
            first_backoff: Duration::ZERO,
        };
        let mut client = Client::new(&server.base_url(), &session(), timing);
        let start = Instant::now();
        for _ in 0..3 {
            client.post("v1/x", &json!({}), &mut |_| Ok(())).unwrap();
        }
        assert!(start.elapsed() >= Duration::from_millis(120));
        assert_eq!(client.requests(), 3);
    }

    #[test]
    fn a_failing_record_sink_aborts_before_classification() {
        let server = MockServer::start();
        server.mock(|_, then| {
            then.status(200).json_body(json!({}));
        });
        let mut client = Client::new(&server.base_url(), &session(), FAST);
        let err = client
            .post("v1/x", &json!({}), &mut |_| Err(anyhow!("disk full")))
            .unwrap_err();
        assert_eq!(err.to_string(), "disk full");
    }
}
