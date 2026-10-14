//! Command line: the `sss` subcommands and their dispatch.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::{self, Config};
use crate::pandora::client::Client;
use crate::pandora::{auth, export, model};
use crate::profiles;
use crate::repo::{self, Repo};
use crate::secrets::StateRoot;
use crate::status;

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
    /// Authorize the profile's Spotify account (PKCE, loopback redirect)
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

/// Runs the parsed command line against the profiles' state directories
/// under `state_root`, writing normal output to `out`.
pub fn run(cli: Cli, state_root: &StateRoot, out: &mut dyn Write) -> Result<()> {
    let repo = Repo::locate(cli.repo.as_deref())?;
    if let Command::Init = cli.command {
        return init(&repo, out);
    }
    let config = Config::load(&repo.config_path())?;
    let profile = match &cli.profile {
        Some(name) => {
            repo::validate_profile_name(name)?;
            Some(name.clone())
        }
        None if cli.command.needs_profile() => Some(only_profile(
            &cli.command,
            &profiles::known(&repo, state_root, &config)?,
        )?),
        None => None,
    };
    match (&cli.command, profile.as_deref()) {
        (Command::Pandora(PandoraCommand::Auth { from_file }), Some(name)) => {
            let state = state_root.profile(name)?;
            let mut stderr = std::io::stderr();
            let (token, source) = auth::read_token(
                from_file.as_deref(),
                &mut std::io::stdin().lock(),
                &mut stderr,
            )?;
            writeln!(stderr, "Verifying the token with Pandora...").context("writing to stderr")?;
            auth::verify_and_store(&state, token, source, Client::from_env, out)?;
            Ok(())
        }
        (Command::Pandora(PandoraCommand::Export), Some(name)) => {
            let paths = repo.profile(name)?;
            let session = state_root
                .profile(name)?
                .load_pandora_session()?
                .with_context(|| {
                    format!("no Pandora session for profile {name}; run `sss pandora auth`")
                })?;
            let manifest = export::export(&paths, &mut Client::from_env(&session), out)?;
            let failed = manifest.steps.failed();
            if !failed.is_empty() {
                let names: Vec<_> = failed.iter().map(|s| s.name()).collect();
                bail!(
                    "run {} is incomplete: failed steps {}; its raw responses are kept",
                    manifest.run_id,
                    names.join(", ")
                );
            }
            writeln!(out, "run {} complete", manifest.run_id).context("writing output")?;
            model::normalize(&paths, out)?;
            Ok(())
        }
        (Command::Pandora(PandoraCommand::Normalize), Some(name)) => {
            model::normalize(&repo.profile(name)?, out)?;
            Ok(())
        }
        (Command::Status, profile) => status::status(&repo, state_root, &config, profile, out),
        _ => bail!("`sss {}` is not implemented yet", cli.command.name()),
    }
}

/// The profile a command run without `--profile` acts on: the only known
/// one. None or several known is an error naming the command.
fn only_profile(command: &Command, known: &[String]) -> Result<String> {
    match known {
        [only] => Ok(only.clone()),
        [] => bail!(
            "`sss {}` needs --profile NAME (no profiles yet)",
            command.name()
        ),
        several => bail!(
            "`sss {}` needs --profile NAME (profiles: {})",
            command.name(),
            several.join(", ")
        ),
    }
}

/// Creates the data repository at `repo`: the directory, `config.toml` and
/// `README.md`, each only if absent. An existing `config.toml` is validated.
pub fn init(repo: &Repo, out: &mut dyn Write) -> Result<()> {
    let root = repo.root();
    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    for (path, contents) in [
        (repo.config_path(), config::SKELETON),
        (repo.readme_path(), DATA_README),
    ] {
        let verb = if repo::write_new_atomic(&path, contents.as_bytes())? {
            "created"
        } else {
            "kept"
        };
        report_line(out, verb, &path)?;
    }
    Config::load(&repo.config_path())?;
    writeln!(out, "data repository ready at {}", root.display()).context("writing output")?;
    Ok(())
}

fn report_line(out: &mut dyn Write, verb: &str, path: &Path) -> Result<()> {
    writeln!(out, "{verb} {}", path.display()).context("writing output")
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

    #[test]
    fn every_documented_command_parses() {
        let cases: &[(&[&str], &str)] = &[
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
        for (args, name) in cases {
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

    #[test]
    fn init_creates_then_keeps() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::at(tmp.path().join("data"));
        let mut out = Vec::new();
        init(&repo, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches("created").count(), 2, "{text}");
        assert_eq!(
            std::fs::read_to_string(repo.config_path()).unwrap(),
            config::SKELETON
        );
        assert_eq!(
            std::fs::read_to_string(repo.readme_path()).unwrap(),
            DATA_README
        );

        std::fs::write(repo.config_path(), "[push]\nliked_songs = true\n").unwrap();
        let mut out = Vec::new();
        init(&repo, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches("kept").count(), 2, "{text}");
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
        let err = format!("{:#}", init(&repo, &mut Vec::new()).unwrap_err());
        assert!(err.contains("batch_size"), "{err}");
    }

    #[test]
    fn profile_scoped_commands_need_a_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let repo = Repo::at(&data);
        init(&repo, &mut Vec::new()).unwrap();
        let state_root = StateRoot::at(tmp.path().join("state"));
        let run_args = |args: &[&str]| {
            let mut full = vec!["--repo", data.to_str().unwrap()];
            full.extend_from_slice(args);
            format!(
                "{:#}",
                run(parse(&full), &state_root, &mut Vec::new()).unwrap_err()
            )
        };
        assert_eq!(
            run_args(&["push"]),
            "`sss push` needs --profile NAME (no profiles yet)"
        );
        assert_eq!(
            run_args(&["pandora", "auth"]),
            "`sss pandora auth` needs --profile NAME (no profiles yet)"
        );
        assert!(run_args(&["push", "--profile", "Bad/Name"]).contains("invalid profile name"));
        assert_eq!(
            run_args(&["push", "--profile", "alice"]),
            "`sss push` is not implemented yet"
        );

        // One profile, known only from a session in the state root: the
        // default. A profile directory with no session does not count.
        state_root
            .profile("alice")
            .unwrap()
            .save_pandora_session(&crate::pandora::test_support::session())
            .unwrap();
        std::fs::create_dir_all(state_root.root().join("ccc/agents")).unwrap();
        assert_eq!(run_args(&["push"]), "`sss push` is not implemented yet");
        let err = run_args(&["pandora", "normalize"]);
        assert!(
            err.contains("no complete export run") && !err.contains("--profile"),
            "{err}"
        );

        // A second profile, known only from config.toml: both are named.
        std::fs::write(repo.config_path(), "[profiles.bob]\n").unwrap();
        assert_eq!(
            run_args(&["push"]),
            "`sss push` needs --profile NAME (profiles: alice, bob)"
        );
        assert_eq!(
            run_args(&["push", "--profile", "bob"]),
            "`sss push` is not implemented yet"
        );
    }

    #[test]
    fn a_profile_known_only_from_config_is_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::at(tmp.path().join("data"));
        init(&repo, &mut Vec::new()).unwrap();
        std::fs::write(repo.config_path(), "[profiles.bob]\n").unwrap();
        let cli = parse(&["--repo", repo.root().to_str().unwrap(), "pandora", "export"]);
        let state_root = StateRoot::at(tmp.path().join("state"));
        let err = format!("{:#}", run(cli, &state_root, &mut Vec::new()).unwrap_err());
        assert_eq!(
            err,
            "no Pandora session for profile bob; run `sss pandora auth`"
        );
    }

    #[test]
    fn commands_other_than_init_need_an_initialized_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let cli = parse(&["--repo", tmp.path().to_str().unwrap(), "status"]);
        let state_root = StateRoot::at(tmp.path().join("state"));
        let err = format!("{:#}", run(cli, &state_root, &mut Vec::new()).unwrap_err());
        assert!(err.contains("sss init"), "{err}");
    }
}
