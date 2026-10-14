//! `sss status`: per profile, the latest export run's step ledger and the
//! match counts by status.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::pandora::export::{self, Step};
use crate::repo::{self, ProfilePaths, Repo};

/// Prints the status of `profile`, or of every profile in `repo` when none
/// is named.
pub fn status(repo: &Repo, profile: Option<&str>, out: &mut dyn Write) -> Result<()> {
    let names = match profile {
        Some(name) => vec![name.to_owned()],
        None => repo.profiles()?,
    };
    if names.is_empty() {
        writeln!(out, "no profiles yet").context("writing output")?;
    }
    for name in names {
        profile_status(&name, &repo.profile(&name)?, out)?;
    }
    Ok(())
}

fn profile_status(name: &str, paths: &ProfilePaths, out: &mut dyn Write) -> Result<()> {
    let mut lines = vec![format!("profile {name}")];
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

    fn run_status(repo: &Repo, profile: Option<&str>) -> String {
        let mut out = Vec::new();
        status(repo, profile, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn prints_the_latest_ledger_and_match_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::at(tmp.path());
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
        let text = run_status(&repo, None);
        assert!(text.starts_with("profile alice\n"), "{text}");
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
        let repo = Repo::at(tmp.path());
        assert_eq!(run_status(&repo, None), "no profiles yet\n");
        assert!(run_status(&repo, Some("bob")).contains("  export: no run yet\n"));
        let paths = repo.profile("bob").unwrap();
        std::fs::create_dir_all(paths.raw_run_dir("20261013T000000Z")).unwrap();
        let text = run_status(&repo, None);
        assert!(text.starts_with("profile bob\n"), "{text}");
        assert!(
            text.contains("  export run 20261013T000000Z: did not finish (no manifest)\n"),
            "{text}"
        );
        assert!(!text.contains("latest complete run"), "{text}");
    }
}
