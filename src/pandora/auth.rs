//! `sss pandora auth`: take the `X-AuthToken` value the browser sends,
//! verify that Pandora accepts it, and store it in the profile's state
//! directory with a freshly generated CSRF value.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};

use crate::error::Kind;
use crate::fail;
use crate::input::{join_pasted, pasted_text, read_pasted};
use crate::pandora::client::{Client, Reply, endpoint};
use crate::repo;
use crate::secrets::{self, PandoraSession, StateDir, TokenSource};

/// The prompt `read_token` writes before reading the token from the input.
pub const PROMPT: &str = "\
Log in at https://www.pandora.com in your browser, open the developer
tools (F12), select the Network tab, reload the page and pick any request
whose URL contains /api/. Under Request Headers, copy the value of
X-AuthToken (the header, not the `at` cookie). Paste it below; if it
arrives on two lines, that is fine. Then press Enter on an empty line.
X-AuthToken: ";

/// Reads the token: every line of `from_file` when given, else the lines
/// of `input` up to an empty line or EOF, after writing [`PROMPT`] to
/// `prompt`. Each line is trimmed and the non-empty ones are joined with no
/// separator ([`join_pasted`]), so a value the browser copied across
/// two lines is whole.
pub fn read_token(
    from_file: Option<&Path>,
    input: &mut dyn BufRead,
    prompt: &mut dyn Write,
) -> Result<(String, TokenSource)> {
    // TODO(browser-cookie-extraction): a third source, the session of a
    // local Chromium or Chrome profile.
    let (token, source) = match from_file {
        Some(path) => {
            let text = pasted_text(std::fs::read_to_string(path), || {
                format!("reading {}", path.display())
            })?;
            (join_pasted(text.lines()), TokenSource::File)
        }
        None => {
            // The prompt is for people: a failed write of it does not stop
            // the read.
            let _ = write!(prompt, "{PROMPT}").and_then(|()| prompt.flush());
            (read_pasted(input)?, TokenSource::Prompt)
        }
    };
    if token.is_empty() {
        fail!(Kind::Input, "no token given");
    }
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        fail!(
            Kind::Input,
            "the token contains spaces or non-ASCII characters; paste the header value only"
        );
    }
    Ok((token, source))
}

/// What `sss pandora auth` reports: the stored session, never its token or
/// CSRF value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthReport {
    pub profile: String,
    pub webname: Option<String>,
    pub source: TokenSource,
    pub stored_at: String,
    pub session_path: PathBuf,
    /// The webname of the session this one replaced, when it differs.
    pub replaced_webname: Option<String>,
}

impl AuthReport {
    /// The warning naming the replaced listener, when there is one.
    pub fn replaced_warning(&self) -> Option<String> {
        let held = self.replaced_webname.as_deref()?;
        Some(format!(
            "warning: profile {} held a session for Pandora webname {held}; it now holds \
             webname {}",
            self.profile,
            self.webname.as_deref().unwrap_or_default()
        ))
    }

    pub fn write_text(&self, out: &mut dyn Write) -> std::io::Result<()> {
        let who = self
            .webname
            .as_deref()
            .map(|w| format!(" (webname {w})"))
            .unwrap_or_default();
        writeln!(
            out,
            "Pandora session verified{who}; stored in {}",
            self.session_path.display()
        )
    }
}

/// The listener's webname as the station list reports it, when it lists a
/// station.
fn webname(stations: &Value) -> Option<String> {
    stations
        .get("stations")?
        .as_array()?
        .iter()
        .find_map(|s| s.get("creatorWebname")?.as_str().map(str::to_owned))
}

/// Verifies `token` with one `getStations` request through the client
/// `connect` builds, then stores the session of `profile` in `state`.
/// Nothing is stored when Pandora does not accept the token. A stored
/// session for a different webname is replaced, and the report names it.
pub fn verify_and_store(
    profile: &str,
    state: &StateDir,
    token: String,
    source: TokenSource,
    connect: impl FnOnce(&PandoraSession) -> Client,
) -> Result<AuthReport> {
    let mut session = PandoraSession {
        auth_token: token,
        csrf_token: secrets::new_csrf_token()?,
        webname: None,
        source,
        stored_at: String::new(),
    };
    let mut client = connect(&session);
    let reply = client
        .post(endpoint::GET_STATIONS, &json!({"pageSize": 1}), &mut |_| {
            Ok(())
        })
        .context("verifying the token")?;
    let stations = match reply {
        Reply::Ok(body) => body,
        Reply::Failed(why) => fail!(
            Kind::Auth,
            "Pandora did not accept the token ({why}); copy the X-AuthToken header of a \
             request to pandora.com/api/, not the `at` cookie"
        ),
        Reply::Unavailable(why) => fail!(
            Kind::Unavailable,
            "Pandora is not answering ({why}); try `sss pandora auth` again later"
        ),
    };
    session.webname = webname(&stations);
    let replaced_webname = match (session.webname.as_deref(), state.held_pandora_webname()?) {
        (Some(new), Some(held)) if new != held => Some(held),
        _ => None,
    };
    session.stored_at = repo::now_rfc3339()?;
    state.save_pandora_session(&session)?;
    Ok(AuthReport {
        profile: profile.to_owned(),
        webname: session.webname,
        source: session.source,
        stored_at: session.stored_at,
        session_path: state.pandora_session_path(),
        replaced_webname,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pandora::test_support::FAST;
    use httpmock::prelude::*;
    use std::fs;

    fn read(from_file: Option<&Path>, input: &str) -> Result<(String, TokenSource)> {
        read_token(from_file, &mut input.as_bytes(), &mut Vec::new())
    }

    #[test]
    fn prompt_joins_trimmed_lines() {
        let mut prompt = Vec::new();
        let got = read_token(None, &mut "  abc.DEF=\nsecond\n".as_bytes(), &mut prompt).unwrap();
        assert_eq!(got, ("abc.DEF=second".to_owned(), TokenSource::Prompt));
        assert_eq!(String::from_utf8(prompt).unwrap(), PROMPT);
    }

    #[test]
    fn a_two_line_paste_is_joined_without_its_surrounding_whitespace() {
        assert_eq!(
            read(None, " \t tok-sec \r\n\u{a0} ret-value\t\n\n")
                .unwrap()
                .0,
            "tok-secret-value"
        );
    }

    #[test]
    fn an_empty_line_ends_the_paste() {
        assert_eq!(
            read(None, "first\nhalf\n   \nignored\n").unwrap().0,
            "firsthalf"
        );
    }

    #[test]
    fn eof_after_one_line_ends_the_paste() {
        assert_eq!(read(None, "only-line").unwrap().0, "only-line");
        assert_eq!(read(None, "only-line\n").unwrap().0, "only-line");
    }

    #[test]
    fn file_source_joins_every_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("token");
        std::fs::write(&path, "tok123\r\n\r\n  more \r\n").unwrap();
        assert_eq!(
            read(Some(&path), "unused").unwrap(),
            ("tok123more".to_owned(), TokenSource::File)
        );
        std::fs::write(&path, "\n  \r\n\t\n").unwrap();
        assert_eq!(
            read(Some(&path), "").unwrap_err().to_string(),
            "no token given"
        );
    }

    #[test]
    fn empty_or_malformed_tokens_are_refused() {
        assert_eq!(read(None, "\n").unwrap_err().to_string(), "no token given");
        assert_eq!(
            crate::error::kind(&read(None, "").unwrap_err()),
            Kind::Input
        );
        assert_eq!(
            crate::error::kind(&read(None, "two words").unwrap_err()),
            Kind::Input
        );
        assert_eq!(read(None, "").unwrap_err().to_string(), "no token given");
        for bad in ["two words\n", "tok\nen\u{e9}\n"] {
            assert!(
                read(None, bad).unwrap_err().to_string().contains("spaces"),
                "{bad:?}"
            );
        }
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        assert_eq!(
            crate::error::kind(&read(Some(&missing), "").unwrap_err()),
            Kind::Other
        );

        // Bytes that are not UTF-8, pasted or in the file.
        let err = read_token(None, &mut &b"tok\xe9n\n"[..], &mut Vec::new()).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Input);
        let latin1 = tmp.path().join("latin1");
        std::fs::write(&latin1, b"tok\xe9n\n").unwrap();
        let err = read(Some(&latin1), "").unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Input);
        assert!(
            format!("{err:#}").starts_with(&format!(
                "reading {}: not valid UTF-8 text",
                latin1.display()
            )),
            "{err:#}"
        );
    }

    #[test]
    fn a_prompt_that_cannot_be_written_does_not_stop_the_read() {
        let got = read_token(
            None,
            &mut "tok-secret-value\n".as_bytes(),
            &mut crate::pandora::test_support::ClosedPipe,
        )
        .unwrap();
        assert_eq!(got.0, "tok-secret-value");
    }

    #[test]
    fn webname_comes_from_the_first_station_that_names_one() {
        assert_eq!(
            webname(&json!({"stations": [{}, {"creatorWebname": "listener"}]})),
            Some("listener".into())
        );
        assert_eq!(webname(&json!({"stations": []})), None);
        assert_eq!(webname(&json!({})), None);
    }

    fn run(server: &MockServer, state: &StateDir) -> Result<AuthReport> {
        verify_and_store(
            "alice",
            state,
            "tok-secret-value".into(),
            TokenSource::Prompt,
            |s| Client::new(&server.base_url(), s, FAST),
        )
    }

    fn text(report: &AuthReport) -> String {
        let mut out = Vec::new();
        report.write_text(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn an_accepted_token_is_stored_with_a_generated_csrf_value() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/v1/station/getStations")
                .header("X-AuthToken", "tok-secret-value")
                .json_body(json!({"pageSize": 1}));
            then.status(200).json_body(json!({
                "totalStations": 1,
                "stations": [{"name": "Chill Out Radio", "creatorWebname": "listener"}]
            }));
        });
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        let report = run(&server, &state).unwrap();
        mock.assert_calls(1);
        let session = state.load_pandora_session().unwrap().unwrap();
        assert_eq!(session.auth_token, "tok-secret-value");
        assert_eq!(session.csrf_token.len(), 16);
        assert_eq!(session.webname.as_deref(), Some("listener"));
        assert_eq!(session.source, TokenSource::Prompt);
        assert!(!session.stored_at.is_empty());
        assert_eq!(
            report,
            AuthReport {
                profile: "alice".into(),
                webname: Some("listener".into()),
                source: TokenSource::Prompt,
                stored_at: session.stored_at.clone(),
                session_path: state.pandora_session_path(),
                replaced_webname: None,
            }
        );
        let out = text(&report);
        assert_eq!(
            out,
            format!(
                "Pandora session verified (webname listener); stored in {}\n",
                state.pandora_session_path().display()
            )
        );
        let json = serde_json::to_string(&report).unwrap();
        for secret in ["tok-secret-value", &session.csrf_token] {
            assert!(!json.contains(secret), "{json}");
        }
    }

    /// A server whose station list names `webname`.
    fn server_for(webname: &str) -> MockServer {
        let server = MockServer::start();
        server.mock(|_, then| {
            then.status(200)
                .json_body(json!({"stations": [{"creatorWebname": webname}]}));
        });
        server
    }

    fn stored(state: &StateDir, webname: Option<&str>) {
        let session = PandoraSession {
            auth_token: "old-token".into(),
            webname: webname.map(str::to_owned),
            ..crate::pandora::test_support::session()
        };
        state.save_pandora_session(&session).unwrap();
    }

    /// The stored session's webname and token.
    fn held(state: &StateDir) -> (Option<String>, String) {
        let s = state.load_pandora_session().unwrap().unwrap();
        (s.webname, s.auth_token)
    }

    #[test]
    fn re_auth_for_the_same_webname_replaces_the_session() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        stored(&state, Some("listener"));
        let report = run(&server_for("listener"), &state).unwrap();
        assert_eq!(report.replaced_webname, None);
        assert_eq!(report.replaced_warning(), None);
        assert_eq!(
            held(&state),
            (Some("listener".into()), "tok-secret-value".into())
        );
    }

    #[test]
    fn a_token_for_another_webname_replaces_the_session_and_names_it() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        stored(&state, Some("listener"));
        let report = run(&server_for("other"), &state).unwrap();
        assert_eq!(report.replaced_webname.as_deref(), Some("listener"));
        assert_eq!(
            report.replaced_warning().as_deref(),
            Some(
                "warning: profile alice held a session for Pandora webname listener; it now \
                 holds webname other"
            )
        );
        assert_eq!(
            held(&state),
            (Some("other".into()), "tok-secret-value".into())
        );
    }

    #[test]
    fn a_session_without_a_webname_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        stored(&state, None);
        let report = run(&server_for("other"), &state).unwrap();
        assert_eq!(report.replaced_webname, None);
        assert_eq!(
            held(&state),
            (Some("other".into()), "tok-secret-value".into())
        );
    }

    #[test]
    fn an_unparseable_session_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        stored(&state, None);
        fs::write(state.pandora_session_path(), "{not json").unwrap();
        let report = run(&server_for("other"), &state).unwrap();
        assert_eq!(report.replaced_webname, None);
        assert_eq!(
            held(&state),
            (Some("other".into()), "tok-secret-value".into())
        );
    }

    #[test]
    fn a_session_of_another_shape_still_names_its_webname() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        stored(&state, None);
        let other_shape = r#"{"webname": "listener", "extra": 1}"#;
        fs::write(state.pandora_session_path(), other_shape).unwrap();
        let report = run(&server_for("other"), &state).unwrap();
        assert_eq!(report.replaced_webname.as_deref(), Some("listener"));
        assert_eq!(
            held(&state),
            (Some("other".into()), "tok-secret-value".into())
        );
    }

    #[test]
    fn an_unreadable_session_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        fs::create_dir_all(state.pandora_session_path()).unwrap();
        let err = format!("{:#}", run(&server_for("other"), &state).unwrap_err());
        assert!(
            err.contains(&format!(
                "reading {}",
                state.pandora_session_path().display()
            )),
            "{err}"
        );
    }

    #[test]
    fn a_refused_token_is_not_stored() {
        let server = MockServer::start();
        server.mock(|_, then| {
            then.status(502).json_body(json!({"errorCode": 0}));
        });
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        let err = run(&server, &state).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Auth);
        let text = err.to_string();
        assert!(
            text.contains("HTTP 502") && text.contains("`at` cookie"),
            "{text}"
        );
        assert_eq!(state.load_pandora_session().unwrap(), None);
    }

    #[test]
    fn an_outage_is_not_reported_as_a_wrong_token() {
        let server = MockServer::start();
        server.mock(|_, then| {
            then.status(503).body("<html>busy</html>");
        });
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        let err = run(&server, &state).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Unavailable);
        let text = err.to_string();
        assert!(
            text.contains("not answering")
                && text.contains("HTTP 503")
                && text.contains("again later"),
            "{text}"
        );
        assert!(!text.contains("`at` cookie"), "{text}");
        assert_eq!(state.load_pandora_session().unwrap(), None);
    }

    #[test]
    fn an_invalid_token_says_to_re_run_auth() {
        let server = MockServer::start();
        server.mock(|_, then| {
            then.status(401).json_body(json!({"errorCode": 1001}));
        });
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        let err = run(&server, &state).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Auth);
        let err = format!("{err:#}");
        assert!(err.contains("re-run `sss pandora auth`"), "{err}");
        assert_eq!(state.load_pandora_session().unwrap(), None);
    }
}
