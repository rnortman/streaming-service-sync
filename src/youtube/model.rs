//! The normalized YouTube model under `profiles/<name>/youtube/`: the play
//! history unioned over every complete import run, and the catalogue —
//! videos, playlists, subscriptions, library — from the newest one, its
//! videos completed by the Data API's cached answers.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use crate::error::Kind;
use crate::fail;
use crate::repo::{self, ProfilePaths};
use crate::youtube::api::{self, ApiVideo, Cached};
use crate::youtube::classify::{self, ApiFacts, ChannelKind, Product, Signals, VideoKind};
use crate::youtube::takeout::{
    self, Archive, HistoryRecord, ItemsSource, Manifest, Step, StepStatus,
};

/// Whether a play's title began with `Watched`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Watched,
    Other,
}

/// A channel as a record names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelRef {
    pub name: String,
    pub channel_id: Option<String>,
}

/// One entry of the watch history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Play {
    /// Canonical RFC 3339 UTC, `YYYY-MM-DDTHH:MM:SS.mmmZ`.
    pub at: String,
    pub video_id: Option<String>,
    pub url: Option<String>,
    pub product: Product,
    pub action: Action,
    /// Without its `Watched ` verb; `None` when Takeout withheld it.
    pub title: Option<String>,
    pub channel: Option<ChannelRef>,
    /// Takeout withheld the title and channel and no run supplied them.
    pub removed: bool,
    pub ad: bool,
}

impl Play {
    /// A play the counts include: it names a video and is not an ad.
    pub fn counts(&self) -> bool {
        self.video_id.is_some() && !self.ad
    }
}

/// Where a video's title, artists and album came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    Library,
    Api,
    History,
    None,
}

/// One video any record names: the unit of matching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Video {
    pub video_id: String,
    pub title: Option<String>,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_s: Option<u32>,
    pub channel: Option<ChannelRef>,
    pub kind: VideoKind,
    pub evidence: Vec<String>,
    pub title_source: TitleSource,
    /// The video's own title, newest seen.
    pub youtube_title: Option<String>,
    pub plays: u32,
    pub music_app_plays: u32,
    pub removed: bool,
    pub first_played: Option<String>,
    pub last_played: Option<String>,
    pub in_library: bool,
    pub liked: bool,
    /// `Some(false)` when the Data API was asked and did not return it.
    pub available: Option<bool>,
    pub category_id: Option<String>,
    pub topics: Vec<String>,
}

/// A list YouTube itself keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Special {
    LikedVideos,
    LikedMusic,
    WatchLater,
}

impl Special {
    /// The special list `title` names, by its English title.
    pub fn of_title(title: &str) -> Option<Special> {
        match title.trim().to_lowercase().as_str() {
            "liked videos" => Some(Special::LikedVideos),
            "liked music" => Some(Special::LikedMusic),
            "watch later" => Some(Special::WatchLater),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistItem {
    pub video_id: String,
    pub added_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    /// `None` for an items file no row of `playlists.csv` names.
    pub playlist_id: Option<String>,
    pub title: String,
    pub created: Option<String>,
    pub updated: Option<String>,
    pub order: Option<String>,
    pub visibility: Option<String>,
    pub special: Option<Special>,
    pub items_source: ItemsSource,
    /// In the archive's order, repeats kept.
    pub items: Vec<PlaylistItem>,
}

/// One subscribed channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub channel_id: String,
    pub name: String,
    pub url: Option<String>,
    pub kind: ChannelKind,
    pub evidence: Vec<String>,
    /// Videos of kind `song` or `music_video` uploaded by this channel.
    pub music_videos: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    pub songs: Vec<String>,
    pub liked: Vec<String>,
    pub disliked: Vec<String>,
}

/// The five normalized files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Normalized {
    pub history: Vec<Play>,
    pub videos: BTreeMap<String, Video>,
    pub playlists: Vec<Playlist>,
    pub subscriptions: Vec<Channel>,
    pub library: Library,
}

/// What identifies a play across archives: its instant, and its video id,
/// else its address, else its title.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PlayKey {
    Video(String, String),
    Url(String, String),
    Title(String, String),
}

fn key_of(r: &HistoryRecord) -> PlayKey {
    match (&r.video_id, &r.title_url) {
        (Some(id), _) => PlayKey::Video(r.time.clone(), id.clone()),
        (None, Some(url)) => PlayKey::Url(r.time.clone(), url.clone()),
        (None, None) => PlayKey::Title(r.time.clone(), r.title.clone()),
    }
}

fn play_of(r: &HistoryRecord) -> Play {
    let withheld = r.is_withheld();
    let (action, title) = match r.title.strip_prefix("Watched ") {
        Some(rest) => (Action::Watched, rest.to_owned()),
        None => (Action::Other, r.title.clone()),
    };
    Play {
        at: r.time.clone(),
        video_id: r.video_id.clone(),
        url: r.title_url.clone(),
        product: classify::product(r.header.as_deref(), r.title_url.as_deref()),
        action,
        title: (!withheld).then_some(title),
        channel: r.channel.as_ref().map(|c| ChannelRef {
            name: c.name.clone(),
            channel_id: c.url.as_deref().and_then(takeout::channel_id_from_url),
        }),
        removed: withheld,
        ad: r.ad,
    }
}

/// The union of the runs' history, each run given oldest first: one play
/// per key, the newest run's record, but its title and channel from the
/// newest run that did not withhold them. Sorted by instant, then run.
pub fn union_history<'a>(runs: impl IntoIterator<Item = &'a [HistoryRecord]>) -> Vec<Play> {
    let mut plays: BTreeMap<PlayKey, (usize, Play)> = BTreeMap::new();
    for (run, records) in runs.into_iter().enumerate() {
        for r in records {
            let mut play = play_of(r);
            if let Some((_, older)) = plays.get(&key_of(r))
                && play.removed
                && !older.removed
            {
                play.title.clone_from(&older.title);
                play.channel.clone_from(&older.channel);
                play.removed = false;
            }
            plays.insert(key_of(r), (run, play));
        }
    }
    let mut out: Vec<(usize, PlayKey, Play)> =
        plays.into_iter().map(|(k, (run, p))| (run, k, p)).collect();
    out.sort_by(|a, b| (&a.2.at, a.0, &a.1).cmp(&(&b.2.at, b.0, &b.1)));
    out.into_iter().map(|(_, _, p)| p).collect()
}

/// The stretches of at least `min_days` with no counted play inside the
/// history's span, as `(last play before, first play after)`.
pub fn gaps(history: &[Play], min_days: i64) -> Vec<(String, String)> {
    let instants: Vec<(OffsetDateTime, &str)> = history
        .iter()
        .filter(|p| p.counts())
        .filter_map(|p| Some((OffsetDateTime::parse(&p.at, &Rfc3339).ok()?, p.at.as_str())))
        .collect();
    instants
        .windows(2)
        .filter(|w| w[1].0 - w[0].0 >= Duration::days(min_days))
        .map(|w| (w[0].1.to_owned(), w[1].1.to_owned()))
        .collect()
}

/// The complete import runs of `paths`, oldest first.
pub fn complete_runs(paths: &ProfilePaths) -> Result<Vec<Manifest>> {
    let mut runs = Vec::new();
    for id in repo::run_ids(&paths.youtube_raw_dir())? {
        if let Some(m) = repo::read_manifest::<Manifest>(&paths.youtube_raw_run_dir(&id))?
            && m.complete
        {
            runs.push(m);
        }
    }
    Ok(runs)
}

/// Builds the model from every complete run's archive, oldest archive
/// first — the history from all of them, the catalogue from the last — and
/// the Data API's latest answer per video.
pub fn build(archives: &[Archive], api: &BTreeMap<String, Cached>) -> Normalized {
    let history = union_history(archives.iter().map(|a| a.history.as_slice()));
    let empty = Archive::default();
    let latest = archives.last().unwrap_or(&empty);

    let playlists: Vec<Playlist> = latest
        .playlists
        .iter()
        .map(|p| Playlist {
            playlist_id: p.playlist_id.clone(),
            title: p.title.clone(),
            created: p.created.clone(),
            updated: p.updated.clone(),
            order: p.order.clone(),
            visibility: p.visibility.clone(),
            special: Special::of_title(&p.title),
            items_source: p.items_source,
            items: p
                .items
                .iter()
                .map(|i| PlaylistItem {
                    video_id: i.video_id.clone(),
                    added_at: i.added_at.clone(),
                })
                .collect(),
        })
        .collect();
    let library = Library {
        songs: unique(latest.library.iter().map(|s| s.video_id.as_str())),
        liked: unique(
            playlists
                .iter()
                .filter(|p| matches!(p.special, Some(Special::LikedVideos | Special::LikedMusic)))
                .flat_map(|p| &p.items)
                .map(|i| i.video_id.as_str()),
        ),
        disliked: Vec::new(),
    };

    let videos = videos(&history, &playlists, latest, &library, api);
    let subscriptions = subscriptions(latest, &history, &videos);
    Normalized {
        history,
        videos,
        playlists,
        subscriptions,
        library,
    }
}

/// `ids` in first-seen order, each once.
fn unique<'a>(ids: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    ids.into_iter()
        .filter(|id| seen.insert(*id))
        .map(str::to_owned)
        .collect()
}

fn draft(video_id: &str) -> Video {
    Video {
        video_id: video_id.to_owned(),
        title: None,
        artists: Vec::new(),
        album: None,
        duration_s: None,
        channel: None,
        kind: VideoKind::Unknown,
        evidence: Vec::new(),
        title_source: TitleSource::None,
        youtube_title: None,
        plays: 0,
        music_app_plays: 0,
        removed: false,
        first_played: None,
        last_played: None,
        in_library: false,
        liked: false,
        available: None,
        category_id: None,
        topics: Vec::new(),
    }
}

/// The catalogue of every video a record names, filled and classified.
fn videos(
    history: &[Play],
    playlists: &[Playlist],
    latest: &Archive,
    library: &Library,
    api: &BTreeMap<String, Cached>,
) -> BTreeMap<String, Video> {
    let mut videos: BTreeMap<String, Video> = BTreeMap::new();
    let mut plays_of: BTreeMap<&str, Vec<&Play>> = BTreeMap::new();
    for play in history.iter().filter(|p| p.counts()) {
        let id = play.video_id.as_deref().unwrap_or_default();
        plays_of.entry(id).or_default().push(play);
        let v = videos.entry(id.to_owned()).or_insert_with(|| draft(id));
        v.plays += 1;
        if play.product == Product::YoutubeMusic {
            v.music_app_plays += 1;
        }
        if v.first_played.is_none() {
            v.first_played = Some(play.at.clone());
        }
        v.last_played = Some(play.at.clone());
    }
    for item in playlists.iter().flat_map(|p| &p.items) {
        videos
            .entry(item.video_id.clone())
            .or_insert_with(|| draft(&item.video_id));
    }
    let songs: BTreeMap<&str, &takeout::LibrarySong> = latest
        .library
        .iter()
        .map(|s| (s.video_id.as_str(), s))
        .collect();
    for id in songs.keys() {
        videos.entry((*id).to_owned()).or_insert_with(|| draft(id));
    }
    for (id, cached) in api {
        if matches!(cached, Cached::Found(_)) {
            videos.entry(id.clone()).or_insert_with(|| draft(id));
        }
    }
    let in_library: BTreeSet<&str> = library.songs.iter().map(String::as_str).collect();
    let liked: BTreeSet<&str> = library.liked.iter().map(String::as_str).collect();

    for (id, v) in &mut videos {
        let plays = plays_of.get(id.as_str()).map(Vec::as_slice).unwrap_or(&[]);
        let found = match api.get(id) {
            Some(Cached::Found(a)) => Some(a),
            _ => None,
        };
        let api_title = found.and_then(|a| a.title.as_deref());
        let api_channel = found.and_then(api_channel);
        let newest_titled = plays.iter().rev().find(|p| p.title.is_some());
        v.youtube_title = api_title
            .map(str::to_owned)
            .or_else(|| newest_titled.and_then(|p| p.title.clone()));
        v.channel = api_channel
            .clone()
            .or_else(|| newest_titled.and_then(|p| p.channel.clone()));
        v.in_library = in_library.contains(id.as_str());
        v.liked = liked.contains(id.as_str());
        if let Some(song) = songs.get(id.as_str()).filter(|s| s.title.is_some()) {
            v.title_source = TitleSource::Library;
            v.title.clone_from(&song.title);
            v.artists.clone_from(&song.artists);
            v.album.clone_from(&song.album);
        } else if let Some(title) = &v.youtube_title {
            let derived = classify::derive(title, v.channel.as_ref().map(|c| c.name.as_str()));
            v.title_source = if api_title.is_some() {
                TitleSource::Api
            } else {
                TitleSource::History
            };
            v.title = Some(derived.title);
            v.artists = derived.artists;
        }
        v.removed = v.title.is_none() && plays.iter().any(|p| p.removed);
        v.available = api.get(id).map(|c| matches!(c, Cached::Found(_)));
        if let Some(a) = found {
            v.duration_s = a.duration_s;
            v.category_id.clone_from(&a.category_id);
            v.topics.clone_from(&a.topics);
        }

        let mut titles: Vec<&str> = Vec::new();
        let library_title = songs.get(id.as_str()).and_then(|s| s.title.as_deref());
        for t in library_title
            .into_iter()
            .chain(api_title)
            .chain(plays.iter().filter_map(|p| p.title.as_deref()))
        {
            if !titles.contains(&t) {
                titles.push(t);
            }
        }
        let signals = Signals {
            in_library: v.in_library,
            channels: api_channel
                .as_ref()
                .map(|c| c.name.as_str())
                .into_iter()
                .chain(
                    plays
                        .iter()
                        .filter_map(|p| p.channel.as_ref().map(|c| c.name.as_str())),
                )
                .collect(),
            titles,
            music_app: plays.iter().any(|p| p.product == Product::YoutubeMusic),
            api: found.map(|a| ApiFacts {
                category_id: a.category_id.clone(),
                topics: a.topics.clone(),
            }),
        };
        (v.kind, v.evidence) = classify::kind(&signals);
    }
    videos
}

/// The uploader the Data API names, when it names one.
fn api_channel(a: &ApiVideo) -> Option<ChannelRef> {
    Some(ChannelRef {
        name: a.channel_title.clone()?,
        channel_id: a.channel_id.clone(),
    })
}

/// The latest run's subscriptions, each classified.
fn subscriptions(
    latest: &Archive,
    history: &[Play],
    videos: &BTreeMap<String, Video>,
) -> Vec<Channel> {
    let played_topic_artists: BTreeSet<String> = history
        .iter()
        .filter(|p| p.counts())
        .filter_map(|p| classify::topic_artist(&p.channel.as_ref()?.name))
        .map(classify::fold)
        .collect();
    let mut music_videos_of: BTreeMap<&str, u32> = BTreeMap::new();
    for v in videos.values().filter(|v| v.kind.is_music()) {
        if let Some(id) = v.channel.as_ref().and_then(|c| c.channel_id.as_deref()) {
            *music_videos_of.entry(id).or_default() += 1;
        }
    }
    latest
        .subscriptions
        .iter()
        .map(|s| {
            let music_videos = music_videos_of
                .get(s.channel_id.as_str())
                .copied()
                .unwrap_or(0);
            let (kind, evidence) =
                classify::channel_kind(&s.name, music_videos, &played_topic_artists);
            Channel {
                channel_id: s.channel_id.clone(),
                name: s.name.clone(),
                url: s.url.clone(),
                kind,
                evidence,
                music_videos,
            }
        })
        .collect()
}

/// Writes the five normalized files of `paths`.
pub fn write(paths: &ProfilePaths, data: &Normalized) -> Result<()> {
    repo::write_json(&paths.youtube_videos(), &data.videos)?;
    repo::write_json(&paths.youtube_playlists(), &data.playlists)?;
    repo::write_json(&paths.youtube_subscriptions(), &data.subscriptions)?;
    repo::write_json(&paths.youtube_library(), &data.library)?;
    repo::write_json(&paths.youtube_history(), &data.history)
}

/// Reads the files [`write`] wrote; `None` when there is no `history.json`
/// (the profile has no YouTube import), an error when it is there and
/// another file is missing or unreadable.
pub fn read_opt(paths: &ProfilePaths) -> Result<Option<Normalized>> {
    let Some(history) = repo::read_json_opt(&paths.youtube_history())? else {
        return Ok(None);
    };
    Ok(Some(Normalized {
        history,
        videos: repo::read_json(&paths.youtube_videos())?,
        playlists: repo::read_json(&paths.youtube_playlists())?,
        subscriptions: repo::read_json(&paths.youtube_subscriptions())?,
        library: repo::read_json(&paths.youtube_library())?,
    }))
}

/// What one normalization holds, counted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// The run the catalogue comes from.
    pub run_id: String,
    /// Every run the history is the union of, oldest first.
    pub history_runs: Vec<String>,
    /// History entries, every kind.
    pub records: usize,
    /// Counted plays: entries naming a video, ads excluded.
    pub plays: usize,
    pub removed_plays: usize,
    pub videos: usize,
    pub songs: usize,
    pub music_videos: usize,
    pub unknown: usize,
    pub playlists: usize,
    pub subscriptions: usize,
    pub artist_channels: usize,
    pub library_songs: usize,
    pub liked: usize,
}

impl Summary {
    /// The `normalized run …` line.
    pub fn write_text(&self, out: &mut dyn Write) -> std::io::Result<()> {
        writeln!(
            out,
            "normalized run {} (history from {} runs): {} history records, {} plays ({} titles \
             withheld), {} videos ({} songs, {} music videos, {} unknown), {} playlists, \
             {} subscriptions ({} artists), {} library songs, {} liked",
            self.run_id,
            self.history_runs.len(),
            self.records,
            self.plays,
            self.removed_plays,
            self.videos,
            self.songs,
            self.music_videos,
            self.unknown,
            self.playlists,
            self.subscriptions,
            self.artist_channels,
            self.library_songs,
            self.liked,
        )
    }
}

/// The counts of `data`, from the runs `runs` (oldest first).
pub fn summary(data: &Normalized, runs: &[Manifest]) -> Summary {
    let kinds = |k: VideoKind| data.videos.values().filter(|v| v.kind == k).count();
    let counted = || data.history.iter().filter(|p| p.counts());
    Summary {
        run_id: runs.last().map(|m| m.run_id.clone()).unwrap_or_default(),
        history_runs: runs.iter().map(|m| m.run_id.clone()).collect(),
        records: data.history.len(),
        plays: counted().count(),
        removed_plays: counted().filter(|p| p.removed).count(),
        videos: data.videos.len(),
        songs: kinds(VideoKind::Song),
        music_videos: kinds(VideoKind::MusicVideo),
        unknown: kinds(VideoKind::Unknown),
        playlists: data.playlists.len(),
        subscriptions: data.subscriptions.len(),
        artist_channels: data
            .subscriptions
            .iter()
            .filter(|c| c.kind == ChannelKind::Artist)
            .count(),
        library_songs: data.library.songs.len(),
        liked: data.library.liked.len(),
    }
}

/// Parses each run's copies again, oldest first. A run whose copies no
/// longer parse complete — a copy gone or changed, or one this version's
/// parser rejects — is kind `config`, naming the run and its failed steps.
fn archives(paths: &ProfilePaths, runs: &[Manifest]) -> Result<Vec<Archive>> {
    runs.iter()
        .map(|m| {
            let parse = takeout::parse_run(&paths.youtube_raw_run_dir(&m.run_id), &m.files);
            if !parse.steps.complete() {
                let lines: Vec<String> = Step::ALL
                    .into_iter()
                    .filter(|&s| parse.steps.get(s).status != StepStatus::Ok)
                    .map(|s| parse.steps.get(s).line(s))
                    .collect();
                fail!(
                    Kind::Config,
                    "run {} was complete when imported but its copies no longer parse ({}); \
                     the normalized files are left as they were",
                    m.run_id,
                    lines.join("; ")
                );
            }
            Ok(parse.archive)
        })
        .collect()
}

/// `sss youtube normalize`: re-derives the normalized files from every
/// complete run, ordered by archive age, and the Data API cache, writes the
/// run ids beside them, and returns what they hold.
pub fn normalize(paths: &ProfilePaths) -> Result<Summary> {
    let runs = complete_runs(paths)?;
    if runs.is_empty() {
        bail!("no complete import run to normalize; run `sss youtube import DIR`");
    }
    let parsed = archives(paths, &runs)?;
    let (runs, archives): (Vec<Manifest>, Vec<Archive>) =
        by_age(runs.into_iter().zip(parsed).collect())
            .into_iter()
            .unzip();
    let cache = api::read_cache(&paths.youtube_api_dir())?;
    let data = build(&archives, &cache);
    let summary = summary(&data, &runs);
    repo::write_json(&paths.youtube_runs(), &summary.history_runs)?;
    write(paths, &data)?;
    Ok(summary)
}

/// The ids of the complete runs the normalized files were built from,
/// oldest archive first, as [`normalize`] last wrote them: the last one
/// supplied the catalogue.
pub fn read_runs(paths: &ProfilePaths) -> Result<Vec<String>> {
    repo::read_json(&paths.youtube_runs())
}

/// `runs`, given in run-id order, ordered by the age of their archive,
/// oldest first: by the newest history record each holds, run-id order
/// kept for a tie. A run imported after another but holding an older
/// archive sorts before it.
fn by_age(mut runs: Vec<(Manifest, Archive)>) -> Vec<(Manifest, Archive)> {
    runs.sort_by_cached_key(|(_, a)| a.history.iter().map(|r| r.time.clone()).max());
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::Repo;
    use crate::youtube::takeout::{ChannelLink, LibrarySong, PlaylistRecord, SubscriptionRecord};

    const V1: &str = "abcDEF12345";
    const V2: &str = "-xyzXYZ0987";
    const V3: &str = "zzzZZZ00000";
    const TOPIC: &str = "UCtttttttttttttttttttttt";
    const OWN: &str = "UCoooooooooooooooooooooo";
    const PL: &str = "PLaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn url(host: &str, id: &str) -> String {
        format!("https://{host}/watch?v={id}")
    }

    fn rec(time: &str, id: &str, title: &str, channel: Option<(&str, &str)>) -> HistoryRecord {
        let title_url = url("music.youtube.com", id);
        HistoryRecord {
            time: takeout::canonical_time(time).unwrap(),
            header: Some("YouTube Music".to_owned()),
            title: format!("Watched {title}"),
            title_url: Some(title_url),
            video_id: Some(id.to_owned()),
            channel: channel.map(|(name, id)| ChannelLink {
                name: name.to_owned(),
                url: Some(format!("https://www.youtube.com/channel/{id}")),
            }),
            ad: false,
        }
    }

    fn withheld(time: &str, id: &str) -> HistoryRecord {
        let u = url("music.youtube.com", id);
        HistoryRecord {
            title: format!("Watched {u}"),
            channel: None,
            ..rec(time, id, "", None)
        }
    }

    fn other(time: &str, title: &str, title_url: Option<&str>) -> HistoryRecord {
        HistoryRecord {
            time: takeout::canonical_time(time).unwrap(),
            header: Some("YouTube".to_owned()),
            title: title.to_owned(),
            title_url: title_url.map(str::to_owned),
            video_id: None,
            channel: None,
            ad: false,
        }
    }

    fn takeout_only(archives: &[Archive]) -> Normalized {
        build(archives, &BTreeMap::new())
    }

    fn topic() -> Option<(&'static str, &'static str)> {
        Some(("Chill Out Radio - Topic", TOPIC))
    }

    #[test]
    fn plays_strip_the_verb_and_keep_withheld_ones() {
        let h = union_history([&[
            rec("2026-01-01T00:00:00Z", V1, "Calm Waters", topic()),
            withheld("2026-01-02T00:00:00Z", V2),
            other(
                "2026-01-03T00:00:00Z",
                "Viewed A post",
                Some("https://www.youtube.com/post/x"),
            ),
        ][..]]);
        assert_eq!(h[0].title.as_deref(), Some("Calm Waters"));
        assert_eq!(h[0].action, Action::Watched);
        assert_eq!(
            h[0].channel.as_ref().unwrap().channel_id.as_deref(),
            Some(TOPIC)
        );
        assert_eq!(h[0].product, Product::YoutubeMusic);
        assert!(h[1].removed && h[1].title.is_none() && h[1].url.is_some());
        assert_eq!(h[2].action, Action::Other);
        assert_eq!(h[2].title.as_deref(), Some("Viewed A post"));
    }

    #[test]
    fn overlapping_runs_union_once_per_play() {
        let older = vec![
            rec("2020-01-01T00:00:00Z", V1, "Calm Waters", topic()),
            rec("2026-01-01T00:00:05Z", V1, "Calm Waters", topic()),
            rec("2026-01-02T00:00:00Z", V2, "Old Name", topic()),
            rec("2026-01-03T00:00:00Z", V3, "Still Here", topic()),
            other(
                "2026-01-04T00:00:00Z",
                "Viewed Post",
                Some("https://www.youtube.com/post/a"),
            ),
            other(
                "2026-01-04T00:00:00Z",
                "Viewed Post",
                Some("https://www.youtube.com/post/b"),
            ),
        ];
        let mut www = rec("2026-01-01T00:00:05.000Z", V1, "Calm Waters", topic());
        www.title_url = Some(url("www.youtube.com", V1));
        www.header = Some("YouTube".to_owned());
        let newer = vec![
            www,
            rec("2026-01-02T00:00:00Z", V2, "New Name", topic()),
            withheld("2026-01-03T00:00:00Z", V3),
            rec("2026-01-04T00:00:00Z", V1, "Calm Waters", topic()),
            other("2026-01-04T00:00:00Z", "Answered survey", None),
        ];
        let h = union_history([older.as_slice(), newer.as_slice()]);
        // Two V1 plays from the older run (one rendered differently in the
        // newer), V2, V3, two posts, the newer V1 play and the survey.
        assert_eq!(h.len(), 8);
        let at = |t: &str, id: &str| {
            h.iter()
                .find(|p| p.at.starts_with(t) && p.video_id.as_deref() == Some(id))
                .unwrap()
        };
        assert_eq!(at("2026-01-01T00:00:05", V1).product, Product::Youtube);
        assert_eq!(at("2026-01-02", V2).title.as_deref(), Some("New Name"));
        let kept = at("2026-01-03", V3);
        assert_eq!(kept.title.as_deref(), Some("Still Here"));
        assert!(!kept.removed && kept.channel.is_some());
        assert_eq!(h.iter().filter(|p| p.video_id.is_none()).count(), 3);
        assert!(h.windows(2).all(|w| w[0].at <= w[1].at));

        let n = takeout_only(&[
            Archive {
                history: older,
                ..Default::default()
            },
            Archive {
                history: newer,
                ..Default::default()
            },
        ]);
        let v1 = &n.videos[V1];
        assert_eq!(v1.plays, 3);
        assert_eq!(v1.first_played.as_deref(), Some("2020-01-01T00:00:00.000Z"));
        assert_eq!(v1.music_app_plays, 2);
        assert_eq!(n.videos[V3].youtube_title.as_deref(), Some("Still Here"));
        assert!(!n.videos[V3].removed);
    }

    #[test]
    fn ads_and_entries_without_a_video_are_counted_nowhere() {
        let mut ad = rec("2026-01-01T00:00:00Z", V2, "An Advert", None);
        ad.ad = true;
        let n = takeout_only(&[Archive {
            history: vec![ad, other("2026-01-02T00:00:00Z", "Answered survey", None)],
            ..Default::default()
        }]);
        assert_eq!(n.history.len(), 2);
        assert!(n.videos.is_empty());
    }

    fn song(id: &str, title: &str) -> LibrarySong {
        LibrarySong {
            video_id: id.to_owned(),
            title: Some(title.to_owned()),
            album: Some("Blue Album".to_owned()),
            artists: vec!["Chill Out Radio".to_owned(), "A Member".to_owned()],
        }
    }

    fn playlist(id: Option<&str>, title: &str, items: &[&str]) -> PlaylistRecord {
        PlaylistRecord {
            playlist_id: id.map(str::to_owned),
            title: title.to_owned(),
            created: None,
            updated: None,
            order: Some("Date Published (Newest)".to_owned()),
            visibility: Some("Private".to_owned()),
            items_source: ItemsSource::File,
            items: items
                .iter()
                .map(|v| takeout::PlaylistItem {
                    video_id: (*v).to_owned(),
                    added_at: None,
                })
                .collect(),
        }
    }

    #[test]
    fn titles_come_from_library_then_history_then_nowhere() {
        let n = takeout_only(&[Archive {
            history: vec![
                rec("2026-01-01T00:00:00Z", V1, "History Title", topic()),
                rec(
                    "2026-01-02T00:00:00Z",
                    V2,
                    "Chill Out Radio - Calm Waters (Official Video)",
                    Some(("Someone", OWN)),
                ),
            ],
            library: vec![song(V1, "Library Title")],
            playlists: vec![playlist(Some(PL), "Mix", &[V3])],
            ..Default::default()
        }]);
        let v1 = &n.videos[V1];
        assert_eq!(v1.title_source, TitleSource::Library);
        assert_eq!(v1.title.as_deref(), Some("Library Title"));
        assert_eq!(v1.album.as_deref(), Some("Blue Album"));
        assert_eq!(v1.youtube_title.as_deref(), Some("History Title"));
        assert_eq!(v1.evidence[..2], ["library", "topic-channel"]);
        let v2 = &n.videos[V2];
        assert_eq!(v2.title_source, TitleSource::History);
        assert_eq!(v2.title.as_deref(), Some("Calm Waters"));
        assert_eq!(v2.artists, ["Chill Out Radio"]);
        assert_eq!(
            (v2.kind, v2.evidence.as_slice()),
            (VideoKind::MusicVideo, &["music-app".to_owned()][..])
        );
        let v3 = &n.videos[V3];
        assert_eq!(
            (v3.title_source, v3.kind, v3.plays),
            (TitleSource::None, VideoKind::Unknown, 0)
        );
        assert!(!v3.removed && v3.title.is_none());
        assert_eq!(v3.available, None);
    }

    fn api_video(id: &str, title: &str, channel: &str, category: &str) -> Cached {
        Cached::Found(ApiVideo {
            video_id: id.to_owned(),
            title: Some(title.to_owned()),
            channel_id: Some(OWN.to_owned()),
            channel_title: Some(channel.to_owned()),
            category_id: Some(category.to_owned()),
            duration_s: Some(220),
            topics: Vec::new(),
        })
    }

    #[test]
    fn the_api_titles_after_the_library_and_before_history() {
        const V4: &str = "yyyYYY11111";
        let archive = Archive {
            history: vec![
                rec("2026-01-01T00:00:00Z", V1, "History Title", topic()),
                rec(
                    "2026-01-02T00:00:00Z",
                    V2,
                    "Old Upload Title",
                    Some(("Band", OWN)),
                ),
                withheld("2026-01-03T00:00:00Z", V4),
            ],
            library: vec![song(V1, "Library Title")],
            playlists: vec![playlist(Some(PL), "Mix", &[V3])],
            ..Default::default()
        };
        let api = BTreeMap::from([
            (
                V1.to_owned(),
                api_video(V1, "Api One", "Chill Out Radio - Topic", "10"),
            ),
            (
                V2.to_owned(),
                api_video(V2, "Band - Calm Waters (Official Video)", "Band", "10"),
            ),
            (
                V3.to_owned(),
                api_video(V3, "Band - Night Drive", "Band", "10"),
            ),
            (V4.to_owned(), Cached::Absent),
        ]);
        let n = build(&[archive], &api);

        let v1 = &n.videos[V1];
        assert_eq!(v1.title_source, TitleSource::Library);
        assert_eq!(v1.title.as_deref(), Some("Library Title"));
        assert_eq!(v1.youtube_title.as_deref(), Some("Api One"));
        assert_eq!((v1.duration_s, v1.available), (Some(220), Some(true)));

        let v2 = &n.videos[V2];
        assert_eq!(v2.title_source, TitleSource::Api);
        assert_eq!(v2.title.as_deref(), Some("Calm Waters"));
        assert_eq!(v2.artists, ["Band"]);
        assert_eq!(
            v2.youtube_title.as_deref(),
            Some("Band - Calm Waters (Official Video)")
        );
        assert_eq!(v2.category_id.as_deref(), Some("10"));
        assert_eq!(v2.kind, VideoKind::MusicVideo);
        assert_eq!(v2.evidence[0], "api-category-music");

        let v3 = &n.videos[V3];
        assert_eq!(
            (v3.title_source, v3.title.as_deref(), v3.kind),
            (TitleSource::Api, Some("Night Drive"), VideoKind::MusicVideo)
        );
        assert_eq!(
            v3.channel,
            Some(ChannelRef {
                name: "Band".into(),
                channel_id: Some(OWN.into())
            })
        );

        let v4 = &n.videos[V4];
        assert_eq!((v4.available, v4.removed), (Some(false), true));
        assert_eq!((v4.duration_s, v4.title_source), (None, TitleSource::None));
    }

    #[test]
    fn the_api_category_settles_what_the_music_app_alone_typed() {
        let archive = Archive {
            history: vec![
                rec(
                    "2026-01-01T00:00:00Z",
                    V1,
                    "Episode 12",
                    Some(("A Podcast", OWN)),
                ),
                rec(
                    "2026-01-02T00:00:00Z",
                    V2,
                    "Episode 13",
                    Some(("A Podcast", OWN)),
                ),
            ],
            ..Default::default()
        };
        let mut talk = api_video(V1, "Episode 12", "A Podcast", "22");
        let mut music_topic = api_video(V2, "Episode 13", "A Podcast", "22");
        if let Cached::Found(a) = &mut music_topic {
            a.topics = vec!["https://en.wikipedia.org/wiki/Electronic_music".into()];
        }
        if let Cached::Found(a) = &mut talk {
            a.duration_s = None;
        }
        let without = takeout_only(std::slice::from_ref(&archive));
        assert_eq!(without.videos[V1].evidence, ["music-app"]);
        assert_eq!(without.videos[V1].available, None);
        let n = build(
            &[archive],
            &BTreeMap::from([(V1.to_owned(), talk), (V2.to_owned(), music_topic)]),
        );
        let v1 = &n.videos[V1];
        assert_eq!(v1.kind, VideoKind::Video);
        assert_eq!(v1.evidence, ["api-category:22", "music-app"]);
        assert_eq!(v1.duration_s, None);
        let v2 = &n.videos[V2];
        assert_eq!(v2.kind, VideoKind::MusicVideo);
        assert_eq!(v2.evidence[0], "api-topic:Electronic_music");
        assert_eq!(v2.topics.len(), 1);
    }

    #[test]
    fn an_api_answer_for_a_video_no_archive_names_makes_a_video() {
        let api = BTreeMap::from([
            (
                V1.to_owned(),
                api_video(V1, "Band - Calm Waters", "Band", "10"),
            ),
            (V2.to_owned(), Cached::Absent),
        ]);
        let n = build(&[Archive::default()], &api);
        assert_eq!(n.videos.keys().collect::<Vec<_>>(), [V1]);
        assert_eq!(n.videos[V1].plays, 0);
    }

    #[test]
    fn a_video_is_removed_only_when_no_source_titles_it() {
        let n = takeout_only(&[Archive {
            history: vec![
                withheld("2026-01-01T00:00:00Z", V1),
                withheld("2026-01-01T00:00:00Z", V2),
            ],
            library: vec![song(V2, "Known Elsewhere")],
            ..Default::default()
        }]);
        assert!(n.videos[V1].removed);
        assert_eq!(n.videos[V1].evidence, ["music-app"]);
        assert!(!n.videos[V2].removed);
    }

    #[test]
    fn special_playlists_and_the_library() {
        assert_eq!(Special::of_title(" liked MUSIC"), Some(Special::LikedMusic));
        assert_eq!(Special::of_title("Watch later"), Some(Special::WatchLater));
        assert_eq!(Special::of_title("Mes vidéos J'aime"), None);
        let without = takeout_only(&[Archive {
            playlists: vec![playlist(Some(PL), "Watch later", &[V1])],
            library: vec![song(V2, "a"), song(V2, "a")],
            ..Default::default()
        }]);
        assert!(without.library.liked.is_empty() && without.library.disliked.is_empty());
        assert_eq!(without.library.songs, [V2]);
        assert!(without.videos[V2].in_library);
        let with = takeout_only(&[Archive {
            playlists: vec![playlist(None, "Liked videos", &[V3, V1, V3])],
            ..Default::default()
        }]);
        assert_eq!(with.library.liked, [V3, V1]);
        assert!(with.videos[V1].liked);
        assert_eq!(with.playlists[0].special, Some(Special::LikedVideos));
    }

    #[test]
    fn playlist_items_keep_the_archive_order_and_catalogue_is_the_newest() {
        let older = Archive {
            playlists: vec![playlist(Some(PL), "Gone", &[V1])],
            ..Default::default()
        };
        let newer = Archive {
            playlists: vec![playlist(Some(PL), "Mix", &[V3, V1, V3, V2])],
            ..Default::default()
        };
        let n = takeout_only(&[older, newer]);
        assert_eq!(n.playlists.len(), 1);
        let order: Vec<&str> = n.playlists[0]
            .items
            .iter()
            .map(|i| i.video_id.as_str())
            .collect();
        assert_eq!(order, [V3, V1, V3, V2]);
    }

    #[test]
    fn subscriptions_are_classified() {
        let n = takeout_only(&[Archive {
            history: vec![
                rec("2026-01-01T00:00:00Z", V1, "Calm Waters", topic()),
                rec(
                    "2026-01-02T00:00:00Z",
                    V2,
                    "x (Official Music Video)",
                    Some(("Band", OWN)),
                ),
            ],
            subscriptions: vec![
                SubscriptionRecord {
                    channel_id: "UCaaaaaaaaaaaaaaaaaaaaaa".into(),
                    url: None,
                    name: "chill out radio ".into(),
                },
                SubscriptionRecord {
                    channel_id: OWN.into(),
                    url: None,
                    name: "Band".into(),
                },
                SubscriptionRecord {
                    channel_id: "UCbbbbbbbbbbbbbbbbbbbbbb".into(),
                    url: None,
                    name: "Racing Daily".into(),
                },
                SubscriptionRecord {
                    channel_id: "UCcccccccccccccccccccccc".into(),
                    url: None,
                    name: String::new(),
                },
            ],
            ..Default::default()
        }]);
        let s = &n.subscriptions;
        assert_eq!(
            (s[0].kind, s[0].evidence.as_slice()),
            (ChannelKind::Artist, &["topic-namesake".to_owned()][..])
        );
        assert_eq!((s[1].kind, s[1].music_videos), (ChannelKind::Artist, 1));
        assert_eq!(s[1].evidence, ["music-videos"]);
        assert_eq!(s[2].kind, ChannelKind::Channel);
        assert_eq!(s[3].kind, ChannelKind::Channel);
    }

    #[test]
    fn gaps_of_thirty_days_or_more() {
        let dense = union_history([&[
            rec("2026-01-01T00:00:00Z", V1, "a", topic()),
            rec("2026-01-29T00:00:00Z", V1, "a", topic()),
        ][..]]);
        assert!(gaps(&dense, 30).is_empty());
        let just_under = union_history([&[
            rec("2026-01-01T00:00:00Z", V1, "a", topic()),
            rec("2026-01-30T23:59:59Z", V1, "a", topic()),
        ][..]]);
        assert!(gaps(&just_under, 30).is_empty());
        let exactly = union_history([&[
            rec("2026-01-01T00:00:00Z", V1, "a", topic()),
            rec("2026-01-31T00:00:00Z", V1, "a", topic()),
        ][..]]);
        assert_eq!(gaps(&exactly, 30).len(), 1);
        let holed = union_history([&[
            rec("2023-12-01T00:00:00Z", V1, "a", topic()),
            other("2024-06-01T00:00:00Z", "Answered survey", None),
            rec("2025-03-12T00:00:00Z", V1, "a", topic()),
            rec("2025-03-13T00:00:00Z", V1, "a", topic()),
        ][..]]);
        assert_eq!(
            gaps(&holed, 30),
            [(
                "2023-12-01T00:00:00.000Z".to_owned(),
                "2025-03-12T00:00:00.000Z".to_owned()
            )]
        );
    }

    /// Imports a Takeout tree holding `history` (and nothing else when it
    /// is `None`) as a new run of `paths`.
    fn import(paths: &ProfilePaths, history: Option<serde_json::Value>) -> Manifest {
        let src = tempfile::tempdir().unwrap();
        let dir = src.path().join("Takeout/YouTube and YouTube Music");
        std::fs::create_dir_all(dir.join("history")).unwrap();
        std::fs::create_dir_all(dir.join("subscriptions")).unwrap();
        std::fs::write(
            dir.join("subscriptions/subscriptions.csv"),
            "Channel Id,Channel Url,Channel Title\n",
        )
        .unwrap();
        if let Some(h) = history {
            std::fs::write(dir.join("history/watch-history.json"), h.to_string()).unwrap();
        }
        takeout::import(paths, src.path(), &mut std::io::sink())
            .unwrap()
            .manifest
    }

    fn entry(time: &str, id: &str) -> serde_json::Value {
        serde_json::json!({"header": "YouTube Music", "title": "Watched Calm Waters",
            "titleUrl": url("music.youtube.com", id), "time": time,
            "subtitles": [{"name": "Chill Out Radio - Topic",
                           "url": format!("https://www.youtube.com/channel/{TOPIC}")}]})
    }

    #[test]
    fn normalize_unions_every_complete_run_and_ignores_incomplete_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Repo::at(tmp.path()).profile("alice").unwrap();
        assert!(normalize(&paths).is_err());
        let first = import(
            &paths,
            Some(serde_json::json!([
                entry("2020-01-01T00:00:00Z", V1),
                entry("2026-01-01T00:00:00Z", V1)
            ])),
        );
        let broken = import(&paths, None);
        assert!(!broken.complete);
        let second = import(
            &paths,
            Some(serde_json::json!([
                entry("2026-01-01T00:00:00.000Z", V1),
                entry("2026-02-01T00:00:00Z", V2)
            ])),
        );
        let summary = normalize(&paths).unwrap();
        assert_eq!(summary.history_runs, [first.run_id, second.run_id.clone()]);
        assert_eq!(summary.run_id, second.run_id);
        assert_eq!((summary.plays, summary.videos, summary.songs), (3, 2, 2));
        let n = read_opt(&paths).unwrap().unwrap();
        assert_eq!(n.videos[V1].plays, 2);
    }

    #[test]
    fn an_older_archive_imported_later_neither_supplies_the_catalogue_nor_wins_a_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Repo::at(tmp.path()).profile("alice").unwrap();
        let mut renamed = entry("2026-01-01T00:00:00Z", V1);
        renamed["title"] = serde_json::json!("Watched Calm Waters (Remastered)");
        let newer = import(
            &paths,
            Some(serde_json::json!([
                renamed,
                entry("2026-02-01T00:00:00Z", V2)
            ])),
        );
        let older = import(
            &paths,
            Some(serde_json::json!([
                entry("2020-01-01T00:00:00Z", V1),
                entry("2026-01-01T00:00:00Z", V1)
            ])),
        );
        let summary = normalize(&paths).unwrap();
        assert_eq!(summary.history_runs, [older.run_id, newer.run_id.clone()]);
        assert_eq!(read_runs(&paths).unwrap(), summary.history_runs);
        assert_eq!(summary.run_id, newer.run_id);
        assert_eq!(summary.plays, 3);
        let n = read_opt(&paths).unwrap().unwrap();
        let shared = n
            .history
            .iter()
            .find(|p| p.at == "2026-01-01T00:00:00.000Z");
        assert_eq!(
            shared.and_then(|p| p.title.as_deref()),
            Some("Calm Waters (Remastered)")
        );
    }

    #[test]
    fn normalize_reads_the_api_cache_latest_answer_winning() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Repo::at(tmp.path()).profile("alice").unwrap();
        import(
            &paths,
            Some(serde_json::json!([entry("2026-01-01T00:00:00Z", V1)])),
        );
        let answer = |n: u32, items: serde_json::Value| {
            let exchange = serde_json::json!({"at": "2026-01-01T00:00:00Z", "method": "GET",
                "path": "videos", "request": {"ids": [V1], "part": api::PART},
                "status": 200, "response": {"items": items}});
            std::fs::create_dir_all(paths.youtube_api_dir()).unwrap();
            std::fs::write(
                paths
                    .youtube_api_dir()
                    .join(format!("{n:04}-videos-list.json")),
                exchange.to_string(),
            )
            .unwrap();
        };
        answer(1, serde_json::json!([]));
        normalize(&paths).unwrap();
        let n = read_opt(&paths).unwrap().unwrap();
        assert_eq!(n.videos[V1].available, Some(false));
        answer(
            2,
            serde_json::json!([{"id": V1, "contentDetails": {"duration": "PT4M"}}]),
        );
        normalize(&paths).unwrap();
        let n = read_opt(&paths).unwrap().unwrap();
        assert_eq!(
            (n.videos[V1].available, n.videos[V1].duration_s),
            (Some(true), Some(240))
        );
        assert_eq!(n.videos[V1].title_source, TitleSource::History);
    }

    #[test]
    fn a_complete_run_whose_copy_no_longer_parses_fails_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Repo::at(tmp.path()).profile("alice").unwrap();
        let run = import(
            &paths,
            Some(serde_json::json!([entry("2026-01-01T00:00:00Z", V1)])),
        );
        normalize(&paths).unwrap();
        let before = std::fs::read(paths.youtube_videos()).unwrap();

        let copy = paths
            .youtube_raw_run_dir(&run.run_id)
            .join("takeout/history/watch-history.json");
        std::fs::write(&copy, "not json").unwrap();
        let err = normalize(&paths).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Config);
        let message = format!("{err:#}");
        assert!(message.contains(&run.run_id), "{message}");
        assert!(message.contains("history: failed"), "{message}");
        assert_eq!(std::fs::read(paths.youtube_videos()).unwrap(), before);
    }

    #[test]
    fn the_files_round_trip_and_serialize_deterministically() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Repo::at(tmp.path()).profile("alice").unwrap();
        assert_eq!(read_opt(&paths).unwrap(), None);
        let n = takeout_only(&[Archive {
            history: vec![rec("2026-01-01T00:00:00Z", V1, "Calm Waters", topic())],
            playlists: vec![playlist(Some(PL), "Mix", &[V2])],
            library: vec![song(V3, "x")],
            ..Default::default()
        }]);
        write(&paths, &n).unwrap();
        let first = std::fs::read(paths.youtube_videos()).unwrap();
        assert_eq!(read_opt(&paths).unwrap(), Some(n.clone()));
        write(&paths, &n).unwrap();
        assert_eq!(std::fs::read(paths.youtube_videos()).unwrap(), first);
        let library: serde_json::Value = repo::read_json(&paths.youtube_library()).unwrap();
        assert_eq!(library["disliked"], serde_json::json!([]));
    }
}
