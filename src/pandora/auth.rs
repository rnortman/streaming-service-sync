//! `sss pandora auth`: take the `X-AuthToken` value the browser sends,
//! verify that Pandora accepts it, and store it in the profile's state
//! directory with a freshly generated CSRF value.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

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
/// separator, so a value the browser copied across two lines is whole.
pub fn read_token(
    from_file: Option<&Path>,
    input: &mut dyn BufRead,
    prompt: &mut dyn Write,
) -> Result<(String, TokenSource)> {
    // TODO(browser-cookie-extraction): a third source, the session of a
    // local Chromium or Chrome profile.
    let (lines, source) = match from_file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            (
                text.lines().map(str::to_owned).collect::<Vec<_>>(),
                TokenSource::File,
            )
        }
        None => {
            write!(prompt, "{PROMPT}")
                .and_then(|()| prompt.flush())
                .context("writing the prompt")?;
            (read_until_empty_line(input)?, TokenSource::Prompt)
        }
    };
    let token: String = lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if token.is_empty() {
        bail!("no token given");
    }
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        bail!("the token contains spaces or non-ASCII characters; paste the header value only");
    }
    Ok((token, source))
}

/// The lines of `input` before the first line that is empty after trimming,
/// or before EOF.
fn read_until_empty_line(input: &mut dyn BufRead) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).context("reading the token")? == 0 || line.trim().is_empty() {
            return Ok(lines);
        }
        lines.push(line);
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
/// `connect` builds, then stores the session in `state`. Nothing is stored
/// when Pandora does not accept it, or when the stored session names a
/// different webname than the one the token reports.
pub fn verify_and_store(
    state: &StateDir,
    token: String,
    source: TokenSource,
    connect: impl FnOnce(&PandoraSession) -> Client,
    out: &mut dyn Write,
) -> Result<PandoraSession> {
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
        Reply::Failed(why) => bail!(
            "Pandora did not accept the token ({why}); copy the X-AuthToken header of a \
             request to pandora.com/api/, not the `at` cookie"
        ),
        Reply::Unavailable(why) => {
            bail!("Pandora is not answering ({why}); try `sss pandora auth` again later")
        }
    };
    session.webname = webname(&stations);
    if let (Some(new), Some(held)) = (session.webname.as_deref(), state.held_pandora_webname()?)
        && new != held
    {
        bail!(
            "this token is for Pandora webname {new}, but the profile's session at {} is for \
             webname {held}; pass --profile NAME to sign in another profile, or delete that \
             file to replace the listener",
            state.pandora_session_path().display()
        );
    }
    session.stored_at = repo::now_rfc3339()?;
    state.save_pandora_session(&session)?;
    let who = session
        .webname
        .as_deref()
        .map(|w| format!(" (webname {w})"))
        .unwrap_or_default();
    writeln!(
        out,
        "Pandora session verified{who}; stored in {}",
        state.pandora_session_path().display()
    )
    .context("writing output")?;
    Ok(session)
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
        assert_eq!(read(None, "").unwrap_err().to_string(), "no token given");
        for bad in ["two words\n", "tok\nen\u{e9}\n"] {
            assert!(
                read(None, bad).unwrap_err().to_string().contains("spaces"),
                "{bad:?}"
            );
        }
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        assert!(read(Some(&missing), "").is_err());
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

    fn run(server: &MockServer, state: &StateDir) -> (Result<PandoraSession>, String) {
        let mut out = Vec::new();
        let result = verify_and_store(
            state,
            "tok-secret-value".into(),
            TokenSource::Prompt,
            |s| Client::new(&server.base_url(), s, FAST),
            &mut out,
        );
        (result, String::from_utf8(out).unwrap())
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
        let (result, out) = run(&server, &state);
        let session = result.unwrap();
        mock.assert_calls(1);
        assert_eq!(state.load_pandora_session().unwrap(), Some(session.clone()));
        assert_eq!(session.auth_token, "tok-secret-value");
        assert_eq!(session.csrf_token.len(), 16);
        assert_eq!(session.webname.as_deref(), Some("listener"));
        assert_eq!(session.source, TokenSource::Prompt);
        assert!(!session.stored_at.is_empty());
        assert!(out.contains("webname listener"), "{out}");
        assert!(!out.contains("tok-secret-value"), "{out}");
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

    fn stored(state: &StateDir, webname: Option<&str>) -> PandoraSession {
        let session = PandoraSession {
            auth_token: "old-token".into(),
            webname: webname.map(str::to_owned),
            ..crate::pandora::test_support::session()
        };
        state.save_pandora_session(&session).unwrap();
        session
    }

    #[test]
    fn re_auth_for_the_same_webname_replaces_the_session() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        stored(&state, Some("listener"));
        let session = run(&server_for("listener"), &state).0.unwrap();
        assert_eq!(state.load_pandora_session().unwrap(), Some(session));
    }

    #[test]
    fn a_token_for_another_webname_is_not_stored() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        let held = stored(&state, Some("listener"));
        let err = run(&server_for("other"), &state).0.unwrap_err().to_string();
        assert!(
            err.contains("webname other")
                && err.contains("webname listener")
                && err.contains("--profile NAME")
                && err.contains(&state.pandora_session_path().display().to_string()),
            "{err}"
        );
        assert_eq!(state.load_pandora_session().unwrap(), Some(held));
    }

    #[test]
    fn a_session_without_a_webname_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        stored(&state, None);
        let session = run(&server_for("other"), &state).0.unwrap();
        assert_eq!(state.load_pandora_session().unwrap(), Some(session));
    }

    #[test]
    fn an_unparseable_session_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        stored(&state, None);
        fs::write(state.pandora_session_path(), "{not json").unwrap();
        let session = run(&server_for("other"), &state).0.unwrap();
        assert_eq!(state.load_pandora_session().unwrap(), Some(session));
    }

    #[test]
    fn a_session_of_another_shape_still_guards_its_webname() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        stored(&state, None);
        let other_shape = r#"{"webname": "listener", "extra": 1}"#;
        fs::write(state.pandora_session_path(), other_shape).unwrap();
        let err = run(&server_for("other"), &state).0.unwrap_err().to_string();
        assert!(err.contains("webname listener"), "{err}");
        assert_eq!(
            fs::read_to_string(state.pandora_session_path()).unwrap(),
            other_shape
        );
    }

    #[test]
    fn an_unreadable_session_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        fs::create_dir_all(state.pandora_session_path()).unwrap();
        let err = format!("{:#}", run(&server_for("other"), &state).0.unwrap_err());
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
        let err = run(&server, &state).0.unwrap_err().to_string();
        assert!(
            err.contains("HTTP 502") && err.contains("`at` cookie"),
            "{err}"
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
        let err = run(&server, &state).0.unwrap_err().to_string();
        assert!(
            err.contains("not answering")
                && err.contains("HTTP 503")
                && err.contains("again later"),
            "{err}"
        );
        assert!(!err.contains("`at` cookie"), "{err}");
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
        let err = format!("{:#}", run(&server, &state).0.unwrap_err());
        assert!(err.contains("re-run `sss pandora auth`"), "{err}");
        assert_eq!(state.load_pandora_session().unwrap(), None);
    }
}
