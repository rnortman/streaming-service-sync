//! Normalized Pandora data and the pass that derives it from a raw run.
//!
//! The pass reads only the stored exchanges of one run, in capture order,
//! and only the successful ones; it never touches the network. Unknown
//! fields are ignored and a missing field becomes `None`, so a response
//! shape the parser does not expect costs information, never the run.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::pandora::client::{self, endpoint};
use crate::pandora::export;
use crate::repo::{self, ProfilePaths};
use crate::spotify::ids::item_id_type as id_type;

/// One recording, the unit of matching. Filled from every source record
/// that names it: feedback records, track seeds and catalog annotations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    /// `TR:…`
    pub pandora_id: String,
    pub music_id: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_s: Option<u32>,
    pub track_num: Option<u32>,
    pub disc_num: Option<u32>,
    /// From an annotation's explicitness when one carries it, else whether
    /// the album title contains "(Explicit)".
    pub explicit_hint: bool,
    pub isrc: Option<String>,
    pub artist_pandora_id: Option<String>,
    pub album_pandora_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeedKind {
    Artist,
    Track,
    Genre,
    /// A seed of a kind the parser does not recognise; the raw run has it.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seed {
    pub kind: SeedKind,
    pub pandora_id: Option<String>,
    pub name: Option<String>,
}

/// One thumb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feedback {
    pub feedback_id: String,
    pub track_pandora_id: Option<String>,
    pub positive: bool,
    /// `feedbackDateCreated` as Pandora gave it.
    pub created: Option<String>,
    pub station_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Station {
    pub station_id: String,
    /// `ST:…`
    pub pandora_id: Option<String>,
    pub name: Option<String>,
    pub created: Option<String>,
    pub last_played: Option<String>,
    pub is_thumbprint: bool,
    pub is_shuffle: bool,
    pub genres: Vec<String>,
    /// The thumb counts the station's details report, to compare with the
    /// feedback captured.
    pub positive_feedback_count: Option<u64>,
    pub negative_feedback_count: Option<u64>,
    pub seeds: Vec<Seed>,
    /// Thumbs up, then thumbs down, each in capture order.
    pub feedback: Vec<Feedback>,
}

impl Station {
    /// The thumbs-up in station-playlist order: dated entries by date, then
    /// undated ones in capture order.
    pub fn thumbs_up_in_order(&self) -> Vec<&Feedback> {
        let up = self.feedback.iter().filter(|f| f.positive);
        let (mut dated, undated): (Vec<_>, Vec<_>) = up
            .map(|f| (f.created.as_deref().and_then(parse_time), f))
            .partition(|(t, _)| t.is_some());
        dated.sort_by_key(|(t, _)| *t);
        dated.into_iter().chain(undated).map(|(_, f)| f).collect()
    }
}

fn parse_time(text: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    /// `PL:…`
    pub pandora_id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub created: Option<String>,
    /// Pandora's `linkedType` (`StationThumbs`, `Personalized`, …); absent
    /// for a playlist the member made, which Pandora sends as `"None"`.
    pub linked_type: Option<String>,
    /// Track ids in playlist order.
    pub items: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Album {
    /// `AL:…`
    pub pandora_id: String,
    pub name: Option<String>,
    pub artist: Option<String>,
    pub artist_pandora_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artist {
    /// `AR:…`
    pub pandora_id: String,
    pub name: Option<String>,
}

/// The on-demand collection, in Pandora's listing order. Playlists in the
/// collection are in `playlists.json`, not here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Collection {
    pub tracks: Vec<String>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub pandora_id: Option<String>,
    /// The song title of a track bookmark, the album title of an album one.
    pub title: Option<String>,
    pub artist: Option<String>,
    pub created: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmarks {
    pub tracks: Vec<Bookmark>,
    pub albums: Vec<Bookmark>,
}

/// Everything derived from one raw run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Normalized {
    pub stations: Vec<Station>,
    pub playlists: Vec<Playlist>,
    pub collection: Collection,
    pub bookmarks: Bookmarks,
    /// Keyed by `pandora_id`.
    pub tracks: BTreeMap<String, Track>,
}

/// A raw file as `export` writes it.
#[derive(Debug, Deserialize)]
struct StoredExchange {
    path: String,
    #[serde(default)]
    request: Value,
    status: u16,
    response: Value,
}

fn text(record: &Value, key: &str) -> Option<String> {
    match record.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// An id that Pandora gives as a string or a number, as text.
fn id_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn number(record: &Value, key: &str) -> Option<u64> {
    match record.get(key)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn small_number(record: &Value, key: &str) -> Option<u32> {
    number(record, key).and_then(|n| u32::try_from(n).ok())
}

fn flag(record: &Value, key: &str) -> bool {
    record.get(key).and_then(Value::as_bool) == Some(true)
}

/// A timestamp field: a string as given, or epoch seconds / milliseconds as
/// RFC 3339 UTC.
fn time_text(record: &Value, key: &str) -> Option<String> {
    match record.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => {
            let n = n.as_i64()?;
            let nanos = if n.abs() >= 100_000_000_000 {
                i128::from(n) * 1_000_000
            } else {
                i128::from(n) * 1_000_000_000
            };
            time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
                .ok()?
                .format(&time::format_description::well_known::Rfc3339)
                .ok()
        }
        _ => None,
    }
}

fn fill<T>(slot: &mut Option<T>, value: Option<T>) {
    if slot.is_none() {
        *slot = value;
    }
}

/// A track being assembled from its sources.
#[derive(Debug, Default)]
struct TrackDraft {
    track: Track,
    explicit_annotation: Option<bool>,
    duration_feedback: Option<u32>,
    duration_annotation: Option<u32>,
}

/// The track catalogue: one draft per `TR:` id.
#[derive(Debug, Default)]
struct Catalog {
    drafts: BTreeMap<String, TrackDraft>,
}

impl Catalog {
    fn entry(&mut self, id: &str) -> &mut TrackDraft {
        self.drafts
            .entry(id.to_owned())
            .or_insert_with(|| TrackDraft {
                track: Track {
                    pandora_id: id.to_owned(),
                    ..Track::default()
                },
                ..TrackDraft::default()
            })
    }

    /// A `getStationFeedback` record.
    fn feedback(&mut self, record: &Value) {
        let Some(id) = text(record, "pandoraId") else {
            return;
        };
        let d = self.entry(&id);
        let t = &mut d.track;
        fill(&mut t.music_id, text(record, "musicId"));
        fill(&mut t.title, text(record, "songTitle"));
        fill(&mut t.artist, text(record, "artistName"));
        fill(&mut t.album, text(record, "albumTitle"));
        fill(&mut t.track_num, small_number(record, "trackNum"));
        fill(&mut t.disc_num, small_number(record, "discNum"));
        fill(
            &mut d.duration_feedback,
            small_number(record, "trackLength"),
        );
    }

    /// The `song` object of a track seed.
    fn seed_song(&mut self, id: &str, song: &Value) {
        let t = &mut self.entry(id).track;
        fill(&mut t.music_id, text(song, "musicId"));
        fill(&mut t.title, text(song, "songTitle"));
        fill(
            &mut t.artist,
            text(song, "artistSummary").or_else(|| text(song, "artistName")),
        );
    }

    /// A catalog annotation of type `TR`.
    fn annotation(&mut self, id: &str, a: &Value) {
        let d = self.entry(id);
        let t = &mut d.track;
        fill(&mut t.title, text(a, "name"));
        fill(&mut t.artist, text(a, "artistName"));
        fill(&mut t.album, text(a, "albumName"));
        fill(&mut t.track_num, small_number(a, "trackNumber"));
        fill(&mut t.artist_pandora_id, text(a, "artistId"));
        fill(&mut t.album_pandora_id, text(a, "albumId"));
        fill(&mut t.isrc, text(a, "isrc"));
        fill(&mut d.duration_annotation, small_number(a, "duration"));
        fill(&mut d.explicit_annotation, explicitness(a));
    }

    fn finish(self) -> BTreeMap<String, Track> {
        self.drafts
            .into_iter()
            .map(|(id, d)| {
                let mut t = d.track;
                t.duration_s = d.duration_feedback.or(d.duration_annotation);
                t.explicit_hint = d.explicit_annotation.unwrap_or_else(|| {
                    t.album
                        .as_deref()
                        .is_some_and(|a| a.to_lowercase().contains("(explicit)"))
                });
                (id, t)
            })
            .collect()
    }
}

/// An annotation's explicitness: `"EXPLICIT"` (any case) or `true` is
/// explicit, any other string or `false` is not.
fn explicitness(a: &Value) -> Option<bool> {
    match a.get("explicitness")? {
        Value::String(s) => Some(s.eq_ignore_ascii_case("explicit")),
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

/// The annotations a response carries, keyed by Pandora id: its
/// `annotations` object, or, for `annotateObjects`, the body itself.
fn annotations(body: &Value) -> Vec<(&str, &Value)> {
    let map: Option<&Map<String, Value>> = match body.get("annotations") {
        Some(Value::Object(m)) => Some(m),
        _ => body.as_object(),
    };
    map.into_iter()
        .flatten()
        .filter(|(k, v)| v.is_object() && id_type(k).is_some())
        .map(|(k, v)| (k.as_str(), v))
        .collect()
}

/// A station as the run assembles it.
#[derive(Debug)]
struct StationDraft {
    station: Station,
    up: Vec<Feedback>,
    down: Vec<Feedback>,
    seen: HashSet<(bool, String)>,
}

impl StationDraft {
    fn new(station_id: String) -> Self {
        Self {
            station: Station {
                station_id,
                pandora_id: None,
                name: None,
                created: None,
                last_played: None,
                is_thumbprint: false,
                is_shuffle: false,
                genres: Vec::new(),
                positive_feedback_count: None,
                negative_feedback_count: None,
                seeds: Vec::new(),
                feedback: Vec::new(),
            },
            up: Vec::new(),
            down: Vec::new(),
            seen: HashSet::new(),
        }
    }
}

#[derive(Debug, Default)]
struct Builder {
    catalog: Catalog,
    station_order: Vec<String>,
    stations: HashMap<String, StationDraft>,
    playlist_order: Vec<String>,
    playlist_meta: HashMap<String, Value>,
    playlist_items: HashMap<String, Vec<String>>,
    collection_ids: Vec<String>,
    collection_seen: HashSet<String>,
    albums: HashMap<String, Value>,
    artists: HashMap<String, Value>,
    bookmarks: Bookmarks,
}

impl Builder {
    fn station(&mut self, id: String) -> &mut StationDraft {
        match self.stations.entry(id) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                self.station_order.push(e.key().clone());
                let draft = StationDraft::new(e.key().clone());
                e.insert(draft)
            }
        }
    }

    fn exchange(&mut self, path: &str, request: &Value, body: &Value) -> Result<()> {
        use endpoint::*;
        match path {
            GET_STATIONS => self.stations_page(body),
            GET_STATION_DETAILS => {
                if let Some(id) = request.get("stationId").and_then(id_text) {
                    self.details(id, body);
                }
            }
            GET_STATION_FEEDBACK => {
                if let Some(id) = request.get("stationId").and_then(id_text) {
                    let Some(positive) = request.get("positive").and_then(Value::as_bool) else {
                        bail!("feedback request for station {id} has no boolean `positive`");
                    };
                    self.feedback_page(id, positive, body);
                }
            }
            GET_SORTED_PLAYLISTS => self.playlists_page(body),
            GET_PLAYLIST_TRACKS => {
                let id = request
                    .pointer("/request/pandoraId")
                    .and_then(Value::as_str)
                    .or_else(|| body.get("pandoraId").and_then(Value::as_str));
                if let Some(id) = id {
                    self.tracks_page(id.to_owned(), body);
                }
            }
            GET_COLLECTION_ITEMS => self.items_page(body),
            ANNOTATE_OBJECTS => self.annotate(body),
            GET_BOOKMARKS => {
                let albums = request.get("type").and_then(Value::as_str) == Some("album");
                self.bookmarks_page(albums, body);
            }
            _ => {}
        }
        Ok(())
    }

    fn stations_page(&mut self, body: &Value) {
        for s in body
            .get("stations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = s.get("stationId").and_then(id_text) else {
                continue;
            };
            let st = &mut self.station(id).station;
            fill(&mut st.pandora_id, text(s, "pandoraId"));
            fill(&mut st.name, text(s, "name"));
            fill(&mut st.created, time_text(s, "dateCreated"));
            fill(&mut st.last_played, time_text(s, "lastPlayed"));
            st.is_thumbprint |= flag(s, "isThumbprint");
            st.is_shuffle |= flag(s, "isShuffle");
            if st.genres.is_empty() {
                st.genres = genres(s);
            }
        }
    }

    fn details(&mut self, id: String, body: &Value) {
        let mut songs = Vec::new();
        let st = &mut self.station(id).station;
        fill(&mut st.pandora_id, text(body, "pandoraId"));
        fill(&mut st.name, text(body, "name"));
        fill(
            &mut st.positive_feedback_count,
            number(body, "positiveFeedbackCount"),
        );
        fill(
            &mut st.negative_feedback_count,
            number(body, "negativeFeedbackCount"),
        );
        if st.seeds.is_empty() {
            for raw in body
                .get("seeds")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let seed = seed(raw);
                if seed.kind == SeedKind::Track
                    && let Some(tid) = &seed.pandora_id
                {
                    songs.push((tid.clone(), raw.get("song").cloned().unwrap_or_default()));
                }
                st.seeds.push(seed);
            }
        }
        for (tid, song) in songs {
            self.catalog.seed_song(&tid, &song);
        }
    }

    /// One page of a station's feedback; `positive` is the polarity the
    /// request asked for, which every record on the page carries.
    fn feedback_page(&mut self, station_id: String, positive: bool, body: &Value) {
        for r in body
            .get("feedback")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(fid) = r.get("feedbackId").and_then(id_text) else {
                continue;
            };
            self.catalog.feedback(r);
            let draft = self.station(station_id.clone());
            if !draft.seen.insert((positive, fid.clone())) {
                continue;
            }
            let fb = Feedback {
                feedback_id: fid,
                track_pandora_id: text(r, "pandoraId"),
                positive,
                created: time_text(r, "feedbackDateCreated"),
                station_id: station_id.clone(),
            };
            if positive {
                draft.up.push(fb);
            } else {
                draft.down.push(fb);
            }
        }
    }

    fn playlists_page(&mut self, body: &Value) {
        for item in body
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(id) = text(item, "pandoraId")
                && !self.playlist_order.contains(&id)
            {
                self.playlist_order.push(id);
            }
        }
        self.annotate(body);
    }

    fn tracks_page(&mut self, playlist: String, body: &Value) {
        let ids: Vec<String> = body
            .get("tracks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|t| text(t, "trackPandoraId").or_else(|| text(t, "pandoraId")))
            .collect();
        for id in &ids {
            self.catalog.entry(id);
        }
        self.playlist_items.entry(playlist).or_default().extend(ids);
        self.annotate(body);
    }

    fn items_page(&mut self, body: &Value) {
        for item in body
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = text(item, "pandoraId") else {
                continue;
            };
            let Some(kind) = id_type(&id) else { continue };
            if kind == "PL" || !self.collection_seen.insert(id.clone()) {
                continue;
            }
            match kind {
                "TR" => {
                    self.catalog.entry(&id);
                }
                "AL" | "AR" => {}
                _ => {
                    let _ = writeln!(
                        std::io::stderr(),
                        "{}",
                        unknown_collection_item_warning(&id)
                    );
                    continue;
                }
            }
            self.collection_ids.push(id);
        }
        self.annotate(body);
    }

    fn annotate(&mut self, body: &Value) {
        for (id, a) in annotations(body) {
            match id_type(id) {
                Some("TR") => self.catalog.annotation(id, a),
                Some("PL") => {
                    self.playlist_meta
                        .entry(id.to_owned())
                        .or_insert_with(|| a.clone());
                }
                Some("AL") => {
                    self.albums
                        .entry(id.to_owned())
                        .or_insert_with(|| a.clone());
                }
                Some("AR") => {
                    self.artists
                        .entry(id.to_owned())
                        .or_insert_with(|| a.clone());
                }
                _ => {}
            }
        }
    }

    fn bookmarks_page(&mut self, albums: bool, body: &Value) {
        let records = body
            .as_object()
            .into_iter()
            .flat_map(|o| o.values())
            .filter_map(Value::as_array)
            .flatten();
        for r in records {
            let title = if albums { "albumTitle" } else { "songTitle" };
            let b = Bookmark {
                pandora_id: text(r, "pandoraId"),
                title: text(r, title),
                artist: text(r, "artistName"),
                created: time_text(r, "dateCreated"),
            };
            if albums {
                self.bookmarks.albums.push(b);
            } else {
                self.bookmarks.tracks.push(b);
            }
        }
    }

    fn finish(mut self) -> Normalized {
        let stations = self
            .station_order
            .iter()
            .filter_map(|id| self.stations.remove(id))
            .map(|d| {
                let mut s = d.station;
                s.feedback = d.up.into_iter().chain(d.down).collect();
                s
            })
            .collect();
        let empty = Value::Null;
        let playlists = self
            .playlist_order
            .iter()
            .map(|id| {
                let meta = self.playlist_meta.get(id).unwrap_or(&empty);
                Playlist {
                    pandora_id: id.clone(),
                    name: text(meta, "name"),
                    description: text(meta, "description"),
                    created: time_text(meta, "timeCreated"),
                    linked_type: text(meta, "linkedType").filter(|kind| kind != "None"),
                    items: self.playlist_items.remove(id).unwrap_or_default(),
                }
            })
            .collect();
        let mut collection = Collection::default();
        for id in self.collection_ids {
            match id_type(&id) {
                Some("TR") => collection.tracks.push(id),
                Some("AL") => {
                    let a = self.albums.get(&id).unwrap_or(&empty);
                    collection.albums.push(Album {
                        pandora_id: id,
                        name: text(a, "name"),
                        artist: text(a, "artistName"),
                        artist_pandora_id: text(a, "artistId"),
                    });
                }
                Some("AR") => {
                    let a = self.artists.get(&id).unwrap_or(&empty);
                    collection.artists.push(Artist {
                        pandora_id: id,
                        name: text(a, "name"),
                    });
                }
                _ => {}
            }
        }
        Normalized {
            stations,
            playlists,
            collection,
            bookmarks: self.bookmarks,
            tracks: self.catalog.finish(),
        }
    }
}

/// The stderr line for a collection item that is not a track, album or
/// artist; the item stays in the raw run only.
fn unknown_collection_item_warning(id: &str) -> String {
    format!(
        "warning: collection item {id} is not a track, album or artist; left out of collection.json"
    )
}

/// A station's genres: strings, or objects carrying a name.
fn genres(station: &Value) -> Vec<String> {
    station
        .get("genre")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|g| match g {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            other => text(other, "name").or_else(|| text(other, "genreName")),
        })
        .collect()
}

/// A `getStationDetails` seed: a `song`, `artist` or `genre` object, else
/// classified by its id's prefix.
fn seed(raw: &Value) -> Seed {
    let (kind, inner, name_key) = if let Some(s) = raw.get("song").filter(|v| v.is_object()) {
        (SeedKind::Track, s, "songTitle")
    } else if let Some(a) = raw.get("artist").filter(|v| v.is_object()) {
        (SeedKind::Artist, a, "artistName")
    } else if let Some(g) = raw.get("genre").filter(|v| v.is_object()) {
        (SeedKind::Genre, g, "genreName")
    } else {
        let kind = match text(raw, "pandoraId").as_deref().and_then(id_type) {
            Some("TR") => SeedKind::Track,
            Some("AR") => SeedKind::Artist,
            Some("GE") => SeedKind::Genre,
            _ => SeedKind::Other,
        };
        (kind, raw, "name")
    };
    Seed {
        kind,
        pandora_id: text(inner, "pandoraId").or_else(|| text(raw, "pandoraId")),
        name: text(inner, name_key)
            .or_else(|| text(inner, "name"))
            .or_else(|| text(raw, "name")),
    }
}

/// The sequence number of a raw file name `NNNN-<slug>.json`.
fn sequence(name: &str) -> Option<u64> {
    name.split_once('-')?.0.parse().ok()
}

/// Derives the normalized data from the raw run in `dir`, reading its
/// successful exchanges in capture order.
pub fn normalize_run(dir: &Path) -> Result<Normalized> {
    let mut files: Vec<(u64, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == repo::MANIFEST_FILE || !name.ends_with(".json") {
            continue;
        }
        if let Some(n) = sequence(name) {
            files.push((n, entry.path()));
        }
    }
    files.sort();
    let mut builder = Builder::default();
    for (_, path) in files {
        let ex: StoredExchange = repo::read_json(&path)?;
        if client::succeeded(ex.status, &ex.response) {
            builder
                .exchange(&ex.path, &ex.request, &ex.response)
                .with_context(|| format!("normalizing {}", path.display()))?;
        }
    }
    Ok(builder.finish())
}

/// Writes the normalized files of `paths` from `data`.
pub fn write(paths: &ProfilePaths, data: &Normalized) -> Result<()> {
    repo::write_json(&paths.stations(), &data.stations)?;
    repo::write_json(&paths.playlists(), &data.playlists)?;
    repo::write_json(&paths.collection(), &data.collection)?;
    repo::write_json(&paths.bookmarks(), &data.bookmarks)?;
    repo::write_json(&paths.tracks(), &data.tracks)
}

/// Reads the normalized files [`write`] wrote; `None` when there is no
/// `stations.json` (the profile has no Pandora export), an error when it is
/// there and another file is missing or unreadable.
pub fn read_opt(paths: &ProfilePaths) -> Result<Option<Normalized>> {
    let Some(stations) = repo::read_json_opt(&paths.stations())? else {
        return Ok(None);
    };
    Ok(Some(Normalized {
        stations,
        playlists: repo::read_json(&paths.playlists())?,
        collection: repo::read_json(&paths.collection())?,
        bookmarks: repo::read_json(&paths.bookmarks())?,
        tracks: repo::read_json(&paths.tracks())?,
    }))
}

/// What one normalization holds, counted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub run_id: String,
    pub stations: usize,
    pub thumbs_up: usize,
    pub thumbs_down: usize,
    pub playlists: usize,
    pub collection_tracks: usize,
    pub collection_albums: usize,
    pub collection_artists: usize,
    pub bookmarks: usize,
    /// Distinct tracks.
    pub tracks: usize,
}

impl Summary {
    /// The `normalized run …` line.
    pub fn write_text(&self, out: &mut dyn Write) -> std::io::Result<()> {
        writeln!(
            out,
            "normalized run {}: {} stations, {} thumbs up, {} thumbs down, {} playlists, \
             collection {} tracks / {} albums / {} artists, {} bookmarks, {} distinct tracks",
            self.run_id,
            self.stations,
            self.thumbs_up,
            self.thumbs_down,
            self.playlists,
            self.collection_tracks,
            self.collection_albums,
            self.collection_artists,
            self.bookmarks,
            self.tracks,
        )
    }
}

/// The counts of `data`, normalized from run `run_id`.
pub fn summary(data: &Normalized, run_id: &str) -> Summary {
    let thumbs = |positive: bool| {
        data.stations
            .iter()
            .flat_map(|s| &s.feedback)
            .filter(|f| f.positive == positive)
            .count()
    };
    let c = &data.collection;
    Summary {
        run_id: run_id.to_owned(),
        stations: data.stations.len(),
        thumbs_up: thumbs(true),
        thumbs_down: thumbs(false),
        playlists: data.playlists.len(),
        collection_tracks: c.tracks.len(),
        collection_albums: c.albums.len(),
        collection_artists: c.artists.len(),
        bookmarks: data.bookmarks.tracks.len() + data.bookmarks.albums.len(),
        tracks: data.tracks.len(),
    }
}

/// `sss pandora normalize`: re-derives the normalized files from the latest
/// complete run and returns what they hold.
pub fn normalize(paths: &ProfilePaths) -> Result<Summary> {
    let Some(manifest) = export::latest_complete_run(paths)? else {
        bail!("no complete export run to normalize; run `sss pandora export`");
    };
    let data = normalize_run(&paths.raw_run_dir(&manifest.run_id))?;
    write(paths, &data)?;
    Ok(summary(&data, &manifest.run_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pandora::export::{Manifest, StepResult, Steps};
    use crate::repo::Repo;
    use serde_json::json;

    /// Writes exchanges into a run directory, numbered in order.
    fn write_run(dir: &Path, exchanges: &[(&str, Value, u16, Value)]) {
        fs::create_dir_all(dir).unwrap();
        for (i, (path, request, status, response)) in exchanges.iter().enumerate() {
            let file = dir.join(format!("{:04}-x.json", i + 1));
            let ex = json!({"at": "2026-10-13T00:00:00Z", "path": path,
                            "request": request, "status": status, "response": response});
            fs::write(file, serde_json::to_vec(&ex).unwrap()).unwrap();
        }
    }

    fn normalize_exchanges(exchanges: &[(&str, Value, u16, Value)]) -> Normalized {
        let tmp = tempfile::tempdir().unwrap();
        write_run(tmp.path(), exchanges);
        normalize_run(tmp.path()).unwrap()
    }

    fn fb(id: &str, track: &str, created: Option<&str>) -> Value {
        let mut r = json!({
            "feedbackId": id, "pandoraId": track, "musicId": "S1",
            "songTitle": "First Day Out", "artistName": "Tee Grizzley",
            "albumTitle": "First Day Out (Explicit)", "trackLength": 254, "trackNum": 1,
        });
        if let Some(c) = created {
            r["feedbackDateCreated"] = json!(c);
        }
        r
    }

    fn feedback_req(station: &str, positive: bool, start: usize) -> Value {
        json!({"stationId": station, "positive": positive, "pageSize": 100, "startIndex": start})
    }

    #[test]
    fn stations_seeds_and_feedback_normalize_and_tracks_dedupe() {
        let data = normalize_exchanges(&[
            (
                "v1/station/getStations",
                json!({}),
                200,
                json!({"totalStations": 2, "stations": [
                    {"stationId": "11", "pandoraId": "ST:0:11", "name": "Chill Out Radio",
                     "dateCreated": "2020-01-01T00:00:00Z", "genre": ["Chill"]},
                    {"stationId": "12", "name": "Thumbprint Radio", "isThumbprint": true},
                ]}),
            ),
            (
                "v1/station/getStationDetails",
                json!({"stationId": "11"}),
                200,
                json!({"positiveFeedbackCount": 2, "negativeFeedbackCount": 1, "seeds": [
                    {"pandoraId": "AR:1", "artist": {"artistName": "Jon Bellion", "pandoraId": "AR:1"}},
                    {"pandoraId": "TR:9", "song": {"songTitle": "All Time Low",
                                                   "artistSummary": "Jon Bellion", "pandoraId": "TR:9"}},
                    {"pandoraId": "GE:4", "genre": {"genreName": "Chill"}},
                ]}),
            ),
            (
                "v1/station/getStationFeedback",
                feedback_req("11", true, 0),
                200,
                json!({"total": 2, "feedback": [fb("f1", "TR:1", None), fb("f2", "TR:2", None)]}),
            ),
            (
                "v1/station/getStationFeedback",
                feedback_req("11", false, 0),
                200,
                json!({"total": 1, "feedback": [fb("f3", "TR:1", None)]}),
            ),
            (
                "v1/station/getStationFeedback",
                feedback_req("12", true, 0),
                200,
                json!({"total": 1, "feedback": [fb("f4", "TR:2", None)]}),
            ),
        ]);
        assert_eq!(data.stations.len(), 2);
        let s = &data.stations[0];
        assert_eq!(s.pandora_id.as_deref(), Some("ST:0:11"));
        assert_eq!(s.name.as_deref(), Some("Chill Out Radio"));
        assert_eq!(s.genres, ["Chill"]);
        assert_eq!(s.positive_feedback_count, Some(2));
        let kinds: Vec<_> = s.seeds.iter().map(|x| (x.kind, x.name.clone())).collect();
        assert_eq!(
            kinds,
            [
                (SeedKind::Artist, Some("Jon Bellion".into())),
                (SeedKind::Track, Some("All Time Low".into())),
                (SeedKind::Genre, Some("Chill".into())),
            ]
        );
        let fbs: Vec<_> = s
            .feedback
            .iter()
            .map(|f| (f.feedback_id.as_str(), f.positive))
            .collect();
        assert_eq!(fbs, [("f1", true), ("f2", true), ("f3", false)]);
        assert!(data.stations[1].is_thumbprint);
        assert_eq!(data.stations[1].feedback[0].station_id, "12");

        // TR:1 and TR:2 once each however often thumbed; the track seed too.
        assert_eq!(
            data.tracks.keys().collect::<Vec<_>>(),
            ["TR:1", "TR:2", "TR:9"]
        );
        let t = &data.tracks["TR:1"];
        assert_eq!(t.title.as_deref(), Some("First Day Out"));
        assert_eq!(t.duration_s, Some(254));
        assert_eq!(t.disc_num, None);
        assert!(t.explicit_hint, "(Explicit) in the album title");
        assert_eq!(t.isrc, None);
        assert_eq!(data.tracks["TR:9"].artist.as_deref(), Some("Jon Bellion"));
    }

    #[test]
    fn feedback_with_and_without_dates_orders_for_the_station_playlist() {
        let data = normalize_exchanges(&[
            (
                "v1/station/getStations",
                json!({}),
                200,
                json!({"stations": [{"stationId": "11"}]}),
            ),
            (
                "v1/station/getStationFeedback",
                feedback_req("11", true, 0),
                200,
                json!({"feedback": [
                    fb("late", "TR:1", Some("2024-05-01T10:00:00.000Z")),
                    fb("undated-a", "TR:2", None),
                    fb("early", "TR:3", Some("2021-01-01T00:00:00.000Z")),
                    fb("undated-b", "TR:4", None),
                ]}),
            ),
        ]);
        let s = &data.stations[0];
        assert_eq!(
            s.feedback[0].created.as_deref(),
            Some("2024-05-01T10:00:00.000Z")
        );
        let order: Vec<_> = s
            .thumbs_up_in_order()
            .iter()
            .map(|f| f.feedback_id.as_str())
            .collect();
        assert_eq!(order, ["early", "late", "undated-a", "undated-b"]);
    }

    #[test]
    fn a_track_in_feedback_and_an_annotation_merges_into_one() {
        let data = normalize_exchanges(&[
            (
                "v1/station/getStationFeedback",
                feedback_req("11", true, 0),
                200,
                json!({"feedback": [fb("f1", "TR:1", None)]}),
            ),
            (
                "v6/collections/getSortedPlaylists",
                json!({}),
                200,
                json!({"items": [{"pandoraId": "PL:1"}, {"pandoraId": "PL:2"}],
                       "annotations": {"PL:1": {"name": "Station thumbs", "linkedType": "StationThumbs",
                                                "timeCreated": 1_600_000_000_000_i64},
                                       "PL:2": {"name": "Road trip", "linkedType": "None"}}}),
            ),
            (
                "v7/playlists/getTracks",
                json!({"request": {"pandoraId": "PL:1", "offset": 0}}),
                200,
                json!({"version": 3, "tracks": [{"trackPandoraId": "TR:1"}],
                       "annotations": {"TR:1": {
                           "name": "Other Title", "artistName": "Tee Grizzley", "albumName": "First Day Out",
                           "isrc": "USAT21600001", "explicitness": "CLEAN", "duration": 250,
                           "durationMillis": 250_000,
                           "artistId": "AR:1", "albumId": "AL:1"}}}),
            ),
        ]);
        assert_eq!(data.tracks.len(), 1);
        let t = &data.tracks["TR:1"];
        assert_eq!(t.isrc.as_deref(), Some("USAT21600001"));
        assert!(!t.explicit_hint, "the annotation overrides the heuristic");
        assert_eq!(t.duration_s, Some(254), "feedback trackLength wins");
        assert_eq!(t.title.as_deref(), Some("First Day Out"), "first seen wins");
        assert_eq!(t.artist_pandora_id.as_deref(), Some("AR:1"));
        assert_eq!(t.album_pandora_id.as_deref(), Some("AL:1"));
        let p = &data.playlists[0];
        assert_eq!(p.items, ["TR:1"]);
        assert_eq!(p.linked_type.as_deref(), Some("StationThumbs"));
        assert_eq!(p.created.as_deref(), Some("2020-09-13T12:26:40Z"));
        let made = &data.playlists[1];
        assert_eq!(made.linked_type, None, "\"None\" is a member-made playlist");
        assert!(
            serde_json::to_string(made)
                .unwrap()
                .contains("\"linked_type\":null")
        );
    }

    #[test]
    fn collection_items_of_other_types_are_left_out() {
        let data = normalize_exchanges(&[(
            "v6/collections/getItems",
            json!({"request": {"limit": 100}}),
            200,
            json!({"items": [{"pandoraId": "ST:0:42"}, {"pandoraId": "AR:5"},
                             {"pandoraId": "PC:123"}, {"pandoraId": "TR:5"}]}),
        )]);
        let c = &data.collection;
        assert_eq!(c.tracks, ["TR:5"]);
        assert!(c.albums.is_empty());
        let artists: Vec<_> = c.artists.iter().map(|a| a.pandora_id.as_str()).collect();
        assert_eq!(artists, ["AR:5"]);
        assert!(!data.tracks.contains_key("PC:123"));
        assert_eq!(
            unknown_collection_item_warning("PC:123"),
            "warning: collection item PC:123 is not a track, album or artist; \
             left out of collection.json"
        );
    }

    #[test]
    fn collection_items_take_names_from_annotate_objects() {
        let data = normalize_exchanges(&[
            (
                "v6/collections/getItems",
                json!({"request": {"limit": 100}}),
                200,
                json!({"items": [{"pandoraId": "PL:1"}, {"pandoraId": "TR:5"}, {"pandoraId": "AL:5"}],
                       "cursor": "c2"}),
            ),
            (
                "v6/collections/getItems",
                json!({"request": {"limit": 100, "cursor": "c2"}}),
                200,
                json!({"items": [{"pandoraId": "AR:5"}]}),
            ),
            (
                "v4/catalog/annotateObjects",
                json!({"pandoraIds": ["TR:5", "AL:5", "AR:5"]}),
                200,
                json!({
                    "TR:5": {"name": "Guillotine", "artistName": "Jon Bellion", "isrc": "USUM71500001",
                             "explicitness": "EXPLICIT", "duration": 200},
                    "AL:5": {"name": "The Human Condition", "artistName": "Jon Bellion", "artistId": "AR:5"},
                    "AR:5": {"name": "Jon Bellion"},
                }),
            ),
        ]);
        let c = &data.collection;
        assert_eq!(c.tracks, ["TR:5"]);
        assert_eq!(c.albums[0].name.as_deref(), Some("The Human Condition"));
        assert_eq!(c.albums[0].artist_pandora_id.as_deref(), Some("AR:5"));
        assert_eq!(c.artists[0].name.as_deref(), Some("Jon Bellion"));
        let t = &data.tracks["TR:5"];
        assert_eq!(t.isrc.as_deref(), Some("USUM71500001"));
        assert!(t.explicit_hint);
        assert_eq!(
            t.duration_s,
            Some(200),
            "annotation duration without feedback"
        );
        assert!(data.playlists.is_empty());
    }

    #[test]
    fn playlist_pages_and_bookmarks_keep_their_order() {
        let data = normalize_exchanges(&[
            (
                "v6/collections/getSortedPlaylists",
                json!({}),
                200,
                json!({"items": [{"pandoraId": "PL:2"}, {"pandoraId": "PL:1"}],
                       "annotations": {"PL:2": {"name": "Mine", "description": "road trip"}}}),
            ),
            (
                "v7/playlists/getTracks",
                json!({"request": {"pandoraId": "PL:2", "offset": 0}}),
                200,
                json!({"version": 7, "tracks": [{"trackPandoraId": "TR:3"}, {"trackPandoraId": "TR:1"}]}),
            ),
            (
                "v7/playlists/getTracks",
                json!({"request": {"pandoraId": "PL:2", "offset": 2, "playlistVersion": 7}}),
                200,
                json!({"version": 7, "tracks": [{"trackPandoraId": "TR:2"}]}),
            ),
            (
                "v1/bookmark/getBookmarks",
                json!({"type": "track"}),
                200,
                json!({"tracks": [{"pandoraId": "TR:7", "songTitle": "Song", "artistName": "Tee Grizzley"}]}),
            ),
            (
                "v1/bookmark/getBookmarks",
                json!({"type": "album"}),
                200,
                json!({"albums": [{"pandoraId": "AL:7", "albumTitle": "Album"}]}),
            ),
        ]);
        let ids: Vec<_> = data
            .playlists
            .iter()
            .map(|p| p.pandora_id.as_str())
            .collect();
        assert_eq!(ids, ["PL:2", "PL:1"]);
        assert_eq!(data.playlists[0].items, ["TR:3", "TR:1", "TR:2"]);
        assert_eq!(data.playlists[0].description.as_deref(), Some("road trip"));
        assert_eq!(data.playlists[0].linked_type, None);
        assert!(data.playlists[1].items.is_empty());
        assert_eq!(data.bookmarks.tracks[0].title.as_deref(), Some("Song"));
        assert_eq!(data.bookmarks.albums[0].title.as_deref(), Some("Album"));
    }

    #[test]
    fn failed_and_retried_exchanges_are_ignored() {
        let data = normalize_exchanges(&[
            ("v1/station/getStations", json!({}), 503, json!("busy")),
            (
                "v1/station/getStations",
                json!({}),
                200,
                json!({"errorCode": 99022, "stations": [{"stationId": "99"}]}),
            ),
            (
                "v1/station/getStations",
                json!({}),
                200,
                json!({"stations": [{"stationId": "11"}]}),
            ),
        ]);
        let ids: Vec<_> = data
            .stations
            .iter()
            .map(|s| s.station_id.as_str())
            .collect();
        assert_eq!(ids, ["11"]);
    }

    #[test]
    fn raw_files_are_read_in_sequence_order_past_9999() {
        let tmp = tempfile::tempdir().unwrap();
        let ex = |name: &str| {
            json!({"at": "", "path": "v1/station/getStations", "request": {}, "status": 200,
                   "response": {"stations": [{"stationId": name}]}})
        };
        fs::write(tmp.path().join("10000-a.json"), ex("late").to_string()).unwrap();
        fs::write(tmp.path().join("9999-a.json"), ex("early").to_string()).unwrap();
        let ids: Vec<_> = normalize_run(tmp.path())
            .unwrap()
            .stations
            .into_iter()
            .map(|s| s.station_id)
            .collect();
        assert_eq!(ids, ["early", "late"]);
    }

    fn manifest(run_id: &str, complete: bool) -> Manifest {
        Manifest {
            run_id: run_id.into(),
            started: "2026-10-13T00:00:00Z".into(),
            finished: "2026-10-13T00:01:00Z".into(),
            requests: 1,
            steps: Steps {
                bookmarks: StepResult {
                    status: if complete {
                        export::StepStatus::Ok
                    } else {
                        export::StepStatus::Failed
                    },
                    ..StepResult::default()
                },
                ..Steps::default()
            },
            complete,
        }
    }

    #[test]
    fn normalize_reads_the_latest_complete_run_and_writes_every_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Repo::at(tmp.path()).profile("alice").unwrap();
        let stations = |id: &str| {
            vec![(
                "v1/station/getStations",
                json!({}),
                200u16,
                json!({"stations": [{"stationId": id}]}),
            )]
        };
        for (id, station, complete) in [
            ("20261013T000000Z", "old", true),
            ("20261013T000000Z-2", "newer", true),
            ("20261013T000000Z-10", "newest", true),
            ("20261014T000000Z", "incomplete", false),
        ] {
            let dir = paths.raw_run_dir(id);
            write_run(&dir, &stations(station));
            repo::write_json(&dir.join(repo::MANIFEST_FILE), &manifest(id, complete)).unwrap();
        }
        // A run that never ended has no manifest.
        write_run(
            &paths.raw_run_dir("20261015T000000Z"),
            &stations("unfinished"),
        );

        let summary = normalize(&paths).unwrap();
        assert_eq!(summary.run_id, "20261013T000000Z-10");
        assert_eq!(summary.stations, 1);
        let on_disk: Vec<Station> = repo::read_json(&paths.stations()).unwrap();
        assert_eq!(on_disk[0].station_id, "newest");
        for file in [
            paths.playlists(),
            paths.collection(),
            paths.bookmarks(),
            paths.tracks(),
        ] {
            assert!(file.is_file(), "{}", file.display());
        }
        // Re-running writes identical bytes.
        let before = fs::read(paths.stations()).unwrap();
        normalize(&paths).unwrap();
        assert_eq!(fs::read(paths.stations()).unwrap(), before);
    }

    #[test]
    fn read_returns_what_write_wrote_and_none_without_an_export() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Repo::at(tmp.path()).profile("alice").unwrap();
        assert_eq!(read_opt(&paths).unwrap(), None);

        let data = normalize_exchanges(&[(
            "v1/station/getStationFeedback",
            feedback_req("11", true, 0),
            200,
            json!({"total": 1, "feedback": [fb("f1", "TR:1", None)]}),
        )]);
        write(&paths, &data).unwrap();
        assert_eq!(read_opt(&paths).unwrap(), Some(data));
        fs::remove_file(paths.tracks()).unwrap();
        assert!(read_opt(&paths).is_err());
    }

    #[test]
    fn normalize_without_a_complete_run_says_to_export() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Repo::at(tmp.path()).profile("alice").unwrap();
        let err = normalize(&paths).unwrap_err().to_string();
        assert_eq!(
            err,
            "no complete export run to normalize; run `sss pandora export`"
        );
    }

    #[test]
    fn feedback_polarity_comes_from_the_request_alone() {
        let mut down = fb("f1", "TR:1", None);
        down["isPositive"] = json!(true);
        let data = normalize_exchanges(&[(
            "v1/station/getStationFeedback",
            feedback_req("11", false, 0),
            200,
            json!({"total": 1, "feedback": [down]}),
        )]);
        assert!(!data.stations[0].feedback[0].positive);
    }

    #[test]
    fn summary_counts_the_normalized_data_and_prints_the_normalize_line() {
        let data = normalize_exchanges(&[
            (
                "v1/station/getStationFeedback",
                feedback_req("11", true, 0),
                200,
                json!({"total": 2, "feedback": [fb("f1", "TR:1", None), fb("f2", "TR:2", None)]}),
            ),
            (
                "v1/station/getStationFeedback",
                feedback_req("11", false, 0),
                200,
                json!({"total": 1, "feedback": [fb("f3", "TR:3", None)]}),
            ),
        ]);
        let summary = summary(&data, "20261013T000000Z");
        assert_eq!(
            (summary.stations, summary.thumbs_up, summary.thumbs_down),
            (1, 2, 1)
        );
        assert_eq!(summary.tracks, 3);
        let mut text = Vec::new();
        summary.write_text(&mut text).unwrap();
        assert_eq!(
            String::from_utf8(text).unwrap(),
            "normalized run 20261013T000000Z: 1 stations, 2 thumbs up, 1 thumbs down, \
             0 playlists, collection 0 tracks / 0 albums / 0 artists, 0 bookmarks, \
             3 distinct tracks\n"
        );
    }

    #[test]
    fn feedback_without_a_requested_polarity_fails_normalization() {
        let tmp = tempfile::tempdir().unwrap();
        let mut record = fb("f1", "TR:1", None);
        record["isPositive"] = json!(false);
        write_run(
            tmp.path(),
            &[(
                "v1/station/getStationFeedback",
                json!({"stationId": "11", "pageSize": 100, "startIndex": 0}),
                200,
                json!({"total": 1, "feedback": [record]}),
            )],
        );
        let err = normalize_run(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("0001-x.json"), "{err:#}");
        assert_eq!(
            err.root_cause().to_string(),
            "feedback request for station 11 has no boolean `positive`"
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(sequence("0012-v1-station.json"), Some(12));
        assert_eq!(
            time_text(&json!({"t": 1_600_000_000}), "t").as_deref(),
            Some("2020-09-13T12:26:40Z")
        );
        assert_eq!(explicitness(&json!({"explicitness": "CLEAN"})), Some(false));
        assert_eq!(explicitness(&json!({})), None);
        assert_eq!(
            seed(&json!({"pandoraId": "XX:1", "name": "?"})).kind,
            SeedKind::Other
        );
    }
}
