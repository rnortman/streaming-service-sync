//! Per-profile credentials — the Pandora session, the Spotify token and a
//! Spotify sign-in in progress — under
//! `$XDG_STATE_HOME/streaming-service-sync/<profile>/`.
//! The directory is mode 0700 and every file in it 0600; nothing here is
//! ever written inside the data repository.

use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::error::Kind;
use crate::fail;
use crate::repo::{self, APP_DIR};
use crate::spotify::auth::{Pending, StoredToken, Token};

/// File name of the stored Pandora session inside a profile's state dir.
pub const PANDORA_SESSION_FILE: &str = "pandora-session.json";

/// File name of the stored Spotify token inside a profile's state dir.
pub const SPOTIFY_TOKEN_FILE: &str = "spotify-token.json";

/// File name of a Spotify sign-in between its two steps.
pub const SPOTIFY_PENDING_FILE: &str = "spotify-auth-pending.json";

/// Every credential file a profile's state directory can hold; a directory
/// under the state root holding at least one of them is a profile.
pub const CREDENTIAL_FILES: &[&str] = &[PANDORA_SESSION_FILE, SPOTIFY_TOKEN_FILE];

/// The state root, `$XDG_STATE_HOME/streaming-service-sync/`, holding one
/// directory per profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRoot {
    root: PathBuf,
}

impl StateRoot {
    /// The state root under `$XDG_STATE_HOME` (else `$HOME/.local/state`).
    pub fn locate() -> Result<Self> {
        let Some(base) = repo::xdg_dir("XDG_STATE_HOME", ".local/state", |v| std::env::var_os(v))
        else {
            fail!(Kind::Config, "cannot determine $XDG_STATE_HOME or $HOME");
        };
        Ok(Self::at(base.join(APP_DIR)))
    }

    /// The state root at `root`.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The state directory of `profile`. The name is validated.
    pub fn profile(&self, name: &str) -> Result<StateDir> {
        StateDir::under(&self.root, name)
    }

    /// Names of the directories under the root that are valid profile names
    /// and hold at least one credential file, sorted. A missing root is an
    /// empty list. An entry whose credential files cannot be examined (for
    /// example a directory the user may not traverse) is an error.
    pub fn profiles(&self) -> Result<Vec<String>> {
        repo::profile_entries(&self.root, |entry| {
            let dir = entry.path();
            for file in CREDENTIAL_FILES {
                match fs::metadata(dir.join(file)) {
                    Ok(m) if m.is_file() => return Ok(true),
                    Ok(_) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                        ) => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(false)
        })
    }
}

/// One profile's state directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir {
    dir: PathBuf,
}

impl StateDir {
    /// The state directory of `profile` under `root`.
    pub fn under(root: &Path, profile: &str) -> Result<Self> {
        repo::validate_profile_name(profile)?;
        Ok(Self {
            dir: root.join(profile),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn pandora_session_path(&self) -> PathBuf {
        self.dir.join(PANDORA_SESSION_FILE)
    }

    /// The stored Pandora session, `None` when there is none.
    pub fn load_pandora_session(&self) -> Result<Option<PandoraSession>> {
        repo::read_json_opt(&self.pandora_session_path())
    }

    /// The `webname` string recorded in the stored Pandora session file,
    /// read from the JSON without requiring the rest of the session to
    /// parse. `None` when there is no file, it is not JSON, or it records no
    /// webname; an error only when the file cannot be read.
    pub fn held_pandora_webname(&self) -> Result<Option<String>> {
        let path = self.pandora_session_path();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("webname")?.as_str().map(str::to_owned)))
    }

    /// Replaces the stored Pandora session.
    pub fn save_pandora_session(&self, session: &PandoraSession) -> Result<()> {
        self.write_private(
            &self.pandora_session_path(),
            repo::to_json(session)?.as_bytes(),
        )
    }

    pub fn spotify_token_path(&self) -> PathBuf {
        self.dir.join(SPOTIFY_TOKEN_FILE)
    }

    /// The stored Spotify token, `None` when there is none.
    pub fn load_spotify_token(&self) -> Result<Option<StoredToken>> {
        repo::read_json_opt(&self.spotify_token_path())
    }

    /// Replaces the stored Spotify token, stamped with the current time.
    pub fn save_spotify_token(&self, token: &Token) -> Result<StoredToken> {
        let stored = StoredToken {
            stored_at: repo::now_rfc3339()?,
            token: token.clone(),
        };
        self.write_private(
            &self.spotify_token_path(),
            repo::to_json(&stored)?.as_bytes(),
        )?;
        Ok(stored)
    }

    pub fn spotify_pending_path(&self) -> PathBuf {
        self.dir.join(SPOTIFY_PENDING_FILE)
    }

    /// The Spotify sign-in in progress, `None` when there is none.
    pub fn load_spotify_pending(&self) -> Result<Option<Pending>> {
        repo::read_json_opt(&self.spotify_pending_path())
    }

    /// Replaces the Spotify sign-in in progress.
    pub fn save_spotify_pending(&self, pending: &Pending) -> Result<()> {
        self.write_private(
            &self.spotify_pending_path(),
            repo::to_json(pending)?.as_bytes(),
        )
    }

    /// Removes the Spotify sign-in in progress; none is not an error.
    pub fn delete_spotify_pending(&self) -> Result<()> {
        let path = self.spotify_pending_path();
        match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", path.display()))
            }
            _ => Ok(()),
        }
    }

    /// Atomically replaces `path` (inside this directory) with a 0600 file,
    /// creating the directory 0700 first. An existing directory's mode is
    /// tightened to 0700.
    fn write_private(&self, path: &Path, contents: &[u8]) -> Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {}", self.dir.display()))?;
        repo::write_atomic_private(path, contents)
    }
}

/// Where a stored session's token came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenSource {
    /// Pasted at the `sss pandora auth` prompt.
    Prompt,
    /// Read from `--from-file`.
    File,
}

impl TokenSource {
    /// The source's name, as the session file spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            TokenSource::Prompt => "prompt",
            TokenSource::File => "file",
        }
    }
}

/// The credentials every Pandora API request presents. `Debug` never shows
/// the token values.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PandoraSession {
    /// The `X-AuthToken` header value.
    pub auth_token: String,
    /// Sent both as the `X-CsrfToken` header and the `csrftoken` cookie;
    /// Pandora only checks that the two are equal.
    pub csrf_token: String,
    /// The listener's webname, when known.
    pub webname: Option<String>,
    /// Where the token came from.
    pub source: TokenSource,
    /// When the session was stored, RFC 3339 UTC.
    pub stored_at: String,
}

impl fmt::Debug for PandoraSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PandoraSession")
            .field("auth_token", &"<redacted>")
            .field("csrf_token", &"<redacted>")
            .field("webname", &self.webname)
            .field("source", &self.source)
            .field("stored_at", &self.stored_at)
            .finish()
    }
}

/// Fills `bytes` from the system's randomness.
pub(crate) fn fill_random(bytes: &mut [u8]) -> Result<()> {
    getrandom::fill(bytes).map_err(|e| anyhow::anyhow!("reading system randomness: {e}"))
}

/// A fresh random CSRF token: 16 lowercase hex digits, the form Pandora's
/// own `csrftoken` cookie takes.
pub fn new_csrf_token() -> Result<String> {
    let mut bytes = [0u8; 8];
    fill_random(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pandora::test_support::session;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn session_round_trips_with_private_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        assert_eq!(state.load_pandora_session().unwrap(), None);
        state.save_pandora_session(&session()).unwrap();
        assert_eq!(state.load_pandora_session().unwrap(), Some(session()));
        assert_eq!(mode(state.dir()), 0o700);
        assert_eq!(mode(&state.pandora_session_path()), 0o600);
    }

    #[test]
    fn saving_tightens_an_existing_directory_and_replaces_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        fs::create_dir_all(state.dir()).unwrap();
        fs::set_permissions(state.dir(), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(state.pandora_session_path(), "old").unwrap();
        fs::set_permissions(
            state.pandora_session_path(),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        state.save_pandora_session(&session()).unwrap();
        assert_eq!(mode(state.dir()), 0o700);
        assert_eq!(mode(&state.pandora_session_path()), 0o600);
        assert_eq!(state.load_pandora_session().unwrap(), Some(session()));
    }

    #[test]
    fn the_session_file_spells_the_token_source_in_lowercase() {
        assert_eq!(
            serde_json::to_value(TokenSource::Prompt).unwrap(),
            serde_json::json!("prompt")
        );
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        fs::create_dir_all(state.dir()).unwrap();
        fs::write(
            state.pandora_session_path(),
            r#"{"auth_token": "a", "csrf_token": "b", "webname": null,
                "source": "file", "stored_at": "2026-10-13T00:00:00Z"}"#,
        )
        .unwrap();
        let loaded = state.load_pandora_session().unwrap().unwrap();
        assert_eq!(loaded.source, TokenSource::File);
    }

    #[test]
    fn as_str_is_the_session_file_spelling() {
        for source in [TokenSource::Prompt, TokenSource::File] {
            assert_eq!(
                serde_json::to_value(source).unwrap(),
                serde_json::json!(source.as_str())
            );
        }
    }

    #[test]
    fn debug_never_shows_tokens() {
        let text = format!("{:?}", session());
        assert!(!text.contains("tok-secret-value"), "{text}");
        assert!(!text.contains("0123456789abcdef"), "{text}");
        assert!(text.contains("listener"), "{text}");
    }

    #[test]
    fn profile_names_are_validated() {
        assert!(StateDir::under(Path::new("/s"), "../x").is_err());
        assert!(StateRoot::at("/s").profile("Bad/Name").is_err());
        assert_eq!(
            StateRoot::at("/s").profile("alice").unwrap().dir(),
            Path::new("/s/alice")
        );
    }

    #[test]
    fn root_profiles_are_directories_holding_a_credential_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = StateRoot::at(tmp.path().join("missing"));
        assert!(root.profiles().unwrap().is_empty());

        let root = StateRoot::at(tmp.path());
        for name in ["carol", "alice"] {
            root.profile(name)
                .unwrap()
                .save_pandora_session(&session())
                .unwrap();
        }
        // No credential file: not a profile.
        fs::create_dir_all(tmp.path().join("bob")).unwrap();
        fs::write(tmp.path().join("bob/notes.txt"), "x").unwrap();
        // The matcher's agent directory is never a profile.
        fs::create_dir_all(tmp.path().join("ccc/agents")).unwrap();
        // A credential file under an invalid name is ignored.
        fs::create_dir_all(tmp.path().join("Dave")).unwrap();
        fs::write(tmp.path().join("Dave").join(PANDORA_SESSION_FILE), "{}").unwrap();
        // A plain file at the root is ignored.
        fs::write(tmp.path().join("erin"), "x").unwrap();
        assert_eq!(root.profiles().unwrap(), ["alice", "carol"]);
    }

    #[test]
    fn an_unexaminable_credential_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("alice");
        fs::create_dir_all(&dir).unwrap();
        // A symlink to itself: examining it fails with ELOOP, not NotFound.
        let file = dir.join(PANDORA_SESSION_FILE);
        std::os::unix::fs::symlink(&file, &file).unwrap();
        let err = format!("{:#}", StateRoot::at(tmp.path()).profiles().unwrap_err());
        assert!(err.contains(&dir.display().to_string()), "{err}");
    }

    #[test]
    fn a_spotify_token_alone_makes_a_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let root = StateRoot::at(tmp.path());
        let state = root.profile("dave").unwrap();
        assert_eq!(state.load_spotify_token().unwrap(), None);
        let token = Token {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: "2026-10-15T01:00:00Z".into(),
            scopes: vec![],
        };
        let stored = state.save_spotify_token(&token).unwrap();
        assert_eq!(stored.token, token);
        assert_eq!(state.load_spotify_token().unwrap(), Some(stored));
        assert_eq!(mode(&state.spotify_token_path()), 0o600);
        assert_eq!(mode(state.dir()), 0o700);
        assert_eq!(root.profiles().unwrap(), ["dave"]);
    }

    #[test]
    fn deleting_a_missing_pending_sign_in_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        state.delete_spotify_pending().unwrap();
        // A pending sign-in alone does not make a profile.
        state
            .save_spotify_pending(&Pending {
                client_id: "test-client-id".into(),
                redirect_uri: "https://127.0.0.1/callback".into(),
                scopes: vec![],
                state: "s".into(),
                verifier: "v".into(),
                created_at: "2026-10-15T00:00:00Z".into(),
            })
            .unwrap();
        assert_eq!(mode(&state.spotify_pending_path()), 0o600);
        assert!(StateRoot::at(tmp.path()).profiles().unwrap().is_empty());
        state.delete_spotify_pending().unwrap();
        assert!(!state.spotify_pending_path().exists());
    }

    #[test]
    fn csrf_tokens_are_16_hex_digits_and_differ() {
        let a = new_csrf_token().unwrap();
        let b = new_csrf_token().unwrap();
        assert_eq!(a.len(), 16);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(a, b);
    }
}
