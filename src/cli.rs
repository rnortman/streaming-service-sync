//! Command line: the `sss` subcommands and their dispatch.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::config::{self, Config};
use crate::error::{self, ExportRun, Kind};
use crate::fail;
use crate::pandora::auth::{self, AuthReport};
use crate::pandora::client::Client;
use crate::pandora::export::{self, ExportIncomplete, ExportReport, Manifest};
use crate::pandora::model::{self, Summary};
use crate::profiles;
use crate::repo::{self, Repo};
use crate::secrets::StateRoot;
use crate::status::{self, StatusReport};

/// Export a Pandora account to a local data repository and migrate it to
/// Spotify.
#[derive(Debug, Parser)]
#[command(name = "sss", version, about)]
pub struct Cli {
    /// Data repository directory [default: $XDG_DATA_HOME/streaming-service-sync]
    #[arg(long, global = true, value_name = "DIR")]
    pub repo: Option<PathBuf>,

    /// Profile (one family member's Pandora login and Spotify account);
    /// defaults to the only profile when there is one
    #[arg(long, global = true, value_name = "NAME")]
    pub profile: Option<String>,

    /// Print one JSON document on stdout instead of text; errors become
    /// `{"ok": false, "kind": …}`
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the data repository: its directory, config.toml and README
    Init,
    /// Pandora: session, export, normalization
    #[command(subcommand)]
    Pandora(PandoraCommand),
    /// Spotify: authorization
    #[command(subcommand)]
    Spotify(SpotifyCommand),
    /// Match exported tracks, artists and albums to Spotify
    Match(MatchArgs),
    /// Apply the matched material to the profile's Spotify account
    Push(PushArgs),
    /// Per profile: the latest export's step ledger, match and push counts
    Status,
    /// Write the profile's report.md and print its path
    Report,
}

#[derive(Debug, Subcommand)]
pub enum PandoraCommand {
    /// Verify and store the X-AuthToken value of the browser's session
    Auth {
        /// Read the token from a file instead of prompting: every line,
        /// trimmed, joined
        #[arg(long, value_name = "FILE")]
        from_file: Option<PathBuf>,
    },
    /// Capture everything from Pandora into a new raw run; normalize it if complete
    Export,
    /// Re-derive the normalized files from the latest complete raw run
    Normalize,
}

#[derive(Debug, Subcommand)]
pub enum SpotifyCommand {
    /// Authorize the profile's Spotify account (PKCE; paste the redirected
    /// address back)
    Auth,
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct MatchArgs {
    /// Deterministic matching only; leave ambiguous items undecided
    #[arg(long)]
    pub no_llm: bool,

    /// Match only this kind of entity
    #[arg(long, value_enum)]
    pub only: Option<MatchKind>,

    #[command(subcommand)]
    pub command: Option<MatchCommand>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MatchKind {
    Tracks,
    Artists,
    Albums,
}

#[derive(Debug, Subcommand)]
pub enum MatchCommand {
    /// List unmatched and low-confidence items with their top candidates
    Review,
    /// Record a manual match
    Set {
        /// Pandora id (TR:…, AR:…, AL:…)
        pandora_id: String,
        /// Spotify URL or URI
        spotify: String,
    },
    /// Record that an item has no Spotify counterpart
    Reject {
        /// Pandora id (TR:…, AR:…, AL:…)
        pandora_id: String,
    },
}

#[derive(Debug, Args)]
pub struct PushArgs {
    /// Print the plan with counts and change nothing
    #[arg(long)]
    pub dry_run: bool,

    /// Push only this part
    #[arg(long, value_enum)]
    pub only: Option<PushPart>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PushPart {
    Stations,
    Playlists,
    Collection,
    Liked,
}

/// The README `sss init` writes into the data repository.
pub const DATA_README: &str = "\
# streaming-service-sync data repository

Written by `sss` (streaming-service-sync). Keep this directory private: it
holds each profile's listening history. It holds no passwords or tokens;
those live in the per-profile state directory, outside this one.

Everything here is plain JSON or TOML, written deterministically so that
diffs are readable. Committing it to a private git repository by hand is
the intended way to keep its history.

```
config.toml                      settings (see the comments in it)
profiles/<name>/
  pandora/raw/<run-id>/          every Pandora response, verbatim, plus manifest.json
  pandora/*.json                 normalized stations, playlists, collection, bookmarks, tracks
  matches/*.jsonl                Pandora -> Spotify matches; the latest entry per id wins
  matches/llm/<session>.json     the full exchange of every LLM matching batch
  spotify/state.json             what has been created or saved on Spotify
  report.md                      the human-readable summary
```
";

impl Command {
    /// The command as typed, for messages.
    fn name(&self) -> &'static str {
        match self {
            Command::Init => "init",
            Command::Pandora(PandoraCommand::Auth { .. }) => "pandora auth",
            Command::Pandora(PandoraCommand::Export) => "pandora export",
            Command::Pandora(PandoraCommand::Normalize) => "pandora normalize",
            Command::Spotify(SpotifyCommand::Auth) => "spotify auth",
            Command::Match(MatchArgs { command: None, .. }) => "match",
            Command::Match(MatchArgs {
                command: Some(MatchCommand::Review),
                ..
            }) => "match review",
            Command::Match(MatchArgs {
                command: Some(MatchCommand::Set { .. }),
                ..
            }) => "match set",
            Command::Match(MatchArgs {
                command: Some(MatchCommand::Reject { .. }),
                ..
            }) => "match reject",
            Command::Push(_) => "push",
            Command::Status => "status",
            Command::Report => "report",
        }
    }

    /// Whether the command acts on one profile and so needs `--profile` or a
    /// single known profile. `status` covers every profile when none is
    /// named.
    fn needs_profile(&self) -> bool {
        !matches!(self, Command::Init | Command::Status)
    }
}

/// The result of a command that succeeded, rendered as text or as JSON.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Output {
    Init(InitReport),
    PandoraAuth(AuthReport),
    PandoraExport(Box<ExportReport>),
    PandoraNormalize(Summary),
    Status(StatusReport),
}

impl Output {
    fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        match self {
            Output::Init(r) => r.write_text(out),
            Output::PandoraAuth(r) => r.write_text(out),
            Output::PandoraExport(r) => r.write_text(out),
            Output::PandoraNormalize(r) => r.write_text(out),
            Output::Status(r) => r.write_text(out),
        }
    }
}

/// The JSON document of a command that succeeded.
#[derive(Serialize)]
struct SuccessDocument<'a> {
    ok: bool,
    command: &'a str,
    #[serde(flatten)]
    report: &'a Output,
}

/// The JSON document of a command that failed.
#[derive(Serialize)]
struct FailureDocument<'a> {
    ok: bool,
    command: &'a str,
    kind: Kind,
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Details<'a>>,
}

/// The `details` of a failed `pandora export`.
#[derive(Serialize)]
#[serde(untagged)]
enum Details<'a> {
    Incomplete {
        run_id: &'a str,
        run_dir: &'a Path,
        failed_steps: Vec<&'static str>,
        manifest: &'a Manifest,
    },
    Stopped {
        run_id: &'a str,
        run_dir: &'a Path,
    },
}

/// `value` as one pretty-printed JSON document and a newline. Nothing is
/// written until the whole document has serialized, so a value that does
/// not serialize leaves no partial document behind.
fn json_document(value: &impl Serialize) -> serde_json::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// What a command that succeeded writes to stdout: one JSON document under
/// `--json`, else its text.
fn render(cli: &Cli, output: &Output) -> Result<Vec<u8>> {
    if cli.json {
        return json_document(&SuccessDocument {
            ok: true,
            command: cli.command.name(),
            report: output,
        })
        .context("serializing the result");
    }
    let mut text = Vec::new();
    output.write_text(&mut text).context("writing output")?;
    Ok(text)
}

/// Under `--json`, writes the error object of `err` to `out`; without it,
/// writes nothing. The `error: …` line on stderr is the caller's.
pub fn report_failure(cli: &Cli, err: &anyhow::Error, out: &mut dyn Write) -> Result<()> {
    if !cli.json {
        return Ok(());
    }
    let details = match error::export_run(err) {
        Some(ExportRun::Stopped(s)) => Some(Details::Stopped {
            run_id: &s.run_id,
            run_dir: &s.run_dir,
        }),
        Some(ExportRun::Incomplete(i)) => Some(Details::Incomplete {
            run_id: &i.0.manifest.run_id,
            run_dir: &i.0.run_dir,
            failed_steps: i.failed_steps(),
            manifest: &i.0.manifest,
        }),
        None => None,
    };
    let mut document = FailureDocument {
        ok: false,
        command: cli.command.name(),
        kind: error::kind(err),
        error: format!("{err:#}"),
        details,
    };
    let bytes = match json_document(&document) {
        Ok(bytes) => bytes,
        // `details` did not serialize (a run directory whose path is not
        // UTF-8): the error object goes out without it.
        Err(_) => {
            document.details = None;
            json_document(&document).context("serializing the error object")?
        }
    };
    out.write_all(&bytes).context("writing output")
}

/// Runs the parsed command line against the profiles' state directories
/// under `state_root`. Pasted values are read from `input`; the result goes
/// to `out`, as text or, with `--json`, as one JSON document; prompts,
/// progress and warnings go to `err`, and a failed write of them is ignored.
/// A failure, including a result that does not serialize, is returned
/// unchanged after its JSON error object is written to `out` under `--json`.
pub fn run(
    cli: Cli,
    state_root: &StateRoot,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<()> {
    match dispatch(&cli, state_root, input, out, err).and_then(|output| render(&cli, &output)) {
        Ok(bytes) => out.write_all(&bytes).context("writing output"),
        Err(e) => {
            // A failure to write the error object leaves stdout broken;
            // the command's own error is still the one to report.
            let _ = report_failure(&cli, &e, out);
            Err(e)
        }
    }
}

fn dispatch(
    cli: &Cli,
    state_root: &StateRoot,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<Output> {
    let repo = Repo::locate(cli.repo.as_deref())?;
    if let Command::Init = cli.command {
        return Ok(Output::Init(init(&repo)?));
    }
    let config = Config::load(&repo.config_path())?;
    let profile = match &cli.profile {
        Some(name) => {
            repo::validate_profile_name(name)?;
            Some(name.clone())
        }
        None if cli.command.needs_profile() => {
            let name = only_profile(&cli.command, &profiles::known(&repo, state_root, &config)?)?;
            // Lines on `err` are for people: a failed write of one does not
            // end the command.
            let _ = writeln!(
                err,
                "profile {name} (the only one; pass --profile NAME to choose)"
            );
            Some(name)
        }
        None => None,
    };
    match (&cli.command, profile.as_deref()) {
        (Command::Pandora(PandoraCommand::Auth { from_file }), Some(name)) => {
            let state = state_root.profile(name)?;
            let (token, source) = auth::read_token(from_file.as_deref(), input, err)?;
            let _ = writeln!(err, "Verifying the token with Pandora...");
            let report = auth::verify_and_store(name, &state, token, source, Client::from_env)?;
            if let Some(warning) = report.replaced_warning() {
                let _ = writeln!(err, "{warning}");
            }
            Ok(Output::PandoraAuth(report))
        }
        (Command::Pandora(PandoraCommand::Export), Some(name)) => {
            let paths = repo.profile(name)?;
            let Some(session) = state_root.profile(name)?.load_pandora_session()? else {
                fail!(
                    Kind::Auth,
                    "no Pandora session for profile {name}; run `sss pandora auth`"
                );
            };
            let progress: &mut dyn Write = if cli.json { err } else { out };
            let captured = export::export(&paths, &mut Client::from_env(&session), progress)?;
            if !captured.manifest.steps.failed().is_empty() {
                return Err(anyhow::Error::new(ExportIncomplete(captured)));
            }
            let normalized = model::normalize(&paths)?;
            Ok(Output::PandoraExport(Box::new(ExportReport {
                profile: name.to_owned(),
                run_id: captured.manifest.run_id.clone(),
                captured,
                normalized,
            })))
        }
        (Command::Pandora(PandoraCommand::Normalize), Some(name)) => Ok(Output::PandoraNormalize(
            model::normalize(&repo.profile(name)?)?,
        )),
        (Command::Status, profile) => Ok(Output::Status(status::status(
            &repo, state_root, &config, profile,
        )?)),
        _ => fail!(
            Kind::NotImplemented,
            "`sss {}` is not implemented yet",
            cli.command.name()
        ),
    }
}

/// The profile a command run without `--profile` acts on: the only known
/// one. None or several known is an error naming the command.
fn only_profile(command: &Command, known: &[String]) -> Result<String> {
    match known {
        [only] => Ok(only.clone()),
        [] => fail!(
            Kind::Config,
            "`sss {}` needs --profile NAME (no profiles yet)",
            command.name()
        ),
        several => fail!(
            Kind::Config,
            "`sss {}` needs --profile NAME (profiles: {})",
            command.name(),
            several.join(", ")
        ),
    }
}

/// What `sss init` reports: the repository and the files it created or
/// kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InitReport {
    pub repo: PathBuf,
    pub created: Vec<PathBuf>,
    pub kept: Vec<PathBuf>,
}

impl InitReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        for (path, _) in init_files(&Repo::at(&self.repo)) {
            if self.created.contains(&path) {
                writeln!(out, "created {}", path.display())?;
            } else if self.kept.contains(&path) {
                writeln!(out, "kept {}", path.display())?;
            }
        }
        writeln!(out, "data repository ready at {}", self.repo.display())
    }
}

/// The files `init` writes into `repo`, in the order it writes them, with
/// their initial contents.
fn init_files(repo: &Repo) -> [(PathBuf, &'static str); 2] {
    [
        (repo.config_path(), config::SKELETON),
        (repo.readme_path(), DATA_README),
    ]
}

/// Creates the data repository at `repo`: the directory, `config.toml` and
/// `README.md`, each only if absent. An existing `config.toml` is validated.
pub fn init(repo: &Repo) -> Result<InitReport> {
    let root = repo.root();
    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let mut report = InitReport {
        repo: root.to_path_buf(),
        created: Vec::new(),
        kept: Vec::new(),
    };
    for (path, contents) in init_files(repo) {
        if repo::write_new_atomic(&path, contents.as_bytes())? {
            report.created.push(path);
        } else {
            report.kept.push(path);
        }
    }
    Config::load(&repo.config_path())?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("sss").chain(args.iter().copied()))
            .unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    /// Every documented command line and its name.
    const DOCUMENTED: &[(&[&str], &str)] = &[
        (&["init"], "init"),
        (&["pandora", "auth"], "pandora auth"),
        (&["pandora", "auth", "--from-file", "f"], "pandora auth"),
        (&["pandora", "export"], "pandora export"),
        (&["pandora", "normalize"], "pandora normalize"),
        (&["spotify", "auth"], "spotify auth"),
        (&["match"], "match"),
        (&["match", "--no-llm", "--only", "artists"], "match"),
        (&["match", "review"], "match review"),
        (&["match", "set", "TR:1", "spotify:track:x"], "match set"),
        (&["match", "reject", "TR:1"], "match reject"),
        (&["push"], "push"),
        (&["push", "--dry-run", "--only", "liked"], "push"),
        (&["status"], "status"),
        (&["report"], "report"),
    ];

    #[test]
    fn every_documented_command_parses() {
        for (args, name) in DOCUMENTED {
            assert_eq!(parse(args).command.name(), *name, "{args:?}");
        }
    }

    #[test]
    fn flags_land_where_expected() {
        let cli = parse(&[
            "push",
            "--dry-run",
            "--only",
            "collection",
            "--profile",
            "alice",
        ]);
        assert_eq!(cli.profile.as_deref(), Some("alice"));
        let Command::Push(p) = cli.command else {
            panic!("not push")
        };
        assert!(p.dry_run);
        assert_eq!(p.only, Some(PushPart::Collection));

        let cli = parse(&["--repo", "/d", "match", "--no-llm", "--only", "albums"]);
        assert_eq!(cli.repo.as_deref(), Some(Path::new("/d")));
        let Command::Match(m) = cli.command else {
            panic!("not match")
        };
        assert!(m.no_llm && m.command.is_none());
        assert_eq!(m.only, Some(MatchKind::Albums));

        let Command::Match(MatchArgs {
            command:
                Some(MatchCommand::Set {
                    pandora_id,
                    spotify,
                }),
            ..
        }) = parse(&["match", "set", "TR:9", "https://open.spotify.com/track/x"]).command
        else {
            panic!("not match set")
        };
        assert_eq!(pandora_id, "TR:9");
        assert_eq!(spotify, "https://open.spotify.com/track/x");
    }

    #[test]
    fn malformed_command_lines_are_rejected() {
        for args in [
            &[][..],
            &["match", "--no-llm", "review"],
            &["push", "--only", "everything"],
            &["match", "set", "TR:1"],
            &["pandora"],
            &["pandora", "import", "b.json"],
        ] {
            assert!(
                Cli::try_parse_from(std::iter::once("sss").chain(args.iter().copied())).is_err(),
                "{args:?} accepted"
            );
        }
    }

    fn text(report: &InitReport) -> String {
        let mut out = Vec::new();
        report.write_text(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn init_creates_then_keeps() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::at(tmp.path().join("data"));
        let report = init(&repo).unwrap();
        assert_eq!(report.created, [repo.config_path(), repo.readme_path()]);
        assert_eq!(
            text(&report),
            format!(
                "created {}\ncreated {}\ndata repository ready at {}\n",
                repo.config_path().display(),
                repo.readme_path().display(),
                repo.root().display()
            )
        );
        assert_eq!(
            std::fs::read_to_string(repo.config_path()).unwrap(),
            config::SKELETON
        );
        assert_eq!(
            std::fs::read_to_string(repo.readme_path()).unwrap(),
            DATA_README
        );

        std::fs::write(repo.config_path(), "[push]\nliked_songs = true\n").unwrap();
        std::fs::remove_file(repo.readme_path()).unwrap();
        let report = init(&repo).unwrap();
        assert_eq!(report.kept, [repo.config_path()]);
        assert_eq!(report.created, [repo.readme_path()]);
        let text = text(&report);
        assert!(
            text.starts_with(&format!("kept {}\ncreated", repo.config_path().display())),
            "{text}"
        );
        assert_eq!(
            std::fs::read_to_string(repo.config_path()).unwrap(),
            "[push]\nliked_songs = true\n"
        );
    }

    #[test]
    fn init_rejects_an_invalid_existing_config() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::at(tmp.path());
        std::fs::write(repo.config_path(), "[matcher]\nbatch_size = 0\n").unwrap();
        let err = init(&repo).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Config);
        assert!(format!("{err:#}").contains("batch_size"), "{err:#}");
    }

    /// What one `run` produced: its result, stdout and stderr.
    struct Ran {
        result: Result<()>,
        out: String,
        err: String,
    }

    impl Ran {
        fn error(&self) -> String {
            format!("{:#}", self.result.as_ref().unwrap_err())
        }

        fn kind(&self) -> Kind {
            error::kind(self.result.as_ref().unwrap_err())
        }

        /// stdout parsed as the one JSON document it must be.
        fn document(&self) -> serde_json::Value {
            serde_json::from_str(&self.out).unwrap_or_else(|e| panic!("{e}: {}", self.out))
        }
    }

    fn exec(args: &[&str], state_root: &StateRoot, input: &str) -> Ran {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let result = run(
            parse(args),
            state_root,
            &mut input.as_bytes(),
            &mut out,
            &mut err,
        );
        Ran {
            result,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    /// An initialized repository at `<tmp>/data` and an empty state root at
    /// `<tmp>/state`.
    fn setup(tmp: &Path) -> (Repo, StateRoot) {
        let repo = Repo::at(tmp.join("data"));
        init(&repo).unwrap();
        (repo, StateRoot::at(tmp.join("state")))
    }

    fn with_repo<'a>(repo: &'a Repo, args: &[&'a str]) -> Vec<&'a str> {
        let mut full = vec!["--repo", repo.root().to_str().unwrap()];
        full.extend_from_slice(args);
        full
    }

    #[test]
    fn profile_scoped_commands_need_a_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = setup(tmp.path());
        let run_args = |args: &[&str]| exec(&with_repo(&repo, args), &state_root, "");
        let ran = run_args(&["push"]);
        assert_eq!(
            ran.error(),
            "`sss push` needs --profile NAME (no profiles yet)"
        );
        assert_eq!(ran.kind(), Kind::Config);
        assert_eq!(ran.out, "", "no JSON without --json");
        assert_eq!(
            run_args(&["pandora", "auth"]).error(),
            "`sss pandora auth` needs --profile NAME (no profiles yet)"
        );
        let ran = run_args(&["push", "--profile", "Bad/Name"]);
        assert!(ran.error().contains("invalid profile name"));
        assert_eq!(ran.kind(), Kind::Config);
        let ran = run_args(&["push", "--profile", "alice"]);
        assert_eq!(ran.error(), "`sss push` is not implemented yet");
        assert_eq!(ran.kind(), Kind::NotImplemented);
        assert_eq!(ran.err, "", "an explicit profile is not announced");

        // One profile, known only from a session in the state root: the
        // default, named on stderr. A profile directory with no session does
        // not count.
        state_root
            .profile("alice")
            .unwrap()
            .save_pandora_session(&crate::pandora::test_support::session())
            .unwrap();
        std::fs::create_dir_all(state_root.root().join("ccc/agents")).unwrap();
        let ran = run_args(&["push"]);
        assert_eq!(ran.error(), "`sss push` is not implemented yet");
        assert_eq!(
            ran.err,
            "profile alice (the only one; pass --profile NAME to choose)\n"
        );
        let err = run_args(&["pandora", "normalize"]).error();
        assert!(
            err.contains("no complete export run") && !err.contains("--profile"),
            "{err}"
        );

        // A second profile, known only from config.toml: both are named.
        std::fs::write(repo.config_path(), "[profiles.bob]\n").unwrap();
        assert_eq!(
            run_args(&["push"]).error(),
            "`sss push` needs --profile NAME (profiles: alice, bob)"
        );
        assert_eq!(
            run_args(&["push", "--profile", "bob"]).error(),
            "`sss push` is not implemented yet"
        );
    }

    #[test]
    fn a_profile_known_only_from_config_is_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = setup(tmp.path());
        std::fs::write(repo.config_path(), "[profiles.bob]\n").unwrap();
        let ran = exec(&with_repo(&repo, &["pandora", "export"]), &state_root, "");
        assert_eq!(
            ran.error(),
            "no Pandora session for profile bob; run `sss pandora auth`"
        );
        assert_eq!(ran.kind(), Kind::Auth);
    }

    #[test]
    fn commands_other_than_init_need_an_initialized_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let args = ["--repo", tmp.path().to_str().unwrap(), "status", "--json"];
        let ran = exec(&args, &StateRoot::at(tmp.path().join("state")), "");
        assert!(ran.error().contains("sss init"), "{}", ran.error());
        let doc = ran.document();
        assert_eq!(doc["ok"], false);
        assert_eq!(doc["command"], "status");
        assert_eq!(doc["kind"], "config");
        assert_eq!(doc["error"], ran.error());
        assert!(doc.get("details").is_none(), "{doc}");
    }

    #[test]
    fn json_is_accepted_before_and_after_the_subcommand() {
        assert!(parse(&["--json", "status"]).json);
        assert!(parse(&["pandora", "export", "--json"]).json);
        assert!(!parse(&["status"]).json);
    }

    /// Every documented command line, with `--json`, writes one JSON
    /// document whose `ok` agrees with the result and whose `command` is the
    /// command's name.
    #[test]
    fn every_command_renders_one_json_document() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = setup(tmp.path());
        std::fs::write(repo.config_path(), "[profiles.alice]\n").unwrap();
        let expected: &[(&str, Option<Kind>)] = &[
            ("init", None),
            ("pandora auth", Some(Kind::Input)),
            ("pandora auth --from-file f", Some(Kind::Other)),
            ("pandora export", Some(Kind::Auth)),
            ("pandora normalize", Some(Kind::Other)),
            ("status", None),
        ];
        for (args, name) in DOCUMENTED {
            let mut full = with_repo(&repo, args);
            full.push("--json");
            let ran = exec(&full, &state_root, "");
            let doc = ran.document();
            assert_eq!(doc["command"], *name, "{args:?}");
            assert_eq!(doc["ok"], ran.result.is_ok(), "{args:?}: {doc}");
            let joined = args.join(" ");
            let want = expected
                .iter()
                .find(|(line, _)| *line == joined)
                .map_or(Some(Kind::NotImplemented), |(_, kind)| *kind);
            match want {
                None => assert!(ran.result.is_ok(), "{args:?}: {}", ran.error()),
                Some(kind) => {
                    assert_eq!(ran.kind(), kind, "{args:?}: {}", ran.error());
                    assert_eq!(doc["kind"], serde_json::to_value(kind).unwrap());
                }
            }
        }
    }

    #[test]
    fn report_failure_writes_only_under_json() {
        let err = anyhow::anyhow!("disk full").context("writing x");
        let mut out = Vec::new();
        report_failure(&parse(&["status"]), &err, &mut out).unwrap();
        assert!(out.is_empty());
        report_failure(&parse(&["status", "--json"]), &err, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "{\n  \"ok\": false,\n  \"command\": \"status\",\n  \"kind\": \"other\",\n  \
             \"error\": \"writing x: disk full\"\n}\n"
        );
    }

    /// A path under `dir` whose last component is not UTF-8.
    #[cfg(unix)]
    fn non_utf8_path(dir: &Path) -> PathBuf {
        use std::os::unix::ffi::OsStringExt;
        dir.join(std::ffi::OsString::from_vec(b"r\xff".to_vec()))
    }

    #[cfg(unix)]
    #[test]
    fn a_result_that_does_not_serialize_is_one_error_document() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = non_utf8_path(tmp.path());
        let cli = Cli::try_parse_from([
            std::ffi::OsStr::new("sss"),
            "--json".as_ref(),
            "init".as_ref(),
            "--repo".as_ref(),
            repo.as_os_str(),
        ])
        .unwrap();
        let state_root = StateRoot::at(tmp.path().join("state"));
        let mut out = Vec::new();
        let result = run(cli, &state_root, &mut &b""[..], &mut out, &mut Vec::new());
        let err = result.unwrap_err();
        assert_eq!(error::kind(&err), Kind::Other);
        let doc: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(doc["ok"], false);
        assert_eq!(doc["command"], "init");
        assert_eq!(doc["error"], format!("{err:#}"));
        assert!(
            doc["error"]
                .as_str()
                .unwrap()
                .starts_with("serializing the result: "),
            "{doc}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn details_that_do_not_serialize_are_left_out_of_the_error_object() {
        let tmp = tempfile::tempdir().unwrap();
        let err = anyhow::Error::new(crate::pandora::export::ExportStopped {
            run_id: "20261013T000000Z".into(),
            run_dir: non_utf8_path(tmp.path()),
            source: anyhow::anyhow!("Pandora stopped answering"),
        });
        let mut out = Vec::new();
        report_failure(&parse(&["pandora", "export", "--json"]), &err, &mut out).unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            doc,
            serde_json::json!({
                "ok": false,
                "command": "pandora export",
                "kind": "other",
                "error": "Pandora stopped answering",
            })
        );
    }

    #[test]
    fn a_closed_stderr_does_not_end_the_command() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = setup(tmp.path());
        std::fs::write(repo.config_path(), "[profiles.alice]\n").unwrap();
        let args = with_repo(&repo, &["pandora", "normalize", "--json"]);
        let mut out = Vec::new();
        let err = run(
            parse(&args),
            &state_root,
            &mut &b""[..],
            &mut out,
            &mut crate::pandora::test_support::ClosedPipe,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("no complete export run"),
            "{err:#}"
        );
        let doc: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(doc["error"], format!("{err:#}"));
    }

    #[test]
    fn success_documents_lead_with_ok_and_command() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = setup(tmp.path());
        let ran = exec(&with_repo(&repo, &["init", "--json"]), &state_root, "");
        ran.result.as_ref().unwrap();
        assert!(
            ran.out
                .starts_with("{\n  \"ok\": true,\n  \"command\": \"init\",\n  \"repo\": "),
            "{}",
            ran.out
        );
        let doc = ran.document();
        assert_eq!(doc["created"], serde_json::json!([]));
        assert_eq!(doc["kept"].as_array().unwrap().len(), 2);

        let ran = exec(&with_repo(&repo, &["status", "--json"]), &state_root, "");
        ran.result.as_ref().unwrap();
        assert_eq!(
            ran.document(),
            serde_json::json!({"ok": true, "command": "status", "profiles": []})
        );
        let ran = exec(&with_repo(&repo, &["status"]), &state_root, "");
        assert_eq!(
            ran.out,
            "no profiles yet; run `sss pandora auth --profile NAME` first\n"
        );
    }

    #[test]
    fn auth_under_json_prompts_on_stderr_and_reads_by_the_paste_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = setup(tmp.path());
        let mut input: &[u8] = b"  \n";
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let args = with_repo(&repo, &["pandora", "auth", "--profile", "alice", "--json"]);
        let result = run(parse(&args), &state_root, &mut input, &mut out, &mut err);
        // An empty first line ends the paste: no token.
        let e = result.unwrap_err();
        assert_eq!(error::kind(&e), Kind::Input);
        assert_eq!(String::from_utf8(err).unwrap(), auth::PROMPT);
        let doc: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(doc["error"], "no token given");

        // A line after the empty one is not consumed.
        let mut input: &[u8] = b"\nlater\n";
        run(
            parse(&args),
            &state_root,
            &mut input,
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(input, b"later\n");
    }
}
