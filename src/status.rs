//! `sss status`: per profile, the stored Pandora session, the latest export
//! run's step ledger and the match counts by status.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use crate::config::Config;
use crate::pandora::export::{self, Manifest, Step};
use crate::profiles;
use crate::repo::{self, ProfilePaths, Repo};
use crate::secrets::{StateDir, StateRoot, TokenSource};

/// What `sss status` reports: one entry per profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    pub profiles: Vec<ProfileStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileStatus {
    pub name: String,
    pub pandora_session: Option<SessionStatus>,
    pub export: ExportStatus,
    pub matches: MatchCounts,
}

/// A stored Pandora session, without its token values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionStatus {
    pub stored_at: String,
    pub webname: Option<String>,
    pub source: TokenSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportStatus {
    /// The newest run.
    pub latest_run: Option<RunStatus>,
    /// The newest run whose manifest says `complete`.
    pub latest_complete_run: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunStatus {
    pub run_id: String,
    /// `None` for a run that did not finish.
    pub manifest: Option<Manifest>,
}

/// Per entity, the count of each match status over the latest entry per
/// `pandora_id`.
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
    Ok(StatusReport { profiles })
}

fn profile_status(name: String, state: &StateDir, paths: &ProfilePaths) -> Result<ProfileStatus> {
    let pandora_session = state.load_pandora_session()?.map(|s| SessionStatus {
        stored_at: s.stored_at,
        webname: s.webname,
        source: s.source,
    });
    let latest_run = match export::run_ids(paths)?.pop() {
        None => None,
        Some(run_id) => Some(RunStatus {
            manifest: export::read_manifest(&paths.raw_run_dir(&run_id))?,
            run_id,
        }),
    };
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
        export: ExportStatus {
            latest_run,
            latest_complete_run,
        },
        matches: MatchCounts {
            tracks: match_counts(&paths.track_matches())?,
            artists: match_counts(&paths.artist_matches())?,
            albums: match_counts(&paths.album_matches())?,
        },
    })
}

impl StatusReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        if self.profiles.is_empty() {
            writeln!(
                out,
                "no profiles yet; run `sss pandora auth --profile NAME` first"
            )?;
        }
        for profile in &self.profiles {
            for line in profile.lines() {
                writeln!(out, "{line}")?;
            }
        }
        Ok(())
    }
}

impl ProfileStatus {
    /// The profile's block of text lines.
    fn lines(&self) -> Vec<String> {
        let name = &self.name;
        let mut lines = vec![format!("profile {name}")];
        lines.push(match &self.pandora_session {
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
        });
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
}

/// The count of each `status` over the latest entry per `pandora_id`.
fn match_counts(path: &Path) -> Result<BTreeMap<String, usize>> {
    let latest = repo::read_jsonl_latest::<Value, String, _>(path, |v| {
        v.get("pandora_id")
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
    use crate::pandora::export::{MANIFEST_FILE, Manifest, StepResult, StepStatus, Steps};
    use crate::pandora::test_support::session;

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
        for (id, status) in [
            ("TR:1", "unmatched"),
            ("TR:2", "matched"),
            ("TR:1", "matched"),
        ] {
            repo::append_jsonl(
                &paths.track_matches(),
                &serde_json::json!({"pandora_id": id, "status": status}),
            )
            .unwrap();
        }
        let text = run_status(&repo, &state_root, None);
        assert!(
            text.starts_with(
                "profile alice\n  pandora session: none (run `sss pandora auth --profile alice`)\n"
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
        assert!(text.contains("  matches tracks   matched 2\n"), "{text}");
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
            &serde_json::json!({"pandora_id": "TR:1", "status": "matched"}),
        )
        .unwrap();
        let json = serde_json::to_value(report(&repo, &state_root, None)).unwrap();
        let profile = &json["profiles"][0];
        assert_eq!(profile["name"], "alice");
        assert_eq!(profile["pandora_session"], Value::Null);
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
            "no profiles yet; run `sss pandora auth --profile NAME` first\n"
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
                 webname listener, from prompt\n  export: no run yet\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "profile bob\n  pandora session: none (run `sss pandora auth --profile bob`)\n"
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
