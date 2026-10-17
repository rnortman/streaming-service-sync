//! The YouTube items `sss` matches — the music videos a member liked, keeps
//! in the library, holds in a pushed playlist or played most, and the
//! subscribed channels with music evidence — and what push makes of them.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::json;

use crate::config::{Config, YoutubeConfig};
use crate::matching::run::{CONTEXT_TITLES, Item, Items};
use crate::push::{self, Matches, NAME_MAX, Wanted};
use crate::source::Source;
use crate::youtube::classify::{self, ChannelKind, VideoKind, evidence};
use crate::youtube::model::{Channel, Normalized, Playlist, Special, Video};
use crate::youtube::takeout::ItemsSource;

/// What a video's item id starts with.
const VIDEO_PREFIX: &str = "YV:";

/// The item id of the video `video_id`.
pub fn video_item_id(video_id: &str) -> String {
    format!("{VIDEO_PREFIX}{video_id}")
}

/// The video id of a video's item id; none for any other item id.
pub fn video_id_of(item_id: &str) -> Option<&str> {
    item_id.strip_prefix(VIDEO_PREFIX)
}

/// The item id of the channel `channel_id`.
pub fn channel_item_id(channel_id: &str) -> String {
    format!("YC:{channel_id}")
}

/// The keys of a YouTube item's `extra`, as [`items`] writes them and the
/// LLM record reads them.
pub mod extra_key {
    pub const YOUTUBE_TITLE: &str = "youtube_title";
    pub const CHANNEL: &str = "channel";
    pub const KIND: &str = "kind";
    pub const PLAYS: &str = "plays";
    pub const LIKED: &str = "liked";
    pub const IN_LIBRARY: &str = "in_library";
}

/// Whether a video of `kind` may be matched: music, or under
/// `include_unknown` a video nothing classified.
fn eligible(kind: VideoKind, config: &YoutubeConfig) -> bool {
    kind.is_music() || (config.include_unknown && kind == VideoKind::Unknown)
}

/// Why a playlist does not go to Spotify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotPushed {
    /// A list YouTube keeps: Watch later or a liked list.
    Special(Special),
    /// An items file no row of `playlists.csv` names.
    NoId,
    /// The archive holds no items file for it.
    NoItemsFile,
    /// Its items file is shared with a playlist of the same file name.
    SharedItemsFile,
    /// Its id is in `[youtube] skip_playlists`.
    Skipped,
}

/// The id of `playlist` when it goes to Spotify — one of the member's own
/// lists, not a list YouTube keeps, with its own items file, and not
/// skipped by id — else why it does not.
pub fn push_decision<'a>(
    playlist: &'a Playlist,
    config: &YoutubeConfig,
) -> Result<&'a str, NotPushed> {
    if let Some(special) = playlist.special {
        return Err(NotPushed::Special(special));
    }
    let id = playlist.playlist_id.as_deref().ok_or(NotPushed::NoId)?;
    match playlist.items_source {
        ItemsSource::File => {}
        ItemsSource::None => return Err(NotPushed::NoItemsFile),
        ItemsSource::AmbiguousTitle => return Err(NotPushed::SharedItemsFile),
    }
    if config.skip_playlists.iter().any(|s| s == id) {
        return Err(NotPushed::Skipped);
    }
    Ok(id)
}

/// Whether `playlist` goes to Spotify, as [`push_decision`] decides.
pub fn playlist_is_pushed(playlist: &Playlist, config: &YoutubeConfig) -> bool {
    push_decision(playlist, config).is_ok()
}

/// The eligible videos played at least once, most played first; ties go
/// to the most recently played, then to the video id.
pub fn by_plays<'a>(data: &'a Normalized, config: &YoutubeConfig) -> Vec<&'a Video> {
    let mut played: Vec<&Video> = data
        .videos
        .values()
        .filter(|v| v.plays > 0 && eligible(v.kind, config))
        .collect();
    played.sort_by(|a, b| {
        b.plays
            .cmp(&a.plays)
            .then_with(|| b.last_played.cmp(&a.last_played))
            .then_with(|| a.video_id.cmp(&b.video_id))
    });
    played
}

/// The ids of the first `[youtube] most_played` videos of [`by_plays`].
pub fn most_played(data: &Normalized, config: &YoutubeConfig) -> Vec<String> {
    by_plays(data, config)
        .into_iter()
        .take(usize::try_from(config.most_played).unwrap_or(usize::MAX))
        .map(|v| v.video_id.clone())
        .collect()
}

/// Every YouTube item push would send: tracks for the eligible videos that
/// are liked, in the library, in a pushed playlist or among the most
/// played; artists for the subscribed channels of kind `artist`. No albums.
pub fn items(data: &Normalized, config: &YoutubeConfig) -> Items {
    let mut playlists_of: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for playlist in data
        .playlists
        .iter()
        .filter(|p| playlist_is_pushed(p, config))
    {
        for item in &playlist.items {
            let names = playlists_of.entry(item.video_id.as_str()).or_default();
            if !names.contains(&playlist.title) {
                names.push(playlist.title.clone());
            }
        }
    }
    let top = most_played(data, config);
    let in_scope: BTreeSet<&str> = data
        .library
        .liked
        .iter()
        .chain(&data.library.songs)
        .map(String::as_str)
        .chain(playlists_of.keys().copied())
        .chain(top.iter().map(String::as_str))
        .collect();

    let tracks = in_scope
        .into_iter()
        .filter_map(|id| data.videos.get(id))
        .filter(|v| eligible(v.kind, config))
        .map(|v| {
            let playlists = playlists_of
                .get(v.video_id.as_str())
                .cloned()
                .unwrap_or_default();
            let item = track(v, playlists);
            (item.item_id.clone(), item)
        })
        .collect();
    let played = Played::new(data);
    let artists = data
        .subscriptions
        .iter()
        .filter(|c| c.kind == ChannelKind::Artist)
        .map(|c| {
            let item = artist(c, &played);
            (item.item_id.clone(), item)
        })
        .collect();
    Items {
        tracks,
        artists,
        albums: BTreeMap::new(),
    }
}

/// The track item of `video`, held by the pushed playlists `playlists`.
fn track(video: &Video, playlists: Vec<String>) -> Item {
    Item {
        name: video.title.clone(),
        artist: video.artists.first().cloned(),
        album: video.album.clone(),
        duration_s: video.duration_s,
        playlists,
        extra: json!({
            (extra_key::YOUTUBE_TITLE): video.youtube_title,
            (extra_key::CHANNEL): video.channel.as_ref().map(|c| &c.name),
            (extra_key::KIND): video.kind,
            (extra_key::PLAYS): video.plays,
            (extra_key::LIKED): video.liked,
            (extra_key::IN_LIBRARY): video.in_library,
        }),
        ..Item::new(Source::Youtube, &video_item_id(&video.video_id))
    }
}

/// The artist a subscribed channel's name stands for: without ` - Topic`
/// or `VEVO`, the suffixes of the channels YouTube and VEVO run for an
/// artist. `Official` and `Music` stay, unlike when a video title's artist
/// is read from its uploader: here the name is the channel the member
/// chose to follow.
fn artist_name(channel: &str) -> String {
    let name = channel.trim();
    let name = name
        .strip_suffix(classify::TOPIC_SUFFIX)
        .or_else(|| name.strip_suffix(classify::VEVO_SUFFIX))
        .unwrap_or(name);
    name.trim().to_owned()
}

/// The played videos with a title, by channel id and by the folded artist
/// of their Topic channel.
struct Played<'a> {
    by_channel: BTreeMap<&'a str, Vec<&'a Video>>,
    by_topic_artist: BTreeMap<String, Vec<&'a Video>>,
}

impl<'a> Played<'a> {
    fn new(data: &'a Normalized) -> Self {
        let mut played = Played {
            by_channel: BTreeMap::new(),
            by_topic_artist: BTreeMap::new(),
        };
        for v in data.videos.values() {
            let Some(c) = v
                .channel
                .as_ref()
                .filter(|_| v.plays > 0 && v.title.is_some())
            else {
                continue;
            };
            if let Some(id) = c.channel_id.as_deref() {
                played.by_channel.entry(id).or_default().push(v);
            }
            if let Some(a) = classify::topic_artist(&c.name) {
                played
                    .by_topic_artist
                    .entry(classify::fold(a))
                    .or_default()
                    .push(v);
            }
        }
        played
    }
}

/// The artist item of the subscribed `channel`, with the titles played most
/// from it — or, when its evidence is a Topic namesake, from that Topic
/// channel — as context.
fn artist(channel: &Channel, played: &Played) -> Item {
    let namesake = channel
        .evidence
        .iter()
        .any(|e| e == evidence::TOPIC_NAMESAKE)
        .then(|| played.by_topic_artist.get(&classify::fold(&channel.name)))
        .flatten();
    let from: BTreeMap<&str, &Video> = played
        .by_channel
        .get(channel.channel_id.as_str())
        .into_iter()
        .chain(namesake)
        .flatten()
        .map(|v| (v.video_id.as_str(), *v))
        .collect();
    let mut from: Vec<&Video> = from.into_values().collect();
    from.sort_by(|a, b| b.plays.cmp(&a.plays).then_with(|| a.title.cmp(&b.title)));
    let mut context_titles: Vec<String> = Vec::new();
    for title in from.iter().filter_map(|v| v.title.as_ref()) {
        if context_titles.len() >= CONTEXT_TITLES {
            break;
        }
        if !context_titles.contains(title) {
            context_titles.push(title.clone());
        }
    }
    Item {
        name: Some(artist_name(&channel.name)).filter(|n| !n.is_empty()),
        context_titles,
        extra: json!({ (extra_key::CHANNEL): channel.name }),
        ..Item::new(Source::Youtube, &channel_item_id(&channel.channel_id))
    }
}

/// The ledger key of the "most played" playlist.
pub const MOST_PLAYED_KEY: &str = "youtube:most-played";

/// The ledger key of the YouTube playlist `playlist_id`.
pub fn playlist_key(playlist_id: &str) -> String {
    format!("YP:{playlist_id}")
}

/// The item ids of the eligible videos among `video_ids`, in order.
fn eligible_ids<'a>(
    data: &Normalized,
    video_ids: impl IntoIterator<Item = &'a String>,
    config: &YoutubeConfig,
) -> Vec<String> {
    video_ids
        .into_iter()
        .filter(|id| {
            data.videos
                .get(*id)
                .is_some_and(|v| eligible(v.kind, config))
        })
        .map(|id| video_item_id(id))
        .collect()
}

/// The matched URIs of the item ids `ids`, in order, each once.
fn matched_tracks(matches: &Matches, ids: &[String]) -> Vec<String> {
    push::matched(&matches.tracks, ids.iter().map(String::as_str))
}

/// A Spotify playlist for every pushed YouTube playlist with a matched
/// music item: its title, its matched items in CSV order, and a description
/// counting the distinct videos matched.
pub fn playlists(data: &Normalized, matches: &Matches, config: &Config) -> Vec<Wanted> {
    let mut wanted = Vec::new();
    for playlist in &data.playlists {
        let Ok(id) = push_decision(playlist, &config.youtube) else {
            continue;
        };
        let rows = eligible_ids(
            data,
            playlist.items.iter().map(|i| &i.video_id),
            &config.youtube,
        );
        let uris = matched_tracks(matches, &rows);
        if uris.is_empty() {
            continue;
        }
        let distinct: HashSet<&str> = playlist.items.iter().map(|i| i.video_id.as_str()).collect();
        let matched_rows: Vec<&str> = rows
            .iter()
            .map(String::as_str)
            .filter(|row| {
                matches
                    .tracks
                    .get(*row)
                    .is_some_and(|e| e.matched_uri().is_some())
            })
            .collect();
        let matched_videos: HashSet<&str> = matched_rows.iter().copied().collect();
        let key = playlist_key(id);
        let description = format!(
            "YouTube playlist \"{}\": {} of {} videos matched.",
            playlist.title,
            matched_videos.len(),
            distinct.len()
        );
        wanted.push(Wanted {
            name: push::cut(&playlist.title, NAME_MAX),
            description: push::with_marker(&description, &key),
            repeats: matched_rows.len() - uris.len(),
            key,
            uris,
        });
    }
    wanted
}

/// The "most played" playlist: the matched videos among the
/// `[youtube] most_played` most played, in play-count order; none when that
/// is 0 or nothing among them is matched.
pub fn most_played_playlist(
    data: &Normalized,
    matches: &Matches,
    config: &Config,
) -> Option<Wanted> {
    let top = most_played(data, &config.youtube);
    let ids: Vec<String> = top.iter().map(|id| video_item_id(id)).collect();
    let uris = matched_tracks(matches, &ids);
    if uris.is_empty() {
        return None;
    }
    let description = format!(
        "Songs that have been among your {} most played on YouTube Music when sss pushed; sss \
         adds, never removes.",
        config.youtube.most_played
    );
    Some(Wanted {
        key: MOST_PLAYED_KEY.to_owned(),
        name: push::cut(
            &format!("{}YouTube Music: most played", config.push.playlist_prefix),
            NAME_MAX,
        ),
        description: push::with_marker(&description, MOST_PLAYED_KEY),
        uris,
        repeats: 0,
    })
}

/// The matched subscribed channels of kind `artist`.
pub fn artist_uris(data: &Normalized, matches: &Matches) -> Vec<String> {
    let ids: Vec<String> = data
        .subscriptions
        .iter()
        .filter(|c| c.kind == ChannelKind::Artist)
        .map(|c| channel_item_id(&c.channel_id))
        .collect();
    push::matched(&matches.artists, ids.iter().map(String::as_str))
}

/// The matched eligible library songs, in library order.
pub fn collection_uris(
    data: &Normalized,
    matches: &Matches,
    config: &YoutubeConfig,
) -> Vec<String> {
    matched_tracks(matches, &eligible_ids(data, &data.library.songs, config))
}

/// The matched eligible liked videos, in liked order.
pub fn liked_uris(data: &Normalized, matches: &Matches, config: &YoutubeConfig) -> Vec<String> {
    matched_tracks(matches, &eligible_ids(data, &data.library.liked, config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matching::llm::BatchItem;
    use crate::matching::model::{Confidence, MatchEntry, Method, Status};
    use crate::spotify::ids::IdKind;
    use crate::youtube::model::{ChannelRef, Library, PlaylistItem, Special, TitleSource};

    fn video(id: &str, kind: VideoKind, plays: u32, last: &str) -> Video {
        Video {
            video_id: id.to_owned(),
            title: Some(format!("Song {id}")),
            artists: vec!["Chill Out Radio".to_owned()],
            album: None,
            duration_s: None,
            channel: Some(ChannelRef {
                name: "Chill Out Radio - Topic".to_owned(),
                channel_id: Some("UCtopic".to_owned()),
            }),
            kind,
            evidence: Vec::new(),
            title_source: TitleSource::History,
            youtube_title: Some(format!("Song {id}")),
            plays,
            music_app_plays: plays,
            removed: false,
            first_played: (plays > 0).then(|| "2024-01-01T00:00:00.000Z".to_owned()),
            last_played: (plays > 0).then(|| last.to_owned()),
            in_library: false,
            liked: false,
            available: None,
            category_id: None,
            topics: Vec::new(),
        }
    }

    fn playlist(id: Option<&str>, title: &str, items: &[&str]) -> Playlist {
        Playlist {
            playlist_id: id.map(str::to_owned),
            title: title.to_owned(),
            created: None,
            updated: None,
            order: None,
            visibility: None,
            special: Special::of_title(title),
            items_source: ItemsSource::File,
            items: items
                .iter()
                .map(|v| PlaylistItem {
                    video_id: (*v).to_owned(),
                    added_at: None,
                })
                .collect(),
        }
    }

    fn data(videos: Vec<Video>) -> Normalized {
        Normalized {
            videos: videos
                .into_iter()
                .map(|v| (v.video_id.clone(), v))
                .collect(),
            ..Normalized::default()
        }
    }

    fn config(most_played: u32) -> YoutubeConfig {
        YoutubeConfig {
            most_played,
            ..YoutubeConfig::default()
        }
    }

    fn ids(items: &BTreeMap<String, Item>) -> Vec<&str> {
        items.keys().map(String::as_str).collect()
    }

    const PL1: &str = "PLaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PL2: &str = "PLbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn the_most_played_are_ranked_by_plays_then_by_the_latest_play() {
        let d = data(vec![
            video("v1", VideoKind::Song, 3, "2024-01-01T00:00:00.000Z"),
            video("v2", VideoKind::Song, 5, "2024-01-01T00:00:00.000Z"),
            video("v3", VideoKind::MusicVideo, 3, "2025-01-01T00:00:00.000Z"),
            video("v4", VideoKind::Unknown, 9, "2025-01-01T00:00:00.000Z"),
            video("v5", VideoKind::Video, 9, "2025-01-01T00:00:00.000Z"),
            video("v6", VideoKind::Song, 0, ""),
        ]);
        assert_eq!(most_played(&d, &config(10)), ["v2", "v3", "v1"]);
        assert_eq!(most_played(&d, &config(2)), ["v2", "v3"]);
        assert!(most_played(&d, &config(0)).is_empty());
        let unknown_too = YoutubeConfig {
            include_unknown: true,
            ..config(2)
        };
        assert_eq!(most_played(&d, &unknown_too), ["v4", "v2"]);
    }

    #[test]
    fn tracks_are_the_liked_library_playlist_and_most_played_music() {
        let mut d = data(vec![
            video("liked", VideoKind::Song, 0, ""),
            video("lib", VideoKind::Song, 0, ""),
            video("listed", VideoKind::MusicVideo, 0, ""),
            video("top", VideoKind::Song, 7, "2024-01-01T00:00:00.000Z"),
            video("second", VideoKind::Song, 6, "2024-01-01T00:00:00.000Z"),
            video("podcast", VideoKind::Video, 0, ""),
            video("bare", VideoKind::Unknown, 0, ""),
            video("elsewhere", VideoKind::Song, 0, ""),
        ]);
        d.library = Library {
            songs: vec!["lib".into()],
            liked: vec!["liked".into()],
            disliked: Vec::new(),
        };
        d.playlists = vec![playlist(
            Some(PL1),
            "Road Trip",
            &["listed", "podcast", "bare"],
        )];
        let items = items(&d, &config(1));
        assert_eq!(
            ids(&items.tracks),
            ["YV:lib", "YV:liked", "YV:listed", "YV:top"]
        );
        assert!(items.albums.is_empty());
        let listed = &items.tracks["YV:listed"];
        assert_eq!(listed.source, Source::Youtube);
        assert_eq!(listed.playlists, ["Road Trip"]);
        assert_eq!(listed.name.as_deref(), Some("Song listed"));
        assert_eq!(listed.artist.as_deref(), Some("Chill Out Radio"));
        assert_eq!(listed.extra["kind"], "music_video");
        assert_eq!(listed.extra["channel"], "Chill Out Radio - Topic");
        assert_eq!(items.tracks["YV:top"].extra["plays"], 7);
    }

    #[test]
    fn an_unclassified_playlist_item_is_an_item_only_with_include_unknown() {
        let mut bare = video("bare", VideoKind::Unknown, 0, "");
        bare.title = None;
        bare.artists.clear();
        bare.channel = None;
        bare.youtube_title = None;
        bare.title_source = TitleSource::None;
        let mut d = data(vec![bare]);
        d.playlists = vec![playlist(Some(PL1), "Road Trip", &["bare"])];
        assert!(items(&d, &config(100)).tracks.is_empty());
        let with = YoutubeConfig {
            include_unknown: true,
            ..config(100)
        };
        let items = items(&d, &with);
        let item = &items.tracks["YV:bare"];
        assert_eq!((item.name.as_deref(), item.artist.as_deref()), (None, None));
    }

    #[test]
    fn a_removed_video_among_the_most_played_is_an_item_without_a_name() {
        let mut gone = video("gone", VideoKind::MusicVideo, 4, "2024-01-01T00:00:00.000Z");
        gone.title = None;
        gone.artists.clear();
        gone.channel = None;
        gone.removed = true;
        let items = items(&data(vec![gone]), &config(100));
        assert_eq!(items.tracks["YV:gone"].name, None);
    }

    #[test]
    fn only_pushed_playlists_bring_their_items_into_scope() {
        let mut ambiguous = playlist(Some(PL2), "Mix", &["amb"]);
        ambiguous.items_source = ItemsSource::AmbiguousTitle;
        let mut d = data(vec![
            video("kept", VideoKind::Song, 0, ""),
            video("skipped", VideoKind::Song, 0, ""),
            video("amb", VideoKind::Song, 0, ""),
            video("later", VideoKind::Song, 0, ""),
            video("orphan", VideoKind::Song, 0, ""),
        ]);
        d.playlists = vec![
            playlist(Some(PL1), "Kept", &["kept", "kept"]),
            playlist(Some(PL2), "Skipped", &["skipped"]),
            ambiguous,
            playlist(Some(PL1), "Watch later", &["later"]),
            playlist(None, "Old file", &["orphan"]),
        ];
        let config = YoutubeConfig {
            skip_playlists: vec![PL2.to_owned()],
            ..config(100)
        };
        let pushed: Vec<&str> = d
            .playlists
            .iter()
            .filter(|p| playlist_is_pushed(p, &config))
            .map(|p| p.title.as_str())
            .collect();
        assert_eq!(pushed, ["Kept"]);
        let items = items(&d, &config);
        assert_eq!(ids(&items.tracks), ["YV:kept"]);
        assert_eq!(items.tracks["YV:kept"].playlists, ["Kept"]);
    }

    fn channel(id: &str, name: &str, kind: ChannelKind, evidence: &[&str]) -> Channel {
        Channel {
            channel_id: id.to_owned(),
            name: name.to_owned(),
            url: None,
            kind,
            evidence: evidence.iter().map(|e| (*e).to_owned()).collect(),
            music_videos: 0,
        }
    }

    #[test]
    fn artists_are_the_subscriptions_with_music_evidence() {
        let mut own = video("own", VideoKind::MusicVideo, 2, "2024-01-01T00:00:00.000Z");
        own.title = Some("Live Session".into());
        own.channel = Some(ChannelRef {
            name: "Jane Doe".into(),
            channel_id: Some("UCjane".into()),
        });
        let mut topic1 = video("t1", VideoKind::Song, 9, "2024-01-01T00:00:00.000Z");
        topic1.title = Some("Hit".into());
        topic1.channel = Some(ChannelRef {
            name: "Jane Doe - Topic".into(),
            channel_id: Some("UCjanetopic".into()),
        });
        let mut topic2 = topic1.clone();
        topic2.video_id = "t2".into();
        topic2.title = Some("Deep Cut".into());
        topic2.plays = 1;
        let mut d = data(vec![own, topic1, topic2]);
        d.subscriptions = vec![
            channel(
                "UCjane",
                "jane doe ",
                ChannelKind::Artist,
                &[evidence::MUSIC_VIDEOS, evidence::TOPIC_NAMESAKE],
            ),
            channel(
                "UCvevo",
                "BandVEVO",
                ChannelKind::Artist,
                &[evidence::VEVO_CHANNEL],
            ),
            channel("UCrace", "Racing Driver", ChannelKind::Channel, &[]),
        ];
        let items = items(&d, &config(100));
        assert_eq!(ids(&items.artists), ["YC:UCjane", "YC:UCvevo"]);
        let jane = &items.artists["YC:UCjane"];
        assert_eq!(jane.name.as_deref(), Some("jane doe"));
        assert_eq!(jane.context_titles, ["Hit", "Live Session", "Deep Cut"]);
        assert_eq!(jane.extra["channel"], "jane doe ");
        let band = &items.artists["YC:UCvevo"];
        assert_eq!(band.name.as_deref(), Some("Band"));
        assert!(band.context_titles.is_empty());
    }

    #[test]
    fn artist_names_drop_the_topic_and_vevo_suffixes() {
        assert_eq!(artist_name("Jane Doe - Topic"), "Jane Doe");
        assert_eq!(artist_name("BandVEVO "), "Band");
        assert_eq!(artist_name("Plain Channel"), "Plain Channel");
        assert_eq!(artist_name("BandOfficial"), "BandOfficial");
    }

    #[test]
    fn a_video_item_id_reads_back_as_its_video_id() {
        assert_eq!(
            video_id_of(&video_item_id("abcDEF12345")),
            Some("abcDEF12345")
        );
        assert_eq!(video_id_of(&channel_item_id("UCabc")), None);
        assert_eq!(video_id_of("TR:1"), None);
    }

    #[test]
    fn the_llm_record_carries_every_context_field_select_writes() {
        let mut top = video("top", VideoKind::MusicVideo, 7, "2024-01-01T00:00:00.000Z");
        top.youtube_title = Some("Chill Out Radio - Song top (Official Video)".into());
        let mut d = data(vec![top]);
        d.subscriptions = vec![channel(
            "UCtopic",
            "Chill Out Radio - Topic",
            ChannelKind::Artist,
            &[evidence::TOPIC_CHANNEL],
        )];
        let items = items(&d, &config(1));
        let track = BatchItem::new(IdKind::Track, &items.tracks["YV:top"], &[]).record;
        assert_eq!(
            track["youtube_title"],
            "Chill Out Radio - Song top (Official Video)"
        );
        assert_eq!(track["channel"], "Chill Out Radio - Topic");
        assert_eq!(track["kind"], "music_video");
        assert_eq!(track["plays"], 7);
        let artist = BatchItem::new(IdKind::Artist, &items.artists["YC:UCtopic"], &[]).record;
        assert_eq!(artist["channel"], "Chill Out Radio - Topic");
        assert_eq!(artist["name"], "Chill Out Radio");
    }

    #[test]
    fn the_push_decision_names_why_a_playlist_is_not_pushed() {
        let skip = YoutubeConfig {
            skip_playlists: vec!["PLskip".into()],
            ..YoutubeConfig::default()
        };
        let mut no_file = playlist(Some("PLnone"), "Empty", &[]);
        no_file.items_source = ItemsSource::None;
        let mut shared = playlist(Some("PLmix"), "Mix", &[]);
        shared.items_source = ItemsSource::AmbiguousTitle;
        // A list YouTube keeps is named as such before its missing id.
        let mut later = playlist(None, "Watch later", &[]);
        later.items_source = ItemsSource::None;
        for (list, want) in [
            (playlist(Some("PLok"), "Road Trip", &[]), Ok("PLok")),
            (later, Err(NotPushed::Special(Special::WatchLater))),
            (playlist(None, "Old List", &[]), Err(NotPushed::NoId)),
            (no_file, Err(NotPushed::NoItemsFile)),
            (shared, Err(NotPushed::SharedItemsFile)),
            (
                playlist(Some("PLskip"), "Skipped", &[]),
                Err(NotPushed::Skipped),
            ),
        ] {
            assert_eq!(push_decision(&list, &skip), want, "{}", list.title);
            assert_eq!(playlist_is_pushed(&list, &skip), want.is_ok());
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

    /// Every `YV:<id>` of `ids` matched to `spotify:track:<id>`.
    fn matched_all(ids: &[&str]) -> Matches {
        let mut m = Matches::default();
        for id in ids {
            let item = video_item_id(id);
            let uri = format!("spotify:track:{id}");
            m.tracks.insert(item.clone(), entry(&item, Some(&uri)));
        }
        m
    }

    fn full(youtube: YoutubeConfig) -> Config {
        Config {
            youtube,
            ..Config::default()
        }
    }

    #[test]
    fn a_pushed_playlist_sends_its_matched_music_in_csv_order() {
        let mut d = data(vec![
            video("b", VideoKind::Song, 0, ""),
            video("a", VideoKind::MusicVideo, 0, ""),
            video("talk", VideoKind::Video, 0, ""),
            video("open", VideoKind::Song, 0, ""),
        ]);
        let mut ambiguous = playlist(Some(PL2), "Mix", &["a"]);
        ambiguous.items_source = ItemsSource::AmbiguousTitle;
        d.playlists = vec![
            playlist(Some(PL1), "Road Trip", &["b", "talk", "a", "b", "open"]),
            ambiguous,
            playlist(Some(PL1), "Watch later", &["a"]),
            playlist(None, "Old file", &["a"]),
        ];
        let mut m = matched_all(&["a", "b", "talk"]);
        m.tracks.insert("YV:open".into(), entry("YV:open", None));
        let wanted = playlists(&d, &m, &full(config(100)));
        assert_eq!(wanted.len(), 1, "{wanted:?}");
        let w = &wanted[0];
        assert_eq!(w.key, format!("YP:{PL1}"));
        assert_eq!(w.name, "Road Trip");
        assert_eq!(w.uris, ["spotify:track:b", "spotify:track:a"]);
        assert_eq!(w.repeats, 1);
        assert_eq!(
            w.description,
            format!("YouTube playlist \"Road Trip\": 2 of 4 videos matched. sss:YP:{PL1}")
        );
    }

    #[test]
    fn a_playlist_with_nothing_matched_or_skipped_by_id_is_not_planned() {
        let mut d = data(vec![video("a", VideoKind::Song, 0, "")]);
        d.playlists = vec![
            playlist(Some(PL1), "Matched", &["a"]),
            playlist(Some(PL2), "Unmatched", &["zzz"]),
        ];
        let m = matched_all(&["a"]);
        let keys = |c: &Config| -> Vec<String> {
            playlists(&d, &m, c).into_iter().map(|w| w.key).collect()
        };
        assert_eq!(keys(&full(config(100))), [format!("YP:{PL1}")]);
        let skip = YoutubeConfig {
            skip_playlists: vec![PL1.to_owned()],
            ..config(100)
        };
        assert!(keys(&full(skip)).is_empty());
    }

    #[test]
    fn the_most_played_playlist_holds_the_matched_top_n_in_play_order() {
        let d = data(vec![
            video("v1", VideoKind::Song, 3, "2024-01-01T00:00:00.000Z"),
            video("v2", VideoKind::Song, 5, "2024-01-01T00:00:00.000Z"),
            video("v3", VideoKind::MusicVideo, 4, "2024-01-01T00:00:00.000Z"),
            video("v4", VideoKind::Song, 1, "2024-01-01T00:00:00.000Z"),
        ]);
        let m = matched_all(&["v1", "v2", "v4"]);
        let mut c = full(config(3));
        c.push.playlist_prefix = "P: ".into();
        let w = most_played_playlist(&d, &m, &c).unwrap();
        assert_eq!(w.key, MOST_PLAYED_KEY);
        assert_eq!(w.name, "P: YouTube Music: most played");
        assert_eq!(
            w.uris,
            ["spotify:track:v2", "spotify:track:v1"],
            "v3 is in the top 3 but unmatched; v4 is fourth"
        );
        assert_eq!(
            w.description,
            "Songs that have been among your 3 most played on YouTube Music when sss pushed; \
             sss adds, never removes. sss:youtube:most-played"
        );
        assert_eq!(most_played_playlist(&d, &m, &full(config(0))), None);
        assert_eq!(
            most_played_playlist(&d, &matched_all(&[]), &full(config(3))),
            None
        );
    }

    #[test]
    fn library_parts_send_matched_music_and_followed_artist_channels() {
        let mut d = data(vec![
            video("lib", VideoKind::Song, 0, ""),
            video("liked", VideoKind::MusicVideo, 0, ""),
            video("clip", VideoKind::Unknown, 0, ""),
        ]);
        d.library = Library {
            songs: vec!["lib".into(), "missing".into()],
            liked: vec!["clip".into(), "liked".into()],
            disliked: Vec::new(),
        };
        d.subscriptions = vec![
            channel("UCband", "Band - Topic", ChannelKind::Artist, &[]),
            channel("UCrace", "Racing Driver", ChannelKind::Channel, &[]),
        ];
        let mut m = matched_all(&["lib", "liked", "clip", "missing"]);
        for id in ["YC:UCband", "YC:UCrace"] {
            let uri = format!("spotify:artist:{}", &id[3..]);
            m.artists.insert(id.into(), entry(id, Some(&uri)));
        }
        assert_eq!(collection_uris(&d, &m, &config(100)), ["spotify:track:lib"]);
        assert_eq!(liked_uris(&d, &m, &config(100)), ["spotify:track:liked"]);
        let unknown_too = YoutubeConfig {
            include_unknown: true,
            ..config(100)
        };
        assert_eq!(
            liked_uris(&d, &m, &unknown_too),
            ["spotify:track:clip", "spotify:track:liked"]
        );
        assert_eq!(artist_uris(&d, &m), ["spotify:artist:UCband"]);
    }
}
