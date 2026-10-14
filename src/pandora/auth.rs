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

/// Reads the token: the first line of `from_file` when given, else one line
/// from `input` after writing a prompt to `prompt`.
pub fn read_token(
    from_file: Option<&Path>,
    input: &mut dyn BufRead,
    prompt: &mut dyn Write,
) -> Result<(String, TokenSource)> {
    // TODO(browser-cookie-extraction): a third source, the session of a
    // local Chromium or Chrome profile.
    let (line, source) = match from_file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            (
                text.lines().next().unwrap_or_default().to_owned(),
                TokenSource::File,
            )
        }
        None => {
            write!(
                prompt,
                "Paste the X-AuthToken request header value from pandora.com: "
            )
            .and_then(|()| prompt.flush())
            .context("writing the prompt")?;
            let mut line = String::new();
            input.read_line(&mut line).context("reading the token")?;
            (line, TokenSource::Prompt)
        }
    };
    let token = line.trim();
    if token.is_empty() {
        bail!("no token given");
    }
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        bail!("the token contains spaces or non-ASCII characters; paste the header value only");
    }
    Ok((token.to_owned(), source))
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
/// when Pandora does not accept it.
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

    fn read(from_file: Option<&Path>, input: &str) -> Result<(String, TokenSource)> {
        read_token(from_file, &mut input.as_bytes(), &mut Vec::new())
    }

    #[test]
    fn prompt_reads_one_trimmed_line() {
        let mut prompt = Vec::new();
        let got = read_token(None, &mut "  abc.DEF=\nsecond\n".as_bytes(), &mut prompt).unwrap();
        assert_eq!(got, ("abc.DEF=".to_owned(), TokenSource::Prompt));
        assert!(String::from_utf8(prompt).unwrap().contains("X-AuthToken"));
    }

    #[test]
    fn file_source_takes_the_first_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("token");
        std::fs::write(&path, "tok123\r\nignored\n").unwrap();
        assert_eq!(
            read(Some(&path), "unused").unwrap(),
            ("tok123".to_owned(), TokenSource::File)
        );
    }

    #[test]
    fn empty_or_malformed_tokens_are_refused() {
        assert_eq!(read(None, "\n").unwrap_err().to_string(), "no token given");
        assert_eq!(read(None, "").unwrap_err().to_string(), "no token given");
        assert!(
            read(None, "two words\n")
                .unwrap_err()
                .to_string()
                .contains("spaces")
        );
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
