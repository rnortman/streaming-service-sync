//! `sss youtube import DIR`: Google Takeout's YouTube and YouTube Music
//! archive. The archive's files are found under a directory, copied byte
//! for byte into `youtube/raw/<run-id>/takeout/…` before anything reads
//! them, and the copies parsed into source records; `manifest.json` records
//! the copies' digests and each step's outcome.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use crate::error::Kind;
use crate::fail;
use crate::repo::{self, ProfilePaths};
use crate::youtube::model::Summary;

const HISTORY_FILE: &str = "watch-history.json";
const HISTORY_HTML_FILE: &str = "watch-history.html";
const PLAYLISTS_FILE: &str = "playlists.csv";
const ITEMS_SUFFIX: &str = "-videos.csv";
const SUBSCRIPTIONS_FILE: &str = "subscriptions.csv";
/// The library file's name in current archives, then in older ones.
const LIBRARY_FILES: [&str; 2] = ["music library songs.csv", "music-library-songs.csv"];

/// Where each kind of file is copied in a run, relative to the run.
const HISTORY_DIR: &str = "takeout/history";
const PLAYLISTS_DIR: &str = "takeout/playlists";
const SUBSCRIPTIONS_DIR: &str = "takeout/subscriptions";
const LIBRARY_DIR: &str = "takeout/music (library and uploads)";

/// Characters Takeout writes as `_` in a playlist's items file name: those a
/// Windows file name may not hold, and the apostrophe.
const FILE_NAME_FORBIDDEN: [char; 10] = ['\\', '/', ':', '*', '?', '"', '<', '>', '|', '\''];

/// Whether `s` has the shape of a YouTube video id.
pub fn is_video_id(s: &str) -> bool {
    s.len() == 11 && s.bytes().all(id_byte)
}

/// Whether `s` has the shape of a playlist id: `PL` and 32 id characters.
pub fn is_playlist_id(s: &str) -> bool {
    s.len() == 34 && s.starts_with("PL") && s.bytes().all(id_byte)
}

/// Whether `s` has the shape of a channel id: `UC` and 22 id characters.
pub fn is_channel_id(s: &str) -> bool {
    s.len() == 24 && s.starts_with("UC") && s.bytes().all(id_byte)
}

fn id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// `s` as an instant in the archive's one canonical form,
/// `YYYY-MM-DDTHH:MM:SS.mmmZ`; `None` when it is not RFC 3339.
pub fn canonical_time(s: &str) -> Option<String> {
    let t = OffsetDateTime::parse(s.trim(), &Rfc3339)
        .ok()?
        .to_offset(UtcOffset::UTC);
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond()
    ))
}

/// The channel id of a `…/channel/UC…` address.
pub fn channel_id_from_url(url: &str) -> Option<String> {
    let id = url.trim().rsplit_once("/channel/")?.1;
    is_channel_id(id).then(|| id.to_owned())
}

/// The video id of a YouTube watch address: `Ok(None)` for any other
/// address, `Err(())` for a watch address whose `v` is not a video id.
fn watch_video_id(url: &str) -> Result<Option<String>, ()> {
    let Ok(parsed) = url::Url::parse(url) else {
        return Ok(None);
    };
    let youtube = parsed
        .host_str()
        .is_some_and(|h| h == "youtube.com" || h.ends_with(".youtube.com"));
    if !youtube || parsed.path() != "/watch" {
        return Ok(None);
    }
    match parsed.query_pairs().find(|(k, _)| k == "v") {
        Some((_, v)) if is_video_id(v.trim()) => Ok(Some(v.trim().to_owned())),
        _ => Err(()),
    }
}

/// The key an items file name and a playlist title share when Takeout wrote
/// that playlist's items to that file: forbidden characters as `_`, trimmed,
/// case-folded.
pub fn file_key(title: &str) -> String {
    let mapped: String = title
        .chars()
        .map(|c| {
            if FILE_NAME_FORBIDDEN.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    mapped.trim().to_lowercase()
}

// ---------------------------------------------------------------------------
// Source records

/// One element of `watch-history.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRecord {
    /// Canonical, see [`canonical_time`].
    pub time: String,
    /// `YouTube Music` or `YouTube`; absent in older archives.
    pub header: Option<String>,
    /// As the archive has it, verb included (`Watched …`).
    pub title: String,
    pub title_url: Option<String>,
    /// From `title_url`'s `v` when it is a watch address.
    pub video_id: Option<String>,
    /// The first subtitle: the uploading channel.
    pub channel: Option<ChannelLink>,
    /// The record is marked `From Google Ads`.
    pub ad: bool,
}

impl HistoryRecord {
    /// Whether Takeout withheld the video's title and channel: the title is
    /// the verb and the address, and there is no channel.
    pub fn is_withheld(&self) -> bool {
        self.channel.is_none()
            && self
                .title_url
                .as_deref()
                .is_some_and(|u| self.title.strip_prefix("Watched ") == Some(u))
    }
}

/// A channel as a history record names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelLink {
    pub name: String,
    pub url: Option<String>,
}

/// Where a playlist's items came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemsSource {
    /// Its items file.
    File,
    /// No items file has its name: an empty playlist.
    None,
    /// Another playlist's title maps to the same items file name, and the
    /// archive does not say whose items the file holds.
    AmbiguousTitle,
}

/// One playlist: a row of `playlists.csv` with its items, or an items file
/// no row names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistRecord {
    /// `None` for an items file no row names.
    pub playlist_id: Option<String>,
    pub title: String,
    pub created: Option<String>,
    pub updated: Option<String>,
    pub order: Option<String>,
    pub visibility: Option<String>,
    pub items_source: ItemsSource,
    /// In the file's row order, repeats kept.
    pub items: Vec<PlaylistItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistItem {
    pub video_id: String,
    pub added_at: Option<String>,
}

/// One row of `subscriptions.csv`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionRecord {
    pub channel_id: String,
    pub url: Option<String>,
    /// Possibly empty.
    pub name: String,
}

/// One row of the music library songs file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySong {
    pub video_id: String,
    pub title: Option<String>,
    pub album: Option<String>,
    /// `Artist Name 1`, `Artist Name 2`, … in order, blanks dropped.
    pub artists: Vec<String>,
}

/// Everything parsed from one run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Archive {
    pub history: Vec<HistoryRecord>,
    pub playlists: Vec<PlaylistRecord>,
    pub subscriptions: Vec<SubscriptionRecord>,
    pub library: Vec<LibrarySong>,
}

/// One file's records, the rows dropped for an invalid id or time, and
/// what a person should know about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed<T> {
    pub records: Vec<T>,
    pub skipped: u64,
    pub warnings: Vec<String>,
}

impl<T> Parsed<T> {
    fn new() -> Self {
        Self {
            records: Vec::new(),
            skipped: 0,
            warnings: Vec::new(),
        }
    }
}

/// A history element as far as it can be read: a field of an unexpected
/// type reads as absent, so only the time and the address decide whether
/// a record is kept.
#[derive(Default, Deserialize)]
struct RawHistory {
    #[serde(default, deserialize_with = "lenient")]
    header: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    title: Option<String>,
    #[serde(rename = "titleUrl", default, deserialize_with = "lenient")]
    title_url: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    subtitles: Option<Vec<RawSubtitle>>,
    #[serde(default, deserialize_with = "lenient")]
    details: Option<Vec<RawDetail>>,
    #[serde(default, deserialize_with = "lenient")]
    time: Option<String>,
}

#[derive(Deserialize)]
struct RawSubtitle {
    #[serde(default, deserialize_with = "lenient")]
    name: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    url: Option<String>,
}

#[derive(Deserialize)]
struct RawDetail {
    #[serde(default, deserialize_with = "lenient")]
    name: Option<String>,
}

/// The field as a `T`, or `None` when it is null or of another type.
fn lenient<'de, D, T>(d: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    Ok(serde_json::from_value(Value::deserialize(d)?).ok())
}

/// `watch-history.json`. A record with no parsable time, or a watch
/// address without a valid video id, is skipped.
pub fn parse_history(bytes: &[u8]) -> Result<Parsed<HistoryRecord>> {
    let entries: Vec<Value> = serde_json::from_slice(bytes).context("not a JSON array")?;
    let mut parsed = Parsed::new();
    for entry in entries {
        let raw = serde_json::from_value::<RawHistory>(entry).unwrap_or_default();
        let Some(time) = raw.time.as_deref().and_then(canonical_time) else {
            parsed.skipped += 1;
            continue;
        };
        let Ok(video_id) = raw.title_url.as_deref().map_or(Ok(None), watch_video_id) else {
            parsed.skipped += 1;
            continue;
        };
        parsed.records.push(HistoryRecord {
            time,
            header: raw.header,
            title: raw.title.unwrap_or_default(),
            title_url: raw.title_url,
            video_id,
            channel: raw
                .subtitles
                .into_iter()
                .flatten()
                .next()
                .map(|s| ChannelLink {
                    name: s.name.unwrap_or_default(),
                    url: s.url,
                }),
            ad: raw
                .details
                .iter()
                .flatten()
                .any(|d| d.name.as_deref() == Some("From Google Ads")),
        });
    }
    if !parsed.records.is_empty()
        && !parsed
            .records
            .iter()
            .any(|r| r.title.starts_with("Watched "))
    {
        parsed.warnings.push(
            "no history entry begins with `Watched `: the archive looks localized, so every \
             play reads as action `other` with its verb left in its title; sss reads English \
             archives (see docs/youtube-takeout.md)"
                .to_owned(),
        );
    }
    Ok(parsed)
}

/// A CSV file read with every field trimmed: its header names and rows.
struct Table {
    headers: Vec<String>,
    rows: Vec<csv::StringRecord>,
}

impl Table {
    fn read(bytes: &[u8]) -> Result<Self> {
        let mut reader = csv::ReaderBuilder::new()
            .trim(csv::Trim::All)
            .flexible(true)
            .from_reader(bytes);
        let headers = reader
            .headers()
            .context("reading the header")?
            .iter()
            .map(|h| h.trim_start_matches('\u{feff}').to_owned())
            .collect();
        let rows = reader
            .records()
            .collect::<Result<_, _>>()
            .context("reading a row")?;
        Ok(Self { headers, rows })
    }

    fn column(&self, name: &str) -> Option<usize> {
        self.headers.iter().position(|h| h == name)
    }

    /// The header, as the file's first line.
    fn first_line(&self) -> String {
        self.headers.join(",")
    }
}

/// The field at `col` of `row`; empty when either is absent.
fn field(row: &csv::StringRecord, col: Option<usize>) -> &str {
    col.and_then(|c| row.get(c)).unwrap_or("")
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_owned())
}

/// `subscriptions.csv`. A row whose channel id is malformed is skipped.
pub fn parse_subscriptions(bytes: &[u8]) -> Result<Parsed<SubscriptionRecord>> {
    let table = Table::read(bytes)?;
    let Some(id) = table.column("Channel Id") else {
        bail!("unknown header: {}", table.first_line());
    };
    let (url, title) = (table.column("Channel Url"), table.column("Channel Title"));
    let mut parsed = Parsed::new();
    for row in &table.rows {
        let channel_id = field(row, Some(id));
        if !is_channel_id(channel_id) {
            parsed.skipped += 1;
            continue;
        }
        parsed.records.push(SubscriptionRecord {
            channel_id: channel_id.to_owned(),
            url: non_empty(field(row, url)),
            name: field(row, title).to_owned(),
        });
    }
    Ok(parsed)
}

/// The music library songs file. A row whose video id is malformed is
/// skipped.
pub fn parse_library(bytes: &[u8]) -> Result<Parsed<LibrarySong>> {
    let table = Table::read(bytes)?;
    let Some(id) = table.column("Video ID") else {
        bail!("unknown header: {}", table.first_line());
    };
    let (title, album) = (table.column("Song Title"), table.column("Album Title"));
    let mut artist_columns: Vec<(u32, usize)> = table
        .headers
        .iter()
        .enumerate()
        .filter_map(|(i, h)| Some((h.strip_prefix("Artist Name ")?.parse().ok()?, i)))
        .collect();
    artist_columns.sort_unstable();
    let mut parsed = Parsed::new();
    for row in &table.rows {
        let video_id = field(row, Some(id));
        if !is_video_id(video_id) {
            parsed.skipped += 1;
            continue;
        }
        parsed.records.push(LibrarySong {
            video_id: video_id.to_owned(),
            title: non_empty(field(row, title)),
            album: non_empty(field(row, album)),
            artists: artist_columns
                .iter()
                .filter_map(|&(_, c)| non_empty(field(row, Some(c))))
                .collect(),
        });
    }
    Ok(parsed)
}

/// One items file: its rows, and how many were skipped for a malformed id
/// or an unparsable time.
fn parse_items(bytes: &[u8]) -> Result<(Vec<PlaylistItem>, u64)> {
    let table = Table::read(bytes)?;
    let Some(id) = table.column("Video ID") else {
        bail!("unknown header: {}", table.first_line());
    };
    let added = table.column("Playlist Video Creation Timestamp");
    let (mut items, mut skipped) = (Vec::new(), 0);
    for row in &table.rows {
        let video_id = field(row, Some(id));
        let added_at = match field(row, added) {
            "" => None,
            t => match canonical_time(t) {
                Some(t) => Some(t),
                None => {
                    skipped += 1;
                    continue;
                }
            },
        };
        if !is_video_id(video_id) {
            skipped += 1;
            continue;
        }
        items.push(PlaylistItem {
            video_id: video_id.to_owned(),
            added_at,
        });
    }
    Ok((items, skipped))
}

/// `playlists.csv` and its items files, given as `(file name, contents)`.
/// Each row takes the items of the one file whose name folds to the row's
/// [`file_key`]; a key two rows share, or two files share, attaches no
/// file. An items file no row names is a playlist titled after it.
pub fn parse_playlists(
    index: &[u8],
    files: &[(String, Vec<u8>)],
) -> Result<Parsed<PlaylistRecord>> {
    let table = Table::read(index).with_context(|| PLAYLISTS_FILE.to_owned())?;
    let (Some(id), Some(title)) = (
        table.column("Playlist ID"),
        table.column("Playlist Title (Original)"),
    ) else {
        bail!("{PLAYLISTS_FILE}: unknown header: {}", table.first_line());
    };
    let created = table.column("Playlist Create Timestamp");
    let updated = table.column("Playlist Update Timestamp");
    let order = table.column("Playlist Video Order");
    let visibility = table.column("Playlist Visibility");
    let mut parsed = Parsed::new();

    let mut by_key: BTreeMap<String, Vec<(&str, Vec<PlaylistItem>)>> = BTreeMap::new();
    let mut files: Vec<&(String, Vec<u8>)> = files.iter().collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, bytes) in files {
        let Some(base) = name.strip_suffix(ITEMS_SUFFIX) else {
            continue;
        };
        let (items, skipped) = parse_items(bytes).with_context(|| name.clone())?;
        parsed.skipped += skipped;
        by_key
            .entry(file_key(base))
            .or_default()
            .push((base, items));
    }

    let mut rows = Vec::new();
    for row in &table.rows {
        let playlist_id = field(row, Some(id));
        if !is_playlist_id(playlist_id) {
            parsed.skipped += 1;
            continue;
        }
        rows.push((playlist_id, field(row, Some(title)), row));
    }
    let mut rows_per_key: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, (_, title, _)) in rows.iter().enumerate() {
        rows_per_key.entry(file_key(title)).or_default().push(i);
    }

    for (i, (playlist_id, title, row)) in rows.iter().enumerate() {
        let key = file_key(title);
        let sharers = &rows_per_key[&key];
        let (items_source, items) = match by_key.get(&key).map(Vec::as_slice) {
            None | Some([]) => {
                parsed.warnings.push(format!(
                    "playlist {title:?} ({playlist_id}) has no items file; read as empty"
                ));
                (ItemsSource::None, Vec::new())
            }
            Some([(_, items)]) if sharers.len() == 1 => (ItemsSource::File, items.clone()),
            Some(found) => {
                // One warning per shared name, at its first row.
                if sharers[0] == i {
                    let names: Vec<String> = sharers
                        .iter()
                        .map(|&j| format!("{:?} ({})", rows[j].1, rows[j].0))
                        .collect();
                    let files: Vec<String> = found
                        .iter()
                        .map(|(base, _)| format!("{base}{ITEMS_SUFFIX}"))
                        .collect();
                    parsed.warnings.push(format!(
                        "playlist items file name shared: {} by {}; no items are attached",
                        files.join(", "),
                        names.join(", ")
                    ));
                }
                (ItemsSource::AmbiguousTitle, Vec::new())
            }
        };
        parsed.records.push(PlaylistRecord {
            playlist_id: Some((*playlist_id).to_owned()),
            title: (*title).to_owned(),
            created: canonical_time(field(row, created)),
            updated: canonical_time(field(row, updated)),
            order: non_empty(field(row, order)),
            visibility: non_empty(field(row, visibility)),
            items_source,
            items,
        });
    }

    for (key, found) in &by_key {
        if rows_per_key.contains_key(key) {
            continue;
        }
        for (base, items) in found {
            parsed.records.push(PlaylistRecord {
                playlist_id: None,
                title: base.trim().to_owned(),
                created: None,
                updated: None,
                order: None,
                visibility: None,
                items_source: ItemsSource::File,
                items: items.clone(),
            });
        }
    }
    Ok(parsed)
}

/// The archive's files found under a directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    pub history: Option<PathBuf>,
    /// Only looked at when there is no JSON history.
    pub history_html: Option<PathBuf>,
    pub playlists: Option<PathBuf>,
    /// The `*-videos.csv` files beside `playlists.csv`, by name.
    pub playlist_items: Vec<PathBuf>,
    pub subscriptions: Option<PathBuf>,
    pub library: Option<PathBuf>,
}

impl Found {
    /// Each file to copy, with its path in the run.
    fn copies(&self) -> Vec<(String, PathBuf)> {
        let named = |dir: &str, path: &Path| {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            (format!("{dir}/{name}"), path.to_owned())
        };
        let mut copies = Vec::new();
        copies.extend(self.history.iter().map(|p| named(HISTORY_DIR, p)));
        copies.extend(self.playlists.iter().map(|p| named(PLAYLISTS_DIR, p)));
        copies.extend(self.playlist_items.iter().map(|p| named(PLAYLISTS_DIR, p)));
        copies.extend(
            self.subscriptions
                .iter()
                .map(|p| named(SUBSCRIPTIONS_DIR, p)),
        );
        copies.extend(self.library.iter().map(|p| named(LIBRARY_DIR, p)));
        copies
    }
}

/// Finds the archive's files anywhere under `dir`, by file name. Fails with
/// kind `input` when `dir` holds none of them, holds the history only as
/// HTML, or holds one of them twice.
pub fn locate(dir: &Path) -> Result<Found> {
    if !dir.is_dir() {
        fail!(Kind::Input, "{} is not a directory", dir.display());
    }
    let mut by_name: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    walk(dir, &mut by_name)?;
    let one = |names: &[&str]| -> Result<Option<PathBuf>> {
        let paths: Vec<&PathBuf> = names
            .iter()
            .flat_map(|n| by_name.get(*n))
            .flatten()
            .collect();
        match paths.as_slice() {
            [] => Ok(None),
            [path] => Ok(Some((*path).clone())),
            many => {
                let listed: Vec<String> = many.iter().map(|p| p.display().to_string()).collect();
                fail!(
                    Kind::Input,
                    "{} found {} times under {}: {}; extract one Takeout archive into its own directory",
                    names.join(" or "),
                    many.len(),
                    dir.display(),
                    listed.join(", ")
                )
            }
        }
    };
    let mut found = Found {
        history: one(&[HISTORY_FILE])?,
        history_html: one(&[HISTORY_HTML_FILE])?,
        playlists: one(&[PLAYLISTS_FILE])?,
        playlist_items: Vec::new(),
        subscriptions: one(&[SUBSCRIPTIONS_FILE])?,
        library: one(&LIBRARY_FILES)?,
    };
    if let Some(parent) = found.playlists.as_deref().and_then(Path::parent) {
        found.playlist_items = items_files(parent)?;
    }
    if found.history.is_none() && found.history_html.is_some() {
        fail!(
            Kind::Input,
            "the archive has the history in HTML; request Takeout again with YouTube history \
             in JSON (Multiple formats → History → JSON), see docs/youtube-takeout.md"
        );
    }
    if found.history.is_none()
        && found.playlists.is_none()
        && found.subscriptions.is_none()
        && found.library.is_none()
    {
        fail!(
            Kind::Input,
            "no YouTube Takeout files under {}; expected {HISTORY_FILE}, {PLAYLISTS_FILE}, \
             {SUBSCRIPTIONS_FILE} or {}",
            dir.display(),
            LIBRARY_FILES[0]
        );
    }
    Ok(found)
}

/// Records, by name, every regular file under `dir` whose name the archive
/// uses. Symbolic links are not followed.
fn walk(dir: &Path, by_name: &mut BTreeMap<String, Vec<PathBuf>>) -> Result<()> {
    let wanted = |n: &str| {
        [
            HISTORY_FILE,
            HISTORY_HTML_FILE,
            PLAYLISTS_FILE,
            SUBSCRIPTIONS_FILE,
        ]
        .contains(&n)
            || LIBRARY_FILES.contains(&n)
    };
    for (path, kind) in sorted_entries(dir)? {
        if kind.is_dir() {
            walk(&path, by_name)?;
        } else if kind.is_file()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
            && wanted(name)
        {
            by_name.entry(name.to_owned()).or_default().push(path);
        }
    }
    Ok(())
}

/// The `*-videos.csv` regular files in `dir`, by name.
fn items_files(dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(sorted_entries(dir)?
        .into_iter()
        .filter(|(path, kind)| {
            kind.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(ITEMS_SUFFIX))
        })
        .map(|(path, _)| path)
        .collect())
}

fn sorted_entries(dir: &Path) -> Result<Vec<(PathBuf, fs::FileType)>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let kind = entry
            .file_type()
            .with_context(|| format!("reading {}", entry.path().display()))?;
        entries.push((entry.path(), kind));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

// ---------------------------------------------------------------------------
// The run and its manifest

/// The parse steps, in ledger order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    History,
    Playlists,
    Subscriptions,
    Library,
}

impl Step {
    pub const ALL: [Step; 4] = [
        Step::History,
        Step::Playlists,
        Step::Subscriptions,
        Step::Library,
    ];

    /// The step's key in the manifest.
    pub fn name(self) -> &'static str {
        match self {
            Step::History => "history",
            Step::Playlists => "playlists",
            Step::Subscriptions => "subscriptions",
            Step::Library => "library",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Ok,
    /// The archive had no such file.
    Missing,
    /// The file was there and did not parse.
    Failed,
}

/// One step's line in the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepResult {
    pub status: StepStatus,
    /// Records parsed.
    pub count: u64,
    /// Rows and records dropped for an invalid id or time.
    pub skipped: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl StepResult {
    fn missing() -> Self {
        Self {
            status: StepStatus::Missing,
            count: 0,
            skipped: 0,
            error: None,
        }
    }

    /// The ledger line of `step`.
    pub fn line(&self, step: Step) -> String {
        let name = step.name();
        match self.status {
            StepStatus::Ok => format!(
                "{name}: ok, {} records, {} skipped",
                self.count, self.skipped
            ),
            StepStatus::Missing => format!("{name}: missing from the archive"),
            StepStatus::Failed => format!(
                "{name}: failed: {}",
                self.error.as_deref().unwrap_or("no reason recorded")
            ),
        }
    }
}

/// The ledger, one entry per [`Step`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Steps {
    pub history: StepResult,
    pub playlists: StepResult,
    pub subscriptions: StepResult,
    pub library: StepResult,
}

impl Steps {
    pub fn get(&self, step: Step) -> &StepResult {
        match step {
            Step::History => &self.history,
            Step::Playlists => &self.playlists,
            Step::Subscriptions => &self.subscriptions,
            Step::Library => &self.library,
        }
    }

    /// The steps that failed, in ledger order.
    pub fn failed(&self) -> Vec<Step> {
        Step::ALL
            .into_iter()
            .filter(|&s| self.get(s).status == StepStatus::Failed)
            .collect()
    }

    /// No step failed and the history, the one essential file, was read.
    pub fn complete(&self) -> bool {
        self.failed().is_empty() && self.history.status == StepStatus::Ok
    }
}

/// One copied file: its path in the run, size and SHA-256 (lowercase hex).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

/// `youtube/raw/<run-id>/manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub run_id: String,
    /// RFC 3339 UTC.
    pub started: String,
    /// RFC 3339 UTC.
    pub finished: String,
    /// The directory the archive was imported from.
    pub source_dir: PathBuf,
    pub files: Vec<FileEntry>,
    pub steps: Steps,
    /// See [`Steps::complete`].
    pub complete: bool,
}

/// What the parse of a run's copies produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parse {
    pub archive: Archive,
    pub steps: Steps,
    pub warnings: Vec<String>,
}

/// Parses the copies `files` lists in the run directory `dir`. A step whose
/// file is not listed is `missing`; one whose file does not read or parse
/// is `failed` and contributes no records.
pub fn parse_run(dir: &Path, files: &[FileEntry]) -> Parse {
    let in_dir = |d: &str| -> Vec<&str> {
        files
            .iter()
            .filter(|f| {
                f.path
                    .rsplit_once('/')
                    .is_some_and(|(parent, _)| parent == d)
            })
            .map(|f| f.path.as_str())
            .collect()
    };
    let read = |rel: &str| fs::read(dir.join(rel)).with_context(|| format!("reading {rel}"));
    let index = format!("{PLAYLISTS_DIR}/{PLAYLISTS_FILE}");
    let mut warnings = Vec::new();

    let history_path = format!("{HISTORY_DIR}/{HISTORY_FILE}");
    let (history, history_records) = run_step(
        files
            .iter()
            .any(|f| f.path == history_path)
            .then_some(history_path.as_str()),
        &mut warnings,
        |rel| parse_history(&read(rel)?),
    );
    let playlist_files = in_dir(PLAYLISTS_DIR);
    let (playlists, playlist_records) = run_step(
        playlist_files
            .contains(&index.as_str())
            .then_some(index.as_str()),
        &mut warnings,
        |rel| {
            let mut items = Vec::new();
            for path in playlist_files.iter().filter(|p| **p != rel) {
                let name = path.rsplit_once('/').map_or(*path, |(_, n)| n);
                items.push((name.to_owned(), read(path)?));
            }
            parse_playlists(&read(rel)?, &items)
        },
    );
    let (subscriptions, subscription_records) = run_step(
        in_dir(SUBSCRIPTIONS_DIR).first().copied(),
        &mut warnings,
        |rel| parse_subscriptions(&read(rel)?),
    );
    let (library, library_records) =
        run_step(in_dir(LIBRARY_DIR).first().copied(), &mut warnings, |rel| {
            parse_library(&read(rel)?)
        });
    Parse {
        archive: Archive {
            history: history_records,
            playlists: playlist_records,
            subscriptions: subscription_records,
            library: library_records,
        },
        steps: Steps {
            history,
            playlists,
            subscriptions,
            library,
        },
        warnings,
    }
}

/// Runs `parse` on the copy at `rel`, when there is one.
fn run_step<T>(
    rel: Option<&str>,
    warnings: &mut Vec<String>,
    parse: impl FnOnce(&str) -> Result<Parsed<T>>,
) -> (StepResult, Vec<T>) {
    let Some(rel) = rel else {
        return (StepResult::missing(), Vec::new());
    };
    match parse(rel).with_context(|| rel.to_owned()) {
        Ok(parsed) => {
            warnings.extend(parsed.warnings);
            let result = StepResult {
                status: StepStatus::Ok,
                count: parsed.records.len() as u64,
                skipped: parsed.skipped,
                error: None,
            };
            (result, parsed.records)
        }
        Err(e) => (
            StepResult {
                status: StepStatus::Failed,
                count: 0,
                skipped: 0,
                error: Some(format!("{e:#}")),
            },
            Vec::new(),
        ),
    }
}

/// An import run that ended: its directory and manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    pub run_dir: PathBuf,
    pub manifest: Manifest,
}

/// Files the Takeout archive under `source` as a new raw run of `paths`:
/// every file found is copied byte for byte before any is parsed, then the
/// copies are parsed and the manifest written. The files found, the run
/// directory, parse warnings and the ledger go to `progress`; a failed
/// write to `progress` is ignored. A run that fails to copy a file or to
/// write its manifest is removed, and the error names it.
pub fn import(paths: &ProfilePaths, source: &Path, progress: &mut dyn Write) -> Result<Imported> {
    let found = locate(source)?;
    let copies = found.copies();
    for (_, from) in &copies {
        let _ = writeln!(progress, "found {}", from.display());
    }
    let started = repo::now_rfc3339()?;
    let (run_id, run_dir) = repo::create_run_dir(&paths.youtube_raw_dir(), &repo::run_id_now())?;
    let _ = writeln!(progress, "run {run_id}: {}", run_dir.display());
    match fill_run(source, copies, &run_id, &run_dir, started, progress) {
        Ok(manifest) => Ok(Imported { run_dir, manifest }),
        Err(err) => {
            let removed = match fs::remove_dir_all(&run_dir) {
                Ok(()) => "was removed".to_owned(),
                Err(e) => format!("could not be removed ({e})"),
            };
            Err(err.context(format!(
                "import run {run_id} stopped; its directory {} {removed}",
                run_dir.display()
            )))
        }
    }
}

/// Copies `copies` into `run_dir`, parses the copies and writes the
/// manifest.
fn fill_run(
    source: &Path,
    copies: Vec<(String, PathBuf)>,
    run_id: &str,
    run_dir: &Path,
    started: String,
    progress: &mut dyn Write,
) -> Result<Manifest> {
    let mut files = Vec::with_capacity(copies.len());
    for (rel, from) in copies {
        let bytes = fs::read(&from).with_context(|| format!("reading {}", from.display()))?;
        repo::write_atomic(&run_dir.join(&rel), &bytes)?;
        files.push(FileEntry {
            path: rel,
            bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        });
    }
    let parse = parse_run(run_dir, &files);
    for warning in &parse.warnings {
        let _ = writeln!(progress, "warning: {warning}");
    }
    for step in Step::ALL {
        let _ = writeln!(progress, "{}", parse.steps.get(step).line(step));
    }
    let manifest = Manifest {
        run_id: run_id.to_owned(),
        started,
        finished: repo::now_rfc3339()?,
        source_dir: fs::canonicalize(source).unwrap_or_else(|_| source.to_owned()),
        files,
        complete: parse.steps.complete(),
        steps: parse.steps,
    };
    repo::write_json(&run_dir.join(repo::MANIFEST_FILE), &manifest)?;
    Ok(manifest)
}

/// An import run that is not complete; its copies are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportIncomplete {
    pub run_dir: PathBuf,
    pub manifest: Manifest,
}

impl ImportIncomplete {
    /// The names of the failed steps, in ledger order.
    pub fn failed_steps(&self) -> Vec<&'static str> {
        self.manifest
            .steps
            .failed()
            .iter()
            .map(|s| s.name())
            .collect()
    }
}

impl fmt::Display for ImportIncomplete {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let run_id = &self.manifest.run_id;
        let failed = self.failed_steps();
        if failed.is_empty() {
            write!(
                f,
                "run {run_id} is incomplete: the archive has no {HISTORY_FILE}; request Takeout \
                 again with YouTube history selected, in JSON; its copies are kept"
            )
        } else {
            write!(
                f,
                "run {run_id} is incomplete: failed steps {}; its copies are kept",
                failed.join(", ")
            )
        }
    }
}

impl std::error::Error for ImportIncomplete {}

/// What `sss youtube import` reports for a complete run, normalized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    pub profile: String,
    pub run_id: String,
    pub run_dir: PathBuf,
    pub manifest: Manifest,
    pub normalized: Summary,
}

impl ImportReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        writeln!(
            out,
            "run {} complete: {}",
            self.run_id,
            self.run_dir.display()
        )?;
        for step in Step::ALL {
            writeln!(out, "{}", self.manifest.steps.get(step).line(step))?;
        }
        self.normalized.write_text(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::Repo;
    use serde_json::json;

    const VIDEO: &str = "abcDEF12345";
    const OTHER: &str = "-xyzXYZ0987";
    const PLAYLIST: &str = "PLaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PLAYLIST_2: &str = "PLbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CHANNEL: &str = "UCcccccccccccccccccccccc";

    fn watch(host: &str, id: &str) -> String {
        format!("https://{host}/watch?v={id}")
    }

    #[test]
    fn ids_are_checked_by_shape() {
        assert!(is_video_id(VIDEO) && is_video_id(OTHER));
        assert!(!is_video_id("abcDEF123456") && !is_video_id("abcDEF1234!"));
        assert!(!is_video_id(" -xyzXYZ0987"));
        assert!(is_playlist_id(PLAYLIST));
        assert!(!is_playlist_id(&PLAYLIST[..33]) && !is_playlist_id(&CHANNEL.replace("UC", "PL")));
        assert!(is_channel_id(CHANNEL) && !is_channel_id(&CHANNEL[..23]));
        assert_eq!(
            channel_id_from_url(&format!("http://www.youtube.com/channel/{CHANNEL}")),
            Some(CHANNEL.to_owned())
        );
        assert_eq!(
            channel_id_from_url("https://www.youtube.com/@someone"),
            None
        );
    }

    #[test]
    fn times_take_one_canonical_form() {
        for (given, want) in [
            ("2026-01-02T03:04:05.678Z", "2026-01-02T03:04:05.678Z"),
            ("2026-01-02T03:04:05Z", "2026-01-02T03:04:05.000Z"),
            (
                "2026-01-02T03:04:05.123456789+00:00",
                "2026-01-02T03:04:05.123Z",
            ),
            ("2026-01-02T05:04:05+02:00", "2026-01-02T03:04:05.000Z"),
        ] {
            assert_eq!(canonical_time(given).as_deref(), Some(want), "{given}");
        }
        assert_eq!(canonical_time("yesterday"), None);
        assert_eq!(canonical_time(""), None);
    }

    #[test]
    fn file_keys_fold_forbidden_characters_space_and_case() {
        assert_eq!(file_key("Mix: 2024"), "mix_ 2024");
        assert_eq!(file_key("Mix/ 2024"), file_key("mix_ 2024"));
        assert_eq!(file_key("Rock 'n' Roll"), "rock _n_ roll");
        assert_eq!(file_key(r#"a\b*c?d"e<f>g|h"#), "a_b_c_d_e_f_g_h");
        assert_eq!(file_key(" Favorites "), file_key("favorites"));
    }

    #[test]
    fn history_records_of_every_shape() {
        let removed = watch("music.youtube.com", OTHER);
        let doc = json!([
            {"header": "YouTube Music", "title": "Watched Calm Waters",
             "titleUrl": watch("music.youtube.com", VIDEO),
             "subtitles": [{"name": "Chill Out Radio - Topic",
                            "url": format!("https://www.youtube.com/channel/{CHANNEL}")}],
             "time": "2026-01-02T03:04:05.678Z", "products": ["YouTube"]},
            {"title": "Watched Calm Waters", "titleUrl": watch("www.youtube.com", VIDEO),
             "time": "2026-01-02T03:04:05Z"},
            {"header": "YouTube Music", "title": format!("Watched {removed}"),
             "titleUrl": removed, "time": "2026-01-03T00:00:00.000Z"},
            {"header": "YouTube", "title": "Watched An Advert", "titleUrl": watch("www.youtube.com", VIDEO),
             "details": [{"name": "From Google Ads"}], "description": "ad text",
             "time": "2026-01-04T00:00:00.000Z"},
            {"header": "YouTube", "title": "Viewed A post",
             "titleUrl": "https://www.youtube.com/post/Ugkx123", "time": "2026-01-05T00:00:00Z"},
            {"header": "YouTube", "title": "Answered survey question", "time": "2026-01-06T00:00:00Z"},
            {"header": "YouTube", "title": "Visited Example",
             "titleUrl": "https://www.google.com/url?q=https://example.com", "time": "2026-01-07T00:00:00Z"},
            {"header": "YouTube", "title": "Watched bad", "titleUrl": watch("www.youtube.com", "short"),
             "time": "2026-01-08T00:00:00Z"},
            {"header": "YouTube", "title": "Watched no time", "titleUrl": watch("www.youtube.com", VIDEO)},
            {"header": "YouTube", "title": "Watched bad time", "titleUrl": watch("www.youtube.com", VIDEO),
             "time": "last week"},
            {"header": 7, "title": null, "titleUrl": watch("www.youtube.com", VIDEO),
             "subtitles": [{"name": null, "url": 3}], "details": "x",
             "time": "2026-01-09T00:00:00Z"},
            "not a record",
            {"header": "YouTube", "title": "Watched number time", "time": 20260110}
        ]);
        let parsed = parse_history(doc.to_string().as_bytes()).unwrap();
        assert_eq!(parsed.skipped, 5);
        let r = &parsed.records;
        assert_eq!(r.len(), 8);
        assert_eq!(r[0].video_id.as_deref(), Some(VIDEO));
        assert_eq!(r[0].header.as_deref(), Some("YouTube Music"));
        assert_eq!(
            r[0].channel.as_ref().unwrap().name,
            "Chill Out Radio - Topic"
        );
        assert!(!r[0].is_withheld() && !r[0].ad);
        assert_eq!(
            (r[1].header.as_deref(), r[1].time.as_str()),
            (None, "2026-01-02T03:04:05.000Z")
        );
        assert_eq!(r[1].video_id.as_deref(), Some(VIDEO));
        assert!(r[2].is_withheld());
        assert_eq!(r[2].video_id.as_deref(), Some(OTHER));
        assert!(r[3].ad);
        assert_eq!(r[4].video_id, None);
        assert_eq!(
            (r[5].video_id.as_ref(), r[5].title_url.as_ref()),
            (None, None)
        );
        assert_eq!(r[6].video_id, None);
        let mistyped = &r[7];
        assert_eq!(mistyped.video_id.as_deref(), Some(VIDEO));
        assert_eq!(
            (mistyped.header.as_deref(), mistyped.title.as_str()),
            (None, "")
        );
        let channel = mistyped.channel.as_ref().unwrap();
        assert_eq!((channel.name.as_str(), channel.url.as_deref()), ("", None));
        assert!(!mistyped.ad);
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        assert!(parse_history(b"{}").is_err());
    }

    #[test]
    fn a_history_with_no_watched_entry_warns_of_a_localized_archive() {
        let doc = json!([
            {"header": "YouTube Music", "title": "Angesehen: Calm Waters",
             "titleUrl": watch("music.youtube.com", VIDEO), "time": "2026-01-02T03:04:05Z"},
            {"header": "YouTube", "title": "Viewed A post",
             "titleUrl": "https://www.youtube.com/post/Ugkx123", "time": "2026-01-05T00:00:00Z"}
        ]);
        let parsed = parse_history(doc.to_string().as_bytes()).unwrap();
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.warnings.len(), 1);
        assert!(
            parsed.warnings[0].contains("looks localized"),
            "{:?}",
            parsed.warnings
        );
        assert!(parse_history(b"[]").unwrap().warnings.is_empty());
    }

    #[test]
    fn subscriptions_are_trimmed_and_checked() {
        let csv = format!(
            "Channel Id,Channel Url,Channel Title\n\
             {CHANNEL},http://www.youtube.com/channel/{CHANNEL},Chill Out Radio \n\
             UCdddddddddddddddddddddd,,\n\
             UCshort,x,Bad\n\n\n\n\n\n"
        );
        let parsed = parse_subscriptions(csv.as_bytes()).unwrap();
        assert_eq!(parsed.skipped, 1);
        assert_eq!(
            parsed.records,
            [
                SubscriptionRecord {
                    channel_id: CHANNEL.into(),
                    url: Some(format!("http://www.youtube.com/channel/{CHANNEL}")),
                    name: "Chill Out Radio".into(),
                },
                SubscriptionRecord {
                    channel_id: "UCdddddddddddddddddddddd".into(),
                    url: None,
                    name: String::new(),
                },
            ]
        );
        assert!(parse_subscriptions(b"Id,Title\nx,y\n").is_err());
    }

    #[test]
    fn library_songs_collect_every_artist_column_in_order() {
        let mut header = String::from("Video ID,Song Title,Album Title");
        for n in [1, 2, 3, 10, 11, 4, 5, 6, 7, 8, 9] {
            header.push_str(&format!(",Artist Name {n}"));
        }
        let csv = format!(
            "{header}\n\
             \" {OTHER}\",\"Calm Waters, Pt. 2\",Quiet Album,Lead Act,Second,,J,K,D,,,,,\n\
             {VIDEO},Song,,Solo,,,,,,,,,,\n\
             abcDEF123456,Too Long,,X,,,,,,,,,,\n\n\n\n\n\n"
        );
        let parsed = parse_library(csv.as_bytes()).unwrap();
        assert_eq!(parsed.skipped, 1);
        assert_eq!(
            parsed.records[0],
            LibrarySong {
                video_id: OTHER.into(),
                title: Some("Calm Waters, Pt. 2".into()),
                album: Some("Quiet Album".into()),
                artists: vec![
                    "Lead Act".into(),
                    "Second".into(),
                    "D".into(),
                    "J".into(),
                    "K".into()
                ],
            }
        );
        assert_eq!(parsed.records[1].album, None);
        assert_eq!(parsed.records[1].artists, ["Solo"]);
    }

    const INDEX_HEADER: &str = "Playlist ID,Add new videos to top,Playlist Image 1 Height,\
        Playlist Image 1 Width,Playlist Title (Original),Playlist Title (Original) Language,\
        Playlist Description (Original),Playlist Image 2 Height,Playlist Create Timestamp,\
        Playlist Update Timestamp,Playlist Video Order,Playlist Visibility";

    fn index_row(id: &str, title: &str) -> String {
        format!(
            "{id},False,,,\"{title}\",,,,2025-01-01T00:00:00.123456789+00:00,\
             2025-02-01T00:00:00+00:00,Manual,Private"
        )
    }

    fn items(rows: &[&str]) -> Vec<u8> {
        let mut s = String::from("Video ID,Playlist Video Creation Timestamp\n");
        for r in rows {
            s.push_str(r);
            s.push('\n');
        }
        s.push_str("\n\n\n\n\n");
        s.into_bytes()
    }

    #[test]
    fn playlists_take_their_items_file_in_csv_order() {
        let index = format!(
            "{INDEX_HEADER}\n{}\n{}\nPLshort,False,,,Bad,,,,,,,\n\n\n\n\n\n",
            index_row(PLAYLIST, "Road Trip: Mix's"),
            index_row(PLAYLIST_2, "Never Filled"),
        );
        let files = vec![(
            "Road Trip_ Mix_s-videos.csv".to_owned(),
            items(&[
                &format!("{VIDEO},2025-01-03T00:00:00+00:00"),
                &format!(" {OTHER},2025-01-02T00:00:00+00:00"),
                &format!("{VIDEO},2025-01-04T00:00:00+00:00"),
                "tooShort,2025-01-04T00:00:00+00:00",
                &format!("{VIDEO},not a time"),
                &format!("{OTHER},"),
            ]),
        )];
        let parsed = parse_playlists(index.as_bytes(), &files).unwrap();
        assert_eq!(parsed.skipped, 3);
        let road = &parsed.records[0];
        assert_eq!(road.playlist_id.as_deref(), Some(PLAYLIST));
        assert_eq!(road.title, "Road Trip: Mix's");
        assert_eq!(road.created.as_deref(), Some("2025-01-01T00:00:00.123Z"));
        assert_eq!(road.order.as_deref(), Some("Manual"));
        assert_eq!(road.visibility.as_deref(), Some("Private"));
        assert_eq!(road.items_source, ItemsSource::File);
        let ids: Vec<&str> = road.items.iter().map(|i| i.video_id.as_str()).collect();
        assert_eq!(ids, [VIDEO, OTHER, VIDEO, OTHER]);
        assert_eq!(road.items[3].added_at, None);
        let empty = &parsed.records[1];
        assert_eq!(
            (empty.items_source, empty.items.len()),
            (ItemsSource::None, 0)
        );
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.warnings.len(), 1, "{:?}", parsed.warnings);
    }

    #[test]
    fn a_shared_items_file_name_attaches_to_no_playlist() {
        for (a, b, file) in [
            ("Favorites", "Favorites", "Favorites"),
            ("Mix: 2024", "Mix/ 2024", "Mix_ 2024"),
            ("Favorites", "favorites", "Favorites"),
        ] {
            let index = format!(
                "{INDEX_HEADER}\n{}\n{}\n",
                index_row(PLAYLIST, a),
                index_row(PLAYLIST_2, b)
            );
            let files = vec![(format!("{file}-videos.csv"), items(&[VIDEO]))];
            let parsed = parse_playlists(index.as_bytes(), &files).unwrap();
            assert_eq!(parsed.records.len(), 2, "{a} / {b}");
            for r in &parsed.records {
                assert_eq!(r.items_source, ItemsSource::AmbiguousTitle, "{a} / {b}");
                assert!(r.items.is_empty());
            }
            assert_eq!(parsed.warnings.len(), 1, "{:?}", parsed.warnings);
            assert!(
                parsed.warnings[0].contains(PLAYLIST_2),
                "{}",
                parsed.warnings[0]
            );
        }
    }

    #[test]
    fn an_items_file_no_row_names_is_its_own_playlist() {
        let index = format!("{INDEX_HEADER}\n");
        let files = vec![("Old List-videos.csv".to_owned(), items(&[VIDEO]))];
        let parsed = parse_playlists(index.as_bytes(), &files).unwrap();
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].playlist_id, None);
        assert_eq!(parsed.records[0].title, "Old List");
        assert_eq!(parsed.records[0].items_source, ItemsSource::File);
    }

    #[test]
    fn columns_are_read_by_name_whatever_their_order() {
        let index = format!(
            "Playlist Visibility,Playlist Title (Original),Playlist Image 1 Height,Playlist ID\n\
             Public,Swapped,1,{PLAYLIST}\n"
        );
        let parsed = parse_playlists(index.as_bytes(), &[]).unwrap();
        assert_eq!(parsed.records[0].title, "Swapped");
        assert_eq!(parsed.records[0].visibility.as_deref(), Some("Public"));
    }

    #[test]
    fn an_unknown_header_fails_naming_the_first_line() {
        let err = parse_playlists(b"Playlist,Something\nx,y\n", &[]).unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown header: Playlist,Something"),
            "{err:#}"
        );
        let index = format!("{INDEX_HEADER}\n{}\n", index_row(PLAYLIST, "Old"));
        let old = b"Playlist Name,Old\nVideo ID,Time Added\n".to_vec();
        let err = parse_playlists(index.as_bytes(), &[("Old-videos.csv".into(), old)]).unwrap_err();
        assert!(
            format!("{err:#}").contains("Old-videos.csv: unknown header: Playlist Name,Old"),
            "{err:#}"
        );
    }

    /// Writes `files` (path relative to `root`, contents) under `root`.
    fn lay_out(root: &Path, files: &[(&str, &[u8])]) {
        for (rel, bytes) in files {
            let path = root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
    }

    const BASE: &str = "Takeout/YouTube and YouTube Music";

    fn history_json() -> Vec<u8> {
        json!([{"header": "YouTube Music", "title": "Watched Calm Waters",
                "titleUrl": watch("music.youtube.com", VIDEO), "time": "2026-01-02T03:04:05.678Z"},
               {"header": "YouTube Music", "title": "Watched later",
                "titleUrl": watch("music.youtube.com", VIDEO), "time": "garbled"}])
        .to_string()
        .into_bytes()
    }

    fn archive(root: &Path) {
        let index = format!("{INDEX_HEADER}\n{}\n", index_row(PLAYLIST, "Road Trip"));
        lay_out(
            root,
            &[
                (
                    &format!("{BASE}/history/watch-history.json"),
                    &history_json(),
                ),
                (&format!("{BASE}/history/search-history.json"), b"[]"),
                (&format!("{BASE}/playlists/playlists.csv"), index.as_bytes()),
                (
                    &format!("{BASE}/playlists/Road Trip-videos.csv"),
                    &items(&[VIDEO]),
                ),
                (
                    &format!("{BASE}/subscriptions/subscriptions.csv"),
                    format!("Channel Id,Channel Url,Channel Title\n{CHANNEL},,Radio\n").as_bytes(),
                ),
                (
                    &format!("{BASE}/music (library and uploads)/music library songs.csv"),
                    format!("Video ID,Song Title,Album Title,Artist Name 1\n{VIDEO},Calm,,Act\n")
                        .as_bytes(),
                ),
                (
                    &format!("{BASE}/video metadata/videos.csv"),
                    b"Video ID\nabcDEF12345\n",
                ),
            ],
        );
    }

    fn profile(tmp: &Path) -> ProfilePaths {
        Repo::at(tmp.join("data")).profile("alice").unwrap()
    }

    #[test]
    fn import_copies_verbatim_then_parses_the_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        archive(&src);
        let paths = profile(tmp.path());
        let mut progress = Vec::new();
        let imported = import(&paths, &src, &mut progress).unwrap();
        let progress = String::from_utf8(progress).unwrap();
        let m = &imported.manifest;
        assert!(m.complete, "{m:?}");
        let copied: Vec<&str> = m.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            copied,
            [
                "takeout/history/watch-history.json",
                "takeout/playlists/playlists.csv",
                "takeout/playlists/Road Trip-videos.csv",
                "takeout/subscriptions/subscriptions.csv",
                "takeout/music (library and uploads)/music library songs.csv",
            ]
        );
        let history = history_json();
        assert_eq!(fs::read(imported.run_dir.join(copied[0])).unwrap(), history);
        assert_eq!(m.files[0].bytes, history.len() as u64);
        assert_eq!(m.files[0].sha256, format!("{:x}", Sha256::digest(&history)));
        assert_eq!(m.files[0].sha256.len(), 64);
        assert_eq!(
            (m.steps.history.count, m.steps.history.skipped),
            (1, 1),
            "{:?}",
            m.steps.history
        );
        assert_eq!(m.steps.playlists.count, 1);
        assert_eq!(m.steps.subscriptions.count, 1);
        assert_eq!(m.steps.library.count, 1);
        // A parse of the run's copies reads what the import recorded.
        let parse = parse_run(&imported.run_dir, &m.files);
        assert_eq!(parse.steps, m.steps);
        assert_eq!(parse.archive.playlists[0].items.len(), 1);
        assert_eq!(
            repo::read_manifest::<Manifest>(&imported.run_dir)
                .unwrap()
                .as_ref(),
            Some(m)
        );
        assert_eq!(imported.run_dir, paths.youtube_raw_run_dir(&m.run_id));
        assert!(
            progress.contains("history: ok, 1 records, 1 skipped"),
            "{progress}"
        );
        assert!(progress.contains("found "), "{progress}");
        // The rest of the archive is never copied.
        let mut all = Vec::new();
        collect(&imported.run_dir, &mut all);
        assert_eq!(all.len(), copied.len() + 1, "{all:?}");
    }

    #[test]
    fn a_copy_that_fails_removes_its_run_and_names_it() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        archive(&src);
        let library = src.join(format!(
            "{BASE}/music (library and uploads)/music library songs.csv"
        ));
        let mode = |m| fs::set_permissions(&library, fs::Permissions::from_mode(m));
        mode(0o000).unwrap();
        if fs::read(&library).is_ok() {
            // Running as root: mode bits do not stop the read.
            mode(0o644).unwrap();
            return;
        }
        let paths = profile(tmp.path());
        let err = import(&paths, &src, &mut Vec::new()).unwrap_err();
        mode(0o644).unwrap();
        let message = format!("{err:#}");
        let run_ids = repo::run_ids(&paths.youtube_raw_dir()).unwrap();
        assert!(run_ids.is_empty(), "{run_ids:?}");
        let run_id = message
            .strip_prefix("import run ")
            .and_then(|m| m.split_once(' '))
            .map(|(id, _)| id)
            .unwrap_or_default();
        assert!(!run_id.is_empty(), "{message}");
        let run_dir = paths.youtube_raw_run_dir(run_id);
        assert!(
            message.contains(&format!("its directory {} was removed", run_dir.display())),
            "{message}"
        );
        assert!(message.contains("music library songs.csv"), "{message}");
    }

    fn collect(dir: &Path, found: &mut Vec<PathBuf>) {
        for (path, kind) in sorted_entries(dir).unwrap() {
            if kind.is_dir() {
                collect(&path, found);
            } else {
                found.push(path);
            }
        }
    }

    #[test]
    fn a_partial_archive_is_complete_only_with_its_history() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        lay_out(
            &src,
            &[(
                &format!("{BASE}/subscriptions/subscriptions.csv"),
                b"Channel Id\n",
            )],
        );
        let imported = import(&profile(tmp.path()), &src, &mut Vec::new()).unwrap();
        let steps = &imported.manifest.steps;
        assert!(!imported.manifest.complete);
        assert_eq!(steps.history.status, StepStatus::Missing);
        assert_eq!(steps.subscriptions.status, StepStatus::Ok);
        let incomplete = ImportIncomplete {
            run_dir: imported.run_dir,
            manifest: imported.manifest,
        };
        assert!(incomplete.failed_steps().is_empty());
        assert!(
            incomplete.to_string().contains("no watch-history.json"),
            "{incomplete}"
        );
    }

    #[test]
    fn a_file_that_does_not_parse_fails_its_step() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        lay_out(
            &src,
            &[
                ("history/watch-history.json", b"not json"),
                ("playlists/playlists.csv", b"Who,Knows\n"),
            ],
        );
        let imported = import(&profile(tmp.path()), &src, &mut Vec::new()).unwrap();
        let m = &imported.manifest;
        assert!(!m.complete);
        assert_eq!(m.steps.failed(), [Step::History, Step::Playlists]);
        let error = m.steps.playlists.error.as_deref().unwrap();
        assert!(
            error.starts_with("takeout/playlists/playlists.csv: ") && error.contains("Who,Knows"),
            "{error}"
        );
        assert_eq!(m.files.len(), 2, "copied though they do not parse");
        let incomplete = ImportIncomplete {
            run_dir: imported.run_dir.clone(),
            manifest: m.clone(),
        };
        assert_eq!(incomplete.failed_steps(), ["history", "playlists"]);
        assert_eq!(
            incomplete.to_string(),
            format!(
                "run {} is incomplete: failed steps history, playlists; its copies are kept",
                m.run_id
            )
        );
    }

    #[test]
    fn locating_fails_on_nothing_html_or_doubles() {
        let tmp = tempfile::tempdir().unwrap();
        let input = |dir: &Path| {
            let err = locate(dir).unwrap_err();
            assert_eq!(crate::error::kind(&err), Kind::Input, "{err:#}");
            err.to_string()
        };
        assert!(input(&tmp.path().join("absent")).contains("is not a directory"));
        let empty = tmp.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert!(input(&empty).contains("no YouTube Takeout files under"));

        let html = tmp.path().join("html");
        lay_out(&html, &[("history/watch-history.html", b"<html>")]);
        assert!(input(&html).contains("history in HTML"));
        lay_out(&html, &[("history/watch-history.json", b"[]")]);
        let found = locate(&html).unwrap();
        assert!(found.history.is_some());
        assert!(
            found
                .copies()
                .iter()
                .all(|(rel, _)| !rel.ends_with(".html"))
        );

        let double = tmp.path().join("double");
        lay_out(
            &double,
            &[
                ("a/watch-history.json", b"[]"),
                ("b/watch-history.json", b"[]"),
            ],
        );
        assert!(input(&double).contains("found 2 times"));
    }

    #[test]
    fn items_files_are_only_siblings_of_the_playlist_index() {
        let tmp = tempfile::tempdir().unwrap();
        lay_out(
            tmp.path(),
            &[
                ("p/playlists.csv", b""),
                ("p/A-videos.csv", b""),
                ("p/notes.txt", b""),
                ("elsewhere/B-videos.csv", b""),
                ("old/music-library-songs.csv", b""),
            ],
        );
        let found = locate(tmp.path()).unwrap();
        assert_eq!(found.playlist_items, [tmp.path().join("p/A-videos.csv")]);
        assert_eq!(
            found.library,
            Some(tmp.path().join("old/music-library-songs.csv"))
        );
    }
}
