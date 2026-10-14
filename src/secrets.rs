//! Per-profile credentials, such as the Pandora session, under
//! `$XDG_STATE_HOME/streaming-service-sync/<profile>/`.
//! The directory is mode 0700 and every file in it 0600; nothing here is
//! ever written inside the data repository.

use std::fmt;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::repo::{self, APP_DIR};

/// File name of the stored Pandora session inside a profile's state dir.
pub const PANDORA_SESSION_FILE: &str = "pandora-session.json";

/// One profile's state directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir {
    dir: PathBuf,
}

impl StateDir {
    /// The state directory of `profile` under `$XDG_STATE_HOME` (else
    /// `$HOME/.local/state`). The profile name is validated.
    pub fn for_profile(profile: &str) -> Result<Self> {
        let base = repo::xdg_dir("XDG_STATE_HOME", ".local/state", |v| std::env::var_os(v))
            .context("cannot determine $XDG_STATE_HOME or $HOME")?;
        Self::under(&base.join(APP_DIR), profile)
    }

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

    /// Replaces the stored Pandora session.
    pub fn save_pandora_session(&self, session: &PandoraSession) -> Result<()> {
        self.write_private(
            &self.pandora_session_path(),
            repo::to_json(session)?.as_bytes(),
        )
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

/// A fresh random CSRF token: 16 lowercase hex digits, the form Pandora's
/// own `csrftoken` cookie takes.
pub fn new_csrf_token() -> Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("reading system randomness: {e}"))?;
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
    fn debug_never_shows_tokens() {
        let text = format!("{:?}", session());
        assert!(!text.contains("tok-secret-value"), "{text}");
        assert!(!text.contains("0123456789abcdef"), "{text}");
        assert!(text.contains("listener"), "{text}");
    }

    #[test]
    fn profile_names_are_validated() {
        assert!(StateDir::under(Path::new("/s"), "../x").is_err());
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
