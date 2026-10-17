//! `sss youtube api-key` and `sss youtube enrich`: storing the owner's Data
//! API key once it has answered a request, and fetching `videos.list` for
//! every video the profile's normalized files name that the cache has no
//! answer for, then normalizing again.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use crate::error::Kind;
use crate::fail;
use crate::repo::{self, ProfilePaths};
use crate::secrets::StateRoot;
use crate::youtube::api::{self, Client, Fetched};
use crate::youtube::model::{self, Summary, Video};

/// What `sss youtube api-key` writes to stderr before reading stdin.
pub const PROMPT: &str = "Paste the YouTube Data API key, then an empty line:\n";

/// The key as read: one token of printable ASCII.
pub fn check_key(key: &str) -> Result<()> {
    if !key.bytes().all(|b| b.is_ascii_graphic()) {
        fail!(
            Kind::Input,
            "the key contains spaces or non-ASCII characters; paste the API key only"
        );
    }
    Ok(())
}

/// What `sss youtube api-key` reports: where the key is and when it was
/// stored, never the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiKeyReport {
    pub stored_at: String,
    pub key_path: PathBuf,
}

impl ApiKeyReport {
    pub fn write_text(&self, out: &mut dyn Write) -> std::io::Result<()> {
        writeln!(
            out,
            "YouTube Data API key verified; stored in {}",
            self.key_path.display()
        )
    }
}

/// Verifies `key` with one request through the client `connect` makes, then
/// stores it under `state_root`. A refused key is not stored.
pub fn verify_and_store(
    state_root: &StateRoot,
    key: &str,
    connect: impl FnOnce(&str) -> Client,
) -> Result<ApiKeyReport> {
    check_key(key)?;
    connect(key).verify()?;
    let stored = state_root.save_youtube_api_key(key)?;
    Ok(ApiKeyReport {
        stored_at: stored.stored_at,
        key_path: state_root.youtube_api_key_path(),
    })
}

/// The stored key, or a [`Kind::Auth`] error saying how to store one.
pub fn stored_key(state_root: &StateRoot) -> Result<String> {
    match state_root.load_youtube_api_key()? {
        Some(stored) => Ok(stored.key),
        None => fail!(
            Kind::Auth,
            "no YouTube Data API key stored; run `sss youtube api-key` (see \
             docs/youtube-takeout.md)"
        ),
    }
}

/// What `sss youtube enrich` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnrichReport {
    pub profile: String,
    /// Ids asked for this run.
    pub requested: usize,
    /// Of those, the ones the API returned.
    pub found: usize,
    /// Of those, the ones it did not: removed, private or never there.
    pub unavailable: usize,
    /// Videos still without an answer, left by `--limit`.
    pub remaining: usize,
    /// The normalization run after the fetch.
    pub normalized: Summary,
}

impl EnrichReport {
    pub fn write_text(&self, out: &mut dyn Write) -> std::io::Result<()> {
        writeln!(
            out,
            "enriched profile {}: {} videos requested, {} found, {} unavailable, {} remaining",
            self.profile, self.requested, self.found, self.unavailable, self.remaining
        )?;
        self.normalized.write_text(out)
    }
}

/// The ids of `videos` with no answer in `cache`, in id order.
pub fn unanswered(
    videos: &BTreeMap<String, Video>,
    cache: &BTreeMap<String, api::Cached>,
) -> Vec<String> {
    videos
        .keys()
        .filter(|id| !cache.contains_key(*id))
        .cloned()
        .collect()
}

/// Fetches the videos of `paths` the cache has no answer for, at most
/// `limit` of them, with `client`, then normalizes. A fetch that stops on an
/// error still normalizes what it recorded before returning the error.
pub fn enrich(
    profile: &str,
    paths: &ProfilePaths,
    client: &mut Client,
    limit: Option<usize>,
    progress: &mut dyn Write,
) -> Result<EnrichReport> {
    let Some(videos) = repo::read_json_opt::<BTreeMap<String, Video>>(&paths.youtube_videos())?
    else {
        fail!(
            Kind::Config,
            "no YouTube export for profile {profile}; run `sss youtube import DIR`"
        );
    };
    let dir = paths.youtube_api_dir();
    let mut ids = unanswered(&videos, &api::read_cache(&dir)?);
    let remaining = ids.len().saturating_sub(limit.unwrap_or(usize::MAX));
    ids.truncate(ids.len() - remaining);
    // Lines on `progress` are for people: a failed write does not stop the
    // command.
    let _ = writeln!(
        progress,
        "asking the YouTube Data API about {} videos ({} requests)",
        ids.len(),
        ids.len().div_ceil(api::CHUNK)
    );
    let fetched = api::fetch(client, &dir, &ids);
    let normalized = model::normalize(paths);
    let Fetched {
        requested,
        found,
        unavailable,
    } = match fetched {
        Ok(fetched) => fetched,
        Err(e) => {
            let _ = match &normalized {
                Ok(_) => writeln!(progress, "normalized what was fetched before the stop"),
                Err(n) => writeln!(
                    progress,
                    "warning: normalizing after the stop failed: {n:#}"
                ),
            };
            return Err(e);
        }
    };
    Ok(EnrichReport {
        profile: profile.to_owned(),
        requested,
        found,
        unavailable,
        remaining,
        normalized: normalized?,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn video(id: &str) -> (String, Video) {
        let v: Video = serde_json::from_value(serde_json::json!({
            "video_id": id, "title": null, "artists": [], "album": null, "duration_s": null,
            "channel": null, "kind": "unknown", "evidence": [], "title_source": "none",
            "youtube_title": null, "plays": 0, "music_app_plays": 0, "removed": false,
            "first_played": null, "last_played": null, "in_library": false, "liked": false,
            "available": null, "category_id": null, "topics": []
        }))
        .unwrap();
        (id.to_owned(), v)
    }

    #[test]
    fn only_videos_without_an_answer_are_asked_for() {
        let videos: BTreeMap<_, _> = ["bbbbbbbbbbb", "aaaaaaaaaaa", "ccccccccccc"]
            .into_iter()
            .map(video)
            .collect();
        let cache = BTreeMap::from([
            ("aaaaaaaaaaa".to_owned(), api::Cached::Absent),
            (
                "ccccccccccc".to_owned(),
                api::Cached::Found(api::ApiVideo::default()),
            ),
        ]);
        assert_eq!(unanswered(&videos, &cache), ["bbbbbbbbbbb"]);
        assert_eq!(
            unanswered(&videos, &BTreeMap::new()),
            ["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc"]
        );
    }

    #[test]
    fn a_key_with_spaces_is_refused_and_nothing_is_stored() {
        let tmp = tempfile::tempdir().unwrap();
        let root = StateRoot::at(tmp.path());
        let err = verify_and_store(&root, "two words", |_| {
            panic!("no request for a malformed key")
        })
        .unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Input);
        assert_eq!(root.load_youtube_api_key().unwrap(), None);
        check_key("AIza_good-key.0").unwrap();
    }

    #[test]
    fn enrich_without_a_key_or_an_export_says_what_to_run() {
        let tmp = tempfile::tempdir().unwrap();
        let err = stored_key(&StateRoot::at(tmp.path())).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Auth);
        assert!(format!("{err}").contains("sss youtube api-key"), "{err}");

        let repo = crate::repo::Repo::at(tmp.path().join("data"));
        let paths = repo.profile("alice").unwrap();
        let mut client = Client::new("http://127.0.0.1:9", "k", api::Timing::LIVE);
        let err = enrich("alice", &paths, &mut client, None, &mut Vec::new()).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Config);
        assert!(format!("{err}").contains("sss youtube import"), "{err}");
        assert_eq!(client.requests(), 0);
    }

    #[test]
    fn the_report_never_names_the_key() {
        let report = ApiKeyReport {
            stored_at: "2026-10-17T00:00:00Z".into(),
            key_path: Path::new("/state/youtube-api-key.json").to_owned(),
        };
        let mut out = Vec::new();
        report.write_text(&mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "YouTube Data API key verified; stored in /state/youtube-api-key.json\n"
        );
    }
}
