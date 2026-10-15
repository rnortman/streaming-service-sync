//! The `match` command — which items to match, the search-and-score
//! pipeline over them, and its report — and `match review`, `match set`
//! and `match reject`.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;
use url::Url;

use crate::config::PushConfig;
use crate::error::Kind;
use crate::fail;
use crate::lazy::Lazy;
use crate::matching::candidates::{self, Found, Search};
use crate::matching::llm::{BatchItem, LlmDecision, Matcher, Outcome, Verdict};
use crate::matching::model::{self, ByKind, Candidate, Confidence, MatchEntry, Method, Status};
use crate::matching::score::{Decision, TrackItem};
use crate::pandora::model::{Normalized, SeedKind, Track};
use crate::push::playlist_is_pushed;
use crate::repo::{self, ProfilePaths};
use crate::spotify::ids::{self, IdKind};

/// One Pandora item to match. `name` is a track's title, an artist's or an
/// album's name; `artist` is empty for an artist; `album`, `duration_s`,
/// `explicit_hint`, `isrc` and `playlists` are a track's. `stations` and
/// `context_titles` are what the LLM is told about where the item came
/// from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Item {
    pub pandora_id: String,
    pub name: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_s: Option<u32>,
    pub explicit_hint: bool,
    pub isrc: Option<String>,
    /// Stations whose thumbs or seeds name the item.
    pub stations: Vec<String>,
    /// Pushed playlists holding the track.
    pub playlists: Vec<String>,
    /// For an artist seed: up to [`CONTEXT_TITLES`] thumbed-up titles from
    /// its stations.
    pub context_titles: Vec<String>,
}

/// How many thumbed-up titles an artist seed carries as context.
pub const CONTEXT_TITLES: usize = 5;

/// Adds `value` to `list` unless it is there.
fn push_once(list: &mut Vec<String>, value: &str) {
    if !list.iter().any(|v| v == value) {
        list.push(value.to_owned());
    }
}

impl Item {
    fn from_track(t: &Track) -> Self {
        Item {
            pandora_id: t.pandora_id.clone(),
            name: t.title.clone(),
            artist: t.artist.clone(),
            album: t.album.clone(),
            duration_s: t.duration_s,
            explicit_hint: t.explicit_hint,
            isrc: t.isrc.clone(),
            ..Item::default()
        }
    }

    fn named(pandora_id: &str, name: Option<&String>, artist: Option<&String>) -> Self {
        Item {
            pandora_id: pandora_id.to_owned(),
            name: name.cloned(),
            artist: artist.cloned(),
            ..Item::default()
        }
    }
}

/// The items of each kind, keyed and ordered by Pandora id.
pub type Items = ByKind<BTreeMap<String, Item>>;

/// Inserts `item` unless its id is there already with a name.
fn insert(map: &mut BTreeMap<String, Item>, item: Item) {
    match map.get(&item.pandora_id) {
        Some(have) if have.name.is_some() => {}
        _ => {
            map.insert(item.pandora_id.clone(), item);
        }
    }
}

/// Every item push would send: the tracks of thumbs-up, track seeds,
/// pushed playlists, the collection and bookmarks (thumbs-down tracks only
/// with `[push] thumbs_down_playlist`); artist seeds and collection
/// artists; collection and bookmarked albums. A track missing from
/// `tracks.json` takes its title and artist from its bookmark.
pub fn select(export: &Normalized, push: &PushConfig) -> Items {
    let mut track_ids: Vec<&str> = Vec::new();
    let mut artists = BTreeMap::new();
    for station in &export.stations {
        for f in &station.feedback {
            if f.positive || push.thumbs_down_playlist {
                track_ids.extend(f.track_pandora_id.as_deref());
            }
        }
        for seed in &station.seeds {
            let Some(id) = seed.pandora_id.as_deref() else {
                continue;
            };
            match seed.kind {
                SeedKind::Track => track_ids.push(id),
                SeedKind::Artist => insert(&mut artists, Item::named(id, seed.name.as_ref(), None)),
                SeedKind::Genre | SeedKind::Other => {}
            }
        }
    }
    for playlist in &export.playlists {
        if playlist_is_pushed(playlist, push) {
            track_ids.extend(playlist.items.iter().map(String::as_str));
        }
    }
    track_ids.extend(export.collection.tracks.iter().map(String::as_str));
    track_ids.extend(
        export
            .bookmarks
            .tracks
            .iter()
            .filter_map(|b| b.pandora_id.as_deref()),
    );

    let mut tracks = BTreeMap::new();
    for id in track_ids {
        if tracks.contains_key(id) {
            continue;
        }
        let item = match export.tracks.get(id) {
            Some(t) => Item::from_track(t),
            None => match export
                .bookmarks
                .tracks
                .iter()
                .find(|b| b.pandora_id.as_deref() == Some(id))
            {
                Some(b) => Item::named(id, b.title.as_ref(), b.artist.as_ref()),
                None => Item::named(id, None, None),
            },
        };
        tracks.insert(id.to_owned(), item);
    }

    for a in &export.collection.artists {
        insert(
            &mut artists,
            Item::named(&a.pandora_id, a.name.as_ref(), None),
        );
    }
    let mut albums = BTreeMap::new();
    for a in &export.collection.albums {
        insert(
            &mut albums,
            Item::named(&a.pandora_id, a.name.as_ref(), a.artist.as_ref()),
        );
    }
    for b in &export.bookmarks.albums {
        if let Some(id) = b.pandora_id.as_deref() {
            insert(
                &mut albums,
                Item::named(id, b.title.as_ref(), b.artist.as_ref()),
            );
        }
    }
    let mut items = Items {
        tracks,
        artists,
        albums,
    };
    add_context(&mut items, export, push);
    items
}

/// Fills each item's stations and playlists, and each artist seed's
/// thumbed-up titles.
fn add_context(items: &mut Items, export: &Normalized, push: &PushConfig) {
    for station in &export.stations {
        let Some(name) = station.name.as_deref() else {
            continue;
        };
        let mut named: Vec<&str> = station
            .feedback
            .iter()
            .filter(|f| f.positive || push.thumbs_down_playlist)
            .filter_map(|f| f.track_pandora_id.as_deref())
            .collect();
        named.extend(
            station
                .seeds
                .iter()
                .filter(|s| s.kind == SeedKind::Track)
                .filter_map(|s| s.pandora_id.as_deref()),
        );
        for id in named {
            if let Some(item) = items.tracks.get_mut(id) {
                push_once(&mut item.stations, name);
            }
        }
        let titles: Vec<&str> = station
            .thumbs_up_in_order()
            .into_iter()
            .filter_map(|f| f.track_pandora_id.as_deref())
            .filter_map(|id| export.tracks.get(id).and_then(|t| t.title.as_deref()))
            .collect();
        for seed in station.seeds.iter().filter(|s| s.kind == SeedKind::Artist) {
            let Some(item) = seed
                .pandora_id
                .as_deref()
                .and_then(|id| items.artists.get_mut(id))
            else {
                continue;
            };
            push_once(&mut item.stations, name);
            for title in &titles {
                if item.context_titles.len() >= CONTEXT_TITLES {
                    break;
                }
                push_once(&mut item.context_titles, title);
            }
        }
    }
    for playlist in &export.playlists {
        let Some(name) = playlist.name.as_deref() else {
            continue;
        };
        if !playlist_is_pushed(playlist, push) {
            continue;
        }
        for id in &playlist.items {
            if let Some(item) = items.tracks.get_mut(id) {
                push_once(&mut item.playlists, name);
            }
        }
    }
}

/// What one run did with one kind.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct KindReport {
    /// Items decided this run.
    pub processed: usize,
    pub by_status: BTreeMap<Status, usize>,
    pub by_method: BTreeMap<Method, usize>,
    pub llm_batches: usize,
    pub llm_failed_ids: Vec<String>,
    pub unmatched_ids: Vec<String>,
}

impl KindReport {
    fn count(&mut self, entry: &MatchEntry) {
        self.processed += 1;
        *self.by_status.entry(entry.status).or_default() += 1;
        if let Some(m) = entry.method {
            *self.by_method.entry(m).or_default() += 1;
        }
        match entry.status {
            Status::Unmatched => self.unmatched_ids.push(entry.pandora_id.clone()),
            Status::LlmFailed => self.llm_failed_ids.push(entry.pandora_id.clone()),
            _ => {}
        }
    }
}

pub type KindReports = ByKind<KindReport>;

/// Why a run ended before every item was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stopped {
    /// `--limit` items were decided and more remain.
    Limit,
}

impl Stopped {
    /// The JSON spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Stopped::Limit => "limit",
        }
    }
}

/// What `sss match` reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MatchReport {
    pub profile: String,
    pub kinds: KindReports,
    pub stopped: Option<Stopped>,
}

impl MatchReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        for kind in IdKind::ALL {
            let r = self.kinds.of(kind);
            let mut parts: Vec<String> = r
                .by_status
                .iter()
                .map(|(s, n)| format!("{} {n}", s.as_str()))
                .collect();
            parts.extend(
                r.by_method
                    .iter()
                    .map(|(m, n)| format!("by {} {n}", m.as_str())),
            );
            let detail = if parts.is_empty() {
                String::new()
            } else {
                format!(": {}", parts.join(", "))
            };
            writeln!(out, "{}: {} decided{detail}", kind.plural(), r.processed)?;
        }
        if let Some(s) = self.stopped {
            writeln!(out, "stopped: {}", s.as_str())?;
        }
        Ok(())
    }
}

/// What `sss match` is asked to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// Only this kind; every kind when `None`.
    pub only: Option<IdKind>,
    /// Decide at most this many items.
    pub limit: Option<usize>,
    /// Items per LLM batch (at least one).
    pub batch_size: usize,
}

/// Makes the LLM matcher, once a run first needs it.
pub type ConnectMatcher<'a> = Box<dyn FnOnce() -> Result<Box<dyn Matcher + 'a>> + 'a>;

/// The entry an item with no name (or a track with no artist) gets.
fn nameless(item: &Item, kind: IdKind, at: String) -> MatchEntry {
    MatchEntry {
        pandora_id: item.pandora_id.clone(),
        status: Status::Unmatched,
        spotify_uri: None,
        method: None,
        confidence: None,
        candidates: Vec::new(),
        queries: Vec::new(),
        decided_at: at,
        session: None,
        reason: Some(
            if kind == IdKind::Track {
                "no title"
            } else {
                "no name"
            }
            .into(),
        ),
    }
}

/// The entry for what the queries found.
fn decided(item: &Item, found: Found, at: String) -> MatchEntry {
    let (status, uri, method, confidence) = match found.scored.decision {
        Decision::Auto { uri, method } => (
            Status::Matched,
            Some(uri),
            Some(method),
            Some(Confidence::High),
        ),
        Decision::Ambiguous => (Status::Ambiguous, None, None, None),
        Decision::Unmatched => (Status::Unmatched, None, None, None),
    };
    MatchEntry {
        pandora_id: item.pandora_id.clone(),
        status,
        spotify_uri: uri,
        method,
        confidence,
        candidates: found.scored.candidates,
        queries: found.queries,
        decided_at: at,
        session: None,
        reason: None,
    }
}

/// Searches for `item` and decides it.
fn find(search: &mut dyn Search, kind: IdKind, item: &Item) -> Result<Option<Found>> {
    let Some(name) = item.name.as_deref().filter(|n| !n.trim().is_empty()) else {
        return Ok(None);
    };
    let artist = item.artist.as_deref().unwrap_or_default();
    Ok(Some(match kind {
        IdKind::Track => {
            if artist.trim().is_empty() {
                return Ok(None);
            }
            let track = TrackItem {
                title: name.to_owned(),
                artist: artist.to_owned(),
                album: item.album.clone(),
                duration_s: item.duration_s,
                explicit_hint: item.explicit_hint,
                isrc: item.isrc.clone(),
            };
            candidates::find_track(search, &track)?
        }
        IdKind::Artist => candidates::find_artist(search, name)?,
        IdKind::Album => candidates::find_album(search, name, artist)?,
    }))
}

/// The entry the LLM's `decision` (or a failed batch) makes of `prior`,
/// the item's stored `ambiguous` or `llm_failed` entry: its candidates and
/// queries are kept.
fn llm_entry(
    prior: &MatchEntry,
    session: Option<&str>,
    decision: Result<&LlmDecision, &str>,
    at: String,
) -> MatchEntry {
    let (status, uri, method, confidence, reason) = match decision {
        Ok(d) => (
            if d.choice.is_some() {
                Status::Matched
            } else {
                Status::Unmatched
            },
            d.choice.clone(),
            Some(Method::Llm),
            Some(d.confidence),
            d.reason.clone(),
        ),
        Err(why) => (Status::LlmFailed, None, None, None, Some(why.to_owned())),
    };
    MatchEntry {
        pandora_id: prior.pandora_id.clone(),
        status,
        spotify_uri: uri,
        method,
        confidence,
        candidates: prior.candidates.clone(),
        queries: prior.queries.clone(),
        decided_at: at,
        session: session.map(str::to_owned),
        reason,
    }
}

/// The clients a run makes on first need: the matcher (when the run has
/// one) before the search client, so that a `ccc` that cannot run fails
/// the run before any Spotify request.
struct Clients<'a, S, F> {
    search: Lazy<S, F>,
    matcher: Option<Lazy<Box<dyn Matcher + 'a>, ConnectMatcher<'a>>>,
}

impl<'a, S, F: FnOnce() -> Result<S>> Clients<'a, S, F> {
    /// The matcher, made now if needed; `None` when the run has none.
    fn matcher(&mut self) -> Result<Option<&mut Box<dyn Matcher + 'a>>> {
        self.matcher.as_mut().map(Lazy::get).transpose()
    }

    fn search(&mut self) -> Result<&mut S> {
        self.matcher()?;
        self.search.get()
    }
}

/// `sss match`: decides every item of `items` that has no entry yet, in
/// Pandora-id order per kind, appending each decision as it is made. With
/// a matcher, items left `ambiguous` (now or by an earlier run) and
/// `llm_failed` ones go to it in batches of `batch_size`, from their
/// stored candidates; without one they are left alone. Settled items are
/// never touched. `connect_search` and `connect_matcher` make the clients,
/// only once an item needs them. A failed search, ccc's usage limit or a
/// ccc that cannot run ends the run; the decisions before it are kept.
pub fn run<S: Search>(
    profile: &str,
    paths: &ProfilePaths,
    items: &Items,
    options: Options,
    connect_search: impl FnOnce() -> Result<S>,
    connect_matcher: Option<ConnectMatcher<'_>>,
    progress: &mut dyn Write,
) -> Result<MatchReport> {
    let mut report = MatchReport {
        profile: profile.to_owned(),
        kinds: KindReports::default(),
        stopped: None,
    };
    let llm = connect_matcher.is_some();
    let mut clients = Clients {
        search: Lazy::new(connect_search),
        matcher: connect_matcher.map(Lazy::new),
    };
    let batch_size = options.batch_size.max(1);
    let mut taken = 0;
    for kind in IdKind::ALL {
        if options.only.is_some_and(|o| o != kind) {
            continue;
        }
        let file = model::file(paths, kind);
        let latest = model::read_latest(&file)?;
        let mut queue: Vec<(&Item, MatchEntry)> = Vec::new();
        for item in items.of(kind).values() {
            let stored = match latest.get(&item.pandora_id) {
                None => None,
                Some(e) if llm && e.awaits_llm() => Some(e.clone()),
                Some(_) => continue,
            };
            if options.limit.is_some_and(|n| taken >= n) {
                flush(&mut clients, kind, &mut queue, &file, &mut report, progress)?;
                report.stopped = Some(Stopped::Limit);
                return Ok(report);
            }
            taken += 1;
            let entry = match stored {
                Some(entry) => {
                    clients.matcher()?;
                    entry
                }
                None => {
                    let client = clients.search()?;
                    let at = repo::now_rfc3339()?;
                    let entry = match find(client, kind, item)? {
                        Some(found) => decided(item, found, at),
                        None => nameless(item, kind, at),
                    };
                    model::append(&file, &entry)?;
                    let _ = writeln!(progress, "{}", progress_line(kind, item, &entry));
                    if !(llm && entry.awaits_llm()) {
                        report.kinds.of_mut(kind).count(&entry);
                        continue;
                    }
                    entry
                }
            };
            queue.push((item, entry));
            if queue.len() >= batch_size {
                flush(&mut clients, kind, &mut queue, &file, &mut report, progress)?;
            }
        }
        flush(&mut clients, kind, &mut queue, &file, &mut report, progress)?;
    }
    Ok(report)
}

/// Sends the queued items to the matcher as one batch and appends what it
/// decided for each.
fn flush<S, F: FnOnce() -> Result<S>>(
    clients: &mut Clients<'_, S, F>,
    kind: IdKind,
    queue: &mut Vec<(&Item, MatchEntry)>,
    file: &std::path::Path,
    report: &mut MatchReport,
    progress: &mut dyn Write,
) -> Result<()> {
    if queue.is_empty() {
        return Ok(());
    }
    let Some(matcher) = clients.matcher()? else {
        unreachable!("items are queued only when the run has a matcher")
    };
    let batch: Vec<BatchItem> = queue
        .iter()
        .map(|(item, entry)| BatchItem::new(kind, item, &entry.candidates))
        .collect();
    let _ = writeln!(
        progress,
        "asking the LLM about {} {}...",
        batch.len(),
        kind.plural()
    );
    let Outcome { session, verdict } = matcher.decide(kind, &batch)?;
    report.kinds.of_mut(kind).llm_batches += 1;
    for (item, prior) in queue.drain(..) {
        let decision = match &verdict {
            Verdict::Decided(all) => match all.iter().find(|d| d.item == item.pandora_id) {
                Some(d) => Ok(d),
                None => Err("the LLM gave no decision"),
            },
            Verdict::Failed(why) => Err(why.as_str()),
        };
        let entry = llm_entry(&prior, session.as_deref(), decision, repo::now_rfc3339()?);
        model::append(file, &entry)?;
        report.kinds.of_mut(kind).count(&entry);
        let _ = writeln!(progress, "{}", progress_line(kind, item, &entry));
    }
    Ok(())
}

/// `track TR:1 "Song" by Artist: matched (isrc)`.
fn progress_line(kind: IdKind, item: &Item, entry: &MatchEntry) -> String {
    let mut line = format!("{} {}", kind.as_str(), item.pandora_id);
    if let Some(name) = &item.name {
        line.push_str(&format!(" {name:?}"));
    }
    if let Some(artist) = &item.artist {
        line.push_str(&format!(" by {artist}"));
    }
    line.push_str(&format!(": {}", entry.status.as_str()));
    if let Some(m) = entry.method {
        line.push_str(&format!(" ({})", m.as_str()));
    }
    if let Some(r) = &entry.reason {
        line.push_str(&format!(" ({r})"));
    }
    line
}

/// How many candidates `match review` shows per item.
pub const REVIEW_CANDIDATES: usize = 3;

/// One item `match review` lists.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewItem {
    pub pandora_id: String,
    pub kind: IdKind,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub status: Status,
    pub confidence: Option<Confidence>,
    pub candidates: Vec<Candidate>,
    pub search_url: String,
}

/// What `sss match review` reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewReport {
    pub items: Vec<ReviewItem>,
}

impl ReviewReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        if self.items.is_empty() {
            return writeln!(out, "nothing to review");
        }
        for item in &self.items {
            let mut head = format!("{} {}", item.pandora_id, item.status.as_str());
            if let Some(c) = item.confidence {
                head.push_str(&format!(" ({} confidence)", c.as_str()));
            }
            if let Some(t) = &item.title {
                head.push_str(&format!(": {t:?}"));
            }
            if let Some(a) = &item.artist {
                head.push_str(&format!(" by {a}"));
            }
            if let Some(a) = &item.album {
                head.push_str(&format!(" on {a:?}"));
            }
            writeln!(out, "{head}")?;
            for (i, c) in item.candidates.iter().enumerate() {
                let mut line = format!("  {}. {} {:?}", i + 1, c.uri, c.name);
                if !c.artists.is_empty() {
                    line.push_str(&format!(" by {}", c.artists.join(", ")));
                }
                if let Some(a) = &c.album {
                    line.push_str(&format!(" on {a:?}"));
                }
                writeln!(out, "{line} score {}", c.score)?;
            }
            writeln!(out, "  search: {}", item.search_url)?;
        }
        Ok(())
    }
}

/// `https://open.spotify.com/search/<name> <artist>`, percent-encoded.
pub fn search_url(name: Option<&str>, artist: Option<&str>) -> String {
    let words: Vec<&str> = [name, artist].into_iter().flatten().collect();
    let mut url = Url::parse("https://open.spotify.com/search").expect("a valid address");
    if let Ok(mut segments) = url.path_segments_mut() {
        segments.push(&words.join(" "));
    }
    url.to_string()
}

/// `sss match review`: every item of `items` whose entry in `latest` is
/// `unmatched`, `llm_failed`, `ambiguous` or of low confidence, with its
/// Pandora fields, its top candidates and a Spotify search address. An
/// entry for an item `items` does not hold (a `[push]` switch turned off,
/// an item gone from the export) is skipped: push would not send it, so
/// there is nothing to decide.
pub fn review(
    latest: &ByKind<BTreeMap<String, MatchEntry>>,
    items: &Items,
    only: Option<IdKind>,
) -> ReviewReport {
    let mut out = Vec::new();
    for kind in IdKind::ALL {
        if only.is_some_and(|o| o != kind) {
            continue;
        }
        for (id, entry) in latest.of(kind) {
            let open = matches!(
                entry.status,
                Status::Unmatched | Status::LlmFailed | Status::Ambiguous
            ) || (entry.status == Status::Matched
                && entry.confidence == Some(Confidence::Low));
            if !open {
                continue;
            }
            let Some(item) = items.of(kind).get(id).cloned() else {
                continue;
            };
            out.push(ReviewItem {
                search_url: search_url(item.name.as_deref(), item.artist.as_deref()),
                pandora_id: id.clone(),
                kind,
                title: item.name,
                artist: item.artist,
                album: item.album,
                status: entry.status,
                confidence: entry.confidence,
                candidates: entry
                    .candidates
                    .iter()
                    .take(REVIEW_CANDIDATES)
                    .cloned()
                    .collect(),
            });
        }
    }
    ReviewReport { items: out }
}

/// What `sss match set` and `sss match reject` report: the entry appended.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MatchSetReport {
    pub entry: MatchEntry,
}

impl MatchSetReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        match self.entry.matched_uri() {
            Some(uri) => writeln!(out, "{} matched to {uri} (manual)", self.entry.pandora_id),
            None => writeln!(out, "{} rejected (manual)", self.entry.pandora_id),
        }
    }
}

/// The match file of the kind `pandora_id`'s prefix names; any other
/// prefix is kind `input`.
fn file_for(paths: &ProfilePaths, pandora_id: &str) -> Result<PathBuf> {
    match IdKind::of_pandora_id(pandora_id) {
        Some(k) => Ok(model::file(paths, k)),
        None => fail!(
            Kind::Input,
            "{pandora_id:?} is not a Pandora track, artist or album id (TR:…, AR:…, CO:…, AL:…)"
        ),
    }
}

/// `sss match set`: records `spotify` (a URI, address or bare id of the
/// kind the Pandora id calls for) as the item's match, by hand.
pub fn set(paths: &ProfilePaths, pandora_id: &str, spotify: &str) -> Result<MatchSetReport> {
    let file = file_for(paths, pandora_id)?;
    let id = ids::parse_for(pandora_id, spotify)?;
    let entry = MatchEntry::manual_set(pandora_id, &id.uri(), repo::now_rfc3339()?);
    model::append(&file, &entry)?;
    Ok(MatchSetReport { entry })
}

/// `sss match reject`: records that the item has no Spotify counterpart.
pub fn reject(paths: &ProfilePaths, pandora_id: &str) -> Result<MatchSetReport> {
    let file = file_for(paths, pandora_id)?;
    let entry = MatchEntry::manual_reject(pandora_id, repo::now_rfc3339()?);
    model::append(&file, &entry)?;
    Ok(MatchSetReport { entry })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error;
    use crate::matching::candidates::test_support::FakeSearch;
    use crate::pandora::model::{
        Album, Artist, Bookmark, Bookmarks, Collection, Feedback, Playlist, Seed, Station,
    };
    use crate::repo::Repo;
    use crate::spotify::client::{FoundArtist, FoundTrack, SearchResults};

    fn track(id: &str, title: &str) -> Track {
        Track {
            pandora_id: id.into(),
            title: Some(title.into()),
            artist: Some("The Band".into()),
            album: Some("Record".into()),
            duration_s: Some(200),
            ..Track::default()
        }
    }

    fn thumb(track: &str, positive: bool) -> Feedback {
        Feedback {
            feedback_id: format!("f-{track}"),
            track_pandora_id: Some(track.into()),
            positive,
            created: None,
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

    fn playlist(id: &str, linked: Option<&str>, items: &[&str]) -> Playlist {
        Playlist {
            pandora_id: id.into(),
            name: Some(id.into()),
            description: None,
            created: None,
            linked_type: linked.map(Into::into),
            items: items.iter().map(|s| (*s).into()).collect(),
        }
    }

    fn bookmark(id: &str, title: &str) -> Bookmark {
        Bookmark {
            pandora_id: Some(id.into()),
            title: Some(title.into()),
            artist: Some("Someone".into()),
            created: None,
        }
    }

    /// One station (TR:1 up, TR:2 down, TR:3 down here and up in a second
    /// station; seeds TR:4, AR:1, CO:1, a genre), a member playlist (TR:5),
    /// a Personalized one (TR:6) and a StationThumbs one (TR:7), the
    /// collection (TR:8, AL:1, AR:2) and bookmarks (TR:9, only there; AL:2).
    fn export() -> Normalized {
        let station = |id: &str, feedback, seeds| Station {
            station_id: id.into(),
            pandora_id: Some(format!("ST:{id}")),
            name: Some(format!("Station {id}")),
            created: None,
            last_played: None,
            is_thumbprint: false,
            is_shuffle: false,
            genres: Vec::new(),
            positive_feedback_count: None,
            negative_feedback_count: None,
            seeds,
            feedback,
        };
        let mut tracks = BTreeMap::new();
        for (id, title) in [
            ("TR:1", "One"),
            ("TR:2", "Two"),
            ("TR:3", "Three"),
            ("TR:4", "Four"),
            ("TR:5", "Five"),
            ("TR:6", "Six"),
            ("TR:7", "Seven"),
            ("TR:8", "Eight"),
        ] {
            tracks.insert(id.to_owned(), track(id, title));
        }
        Normalized {
            stations: vec![
                station(
                    "1",
                    vec![
                        thumb("TR:1", true),
                        thumb("TR:2", false),
                        thumb("TR:3", false),
                    ],
                    vec![
                        seed(SeedKind::Track, "TR:4", "Four"),
                        seed(SeedKind::Artist, "AR:1", "Seed Artist"),
                        seed(SeedKind::Artist, "CO:1", "A Composer"),
                        seed(SeedKind::Genre, "G:1", "Rock"),
                    ],
                ),
                station("2", vec![thumb("TR:3", true)], Vec::new()),
            ],
            playlists: vec![
                playlist("PL:1", None, &["TR:5"]),
                playlist("PL:2", Some("Personalized"), &["TR:6"]),
                playlist("PL:3", Some("StationThumbs"), &["TR:7"]),
            ],
            collection: Collection {
                tracks: vec!["TR:8".into()],
                albums: vec![Album {
                    pandora_id: "AL:1".into(),
                    name: Some("Record".into()),
                    artist: Some("The Band".into()),
                    artist_pandora_id: None,
                }],
                artists: vec![Artist {
                    pandora_id: "AR:2".into(),
                    name: Some("Other Artist".into()),
                }],
            },
            bookmarks: Bookmarks {
                tracks: vec![bookmark("TR:9", "Nine")],
                albums: vec![bookmark("AL:2", "Second Record")],
            },
            tracks,
        }
    }

    fn ids(map: &BTreeMap<String, Item>) -> Vec<&str> {
        map.keys().map(String::as_str).collect()
    }

    #[test]
    fn the_item_set_is_what_push_would_send() {
        let items = select(&export(), &PushConfig::default());
        assert_eq!(
            ids(&items.tracks),
            ["TR:1", "TR:3", "TR:4", "TR:5", "TR:8", "TR:9"],
            "no thumbs-down-only track, no Pandora-made playlist"
        );
        assert_eq!(ids(&items.artists), ["AR:1", "AR:2", "CO:1"]);
        assert_eq!(ids(&items.albums), ["AL:1", "AL:2"]);
        let nine = &items.tracks["TR:9"];
        assert_eq!(nine.name.as_deref(), Some("Nine"), "from bookmarks.json");
        assert_eq!(nine.artist.as_deref(), Some("Someone"));
        assert_eq!((nine.album.as_ref(), nine.duration_s), (None, None));
        assert_eq!(items.albums["AL:2"].name.as_deref(), Some("Second Record"));
        assert_eq!(items.tracks["TR:3"].stations, ["Station 2"]);
        assert_eq!(items.tracks["TR:4"].stations, ["Station 1"], "a track seed");
        assert_eq!(items.tracks["TR:5"].playlists, ["PL:1"]);
        assert!(items.tracks["TR:8"].stations.is_empty());
        assert_eq!(items.artists["AR:1"].stations, ["Station 1"]);
        assert_eq!(items.artists["AR:1"].context_titles, ["One"]);
        assert!(items.artists["AR:2"].stations.is_empty());

        let all = PushConfig {
            thumbs_down_playlist: true,
            linked_playlists: true,
            ..PushConfig::default()
        };
        assert_eq!(
            ids(&select(&export(), &all).tracks),
            [
                "TR:1", "TR:2", "TR:3", "TR:4", "TR:5", "TR:6", "TR:8", "TR:9"
            ],
            "StationThumbs is never pushed"
        );
    }

    fn found_track(uri: &str, name: &str) -> FoundTrack {
        FoundTrack {
            uri: uri.into(),
            name: name.into(),
            artists: vec!["The Band".into()],
            album: Some("Record".into()),
            album_type: Some("album".into()),
            release_date: None,
            duration_ms: Some(200_000),
            explicit: Some(false),
            isrc: None,
        }
    }

    fn paths(tmp: &std::path::Path) -> ProfilePaths {
        Repo::at(tmp).profile("alice").unwrap()
    }

    fn entry(id: &str, status: Status) -> MatchEntry {
        MatchEntry {
            pandora_id: id.into(),
            status,
            spotify_uri: None,
            method: None,
            confidence: None,
            candidates: Vec::new(),
            queries: Vec::new(),
            decided_at: "2026-10-15T12:00:00Z".into(),
            session: None,
            reason: None,
        }
    }

    fn items_of(tracks: &[(&str, Option<&str>)]) -> Items {
        let mut items = Items::default();
        for (id, title) in tracks {
            items.tracks.insert(
                (*id).into(),
                Item {
                    pandora_id: (*id).into(),
                    name: title.map(Into::into),
                    artist: Some("The Band".into()),
                    album: Some("Record".into()),
                    duration_s: Some(200),
                    ..Item::default()
                },
            );
        }
        items
    }

    fn run_with(
        paths: &ProfilePaths,
        items: &Items,
        options: Options,
        search: &mut FakeSearch,
    ) -> (Result<MatchReport>, String) {
        let mut progress = Vec::new();
        let result = run(
            "alice",
            paths,
            items,
            options,
            || Ok(&mut *search),
            None,
            &mut progress,
        );
        (result, String::from_utf8(progress).unwrap())
    }

    impl Search for &mut FakeSearch {
        fn search(&mut self, q: &str, kind: IdKind) -> Result<SearchResults> {
            (**self).search(q, kind)
        }
    }

    #[test]
    fn a_run_decides_and_appends_each_item() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let items = items_of(&[("TR:1", Some("One")), ("TR:2", Some("Two")), ("TR:3", None)]);
        let mut search = FakeSearch::default();
        search.answers.insert(
            "track:\"one\" artist:\"the band\"".into(),
            SearchResults {
                tracks: vec![found_track("spotify:track:one", "One")],
                ..SearchResults::default()
            },
        );
        search.answers.insert(
            "two the band".into(),
            SearchResults {
                tracks: vec![
                    found_track("spotify:track:a", "Two (Live)"),
                    found_track("spotify:track:b", "Two Reprise"),
                ],
                ..SearchResults::default()
            },
        );
        let (report, progress) = run_with(&paths, &items, Options::default(), &mut search);
        let report = report.unwrap();
        let latest = model::read_latest(&paths.track_matches()).unwrap();
        assert_eq!(latest["TR:1"].matched_uri(), Some("spotify:track:one"));
        assert_eq!(latest["TR:1"].method, Some(Method::Exact));
        assert_eq!(latest["TR:1"].queries.len(), 1);
        assert_eq!(latest["TR:2"].status, Status::Ambiguous);
        assert_eq!(latest["TR:2"].candidates.len(), 2);
        assert_eq!(latest["TR:3"].status, Status::Unmatched);
        assert_eq!(latest["TR:3"].reason.as_deref(), Some("no title"));
        assert_eq!(report.kinds.tracks.processed, 3);
        assert_eq!(report.kinds.tracks.by_method[&Method::Exact], 1);
        assert_eq!(report.kinds.tracks.unmatched_ids, ["TR:3"]);
        assert_eq!(report.stopped, None);
        assert!(
            progress.contains("track TR:1 \"One\" by The Band: matched (exact)\n"),
            "{progress}"
        );
        assert!(
            progress.contains("TR:3 by The Band: unmatched (no title)"),
            "{progress}"
        );

        let mut text = Vec::new();
        report.write_text(&mut text).unwrap();
        assert_eq!(
            String::from_utf8(text).unwrap(),
            "tracks: 3 decided: matched 1, unmatched 1, ambiguous 1, by exact 1\n\
             artists: 0 decided\nalbums: 0 decided\n"
        );

        // A second run decides nothing and makes no client.
        let mut progress = Vec::new();
        let report = run(
            "alice",
            &paths,
            &items,
            Options::default(),
            || -> Result<FakeSearch> { panic!("no client needed") },
            None,
            &mut progress,
        )
        .unwrap();
        assert_eq!(report.kinds.tracks.processed, 0);
    }

    #[test]
    fn ambiguous_and_failed_items_are_not_searched_again() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        model::append(&paths.track_matches(), &entry("TR:1", Status::Ambiguous)).unwrap();
        model::append(&paths.track_matches(), &entry("TR:2", Status::LlmFailed)).unwrap();
        let items = items_of(&[
            ("TR:1", Some("One")),
            ("TR:2", Some("Two")),
            ("TR:3", Some("Three")),
        ]);
        let mut search = FakeSearch::default();
        let (report, _) = run_with(&paths, &items, Options::default(), &mut search);
        assert_eq!(report.unwrap().kinds.tracks.processed, 1);
        assert!(
            search.asked.iter().all(|q| q.contains("three")),
            "{:?}",
            search.asked
        );
    }

    #[test]
    fn limit_and_only_bound_the_run() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let mut items = items_of(&[("TR:1", Some("One")), ("TR:2", Some("Two"))]);
        items.artists.insert(
            "AR:1".into(),
            Item::named("AR:1", Some(&"Artist".into()), None),
        );
        let mut search = FakeSearch::default();
        let options = Options {
            limit: Some(1),
            ..Options::default()
        };
        let (report, _) = run_with(&paths, &items, options, &mut search);
        let report = report.unwrap();
        assert_eq!(report.kinds.tracks.processed, 1);
        assert_eq!(report.stopped, Some(Stopped::Limit));
        assert_eq!(
            ids_of(&model::read_latest(&paths.track_matches()).unwrap()),
            ["TR:1"]
        );

        let options = Options {
            only: Some(IdKind::Artist),
            limit: Some(1),
            ..Options::default()
        };
        search.answers.insert(
            "Artist".into(),
            SearchResults {
                artists: vec![FoundArtist {
                    uri: "spotify:artist:a".into(),
                    name: "Artist".into(),
                }],
                ..SearchResults::default()
            },
        );
        let (report, _) = run_with(&paths, &items, options, &mut search);
        let report = report.unwrap();
        assert_eq!(report.kinds.artists.processed, 1);
        assert_eq!(report.kinds.tracks.processed, 0);
        assert_eq!(report.stopped, None, "nothing of the kind remains");
        assert_eq!(
            model::read_latest(&paths.artist_matches()).unwrap()["AR:1"].matched_uri(),
            Some("spotify:artist:a")
        );
    }

    #[test]
    fn a_quota_stop_keeps_what_was_decided() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let items = items_of(&[("TR:1", None), ("TR:2", Some("Two"))]);
        let mut search = FakeSearch {
            fail: Some(Kind::Quota),
            ..FakeSearch::default()
        };
        let (result, _) = run_with(&paths, &items, Options::default(), &mut search);
        assert_eq!(error::kind(&result.unwrap_err()), Kind::Quota);
        let latest = model::read_latest(&paths.track_matches()).unwrap();
        assert_eq!(ids_of(&latest), ["TR:1"]);
    }

    fn ids_of(map: &BTreeMap<String, MatchEntry>) -> Vec<&str> {
        map.keys().map(String::as_str).collect()
    }

    #[test]
    fn a_client_that_cannot_be_made_ends_the_run() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let items = items_of(&[("TR:1", Some("One"))]);
        let err = run(
            "alice",
            &paths,
            &items,
            Options::default(),
            || -> Result<FakeSearch> { fail!(Kind::Auth, "no token") },
            None,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Auth);
        assert!(!paths.track_matches().exists());
    }

    #[test]
    fn review_lists_open_items_with_three_candidates_and_a_search_link() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let candidate = |n: u8| Candidate {
            uri: format!("spotify:track:{n}"),
            name: format!("Song {n}"),
            score: 0.5,
            ..Candidate::default()
        };
        let mut ambiguous = entry("TR:1", Status::Ambiguous);
        ambiguous.candidates = (1..=5).map(candidate).collect();
        let mut low = entry("TR:2", Status::Matched);
        low.confidence = Some(Confidence::Low);
        low.spotify_uri = Some("spotify:track:2".into());
        let mut high = entry("TR:3", Status::Matched);
        high.confidence = Some(Confidence::High);
        let outside = entry("TR:5", Status::Unmatched);
        for e in [
            &ambiguous,
            &low,
            &high,
            &entry("TR:4", Status::Rejected),
            &outside,
        ] {
            model::append(&paths.track_matches(), e).unwrap();
        }
        model::append(&paths.album_matches(), &entry("AL:1", Status::Unmatched)).unwrap();
        let mut items = items_of(&[("TR:1", Some("Song & Dance")), ("TR:2", Some("Two"))]);
        items.albums.insert(
            "AL:1".into(),
            Item {
                pandora_id: "AL:1".into(),
                name: Some("Record".into()),
                artist: Some("The Band".into()),
                ..Item::default()
            },
        );

        let latest = model::read_all_latest(&paths).unwrap();
        let report = review(&latest, &items, None);
        let listed: Vec<&str> = report.items.iter().map(|i| i.pandora_id.as_str()).collect();
        assert_eq!(
            listed,
            ["TR:1", "TR:2", "AL:1"],
            "TR:5 is not in the item set"
        );
        let first = &report.items[0];
        assert_eq!(first.candidates.len(), REVIEW_CANDIDATES);
        assert_eq!(first.title.as_deref(), Some("Song & Dance"));
        assert_eq!(
            first.search_url,
            "https://open.spotify.com/search/Song%20&%20Dance%20The%20Band"
        );
        assert_eq!(report.items[2].kind, IdKind::Album);
        assert_eq!(
            report.items[2].search_url,
            "https://open.spotify.com/search/Record%20The%20Band"
        );

        let only = review(&latest, &items, Some(IdKind::Album));
        assert_eq!(only.items.len(), 1);

        let mut text = Vec::new();
        report.write_text(&mut text).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(
            text.starts_with(
                "TR:1 ambiguous: \"Song & Dance\" by The Band on \"Record\"\n  \
                 1. spotify:track:1 \"Song 1\" score 0.5\n"
            ),
            "{text}"
        );
        assert!(text.contains("TR:2 matched (low confidence)"), "{text}");
    }

    #[test]
    fn set_and_reject_append_manual_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let id = "4uLU6hMCjMI75M1A2tKUQC";
        let report = set(
            &paths,
            "TR:1",
            &format!("https://open.spotify.com/track/{id}?si=x"),
        )
        .unwrap();
        assert_eq!(report.entry.method, Some(Method::Manual));
        let latest = model::read_latest(&paths.track_matches()).unwrap();
        assert_eq!(
            latest["TR:1"].matched_uri(),
            Some(format!("spotify:track:{id}").as_str())
        );
        let mut text = Vec::new();
        report.write_text(&mut text).unwrap();
        assert_eq!(
            String::from_utf8(text).unwrap(),
            format!("TR:1 matched to spotify:track:{id} (manual)\n")
        );

        set(&paths, "CO:1", &format!("spotify:artist:{id}")).unwrap();
        assert!(
            model::read_latest(&paths.artist_matches())
                .unwrap()
                .contains_key("CO:1")
        );

        for (pandora, spotify) in [
            ("AL:1", format!("spotify:track:{id}")),
            ("ST:1", format!("spotify:track:{id}")),
            ("TR:1", "not a link".to_owned()),
        ] {
            let err = set(&paths, pandora, &spotify).unwrap_err();
            assert_eq!(
                error::kind(&err),
                Kind::Input,
                "{pandora} {spotify}: {err:#}"
            );
        }
        assert!(!paths.album_matches().exists());

        let report = reject(&paths, "AL:1").unwrap();
        assert_eq!(report.entry.status, Status::Rejected);
        let mut text = Vec::new();
        report.write_text(&mut text).unwrap();
        assert_eq!(String::from_utf8(text).unwrap(), "AL:1 rejected (manual)\n");
        assert_eq!(
            model::read_latest(&paths.album_matches()).unwrap()["AL:1"].status,
            Status::Rejected
        );
        assert_eq!(
            error::kind(&reject(&paths, "XX:1").unwrap_err()),
            Kind::Input
        );
    }

    #[test]
    fn the_report_serializes_with_snake_case_keys() {
        let mut report = MatchReport {
            profile: "alice".into(),
            kinds: KindReports::default(),
            stopped: Some(Stopped::Limit),
        };
        report.kinds.tracks.count(&entry("TR:1", Status::LlmFailed));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["kinds"]["tracks"]["by_status"]["llm_failed"], 1);
        assert_eq!(json["kinds"]["tracks"]["llm_failed_ids"][0], "TR:1");
        assert_eq!(json["kinds"]["albums"]["processed"], 0);
        assert_eq!(json["stopped"], "limit");
        assert_eq!(json["stopped"], Stopped::Limit.as_str());
    }

    /// A matcher that records each batch's item ids and answers from
    /// `verdicts` in turn.
    #[derive(Default)]
    struct FakeMatcher {
        batches: Vec<Vec<String>>,
        verdicts: Vec<Result<Verdict>>,
    }

    impl Matcher for &mut FakeMatcher {
        fn decide(&mut self, _: IdKind, batch: &[BatchItem]) -> Result<Outcome> {
            self.batches
                .push(batch.iter().map(|b| b.item.clone()).collect());
            let verdict = self.verdicts.remove(0)?;
            Ok(Outcome {
                session: Some(format!("s{}", self.batches.len())),
                verdict,
            })
        }
    }

    fn llm_decision(item: &str, choice: Option<&str>) -> LlmDecision {
        LlmDecision {
            item: item.into(),
            choice: choice.map(Into::into),
            confidence: Confidence::Medium,
            reason: Some("because".into()),
        }
    }

    fn run_llm(
        paths: &ProfilePaths,
        items: &Items,
        options: Options,
        search: &mut FakeSearch,
        matcher: &mut FakeMatcher,
    ) -> Result<MatchReport> {
        run(
            "alice",
            paths,
            items,
            options,
            || Ok(&mut *search),
            Some(Box::new(move || Ok(Box::new(matcher) as Box<dyn Matcher>))),
            &mut Vec::new(),
        )
    }

    fn ambiguous_answer(search: &mut FakeSearch, q: &str) {
        search.answers.insert(
            q.into(),
            SearchResults {
                tracks: vec![
                    found_track("spotify:track:a", "One (Live)"),
                    found_track("spotify:track:b", "One Reprise"),
                ],
                ..SearchResults::default()
            },
        );
    }

    #[test]
    fn the_llm_decides_ambiguous_and_failed_items_in_batches() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let mut stored = entry("TR:2", Status::Ambiguous);
        stored.candidates = vec![Candidate {
            uri: "spotify:track:two".into(),
            name: "Two".into(),
            score: 0.7,
            ..Candidate::default()
        }];
        stored.queries = vec!["two the band".into()];
        model::append(&paths.track_matches(), &stored).unwrap();
        model::append(&paths.track_matches(), &entry("TR:3", Status::LlmFailed)).unwrap();
        let mut done = entry("TR:4", Status::Matched);
        done.spotify_uri = Some("spotify:track:four".into());
        model::append(&paths.track_matches(), &done).unwrap();
        let items = items_of(&[
            ("TR:1", Some("One")),
            ("TR:2", Some("Two")),
            ("TR:3", Some("Three")),
            ("TR:4", Some("Four")),
        ]);
        let mut search = FakeSearch::default();
        ambiguous_answer(&mut search, "one the band");
        let mut matcher = FakeMatcher {
            verdicts: vec![
                Ok(Verdict::Decided(vec![
                    llm_decision("TR:1", Some("spotify:track:b")),
                    llm_decision("TR:2", None),
                ])),
                Ok(Verdict::Failed("ccc failed (exit 3)".into())),
            ],
            ..FakeMatcher::default()
        };
        let options = Options {
            batch_size: 2,
            ..Options::default()
        };
        let report = run_llm(&paths, &items, options, &mut search, &mut matcher).unwrap();
        assert_eq!(matcher.batches, [vec!["TR:1", "TR:2"], vec!["TR:3"]]);
        assert!(
            search.asked.iter().all(|q| q.contains("one")),
            "only the new item is searched: {:?}",
            search.asked
        );

        let latest = model::read_latest(&paths.track_matches()).unwrap();
        let one = &latest["TR:1"];
        assert_eq!(one.matched_uri(), Some("spotify:track:b"));
        assert_eq!(one.method, Some(Method::Llm));
        assert_eq!(one.confidence, Some(Confidence::Medium));
        assert_eq!(one.session.as_deref(), Some("s1"));
        assert_eq!(one.reason.as_deref(), Some("because"));
        assert_eq!(one.candidates.len(), 2, "the searched candidates are kept");
        let two = &latest["TR:2"];
        assert_eq!(
            (two.status, two.method),
            (Status::Unmatched, Some(Method::Llm))
        );
        assert_eq!(two.candidates, stored.candidates);
        assert_eq!(two.queries, stored.queries);
        let three = &latest["TR:3"];
        assert_eq!(three.status, Status::LlmFailed);
        assert_eq!(three.reason.as_deref(), Some("ccc failed (exit 3)"));
        assert_eq!(three.session.as_deref(), Some("s2"));
        assert_eq!(latest["TR:4"], done);

        let r = &report.kinds.tracks;
        assert_eq!(r.processed, 3, "each item counted once, by its last entry");
        assert_eq!(r.llm_batches, 2);
        assert_eq!(r.by_method[&Method::Llm], 2);
        assert_eq!(r.llm_failed_ids, ["TR:3"]);
        assert_eq!(r.unmatched_ids, ["TR:2"]);
        // The ambiguous entry from the search was written before the LLM's.
        let lines = repo::read_jsonl::<MatchEntry>(&paths.track_matches()).unwrap();
        assert_eq!(
            lines
                .iter()
                .filter(|e| e.pandora_id == "TR:1")
                .map(|e| e.status)
                .collect::<Vec<_>>(),
            [Status::Ambiguous, Status::Matched]
        );
    }

    #[test]
    fn the_limit_counts_items_sent_to_the_llm() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        model::append(&paths.track_matches(), &entry("TR:2", Status::Ambiguous)).unwrap();
        let items = items_of(&[("TR:1", Some("One")), ("TR:2", Some("Two"))]);
        let mut search = FakeSearch::default();
        ambiguous_answer(&mut search, "one the band");
        let mut matcher = FakeMatcher {
            verdicts: vec![Ok(Verdict::Decided(vec![llm_decision("TR:1", None)]))],
            ..FakeMatcher::default()
        };
        let options = Options {
            limit: Some(1),
            batch_size: 25,
            ..Options::default()
        };
        let report = run_llm(&paths, &items, options, &mut search, &mut matcher).unwrap();
        assert_eq!(report.stopped, Some(Stopped::Limit));
        assert_eq!(
            matcher.batches,
            [vec!["TR:1"]],
            "the queue is sent before stopping"
        );
        assert_eq!(report.kinds.tracks.processed, 1);
    }

    #[test]
    fn a_matcher_error_ends_the_run_and_keeps_the_entries_before_it() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let items = items_of(&[("TR:1", Some("One"))]);
        let mut search = FakeSearch::default();
        ambiguous_answer(&mut search, "one the band");
        let mut matcher = FakeMatcher {
            verdicts: vec![Err(anyhow::Error::new(error::Failure::new(
                Kind::Quota,
                "usage limit",
            )))],
            ..FakeMatcher::default()
        };
        let err = run_llm(
            &paths,
            &items,
            Options::default(),
            &mut search,
            &mut matcher,
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Quota);
        let latest = model::read_latest(&paths.track_matches()).unwrap();
        assert_eq!(latest["TR:1"].status, Status::Ambiguous);
    }

    #[test]
    fn a_matcher_that_cannot_be_made_fails_before_any_search() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let items = items_of(&[("TR:1", Some("One"))]);
        let mut search = FakeSearch::default();
        let err = run(
            "alice",
            &paths,
            &items,
            Options::default(),
            || Ok(&mut search),
            Some(Box::new(|| fail!(Kind::Config, "ccc cannot resolve"))),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Config);
        assert!(search.asked.is_empty());
        assert!(!paths.track_matches().exists());
    }
}
