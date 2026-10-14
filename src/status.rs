//! `sss status`: per profile, the stored Pandora session, the latest export
//! run's step ledger and the match counts by status.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::Config;
use crate::pandora::export::{self, Step};
use crate::profiles;
use crate::repo::{self, ProfilePaths, Repo};
use crate::secrets::{StateDir, StateRoot};

/// Prints the status of `profile`, or of every known profile when none is
/// named.
pub fn status(
    repo: &Repo,
    state_root: &StateRoot,
    config: &Config,
    profile: Option<&str>,
    out: &mut dyn Write,
) -> Result<()> {
    let names = match profile {
        Some(name) => vec![name.to_owned()],
        None => profiles::known(repo, state_root, config)?,
    };
    if names.is_empty() {
        writeln!(
            out,
            "no profiles yet; run `sss pandora auth --profile NAME` first"
        )
        .context("writing output")?;
    }
    for name in names {
        profile_status(
            &name,
            &state_root.profile(&name)?,
            &repo.profile(&name)?,
            out,
        )?;
    }
    Ok(())
}

/// The `pandora session:` line of a profile's block.
fn session_line(name: &str, state: &StateDir) -> Result<String> {
    let Some(session) = state.load_pandora_session()? else {
        return Ok(format!(
            "  pandora session: none (run `sss pandora auth --profile {name}`)"
        ));
    };
    let webname = session
        .webname
        .map(|w| format!(", webname {w}"))
        .unwrap_or_default();
    let source = session.source.as_str();
    Ok(format!(
        "  pandora session: stored {}{webname}, from {source}",
        session.stored_at
    ))
}

fn profile_status(
    name: &str,
    state: &StateDir,
    paths: &ProfilePaths,
    out: &mut dyn Write,
) -> Result<()> {
    let mut lines = vec![format!("profile {name}"), session_line(name, state)?];
    let mut newest_complete = true;
    match export::run_ids(paths)?.last() {
        None => lines.push("  export: no run yet".to_owned()),
        Some(id) => match export::read_manifest(&paths.raw_run_dir(id))? {
            None => {
                newest_complete = false;
                lines.push(format!("  export run {id}: did not finish (no manifest)"));
            }
            Some(m) => {
                newest_complete = m.complete;
                let verdict = if m.complete { "complete" } else { "INCOMPLETE" };
                lines.push(format!(
                    "  export run {id}: {verdict}, {} requests, finished {}",
                    m.requests, m.finished
                ));
                for step in Step::ALL {
                    lines.push(format!(
                        "    {}",
                        export::step_line(step, m.steps.get(step))
                    ));
                }
            }
        },
    }
    if !newest_complete && let Some(m) = export::latest_complete_run(paths)? {
        lines.push(format!("  latest complete run: {}", m.run_id));
    }
    for (kind, path) in [
        ("tracks", paths.track_matches()),
        ("artists", paths.artist_matches()),
        ("albums", paths.album_matches()),
    ] {
        lines.push(format!("  matches {kind:<8} {}", match_counts(&path)?));
    }
    for line in lines {
        writeln!(out, "{line}").context("writing output")?;
    }
    Ok(())
}

/// `none` or `<status> <n>, …` over the latest entry per `pandora_id`.
fn match_counts(path: &Path) -> Result<String> {
    let latest = repo::read_jsonl_latest::<Value, String, _>(path, |v| {
        v.get("pandora_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    })?;
    if latest.is_empty() {
        return Ok("none".to_owned());
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for entry in latest.values() {
        let status = entry
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        *counts.entry(status.to_owned()).or_default() += 1;
    }
    Ok(counts
        .iter()
        .map(|(s, n)| format!("{s} {n}"))
        .collect::<Vec<_>>()
        .join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pandora::export::{MANIFEST_FILE, Manifest, StepResult, StepStatus, Steps};
    use crate::pandora::test_support::session;
    use crate::secrets::TokenSource;

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

    fn run_status(repo: &Repo, state_root: &StateRoot, profile: Option<&str>) -> String {
        let config = Config::load(&repo.config_path()).unwrap();
        let mut out = Vec::new();
        status(repo, state_root, &config, profile, &mut out).unwrap();
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
        let err = status(&repo, &state_root, &config, None, &mut Vec::new()).unwrap_err();
        assert!(
            format!("{err:#}").contains("pandora-session.json"),
            "{err:#}"
        );
    }
}
