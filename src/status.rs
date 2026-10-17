//! `sss status`: per profile, the stored Pandora session and Spotify token,
//! the latest Pandora export run's and YouTube import run's step ledgers,
//! the YouTube video counts and the match counts by status; and whether the
//! YouTube Data API key is stored.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::config::Config;
use crate::pandora::export::{self, Manifest, Step};
use crate::profiles;
use crate::repo::{self, ProfilePaths, Repo};
use crate::secrets::{StateDir, StateRoot, TokenSource};
use crate::spotify::auth::{self, SCOPES};
use crate::youtube::classify::VideoKind;
use crate::youtube::model::{self as youtube_model, Library, Video};
use crate::youtube::takeout::{self, Step as ImportStep};

/// What `sss status` reports: one entry per profile, and the YouTube Data
/// API key every profile shares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    pub profiles: Vec<ProfileStatus>,
    pub youtube_api_key: Option<ApiKeyStatus>,
}

/// The stored YouTube Data API key, without the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiKeyStatus {
    pub stored_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileStatus {
    pub name: String,
    pub pandora_session: Option<SessionStatus>,
    pub spotify_token: Option<TokenStatus>,
    pub export: ExportStatus,
    pub youtube: YoutubeStatus,
    pub matches: MatchCounts,
}

/// A stored Pandora session, without its token values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionStatus {
    pub stored_at: String,
    pub webname: Option<String>,
    pub source: TokenSource,
}

/// A stored Spotify token, without its token values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenStatus {
    pub stored_at: String,
    pub expires_at: String,
    pub scopes: Vec<String>,
    /// Scopes this build's sign-in asks for that the token was not granted;
    /// empty for a token signed in since the list last grew.
    pub missing_scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportStatus {
    /// The newest run.
    pub latest_run: Option<RunStatus>,
    /// The newest run whose manifest says `complete`.
    pub latest_complete_run: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunStatus<M = Manifest> {
    pub run_id: String,
    /// `None` for a run that did not finish.
    pub manifest: Option<M>,
}

/// The profile's YouTube Takeout imports and normalized videos.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct YoutubeStatus {
    /// The newest import run.
    pub latest_run: Option<RunStatus<takeout::Manifest>>,
    /// The newest import run whose manifest says `complete`.
    pub latest_complete_run: Option<String>,
    /// Videos in `youtube/videos.json`.
    pub videos: usize,
    /// Of those, the ones the Data API has been asked about.
    pub enriched: usize,
    /// Of those, the ones of kind `unknown`.
    pub unknown: usize,
    /// Liked videos in `youtube/library.json`; 0 when the archive had no
    /// liked list.
    pub liked: usize,
}

/// Per entity, the count of each match status over the latest entry per
/// item id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MatchCounts {
    pub tracks: BTreeMap<String, usize>,
    pub artists: BTreeMap<String, usize>,
    pub albums: BTreeMap<String, usize>,
}

/// The status of `profile`, or of every known profile when none is named.
pub fn status(
    repo: &Repo,
    state_root: &StateRoot,
    config: &Config,
    profile: Option<&str>,
) -> Result<StatusReport> {
    let names = match profile {
        Some(name) => vec![name.to_owned()],
        None => profiles::known(repo, state_root, config)?,
    };
    let profiles = names
        .into_iter()
        .map(|name| {
            let state = state_root.profile(&name)?;
            let paths = repo.profile(&name)?;
            profile_status(name, &state, &paths)
        })
        .collect::<Result<_>>()?;
    let youtube_api_key = state_root.load_youtube_api_key()?.map(|k| ApiKeyStatus {
        stored_at: k.stored_at,
    });
    Ok(StatusReport {
        profiles,
        youtube_api_key,
    })
}

fn profile_status(name: String, state: &StateDir, paths: &ProfilePaths) -> Result<ProfileStatus> {
    let pandora_session = state.load_pandora_session()?.map(|s| SessionStatus {
        stored_at: s.stored_at,
        webname: s.webname,
        source: s.source,
    });
    let spotify_token = state.load_spotify_token()?.map(|s| TokenStatus {
        stored_at: s.stored_at,
        expires_at: s.token.expires_at,
        missing_scopes: missing_scopes(&s.token.scopes),
        scopes: s.token.scopes,
    });
    let latest_run: Option<RunStatus> = latest_run(&paths.raw_dir(), |id| paths.raw_run_dir(id))?;
    let latest_complete_run = match &latest_run {
        Some(RunStatus {
            run_id,
            manifest: Some(m),
        }) if m.complete => Some(run_id.clone()),
        _ => export::latest_complete_run(paths)?.map(|m| m.run_id),
    };
    Ok(ProfileStatus {
        name,
        pandora_session,
        spotify_token,
        export: ExportStatus {
            latest_run,
            latest_complete_run,
        },
        youtube: youtube_status(paths)?,
        matches: MatchCounts {
            tracks: match_counts(&paths.track_matches())?,
            artists: match_counts(&paths.artist_matches())?,
            albums: match_counts(&paths.album_matches())?,
        },
    })
}

/// The newest run under `raw_dir`, its manifest read from `run_dir(run_id)`;
/// none when there is no run.
fn latest_run<M: DeserializeOwned>(
    raw_dir: &Path,
    run_dir: impl FnOnce(&str) -> PathBuf,
) -> Result<Option<RunStatus<M>>> {
    let Some(run_id) = repo::run_ids(raw_dir)?.pop() else {
        return Ok(None);
    };
    Ok(Some(RunStatus {
        manifest: repo::read_manifest::<M>(&run_dir(&run_id))?,
        run_id,
    }))
}

fn youtube_status(paths: &ProfilePaths) -> Result<YoutubeStatus> {
    let latest_run: Option<RunStatus<takeout::Manifest>> =
        latest_run(&paths.youtube_raw_dir(), |id| paths.youtube_raw_run_dir(id))?;
    let latest_complete_run = match &latest_run {
        Some(RunStatus {
            run_id,
            manifest: Some(m),
        }) if m.complete => Some(run_id.clone()),
        _ => youtube_model::complete_runs(paths)?.pop().map(|m| m.run_id),
    };
    let videos: BTreeMap<String, Video> =
        repo::read_json_opt(&paths.youtube_videos())?.unwrap_or_default();
    Ok(YoutubeStatus {
        latest_run,
        latest_complete_run,
        videos: videos.len(),
        enriched: videos.values().filter(|v| v.available.is_some()).count(),
        unknown: videos
            .values()
            .filter(|v| v.kind == VideoKind::Unknown)
            .count(),
        liked: repo::read_json_opt::<Library>(&paths.youtube_library())?
            .map_or(0, |l| l.liked.len()),
    })
}

/// [`SCOPES`] minus `granted`, in `SCOPES` order. A granted scope outside
/// `SCOPES` is ignored.
fn missing_scopes(granted: &[String]) -> Vec<String> {
    auth::missing(SCOPES, granted)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

impl StatusReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        if self.profiles.is_empty() {
            writeln!(
                out,
                "no profiles yet; run `sss pandora auth --profile NAME` or `sss youtube import \
                 --profile NAME DIR` first"
            )?;
        }
        for profile in &self.profiles {
            for line in profile.lines() {
                writeln!(out, "{line}")?;
            }
        }
        match &self.youtube_api_key {
            Some(k) => writeln!(out, "youtube api key: stored {}", k.stored_at)?,
            None if self.profiles.iter().any(|p| p.youtube.latest_run.is_some()) => writeln!(
                out,
                "youtube api key: none (optional; run `sss youtube api-key`, see \
                 docs/youtube-takeout.md)"
            )?,
            None => {}
        }
        Ok(())
    }
}

impl ProfileStatus {
    /// The profile's block of text lines.
    fn lines(&self) -> Vec<String> {
        let name = &self.name;
        let mut lines = vec![format!("profile {name}")];
        let has_pandora = self.pandora_session.is_some() || self.export.latest_run.is_some();
        let has_youtube = self.youtube.latest_run.is_some();
        if has_pandora {
            lines.push(self.session_line());
        }
        lines.push(match &self.spotify_token {
            None => format!("  spotify token: none (run `sss spotify auth --profile {name}`)"),
            Some(t) => format!(
                "  spotify token: stored {}, expires {}",
                t.stored_at, t.expires_at
            ),
        });
        if let Some(t) = &self.spotify_token
            && !t.missing_scopes.is_empty()
        {
            lines.push(format!(
                "    lacks {}; run `sss spotify auth --profile {name}` to sign in again",
                t.missing_scopes.join(", ")
            ));
        }
        if has_pandora || !has_youtube {
            self.export_lines(&mut lines);
        }
        if has_youtube {
            self.youtube_lines(&mut lines);
        }
        for (kind, counts) in [
            ("tracks", &self.matches.tracks),
            ("artists", &self.matches.artists),
            ("albums", &self.matches.albums),
        ] {
            let counts = if counts.is_empty() {
                "none".to_owned()
            } else {
                counts
                    .iter()
                    .map(|(s, n)| format!("{s} {n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            lines.push(format!("  matches {kind:<8} {counts}"));
        }
        lines
    }

    /// The Pandora session line.
    fn session_line(&self) -> String {
        let name = &self.name;
        match &self.pandora_session {
            None => format!("  pandora session: none (run `sss pandora auth --profile {name}`)"),
            Some(s) => {
                let webname = s
                    .webname
                    .as_ref()
                    .map(|w| format!(", webname {w}"))
                    .unwrap_or_default();
                format!(
                    "  pandora session: stored {}{webname}, from {}",
                    s.stored_at,
                    s.source.as_str()
                )
            }
        }
    }

    /// The Pandora export ledger.
    fn export_lines(&self, lines: &mut Vec<String>) {
        let mut newest_complete = true;
        match &self.export.latest_run {
            None => lines.push("  export: no run yet".to_owned()),
            Some(RunStatus {
                run_id,
                manifest: None,
            }) => {
                newest_complete = false;
                lines.push(format!(
                    "  export run {run_id}: did not finish (no manifest)"
                ));
            }
            Some(RunStatus {
                run_id,
                manifest: Some(m),
            }) => {
                newest_complete = m.complete;
                let verdict = if m.complete { "complete" } else { "INCOMPLETE" };
                lines.push(format!(
                    "  export run {run_id}: {verdict}, {} requests, finished {}",
                    m.requests, m.finished
                ));
                for step in Step::ALL {
                    lines.push(format!(
                        "    {}",
                        export::step_line(step, m.steps.get(step))
                    ));
                }
            }
        }
        if !newest_complete && let Some(id) = &self.export.latest_complete_run {
            lines.push(format!("  latest complete run: {id}"));
        }
    }

    /// The YouTube import ledger and video counts.
    fn youtube_lines(&self, lines: &mut Vec<String>) {
        let yt = &self.youtube;
        let mut newest_complete = true;
        match &yt.latest_run {
            None => {}
            Some(RunStatus {
                run_id,
                manifest: None,
            }) => {
                newest_complete = false;
                lines.push(format!(
                    "  youtube import run {run_id}: did not finish (no manifest)"
                ));
            }
            Some(RunStatus {
                run_id,
                manifest: Some(m),
            }) => {
                newest_complete = m.complete;
                let verdict = if m.complete { "complete" } else { "INCOMPLETE" };
                lines.push(format!(
                    "  youtube import run {run_id}: {verdict}, {} files, finished {}",
                    m.files.len(),
                    m.finished
                ));
                for step in ImportStep::ALL {
                    lines.push(format!("    {}", m.steps.get(step).line(step)));
                }
            }
        }
        if !newest_complete && let Some(id) = &yt.latest_complete_run {
            lines.push(format!("  latest complete youtube run: {id}"));
        }
        let liked = match yt.liked {
            0 => "no likes in this archive".to_owned(),
            n => format!("{n} liked"),
        };
        lines.push(format!(
            "  youtube videos: {} ({} enriched, {} unknown); {liked}",
            yt.videos, yt.enriched, yt.unknown
        ));
    }
}

/// The count of each `status` over the latest entry per item id, read
/// from `item_id` or, in lines written before it, `pandora_id`.
fn match_counts(path: &Path) -> Result<BTreeMap<String, usize>> {
    let latest = repo::read_jsonl_latest::<Value, String, _>(path, |v| {
        v.get("item_id")
            .or_else(|| v.get("pandora_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    })?;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for entry in latest.values() {
        let status = entry
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        *counts.entry(status.to_owned()).or_default() += 1;
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pandora::export::{Manifest, StepResult, StepStatus, Steps};
    use crate::pandora::test_support::session;
    use crate::repo::MANIFEST_FILE;

    fn manifest(id: &str, complete: bool) -> Manifest {
        let mut steps = Steps::default();
        steps.stations.count = 3;
        if !complete {
            steps.playlists = StepResult {
                status: StepStatus::Failed,
                count: 0,
                error: Some("v6/collections/getSortedPlaylists: HTTP 502".into()),
            };
        }
        Manifest {
            run_id: id.into(),
            started: "2026-10-13T00:00:00Z".into(),
            finished: "2026-10-13T00:01:00Z".into(),
            requests: 9,
            steps,
            complete,
        }
    }

    fn report(repo: &Repo, state_root: &StateRoot, profile: Option<&str>) -> StatusReport {
        let config = Config::load(&repo.config_path()).unwrap();
        status(repo, state_root, &config, profile).unwrap()
    }

    fn run_status(repo: &Repo, state_root: &StateRoot, profile: Option<&str>) -> String {
        let mut out = Vec::new();
        report(repo, state_root, profile)
            .write_text(&mut out)
            .unwrap();
        String::from_utf8(out).unwrap()
    }

    /// A repository and a state root, both empty, under `tmp`.
    fn empty(tmp: &Path) -> (Repo, StateRoot) {
        let repo = Repo::at(tmp.join("data"));
        std::fs::create_dir_all(repo.root()).unwrap();
        std::fs::write(repo.config_path(), "").unwrap();
        (repo, StateRoot::at(tmp.join("state")))
    }

    #[test]
    fn prints_the_latest_ledger_and_match_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = empty(tmp.path());
        let paths = repo.profile("alice").unwrap();
        for (id, complete) in [("20261013T000000Z", true), ("20261014T000000Z", false)] {
            repo::write_json(
                &paths.raw_run_dir(id).join(MANIFEST_FILE),
                &manifest(id, complete),
            )
            .unwrap();
        }
        for (key, id, status) in [
            ("pandora_id", "TR:1", "unmatched"),
            ("item_id", "TR:2", "matched"),
            ("item_id", "TR:1", "matched"),
            ("item_id", "YV:dQw4w9WgXcQ", "ambiguous"),
        ] {
            repo::append_jsonl(
                &paths.track_matches(),
                &serde_json::json!({key: id, "status": status}),
            )
            .unwrap();
        }
        let text = run_status(&repo, &state_root, None);
        assert!(
            text.starts_with(
                "profile alice\n  pandora session: none (run `sss pandora auth --profile alice`)\n  \
                 spotify token: none (run `sss spotify auth --profile alice`)\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("  export run 20261014T000000Z: INCOMPLETE, 9 requests"),
            "{text}"
        );
        assert!(text.contains("    stations         ok      3\n"), "{text}");
        assert!(
            text.contains(
                "    playlists        FAILED  0  v6/collections/getSortedPlaylists: HTTP 502\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("  latest complete run: 20261013T000000Z\n"),
            "{text}"
        );
        assert!(
            text.contains("  matches tracks   ambiguous 1, matched 2\n"),
            "{text}"
        );
        assert!(text.contains("  matches albums   none\n"), "{text}");
    }

    #[test]
    fn the_report_carries_the_ledger_counts_and_no_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = empty(tmp.path());
        let paths = repo.profile("alice").unwrap();
        for (id, complete) in [("20261013T000000Z", true), ("20261014T000000Z", false)] {
            repo::write_json(
                &paths.raw_run_dir(id).join(MANIFEST_FILE),
                &manifest(id, complete),
            )
            .unwrap();
        }
        repo::append_jsonl(
            &paths.track_matches(),
            &serde_json::json!({"item_id": "TR:1", "status": "matched"}),
        )
        .unwrap();
        let json = serde_json::to_value(report(&repo, &state_root, None)).unwrap();
        let profile = &json["profiles"][0];
        assert_eq!(profile["name"], "alice");
        assert_eq!(profile["pandora_session"], Value::Null);
        assert_eq!(profile["spotify_token"], Value::Null);
        assert_eq!(
            profile["export"]["latest_run"]["manifest"],
            serde_json::to_value(manifest("20261014T000000Z", false)).unwrap()
        );
        assert_eq!(profile["export"]["latest_complete_run"], "20261013T000000Z");
        assert_eq!(
            profile["matches"],
            serde_json::json!({"tracks": {"matched": 1}, "artists": {}, "albums": {}})
        );

        state_root
            .profile("alice")
            .unwrap()
            .save_pandora_session(&session())
            .unwrap();
        let signed_in = report(&repo, &state_root, None);
        assert_eq!(
            signed_in.profiles[0].pandora_session,
            Some(SessionStatus {
                stored_at: "2026-10-13T00:00:00Z".into(),
                webname: Some("listener".into()),
                source: TokenSource::Prompt,
            })
        );
        let text = serde_json::to_string(&signed_in).unwrap();
        let stored = session();
        for secret in [&stored.auth_token, &stored.csrf_token] {
            assert!(!text.contains(secret), "{secret}: {text}");
        }

        // A complete newest run is the latest complete one.
        repo::write_json(
            &paths.raw_run_dir("20261015T000000Z").join(MANIFEST_FILE),
            &manifest("20261015T000000Z", true),
        )
        .unwrap();
        let export = &report(&repo, &state_root, None).profiles[0].export;
        assert_eq!(
            export.latest_complete_run.as_deref(),
            Some("20261015T000000Z")
        );
    }

    #[test]
    fn profiles_without_runs_and_unfinished_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = empty(tmp.path());
        assert_eq!(
            run_status(&repo, &state_root, None),
            "no profiles yet; run `sss pandora auth --profile NAME` or `sss youtube import \
             --profile NAME DIR` first\n"
        );
        assert!(run_status(&repo, &state_root, Some("bob")).contains("  export: no run yet\n"));
        let paths = repo.profile("bob").unwrap();
        std::fs::create_dir_all(paths.raw_run_dir("20261013T000000Z")).unwrap();
        let text = run_status(&repo, &state_root, None);
        assert!(text.starts_with("profile bob\n"), "{text}");
        assert!(
            text.contains("  export run 20261013T000000Z: did not finish (no manifest)\n"),
            "{text}"
        );
        assert!(!text.contains("latest complete run"), "{text}");
    }

    #[test]
    fn profiles_known_from_the_state_root_and_config_show_their_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = empty(tmp.path());
        state_root
            .profile("alice")
            .unwrap()
            .save_pandora_session(&session())
            .unwrap();
        std::fs::write(repo.config_path(), "[profiles.bob]\n").unwrap();
        let mut anonymous = session();
        anonymous.webname = None;
        anonymous.source = TokenSource::File;
        state_root
            .profile("carol")
            .unwrap()
            .save_pandora_session(&anonymous)
            .unwrap();
        let text = run_status(&repo, &state_root, None);
        assert!(
            text.starts_with(
                "profile alice\n  pandora session: stored 2026-10-13T00:00:00Z, \
                 webname listener, from prompt\n  spotify token: none (run `sss spotify auth \
                 --profile alice`)\n  export: no run yet\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "profile bob\n  spotify token: none (run `sss spotify auth --profile bob`)\n  \
                 export: no run yet\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "profile carol\n  pandora session: stored 2026-10-13T00:00:00Z, from file\n"
            ),
            "{text}"
        );
        assert!(!text.contains("tok-secret-value"), "{text}");
        assert!(!repo.profiles_dir().exists(), "status creates nothing");
    }

    fn import_manifest(id: &str, complete: bool) -> takeout::Manifest {
        let step = |status, count| takeout::StepResult {
            status,
            count,
            skipped: 0,
            error: None,
        };
        let history = if complete {
            step(takeout::StepStatus::Ok, 5)
        } else {
            step(takeout::StepStatus::Missing, 0)
        };
        takeout::Manifest {
            run_id: id.into(),
            started: "2026-10-17T00:00:00Z".into(),
            finished: "2026-10-17T00:00:01Z".into(),
            source_dir: "/tmp/takeout".into(),
            files: Vec::new(),
            steps: takeout::Steps {
                history,
                playlists: step(takeout::StepStatus::Ok, 2),
                subscriptions: step(takeout::StepStatus::Missing, 0),
                library: step(takeout::StepStatus::Ok, 1),
            },
            complete,
        }
    }

    #[test]
    fn a_youtube_only_profile_shows_its_import_ledger_and_no_pandora_line() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = empty(tmp.path());
        let paths = repo.profile("erin").unwrap();
        for (id, complete) in [("20261016T000000Z", true), ("20261017T000000Z", false)] {
            repo::write_json(
                &paths.youtube_raw_run_dir(id).join(MANIFEST_FILE),
                &import_manifest(id, complete),
            )
            .unwrap();
        }
        let video = |id: &str, kind, available| {
            let v = serde_json::json!({
                "video_id": id, "title": null, "artists": [], "album": null,
                "duration_s": null, "channel": null, "kind": kind, "evidence": [],
                "title_source": "none", "youtube_title": null, "plays": 0,
                "music_app_plays": 0, "removed": false, "first_played": null,
                "last_played": null, "in_library": false, "liked": false,
                "available": available, "category_id": null, "topics": []
            });
            (id.to_owned(), serde_json::from_value::<Video>(v).unwrap())
        };
        let videos: BTreeMap<String, Video> = [
            video("abcDEF12345", "song", Value::Null),
            video("zzzZZZ00000", "unknown", Value::Bool(false)),
            video("yyyYYY00000", "unknown", Value::Null),
        ]
        .into_iter()
        .collect();
        repo::write_json(&paths.youtube_videos(), &videos).unwrap();

        let text = run_status(&repo, &state_root, None);
        assert!(!text.contains("pandora"), "{text}");
        assert!(!text.contains("export"), "{text}");
        assert!(
            text.contains(
                "  youtube import run 20261017T000000Z: INCOMPLETE, 0 files, finished \
                 2026-10-17T00:00:01Z\n    history: missing from the archive\n    \
                 playlists: ok, 2 records, 0 skipped\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "  latest complete youtube run: 20261016T000000Z\n  youtube videos: 3 \
                 (1 enriched, 2 unknown); no likes in this archive\n"
            ),
            "{text}"
        );

        let json = serde_json::to_value(report(&repo, &state_root, None)).unwrap();
        let yt = &json["profiles"][0]["youtube"];
        assert_eq!(yt["latest_run"]["run_id"], "20261017T000000Z");
        assert_eq!(yt["latest_run"]["manifest"]["complete"], false);
        assert_eq!(yt["latest_complete_run"], "20261016T000000Z");
        assert_eq!(
            (
                yt["videos"].as_u64(),
                yt["enriched"].as_u64(),
                yt["unknown"].as_u64()
            ),
            (Some(3), Some(1), Some(2))
        );
        assert_eq!(yt["liked"], 0);
        repo::write_json(
            &paths.youtube_library(),
            &Library {
                liked: vec!["abcDEF12345".into()],
                ..Library::default()
            },
        )
        .unwrap();
        let text = run_status(&repo, &state_root, None);
        assert!(
            text.contains("(1 enriched, 2 unknown); 1 liked\n"),
            "{text}"
        );

        // A Pandora session makes the Pandora lines appear beside YouTube's.
        state_root
            .profile("erin")
            .unwrap()
            .save_pandora_session(&session())
            .unwrap();
        let text = run_status(&repo, &state_root, None);
        assert!(text.contains("  pandora session: stored"), "{text}");
        assert!(text.contains("  export: no run yet\n"), "{text}");
        assert!(text.contains("  youtube videos: 3"), "{text}");

        // A YouTube profile and no key: the line says how to store one.
        assert!(
            text.ends_with(
                "youtube api key: none (optional; run `sss youtube api-key`, see \
                 docs/youtube-takeout.md)\n"
            ),
            "{text}"
        );
        let stored = state_root.save_youtube_api_key("AIza-status-key").unwrap();
        let text = run_status(&repo, &state_root, None);
        assert!(
            text.ends_with(&format!("youtube api key: stored {}\n", stored.stored_at)),
            "{text}"
        );
        assert!(!text.contains("AIza-status-key"), "{text}");
        let json = serde_json::to_value(report(&repo, &state_root, None)).unwrap();
        assert_eq!(
            json["youtube_api_key"],
            serde_json::json!({ "stored_at": stored.stored_at })
        );
    }

    #[test]
    fn a_pandora_profile_has_an_empty_youtube_status_and_no_youtube_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = empty(tmp.path());
        state_root
            .profile("alice")
            .unwrap()
            .save_pandora_session(&session())
            .unwrap();
        let text = run_status(&repo, &state_root, None);
        assert!(!text.contains("youtube"), "{text}");
        let json = serde_json::to_value(report(&repo, &state_root, None)).unwrap();
        assert_eq!(
            json["profiles"][0]["youtube"],
            serde_json::json!({"latest_run": null, "latest_complete_run": null,
                               "videos": 0, "enriched": 0, "unknown": 0,
                               "liked": 0})
        );
        assert_eq!(json["youtube_api_key"], Value::Null);
    }

    #[test]
    fn a_stored_spotify_token_is_shown_without_its_values() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = empty(tmp.path());
        let token = crate::spotify::auth::Token {
            access_token: "access-secret".into(),
            refresh_token: "refresh-secret".into(),
            expires_at: "2026-10-15T01:00:00Z".into(),
            scopes: vec!["user-library-read".into()],
        };
        let stored = state_root
            .profile("dave")
            .unwrap()
            .save_spotify_token(&token)
            .unwrap();
        let signed_in = report(&repo, &state_root, None);
        assert_eq!(
            signed_in.profiles[0].spotify_token,
            Some(TokenStatus {
                stored_at: stored.stored_at.clone(),
                expires_at: "2026-10-15T01:00:00Z".into(),
                scopes: vec!["user-library-read".into()],
                missing_scopes: SCOPES
                    .iter()
                    .filter(|&&s| s != "user-library-read")
                    .map(|&s| s.to_owned())
                    .collect(),
            })
        );
        let text = run_status(&repo, &state_root, None);
        assert!(
            text.contains(&format!(
                "  spotify token: stored {}, expires 2026-10-15T01:00:00Z\n",
                stored.stored_at
            )),
            "{text}"
        );
        let json = serde_json::to_string(&signed_in).unwrap();
        for out in [&text, &json] {
            assert!(!out.contains("secret"), "{out}");
        }
    }

    fn token_with(scopes: &[&str]) -> crate::spotify::auth::Token {
        crate::spotify::auth::Token {
            access_token: "access-secret".into(),
            refresh_token: "refresh-secret".into(),
            expires_at: "2026-10-15T01:00:00Z".into(),
            scopes: scopes.iter().map(|&s| s.to_owned()).collect(),
        }
    }

    #[test]
    fn missing_scopes_are_the_sign_in_set_minus_the_grant_in_order() {
        let old_eight = [
            "playlist-read-private",
            "playlist-modify-private",
            "user-library-read",
            "user-library-modify",
            "user-follow-read",
            "user-follow-modify",
            "user-read-playback-state",
            "user-modify-playback-state",
        ];
        let granted: Vec<String> = old_eight.iter().map(|&s| s.to_owned()).collect();
        assert_eq!(
            missing_scopes(&granted),
            [
                "playlist-read-collaborative",
                "playlist-modify-public",
                "user-read-currently-playing",
                "user-read-playback-position",
                "user-read-recently-played",
                "user-top-read",
            ]
        );
        let mut full: Vec<String> = SCOPES.iter().map(|&s| s.to_owned()).collect();
        assert!(missing_scopes(&full).is_empty());
        full.push("ugc-image-upload".into());
        assert!(
            missing_scopes(&full).is_empty(),
            "an extra scope is not flagged"
        );
    }

    #[test]
    fn the_lacks_line_appears_exactly_when_scopes_are_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = empty(tmp.path());
        let state = state_root.profile("alice").unwrap();
        state
            .save_spotify_token(&token_with(&SCOPES[..12]))
            .unwrap();
        let text = run_status(&repo, &state_root, None);
        assert!(
            text.contains(
                "\n    lacks user-read-recently-played, user-top-read; run `sss spotify auth \
                 --profile alice` to sign in again\n"
            ),
            "{text}"
        );
        let json = serde_json::to_value(report(&repo, &state_root, None)).unwrap();
        assert_eq!(
            json["profiles"][0]["spotify_token"]["missing_scopes"],
            serde_json::json!(["user-read-recently-played", "user-top-read"])
        );

        state.save_spotify_token(&token_with(SCOPES)).unwrap();
        let text = run_status(&repo, &state_root, None);
        assert!(!text.contains("lacks"), "{text}");
        let json = serde_json::to_value(report(&repo, &state_root, None)).unwrap();
        assert_eq!(
            json["profiles"][0]["spotify_token"]["missing_scopes"],
            serde_json::json!([])
        );
    }

    #[test]
    fn an_unreadable_session_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = empty(tmp.path());
        let state = state_root.profile("alice").unwrap();
        std::fs::create_dir_all(state.dir()).unwrap();
        std::fs::write(state.pandora_session_path(), "not json").unwrap();
        let config = Config::load(&repo.config_path()).unwrap();
        let err = status(&repo, &state_root, &config, None).unwrap_err();
        assert!(
            format!("{err:#}").contains("pandora-session.json"),
            "{err:#}"
        );
    }
}
