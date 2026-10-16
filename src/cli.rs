//! Command line: the `sss` subcommands and their dispatch.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::config::{self, Config};
use crate::error::{self, ExportRun, Kind};
use crate::fail;
use crate::guide::{self, GuideReport};
use crate::input;
use crate::matching::llm;
use crate::matching::run::{
    self as matching, AnswerReport, DecisionsRefused, MatchReport, MatchSetReport, QuestionsReport,
    ReviewReport,
};
use crate::pandora::auth::{self, AuthReport};
use crate::pandora::client::Client;
use crate::pandora::export::{self, ExportIncomplete, ExportReport, Manifest};
use crate::pandora::model::{self, Summary};
use crate::player::{self, Control, DevicesReport, PlayerReport, PlayerStateReport, QueueReport};
use crate::profiles;
use crate::push::{self, PushReport};
use crate::repo::{self, Repo};
use crate::report::{self, ReportFile};
use crate::search::{self, SearchReport};
use crate::secrets::{StateDir, StateRoot};
use crate::spotify;
use crate::spotify::ids::IdKind;
use crate::spotify::playback::{Device, PlayerRefused, Repeat};
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
    /// Print the guide for an agent operating `sss`, or the matching protocol
    Guide {
        /// Which document [default: agent]
        #[arg(value_enum)]
        topic: Option<guide::Topic>,
    },
    /// List the Spotify players open on the profile's account
    Devices,
    /// Show what the profile's Spotify player is playing and what is queued
    Player,
    /// Play tracks, a playlist, album or artist, a station's playlist or a
    /// track's stored candidates; with nothing named, resume
    Play(PlayArgs),
    /// Pause playback
    Pause(DeviceArg),
    /// Skip to the next track
    Next(DeviceArg),
    /// Go back to the previous track
    Previous(DeviceArg),
    /// Add tracks to the queue, in order; a queued track cannot be removed
    Queue(QueueArgs),
    /// Turn shuffle on or off
    Shuffle {
        #[arg(value_enum)]
        state: OnOff,
        #[command(flatten)]
        device: DeviceArg,
    },
    /// Set the repeat mode
    Repeat {
        #[arg(value_enum)]
        mode: RepeatMode,
        #[command(flatten)]
        device: DeviceArg,
    },
    /// Set the volume, 0 to 100
    Volume {
        #[arg(value_parser = clap::value_parser!(u8).range(0..=100))]
        percent: u8,
        #[command(flatten)]
        device: DeviceArg,
    },
    /// Seek to a position in the playing track
    Seek {
        /// Whole seconds from the start of the track
        seconds: u64,
        #[command(flatten)]
        device: DeviceArg,
    },
    /// Search Spotify for tracks, artists and albums; several queries at once
    Search(SearchArgs),
}

/// The Spotify player a command that changes playback goes to.
#[derive(Debug, Args)]
pub struct DeviceArg {
    /// The player, by name or id (see `sss devices`) [default: the active
    /// one, else the only one open]
    #[arg(long, value_name = "NAME|ID")]
    pub device: Option<String>,
}

#[derive(Debug, Args)]
pub struct PlayArgs {
    /// Spotify tracks (URI, link or bare id), or one playlist, album or
    /// artist
    #[arg(value_name = "TARGET")]
    pub targets: Vec<String>,

    /// Play the Spotify playlist `sss push` made for this station
    #[arg(long, value_name = "NAME")]
    pub station: Option<String>,

    /// Play the stored candidates of this Pandora track (TR:…), in order
    #[arg(long, value_name = "TR:…")]
    pub item: Option<String>,

    /// Turn shuffle on first; a playlist or album starts at a random track
    #[arg(long, conflicts_with = "no_shuffle")]
    pub shuffle: bool,

    /// Turn shuffle off first
    #[arg(long)]
    pub no_shuffle: bool,

    #[command(flatten)]
    pub device: DeviceArg,
}

impl PlayArgs {
    /// The shuffle state to set first; `None` leaves it as it is.
    fn shuffle(&self) -> Option<bool> {
        match (self.shuffle, self.no_shuffle) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        }
    }
}

#[derive(Debug, Args)]
pub struct QueueArgs {
    /// Spotify tracks (URI, link or bare id), in the order to queue them
    #[arg(value_name = "TARGET", required = true)]
    pub targets: Vec<String>,

    #[command(flatten)]
    pub device: DeviceArg,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// Free-text queries, one request each
    #[arg(value_name = "QUERY", required = true)]
    pub queries: Vec<String>,

    /// Search only this kind of entity
    #[arg(long, value_enum)]
    pub only: Option<MatchKind>,

    /// Results of each kind per query, 1 to 10
    #[arg(long, value_name = "N", default_value_t = spotify::client::SEARCH_LIMIT,
          value_parser = clap::value_parser!(u32).range(1..=i64::from(spotify::client::SEARCH_LIMIT)))]
    pub limit: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RepeatMode {
    Off,
    Track,
    Context,
}

impl From<RepeatMode> for Repeat {
    fn from(mode: RepeatMode) -> Self {
        match mode {
            RepeatMode::Off => Repeat::Off,
            RepeatMode::Track => Repeat::Track,
            RepeatMode::Context => Repeat::Context,
        }
    }
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
    /// Authorize the profile's Spotify account: print the address to open,
    /// then, with --callback, take back the address the browser landed on
    Auth {
        /// Finish the sign-in: read the redirected address (pasted, up to an
        /// empty line) and store the token
        #[arg(long)]
        callback: bool,
        /// With --callback, read the address from a file instead
        #[arg(long, value_name = "FILE", requires = "callback")]
        from_file: Option<PathBuf>,
    },
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

    /// Decide at most N items this run, then stop
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    #[command(subcommand)]
    pub command: Option<MatchCommand>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MatchKind {
    Tracks,
    Artists,
    Albums,
}

impl From<MatchKind> for IdKind {
    fn from(kind: MatchKind) -> Self {
        match kind {
            MatchKind::Tracks => IdKind::Track,
            MatchKind::Artists => IdKind::Artist,
            MatchKind::Albums => IdKind::Album,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum MatchCommand {
    /// List unmatched, ambiguous, failed and low-confidence items with their
    /// top candidates
    Review {
        /// Review only this kind of entity
        #[arg(long, value_enum)]
        only: Option<MatchKind>,
    },
    /// Print the items awaiting an LLM decision, with every candidate, as one
    /// JSON document; contacts nothing
    Questions {
        /// List only this kind of entity
        #[arg(long, value_enum)]
        only: Option<MatchKind>,
        /// List at most N items
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
    /// Record decisions for open items from a `{"decisions":[…]}` document
    /// (pasted up to an empty line, or --from-file)
    Answer {
        /// Read the decisions document from a file instead
        #[arg(long, value_name = "FILE")]
        from_file: Option<PathBuf>,
        /// Record at most the first N decisions
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
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
    Artists,
    Stations,
    Playlists,
    Collection,
    Liked,
    ThumbsDown,
}

impl From<PushPart> for push::Part {
    fn from(part: PushPart) -> Self {
        match part {
            PushPart::Artists => push::Part::Artists,
            PushPart::Stations => push::Part::Stations,
            PushPart::Playlists => push::Part::Playlists,
            PushPart::Collection => push::Part::Collection,
            PushPart::Liked => push::Part::Liked,
            PushPart::ThumbsDown => push::Part::ThumbsDown,
        }
    }
}

/// What `sss match answer` writes to stderr before reading stdin.
pub const ANSWER_PROMPT: &str = "Paste the decisions JSON, then an empty line:\n";

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
            Command::Spotify(SpotifyCommand::Auth { .. }) => "spotify auth",
            Command::Match(MatchArgs { command: None, .. }) => "match",
            Command::Match(MatchArgs {
                command: Some(MatchCommand::Review { .. }),
                ..
            }) => "match review",
            Command::Match(MatchArgs {
                command: Some(MatchCommand::Questions { .. }),
                ..
            }) => "match questions",
            Command::Match(MatchArgs {
                command: Some(MatchCommand::Answer { .. }),
                ..
            }) => "match answer",
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
            Command::Guide { .. } => "guide",
            Command::Devices => "devices",
            Command::Player => "player",
            Command::Play(_) => "play",
            Command::Pause(_) => "pause",
            Command::Next(_) => "next",
            Command::Previous(_) => "previous",
            Command::Queue(_) => "queue",
            Command::Shuffle { .. } => "shuffle",
            Command::Repeat { .. } => "repeat",
            Command::Volume { .. } => "volume",
            Command::Seek { .. } => "seek",
            Command::Search(_) => "search",
        }
    }

    /// The player control of a command that is one, with its `--device`.
    fn control(&self) -> Option<(Control, Option<&str>)> {
        let (control, device) = match self {
            Command::Pause(d) => (Control::Pause, d),
            Command::Next(d) => (Control::Next, d),
            Command::Previous(d) => (Control::Previous, d),
            Command::Shuffle { state, device } => (Control::Shuffle(*state == OnOff::On), device),
            Command::Repeat { mode, device } => (Control::Repeat(Repeat::from(*mode)), device),
            Command::Volume { percent, device } => (Control::Volume(*percent), device),
            Command::Seek { seconds, device } => (Control::Seek(*seconds), device),
            _ => return None,
        };
        Some((control, device.device.as_deref()))
    }

    /// Whether the command acts on one profile and so needs `--profile` or a
    /// single known profile. `status` covers every profile when none is
    /// named; `init` and `guide` act on none.
    fn needs_profile(&self) -> bool {
        !matches!(
            self,
            Command::Init | Command::Status | Command::Guide { .. }
        )
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
    SpotifyAuthStart(spotify::auth::AuthStartReport),
    SpotifyAuth(spotify::auth::AuthReport),
    Match(MatchReport),
    MatchReview(ReviewReport),
    MatchQuestions(QuestionsReport),
    MatchAnswer(AnswerReport),
    MatchSet(MatchSetReport),
    Push(PushReport),
    Report(ReportFile),
    Status(StatusReport),
    Guide(GuideReport),
    Devices(DevicesReport),
    PlayerState(PlayerStateReport),
    Player(PlayerReport),
    Queue(QueueReport),
    Search(SearchReport),
}

impl Output {
    fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        match self {
            Output::Init(r) => r.write_text(out),
            Output::PandoraAuth(r) => r.write_text(out),
            Output::PandoraExport(r) => r.write_text(out),
            Output::PandoraNormalize(r) => r.write_text(out),
            Output::SpotifyAuthStart(r) => r.write_text(out),
            Output::SpotifyAuth(r) => r.write_text(out),
            Output::Match(r) => r.write_text(out),
            Output::MatchReview(r) => r.write_text(out),
            Output::MatchQuestions(r) => r.write_text(out),
            Output::MatchAnswer(r) => r.write_text(out),
            Output::MatchSet(r) => r.write_text(out),
            Output::Push(r) => r.write_text(out),
            Output::Report(r) => r.write_text(out),
            Output::Status(r) => r.write_text(out),
            Output::Guide(r) => r.write_text(out),
            Output::Devices(r) => r.write_text(out),
            Output::PlayerState(r) => r.write_text(out),
            Output::Player(r) => r.write_text(out),
            Output::Queue(r) => r.write_text(out),
            Output::Search(r) => r.write_text(out),
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

/// The `details` of a failed `pandora export`, a refused `match answer` or
/// a player refusal.
#[derive(Serialize)]
#[serde(untagged)]
enum Details<'a> {
    Refused {
        problems: &'a [llm::Problem],
    },
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
    Player {
        reason: &'a str,
        devices: &'a [Device],
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
fn report_failure(cli: &Cli, err: &anyhow::Error, out: &mut dyn Write) -> Result<()> {
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
        None => err.chain().find_map(|e| {
            if let Some(r) = e.downcast_ref::<DecisionsRefused>() {
                Some(Details::Refused {
                    problems: &r.problems,
                })
            } else {
                e.downcast_ref::<PlayerRefused>().map(|p| Details::Player {
                    reason: &p.reason,
                    devices: &p.devices,
                })
            }
        }),
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
/// under the state root `locate_state` returns, called only by a command
/// that reads a repository (every one but `guide`). Pasted values are read
/// from `input`; the result goes to `out`, as text or, with `--json`, as one
/// JSON document; prompts, progress and warnings go to `err`, and a failed
/// write of them is ignored.
/// A failure, including a result that does not serialize, is returned
/// unchanged after its JSON error object is written to `out` under `--json`.
pub fn run(
    cli: Cli,
    locate_state: impl FnOnce() -> Result<StateRoot>,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<()> {
    match dispatch(&cli, locate_state, input, out, err).and_then(|output| render(&cli, &output)) {
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
    locate_state: impl FnOnce() -> Result<StateRoot>,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<Output> {
    if let Command::Guide { topic } = cli.command {
        return Ok(Output::Guide(GuideReport::new(topic.unwrap_or_default())));
    }
    let state_root = &locate_state()?;
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
        (
            Command::Spotify(SpotifyCommand::Auth {
                callback: false, ..
            }),
            Some(name),
        ) => {
            let report = spotify::auth::start(
                name,
                config.client_id(name),
                &repo.config_path(),
                config.push.public_playlists,
                &state_root.profile(name)?,
                &spotify::auth::auth_base_from_env(),
            )?;
            let _ = writeln!(err, "{}", report.instructions());
            Ok(Output::SpotifyAuthStart(report))
        }
        (
            Command::Spotify(SpotifyCommand::Auth {
                callback: true,
                from_file,
            }),
            Some(name),
        ) => Ok(Output::SpotifyAuth(spotify::auth::complete(
            name,
            &state_root.profile(name)?,
            from_file.as_deref(),
            input,
            err,
            &spotify::auth::auth_base_from_env(),
        )?)),
        (
            Command::Match(MatchArgs {
                command: None,
                no_llm,
                only,
                limit,
            }),
            Some(name),
        ) => {
            let paths = repo.profile(name)?;
            let items = matching::select(&model::read(&paths)?, &config.push);
            let state = state_root.profile(name)?;
            let connect = || spotify_client(name, &config, &repo, state);
            let options = matching::Options {
                only: only.map(IdKind::from),
                limit: *limit,
                batch_size: usize::try_from(config.matcher.batch_size).unwrap_or(usize::MAX),
            };
            let connect_matcher: Option<matching::ConnectMatcher<'_>> = if *no_llm {
                None
            } else {
                let paths = &paths;
                let matcher = &config.matcher;
                Some(Box::new(move || {
                    let ccc =
                        llm::Ccc::prepare(llm::ccc_program(), state_root.root(), matcher, paths)?;
                    Ok(Box::new(ccc) as Box<dyn llm::Matcher>)
                }))
            };
            Ok(Output::Match(matching::run(
                name,
                &paths,
                &items,
                options,
                connect,
                connect_matcher,
                err,
            )?))
        }
        (
            Command::Match(MatchArgs {
                command: Some(MatchCommand::Review { only }),
                ..
            }),
            Some(name),
        ) => {
            let paths = repo.profile(name)?;
            let items = matching::select(&model::read(&paths)?, &config.push);
            Ok(Output::MatchReview(matching::review(
                &crate::matching::model::read_all_latest(&paths)?,
                &items,
                only.map(IdKind::from),
            )))
        }
        (
            Command::Match(MatchArgs {
                command: Some(MatchCommand::Questions { only, limit }),
                ..
            }),
            Some(name),
        ) => {
            let paths = repo.profile(name)?;
            let items = matching::select(&model::read(&paths)?, &config.push);
            Ok(Output::MatchQuestions(matching::questions(
                &crate::matching::model::read_all_latest(&paths)?,
                &items,
                only.map(IdKind::from),
                *limit,
            )))
        }
        (
            Command::Match(MatchArgs {
                command: Some(MatchCommand::Answer { from_file, limit }),
                ..
            }),
            Some(name),
        ) => {
            let text =
                input::read_value(from_file.as_deref(), input, err, ANSWER_PROMPT, "decisions")?;
            let paths = repo.profile(name)?;
            let items = matching::select(&model::read(&paths)?, &config.push);
            Ok(Output::MatchAnswer(matching::answer(
                &paths, &items, &text, *limit,
            )?))
        }
        (
            Command::Match(MatchArgs {
                command:
                    Some(MatchCommand::Set {
                        pandora_id,
                        spotify,
                    }),
                ..
            }),
            Some(name),
        ) => Ok(Output::MatchSet(matching::set(
            &repo.profile(name)?,
            pandora_id,
            spotify,
        )?)),
        (
            Command::Match(MatchArgs {
                command: Some(MatchCommand::Reject { pandora_id }),
                ..
            }),
            Some(name),
        ) => Ok(Output::MatchSet(matching::reject(
            &repo.profile(name)?,
            pandora_id,
        )?)),
        (Command::Push(PushArgs { dry_run, only }), Some(name)) => {
            let paths = repo.profile(name)?;
            let export = model::read(&paths)?;
            let state = state_root.profile(name)?;
            let (config, repo) = (&config, &repo);
            let connect = move || spotify_client(name, config, repo, state);
            let options = push::Options {
                only: only.map(push::Part::from),
                dry_run: *dry_run,
            };
            let report = push::push(
                name,
                &paths,
                &export,
                &config.push,
                options,
                Box::new(connect),
                &mut *err,
            )?;
            if cli.json {
                // The text carries these; under --json they go to the person
                // on stderr as well as into the document's `pending` entries.
                for instruction in report.pending_instructions() {
                    let _ = writeln!(err, "{instruction}");
                }
            }
            Ok(Output::Push(report))
        }
        (Command::Report, Some(name)) => Ok(Output::Report(report::report(
            name,
            &repo.profile(name)?,
            &config.push,
        )?)),
        (Command::Status, profile) => Ok(Output::Status(status::status(
            &repo, state_root, &config, profile,
        )?)),
        (
            command @ (Command::Devices
            | Command::Player
            | Command::Play(_)
            | Command::Pause(_)
            | Command::Next(_)
            | Command::Previous(_)
            | Command::Queue(_)
            | Command::Shuffle { .. }
            | Command::Repeat { .. }
            | Command::Volume { .. }
            | Command::Seek { .. }),
            Some(name),
        ) => player_command(command, name, &config, &repo, state_root),
        (Command::Search(args), Some(name)) => Ok(Output::Search(search::search(
            &args.queries,
            args.only.map(IdKind::from),
            args.limit,
            || spotify_client(name, &config, &repo, state_root.profile(name)?),
        )?)),
        // `guide` and `init` returned above, and every other command has a
        // profile by now: `--profile`, or the only one.
        (Command::Guide { .. } | Command::Init, _) | (_, None) => fail!(
            Kind::Config,
            "`sss {}` needs --profile NAME",
            cli.command.name()
        ),
    }
}

/// Runs a playback command for profile `name`: its arguments are resolved
/// before the profile's Spotify client is made.
fn player_command(
    command: &Command,
    name: &str,
    config: &Config,
    repo: &Repo,
    state_root: &StateRoot,
) -> Result<Output> {
    let connect = || spotify_client(name, config, repo, state_root.profile(name)?);
    if let Some((control, device)) = command.control() {
        return Ok(Output::Player(player::control(
            name, control, device, connect,
        )?));
    }
    match command {
        Command::Devices => Ok(Output::Devices(player::devices(name, connect)?)),
        Command::Player => Ok(Output::PlayerState(player::player(name, connect)?)),
        Command::Play(args) => {
            let source = player::resolve_source(
                &repo.profile(name)?,
                &args.targets,
                args.station.as_deref(),
                args.item.as_deref(),
            )?;
            Ok(Output::Player(player::play(
                name,
                &source,
                args.shuffle(),
                args.device.device.as_deref(),
                connect,
            )?))
        }
        Command::Queue(args) => Ok(Output::Queue(player::queue(
            name,
            &args.targets,
            args.device.device.as_deref(),
            connect,
        )?)),
        _ => unreachable!("`{}` is not a playback command", command.name()),
    }
}

/// The Spotify client of profile `name`: its client id from the
/// configuration (kind `config` when there is none), the configured market
/// and the environment's base URLs.
fn spotify_client(
    name: &str,
    config: &Config,
    repo: &Repo,
    state: StateDir,
) -> Result<spotify::client::Spotify> {
    let client_id =
        spotify::auth::require_client_id(name, config.client_id(name), &repo.config_path())?;
    spotify::client::Spotify::from_env(state, client_id, &config.spotify.market)
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
        (&["spotify", "auth", "--callback"], "spotify auth"),
        (
            &["spotify", "auth", "--callback", "--from-file", "f"],
            "spotify auth",
        ),
        (&["match"], "match"),
        (&["match", "--no-llm", "--only", "artists"], "match"),
        (&["match", "--no-llm", "--limit", "5"], "match"),
        (&["match", "review"], "match review"),
        (&["match", "review", "--only", "tracks"], "match review"),
        (&["match", "questions"], "match questions"),
        (
            &["match", "questions", "--only", "tracks", "--limit", "5"],
            "match questions",
        ),
        (&["match", "answer"], "match answer"),
        (
            &["match", "answer", "--from-file", "f", "--limit", "1"],
            "match answer",
        ),
        (&["match", "set", "TR:1", "spotify:track:x"], "match set"),
        (&["match", "reject", "TR:1"], "match reject"),
        (&["push"], "push"),
        (&["push", "--dry-run", "--only", "liked"], "push"),
        (&["push", "--only", "artists"], "push"),
        (&["push", "--only", "thumbs-down"], "push"),
        (&["status"], "status"),
        (&["report"], "report"),
        (&["guide"], "guide"),
        (&["guide", "matching"], "guide"),
        (&["devices"], "devices"),
        (&["player"], "player"),
        (&["play"], "play"),
        (&["play", TRACK], "play"),
        (&["play", "--station", "x"], "play"),
        (&["play", "--item", "TR:1"], "play"),
        (&["pause"], "pause"),
        (&["next"], "next"),
        (&["previous"], "previous"),
        (&["queue", TRACK], "queue"),
        (&["shuffle", "on"], "shuffle"),
        (&["repeat", "off"], "repeat"),
        (&["volume", "50"], "volume"),
        (&["seek", "30"], "seek"),
        (&["search", "x"], "search"),
        (
            &["search", "x", "y", "--only", "tracks", "--limit", "5"],
            "search",
        ),
    ];

    /// The documentation's public example track.
    const TRACK: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";

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
        assert_eq!(m.limit, None);

        let Command::Match(m) = parse(&["match", "--limit", "25", "--no-llm"]).command else {
            panic!("not match")
        };
        assert_eq!(m.limit, Some(25));
        let Command::Match(MatchArgs {
            command: Some(MatchCommand::Review { only }),
            ..
        }) = parse(&["match", "review", "--only", "artists"]).command
        else {
            panic!("not match review")
        };
        assert_eq!(only, Some(MatchKind::Artists));

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

        let Command::Search(s) = parse(&["search", "a b", "c"]).command else {
            panic!("not search")
        };
        assert_eq!(s.queries, ["a b", "c"]);
        assert_eq!((s.only, s.limit), (None, 10));
        let Command::Search(s) =
            parse(&["search", "x", "--only", "artists", "--limit", "1"]).command
        else {
            panic!("not search")
        };
        assert_eq!((s.only, s.limit), (Some(MatchKind::Artists), 1));
    }

    #[test]
    fn malformed_command_lines_are_rejected() {
        for args in [
            &[][..],
            &["match", "--no-llm", "review"],
            &["match", "--limit", "x"],
            &["match", "--limit", "5", "review"],
            &["match", "--only", "tracks", "questions"],
            &["match", "--limit", "5", "answer"],
            &["match", "answer", "--only", "tracks"],
            &["push", "--only", "everything"],
            &["match", "set", "TR:1"],
            &["spotify", "auth", "--from-file", "f"],
            &["pandora"],
            &["pandora", "import", "b.json"],
            &["guide", "everything"],
            &["volume", "101"],
            &["volume", "-1"],
            &["seek", "-5"],
            &["shuffle", "maybe"],
            &["repeat", "all"],
            &["queue"],
            &["play", "--shuffle", "--no-shuffle"],
            &["devices", "--device", "Firefox"],
            &["player", "--device", "Firefox"],
            &["search"],
            &["search", "x", "--limit", "0"],
            &["search", "x", "--limit", "11"],
            &["search", "x", "--only", "songs"],
            &["search", "x", "--device", "Firefox"],
        ] {
            assert!(
                Cli::try_parse_from(std::iter::once("sss").chain(args.iter().copied())).is_err(),
                "{args:?} accepted"
            );
        }
    }

    #[test]
    fn every_command_that_changes_playback_takes_a_device() {
        let lines: &[&[&str]] = &[
            &["play"],
            &["play", TRACK, "--shuffle"],
            &["pause"],
            &["next"],
            &["previous"],
            &["queue", TRACK],
            &["shuffle", "off"],
            &["repeat", "context"],
            &["volume", "0"],
            &["seek", "0"],
        ];
        for args in lines {
            let mut full = args.to_vec();
            full.extend(["--device", "Kitchen"]);
            let command = parse(&full).command;
            let device = match &command {
                Command::Play(p) => p.device.device.as_deref(),
                Command::Queue(q) => q.device.device.as_deref(),
                other => other.control().unwrap().1,
            };
            assert_eq!(device, Some("Kitchen"), "{full:?}");
        }
        let Command::Play(p) = parse(&["play", "--no-shuffle", "a", "b"]).command else {
            panic!("not play")
        };
        assert_eq!((p.shuffle(), p.targets.len()), (Some(false), 2));
        let Command::Play(p) = parse(&["play", "--station", "Jazz", "--shuffle"]).command else {
            panic!("not play")
        };
        assert_eq!(
            (p.shuffle(), p.station.as_deref()),
            (Some(true), Some("Jazz"))
        );
        let Command::Play(p) = parse(&["play"]).command else {
            panic!("not play")
        };
        assert_eq!(p.shuffle(), None);
        assert_eq!(
            parse(&["volume", "100"]).command.control().unwrap().0,
            Control::Volume(100)
        );
        assert_eq!(
            parse(&["seek", "90"]).command.control().unwrap().0,
            Control::Seek(90)
        );
        assert_eq!(
            parse(&["repeat", "track"]).command.control().unwrap().0,
            Control::Repeat(Repeat::Track)
        );
        assert_eq!(
            parse(&["shuffle", "on"]).command.control().unwrap().0,
            Control::Shuffle(true)
        );
        assert!(parse(&["devices"]).command.control().is_none());
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
            || Ok(state_root.clone()),
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
        assert!(
            ran.error().contains("run `sss pandora export`"),
            "{}",
            ran.error()
        );
        assert_eq!(ran.kind(), Kind::Other);
        let ran = run_args(&["report", "--profile", "alice"]);
        assert!(
            ran.error().contains("run `sss pandora export`"),
            "{}",
            ran.error()
        );
        assert_eq!(ran.kind(), Kind::Other);
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
        let ran = run_args(&["report"]);
        assert!(
            ran.error().contains("profiles/alice/pandora"),
            "{}",
            ran.error()
        );
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
        assert!(
            run_args(&["report", "--profile", "bob"])
                .error()
                .contains("profiles/bob/pandora")
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
            ("spotify auth", Some(Kind::Config)),
            ("spotify auth --callback", Some(Kind::Auth)),
            ("spotify auth --callback --from-file f", Some(Kind::Auth)),
            ("match", Some(Kind::Other)),
            ("match --no-llm --only artists", Some(Kind::Other)),
            ("match --no-llm --limit 5", Some(Kind::Other)),
            ("match review", Some(Kind::Other)),
            ("match review --only tracks", Some(Kind::Other)),
            ("match questions", Some(Kind::Other)),
            ("match questions --only tracks --limit 5", Some(Kind::Other)),
            ("match answer", Some(Kind::Input)),
            ("match answer --from-file f --limit 1", Some(Kind::Other)),
            ("match set TR:1 spotify:track:x", Some(Kind::Input)),
            ("match reject TR:1", None),
            ("push", Some(Kind::Other)),
            ("push --dry-run --only liked", Some(Kind::Other)),
            ("push --only artists", Some(Kind::Other)),
            ("push --only thumbs-down", Some(Kind::Other)),
            ("status", None),
            ("report", Some(Kind::Other)),
            ("guide", None),
            ("guide matching", None),
            ("devices", Some(Kind::Config)),
            ("player", Some(Kind::Config)),
            ("play", Some(Kind::Config)),
            (
                "play spotify:track:4uLU6hMCjMI75M1A2tKUQC",
                Some(Kind::Config),
            ),
            ("play --station x", Some(Kind::Other)),
            ("play --item TR:1", Some(Kind::Input)),
            ("pause", Some(Kind::Config)),
            ("next", Some(Kind::Config)),
            ("previous", Some(Kind::Config)),
            (
                "queue spotify:track:4uLU6hMCjMI75M1A2tKUQC",
                Some(Kind::Config),
            ),
            ("shuffle on", Some(Kind::Config)),
            ("repeat off", Some(Kind::Config)),
            ("volume 50", Some(Kind::Config)),
            ("seek 30", Some(Kind::Config)),
            ("search x", Some(Kind::Config)),
            ("search x y --only tracks --limit 5", Some(Kind::Config)),
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
                .unwrap_or_else(|| panic!("no expected outcome for {joined:?}"))
                .1;
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

    #[test]
    fn a_player_refusal_writes_its_reason_and_devices() {
        let err = anyhow::Error::new(PlayerRefused {
            what: "play".into(),
            status: 0,
            reason: "SEVERAL_DEVICES".into(),
            message: String::new(),
            devices: vec![Device {
                id: "d1".into(),
                name: "Firefox".into(),
                kind: "Computer".into(),
                is_active: false,
                is_restricted: false,
                volume_percent: Some(80),
            }],
        })
        .context("playing");
        let mut out = Vec::new();
        report_failure(&parse(&["status", "--json"]), &err, &mut out).unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(doc["kind"], "player");
        assert_eq!(
            doc["details"],
            serde_json::json!({"reason": "SEVERAL_DEVICES", "devices": [
                {"id": "d1", "name": "Firefox", "type": "Computer", "is_active": false,
                 "is_restricted": false, "volume_percent": 80}]})
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
        let result = run(
            cli,
            || Ok(state_root.clone()),
            &mut &b""[..],
            &mut out,
            &mut Vec::new(),
        );
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
            || Ok(state_root.clone()),
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
    fn spotify_auth_writes_the_address_to_stdout_and_the_steps_to_stderr() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = setup(tmp.path());
        std::fs::write(
            repo.config_path(),
            "[spotify]\nclient_id = \"test-client-id\"\n",
        )
        .unwrap();
        let ran = exec(
            &with_repo(&repo, &["spotify", "auth", "--profile", "alice"]),
            &state_root,
            "",
        );
        ran.result.as_ref().unwrap();
        let state = state_root.profile("alice").unwrap();
        let pending = state.load_spotify_pending().unwrap().unwrap();
        assert_eq!(pending.client_id, "test-client-id");
        assert!(
            ran.out.contains("/authorize?client_id=test-client-id&"),
            "{}",
            ran.out
        );
        assert_eq!(ran.out.lines().count(), 1, "{}", ran.out);
        assert!(
            ran.err
                .contains("sss spotify auth --callback --profile alice"),
            "{}",
            ran.err
        );

        // The second step reads the address by the paste rule; a wrong state
        // is refused and keeps the pending sign-in.
        let ran = exec(
            &with_repo(
                &repo,
                &[
                    "spotify",
                    "auth",
                    "--callback",
                    "--profile",
                    "alice",
                    "--json",
                ],
            ),
            &state_root,
            "https://127.0.0.1/callback?code=c&state=wrong\n\n",
        );
        assert_eq!(ran.kind(), Kind::Input);
        assert_eq!(ran.document()["command"], "spotify auth");
        assert_eq!(ran.err, spotify::auth::CALLBACK_PROMPT);
        assert!(state.spotify_pending_path().exists());

        // Without a client id: kind config naming the key, nothing written.
        std::fs::write(repo.config_path(), "").unwrap();
        let ran = exec(
            &with_repo(&repo, &["spotify", "auth", "--profile", "bob"]),
            &state_root,
            "",
        );
        assert_eq!(ran.kind(), Kind::Config);
        assert!(
            ran.error().contains("`[spotify] client_id`"),
            "{}",
            ran.error()
        );
        assert!(!state_root.root().join("bob").exists());
    }

    #[test]
    fn auth_under_json_prompts_on_stderr_and_reads_by_the_paste_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = setup(tmp.path());
        let mut input: &[u8] = b"  \n";
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let args = with_repo(&repo, &["pandora", "auth", "--profile", "alice", "--json"]);
        let result = run(
            parse(&args),
            || Ok(state_root.clone()),
            &mut input,
            &mut out,
            &mut err,
        );
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
            || Ok(state_root.clone()),
            &mut input,
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(input, b"later\n");
    }

    /// A profile `alice` whose export holds collection track TR:1, matched
    /// `ambiguous` with candidates a and b.
    fn questions_fixture(repo: &Repo) {
        use crate::matching::model::{Candidate, MatchEntry, Status};
        use crate::pandora::model::{Bookmarks, Collection, Track};
        let paths = repo.profile("alice").unwrap();
        let mut tracks = std::collections::BTreeMap::new();
        tracks.insert(
            "TR:1".to_owned(),
            Track {
                pandora_id: "TR:1".into(),
                title: Some("Song".into()),
                artist: Some("The Band".into()),
                ..Track::default()
            },
        );
        repo::write_json(&paths.stations(), &serde_json::json!([])).unwrap();
        repo::write_json(&paths.playlists(), &serde_json::json!([])).unwrap();
        repo::write_json(
            &paths.collection(),
            &Collection {
                tracks: vec!["TR:1".into()],
                ..Collection::default()
            },
        )
        .unwrap();
        repo::write_json(&paths.bookmarks(), &Bookmarks::default()).unwrap();
        repo::write_json(&paths.tracks(), &tracks).unwrap();
        let candidate = |uri: &str| Candidate {
            uri: uri.into(),
            name: "Song".into(),
            score: 0.5,
            ..Candidate::default()
        };
        crate::matching::model::append(
            &paths.track_matches(),
            &MatchEntry {
                pandora_id: "TR:1".into(),
                status: Status::Ambiguous,
                spotify_uri: None,
                method: None,
                confidence: None,
                candidates: vec![candidate("spotify:track:a"), candidate("spotify:track:b")],
                queries: Vec::new(),
                decided_at: "2026-10-15T12:00:00Z".into(),
                session: None,
                reason: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn questions_print_json_and_answers_are_checked_and_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, state_root) = setup(tmp.path());
        std::fs::write(repo.config_path(), "[profiles.alice]\n").unwrap();
        questions_fixture(&repo);
        let run_args =
            |args: &[&str], input: &str| exec(&with_repo(&repo, args), &state_root, input);

        let ran = run_args(&["match", "questions"], "");
        ran.result.as_ref().unwrap();
        let doc = ran.document();
        assert_eq!(doc["items"][0]["item"], "TR:1");
        assert_eq!(doc["items"][0]["candidates"].as_array().unwrap().len(), 2);
        assert_eq!(doc["remaining"], 0);
        assert!(doc.get("ok").is_none(), "the text form is the bare object");
        assert_eq!(
            ran.err,
            "profile alice (the only one; pass --profile NAME to choose)\n"
        );
        let doc = run_args(&["match", "questions", "--limit", "0", "--json"], "").document();
        assert_eq!(
            (&doc["ok"], &doc["command"], &doc["remaining"]),
            (
                &serde_json::json!(true),
                &serde_json::json!("match questions"),
                &serde_json::json!(1)
            )
        );

        let bad = r#"{"decisions":[{"item":"TR:1","choice":"spotify:track:z","confidence":"high"},{"choice":null}]}"#;
        let ran = run_args(&["match", "answer", "--json"], &format!("{bad}\n\n"));
        assert_eq!(ran.kind(), Kind::Input);
        assert!(ran.err.contains(ANSWER_PROMPT), "{}", ran.err);
        let doc = ran.document();
        assert_eq!(doc["command"], "match answer");
        assert_eq!(
            doc["details"],
            serde_json::json!({"problems": [
                {"item": "TR:1",
                 "problem": "item TR:1: choice \"spotify:track:z\" was not offered for it"},
                {"item": null, "problem": "decision 2 has no \"item\" string"}]})
        );
        assert_eq!(
            doc["error"],
            "2 problems with the decisions; nothing was recorded: \
             item TR:1: choice \"spotify:track:z\" was not offered for it; \
             decision 2 has no \"item\" string"
        );

        let file = tmp.path().join("decisions.json");
        std::fs::write(
            &file,
            "{\"decisions\": [\n\n  {\"item\": \"TR:1\", \"choice\": \"spotify:track:b\",\n   \"confidence\": \"high\", \"reason\": \"same\"}\n]}\n",
        )
        .unwrap();
        let ran = run_args(
            &["match", "answer", "--from-file", file.to_str().unwrap()],
            "",
        );
        ran.result.as_ref().unwrap();
        assert_eq!(
            ran.out,
            "TR:1 matched to spotify:track:b (external, high)\n"
        );
        let doc = run_args(&["match", "questions", "--json"], "").document();
        assert_eq!(doc["items"], serde_json::json!([]));
        let doc = run_args(&["match", "review", "--json"], "").document();
        assert_eq!(doc["items"], serde_json::json!([]));
    }

    #[test]
    fn guide_prints_its_documents_with_no_repository_or_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-repo");
        let missing = missing.to_str().unwrap();
        // An empty state root, a repository that does not exist and two
        // profiles' worth of ambiguity: none of it is read.
        let state_root = StateRoot::at(tmp.path().join("state"));
        let ran = exec(
            &["--repo", missing, "--profile", "alice", "guide"],
            &state_root,
            "",
        );
        ran.result.as_ref().unwrap();
        assert_eq!(ran.out, guide::AGENT);
        assert_eq!(ran.err, "");
        // The state root is never located.
        let mut out = Vec::new();
        run(
            parse(&["guide"]),
            || panic!("guide located the state root"),
            &mut &b""[..],
            &mut out,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(out, guide::AGENT.as_bytes());
        let ran = exec(&["--repo", missing, "guide", "matching"], &state_root, "");
        assert_eq!(ran.out, guide::MATCHING);
        let ran = exec(&["guide", "agent", "--json"], &state_root, "");
        assert_eq!(
            ran.document(),
            serde_json::json!({"ok": true, "command": "guide", "topic": "agent",
                "text": guide::AGENT})
        );
        assert!(!tmp.path().join("no-such-repo").exists());
        assert!(!tmp.path().join("state").exists());
    }

    #[test]
    fn the_guide_names_every_command() {
        let mut names: Vec<&str> = DOCUMENTED.iter().map(|(_, name)| *name).collect();
        names.dedup();
        // Every command line clap accepts is in `DOCUMENTED`, which
        // `every_documented_command_parses` ties to `Command::name()`.
        let mut runnable = Vec::new();
        runnable_paths(&Cli::command(), "", &mut runnable);
        let mut documented: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        documented.sort();
        runnable.sort();
        assert_eq!(
            documented, runnable,
            "DOCUMENTED and the command tree differ"
        );
        for name in names {
            assert!(
                guide::AGENT.contains(&format!("sss {name}")),
                "docs/llm-guide.md never shows `sss {name}`"
            );
        }
    }

    /// The space-joined subcommand path of every command under `cmd` that
    /// runs on its own: one with no subcommands, or whose subcommand is
    /// optional.
    fn runnable_paths(cmd: &clap::Command, path: &str, found: &mut Vec<String>) {
        if !path.is_empty()
            && (cmd.get_subcommands().next().is_none() || !cmd.is_subcommand_required_set())
        {
            found.push(path.to_owned());
        }
        for sub in cmd.get_subcommands() {
            let name = sub.get_name();
            let sub_path = if path.is_empty() {
                name.to_owned()
            } else {
                format!("{path} {name}")
            };
            runnable_paths(sub, &sub_path, found);
        }
    }
}
