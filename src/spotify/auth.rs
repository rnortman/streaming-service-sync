//! `sss spotify auth`: the PKCE sign-in in two steps — print the authorize
//! address, then take back the address the browser was redirected to — the
//! token endpoint's code exchange and refresh, and the stored token.

use std::fmt;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::error::{Kind, WithKind};
use crate::fail;
use crate::http;
use crate::input::read_value;
use crate::secrets::StateDir;

/// The live accounts origin, home of `/authorize` and `/api/token`.
pub const DEFAULT_AUTH_BASE_URL: &str = "https://accounts.spotify.com";

/// Environment variable that replaces [`DEFAULT_AUTH_BASE_URL`] (used to
/// point the binary at a test server).
pub const AUTH_BASE_URL_ENV: &str = "SSS_SPOTIFY_AUTH_BASE_URL";

/// The redirect URI registered for the app. Nothing listens there: the
/// browser fails to load it and the member copies the address back.
pub const REDIRECT_URI: &str = "https://127.0.0.1/callback";

/// The scopes every sign-in asks for.
pub const SCOPES: &[&str] = &[
    "playlist-read-private",
    "playlist-modify-private",
    "user-library-read",
    "user-library-modify",
    "user-follow-read",
    "user-follow-modify",
];

/// The extra scope asked for when `[push] public_playlists = true`.
pub const PUBLIC_PLAYLISTS_SCOPE: &str = "playlist-modify-public";

/// The public document that walks through registering the app.
pub const SETUP_DOC: &str = "docs/spotify-app-setup.md";

/// The prompt written before the redirected address is read from the input.
pub const CALLBACK_PROMPT: &str = "\
Paste the whole address the browser landed on (https://127.0.0.1/callback?code=…),
then press Enter on an empty line.
Address: ";

/// The scopes to ask for.
pub fn scopes(public_playlists: bool) -> Vec<String> {
    let mut scopes: Vec<String> = SCOPES.iter().map(|&s| s.to_owned()).collect();
    if public_playlists {
        scopes.push(PUBLIC_PLAYLISTS_SCOPE.to_owned());
    }
    scopes
}

/// `$SSS_SPOTIFY_AUTH_BASE_URL` when set and non-empty, else the live
/// accounts origin; without a trailing `/`.
pub fn auth_base_from_env() -> String {
    http::base_url_from_env(AUTH_BASE_URL_ENV, DEFAULT_AUTH_BASE_URL)
}

/// `len` random bytes, base64url without padding.
fn random_base64(len: usize) -> Result<String> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("reading system randomness: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// A fresh PKCE code verifier: 64 random bytes as 86 characters of RFC
/// 7636's unreserved alphabet.
pub fn new_verifier() -> Result<String> {
    random_base64(64)
}

/// A fresh `state` value: 16 random bytes, base64url.
pub fn new_state() -> Result<String> {
    random_base64(16)
}

/// The S256 code challenge of `verifier`.
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// A sign-in between its two steps, kept in the profile's state directory.
/// `Debug` never shows the verifier.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    pub state: String,
    pub verifier: String,
    /// RFC 3339 UTC.
    pub created_at: String,
}

impl fmt::Debug for Pending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pending")
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri)
            .field("scopes", &self.scopes)
            .field("state", &self.state)
            .field("verifier", &"<redacted>")
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// A Spotify access token and the refresh token that renews it. `Debug`
/// never shows either.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: String,
    /// When the access token expires, RFC 3339 UTC.
    pub expires_at: String,
    pub scopes: Vec<String>,
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Token")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .finish()
    }
}

/// The token file's contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredToken {
    /// When the file was written, RFC 3339 UTC.
    pub stored_at: String,
    pub token: Token,
}

/// The authorize address for `pending`, every value encoded once.
pub fn authorize_url(auth_base: &str, pending: &Pending) -> Result<String> {
    let mut url = Url::parse(&format!("{auth_base}/authorize"))
        .with_context(|| format!("{auth_base} is not a valid base URL"))?;
    url.query_pairs_mut()
        .append_pair("client_id", &pending.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &pending.redirect_uri)
        .append_pair("state", &pending.state)
        .append_pair("scope", &pending.scopes.join(" "))
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", &challenge(&pending.verifier));
    Ok(url.into())
}

/// What the first step of `sss spotify auth` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthStartReport {
    pub profile: String,
    pub authorize_url: String,
    pub redirect_uri: String,
}

impl AuthStartReport {
    pub fn write_text(&self, out: &mut dyn Write) -> std::io::Result<()> {
        writeln!(out, "{}", self.authorize_url)
    }

    /// What the member does next, for stderr.
    pub fn instructions(&self) -> String {
        format!(
            "Open the address this command printed in a browser, signed in to Spotify as the \
             member of profile {profile}, and approve.\n\
             The browser then lands on {redirect}?code=…&state=… and shows an error such as \
             \"can't connect\"; that is expected.\n\
             Copy the whole address from the address bar and run\n  \
             sss spotify auth --callback --profile {profile}\n\
             pasting it followed by an empty line, or save it in a file and add --from-file FILE.",
            profile = self.profile,
            redirect = self.redirect_uri,
        )
    }
}

/// `client_id`, the profile's configured Spotify client id; none is kind
/// `config`, naming the key to set in `config_path`.
pub fn require_client_id<'a>(
    profile: &str,
    client_id: Option<&'a str>,
    config_path: &Path,
) -> Result<&'a str> {
    let Some(client_id) = client_id else {
        fail!(
            Kind::Config,
            "no Spotify client id for profile {profile}: set `[spotify] client_id` in {} \
             (see {SETUP_DOC})",
            config_path.display()
        );
    };
    Ok(client_id)
}

/// The first step: writes a fresh pending sign-in for `profile` into
/// `state` and reports the authorize address. Without a client id nothing
/// is written.
pub fn start(
    profile: &str,
    client_id: Option<&str>,
    config_path: &Path,
    public_playlists: bool,
    state: &StateDir,
    auth_base: &str,
) -> Result<AuthStartReport> {
    let client_id = require_client_id(profile, client_id, config_path)?;
    let pending = Pending {
        client_id: client_id.to_owned(),
        redirect_uri: REDIRECT_URI.to_owned(),
        scopes: scopes(public_playlists),
        state: new_state()?,
        verifier: new_verifier()?,
        created_at: crate::repo::now_rfc3339()?,
    };
    let authorize_url = authorize_url(auth_base, &pending)?;
    state.save_spotify_pending(&pending)?;
    Ok(AuthStartReport {
        profile: profile.to_owned(),
        authorize_url,
        redirect_uri: pending.redirect_uri,
    })
}

/// What `sss spotify auth --callback` reports: the stored token's file,
/// scopes and expiry, never its values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthReport {
    pub profile: String,
    pub token_path: PathBuf,
    pub scopes: Vec<String>,
    pub expires_at: String,
}

impl AuthReport {
    pub fn write_text(&self, out: &mut dyn Write) -> std::io::Result<()> {
        writeln!(
            out,
            "Spotify account authorized for profile {}; token stored in {} (access token \
             expires {}, renewed automatically)",
            self.profile,
            self.token_path.display(),
            self.expires_at
        )
    }
}

/// Reads the redirected address by [`read_value`] with
/// [`CALLBACK_PROMPT`].
pub fn read_address(
    from_file: Option<&Path>,
    input: &mut dyn BufRead,
    prompt: &mut dyn Write,
) -> Result<String> {
    read_value(from_file, input, prompt, CALLBACK_PROMPT, "address")
}

/// The authorization code in `address`, the address the browser was
/// redirected to for `pending`. The address must be `pending`'s redirect
/// URI (scheme, host, port and path) carrying its `state`.
pub fn parse_callback(address: &str, pending: &Pending) -> Result<String> {
    let expected = Url::parse(&pending.redirect_uri).with_context(|| {
        format!(
            "the pending sign-in's redirect URI {:?}",
            pending.redirect_uri
        )
    })?;
    let wanted = format!(
        "paste the whole address the browser landed on, starting {}?code=",
        pending.redirect_uri
    );
    let Ok(url) = Url::parse(address) else {
        fail!(
            Kind::Input,
            "the pasted text is not a web address; {wanted}"
        );
    };
    if url.scheme() != expected.scheme()
        || url.host_str() != expected.host_str()
        || url.port_or_known_default() != expected.port_or_known_default()
        || url.path() != expected.path()
    {
        fail!(
            Kind::Input,
            "the pasted address is not the redirect address; {wanted}"
        );
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    if let Some(error) = param("error") {
        fail!(
            Kind::Auth,
            "Spotify did not authorize the account (error={error}); start over at \
             `sss spotify auth`"
        );
    }
    if param("state").as_deref() != Some(pending.state.as_str()) {
        fail!(
            Kind::Input,
            "the pasted address belongs to a different sign-in (its state does not match); \
             paste the address of the latest `sss spotify auth`, or start over"
        );
    }
    match param("code") {
        Some(code) if !code.is_empty() => Ok(code),
        _ => fail!(Kind::Input, "the pasted address carries no code; {wanted}"),
    }
}

/// The second step: reads the redirected address, exchanges its code at
/// the token endpoint under `auth_base`, writes the token file and then
/// removes the pending sign-in.
pub fn complete(
    profile: &str,
    state: &StateDir,
    from_file: Option<&Path>,
    input: &mut dyn BufRead,
    prompt: &mut dyn Write,
    auth_base: &str,
) -> Result<AuthReport> {
    let Some(pending) = state.load_spotify_pending()? else {
        fail!(
            Kind::Auth,
            "no Spotify sign-in in progress for profile {profile}; run `sss spotify auth` first"
        );
    };
    let address = read_address(from_file, input, prompt)?;
    let code = parse_callback(&address, &pending)?;
    let reply = token_request(
        auth_base,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &pending.redirect_uri),
            ("client_id", &pending.client_id),
            ("code_verifier", &pending.verifier),
        ],
        "start over at `sss spotify auth`",
    )
    .context("exchanging the authorization code")?;
    let Some(refresh_token) = reply.refresh_token else {
        fail!(
            Kind::Auth,
            "Spotify's token reply carried no refresh token; start over at `sss spotify auth`"
        );
    };
    let token = Token {
        access_token: reply.access_token,
        refresh_token,
        expires_at: expires_at(reply.expires_in)?,
        scopes: reply
            .scope
            .map_or_else(|| pending.scopes.clone(), |s| split_scopes(&s)),
    };
    state.save_spotify_token(&token)?;
    state.delete_spotify_pending()?;
    Ok(AuthReport {
        profile: profile.to_owned(),
        token_path: state.spotify_token_path(),
        scopes: token.scopes,
        expires_at: token.expires_at,
    })
}

/// Renews `token` at the token endpoint under `auth_base`. A reply without
/// a refresh token keeps the old one; one without `scope` keeps the old
/// scopes.
pub fn refresh(auth_base: &str, token: &Token, client_id: &str) -> Result<Token> {
    let reply = token_request(
        auth_base,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &token.refresh_token),
            ("client_id", client_id),
        ],
        "run `sss spotify auth` to sign in again",
    )
    .context("renewing the Spotify access token")?;
    Ok(Token {
        access_token: reply.access_token,
        refresh_token: reply
            .refresh_token
            .unwrap_or_else(|| token.refresh_token.clone()),
        expires_at: expires_at(reply.expires_in)?,
        scopes: reply
            .scope
            .map_or_else(|| token.scopes.clone(), |s| split_scopes(&s)),
    })
}

/// The token endpoint's reply; `token_type` and anything else are ignored.
#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    expires_in: u32,
    refresh_token: Option<String>,
    scope: Option<String>,
}

fn split_scopes(scope: &str) -> Vec<String> {
    scope.split_whitespace().map(str::to_owned).collect()
}

/// Now plus `seconds`, RFC 3339 UTC.
fn expires_at(seconds: u32) -> Result<String> {
    (time::OffsetDateTime::now_utc() + time::Duration::seconds(i64::from(seconds)))
        .format(&time::format_description::well_known::Rfc3339)
        .context("formatting the expiry time")
}

/// POSTs `form` to `<auth_base>/api/token`. A 400 or 401 is Spotify
/// refusing the grant: kind `auth`, with `refused` as the next step. No
/// message quotes a form value.
fn token_request(auth_base: &str, form: &[(&str, &str)], refused: &str) -> Result<TokenReply> {
    let url = format!("{auth_base}/api/token");
    let reply = http::read(http::agent().post(&url).send_form(form))
        .with_kind(Kind::Unavailable, || format!("POST {url}: no response"))?;
    let (status, body) = (reply.status, reply.body);
    match status {
        200..=299 => {
            serde_json::from_str(&body).with_context(|| format!("POST {url}: parsing the reply"))
        }
        400 | 401 => fail!(
            Kind::Auth,
            "Spotify refused the request ({}); {refused}",
            oauth_detail(status, &body)
        ),
        500..=599 => fail!(
            Kind::Unavailable,
            "POST {url}: {}",
            oauth_detail(status, &body)
        ),
        _ => fail!(Kind::Other, "POST {url}: {}", oauth_detail(status, &body)),
    }
}

/// `HTTP <status>` with the OAuth error body's `error` and
/// `error_description`, or the body quoted when it is not one.
fn oauth_detail(status: u16, body: &str) -> String {
    let text = |v: &serde_json::Value, k: &str| v.get(k)?.as_str().map(str::to_owned);
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) if text(&v, "error").is_some() => {
            let error = text(&v, "error").unwrap_or_default();
            match text(&v, "error_description") {
                Some(d) => format!("HTTP {status} {error}: {d}"),
                None => format!("HTTP {status} {error}"),
            }
        }
        _ if body.trim().is_empty() => format!("HTTP {status}"),
        _ => format!("HTTP {status}: {:?}", body.trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::Method::POST;
    use httpmock::MockServer;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn state_dir(tmp: &Path) -> StateDir {
        StateDir::under(tmp, "alice").unwrap()
    }

    fn pending() -> Pending {
        Pending {
            client_id: "test-client-id".into(),
            redirect_uri: REDIRECT_URI.into(),
            scopes: scopes(false),
            state: "st4te".into(),
            verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into(),
            created_at: "2026-10-15T00:00:00Z".into(),
        }
    }

    fn token() -> Token {
        Token {
            access_token: "access-secret".into(),
            refresh_token: "refresh-secret".into(),
            expires_at: "2026-10-15T01:00:00Z".into(),
            scopes: vec!["user-library-read".into()],
        }
    }

    fn seconds_from_now(at: &str) -> i64 {
        let at = time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339)
            .unwrap();
        (at - time::OffsetDateTime::now_utc()).whole_seconds()
    }

    #[test]
    fn the_rfc_7636_challenge_vector() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifiers_are_86_unreserved_characters_and_differ() {
        let (a, b) = (new_verifier().unwrap(), new_verifier().unwrap());
        assert_eq!(a.len(), 86);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c)),
            "{a}"
        );
        assert_ne!(a, b);
        assert_eq!(new_state().unwrap().len(), 22);
    }

    #[test]
    fn the_authorize_url_carries_every_parameter_encoded_once() {
        let url = authorize_url("https://accounts.example", &pending()).unwrap();
        assert!(
            url.starts_with("https://accounts.example/authorize?"),
            "{url}"
        );
        assert!(
            url.contains("redirect_uri=https%3A%2F%2F127.0.0.1%2Fcallback"),
            "{url}"
        );
        let pairs: Vec<(String, String)> = Url::parse(&url)
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let want = [
            ("client_id", "test-client-id"),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("state", "st4te"),
            ("scope", &SCOPES.join(" ")),
            ("code_challenge_method", "S256"),
            (
                "code_challenge",
                "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            ),
        ];
        let want: Vec<(String, String)> = want
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert_eq!(pairs, want);
    }

    #[test]
    fn public_playlists_add_their_scope() {
        assert!(!scopes(false).contains(&PUBLIC_PLAYLISTS_SCOPE.to_owned()));
        let public = scopes(true);
        assert_eq!(public.len(), SCOPES.len() + 1);
        assert_eq!(public.last().unwrap(), PUBLIC_PLAYLISTS_SCOPE);
    }

    #[test]
    fn start_without_a_client_id_is_config_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_dir(tmp.path());
        let err = start(
            "alice",
            None,
            Path::new("/data/config.toml"),
            false,
            &state,
            "https://a",
        )
        .unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Config);
        assert_eq!(
            format!("{err:#}"),
            "no Spotify client id for profile alice: set `[spotify] client_id` in \
             /data/config.toml (see docs/spotify-app-setup.md)"
        );
        assert!(!state.dir().exists());
    }

    #[test]
    fn start_writes_a_private_pending_file_and_reports_the_address() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_dir(tmp.path());
        let report = start(
            "alice",
            Some("test-client-id"),
            Path::new("/c"),
            true,
            &state,
            "https://a",
        )
        .unwrap();
        let stored = state.load_spotify_pending().unwrap().unwrap();
        assert_eq!(mode(&state.spotify_pending_path()), 0o600);
        assert_eq!(stored.verifier.len(), 86);
        assert_eq!(stored.client_id, "test-client-id");
        assert_eq!(stored.redirect_uri, REDIRECT_URI);
        assert_eq!(stored.scopes, scopes(true));
        assert_eq!(
            report.authorize_url,
            authorize_url("https://a", &stored).unwrap()
        );
        assert_eq!(report.redirect_uri, REDIRECT_URI);
        let text = report.instructions();
        assert!(
            text.contains("sss spotify auth --callback --profile alice"),
            "{text}"
        );
        assert!(!format!("{stored:?}").contains(&stored.verifier));
    }

    #[test]
    fn the_callback_must_be_the_redirect_uri_with_its_state_and_a_code() {
        let p = pending();
        assert_eq!(
            parse_callback("https://127.0.0.1/callback?code=c0de&state=st4te", &p).unwrap(),
            "c0de"
        );
        assert_eq!(
            parse_callback("https://127.0.0.1:443/callback?state=st4te&code=c%2Fd", &p).unwrap(),
            "c/d"
        );
        for (address, needle) in [
            ("c0de", "not a web address"),
            (
                "http://127.0.0.1/callback?code=c&state=st4te",
                "not the redirect",
            ),
            (
                "https://localhost/callback?code=c&state=st4te",
                "not the redirect",
            ),
            (
                "https://127.0.0.1:8080/callback?code=c&state=st4te",
                "not the redirect",
            ),
            (
                "https://127.0.0.1/other?code=c&state=st4te",
                "not the redirect",
            ),
            (
                "https://accounts.spotify.com/authorize?client_id=x&state=st4te",
                "not the redirect",
            ),
            (
                "https://127.0.0.1/callback?code=c&state=other",
                "different sign-in",
            ),
            ("https://127.0.0.1/callback?code=c", "different sign-in"),
            ("https://127.0.0.1/callback?state=st4te", "no code"),
            ("https://127.0.0.1/callback?code=&state=st4te", "no code"),
        ] {
            let err = parse_callback(address, &p).unwrap_err();
            assert_eq!(crate::error::kind(&err), Kind::Input, "{address}");
            let text = format!("{err:#}");
            assert!(text.contains(needle), "{address}: {text}");
        }
        let err = parse_callback(
            "https://127.0.0.1/callback?error=access_denied&state=st4te",
            &p,
        )
        .unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Auth);
        assert!(
            format!("{err:#}").contains("error=access_denied"),
            "{err:#}"
        );
    }

    #[test]
    fn complete_without_a_pending_sign_in_is_auth() {
        let tmp = tempfile::tempdir().unwrap();
        let err = complete(
            "alice",
            &state_dir(tmp.path()),
            None,
            &mut &b"https://127.0.0.1/callback?code=c\n"[..],
            &mut Vec::new(),
            "http://127.0.0.1:1",
        )
        .unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Auth);
        assert_eq!(
            format!("{err:#}"),
            "no Spotify sign-in in progress for profile alice; run `sss spotify auth` first"
        );
    }

    #[test]
    fn an_empty_paste_is_input() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_dir(tmp.path());
        state.save_spotify_pending(&pending()).unwrap();
        let mut prompt = Vec::new();
        let err = complete(
            "alice",
            &state,
            None,
            &mut &b"\n"[..],
            &mut prompt,
            "http://127.0.0.1:1",
        )
        .unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Input);
        assert_eq!(format!("{err:#}"), "no address given");
        assert_eq!(String::from_utf8(prompt).unwrap(), CALLBACK_PROMPT);
    }

    /// A pending sign-in in `tmp` and a mock token endpoint answering the
    /// code exchange with `reply`.
    fn exchange_against(
        server: &MockServer,
        status: u16,
        reply: serde_json::Value,
    ) -> httpmock::Mock<'_> {
        server.mock(|when, then| {
            when.method(POST)
                .path("/api/token")
                .header("content-type", "application/x-www-form-urlencoded")
                .form_urlencoded_tuple("grant_type", "authorization_code")
                .form_urlencoded_tuple("code", "c0de")
                .form_urlencoded_tuple("redirect_uri", REDIRECT_URI)
                .form_urlencoded_tuple("client_id", "test-client-id")
                .form_urlencoded_tuple("code_verifier", pending().verifier);
            then.status(status).json_body(reply);
        })
    }

    fn run_complete(state: &StateDir, server: &MockServer) -> Result<AuthReport> {
        complete(
            "alice",
            state,
            None,
            &mut &b"https://127.0.0.1/callback?code=c0de&state=st4te\n\n"[..],
            &mut Vec::new(),
            &server.base_url(),
        )
    }

    #[test]
    fn the_exchange_writes_a_private_token_file_and_removes_the_pending_one() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_dir(tmp.path());
        state.save_spotify_pending(&pending()).unwrap();
        let server = MockServer::start();
        let mock = exchange_against(
            &server,
            200,
            json!({"access_token": "access-secret", "token_type": "Bearer",
                   "expires_in": 3600, "refresh_token": "refresh-secret",
                   "scope": "user-library-read playlist-modify-private"}),
        );
        let report = run_complete(&state, &server).unwrap();
        mock.assert();
        assert_eq!(report.token_path, state.spotify_token_path());
        assert_eq!(
            report.scopes,
            ["user-library-read", "playlist-modify-private"]
        );
        assert_eq!(mode(&state.spotify_token_path()), 0o600);
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(state.spotify_token_path()).unwrap())
                .unwrap();
        assert_eq!(file["token"]["access_token"], "access-secret");
        assert_eq!(file["token"]["refresh_token"], "refresh-secret");
        assert!(file["stored_at"].is_string(), "{file}");
        let stored = state.load_spotify_token().unwrap().unwrap();
        assert_eq!(stored.token.expires_at, report.expires_at);
        let left = seconds_from_now(&report.expires_at);
        assert!((3590..=3600).contains(&left), "{left}");
        assert!(!state.spotify_pending_path().exists());
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("secret"), "{json}");
    }

    #[test]
    fn an_exchange_reply_without_scope_keeps_the_requested_scopes() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_dir(tmp.path());
        state.save_spotify_pending(&pending()).unwrap();
        let server = MockServer::start();
        exchange_against(
            &server,
            200,
            json!({"access_token": "a", "expires_in": 60, "refresh_token": "r"}),
        );
        assert_eq!(run_complete(&state, &server).unwrap().scopes, scopes(false));
    }

    #[test]
    fn an_exchange_reply_without_a_refresh_token_is_auth_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_dir(tmp.path());
        state.save_spotify_pending(&pending()).unwrap();
        let server = MockServer::start();
        exchange_against(&server, 200, json!({"access_token": "a", "expires_in": 60}));
        let err = run_complete(&state, &server).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Auth);
        assert!(format!("{err:#}").contains("no refresh token"), "{err:#}");
        assert!(!state.spotify_token_path().exists());
        assert!(state.spotify_pending_path().exists());
    }

    #[test]
    fn a_refused_code_is_auth_and_quotes_spotify() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_dir(tmp.path());
        state.save_spotify_pending(&pending()).unwrap();
        let server = MockServer::start();
        exchange_against(
            &server,
            400,
            json!({"error": "invalid_grant", "error_description": "Invalid authorization code"}),
        );
        let err = run_complete(&state, &server).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Auth);
        assert_eq!(
            format!("{err:#}"),
            "exchanging the authorization code: Spotify refused the request (HTTP 400 \
             invalid_grant: Invalid authorization code); start over at `sss spotify auth`"
        );
        assert!(!state.spotify_token_path().exists());
    }

    #[test]
    fn a_token_file_that_cannot_be_written_fails_the_command() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_dir(tmp.path());
        state.save_spotify_pending(&pending()).unwrap();
        // A directory where the token file belongs: the rename onto it fails.
        std::fs::create_dir_all(state.spotify_token_path().join("x")).unwrap();
        let server = MockServer::start();
        exchange_against(
            &server,
            200,
            json!({"access_token": "a", "expires_in": 60, "refresh_token": "r"}),
        );
        let err = run_complete(&state, &server).unwrap_err();
        assert!(format!("{err:#}").contains("spotify-token.json"), "{err:#}");
        assert!(state.spotify_pending_path().exists());
    }

    #[test]
    fn refresh_sends_the_refresh_grant_and_keeps_an_omitted_refresh_token() {
        let server = MockServer::start();
        let mut mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/token")
                .form_urlencoded_tuple("grant_type", "refresh_token")
                .form_urlencoded_tuple("refresh_token", "refresh-secret")
                .form_urlencoded_tuple("client_id", "test-client-id");
            then.status(200)
                .json_body(json!({"access_token": "fresh", "expires_in": 3600}));
        });
        let renewed = refresh(&server.base_url(), &token(), "test-client-id").unwrap();
        mock.assert();
        assert_eq!(renewed.access_token, "fresh");
        assert_eq!(renewed.refresh_token, "refresh-secret");
        assert_eq!(renewed.scopes, token().scopes);
        assert!(seconds_from_now(&renewed.expires_at) > 3500);
        mock.delete();

        server.mock(|when, then| {
            when.method(POST).path("/api/token");
            then.status(200).json_body(json!({
                "access_token": "fresher", "expires_in": 60,
                "refresh_token": "rotated", "scope": "a b"}));
        });
        let renewed = refresh(&server.base_url(), &token(), "test-client-id").unwrap();
        assert_eq!(renewed.refresh_token, "rotated");
        assert_eq!(renewed.scopes, ["a", "b"]);
    }

    #[test]
    fn a_refused_refresh_is_auth_and_an_outage_is_unavailable() {
        for (status, kind) in [
            (400, Kind::Auth),
            (401, Kind::Auth),
            (503, Kind::Unavailable),
            (418, Kind::Other),
        ] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(POST).path("/api/token");
                then.status(status).body("nope");
            });
            let err = refresh(&server.base_url(), &token(), "id").unwrap_err();
            assert_eq!(crate::error::kind(&err), kind, "{status}");
            let text = format!("{err:#}");
            assert!(text.contains(&format!("HTTP {status}: \"nope\"")), "{text}");
            assert!(!text.contains("secret"), "{text}");
            if kind == Kind::Auth {
                assert!(
                    text.ends_with("run `sss spotify auth` to sign in again"),
                    "{text}"
                );
            }
        }
        let err = refresh("http://127.0.0.1:1", &token(), "id").unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Unavailable);
    }

    #[test]
    fn debug_never_shows_tokens() {
        let text = format!("{:?}", token());
        assert!(!text.contains("secret"), "{text}");
        assert!(text.contains("2026-10-15T01:00:00Z"), "{text}");
    }
}
