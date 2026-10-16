//! The data repository's `config.toml`: schema, defaults, validation and
//! the skeleton `sss init` writes.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::error::{Kind, WithKind};
use crate::fail;
use crate::repo::validate_profile_name;

/// The text `sss init` writes as `config.toml`. Every value in it equals
/// the built-in default.
pub const SKELETON: &str = r#"# streaming-service-sync configuration.

[spotify]
client_id = ""          # the dev-mode app's client ID; per-profile override below
market = ""             # empty = the market of the signed-in account

[matcher]
agent = "music-matcher"
batch_size = 25
max_format_retries = 3
timeout_s = 900
# model = "…"
# effort = "…"
# agents_config = "/path/agents.toml"

[push]
playlist_prefix = ""
liked_songs = true
thumbs_down_playlist = false
public_playlists = false
linked_playlists = false  # also push Pandora-made playlists (Personalized, SharedListening, Curated, …)

# One table per profile, for overrides only.
# [profiles.alice]
# client_id = "…"       # a second app if the first app's user allowlist is full
"#;

/// The parsed `config.toml`. Every key is optional; absent keys take the
/// built-in defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub spotify: SpotifyConfig,
    pub matcher: MatcherConfig,
    pub push: PushConfig,
    pub profiles: BTreeMap<String, ProfileConfig>,
}

/// The `[spotify]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpotifyConfig {
    /// The dev-mode app's client ID; empty when not configured.
    pub client_id: String,
    /// Search market; empty means the market of the signed-in account.
    pub market: String,
}

/// The `[matcher]` table: settings for the LLM matching run.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MatcherConfig {
    pub agent: String,
    pub batch_size: u32,
    pub max_format_retries: u32,
    pub timeout_s: u64,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub agents_config: Option<PathBuf>,
}

impl Default for MatcherConfig {
    fn default() -> Self {
        Self {
            agent: "music-matcher".into(),
            batch_size: 25,
            max_format_retries: 3,
            timeout_s: 900,
            model: None,
            effort: None,
            agents_config: None,
        }
    }
}

/// The `[push]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PushConfig {
    pub playlist_prefix: String,
    /// Save matched thumbed-up tracks to Liked Songs; on by default.
    pub liked_songs: bool,
    pub thumbs_down_playlist: bool,
    pub public_playlists: bool,
    /// Push playlists Pandora made (any `linked_type` but `StationThumbs`
    /// and `MyThumbsUp`) as well as the member's own.
    pub linked_playlists: bool,
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            playlist_prefix: String::new(),
            liked_songs: true,
            thumbs_down_playlist: false,
            public_playlists: false,
            linked_playlists: false,
        }
    }
}

/// One `[profiles.<name>]` table: per-profile overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileConfig {
    pub client_id: Option<String>,
}

impl Config {
    /// Parses and validates `config.toml` text.
    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// Reads the repository's `config.toml`. A missing file means the
    /// repository was never initialized; a dangling symlink is reported as
    /// such.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound && path.is_symlink() => {
                fail!(
                    Kind::Config,
                    "{} is a symlink to a missing file",
                    path.display()
                )
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fail!(
                    Kind::Config,
                    "{} does not exist; run `sss init` to create the data repository",
                    path.display()
                )
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Self::parse(&text).with_kind(Kind::Config, || format!("in {}", path.display()))
    }

    fn validate(&self) -> Result<()> {
        if self.matcher.agent.trim().is_empty() {
            bail!("[matcher] agent must not be empty");
        }
        if self.matcher.batch_size == 0 {
            bail!("[matcher] batch_size must be at least 1");
        }
        if self.matcher.timeout_s == 0 {
            bail!("[matcher] timeout_s must be at least 1");
        }
        for name in self.profiles.keys() {
            validate_profile_name(name).context("in a [profiles.…] table name")?;
        }
        Ok(())
    }

    /// The Spotify client ID for `profile`: its own override when set and
    /// non-empty, else `[spotify] client_id`. `None` when neither is set.
    pub fn client_id(&self, profile: &str) -> Option<&str> {
        self.profiles
            .get(profile)
            .and_then(|p| p.client_id.as_deref())
            .filter(|id| !id.is_empty())
            .or(Some(self.spotify.client_id.as_str()).filter(|id| !id.is_empty()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_parses_to_the_defaults() {
        assert_eq!(Config::parse(SKELETON).unwrap(), Config::default());
    }

    #[test]
    fn empty_file_is_all_defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.matcher.agent, "music-matcher");
        assert_eq!(c.matcher.batch_size, 25);
        assert_eq!(c.matcher.max_format_retries, 3);
        assert_eq!(c.matcher.timeout_s, 900);
        assert!(c.push.liked_songs);
        assert!(!c.push.thumbs_down_playlist && !c.push.public_playlists);
    }

    #[test]
    fn liked_songs_can_be_turned_off() {
        let c = Config::parse("[push]\nliked_songs = false\n").unwrap();
        assert!(!c.push.liked_songs);
    }

    #[test]
    fn values_are_read() {
        let c = Config::parse(
            r#"
            [spotify]
            client_id = "app1"
            market = "US"
            [matcher]
            batch_size = 10
            model = "m"
            effort = "high"
            agents_config = "/x/agents.toml"
            [push]
            playlist_prefix = "P: "
            liked_songs = true
            [profiles.bob]
            client_id = "app2"
            "#,
        )
        .unwrap();
        assert_eq!(c.spotify.market, "US");
        assert_eq!(c.matcher.batch_size, 10);
        assert_eq!(c.matcher.timeout_s, 900);
        assert_eq!(c.matcher.model.as_deref(), Some("m"));
        assert_eq!(c.matcher.effort.as_deref(), Some("high"));
        assert_eq!(
            c.matcher.agents_config.as_deref(),
            Some(Path::new("/x/agents.toml"))
        );
        assert_eq!(c.push.playlist_prefix, "P: ");
        assert!(c.push.liked_songs);
        assert_eq!(c.client_id("bob"), Some("app2"));
        assert_eq!(c.client_id("alice"), Some("app1"));
    }

    #[test]
    fn client_id_falls_back_and_ignores_empty() {
        let c = Config::parse("[profiles.bob]\nclient_id = \"\"\n").unwrap();
        assert_eq!(c.client_id("bob"), None);
        assert_eq!(c.client_id("alice"), None);
        let c =
            Config::parse("[spotify]\nclient_id = \"app1\"\n[profiles.bob]\nclient_id = \"\"\n")
                .unwrap();
        assert_eq!(c.client_id("bob"), Some("app1"));
    }

    #[test]
    fn invalid_configs_are_rejected() {
        for (text, needle) in [
            ("[spotify]\nclientid = \"x\"\n", "clientid"),
            ("[nonsense]\n", "nonsense"),
            ("[matcher]\nbatch_size = 0\n", "batch_size"),
            ("[matcher]\ntimeout_s = 0\n", "timeout_s"),
            ("[matcher]\nagent = \" \"\n", "agent"),
            ("[matcher]\nbatch_size = \"many\"\n", "batch_size"),
            ("[profiles.Alice]\n", "Alice"),
            ("[push\n", "line 1"),
        ] {
            let err = format!("{:#}", Config::parse(text).unwrap_err());
            assert!(err.contains(needle), "{text:?}: {err}");
        }
    }

    #[test]
    fn load_reports_a_missing_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let err = format!(
            "{:#}",
            Config::load(&tmp.path().join("config.toml")).unwrap_err()
        );
        assert!(err.contains("sss init"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn load_reports_a_dangling_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::os::unix::fs::symlink(tmp.path().join("gone.toml"), &path).unwrap();
        let err = format!("{:#}", Config::load(&path).unwrap_err());
        assert!(err.contains("symlink to a missing file"), "{err}");
        assert!(!err.contains("sss init"), "{err}");
    }

    #[test]
    fn load_names_the_file_on_parse_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "[matcher]\nbatch_size = 0\n").unwrap();
        let err = Config::load(&path).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Config);
        let text = format!("{err:#}");
        assert!(
            text.starts_with(&format!("in {}: ", path.display())) && text.contains("batch_size"),
            "{text}"
        );
    }
}
