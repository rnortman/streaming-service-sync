//! The LLM matcher: items the deterministic step could not decide go, in
//! batches, to the `music-matcher` agent run through `ccc`.
//!
//! The agent definition is embedded and written under the state root with
//! an agents config beside it, which every `ccc` child gets as
//! `CCC_AGENTS_CONFIG`. A batch is one `ccc run`; a reply that does not
//! parse or validate is sent back with `ccc resume` and the exact complaint,
//! up to `[matcher] max_format_retries` times. Every batch's exchange is
//! kept in `matches/llm/<session>.json`.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::MatcherConfig;
use crate::error::Kind;
use crate::fail;
use crate::matching::model::{Candidate, Confidence};
use crate::matching::normalize;
use crate::matching::run::Item;
use crate::repo::{self, ProfilePaths};
use crate::spotify::ids::IdKind;
use crate::spotify::item::Offered;

/// The embedded agent definition.
pub const DEFINITION: &str = include_str!("../../agents/music-matcher.md");

/// The agent type the embedded definition defines.
pub const DEFINED_AGENT: &str = "music-matcher";

/// Names the `ccc` executable to run instead of `ccc` on `PATH`.
pub const CCC_ENV: &str = "SSS_CCC";

/// The `ccc` executable: `$SSS_CCC`, else `ccc` from `PATH`.
pub fn ccc_program() -> OsString {
    std::env::var_os(CCC_ENV)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ccc".into())
}

/// One item of a batch, as the prompt shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BatchItem {
    /// The Pandora id; a decision names it.
    pub item: String,
    pub kind: IdKind,
    pub pandora: Value,
    pub candidates: Vec<Offered>,
}

impl BatchItem {
    /// The batch form of `item` with `candidates`: a track's title, artist,
    /// album, duration, explicit hint, title flags, stations and playlists;
    /// an artist's name, the stations it seeds and up to five thumbed-up
    /// titles from them; an album's name and artist.
    pub fn new(kind: IdKind, item: &Item, candidates: &[Candidate]) -> Self {
        let pandora = match kind {
            IdKind::Track => {
                let flags = item
                    .name
                    .as_deref()
                    .map(|t| normalize::title(t).flags)
                    .unwrap_or_default();
                json!({
                    "title": item.name,
                    "artist": item.artist,
                    "album": item.album,
                    "duration_s": item.duration_s,
                    "explicit_hint": item.explicit_hint,
                    "flags": flags,
                    "station_names": item.stations,
                    "playlist_names": item.playlists,
                })
            }
            IdKind::Artist => json!({
                "name": item.name,
                "station_names": item.stations,
                "station_thumbs_up": item.context_titles,
            }),
            IdKind::Album => json!({"name": item.name, "artist": item.artist}),
        };
        BatchItem {
            item: item.pandora_id.clone(),
            kind,
            pandora,
            candidates: candidates.iter().map(Offered::from).collect(),
        }
    }

    fn offers(&self, id: &str) -> bool {
        self.candidates.iter().any(|c| c.id == id)
    }
}

/// The LLM's decision for one item.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LlmDecision {
    pub item: String,
    /// The chosen candidate's id (its URI); `None` when nothing qualifies.
    pub choice: Option<String>,
    pub confidence: Confidence,
    pub reason: Option<String>,
}

/// What became of a batch.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// One decision per item.
    Decided(Vec<LlmDecision>),
    /// No usable reply; the items are `llm_failed` with this reason.
    Failed(String),
}

/// A batch's verdict and the ccc session behind it.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub session: Option<String>,
    pub verdict: Verdict,
}

/// Decides batches of items. An `Err` ends the matching run: ccc's usage
/// limit (kind `quota`), ccc refusing to run (kind `config`), or a failed
/// write. A bad batch is an `Ok` [`Verdict::Failed`].
pub trait Matcher {
    fn decide(&mut self, kind: IdKind, batch: &[BatchItem]) -> Result<Outcome>;
}

/// The prompt for `batch`.
pub fn render_prompt(kind: IdKind, batch: &[BatchItem]) -> Result<String> {
    let items = serde_json::to_string_pretty(batch).context("rendering the batch")?;
    Ok(format!(
        "Match these {n} Pandora {noun} to the Spotify candidates offered for each.\n\
         Reply with only the JSON object \
         {{\"decisions\":[{{\"item\":\"…\",\"choice\":\"<candidate id>\"|null,\
         \"confidence\":\"{confidence}\",\"reason\":\"…\"}}]}}, \
         one decision per item.\n\nItems:\n{items}\n",
        n = batch.len(),
        noun = kind.plural(),
        confidence = Confidence::ALL.map(Confidence::as_str).join("|"),
    ))
}

/// Every confidence's JSON spelling, quoted: `"high", "medium", "low"`.
fn confidence_spellings() -> String {
    Confidence::ALL
        .map(|c| format!("{:?}", c.as_str()))
        .join(", ")
}

/// The first balanced `{…}` in `text` that parses as JSON and has a
/// `decisions` key, skipping prose, code fences and anything before or
/// after it. When there is none, the reading of the first `{`: the object
/// it opens, or the parse error of that object — or of the rest of the
/// text when the object never closes — so a broken document is reported
/// as such, not as one of the decisions nested in it.
fn extract_object(text: &str) -> std::result::Result<Value, String> {
    let mut first = None;
    for (start, _) in text.match_indices('{') {
        let rest = &text[start..];
        let parsed = match balanced_end(rest) {
            Some(end) => serde_json::from_str::<Value>(&rest[..end]),
            None if first.is_none() => serde_json::from_str::<Value>(rest),
            None => continue,
        };
        match parsed {
            Ok(value) if value.get("decisions").is_some() => return Ok(value),
            parsed => {
                first.get_or_insert_with(|| {
                    parsed.map_err(|e| format!("the JSON object does not parse: {e}"))
                });
            }
        }
    }
    first.unwrap_or_else(|| Err("the reply holds no JSON object".into()))
}

/// The length of the balanced object `text` starts with, strings and
/// escapes respected.
fn balanced_end(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in text.char_indices() {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// One thing wrong with a decisions document: the item it concerns, or
/// `None` for the document as a whole, and the full line describing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub item: Option<String>,
    pub problem: String,
}

impl Problem {
    fn of(item: Option<&str>, problem: String) -> Self {
        Problem {
            item: item.map(str::to_owned),
            problem,
        }
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.problem)
    }
}

/// Which of the offered items a decisions document must answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completeness {
    /// Every one: a missing item is a problem.
    Every,
    /// Any subset, down to none.
    Any,
}

/// Reads and validates a decisions document against the `offered` items:
/// no item decided twice, every choice `null` or an id offered for that
/// item, every confidence `high`, `medium` or `low`; with
/// [`Completeness::Every`], every offered item decided. An item not in
/// `offered` is described by `unknown(id)`, so every decision returned names
/// an item in `offered`. A `reason` that is not a string
/// is read as none. The error lists every problem, in document order.
pub fn parse_decisions(
    text: &str,
    offered: &[BatchItem],
    completeness: Completeness,
    unknown: impl Fn(&str) -> String,
) -> std::result::Result<Vec<LlmDecision>, Vec<Problem>> {
    let value = extract_object(text).map_err(|e| vec![Problem::of(None, e)])?;
    let Some(list) = value.get("decisions").and_then(Value::as_array) else {
        return Err(vec![Problem::of(
            None,
            "the object has no \"decisions\" array".into(),
        )]);
    };
    let mut problems = Vec::new();
    let mut decisions: Vec<LlmDecision> = Vec::new();
    let mut answered: Vec<&str> = Vec::new();
    for (i, d) in list.iter().enumerate() {
        let Some(id) = d.get("item").and_then(Value::as_str) else {
            problems.push(Problem::of(
                None,
                format!("decision {} has no \"item\" string", i + 1),
            ));
            continue;
        };
        let item = Some(id);
        let Some(offer) = offered.iter().find(|b| b.item == id) else {
            problems.push(Problem::of(item, unknown(id)));
            continue;
        };
        if answered.contains(&id) {
            problems.push(Problem::of(
                item,
                format!("item {id} is decided more than once"),
            ));
            continue;
        }
        answered.push(id);
        let choice = match d.get("choice") {
            None | Some(Value::Null) => None,
            Some(Value::String(c)) if offer.offers(c) => Some(c.clone()),
            Some(Value::String(c)) => {
                problems.push(Problem::of(
                    item,
                    format!("item {id}: choice {c:?} was not offered for it"),
                ));
                continue;
            }
            Some(other) => {
                problems.push(Problem::of(
                    item,
                    format!("item {id}: choice must be a candidate id or null, not {other}"),
                ));
                continue;
            }
        };
        let confidence = match d.get("confidence").cloned().map(serde_json::from_value) {
            Some(Ok(c)) => c,
            _ => {
                problems.push(Problem::of(
                    item,
                    format!(
                        "item {id}: confidence must be one of {}",
                        confidence_spellings()
                    ),
                ));
                continue;
            }
        };
        decisions.push(LlmDecision {
            item: id.to_owned(),
            choice,
            confidence,
            reason: d.get("reason").and_then(Value::as_str).map(str::to_owned),
        });
    }
    if completeness == Completeness::Every {
        let missing: Vec<&str> = offered
            .iter()
            .map(|b| b.item.as_str())
            .filter(|id| !answered.contains(id))
            .collect();
        if !missing.is_empty() {
            problems.push(Problem::of(
                None,
                format!("no decision for {}", missing.join(", ")),
            ));
        }
    }
    if problems.is_empty() {
        Ok(decisions)
    } else {
        Err(problems)
    }
}

/// Reads and validates a `ccc` reply to `batch`: [`parse_decisions`] with
/// every item required. The error is the complaint sent back, the problems
/// joined by `"; "`.
pub fn parse_reply(
    text: &str,
    batch: &[BatchItem],
) -> std::result::Result<Vec<LlmDecision>, String> {
    parse_decisions(text, batch, Completeness::Every, |id| {
        format!("item {id:?} is not in the batch")
    })
    .map_err(|problems| join_problems(&problems))
}

/// The problems' lines joined by `"; "`.
pub fn join_problems(problems: &[Problem]) -> String {
    problems
        .iter()
        .map(|p| p.problem.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Writes `contents` to `path` unless it holds exactly that already.
fn write_if_changed(path: &Path, contents: &str) -> Result<()> {
    match fs::read_to_string(path) {
        Ok(have) if have == contents => Ok(()),
        _ => repo::write_atomic(path, contents.as_bytes()),
    }
}

/// Writes the embedded definition to `<state root>/ccc/agents/` and the
/// agents config naming that directory, with any `[matcher] model` and
/// `effort` in its `[agents.music-matcher]` table, to
/// `<state root>/ccc/agents.toml`; returns the config's path. With
/// `[matcher] agents_config` nothing is written and that path is returned.
pub fn prepare_agents(state_root: &Path, matcher: &MatcherConfig) -> Result<PathBuf> {
    if let Some(path) = &matcher.agents_config {
        return Ok(path.clone());
    }
    let dir = state_root.join("ccc");
    let agents_dir = dir.join("agents");
    write_if_changed(&agents_dir.join(format!("{DEFINED_AGENT}.md")), DEFINITION)?;
    let mut config = toml::Table::new();
    config.insert(
        "agents_dir".into(),
        toml::Value::String(agents_dir.display().to_string()),
    );
    let mut overrides = toml::Table::new();
    for (key, value) in [("model", &matcher.model), ("effort", &matcher.effort)] {
        if let Some(v) = value {
            overrides.insert(key.into(), toml::Value::String(v.clone()));
        }
    }
    if !overrides.is_empty() {
        let mut agents = toml::Table::new();
        agents.insert(DEFINED_AGENT.into(), toml::Value::Table(overrides));
        config.insert("agents".into(), toml::Value::Table(agents));
    }
    let path = dir.join("agents.toml");
    let text = toml::to_string(&config).context("rendering the ccc agents config")?;
    write_if_changed(&path, &text)?;
    Ok(path)
}

/// How one `ccc` child ended.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct Exit {
    /// `None` when killed at the time limit or by a signal.
    code: Option<i32>,
    timed_out: bool,
    stdout: String,
    stderr: String,
}

/// One `ccc` invocation of a batch, as the audit record keeps it.
#[derive(Debug, Clone, Serialize)]
struct Turn {
    command: &'static str,
    /// The complaint a `resume` sent.
    complaint: Option<String>,
    session: Option<String>,
    exit: Option<i32>,
    timed_out: bool,
    result: Option<String>,
    stderr: String,
}

/// `matches/llm/<session>.json`: one batch's full exchange.
#[derive(Debug, Clone, Serialize)]
struct Audit<'a> {
    kind: IdKind,
    items: Vec<&'a str>,
    prompt: &'a str,
    turns: Vec<Turn>,
    decisions: Vec<LlmDecision>,
    failure: Option<String>,
}

/// What a finished child said.
enum Reply {
    Answer {
        session: String,
        result: String,
    },
    /// Exit 1 or 3, or no JSON result: worth one fresh run.
    Failed(String),
    TimedOut,
}

/// The `ccc`-backed [`Matcher`].
pub struct Ccc {
    program: OsString,
    agent: String,
    agents_config: PathBuf,
    timeout: Duration,
    max_format_retries: u32,
    /// The agent's working directory: empty, so it has nothing to touch.
    scratch: tempfile::TempDir,
    paths: ProfilePaths,
}

impl Ccc {
    /// Writes the agent files (see [`prepare_agents`]) and checks with `ccc
    /// which` that `[matcher] agent` resolves; a `ccc` that cannot be run
    /// or cannot resolve the agent is kind `config`.
    pub fn prepare(
        program: OsString,
        state_root: &Path,
        matcher: &MatcherConfig,
        paths: &ProfilePaths,
    ) -> Result<Self> {
        let ccc = Ccc {
            program,
            agent: matcher.agent.clone(),
            agents_config: prepare_agents(state_root, matcher)?,
            timeout: Duration::from_secs(matcher.timeout_s),
            max_format_retries: matcher.max_format_retries,
            scratch: tempfile::tempdir().context("creating the matcher's scratch directory")?,
            paths: paths.clone(),
        };
        let exit = ccc.execute(&[OsStr::new("which"), OsStr::new(&ccc.agent)])?;
        if exit.code != Some(0) {
            fail!(
                Kind::Config,
                "ccc cannot resolve the agent {} ({}){}; check the ccc install and \
                 [matcher] in config.toml, or run `sss match --no-llm`",
                ccc.agent,
                describe_exit(&exit),
                stderr_tail(&exit.stderr)
            );
        }
        Ok(ccc)
    }

    /// Runs `ccc <args>` with the agents config set, killing its process
    /// group at the time limit.
    fn execute(&self, args: &[&OsStr]) -> Result<Exit> {
        let mut child = match Command::new(&self.program)
            .args(args)
            .env("CCC_AGENTS_CONFIG", &self.agents_config)
            .current_dir(self.scratch.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => fail!(
                Kind::Config,
                "cannot run ccc ({}): {e}; install ccc, set {CCC_ENV}, or run `sss match --no-llm`",
                self.program.to_string_lossy()
            ),
        };
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child.try_wait().context("waiting for ccc")? {
                break Some(status);
            }
            if Instant::now() >= deadline {
                kill_group(&mut child);
                break None;
            }
            thread::sleep(Duration::from_millis(20));
        };
        let Some(status) = status else {
            // Readers are left to finish on their own: a descendant outside
            // the group may still hold the pipes.
            return Ok(Exit {
                code: None,
                timed_out: true,
                stdout: String::new(),
                stderr: String::new(),
            });
        };
        Ok(Exit {
            code: status.code(),
            timed_out: false,
            stdout: stdout.join().unwrap_or_default(),
            stderr: stderr.join().unwrap_or_default(),
        })
    }

    /// Reads a finished child: exit 4 (usage limit) and 2 (ccc refused) end
    /// the run.
    fn read_reply(&self, exit: &Exit) -> Result<Reply> {
        if exit.timed_out {
            return Ok(Reply::TimedOut);
        }
        match exit.code {
            Some(0) => {}
            Some(4) => fail!(
                Kind::Quota,
                "ccc reached a usage limit no other account could take over (exit 4); \
                 every decision so far is recorded — re-run `sss match` later"
            ),
            Some(2) => fail!(
                Kind::Config,
                "ccc refused to run the matcher (exit 2){}",
                stderr_tail(&exit.stderr)
            ),
            _ => {
                return Ok(Reply::Failed(format!(
                    "ccc failed ({}){}",
                    describe_exit(exit),
                    stderr_tail(&exit.stderr)
                )));
            }
        }
        let parsed: Option<(String, String)> = serde_json::from_str::<Value>(&exit.stdout)
            .ok()
            .and_then(|v| {
                Some((
                    v.get("session")?.as_str()?.to_owned(),
                    v.get("result")?.as_str()?.to_owned(),
                ))
            });
        Ok(match parsed {
            Some((session, result)) => Reply::Answer { session, result },
            None => Reply::Failed("ccc printed no JSON result".into()),
        })
    }

    /// Runs `args`, records the turn and reads the reply.
    fn turn(
        &self,
        command: &'static str,
        args: &[&OsStr],
        complaint: Option<String>,
        turns: &mut Vec<Turn>,
    ) -> Result<Reply> {
        let exit = self.execute(args)?;
        let reply = self.read_reply(&exit);
        let (session, result) = match &reply {
            Ok(Reply::Answer { session, result }) => (Some(session.clone()), Some(result.clone())),
            _ => (None, None),
        };
        turns.push(Turn {
            command,
            complaint,
            session,
            exit: exit.code,
            timed_out: exit.timed_out,
            result,
            stderr: exit.stderr,
        });
        reply
    }

    /// The batch's exchange: a fresh `run`, `resume` with each complaint up
    /// to the retry cap, and one more fresh `run` after a failed child.
    fn exchange(
        &self,
        prompt_file: &Path,
        batch: &[BatchItem],
        turns: &mut Vec<Turn>,
        session: &mut Option<String>,
    ) -> Result<Verdict> {
        let run_args = [
            OsStr::new("run"),
            OsStr::new(&self.agent),
            OsStr::new("--json"),
            OsStr::new("--cwd"),
            self.scratch.path().as_os_str(),
            OsStr::new("--prompt-file"),
            prompt_file.as_os_str(),
        ];
        let mut failure = String::new();
        'fresh: for _ in 0..2 {
            let mut reply = self.turn("run", &run_args, None, turns)?;
            let mut retries = 0;
            loop {
                let (id, result) = match reply {
                    Reply::Answer { session, result } => (session, result),
                    Reply::Failed(why) => {
                        failure = why;
                        continue 'fresh;
                    }
                    Reply::TimedOut => {
                        return Ok(Verdict::Failed(format!(
                            "ccc was stopped after [matcher] timeout_s = {} s",
                            self.timeout.as_secs()
                        )));
                    }
                };
                *session = Some(id.clone());
                let complaint = match parse_reply(&result, batch) {
                    Ok(decisions) => return Ok(Verdict::Decided(decisions)),
                    Err(complaint) => complaint,
                };
                if retries == self.max_format_retries {
                    return Ok(Verdict::Failed(format!(
                        "the reply was still malformed after {retries} retries: {complaint}"
                    )));
                }
                retries += 1;
                let message =
                    format!("{complaint}. Reply with only the JSON object, nothing else.");
                reply = self.turn(
                    "resume",
                    &[
                        OsStr::new("resume"),
                        OsStr::new(&id),
                        OsStr::new("--json"),
                        OsStr::new(&message),
                    ],
                    Some(complaint),
                    turns,
                )?;
            }
        }
        Ok(Verdict::Failed(failure))
    }

    /// Writes the audit record, named by the session or, when ccc never
    /// returned one, by the time.
    fn write_audit(&self, session: Option<&str>, audit: &Audit<'_>) -> Result<()> {
        let name = match session {
            Some(s) => s.to_owned(),
            None => format!(
                "unrecorded-{}",
                repo::now_rfc3339()?
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .collect::<String>()
            ),
        };
        repo::write_json(&self.paths.llm_session(&name), audit)
    }
}

impl Matcher for Ccc {
    fn decide(&mut self, kind: IdKind, batch: &[BatchItem]) -> Result<Outcome> {
        let prompt = render_prompt(kind, batch)?;
        let prompt_dir = tempfile::tempdir().context("creating the prompt directory")?;
        let prompt_file = prompt_dir.path().join("prompt.md");
        fs::write(&prompt_file, &prompt)
            .with_context(|| format!("writing {}", prompt_file.display()))?;
        let mut turns = Vec::new();
        let mut session = None;
        let verdict = self.exchange(&prompt_file, batch, &mut turns, &mut session);
        let mut audit = Audit {
            kind,
            items: batch.iter().map(|b| b.item.as_str()).collect(),
            prompt: &prompt,
            turns,
            decisions: Vec::new(),
            failure: None,
        };
        match &verdict {
            Ok(Verdict::Decided(d)) => audit.decisions = d.clone(),
            Ok(Verdict::Failed(why)) => audit.failure = Some(why.clone()),
            Err(e) => audit.failure = Some(format!("{e:#}")),
        }
        let written = self.write_audit(session.as_deref(), &audit);
        let verdict = verdict?;
        written?;
        Ok(Outcome { session, verdict })
    }
}

/// Reads `pipe` to the end on a thread.
fn drain(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// Kills `child`'s process group, then `child` itself, and reaps it.
fn kill_group(child: &mut Child) {
    let group = format!("-{}", child.id());
    let _ = Command::new("kill")
        .args(["-KILL", "--", &group])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

fn describe_exit(exit: &Exit) -> String {
    match exit.code {
        Some(c) => format!("exit {c}"),
        None if exit.timed_out => "timed out".into(),
        None => "killed by a signal".into(),
    }
}

/// `: <the last lines of stderr>`, or nothing when it is empty.
fn stderr_tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.trim().lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    format!(": {}", lines[lines.len().saturating_sub(3)..].join(" / "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error;
    use crate::repo::Repo;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn candidate(uri: &str) -> Candidate {
        Candidate {
            uri: uri.into(),
            name: "Song".into(),
            artists: vec!["The Band".into()],
            duration_ms: Some(200_000),
            score: 0.8,
            ..Candidate::default()
        }
    }

    fn track_item(id: &str) -> Item {
        Item {
            pandora_id: id.into(),
            name: Some("Song (Live)".into()),
            artist: Some("The Band".into()),
            duration_s: Some(200),
            stations: vec!["Station".into()],
            playlists: vec!["Mix".into()],
            ..Item::default()
        }
    }

    fn batch(ids: &[&str]) -> Vec<BatchItem> {
        ids.iter()
            .map(|id| {
                BatchItem::new(
                    IdKind::Track,
                    &track_item(id),
                    &[candidate("spotify:track:a"), candidate("spotify:track:b")],
                )
            })
            .collect()
    }

    #[test]
    fn a_batch_item_carries_the_record_and_unscored_candidates() {
        let item = &batch(&["TR:1"])[0];
        assert_eq!(item.item, "TR:1");
        assert_eq!(item.pandora["title"], "Song (Live)");
        assert_eq!(item.pandora["flags"], json!(["live"]));
        assert_eq!(item.pandora["station_names"], json!(["Station"]));
        assert_eq!(item.pandora["playlist_names"], json!(["Mix"]));
        assert_eq!(
            serde_json::to_value(&item.candidates[0]).unwrap(),
            json!({"id": "spotify:track:a", "name": "Song", "artists": ["The Band"],
                   "duration_ms": 200_000}),
            "the URI as id, no score"
        );

        let artist = Item {
            pandora_id: "AR:1".into(),
            name: Some("Someone".into()),
            stations: vec!["Station".into()],
            context_titles: vec!["Hit".into()],
            ..Item::default()
        };
        let item = BatchItem::new(IdKind::Artist, &artist, &[]);
        assert_eq!(
            item.pandora,
            json!({"name": "Someone", "station_names": ["Station"], "station_thumbs_up": ["Hit"]})
        );
        let prompt = render_prompt(IdKind::Artist, &[item]).unwrap();
        assert!(
            prompt.starts_with("Match these 1 Pandora artists"),
            "{prompt}"
        );
        assert!(prompt.contains("\"item\": \"AR:1\""), "{prompt}");
    }

    const GOOD: &str = r#"{"decisions":[{"item":"TR:1","choice":"spotify:track:b","confidence":"high","reason":"same"}]}"#;

    #[test]
    fn the_reply_object_is_found_in_fences_and_prose() {
        let b = batch(&["TR:1"]);
        for text in [
            GOOD.to_owned(),
            format!("```json\n{GOOD}\n```"),
            format!("Here is my answer: {GOOD}\nHope that helps {{smile}}."),
            format!("Note {{this}} first.\n{GOOD}"),
            GOOD.replace("same", "a \\\"}\\\" in a string"),
        ] {
            let d = parse_reply(&text, &b).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(d[0].choice.as_deref(), Some("spotify:track:b"), "{text}");
            assert_eq!(d[0].confidence, Confidence::High);
        }
        let d = parse_reply(
            r#"{"decisions":[{"item":"TR:1","choice":null,"confidence":"low"}]}"#,
            &b,
        )
        .unwrap();
        assert_eq!((d[0].choice.as_ref(), d[0].reason.as_ref()), (None, None));
    }

    #[test]
    fn a_bad_reply_is_described_exactly() {
        let b = batch(&["TR:1", "TR:2"]);
        let cases = [
            ("no json here", "the reply holds no JSON object"),
            (
                "{not json}",
                "the JSON object does not parse: key must be a string at line 1 column 2",
            ),
            (r#"{"answers":[]}"#, "the object has no \"decisions\" array"),
            (
                r#"{"decisions":[{"item":"TR:1","choice":"spotify:track:z","confidence":"high"},
                                 {"item":"TR:9","choice":null,"confidence":"high"}]}"#,
                "item TR:1: choice \"spotify:track:z\" was not offered for it; \
                 item \"TR:9\" is not in the batch; no decision for TR:2",
            ),
            (
                r#"{"decisions":[{"item":"TR:1","choice":null,"confidence":"sure"},
                                 {"item":"TR:2","choice":null,"confidence":"low"},
                                 {"item":"TR:2","choice":null,"confidence":"low"},
                                 {"choice":null}]}"#,
                "item TR:1: confidence must be one of \"high\", \"medium\", \"low\"; \
                 item TR:2 is decided more than once; decision 4 has no \"item\" string",
            ),
            (
                r#"{"decisions":[{"item":"TR:1","choice":3,"confidence":"low"},
                                 {"item":"TR:2","choice":null,"confidence":"low"}]}"#,
                "item TR:1: choice must be a candidate id or null, not 3",
            ),
        ];
        for (text, want) in cases {
            assert_eq!(parse_reply(text, &b).unwrap_err(), want, "{text}");
        }
    }

    #[test]
    fn a_broken_document_is_reported_as_broken_not_by_a_decision_inside_it() {
        let b = batch(&["TR:1", "TR:2"]);
        let cases = [
            // A trailing comma: the outer object is balanced but does not
            // parse, while each decision in it does.
            (
                r#"{"decisions":[{"item":"TR:1","choice":null,"confidence":"high"},]}"#,
                "the JSON object does not parse: trailing comma at line 1 column 65",
            ),
            // Cut short, as a paste stops at a blank line: the outer object
            // never closes.
            (
                "{\"decisions\": [\n  {\"item\": \"TR:1\", \"choice\": null, \"confidence\": \"high\"},\n",
                "the JSON object does not parse: EOF while parsing a value at line 3 column 0",
            ),
        ];
        for (text, want) in cases {
            let problems =
                parse_decisions(text, &b, Completeness::Any, |id| id.into()).unwrap_err();
            assert_eq!(join_problems(&problems), want, "{text}");
        }
        // Braces in prose before the document do not hide it.
        let text = "Here is {my answer}, not { this one:\n\
                    {\"decisions\":[{\"item\":\"TR:1\",\"choice\":null,\"confidence\":\"low\"}]}";
        let d = parse_decisions(text, &b, Completeness::Any, |id| id.into()).unwrap();
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn any_subset_may_be_answered_and_unknown_items_carry_the_callers_line() {
        let b = batch(&["TR:1", "TR:2", "TR:3"]);
        let unknown = |id: &str| format!("item {id:?} has no open question");
        let one = r#"{"decisions":[{"item":"TR:2","choice":"spotify:track:a","confidence":"medium","reason":7}]}"#;
        let d = parse_decisions(one, &b, Completeness::Any, unknown).unwrap();
        assert_eq!(
            d,
            [LlmDecision {
                item: "TR:2".into(),
                choice: Some("spotify:track:a".into()),
                confidence: Confidence::Medium,
                reason: None,
            }],
            "a reason that is not a string is none"
        );
        assert_eq!(
            parse_reply(one, &b).unwrap_err(),
            "no decision for TR:1, TR:3",
            "the ccc path still needs every item"
        );
        assert!(
            parse_decisions(r#"{"decisions":[]}"#, &b, Completeness::Any, unknown)
                .unwrap()
                .is_empty()
        );

        let bad = r#"{"decisions":[{"item":"TR:9","choice":null,"confidence":"low"},
                                   {"choice":null},
                                   {"item":"TR:1","choice":"spotify:track:z","confidence":"low"}]}"#;
        let problems = parse_decisions(bad, &b, Completeness::Any, unknown).unwrap_err();
        assert_eq!(
            problems,
            [
                Problem::of(Some("TR:9"), "item \"TR:9\" has no open question".into()),
                Problem::of(None, "decision 2 has no \"item\" string".into()),
                Problem::of(
                    Some("TR:1"),
                    "item TR:1: choice \"spotify:track:z\" was not offered for it".into()
                ),
            ]
        );
        assert!(
            problems
                .iter()
                .all(|p| !p.problem.contains("no decision for"))
        );
        assert_eq!(
            problems[0].to_string(),
            "item \"TR:9\" has no open question"
        );
        assert_eq!(problems[1].to_string(), "decision 2 has no \"item\" string");
        assert_eq!(
            parse_decisions("nothing", &b, Completeness::Any, unknown).unwrap_err(),
            [Problem::of(None, "the reply holds no JSON object".into())]
        );
    }

    #[test]
    fn every_problem_of_a_full_reply_is_kept_apart() {
        let b = batch(&["TR:1", "TR:2"]);
        let text = r#"{"decisions":[{"item":"TR:1","choice":"spotify:track:z","confidence":"high"},
                                     {"item":"TR:9","choice":null,"confidence":"high"}]}"#;
        let problems = parse_decisions(text, &b, Completeness::Every, |id| {
            format!("item {id:?} is not in the batch")
        })
        .unwrap_err();
        let items: Vec<Option<&str>> = problems.iter().map(|p| p.item.as_deref()).collect();
        assert_eq!(items, [Some("TR:1"), Some("TR:9"), None]);
        assert_eq!(join_problems(&problems), parse_reply(text, &b).unwrap_err());
    }

    #[test]
    fn a_batch_item_names_its_kind_second() {
        let item = &batch(&["TR:1"])[0];
        assert_eq!(item.kind, IdKind::Track);
        let json = serde_json::to_string(item).unwrap();
        assert!(
            json.starts_with(r#"{"item":"TR:1","kind":"track","pandora":"#),
            "{json}"
        );
        let prompt = render_prompt(IdKind::Track, &batch(&["TR:1", "TR:2"])).unwrap();
        assert!(
            prompt.starts_with("Match these 2 Pandora tracks"),
            "{prompt}"
        );
        assert!(prompt.contains("\"kind\": \"track\""), "{prompt}");
    }

    #[test]
    fn the_embedded_definition_pins_the_tools_and_harness() {
        let front = DEFINITION
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---\n"))
            .map(|(front, _)| front)
            .unwrap();
        let lines: Vec<&str> = front.lines().collect();
        for want in [
            "name: music-matcher",
            "tools: Glob",
            "harness: claude",
            "model: claude-opus-5-5[1M]",
            "effort: low",
        ] {
            assert!(lines.contains(&want), "{want} in {front}");
        }
    }

    #[test]
    fn the_agent_files_are_written_once_and_rewritten_on_change() {
        let tmp = tempfile::tempdir().unwrap();
        let mut matcher = MatcherConfig::default();
        let path = prepare_agents(tmp.path(), &matcher).unwrap();
        assert_eq!(path, tmp.path().join("ccc/agents.toml"));
        let definition = tmp.path().join("ccc/agents/music-matcher.md");
        assert_eq!(fs::read_to_string(&definition).unwrap(), DEFINITION);
        let config: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            config["agents_dir"].as_str(),
            Some(tmp.path().join("ccc/agents").to_str().unwrap())
        );
        assert!(!config.contains_key("agents"));

        let inode = |p: &Path| fs::metadata(p).unwrap().ino();
        let (def_ino, config_ino) = (inode(&definition), inode(&path));
        prepare_agents(tmp.path(), &matcher).unwrap();
        assert_eq!((inode(&definition), inode(&path)), (def_ino, config_ino));

        matcher.model = Some("other-model".into());
        matcher.effort = Some("high".into());
        prepare_agents(tmp.path(), &matcher).unwrap();
        assert_eq!(inode(&definition), def_ino);
        assert_ne!(inode(&path), config_ino);
        let config: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            config["agents"]["music-matcher"]["model"].as_str(),
            Some("other-model")
        );
        assert_eq!(
            config["agents"]["music-matcher"]["effort"].as_str(),
            Some("high")
        );

        let own = MatcherConfig {
            agents_config: Some("/elsewhere/agents.toml".into()),
            ..MatcherConfig::default()
        };
        let other = tempfile::tempdir().unwrap();
        assert_eq!(
            prepare_agents(other.path(), &own).unwrap(),
            Path::new("/elsewhere/agents.toml")
        );
        assert!(!other.path().join("ccc").exists());
    }

    // TODO(fake-ccc-etxtbsy): writing this script races other tests' spawns.
    /// A fake `ccc` in `dir`: logs its arguments, exits `which` for `which`,
    /// and on its Nth other call sleeps `sleepN` seconds when that file
    /// exists, prints `outN` and exits `exitN` (default 0).
    fn fake_ccc(dir: &Path, which: i32) -> OsString {
        let path = dir.join("ccc");
        let d = dir.display();
        fs::write(
            &path,
            format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$*\" >> '{d}/argv'\n\
                 if [ \"$1\" = which ]; then echo 'no definition for agent type' >&2; exit {which}; fi\n\
                 n=$(( $(cat '{d}/count' 2>/dev/null || echo 0) + 1 ))\n\
                 echo $n > '{d}/count'\n\
                 [ -f \"{d}/sleep$n\" ] && sleep $(cat \"{d}/sleep$n\")\n\
                 cat \"{d}/out$n\" 2>/dev/null\n\
                 exit $(cat \"{d}/exit$n\" 2>/dev/null || echo 0)\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path.into_os_string()
    }

    fn reply(dir: &Path, n: u32, session: &str, result: &str) {
        let out = json!({"session": session, "result": result});
        fs::write(dir.join(format!("out{n}")), out.to_string()).unwrap();
    }

    fn exits(dir: &Path, n: u32, code: i32) {
        fs::write(dir.join(format!("exit{n}")), code.to_string()).unwrap();
    }

    struct Rig {
        tmp: tempfile::TempDir,
        fake: PathBuf,
        paths: ProfilePaths,
    }

    fn new_rig() -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake");
        fs::create_dir_all(&fake).unwrap();
        let paths = Repo::at(tmp.path().join("data")).profile("alice").unwrap();
        Rig { tmp, fake, paths }
    }

    impl Rig {
        fn ccc(&self, matcher: &MatcherConfig) -> Result<Ccc> {
            Ccc::prepare(
                fake_ccc(&self.fake, 0),
                &self.tmp.path().join("state"),
                matcher,
                &self.paths,
            )
        }

        fn argv(&self) -> Vec<String> {
            fs::read_to_string(self.fake.join("argv"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn audit(&self, name: &str) -> Value {
            repo::read_json(&self.paths.llm_session(name)).unwrap()
        }
    }

    #[test]
    fn a_malformed_reply_is_resumed_with_the_complaint() {
        let rig = new_rig();
        reply(&rig.fake, 1, "sess-1", "Sure! {\"decisions\": []}");
        reply(&rig.fake, 2, "sess-1", &format!("```json\n{GOOD}\n```"));
        let mut ccc = rig.ccc(&MatcherConfig::default()).unwrap();
        let outcome = ccc.decide(IdKind::Track, &batch(&["TR:1"])).unwrap();
        assert_eq!(outcome.session.as_deref(), Some("sess-1"));
        let Verdict::Decided(d) = outcome.verdict else {
            panic!("{:?}", outcome.verdict)
        };
        assert_eq!(d[0].choice.as_deref(), Some("spotify:track:b"));

        let argv = rig.argv();
        assert_eq!(argv[0], "which music-matcher");
        assert_eq!(
            argv[1],
            format!(
                "run music-matcher --json --cwd {} --prompt-file {}",
                ccc.scratch.path().display(),
                argv[1].rsplit(' ').next().unwrap()
            )
        );
        assert_eq!(
            argv[2],
            "resume sess-1 --json no decision for TR:1. Reply with only the JSON object, nothing else."
        );
        assert!(
            fs::read_dir(ccc.scratch.path()).unwrap().next().is_none(),
            "the agent's directory holds nothing"
        );

        let audit = rig.audit("sess-1");
        assert_eq!(audit["kind"], "track");
        assert_eq!(audit["items"], json!(["TR:1"]));
        assert!(audit["prompt"].as_str().unwrap().contains("Song (Live)"));
        assert_eq!(audit["turns"][0]["command"], "run");
        assert_eq!(audit["turns"][1]["complaint"], "no decision for TR:1");
        assert_eq!(audit["decisions"][0]["choice"], "spotify:track:b");
        assert_eq!(audit["failure"], Value::Null);
    }

    #[test]
    fn the_format_retries_are_capped() {
        let rig = new_rig();
        for n in 1..=2 {
            reply(&rig.fake, n, "sess-1", "no idea");
        }
        let matcher = MatcherConfig {
            max_format_retries: 1,
            ..MatcherConfig::default()
        };
        let outcome = rig
            .ccc(&matcher)
            .unwrap()
            .decide(IdKind::Track, &batch(&["TR:1"]))
            .unwrap();
        assert_eq!(
            outcome.verdict,
            Verdict::Failed(
                "the reply was still malformed after 1 retries: the reply holds no JSON object"
                    .into()
            )
        );
        assert_eq!(rig.argv().len(), 3, "which, run, one resume");
        assert_eq!(
            rig.audit("sess-1")["failure"],
            "the reply was still malformed after 1 retries: the reply holds no JSON object"
        );
    }

    #[test]
    fn a_failed_child_gets_one_fresh_run() {
        let rig = new_rig();
        exits(&rig.fake, 1, 1);
        reply(&rig.fake, 2, "sess-2", GOOD);
        let outcome = rig
            .ccc(&MatcherConfig::default())
            .unwrap()
            .decide(IdKind::Track, &batch(&["TR:1"]))
            .unwrap();
        assert!(matches!(outcome.verdict, Verdict::Decided(_)));
        assert_eq!(outcome.session.as_deref(), Some("sess-2"));

        let rig = new_rig();
        exits(&rig.fake, 1, 3);
        exits(&rig.fake, 2, 1);
        let outcome = rig
            .ccc(&MatcherConfig::default())
            .unwrap()
            .decide(IdKind::Track, &batch(&["TR:1"]))
            .unwrap();
        assert_eq!(outcome.session, None);
        let Verdict::Failed(why) = outcome.verdict else {
            panic!("{:?}", outcome.verdict)
        };
        assert_eq!(why, "ccc failed (exit 1)");
        let runs: Vec<String> = rig.argv().into_iter().skip(1).collect();
        assert!(
            runs.len() == 2 && runs.iter().all(|a| a.starts_with("run ")),
            "{runs:?}"
        );
        let audits: Vec<_> = fs::read_dir(rig.paths.matches_dir().join("llm"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            audits.len() == 1 && audits[0].starts_with("unrecorded-"),
            "{audits:?}"
        );
    }

    #[test]
    fn a_usage_limit_or_a_refusal_ends_the_run() {
        let rig = new_rig();
        exits(&rig.fake, 1, 4);
        let err = rig
            .ccc(&MatcherConfig::default())
            .unwrap()
            .decide(IdKind::Track, &batch(&["TR:1"]))
            .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Quota);
        assert!(format!("{err:#}").contains("re-run `sss match` later"));
        assert_eq!(
            fs::read_dir(rig.paths.matches_dir().join("llm"))
                .unwrap()
                .count(),
            1,
            "the exchange is kept"
        );

        let rig = new_rig();
        exits(&rig.fake, 1, 2);
        let err = rig
            .ccc(&MatcherConfig::default())
            .unwrap()
            .decide(IdKind::Track, &batch(&["TR:1"]))
            .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Config);
    }

    #[test]
    fn ccc_that_cannot_run_or_resolve_the_agent_is_a_config_error() {
        let rig = new_rig();
        let state = rig.tmp.path().join("state");
        let program = fake_ccc(&rig.fake, 2);
        let err = Ccc::prepare(program, &state, &MatcherConfig::default(), &rig.paths)
            .err()
            .unwrap();
        assert_eq!(error::kind(&err), Kind::Config);
        let message = format!("{err:#}");
        assert!(
            message.contains("music-matcher (exit 2): no definition for agent type"),
            "{message}"
        );
        let err = Ccc::prepare(
            rig.tmp.path().join("missing").into_os_string(),
            &state,
            &MatcherConfig::default(),
            &rig.paths,
        )
        .err()
        .unwrap();
        assert_eq!(error::kind(&err), Kind::Config);
        assert!(format!("{err:#}").contains(CCC_ENV));
    }

    #[test]
    fn a_child_past_the_time_limit_is_killed_and_the_batch_fails() {
        let rig = new_rig();
        fs::write(rig.fake.join("sleep1"), "30").unwrap();
        let matcher = MatcherConfig {
            timeout_s: 1,
            ..MatcherConfig::default()
        };
        let started = Instant::now();
        let outcome = rig
            .ccc(&matcher)
            .unwrap()
            .decide(IdKind::Track, &batch(&["TR:1"]))
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(
            outcome.verdict,
            Verdict::Failed("ccc was stopped after [matcher] timeout_s = 1 s".into())
        );
        assert_eq!(rig.argv().len(), 2, "no fresh run after a timeout");
    }
}
