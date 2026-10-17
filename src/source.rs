//! The import sources a profile may hold, and the normalized exports of
//! each loaded together for the commands that work across them.

use std::fmt;

use anyhow::Result;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::error::Kind;
use crate::fail;
use crate::pandora;
use crate::repo::ProfilePaths;
use crate::youtube;

/// Where an item to match came from.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Pandora,
    Youtube,
}

impl Source {
    /// `pandora`, `youtube`: the JSON spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Pandora => "pandora",
            Source::Youtube => "youtube",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The normalized export of every source a profile holds; a source the
/// profile has not exported is `None`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Exports {
    pub pandora: Option<pandora::model::Normalized>,
    pub youtube: Option<youtube::model::Normalized>,
}

impl Exports {
    /// Every normalized export of `paths`, none of them required.
    pub fn load_opt(paths: &ProfilePaths) -> Result<Self> {
        Ok(Exports {
            pandora: pandora::model::read_opt(paths)?,
            youtube: youtube::model::read_opt(paths)?,
        })
    }

    /// Every normalized export of profile `profile` at `paths`; a profile
    /// with none is kind `config`.
    pub fn load(profile: &str, paths: &ProfilePaths) -> Result<Self> {
        let exports = Self::load_opt(paths)?;
        if exports.is_empty() {
            fail!(
                Kind::Config,
                "no export for profile {profile}; run `sss pandora export` or `sss youtube \
                 import DIR`"
            );
        }
        Ok(exports)
    }

    /// Drops the export of every source but `source`.
    pub fn retain_source(&mut self, source: Source) {
        match source {
            Source::Pandora => self.youtube = None,
            Source::Youtube => self.pandora = None,
        }
    }

    /// Whether no source has an export.
    pub fn is_empty(&self) -> bool {
        self.pandora.is_none() && self.youtube.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::Repo;

    #[test]
    fn sources_serialize_in_snake_case() {
        assert_eq!(serde_json::to_value(Source::Pandora).unwrap(), "pandora");
        assert_eq!(serde_json::to_value(Source::Youtube).unwrap(), "youtube");
        assert_eq!(Source::Youtube.to_string(), "youtube");
    }

    #[test]
    fn a_profile_with_no_export_is_a_config_error() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Repo::at(tmp.path()).profile("alice").unwrap();
        assert!(Exports::load_opt(&paths).unwrap().is_empty());
        let err = Exports::load("alice", &paths).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Config);
        assert_eq!(
            err.to_string(),
            "no export for profile alice; run `sss pandora export` or `sss youtube import DIR`"
        );
    }

    #[test]
    fn a_pandora_export_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Repo::at(tmp.path()).profile("alice").unwrap();
        let data = pandora::model::Normalized::default();
        pandora::model::write(&paths, &data).unwrap();
        let exports = Exports::load("alice", &paths).unwrap();
        assert_eq!(exports.pandora, Some(data));
        assert_eq!(exports.youtube, None);
        assert!(!exports.is_empty());
    }

    #[test]
    fn a_youtube_export_loads_alone_and_beside_pandora() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Repo::at(tmp.path()).profile("alice").unwrap();
        let data = youtube::model::Normalized::default();
        youtube::model::write(&paths, &data).unwrap();
        let exports = Exports::load("alice", &paths).unwrap();
        assert_eq!(
            (exports.pandora, exports.youtube),
            (None, Some(data.clone()))
        );

        pandora::model::write(&paths, &pandora::model::Normalized::default()).unwrap();
        let both = Exports::load("alice", &paths).unwrap();
        assert!(both.pandora.is_some());
        assert_eq!(both.youtube, Some(data.clone()));

        let mut youtube = both.clone();
        youtube.retain_source(Source::Youtube);
        assert_eq!((youtube.pandora, youtube.youtube), (None, Some(data)));
        let mut pandora = both;
        pandora.retain_source(Source::Pandora);
        assert!(pandora.pandora.is_some() && pandora.youtube.is_none());
    }
}
