//! The `push` command: the plan of what the matched material makes in the
//! member's Spotify account, its idempotent application, and the ledger
//! `spotify/state.json` that records every write.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{Config, PushConfig};
use crate::lazy::Lazy;
use crate::matching::model::{self, ByKind, MatchEntry};
use crate::pandora::model::{Normalized, Playlist, SeedKind, Station};
use crate::repo::{self, ProfilePaths};
use crate::source::Exports;
use crate::spotify::auth::PLAYLIST_MODIFY_PUBLIC_SCOPE;
use crate::spotify::client::{Spotify, http_status};
use crate::youtube;

/// Longest playlist name Spotify takes, in characters.
pub const NAME_MAX: usize = 100;

/// Longest playlist description Spotify takes, in characters.
pub const DESCRIPTION_MAX: usize = 300;

/// The ledger key and name of the thumbs-down reference playlist.
pub const THUMBS_DOWN_KEY: &str = "thumbs-down";
pub const THUMBS_DOWN_NAME: &str = "Pandora thumbs-down (reference)";

/// One playlist the tool made or started to make, keyed in the ledger by
/// its source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PlaylistEntry {
    /// Written just before the create request; still here means the run
    /// ended before the playlist's id was recorded.
    Pending { name: String, at: String },
    Created {
        spotify_id: String,
        #[serde(default)]
        snapshot_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        /// What this tool has added, in order.
        #[serde(default)]
        items: Vec<String>,
        #[serde(default)]
        at: Option<String>,
    },
}

/// When a URI was saved to (or found in) the member's library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub at: String,
}

/// `spotify/state.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub playlists: BTreeMap<String, PlaylistEntry>,
    #[serde(default)]
    pub library: BTreeMap<String, LibraryEntry>,
}

impl Ledger {
    /// The ledger at `path`; none yet is an empty one.
    pub fn load(path: &Path) -> Result<Self> {
        Ok(repo::read_json_opt(path)?.unwrap_or_default())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        repo::write_json(path, self)
    }
}

/// The ledger key of `station`: its `ST:` id, else `station:<station_id>`.
pub fn station_key(station: &Station) -> String {
    station
        .pandora_id
        .clone()
        .unwrap_or_else(|| format!("station:{}", station.station_id))
}

/// What to do about a `pending` ledger entry, for the owner. `name` and
/// `state_file` (the ledger's path) are placed as given.
pub fn pending_instruction(key: &str, name: &str, at: &str, state_file: &str) -> String {
    format!(
        "Playlist `{key}` (\"{name}\") may have been created at {at} without being recorded. \
         Look in Spotify for a playlist of that name created then. If it is there, either keep \
         it — replace the entry in {state_file} with `\"{key}\": {{\"status\":\"created\",\"spotify_id\":\"<id>\"}}` \
         — or delete it in Spotify and remove the entry. If it is not there, remove the entry. \
         Then re-run."
    )
}

/// Whether `err` from a Spotify write certainly left nothing behind: the
/// request was refused before it was sent (sign-in, configuration) or
/// answered with a refusal (401, 403, 429, another 4xx). No response, a
/// 5xx or an unreadable 2xx body may have been applied, and is not.
fn not_applied(err: &anyhow::Error) -> bool {
    http_status(err).is_some()
        || matches!(
            crate::error::kind(err),
            crate::error::Kind::Auth | crate::error::Kind::Config | crate::error::Kind::Quota
        )
}

/// The parts of a push, in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Artists,
    Stations,
    Playlists,
    Collection,
    Liked,
    MostPlayed,
    ThumbsDown,
}

impl Part {
    pub const ALL: [Part; 7] = [
        Part::Artists,
        Part::Stations,
        Part::Playlists,
        Part::Collection,
        Part::Liked,
        Part::MostPlayed,
        Part::ThumbsDown,
    ];
}

/// The playlists `part` makes or keeps up to date under `config`, from
/// every source `exports` holds: none for a library part or a playlist
/// part switched off.
pub fn wanted_playlists(
    part: Part,
    exports: &Exports,
    matches: &Matches,
    config: &Config,
) -> Vec<Wanted> {
    let mut wanted: Vec<Wanted> = exports
        .pandora
        .as_ref()
        .map(|export| pandora_wanted_playlists(part, export, matches, &config.push))
        .unwrap_or_default();
    if let Some(data) = &exports.youtube {
        match part {
            Part::Playlists => wanted.extend(youtube::select::playlists(data, matches, config)),
            Part::MostPlayed => {
                wanted.extend(youtube::select::most_played_playlist(data, matches, config))
            }
            Part::Artists | Part::Stations | Part::Collection | Part::Liked | Part::ThumbsDown => {}
        }
    }
    wanted
}

/// The library URIs library part `part` saves, from every source `exports`
/// holds, Pandora's first, each once; none for a playlist part.
fn library_uris(part: Part, exports: &Exports, matches: &Matches, config: &Config) -> Vec<String> {
    let pandora = exports.pandora.as_ref();
    let youtube = exports.youtube.as_ref();
    let yt = &config.youtube;
    let (first, more) = match part {
        Part::Artists => (
            pandora.map(|e| artist_uris(e, matches)),
            youtube.map(|d| youtube::select::artist_uris(d, matches)),
        ),
        Part::Collection => (
            pandora.map(|e| collection_uris(e, matches)),
            youtube.map(|d| youtube::select::collection_uris(d, matches, yt)),
        ),
        Part::Liked => (
            pandora.map(|e| liked_uris(e, matches)),
            youtube.map(|d| youtube::select::liked_uris(d, matches, yt)),
        ),
        Part::Stations | Part::Playlists | Part::MostPlayed | Part::ThumbsDown => {
            return Vec::new();
        }
    };
    let mut uris = first.unwrap_or_default();
    uris.extend(more.unwrap_or_default());
    let mut seen = HashSet::new();
    uris.retain(|uri| seen.insert(uri.clone()));
    uris
}

/// The playlists `part` makes or keeps up to date from the Pandora export.
fn pandora_wanted_playlists(
    part: Part,
    export: &Normalized,
    matches: &Matches,
    config: &PushConfig,
) -> Vec<Wanted> {
    match part {
        Part::Stations => station_playlists(export, matches, config),
        Part::Playlists => pandora_playlists(export, matches, config),
        Part::ThumbsDown if config.thumbs_down_playlist => {
            thumbs_down_playlist(export, matches).into_iter().collect()
        }
        Part::ThumbsDown | Part::Artists | Part::Collection | Part::Liked | Part::MostPlayed => {
            Vec::new()
        }
    }
}

/// The latest match entry per item id, per kind.
pub type Matches = ByKind<BTreeMap<String, MatchEntry>>;

/// Playlist kinds never pushed: they duplicate the station playlists and
/// the thumbs.
pub const NEVER_PUSHED_KINDS: [&str; 2] = ["StationThumbs", "MyThumbsUp"];

/// Whether push sends `playlist`: the member's own, and with
/// `linked_playlists` every Pandora-made kind but [`NEVER_PUSHED_KINDS`].
pub fn playlist_is_pushed(playlist: &Playlist, push: &PushConfig) -> bool {
    match playlist.linked_type.as_deref() {
        None => true,
        Some(kind) => push.linked_playlists && !NEVER_PUSHED_KINDS.contains(&kind),
    }
}

/// The matched URIs of `ids` in order, each once.
pub(crate) fn matched<'a>(
    map: &'a BTreeMap<String, MatchEntry>,
    ids: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.into_iter()
        .filter_map(|id| map.get(id)?.matched_uri())
        .filter(|uri| seen.insert(*uri))
        .map(str::to_owned)
        .collect()
}

/// `text` cut to `max` characters.
pub(crate) fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// `text` cut so that it and ` sss:<key>` fit in [`DESCRIPTION_MAX`],
/// followed by the marker.
pub(crate) fn with_marker(text: &str, key: &str) -> String {
    let marker = format!("sss:{key}");
    let room = DESCRIPTION_MAX.saturating_sub(marker.chars().count() + 1);
    let text = cut(text.trim(), room);
    if text.is_empty() {
        cut(&marker, DESCRIPTION_MAX)
    } else {
        format!("{text} {marker}")
    }
}

/// A playlist push makes or keeps up to date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub key: String,
    pub name: String,
    pub description: String,
    /// The matched URIs, in order, each once.
    pub uris: Vec<String>,
    /// Matched tracks the source holds more than once.
    pub repeats: usize,
}

/// A station playlist for every station that is not a Thumbprint or
/// shuffle station and has a matched track seed or thumbs-up: the seeds,
/// then the thumbs-up in thumb order.
pub fn station_playlists(export: &Normalized, matches: &Matches, push: &PushConfig) -> Vec<Wanted> {
    let mut wanted = Vec::new();
    for station in &export.stations {
        if station.is_thumbprint || station.is_shuffle {
            continue;
        }
        let up: Vec<&str> = station
            .thumbs_up_in_order()
            .into_iter()
            .filter_map(|f| f.track_pandora_id.as_deref())
            .collect();
        let seeds = station
            .seeds
            .iter()
            .filter(|s| s.kind == SeedKind::Track)
            .filter_map(|s| s.pandora_id.as_deref());
        let uris = matched(&matches.tracks, seeds.chain(up.iter().copied()));
        if uris.is_empty() {
            continue;
        }
        let key = station_key(station);
        let name = station.name.clone().unwrap_or_else(|| key.clone());
        let up_matched = matched(&matches.tracks, up.iter().copied()).len();
        let seed_names: Vec<&str> = station
            .seeds
            .iter()
            .filter_map(|s| s.name.as_deref())
            .collect();
        let description = format!(
            "Pandora station \"{name}\": {up_matched} of {} thumbs-up matched; seeds: {}.",
            up.len(),
            seed_names.join(", ")
        );
        wanted.push(Wanted {
            name: cut(&format!("{}{name}", push.playlist_prefix), NAME_MAX),
            description: with_marker(&description, &key),
            key,
            uris,
            repeats: 0,
        });
    }
    wanted
}

/// A playlist for every pushed Pandora playlist with a matched item: its
/// name, its description and its matched tracks in Pandora order.
pub fn pandora_playlists(export: &Normalized, matches: &Matches, push: &PushConfig) -> Vec<Wanted> {
    let mut wanted = Vec::new();
    for playlist in &export.playlists {
        if !playlist_is_pushed(playlist, push) {
            continue;
        }
        let ids = playlist.items.iter().map(String::as_str);
        let uris = matched(&matches.tracks, ids.clone());
        if uris.is_empty() {
            continue;
        }
        let all = ids.filter_map(|id| matches.tracks.get(id)?.matched_uri());
        let key = playlist.pandora_id.clone();
        wanted.push(Wanted {
            name: cut(playlist.name.as_deref().unwrap_or(&key), NAME_MAX),
            description: with_marker(playlist.description.as_deref().unwrap_or(""), &key),
            repeats: all.count() - uris.len(),
            key,
            uris,
        });
    }
    wanted
}

/// The thumbs-down reference playlist: every matched thumbs-down track,
/// station by station in capture order; none when nothing matched.
pub fn thumbs_down_playlist(export: &Normalized, matches: &Matches) -> Option<Wanted> {
    let ids = export
        .stations
        .iter()
        .flat_map(|s| &s.feedback)
        .filter(|f| !f.positive)
        .filter_map(|f| f.track_pandora_id.as_deref());
    let uris = matched(&matches.tracks, ids);
    (!uris.is_empty()).then(|| Wanted {
        key: THUMBS_DOWN_KEY.to_owned(),
        name: THUMBS_DOWN_NAME.to_owned(),
        description: with_marker(
            "Tracks thumbed down on Pandora, for reference.",
            THUMBS_DOWN_KEY,
        ),
        uris,
        repeats: 0,
    })
}

/// The matched artists of the station seeds and the collection.
pub fn artist_uris(export: &Normalized, matches: &Matches) -> Vec<String> {
    let seeds = export
        .stations
        .iter()
        .flat_map(|s| &s.seeds)
        .filter(|s| s.kind == SeedKind::Artist)
        .filter_map(|s| s.pandora_id.as_deref());
    let collected = export
        .collection
        .artists
        .iter()
        .map(|a| a.pandora_id.as_str());
    matched(&matches.artists, seeds.chain(collected))
}

/// The matched collection tracks, albums and artists and bookmarked tracks
/// and albums.
pub fn collection_uris(export: &Normalized, matches: &Matches) -> Vec<String> {
    let c = &export.collection;
    let b = &export.bookmarks;
    let mut uris = matched(
        &matches.tracks,
        c.tracks
            .iter()
            .map(String::as_str)
            .chain(b.tracks.iter().filter_map(|t| t.pandora_id.as_deref())),
    );
    uris.extend(matched(
        &matches.albums,
        c.albums
            .iter()
            .map(|a| a.pandora_id.as_str())
            .chain(b.albums.iter().filter_map(|a| a.pandora_id.as_deref())),
    ));
    uris.extend(matched(
        &matches.artists,
        c.artists.iter().map(|a| a.pandora_id.as_str()),
    ));
    uris
}

/// Every matched thumbs-up track of every station, Thumbprint included.
pub fn liked_uris(export: &Normalized, matches: &Matches) -> Vec<String> {
    let ids = export
        .stations
        .iter()
        .flat_map(|s| &s.feedback)
        .filter(|f| f.positive)
        .filter_map(|f| f.track_pandora_id.as_deref());
    matched(&matches.tracks, ids)
}

/// What happens to one playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// No ledger entry: the playlist is made.
    Create,
    /// Items are missing from a playlist the tool made.
    Update,
    /// Nothing to add.
    Skip,
    /// A `pending` ledger entry: left alone until the owner resolves it.
    Pending,
}

/// One playlist of the plan or the push.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlaylistReport {
    pub key: String,
    pub name: String,
    pub action: Action,
    pub to_add: usize,
    pub added: usize,
    /// When a `pending` entry was written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// Matched tracks the source playlist repeats; Spotify gets each once.
    #[serde(skip_serializing_if = "is_zero")]
    pub repeats: usize,
    /// The created playlist's Spotify id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spotify_id: Option<String>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// A playlist part: `enabled` is false when its `[push]` switch is off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlaylistsPart {
    pub enabled: bool,
    pub playlists: Vec<PlaylistReport>,
}

/// A library part: URIs to save, saved this run, and already saved (in
/// the ledger, or found in the library).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LibraryPart {
    pub enabled: bool,
    pub to_save: usize,
    pub saved: usize,
    pub skipped: usize,
}

/// The artist part: follows are library saves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FollowsPart {
    pub to_follow: usize,
    pub followed: usize,
    pub skipped: usize,
}

/// The parts a push covered; a part not selected is absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Parts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artists: Option<FollowsPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stations: Option<PlaylistsPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlists: Option<PlaylistsPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection: Option<LibraryPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liked: Option<LibraryPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub most_played: Option<PlaylistsPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbs_down: Option<PlaylistsPart>,
}

/// What `sss push` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PushReport {
    pub profile: String,
    pub dry_run: bool,
    pub parts: Parts,
    /// The ledger, for the pending-entry instructions.
    #[serde(skip)]
    pub state_file: PathBuf,
}

impl PushReport {
    /// Every `pending` playlist with what the owner is to do about it.
    pub fn pending_instructions(&self) -> Vec<String> {
        [
            &self.parts.stations,
            &self.parts.playlists,
            &self.parts.most_played,
            &self.parts.thumbs_down,
        ]
        .into_iter()
        .flatten()
        .flat_map(|p| &p.playlists)
        .filter(|p| p.action == Action::Pending)
        .map(|p| {
            pending_instruction(
                &p.key,
                &p.name,
                p.at.as_deref().unwrap_or("an unknown time"),
                &self.state_file.display().to_string(),
            )
        })
        .collect()
    }

    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        let (verb, verbed) = if self.dry_run {
            ("dry run: nothing changed", "would add")
        } else {
            ("pushed", "added")
        };
        writeln!(out, "{verb} for profile {}", self.profile)?;
        let p = &self.parts;
        if let Some(a) = &p.artists {
            writeln!(
                out,
                "artists: {} to follow, {} followed, {} already followed",
                a.to_follow, a.followed, a.skipped
            )?;
        }
        for (label, table, part) in [
            ("stations", "push", &p.stations),
            ("playlists", "push", &p.playlists),
            ("most-played", "youtube", &p.most_played),
            ("thumbs-down", "push", &p.thumbs_down),
        ] {
            let Some(part) = part else { continue };
            if !part.enabled {
                writeln!(out, "{label}: off in config.toml [{table}]")?;
                continue;
            }
            writeln!(out, "{label}: {} playlists", part.playlists.len())?;
            for pl in &part.playlists {
                let action = match pl.action {
                    Action::Create => "create",
                    Action::Update => "update",
                    Action::Skip => "up to date",
                    Action::Pending => "pending",
                };
                let added = if self.dry_run { pl.to_add } else { pl.added };
                let repeats = if pl.repeats > 0 {
                    format!(", {} repeated tracks once", pl.repeats)
                } else {
                    String::new()
                };
                writeln!(
                    out,
                    "  {action} \"{}\" ({}): {verbed} {added} of {}{repeats}",
                    pl.name, pl.key, pl.to_add
                )?;
            }
        }
        for (label, part) in [("collection", &p.collection), ("liked songs", &p.liked)] {
            let Some(part) = part else { continue };
            if !part.enabled {
                writeln!(out, "{label}: off in config.toml [push]")?;
                continue;
            }
            writeln!(
                out,
                "{label}: {} to save, {} saved, {} already saved",
                part.to_save, part.saved, part.skipped
            )?;
        }
        for instruction in self.pending_instructions() {
            writeln!(out, "{instruction}")?;
        }
        Ok(())
    }
}

/// How the client is made: on first need, so a dry run or a push with
/// nothing to do needs no token.
pub type Connect<'a> = Box<dyn FnOnce() -> Result<Spotify> + 'a>;

/// What `sss push` is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    pub only: Option<Part>,
    pub dry_run: bool,
}

struct Pusher<'a> {
    profile: String,
    ledger: Ledger,
    state_file: PathBuf,
    dry_run: bool,
    public: bool,
    client: Lazy<Spotify, Connect<'a>>,
    progress: &'a mut dyn Write,
}

/// Plans the selected parts from the exports, the match entries and the
/// ledger; unless `dry_run`, applies them, writing the ledger after every
/// Spotify write. A failure leaves the ledger consistent with what landed.
pub fn push<'a>(
    profile: &str,
    paths: &ProfilePaths,
    exports: &Exports,
    config: &Config,
    options: Options,
    connect: Connect<'a>,
    progress: &'a mut dyn Write,
) -> Result<PushReport> {
    let matches = model::read_all_latest(paths)?;
    let state_file = paths.spotify_state();
    let mut pusher = Pusher {
        profile: profile.to_owned(),
        ledger: Ledger::load(&state_file)?,
        state_file: state_file.clone(),
        dry_run: options.dry_run,
        public: config.push.public_playlists,
        client: Lazy::new(connect),
        progress,
    };
    let mut parts = Parts::default();
    for part in Part::ALL {
        if options.only.is_some_and(|only| only != part) {
            continue;
        }
        match part {
            Part::Artists => {
                let r = pusher
                    .save_library("artists", library_uris(part, exports, &matches, config))?;
                parts.artists = Some(FollowsPart {
                    to_follow: r.to_save,
                    followed: r.saved,
                    skipped: r.skipped,
                });
            }
            Part::Stations => {
                let wanted = wanted_playlists(part, exports, &matches, config);
                parts.stations = Some(pusher.playlists(true, wanted)?);
            }
            Part::Playlists => {
                let wanted = wanted_playlists(part, exports, &matches, config);
                parts.playlists = Some(pusher.playlists(true, wanted)?);
            }
            Part::Collection => {
                parts.collection =
                    Some(pusher.save_library(
                        "collection",
                        library_uris(part, exports, &matches, config),
                    )?);
            }
            Part::Liked => {
                parts.liked = Some(if config.push.liked_songs {
                    pusher.save_library(
                        "liked songs",
                        library_uris(part, exports, &matches, config),
                    )?
                } else {
                    LibraryPart::default()
                });
            }
            Part::MostPlayed => {
                let wanted = wanted_playlists(part, exports, &matches, config);
                parts.most_played = Some(pusher.playlists(config.youtube.most_played > 0, wanted)?);
            }
            Part::ThumbsDown => {
                let wanted = wanted_playlists(part, exports, &matches, config);
                parts.thumbs_down =
                    Some(pusher.playlists(config.push.thumbs_down_playlist, wanted)?);
            }
        }
    }
    Ok(PushReport {
        profile: profile.to_owned(),
        dry_run: options.dry_run,
        parts,
        state_file,
    })
}

impl Pusher<'_> {
    fn note(&mut self, line: &str) {
        // Progress is for people; a failed write of it does not end the push.
        let _ = writeln!(self.progress, "{line}");
    }

    /// Saves `uris` not in the ledger's library, after asking which the
    /// library already holds; both kinds are recorded.
    fn save_library(&mut self, label: &str, uris: Vec<String>) -> Result<LibraryPart> {
        let (known, todo): (Vec<String>, Vec<String>) = uris
            .into_iter()
            .partition(|u| self.ledger.library.contains_key(u));
        let mut part = LibraryPart {
            enabled: true,
            to_save: todo.len(),
            saved: 0,
            skipped: known.len(),
        };
        if self.dry_run || todo.is_empty() {
            return Ok(part);
        }
        self.note(&format!("{label}: checking {} in the library", todo.len()));
        let Pusher {
            ledger,
            state_file,
            client,
            ..
        } = self;
        let client = client.get()?;
        let held = client.library_contains(&todo)?;
        let at = repo::now_rfc3339()?;
        let mut send = Vec::new();
        for (uri, held) in todo.into_iter().zip(held) {
            if held {
                ledger.library.insert(uri, LibraryEntry { at: at.clone() });
                part.skipped += 1;
            } else {
                send.push(uri);
            }
        }
        if part.skipped > known.len() {
            ledger.save(state_file)?;
        }
        client.library_add(&send, &mut |chunk| {
            let at = repo::now_rfc3339()?;
            for uri in chunk {
                ledger
                    .library
                    .insert(uri.clone(), LibraryEntry { at: at.clone() });
            }
            ledger.save(state_file)?;
            part.saved += chunk.len();
            Ok(())
        })?;
        self.note(&format!("{label}: saved {}", part.saved));
        Ok(part)
    }

    fn playlists(&mut self, enabled: bool, wanted: Vec<Wanted>) -> Result<PlaylistsPart> {
        let mut part = PlaylistsPart {
            enabled,
            playlists: Vec::new(),
        };
        if enabled {
            for w in wanted {
                part.playlists.push(self.playlist(w)?);
            }
        }
        Ok(part)
    }

    /// One playlist by its ledger entry: none → create and fill; `created`
    /// → add the matched URIs it lacks (made again when Spotify answers 404
    /// for its id); `pending` → left alone.
    fn playlist(&mut self, want: Wanted) -> Result<PlaylistReport> {
        let mut report = PlaylistReport {
            key: want.key.clone(),
            name: want.name.clone(),
            action: Action::Create,
            to_add: want.uris.len(),
            added: 0,
            at: None,
            repeats: want.repeats,
            spotify_id: None,
        };
        match self.ledger.playlists.get(&want.key).cloned() {
            Some(PlaylistEntry::Pending { name, at }) => {
                report.action = Action::Pending;
                report.name = name;
                report.at = Some(at);
                Ok(report)
            }
            Some(PlaylistEntry::Created {
                spotify_id, items, ..
            }) => {
                report.spotify_id = Some(spotify_id.clone());
                let present: Vec<String> = if self.dry_run {
                    items
                } else {
                    match self.client.get()?.playlist_items(&spotify_id) {
                        Ok(uris) => uris.into_iter().flatten().collect(),
                        Err(e) if http_status(&e) == Some(404) => {
                            self.note(&format!(
                                "playlist \"{}\" ({}): Spotify does not know its id; making it again",
                                want.name, want.key
                            ));
                            return self.create(want, report);
                        }
                        Err(e) => return Err(e),
                    }
                };
                let present: HashSet<&str> = present.iter().map(String::as_str).collect();
                let missing: Vec<String> = want
                    .uris
                    .iter()
                    .filter(|u| !present.contains(u.as_str()))
                    .cloned()
                    .collect();
                report.to_add = missing.len();
                report.action = if missing.is_empty() {
                    Action::Skip
                } else {
                    Action::Update
                };
                if !self.dry_run && !missing.is_empty() {
                    self.note(&format!(
                        "adding {} to \"{}\" ({})",
                        missing.len(),
                        want.name,
                        want.key
                    ));
                    report.added = self.add_items(&want.key, &spotify_id, &missing)?;
                }
                Ok(report)
            }
            None if self.dry_run => Ok(report),
            None => self.create(want, report),
        }
    }

    /// Makes the client, writes `pending`, creates the playlist, writes
    /// `created` and adds every wanted URI. A public playlist needs a token
    /// granted `playlist-modify-public`; without it the `auth` failure comes
    /// before the ledger is touched. A create Spotify refused, or that was
    /// never sent, puts the key's prior entry back before the error is
    /// returned; any other failure leaves `pending`.
    fn create(&mut self, want: Wanted, mut report: PlaylistReport) -> Result<PlaylistReport> {
        self.note(&format!(
            "creating \"{}\" ({}) with {} tracks",
            want.name,
            want.key,
            want.uris.len()
        ));
        let client = self.client.get()?;
        if self.public {
            client.require_scopes(&self.profile, &[PLAYLIST_MODIFY_PUBLIC_SCOPE])?;
        }
        let at = repo::now_rfc3339()?;
        let prior = self.ledger.playlists.insert(
            want.key.clone(),
            PlaylistEntry::Pending {
                name: want.name.clone(),
                at: at.clone(),
            },
        );
        self.ledger.save(&self.state_file)?;
        let created =
            match self
                .client
                .get()?
                .playlist_create(&want.name, self.public, &want.description)
            {
                Ok(created) => created,
                Err(e) if not_applied(&e) => {
                    match prior {
                        Some(entry) => self.ledger.playlists.insert(want.key.clone(), entry),
                        None => self.ledger.playlists.remove(&want.key),
                    };
                    self.ledger.save(&self.state_file)?;
                    return Err(e);
                }
                Err(e) => return Err(e),
            };
        self.ledger.playlists.insert(
            want.key.clone(),
            PlaylistEntry::Created {
                spotify_id: created.id.clone(),
                snapshot_id: created.snapshot_id,
                name: Some(want.name.clone()),
                items: Vec::new(),
                at: Some(at),
            },
        );
        self.ledger.save(&self.state_file)?;
        report.action = Action::Create;
        report.to_add = want.uris.len();
        report.spotify_id = Some(created.id.clone());
        report.added = self.add_items(&want.key, &created.id, &want.uris)?;
        Ok(report)
    }

    /// Adds `uris` to the playlist, recording each chunk in the ledger
    /// entry `key` as it lands. Returns how many were added.
    fn add_items(&mut self, key: &str, spotify_id: &str, uris: &[String]) -> Result<usize> {
        let Pusher {
            ledger,
            state_file,
            client,
            ..
        } = self;
        let mut added = 0;
        client
            .get()?
            .playlist_add_items(spotify_id, uris, &mut |chunk, snapshot| {
                if let Some(PlaylistEntry::Created {
                    items, snapshot_id, ..
                }) = ledger.playlists.get_mut(key)
                {
                    items.extend_from_slice(chunk);
                    if let Some(s) = snapshot {
                        *snapshot_id = Some(s.to_owned());
                    }
                }
                ledger.save(state_file)?;
                added += chunk.len();
                Ok(())
            })?;
        Ok(added)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{self, Kind};
    use crate::matching::model::{Confidence, Method, Status};
    use crate::pandora::model::{Album, Artist, Bookmark, Feedback, Playlist, Seed};
    use crate::secrets::StateDir;
    use crate::spotify::ids::IdKind;
    use crate::spotify::test_support::{FAST, fresh_token, state_with};
    use httpmock::prelude::*;
    use serde_json::json;

    fn thumb(track: &str, positive: bool, created: Option<&str>) -> Feedback {
        Feedback {
            feedback_id: format!("F{track}"),
            track_pandora_id: Some(track.into()),
            positive,
            created: created.map(str::to_owned),
            station_id: "1".into(),
        }
    }

    fn seed(kind: SeedKind, id: &str, name: &str) -> Seed {
        Seed {
            kind,
            pandora_id: Some(id.into()),
            name: Some(name.into()),
        }
    }

    fn station(station_id: &str, key: Option<&str>, name: &str) -> Station {
        Station {
            station_id: station_id.into(),
            pandora_id: key.map(str::to_owned),
            name: Some(name.into()),
            created: None,
            last_played: None,
            is_thumbprint: false,
            is_shuffle: false,
            genres: Vec::new(),
            positive_feedback_count: None,
            negative_feedback_count: None,
            seeds: Vec::new(),
            feedback: Vec::new(),
        }
    }

    fn playlist(id: &str, linked: Option<&str>, items: &[&str]) -> Playlist {
        Playlist {
            pandora_id: id.into(),
            name: Some(format!("List {id}")),
            description: Some("Made on Pandora".into()),
            created: None,
            linked_type: linked.map(str::to_owned),
            items: items.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn bookmark(id: &str) -> Bookmark {
        Bookmark {
            pandora_id: Some(id.into()),
            title: Some("Marked".into()),
            artist: Some("Someone".into()),
            created: None,
        }
    }

    /// Station `ST:9` (station id 41) seeded with artist `AR:1` and track
    /// `TR:s`, thumbs up `TR:b` (later) and `TR:a` (earlier), thumbs down
    /// `TR:d`; a Thumbprint station thumbing up `TR:p`; a shuffle station
    /// thumbing up `TR:b`; the member's playlist `PL:1` repeating `TR:a`, a
    /// `Personalized`, a `StationThumbs` and a `MyThumbsUp` one; a
    /// collection and bookmarks.
    fn with_push(push: &PushConfig) -> Config {
        Config {
            push: push.clone(),
            ..Config::default()
        }
    }

    fn pandora_only(export: Normalized) -> Exports {
        Exports {
            pandora: Some(export),
            youtube: None,
        }
    }

    fn export() -> Normalized {
        let mut st = station("41", Some("ST:9"), "Morning");
        st.seeds = vec![
            seed(SeedKind::Artist, "AR:1", "The Band"),
            seed(SeedKind::Track, "TR:s", "Seed Song"),
            seed(SeedKind::Genre, "G:1", "Jazz"),
        ];
        st.feedback = vec![
            thumb("TR:b", true, Some("2024-02-01T00:00:00Z")),
            thumb("TR:a", true, Some("2023-01-01T00:00:00Z")),
            thumb("TR:x", true, None),
            thumb("TR:d", false, None),
        ];
        let mut print = station("42", Some("ST:p"), "Thumbprint Radio");
        print.is_thumbprint = true;
        print.feedback = vec![thumb("TR:p", true, None)];
        let mut shuffle = station("43", Some("ST:sh"), "Shuffle");
        shuffle.is_shuffle = true;
        shuffle.feedback = vec![thumb("TR:b", true, None)];
        Normalized {
            stations: vec![st, print, shuffle],
            playlists: vec![
                playlist("PL:1", None, &["TR:a", "TR:c", "TR:a", "TR:none"]),
                playlist("PL:2", Some("Personalized"), &["TR:c"]),
                playlist("PL:3", Some("StationThumbs"), &["TR:b"]),
                playlist("PL:4", Some("MyThumbsUp"), &["TR:a"]),
            ],
            collection: crate::pandora::model::Collection {
                tracks: vec!["TR:c".into()],
                albums: vec![Album {
                    pandora_id: "AL:1".into(),
                    name: Some("Record".into()),
                    artist: Some("The Band".into()),
                    artist_pandora_id: None,
                }],
                artists: vec![Artist {
                    pandora_id: "AR:2".into(),
                    name: Some("Other".into()),
                }],
            },
            bookmarks: crate::pandora::model::Bookmarks {
                tracks: vec![bookmark("TR:k")],
                albums: vec![bookmark("AL:2")],
            },
            tracks: BTreeMap::new(),
        }
    }

    fn entry(id: &str, uri: Option<&str>) -> MatchEntry {
        MatchEntry {
            item_id: id.into(),
            status: if uri.is_some() {
                Status::Matched
            } else {
                Status::Unmatched
            },
            spotify_uri: uri.map(str::to_owned),
            method: Some(Method::Exact),
            confidence: Some(Confidence::High),
            candidates: Vec::new(),
            queries: Vec::new(),
            decided_at: "2026-10-15T00:00:00Z".into(),
            session: None,
            reason: None,
        }
    }

    /// Every track matched to `spotify:track:<letter>` but `TR:x`; both
    /// artists, both albums matched.
    fn write_matches(paths: &ProfilePaths) {
        for id in ["TR:s", "TR:a", "TR:b", "TR:c", "TR:d", "TR:p", "TR:k"] {
            let uri = format!("spotify:track:{}", &id[3..]);
            model::append(&paths.track_matches(), &entry(id, Some(&uri))).unwrap();
        }
        model::append(&paths.track_matches(), &entry("TR:x", None)).unwrap();
        for id in ["AR:1", "AR:2"] {
            let uri = format!("spotify:artist:{}", &id[3..]);
            model::append(&paths.artist_matches(), &entry(id, Some(&uri))).unwrap();
        }
        for id in ["AL:1", "AL:2"] {
            let uri = format!("spotify:album:{}", &id[3..]);
            model::append(&paths.album_matches(), &entry(id, Some(&uri))).unwrap();
        }
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        paths: ProfilePaths,
        state: StateDir,
        server: MockServer,
    }

    fn fixture() -> Fixture {
        fixture_with(&[])
    }

    /// A fixture whose stored token was granted `scopes`.
    fn fixture_with(scopes: &[&str]) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::repo::Repo::at(tmp.path().join("data"))
            .profile("alice")
            .unwrap();
        write_matches(&paths);
        let state = state_with(&tmp.path().join("state"), &fresh_token(scopes));
        Fixture {
            _tmp: tmp,
            paths,
            state,
            server: MockServer::start(),
        }
    }

    impl Fixture {
        fn run(&self, config: &PushConfig, options: Options) -> Result<PushReport> {
            let state = self.state.clone();
            let (api, auth) = (self.server.url("/v1"), self.server.base_url());
            let connect: Connect<'_> =
                Box::new(move || Spotify::new(state, "test-client-id", "", &api, &auth, FAST));
            push(
                "alice",
                &self.paths,
                &pandora_only(export()),
                &with_push(config),
                options,
                connect,
                &mut Vec::new(),
            )
        }

        fn only(&self, part: Part) -> Result<PushReport> {
            self.run(
                &PushConfig::default(),
                Options {
                    only: Some(part),
                    dry_run: false,
                },
            )
        }

        fn ledger(&self) -> Ledger {
            Ledger::load(&self.paths.spotify_state()).unwrap()
        }

        fn create_mock(&self, name: &str, id: &str) -> httpmock::Mock<'_> {
            let name = name.to_owned();
            let id = id.to_owned();
            self.server.mock(move |when, then| {
                when.method(POST)
                    .path("/v1/me/playlists")
                    .json_body_includes(json!({ "name": name, "public": false }).to_string());
                then.status(201)
                    .json_body(json!({"id": id, "snapshot_id": "snap0"}));
            })
        }

        fn add_mock(&self, id: &str, uris: &[&str]) -> httpmock::Mock<'_> {
            let path = format!("/v1/playlists/{id}/items");
            let uris: Vec<String> = uris.iter().map(|u| (*u).to_owned()).collect();
            self.server.mock(move |when, then| {
                when.method(POST)
                    .path(path)
                    .json_body(json!({ "uris": uris }));
                then.status(201).json_body(json!({"snapshot_id": "snap1"}));
            })
        }

        fn items_mock(&self, id: &str, uris: &[&str]) -> httpmock::Mock<'_> {
            let path = format!("/v1/playlists/{id}/items");
            let items: Vec<serde_json::Value> =
                uris.iter().map(|u| json!({"item": {"uri": u}})).collect();
            self.server.mock(move |when, then| {
                when.method(GET).path(path);
                then.status(200)
                    .json_body(json!({"items": items, "next": null}));
            })
        }
    }

    fn panics() -> Connect<'static> {
        Box::new(|| panic!("a dry run makes no client"))
    }

    /// Every track, artist and album item id anywhere in `value`.
    fn ids_in(value: &serde_json::Value, found: &mut Vec<(IdKind, String)>) {
        match value {
            serde_json::Value::String(s) => {
                if let Some(kind) = IdKind::of_item_id(s) {
                    found.push((kind, s.clone()));
                }
            }
            serde_json::Value::Array(list) => list.iter().for_each(|v| ids_in(v, found)),
            serde_json::Value::Object(map) => map.values().for_each(|v| ids_in(v, found)),
            _ => {}
        }
    }

    fn yt_video(id: &str, kind: youtube::classify::VideoKind, plays: u32) -> youtube::model::Video {
        youtube::model::Video {
            video_id: id.to_owned(),
            title: Some(format!("Song {id}")),
            artists: vec!["The Band".to_owned()],
            album: None,
            duration_s: None,
            channel: None,
            kind,
            evidence: Vec::new(),
            title_source: youtube::model::TitleSource::History,
            youtube_title: None,
            plays,
            music_app_plays: plays,
            removed: false,
            first_played: None,
            last_played: (plays > 0).then(|| "2025-01-01T00:00:00.000Z".to_owned()),
            in_library: false,
            liked: false,
            available: None,
            category_id: None,
            topics: Vec::new(),
        }
    }

    fn yt_playlist(id: Option<&str>, title: &str, items: &[&str]) -> youtube::model::Playlist {
        youtube::model::Playlist {
            playlist_id: id.map(str::to_owned),
            title: title.to_owned(),
            created: None,
            updated: None,
            order: None,
            visibility: None,
            special: youtube::model::Special::of_title(title),
            items_source: youtube::takeout::ItemsSource::File,
            items: items
                .iter()
                .map(|v| youtube::model::PlaylistItem {
                    video_id: (*v).to_owned(),
                    added_at: None,
                })
                .collect(),
        }
    }

    const YP1: &str = "PLaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const YP2: &str = "PLbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// Songs `s1` (most played), `s2` and `lib` (library), a music video
    /// `mv` (liked), a `video` and an `unknown` played most of all; a pushed
    /// playlist holding every kind, an ambiguous one, a skipped one and
    /// Watch later; an artist channel and a plain one.
    fn youtube_export() -> youtube::model::Normalized {
        use youtube::classify::{ChannelKind, VideoKind};
        let videos = vec![
            yt_video("s1", VideoKind::Song, 9),
            yt_video("s2", VideoKind::Song, 2),
            yt_video("lib", VideoKind::Song, 0),
            yt_video("mv", VideoKind::MusicVideo, 1),
            yt_video("talk", VideoKind::Video, 20),
            yt_video("bare", VideoKind::Unknown, 15),
            yt_video("later", VideoKind::Song, 0),
        ];
        let mut ambiguous = yt_playlist(Some(YP2), "Mix", &["later"]);
        ambiguous.items_source = youtube::takeout::ItemsSource::AmbiguousTitle;
        let channel = |id: &str, kind| youtube::model::Channel {
            channel_id: id.to_owned(),
            name: format!("Channel {id}"),
            url: None,
            kind,
            evidence: Vec::new(),
            music_videos: 0,
        };
        youtube::model::Normalized {
            history: Vec::new(),
            videos: videos
                .into_iter()
                .map(|v| (v.video_id.clone(), v))
                .collect(),
            playlists: vec![
                yt_playlist(Some(YP1), "Road Trip", &["s2", "talk", "bare", "mv", "s2"]),
                ambiguous,
                yt_playlist(Some(YP1), "Watch later", &["later"]),
                yt_playlist(None, "Old file", &["later"]),
            ],
            subscriptions: vec![
                channel("UCartist", ChannelKind::Artist),
                channel("UCplain", ChannelKind::Channel),
            ],
            library: youtube::model::Library {
                songs: vec!["lib".into()],
                liked: vec!["mv".into(), "talk".into()],
                disliked: Vec::new(),
            },
        }
    }

    /// Push sends only what `match` decides: with every switch on, each part
    /// built from a match of every id a mixed export holds equals the part
    /// built from matches of the selected items alone.
    #[test]
    fn every_id_a_part_sends_is_in_the_match_selection() {
        let export = export();
        let yt = youtube_export();
        let exports = Exports {
            pandora: Some(export.clone()),
            youtube: Some(yt.clone()),
        };
        let config = Config {
            push: PushConfig {
                liked_songs: true,
                thumbs_down_playlist: true,
                linked_playlists: true,
                ..PushConfig::default()
            },
            youtube: crate::config::YoutubeConfig {
                most_played: 2,
                ..crate::config::YoutubeConfig::default()
            },
            ..Config::default()
        };
        let mut found = Vec::new();
        for value in [
            serde_json::to_value(&export.stations).unwrap(),
            serde_json::to_value(&export.playlists).unwrap(),
            serde_json::to_value(&export.collection).unwrap(),
            serde_json::to_value(&export.bookmarks).unwrap(),
        ] {
            ids_in(&value, &mut found);
        }
        let mut youtube_ids: Vec<String> = yt
            .videos
            .keys()
            .map(|id| youtube::select::video_item_id(id))
            .collect();
        youtube_ids.extend(
            yt.subscriptions
                .iter()
                .map(|c| youtube::select::channel_item_id(&c.channel_id)),
        );
        ids_in(&serde_json::to_value(&youtube_ids).unwrap(), &mut found);
        assert!(found.iter().any(|(_, id)| id.starts_with("YV:")));
        assert!(found.iter().any(|(_, id)| id.starts_with("YC:")));
        let mut every = Matches::default();
        for (kind, id) in found {
            let uri = format!("spotify:{kind}:{}", &id[3..]);
            every
                .of_mut(kind)
                .insert(id.clone(), entry(&id, Some(&uri)));
        }
        let items = crate::matching::run::select(&exports, &config);
        let mut selected = every.clone();
        for kind in IdKind::ALL {
            selected
                .of_mut(kind)
                .retain(|id, _| items.of(kind).contains_key(id));
        }

        let parts = |m: &Matches| {
            let playlists: Vec<Vec<Wanted>> = Part::ALL
                .into_iter()
                .map(|part| wanted_playlists(part, &exports, m, &config))
                .collect();
            let libraries: Vec<Vec<String>> = [Part::Artists, Part::Collection, Part::Liked]
                .into_iter()
                .map(|part| library_uris(part, &exports, m, &config))
                .collect();
            (playlists, libraries)
        };
        let all = parts(&every);
        assert!(all.0.iter().filter(|p| !p.is_empty()).count() >= 4);
        assert!(all.1.iter().all(|uris| !uris.is_empty()));
        let keys: Vec<&str> = all.0.iter().flatten().map(|w| w.key.as_str()).collect();
        assert!(keys.contains(&youtube::select::MOST_PLAYED_KEY), "{keys:?}");
        assert!(keys.contains(&format!("YP:{YP1}").as_str()), "{keys:?}");
        assert_eq!(parts(&selected), all);
    }

    #[test]
    fn wanted_playlists_are_the_playlist_parts_under_their_switches() {
        let export = export();
        let mut matches = Matches::default();
        for id in ["TR:s", "TR:a", "TR:b", "TR:c", "TR:d"] {
            let uri = format!("spotify:track:{}", &id[3..]);
            matches.tracks.insert(id.into(), entry(id, Some(&uri)));
        }
        for thumbs_down_playlist in [false, true] {
            let config = PushConfig {
                thumbs_down_playlist,
                ..PushConfig::default()
            };
            let exports = pandora_only(export.clone());
            let full = with_push(&config);
            let of = |part| wanted_playlists(part, &exports, &matches, &full);
            assert_eq!(
                of(Part::Stations),
                station_playlists(&export, &matches, &config)
            );
            assert_eq!(
                of(Part::Playlists),
                pandora_playlists(&export, &matches, &config)
            );
            let down: Vec<(String, Vec<String>)> = of(Part::ThumbsDown)
                .into_iter()
                .map(|w| (w.key, w.uris))
                .collect();
            if thumbs_down_playlist {
                assert_eq!(
                    down,
                    [("thumbs-down".to_owned(), vec!["spotify:track:d".to_owned()])]
                );
            } else {
                assert!(down.is_empty(), "{down:?}");
            }
            assert!(!of(Part::Stations).is_empty() && !of(Part::Playlists).is_empty());
            for library in [Part::Artists, Part::Collection, Part::Liked] {
                assert!(of(library).is_empty());
            }
        }
    }

    #[test]
    fn the_plan_covers_every_part_and_a_dry_run_sends_nothing() {
        let f = fixture();
        let config = PushConfig {
            liked_songs: true,
            thumbs_down_playlist: true,
            playlist_prefix: "P: ".into(),
            ..PushConfig::default()
        };
        let report = push(
            "alice",
            &f.paths,
            &pandora_only(export()),
            &with_push(&config),
            Options {
                only: None,
                dry_run: true,
            },
            panics(),
            &mut Vec::new(),
        )
        .unwrap();
        assert!(!f.paths.spotify_state().exists());
        let p = &report.parts;
        assert_eq!(
            p.artists,
            Some(FollowsPart {
                to_follow: 2,
                followed: 0,
                skipped: 0
            })
        );
        let stations = &p.stations.as_ref().unwrap().playlists;
        assert_eq!(
            stations.len(),
            1,
            "neither the Thumbprint nor the shuffle station is a playlist"
        );
        assert_eq!(
            (stations[0].key.as_str(), stations[0].name.as_str()),
            ("ST:9", "P: Morning")
        );
        assert_eq!(
            (stations[0].action, stations[0].to_add),
            (Action::Create, 3)
        );
        let lists = &p.playlists.as_ref().unwrap().playlists;
        assert_eq!(lists.len(), 1, "no Pandora-made playlist is pushed");
        assert_eq!((lists[0].key.as_str(), lists[0].to_add), ("PL:1", 2));
        assert_eq!(lists[0].repeats, 1);
        // tracks c, k; albums 1, 2; artist 2.
        assert_eq!(p.collection.as_ref().unwrap().to_save, 5);
        // a, b and the Thumbprint station's p; x is unmatched.
        assert_eq!(p.liked.as_ref().unwrap().to_save, 3);
        let down = &p.thumbs_down.as_ref().unwrap().playlists;
        assert_eq!((down[0].key.as_str(), down[0].to_add), (THUMBS_DOWN_KEY, 1));

        let mut text = Vec::new();
        report.write_text(&mut text).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.starts_with("dry run: nothing changed for profile alice\n"));
        assert!(
            text.contains("  create \"P: Morning\" (ST:9): would add 3 of 3\n"),
            "{text}"
        );
        assert!(text.contains("1 repeated tracks once"), "{text}");
    }

    #[test]
    fn switched_off_parts_do_nothing() {
        let f = fixture();
        let off = PushConfig {
            liked_songs: false,
            ..PushConfig::default()
        };
        let liked = Options {
            only: Some(Part::Liked),
            dry_run: false,
        };
        let report = f.run(&off, liked).unwrap();
        assert_eq!(report.parts.liked, Some(LibraryPart::default()));
        assert!(report.parts.stations.is_none());
        let report = f.only(Part::ThumbsDown).unwrap();
        let down = report.parts.thumbs_down.unwrap();
        assert!(!down.enabled && down.playlists.is_empty());
        let mut text = Vec::new();
        f.run(&off, liked).unwrap().write_text(&mut text).unwrap();
        assert!(
            String::from_utf8(text)
                .unwrap()
                .contains("liked songs: off in config.toml [push]")
        );
        assert!(!f.paths.spotify_state().exists());
    }

    #[test]
    fn station_playlist_order_name_and_description() {
        let wanted = station_playlists(
            &export(),
            &model::read_all_latest(&fixture().paths).unwrap(),
            &PushConfig::default(),
        );
        let w = &wanted[0];
        assert_eq!(
            w.uris,
            ["spotify:track:s", "spotify:track:a", "spotify:track:b"],
            "seeds, then thumbs-up by date"
        );
        assert_eq!(
            w.description,
            "Pandora station \"Morning\": 2 of 3 thumbs-up matched; seeds: The Band, Seed Song, \
             Jazz. sss:ST:9",
            "the marker is the ledger key, not the station id 41"
        );

        let mut long = export();
        long.stations[0].name = Some("n".repeat(400));
        long.stations[0].pandora_id = None;
        let w = &station_playlists(
            &long,
            &model::read_all_latest(&fixture().paths).unwrap(),
            &PushConfig::default(),
        )[0];
        assert_eq!(w.key, "station:41");
        assert_eq!(w.name.chars().count(), NAME_MAX);
        assert_eq!(w.description.chars().count(), DESCRIPTION_MAX);
        assert!(
            w.description.ends_with("n sss:station:41"),
            "{}",
            w.description
        );
    }

    #[test]
    fn a_description_is_cut_before_the_marker() {
        assert_eq!(with_marker("", "PL:1"), "sss:PL:1");
        assert_eq!(with_marker(" hi ", "PL:1"), "hi sss:PL:1");
        let d = with_marker(&"é".repeat(400), "PL:1");
        assert_eq!(d.chars().count(), DESCRIPTION_MAX);
        assert!(d.ends_with("é sss:PL:1"));
    }

    #[test]
    fn a_new_playlist_is_recorded_pending_then_created_and_filled_in_hundreds() {
        let f = fixture();
        let mut ex = export();
        ex.stations[0].feedback = (0..150)
            .map(|n| thumb(&format!("TR:n{n:03}"), true, None))
            .collect();
        ex.stations[0].seeds.clear();
        let uris: Vec<String> = (0..150).map(|n| format!("spotify:track:n{n:03}")).collect();
        for (n, uri) in uris.iter().enumerate() {
            model::append(
                &f.paths.track_matches(),
                &entry(&format!("TR:n{n:03}"), Some(uri)),
            )
            .unwrap();
        }
        let create = f.create_mock("Morning", "pl9");
        let refs: Vec<&str> = uris.iter().map(String::as_str).collect();
        let first = f.add_mock("pl9", &refs[..100]);
        let second = f.add_mock("pl9", &refs[100..]);
        let state = f.state.clone();
        let (api, auth) = (f.server.url("/v1"), f.server.base_url());
        let report = push(
            "alice",
            &f.paths,
            &pandora_only(ex.clone()),
            &Config::default(),
            Options {
                only: Some(Part::Stations),
                dry_run: false,
            },
            Box::new(move || Spotify::new(state, "test-client-id", "", &api, &auth, FAST)),
            &mut Vec::new(),
        )
        .unwrap();
        create.assert();
        first.assert();
        second.assert();
        let pl = &report.parts.stations.unwrap().playlists[0];
        assert_eq!((pl.action, pl.to_add, pl.added), (Action::Create, 150, 150));
        assert_eq!(pl.spotify_id.as_deref(), Some("pl9"));
        let Some(PlaylistEntry::Created {
            spotify_id,
            snapshot_id,
            name,
            items,
            at,
        }) = f.ledger().playlists.get("ST:9").cloned()
        else {
            panic!("no created entry")
        };
        assert_eq!(spotify_id, "pl9");
        assert_eq!(snapshot_id.as_deref(), Some("snap1"));
        assert_eq!(name.as_deref(), Some("Morning"));
        assert_eq!(items, uris);
        assert!(at.is_some());
    }

    #[test]
    fn a_second_push_writes_nothing() {
        let f = fixture();
        let create = f.create_mock("Morning", "pl9");
        let add = f.add_mock(
            "pl9",
            &["spotify:track:s", "spotify:track:a", "spotify:track:b"],
        );
        let contains = f.server.mock(|when, then| {
            when.method(GET).path("/v1/me/library/contains");
            then.status(200).json_body(json!([false, true]));
        });
        let save = f.server.mock(|when, then| {
            when.method(PUT)
                .path("/v1/me/library")
                .query_param("uris", "spotify:artist:1");
            then.status(200).body("");
        });
        for part in [Part::Artists, Part::Stations] {
            f.only(part).unwrap();
        }
        create.assert();
        add.assert();
        contains.assert();
        save.assert();
        let ledger = f.ledger();
        assert_eq!(
            ledger.library.keys().collect::<Vec<_>>(),
            ["spotify:artist:1", "spotify:artist:2"],
            "a follow the member had is recorded too"
        );

        let items = f.items_mock(
            "pl9",
            &["spotify:track:s", "spotify:track:a", "spotify:track:b"],
        );
        let a = f.only(Part::Artists).unwrap().parts.artists.unwrap();
        assert_eq!((a.to_follow, a.followed, a.skipped), (0, 0, 2));
        let s = f.only(Part::Stations).unwrap().parts.stations.unwrap();
        assert_eq!(s.playlists[0].action, Action::Skip);
        items.assert();
        create.assert_calls(1);
        add.assert_calls(1);
        contains.assert_calls(1);
        save.assert_calls(1);
    }

    #[test]
    fn only_artists_sends_only_the_follows() {
        let f = fixture();
        let contains = f.server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/library/contains")
                .query_param("uris", "spotify:artist:1,spotify:artist:2");
            then.status(200).json_body(json!([false, false]));
        });
        let save = f.server.mock(|when, then| {
            when.method(PUT)
                .path("/v1/me/library")
                .query_param("uris", "spotify:artist:1,spotify:artist:2");
            then.status(200).body("");
        });
        let other = f.server.mock(|when, then| {
            when.method(POST);
            then.status(500);
        });
        let report = f.only(Part::Artists).unwrap();
        contains.assert();
        save.assert();
        other.assert_calls(0);
        assert_eq!(
            report.parts,
            Parts {
                artists: Some(FollowsPart {
                    to_follow: 2,
                    followed: 2,
                    skipped: 0
                }),
                ..Parts::default()
            }
        );
    }

    #[test]
    fn the_collection_is_saved_in_forties_after_a_contains_check() {
        let f = fixture();
        let mut ex = export();
        ex.collection.tracks = (0..50).map(|n| format!("TR:c{n:02}")).collect();
        ex.collection.albums.clear();
        ex.collection.artists.clear();
        ex.bookmarks = Default::default();
        let uris: Vec<String> = (0..50).map(|n| format!("spotify:track:c{n:02}")).collect();
        for (n, uri) in uris.iter().enumerate() {
            model::append(
                &f.paths.track_matches(),
                &entry(&format!("TR:c{n:02}"), Some(uri)),
            )
            .unwrap();
        }
        let checks: Vec<_> = [&uris[..40], &uris[40..]]
            .into_iter()
            .map(|chunk| {
                let joined = chunk.join(",");
                let answer = vec![false; chunk.len()];
                f.server.mock(move |when, then| {
                    when.method(GET)
                        .path("/v1/me/library/contains")
                        .query_param("uris", joined);
                    then.status(200).json_body(json!(answer));
                })
            })
            .collect();
        let saves: Vec<_> = [&uris[..40], &uris[40..]]
            .into_iter()
            .map(|chunk| {
                let joined = chunk.join(",");
                f.server.mock(move |when, then| {
                    when.method(PUT)
                        .path("/v1/me/library")
                        .query_param("uris", joined);
                    then.status(200).body("");
                })
            })
            .collect();
        let state = f.state.clone();
        let (api, auth) = (f.server.url("/v1"), f.server.base_url());
        let report = push(
            "alice",
            &f.paths,
            &pandora_only(ex.clone()),
            &Config::default(),
            Options {
                only: Some(Part::Collection),
                dry_run: false,
            },
            Box::new(move || Spotify::new(state, "test-client-id", "", &api, &auth, FAST)),
            &mut Vec::new(),
        )
        .unwrap();
        for m in checks.iter().chain(&saves) {
            m.assert();
        }
        assert_eq!(report.parts.collection.unwrap().saved, 50);
        assert_eq!(f.ledger().library.len(), 50);
    }

    #[test]
    fn a_pending_entry_is_left_alone_and_named_for_the_owner() {
        let f = fixture();
        let mut ledger = Ledger::default();
        ledger.playlists.insert(
            "ST:9".into(),
            PlaylistEntry::Pending {
                name: "Morning".into(),
                at: "2026-10-15T01:02:03Z".into(),
            },
        );
        ledger.save(&f.paths.spotify_state()).unwrap();
        let morning = f.create_mock("Morning", "never");
        let list = f.create_mock("List PL:1", "pl1");
        let add = f.add_mock("pl1", &["spotify:track:a", "spotify:track:c"]);
        let report = f.only(Part::Stations).unwrap();
        let lists = f.only(Part::Playlists).unwrap().parts.playlists.unwrap();
        assert_eq!(lists.playlists[0].action, Action::Create);
        morning.assert_calls(0);
        list.assert();
        add.assert();
        let st = &report.parts.stations.as_ref().unwrap().playlists[0];
        assert_eq!(st.action, Action::Pending);
        assert_eq!(st.at.as_deref(), Some("2026-10-15T01:02:03Z"));
        let doc = serde_json::to_value(&report).unwrap();
        assert_eq!(
            doc["parts"]["stations"]["playlists"][0],
            json!({"key": "ST:9", "name": "Morning", "action": "pending", "to_add": 3,
                   "added": 0, "at": "2026-10-15T01:02:03Z"})
        );
        let instructions = report.pending_instructions();
        assert_eq!(instructions.len(), 1);
        let state_file = f.paths.spotify_state().display().to_string();
        for needle in [
            "`ST:9`",
            "\"Morning\"",
            "2026-10-15T01:02:03Z",
            state_file.as_str(),
        ] {
            assert!(
                instructions[0].contains(needle),
                "{needle}: {}",
                instructions[0]
            );
        }
        let mut text = Vec::new();
        report.write_text(&mut text).unwrap();
        assert!(String::from_utf8(text).unwrap().contains(&instructions[0]));
        assert!(matches!(
            f.ledger().playlists.get("ST:9"),
            Some(PlaylistEntry::Pending { .. })
        ));
    }

    #[test]
    fn a_hand_adopted_entry_is_paged_and_filled_in() {
        let f = fixture();
        std::fs::create_dir_all(f.paths.spotify_state().parent().unwrap()).unwrap();
        std::fs::write(
            f.paths.spotify_state(),
            r#"{"playlists": {"ST:9": {"status":"created","spotify_id":"mine"}}}"#,
        )
        .unwrap();
        let items = f.items_mock("mine", &["spotify:track:a", "spotify:track:zz"]);
        let add = f.add_mock("mine", &["spotify:track:s", "spotify:track:b"]);
        let report = f.only(Part::Stations).unwrap();
        items.assert();
        add.assert();
        let pl = &report.parts.stations.unwrap().playlists[0];
        assert_eq!((pl.action, pl.to_add, pl.added), (Action::Update, 2, 2));
        let Some(PlaylistEntry::Created { items, .. }) = f.ledger().playlists.get("ST:9").cloned()
        else {
            panic!("not created")
        };
        assert_eq!(items, ["spotify:track:s", "spotify:track:b"]);
    }

    #[test]
    fn a_playlist_id_spotify_does_not_know_is_made_again() {
        let f = fixture();
        let mut ledger = Ledger::default();
        ledger.playlists.insert(
            "ST:9".into(),
            PlaylistEntry::Created {
                spotify_id: "gone".into(),
                snapshot_id: None,
                name: Some("Morning".into()),
                items: vec!["spotify:track:s".into()],
                at: None,
            },
        );
        ledger.save(&f.paths.spotify_state()).unwrap();
        let gone = f.server.mock(|when, then| {
            when.method(GET).path("/v1/playlists/gone/items");
            then.status(404)
                .json_body(json!({"error": {"status": 404, "message": "Not found."}}));
        });
        let create = f.create_mock("Morning", "new");
        let add = f.add_mock(
            "new",
            &["spotify:track:s", "spotify:track:a", "spotify:track:b"],
        );
        let pl = f
            .only(Part::Stations)
            .unwrap()
            .parts
            .stations
            .unwrap()
            .playlists[0]
            .clone();
        gone.assert();
        create.assert();
        add.assert();
        assert_eq!((pl.action, pl.added), (Action::Create, 3));
        assert!(matches!(
            f.ledger().playlists.get("ST:9"),
            Some(PlaylistEntry::Created { spotify_id, .. }) if spotify_id == "new"
        ));
    }

    #[test]
    fn a_quota_stop_mid_playlist_leaves_the_ledger_with_what_landed() {
        let f = fixture();
        let mut ex = export();
        ex.stations[0].seeds.clear();
        ex.stations[0].feedback = (0..120)
            .map(|n| thumb(&format!("TR:q{n:03}"), true, None))
            .collect();
        let uris: Vec<String> = (0..120).map(|n| format!("spotify:track:q{n:03}")).collect();
        for (n, uri) in uris.iter().enumerate() {
            model::append(
                &f.paths.track_matches(),
                &entry(&format!("TR:q{n:03}"), Some(uri)),
            )
            .unwrap();
        }
        f.create_mock("Morning", "pl9");
        let refs: Vec<&str> = uris.iter().map(String::as_str).collect();
        f.add_mock("pl9", &refs[..100]);
        let rest = refs[100..].to_vec();
        f.server.mock(move |when, then| {
            when.method(POST)
                .path("/v1/playlists/pl9/items")
                .json_body(json!({ "uris": rest }));
            then.status(429).header("Retry-After", "3600").body("");
        });
        let state = f.state.clone();
        let (api, auth) = (f.server.url("/v1"), f.server.base_url());
        let err = push(
            "alice",
            &f.paths,
            &pandora_only(ex.clone()),
            &Config::default(),
            Options {
                only: Some(Part::Stations),
                dry_run: false,
            },
            Box::new(move || Spotify::new(state, "test-client-id", "", &api, &auth, FAST)),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Quota);
        let Some(PlaylistEntry::Created { items, .. }) = f.ledger().playlists.get("ST:9").cloned()
        else {
            panic!("not created")
        };
        assert_eq!(items, uris[..100]);
    }

    #[test]
    fn a_create_that_may_have_landed_leaves_the_entry_pending() {
        for (status, body, kind) in [(502, "", Kind::Unavailable), (201, "not json", Kind::Other)] {
            let f = fixture();
            f.server.mock(|when, then| {
                when.method(POST).path("/v1/me/playlists");
                then.status(status).body(body);
            });
            let err = f.only(Part::Stations).unwrap_err();
            assert_eq!(error::kind(&err), kind, "{status}");
            assert!(matches!(
                f.ledger().playlists.get("ST:9"),
                Some(PlaylistEntry::Pending { name, .. }) if name == "Morning"
            ));
        }
    }

    #[test]
    fn a_create_spotify_refused_leaves_no_entry_and_is_tried_again() {
        let f = fixture();
        for (status, retry_after, kind) in [
            (429, "3600", Kind::Quota),
            (403, "", Kind::Config),
            (400, "", Kind::Other),
        ] {
            let mut refused = f.server.mock(|when, then| {
                when.method(POST).path("/v1/me/playlists");
                then.status(status)
                    .header("Retry-After", retry_after)
                    .body("");
            });
            let err = f.only(Part::Stations).unwrap_err();
            assert_eq!(error::kind(&err), kind, "{status}");
            assert!(f.ledger().playlists.is_empty(), "{status}");
            refused.delete();
        }
        let create = f.create_mock("Morning", "pl9");
        let add = f.add_mock(
            "pl9",
            &["spotify:track:s", "spotify:track:a", "spotify:track:b"],
        );
        f.only(Part::Stations).unwrap();
        create.assert();
        add.assert();
    }

    #[test]
    fn a_client_that_cannot_be_made_writes_no_ledger() {
        let f = fixture();
        let err = push(
            "alice",
            &f.paths,
            &pandora_only(export()),
            &Config::default(),
            Options {
                only: Some(Part::Stations),
                dry_run: false,
            },
            Box::new(|| Err(anyhow::anyhow!("no client id"))),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "no client id");
        assert!(!f.paths.spotify_state().exists());
    }

    #[test]
    fn a_refused_create_after_a_404_puts_the_prior_entry_back() {
        let f = fixture();
        let prior = PlaylistEntry::Created {
            spotify_id: "gone".into(),
            snapshot_id: None,
            name: Some("Morning".into()),
            items: vec!["spotify:track:s".into()],
            at: None,
        };
        let mut ledger = Ledger::default();
        ledger.playlists.insert("ST:9".into(), prior.clone());
        ledger.save(&f.paths.spotify_state()).unwrap();
        f.server.mock(|when, then| {
            when.method(GET).path("/v1/playlists/gone/items");
            then.status(404).body("");
        });
        f.server.mock(|when, then| {
            when.method(POST).path("/v1/me/playlists");
            then.status(403).body("");
        });
        let err = f.only(Part::Stations).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Config);
        assert_eq!(f.ledger().playlists.get("ST:9"), Some(&prior));
    }

    #[test]
    fn public_playlists_makes_the_created_playlists_public() {
        let f = fixture_with(&[PLAYLIST_MODIFY_PUBLIC_SCOPE]);
        let create = f.server.mock(|when, then| {
            when.method(POST)
                .path("/v1/me/playlists")
                .json_body_includes(json!({"name": "Morning", "public": true}).to_string());
            then.status(201)
                .json_body(json!({"id": "pl9", "snapshot_id": "snap0"}));
        });
        let add = f.add_mock(
            "pl9",
            &["spotify:track:s", "spotify:track:a", "spotify:track:b"],
        );
        let config = PushConfig {
            public_playlists: true,
            ..PushConfig::default()
        };
        f.run(
            &config,
            Options {
                only: Some(Part::Stations),
                dry_run: false,
            },
        )
        .unwrap();
        create.assert();
        add.assert();
    }

    #[test]
    fn a_public_playlist_needs_a_token_granted_its_scope() {
        let f = fixture_with(&["playlist-modify-private"]);
        let create = f.server.mock(|when, then| {
            when.method(POST).path("/v1/me/playlists");
            then.status(201)
                .json_body(json!({"id": "pl9", "snapshot_id": "snap0"}));
        });
        let config = PushConfig {
            public_playlists: true,
            ..PushConfig::default()
        };
        let err = f
            .run(
                &config,
                Options {
                    only: Some(Part::Stations),
                    dry_run: false,
                },
            )
            .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Auth);
        let msg = format!("{err:#}");
        assert!(msg.contains("without playlist-modify-public"), "{msg}");
        assert!(msg.contains("sss spotify auth --profile alice"), "{msg}");
        create.assert_calls(0);
        assert!(f.ledger().playlists.is_empty());

        let dry = f
            .run(
                &config,
                Options {
                    only: Some(Part::Stations),
                    dry_run: true,
                },
            )
            .unwrap();
        assert!(dry.dry_run);
        create.assert_calls(0);
    }

    #[test]
    fn liked_songs_saves_every_thumbs_up_thumbprint_included() {
        let f = fixture();
        let contains = f.server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/library/contains")
                .query_param("uris", "spotify:track:b,spotify:track:a,spotify:track:p");
            then.status(200).json_body(json!([false, true, false]));
        });
        let save = f.server.mock(|when, then| {
            when.method(PUT)
                .path("/v1/me/library")
                .query_param("uris", "spotify:track:b,spotify:track:p");
            then.status(200).body("");
        });
        let config = PushConfig {
            liked_songs: true,
            ..PushConfig::default()
        };
        let report = f
            .run(
                &config,
                Options {
                    only: Some(Part::Liked),
                    dry_run: false,
                },
            )
            .unwrap();
        contains.assert();
        save.assert();
        assert_eq!(
            report.parts.liked,
            Some(LibraryPart {
                enabled: true,
                to_save: 3,
                saved: 2,
                skipped: 1
            })
        );
        assert_eq!(
            f.ledger().library.keys().collect::<Vec<_>>(),
            ["spotify:track:a", "spotify:track:b", "spotify:track:p"]
        );
    }

    #[test]
    fn linked_playlists_pushes_every_kind_but_the_thumbs_ones() {
        for (kind, off, on) in [
            (None, true, true),
            (Some("Personalized"), false, true),
            (Some("StationThumbs"), false, false),
            (Some("MyThumbsUp"), false, false),
        ] {
            let list = playlist("PL:9", kind, &[]);
            for (linked, want) in [(false, off), (true, on)] {
                let config = PushConfig {
                    linked_playlists: linked,
                    ..PushConfig::default()
                };
                assert_eq!(
                    playlist_is_pushed(&list, &config),
                    want,
                    "{kind:?}, linked_playlists = {linked}"
                );
            }
        }
        let config = PushConfig {
            linked_playlists: true,
            ..PushConfig::default()
        };
        let matches = model::read_all_latest(&fixture().paths).unwrap();
        let keys: Vec<String> = pandora_playlists(&export(), &matches, &config)
            .into_iter()
            .map(|w| w.key)
            .collect();
        assert_eq!(keys, ["PL:1", "PL:2"]);
    }

    #[test]
    fn the_ledger_file_shape() {
        let mut ledger = Ledger::default();
        ledger.playlists.insert(
            "PL:4".into(),
            PlaylistEntry::Pending {
                name: "n".into(),
                at: "t".into(),
            },
        );
        ledger.playlists.insert(
            "ST:1".into(),
            PlaylistEntry::Created {
                spotify_id: "i".into(),
                snapshot_id: Some("s".into()),
                name: Some("n".into()),
                items: vec!["spotify:track:a".into()],
                at: Some("t".into()),
            },
        );
        ledger
            .library
            .insert("spotify:artist:1".into(), LibraryEntry { at: "t".into() });
        assert_eq!(
            serde_json::to_value(&ledger).unwrap(),
            json!({
                "playlists": {
                    "PL:4": {"status": "pending", "name": "n", "at": "t"},
                    "ST:1": {"status": "created", "spotify_id": "i", "snapshot_id": "s",
                             "name": "n", "items": ["spotify:track:a"], "at": "t"}
                },
                "library": {"spotify:artist:1": {"at": "t"}}
            })
        );
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            Ledger::load(&tmp.path().join("none.json")).unwrap(),
            Ledger::default()
        );
    }
}
