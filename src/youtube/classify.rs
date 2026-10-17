//! Telling music from video: the product of a play, the kind of a video
//! with the evidence for it, the title and artists a video title is read as
//! for matching, and the kind of a subscribed channel.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// The evidence labels [`kind`] and [`channel_kind`] write, and readers of
/// the normalized files match on. A `*_PREFIX` label is followed by its
/// parameter.
pub mod evidence {
    pub const LIBRARY: &str = "library";
    pub const TOPIC_CHANNEL: &str = "topic-channel";
    pub const API_CATEGORY_MUSIC: &str = "api-category-music";
    pub const API_TOPIC_PREFIX: &str = "api-topic:";
    pub const API_CATEGORY_PREFIX: &str = "api-category:";
    pub const VEVO_CHANNEL: &str = "vevo-channel";
    pub const TITLE_MARKER_PREFIX: &str = "title-marker:";
    pub const MUSIC_APP: &str = "music-app";
    pub const MUSIC_VIDEOS: &str = "music-videos";
    pub const TOPIC_NAMESAKE: &str = "topic-namesake";
}

/// The app a play happened in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Product {
    YoutubeMusic,
    Youtube,
}

/// `youtube_music` when the record's header says so or its address is on
/// `music.youtube.com`; `youtube` otherwise.
pub fn product(header: Option<&str>, title_url: Option<&str>) -> Product {
    let music_host = title_url
        .and_then(|u| url::Url::parse(u).ok())
        .is_some_and(|u| u.host_str() == Some("music.youtube.com"));
    if header == Some("YouTube Music") || music_host {
        Product::YoutubeMusic
    } else {
        Product::Youtube
    }
}

/// What a video is, as far as the evidence goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoKind {
    Song,
    MusicVideo,
    Video,
    Unknown,
}

impl VideoKind {
    /// `song` or `music_video`.
    pub fn is_music(self) -> bool {
        matches!(self, VideoKind::Song | VideoKind::MusicVideo)
    }
}

/// The Data API's facts about a video that bear on its kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiFacts {
    pub category_id: Option<String>,
    /// `topicDetails.topicCategories`: Wikipedia addresses.
    pub topics: Vec<String>,
}

/// Everything known about one video that the kind rules read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Signals<'a> {
    pub in_library: bool,
    /// The channel names of its records (plays, the API).
    pub channels: Vec<&'a str>,
    /// Its titles: the library's, the API's, its plays'.
    pub titles: Vec<&'a str>,
    /// At least one play has product `youtube_music`.
    pub music_app: bool,
    pub api: Option<ApiFacts>,
}

/// What a Topic channel's name ends in, after the artist.
pub const TOPIC_SUFFIX: &str = " - Topic";

/// What a VEVO channel's name ends in, after the artist.
pub const VEVO_SUFFIX: &str = "VEVO";

fn is_topic(channel: &str) -> bool {
    channel.ends_with(TOPIC_SUFFIX)
}

fn is_vevo(channel: &str) -> bool {
    channel.ends_with(VEVO_SUFFIX)
}

/// A Topic channel's artist: its name without ` - Topic`.
pub fn topic_artist(channel: &str) -> Option<&str> {
    channel.strip_suffix(TOPIC_SUFFIX)
}

/// The music topic segment of a Wikipedia topic address: its last segment
/// when that ends in `_music` or is `Music`.
fn music_topic(url: &str) -> Option<&str> {
    let segment = url.trim_end_matches('/').rsplit('/').next()?;
    (segment.ends_with("_music") || segment == "Music").then_some(segment)
}

/// The kind of a video and the evidence for it. The kind is the first rule
/// of the table that fires; the evidence lists every rule that fires, in
/// table order, so a video typed on one weak rule alone can be told apart.
pub fn kind(s: &Signals<'_>) -> (VideoKind, Vec<String>) {
    let mut fired: Vec<(VideoKind, String)> = Vec::new();
    if s.in_library {
        fired.push((VideoKind::Song, evidence::LIBRARY.to_owned()));
    }
    let topic = s.channels.iter().any(|c| is_topic(c));
    if topic {
        fired.push((VideoKind::Song, evidence::TOPIC_CHANNEL.to_owned()));
    }
    if let Some(api) = &s.api {
        if api.category_id.as_deref() == Some("10") && !topic {
            fired.push((
                VideoKind::MusicVideo,
                evidence::API_CATEGORY_MUSIC.to_owned(),
            ));
        }
        let topics: Vec<&str> = api.topics.iter().filter_map(|t| music_topic(t)).collect();
        for t in &topics {
            fired.push((
                VideoKind::MusicVideo,
                format!("{}{t}", evidence::API_TOPIC_PREFIX),
            ));
        }
        if let Some(id) = api.category_id.as_deref()
            && id != "10"
            && topics.is_empty()
        {
            fired.push((
                VideoKind::Video,
                format!("{}{id}", evidence::API_CATEGORY_PREFIX),
            ));
        }
    }
    if s.channels.iter().any(|c| is_vevo(c)) {
        fired.push((VideoKind::MusicVideo, evidence::VEVO_CHANNEL.to_owned()));
    }
    let mut music_markers: Vec<&str> = Vec::new();
    for n in s.titles.iter().flat_map(|t| markers(t)) {
        if n.music && !music_markers.contains(&n.marker) {
            music_markers.push(n.marker);
        }
    }
    for m in music_markers {
        fired.push((
            VideoKind::MusicVideo,
            format!("{}{m}", evidence::TITLE_MARKER_PREFIX),
        ));
    }
    if s.music_app {
        fired.push((VideoKind::MusicVideo, evidence::MUSIC_APP.to_owned()));
    }
    let kind = fired.first().map_or(VideoKind::Unknown, |(k, _)| *k);
    (kind, fired.into_iter().map(|(_, e)| e).collect())
}

/// One bracketed annotation a video title may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Noise {
    pub marker: &'static str,
    /// Whether the annotation says the video is music.
    pub music: bool,
}

/// The bracketed annotations removed from a video title before it is read
/// as a song title; `(…)` and `[…]` alike, case-insensitive.
pub const NOISE: [Noise; 12] = [
    Noise {
        marker: "Official Music Video",
        music: true,
    },
    Noise {
        marker: "Official Audio",
        music: true,
    },
    Noise {
        marker: "Official Visualizer",
        music: true,
    },
    Noise {
        marker: "Lyric Video",
        music: true,
    },
    Noise {
        marker: "Lyrics",
        music: true,
    },
    Noise {
        marker: "Visualizer",
        music: true,
    },
    Noise {
        marker: "Audio",
        music: true,
    },
    Noise {
        marker: "Official Video",
        music: false,
    },
    Noise {
        marker: "Official",
        music: false,
    },
    Noise {
        marker: "HD",
        music: false,
    },
    Noise {
        marker: "HQ",
        music: false,
    },
    Noise {
        marker: "4K",
        music: false,
    },
];

/// A bracketed group of a title: its byte span and inner text.
fn groups(title: &str) -> Vec<(usize, usize, &str)> {
    let mut out = Vec::new();
    let mut open: Option<(usize, char)> = None;
    for (i, c) in title.char_indices() {
        match (open, c) {
            (None, '(') => open = Some((i, ')')),
            (None, '[') => open = Some((i, ']')),
            (Some((start, close)), c) if c == close => {
                out.push((start, i + 1, title[start + 1..i].trim()));
                open = None;
            }
            _ => {}
        }
    }
    out
}

fn noise_of(inner: &str) -> Option<Noise> {
    NOISE
        .iter()
        .copied()
        .find(|n| n.marker.eq_ignore_ascii_case(inner))
}

/// The noise annotations `title` carries, in title order.
pub fn markers(title: &str) -> Vec<Noise> {
    groups(title)
        .into_iter()
        .filter_map(|(_, _, inner)| noise_of(inner))
        .collect()
}

/// The artists a `feat.`/`ft.`/`featuring` clause names.
fn featured(clause: &str) -> Option<Vec<String>> {
    let lower = clause.to_ascii_lowercase();
    let rest = ["feat.", "ft.", "featuring "]
        .iter()
        .find_map(|p| lower.starts_with(p).then(|| &clause[p.len()..]))?;
    Some(
        rest.split([',', '&'])
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// `s` with its bracketed noise and featuring groups removed, and the
/// artists those featuring groups named.
fn strip_groups(s: &str) -> (String, Vec<String>) {
    let mut kept = String::new();
    let mut artists = Vec::new();
    let mut at = 0;
    for (start, end, inner) in groups(s) {
        let feat = featured(inner);
        if noise_of(inner).is_none() && feat.is_none() {
            continue;
        }
        kept.push_str(&s[at..start]);
        at = end;
        artists.extend(feat.unwrap_or_default());
    }
    kept.push_str(&s[at..]);
    (
        kept.split_whitespace().collect::<Vec<_>>().join(" "),
        artists,
    )
}

/// `s` without a trailing ` feat. …` / ` ft. …` clause, and its artists.
fn strip_feat_tail(s: &str) -> (String, Vec<String>) {
    let lower = s.to_ascii_lowercase();
    for p in [" feat. ", " ft. ", " featuring "] {
        if let Some(i) = lower.find(p) {
            let artists = featured(s[i + 1..].trim()).unwrap_or_default();
            return (s[..i].trim().to_owned(), artists);
        }
    }
    (s.to_owned(), Vec::new())
}

const SEPARATORS: [&str; 4] = [" - ", " – ", " — ", " | "];

/// The channel name read as an artist: a trailing `VEVO`, `Official`,
/// `Music` or ` - Topic` removed.
fn channel_artist(channel: &str) -> String {
    let mut name = channel.trim();
    for suffix in [TOPIC_SUFFIX, VEVO_SUFFIX, "Official", "Music"] {
        if let Some(rest) = name.strip_suffix(suffix) {
            name = rest.trim_end();
        }
    }
    name.to_owned()
}

/// A video title read as a song title and its artists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
    pub title: String,
    pub artists: Vec<String>,
}

/// Reads the video title `title` uploaded by `channel` as a song: a Topic
/// channel's title is the song and its artist the channel's; any other is
/// cleaned of noise and split at the first separator into artist and
/// title, or, with no separator, titled whole with the channel as artist.
pub fn derive(title: &str, channel: Option<&str>) -> Derived {
    if let Some(artist) = channel.and_then(topic_artist) {
        return Derived {
            title: title.trim().to_owned(),
            artists: vec![artist.trim().to_owned()],
        };
    }
    let (cleaned, mut featured) = strip_groups(title);
    let split = SEPARATORS
        .iter()
        .filter_map(|sep| cleaned.find(sep).map(|i| (i, sep.len())))
        .min();
    let (artist, song) = match split {
        Some((i, len)) => (
            Some(cleaned[..i].trim().to_owned()),
            cleaned[i + len..].trim().to_owned(),
        ),
        None => (channel.map(channel_artist), cleaned.clone()),
    };
    let (song, tail) = strip_feat_tail(&song);
    featured.extend(tail);
    let mut artists = Vec::new();
    if let Some(artist) = artist {
        let (artist, more) = strip_feat_tail(&artist);
        if !artist.is_empty() {
            artists.push(artist);
        }
        artists.extend(more);
    }
    artists.extend(featured);
    Derived {
        title: song,
        artists,
    }
}

/// What a subscribed channel is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Artist,
    Channel,
}

/// `name` as [`channel_kind`] compares it with a Topic artist: trimmed and
/// lowercased.
pub fn fold(name: &str) -> String {
    name.trim().to_lowercase()
}

/// The kind of the subscribed channel `name` and its evidence:
/// `music_videos` is how many music videos the catalogue has from it,
/// `played_topic_artists` the artist part of every Topic channel with at
/// least one play, each [`fold`]ed. A channel with an empty name is a
/// `channel`.
pub fn channel_kind(
    name: &str,
    music_videos: u32,
    played_topic_artists: &BTreeSet<String>,
) -> (ChannelKind, Vec<String>) {
    let name = name.trim();
    if name.is_empty() {
        return (ChannelKind::Channel, Vec::new());
    }
    let mut found = Vec::new();
    if is_topic(name) {
        found.push(evidence::TOPIC_CHANNEL.to_owned());
    }
    if is_vevo(name) {
        found.push(evidence::VEVO_CHANNEL.to_owned());
    }
    if music_videos > 0 {
        found.push(evidence::MUSIC_VIDEOS.to_owned());
    }
    if played_topic_artists.contains(&fold(name)) {
        found.push(evidence::TOPIC_NAMESAKE.to_owned());
    }
    let kind = if found.is_empty() {
        ChannelKind::Channel
    } else {
        ChannelKind::Artist
    };
    (kind, found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_follows_header_or_host() {
        let music = "https://music.youtube.com/watch?v=abcDEF12345";
        let web = "https://www.youtube.com/watch?v=abcDEF12345";
        assert_eq!(
            product(Some("YouTube Music"), Some(web)),
            Product::YoutubeMusic
        );
        assert_eq!(product(None, Some(music)), Product::YoutubeMusic);
        assert_eq!(product(Some("YouTube"), Some(web)), Product::Youtube);
        assert_eq!(product(None, None), Product::Youtube);
    }

    fn api(category: Option<&str>, topics: &[&str]) -> Option<ApiFacts> {
        Some(ApiFacts {
            category_id: category.map(str::to_owned),
            topics: topics.iter().map(|t| (*t).to_owned()).collect(),
        })
    }

    #[test]
    fn kind_rules_fire_in_table_order() {
        let wiki = |s: &str| format!("https://en.wikipedia.org/wiki/{s}");
        let pop = wiki("Pop_music");
        let music = wiki("Music");
        let film = wiki("Film");
        let cases: Vec<(Signals, VideoKind, Vec<&str>)> = vec![
            (
                Signals {
                    in_library: true,
                    music_app: true,
                    ..Default::default()
                },
                VideoKind::Song,
                vec!["library", "music-app"],
            ),
            (
                Signals {
                    channels: vec!["Chill Out Radio - Topic"],
                    ..Default::default()
                },
                VideoKind::Song,
                vec!["topic-channel"],
            ),
            (
                Signals {
                    channels: vec!["Chill Out Radio - Topic"],
                    api: api(Some("10"), &[]),
                    ..Default::default()
                },
                VideoKind::Song,
                vec!["topic-channel"],
            ),
            (
                Signals {
                    channels: vec!["Chill Out Radio"],
                    api: api(Some("10"), &[]),
                    ..Default::default()
                },
                VideoKind::MusicVideo,
                vec!["api-category-music"],
            ),
            (
                Signals {
                    api: api(Some("24"), &[&pop]),
                    ..Default::default()
                },
                VideoKind::MusicVideo,
                vec!["api-topic:Pop_music"],
            ),
            (
                Signals {
                    api: api(Some("24"), &[&music]),
                    ..Default::default()
                },
                VideoKind::MusicVideo,
                vec!["api-topic:Music"],
            ),
            (
                Signals {
                    music_app: true,
                    api: api(Some("22"), &[&film]),
                    ..Default::default()
                },
                VideoKind::Video,
                vec!["api-category:22", "music-app"],
            ),
            (
                Signals {
                    channels: vec!["ChillOutVEVO"],
                    ..Default::default()
                },
                VideoKind::MusicVideo,
                vec!["vevo-channel"],
            ),
            (
                Signals {
                    titles: vec!["A - B (Official Music Video)"],
                    ..Default::default()
                },
                VideoKind::MusicVideo,
                vec!["title-marker:Official Music Video"],
            ),
            (
                Signals {
                    titles: vec!["A - B [lyric video]"],
                    ..Default::default()
                },
                VideoKind::MusicVideo,
                vec!["title-marker:Lyric Video"],
            ),
            (
                Signals {
                    titles: vec!["A - B (Official Video)", "A - B (HD)"],
                    ..Default::default()
                },
                VideoKind::Unknown,
                vec![],
            ),
            (
                Signals {
                    music_app: true,
                    ..Default::default()
                },
                VideoKind::MusicVideo,
                vec!["music-app"],
            ),
            (Signals::default(), VideoKind::Unknown, vec![]),
        ];
        for (signals, want, evidence) in cases {
            let got = kind(&signals);
            assert_eq!(
                got,
                (want, evidence.iter().map(|e| (*e).to_owned()).collect()),
                "{signals:?}"
            );
        }
    }

    #[test]
    fn a_withheld_play_fires_only_the_music_app_rule() {
        let withheld = Signals {
            music_app: true,
            ..Default::default()
        };
        assert_eq!(
            kind(&withheld),
            (VideoKind::MusicVideo, vec!["music-app".to_owned()])
        );
        let withheld_web = Signals::default();
        assert_eq!(kind(&withheld_web).0, VideoKind::Unknown);
    }

    #[test]
    fn noise_flags_are_the_music_ones() {
        let music: Vec<&str> = NOISE.iter().filter(|n| n.music).map(|n| n.marker).collect();
        assert_eq!(
            music,
            [
                "Official Music Video",
                "Official Audio",
                "Official Visualizer",
                "Lyric Video",
                "Lyrics",
                "Visualizer",
                "Audio"
            ]
        );
        assert_eq!(markers("x (4K) [Official]").len(), 2);
        assert!(markers("x (Live)").is_empty());
    }

    #[test]
    fn titles_are_read_as_songs() {
        let s = |v: &[&str]| v.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
        for (title, channel, want_title, want_artists) in [
            (
                "Calm Waters",
                Some("Chill Out Radio - Topic"),
                "Calm Waters",
                s(&["Chill Out Radio"]),
            ),
            (
                "Calm Waters (Official Video)",
                Some("Chill Out Radio"),
                "Calm Waters",
                s(&["Chill Out Radio"]),
            ),
            (
                "Chill Out Radio - Calm Waters (Official Video)",
                Some("Someone"),
                "Calm Waters",
                s(&["Chill Out Radio"]),
            ),
            (
                "Chill Out Radio | Calm Waters [HD]",
                None,
                "Calm Waters",
                s(&["Chill Out Radio"]),
            ),
            (
                "Chill Out Radio – Calm Waters",
                None,
                "Calm Waters",
                s(&["Chill Out Radio"]),
            ),
            (
                "Calm Waters",
                Some("ChillOutRadioVEVO"),
                "Calm Waters",
                s(&["ChillOutRadio"]),
            ),
            (
                "Calm Waters",
                Some("Chill Out Radio Official"),
                "Calm Waters",
                s(&["Chill Out Radio"]),
            ),
            (
                "Chill Out Radio - Calm Waters (feat. Sea Breeze & Low Tide) [Lyrics]",
                None,
                "Calm Waters",
                s(&["Chill Out Radio", "Sea Breeze", "Low Tide"]),
            ),
            (
                "Chill Out Radio ft. Sea Breeze - Calm Waters",
                None,
                "Calm Waters",
                s(&["Chill Out Radio", "Sea Breeze"]),
            ),
            (
                "Chill Out Radio - Calm Waters feat. Sea Breeze",
                None,
                "Calm Waters",
                s(&["Chill Out Radio", "Sea Breeze"]),
            ),
            ("Calm Waters (Live)", None, "Calm Waters (Live)", s(&[])),
        ] {
            assert_eq!(
                derive(title, channel),
                Derived {
                    title: want_title.to_owned(),
                    artists: want_artists
                },
                "{title}"
            );
        }
    }

    #[test]
    fn channel_kinds_by_evidence() {
        let none = BTreeSet::new();
        let played: BTreeSet<String> = [fold(" Chill Out Radio ")].into();
        assert_eq!(
            channel_kind("Chill Out Radio - Topic", 0, &none),
            (ChannelKind::Artist, vec!["topic-channel".to_owned()])
        );
        assert_eq!(
            channel_kind("ChillOutVEVO", 0, &none),
            (ChannelKind::Artist, vec!["vevo-channel".to_owned()])
        );
        assert_eq!(
            channel_kind("Chill Out Radio", 2, &none),
            (ChannelKind::Artist, vec!["music-videos".to_owned()])
        );
        assert_eq!(
            channel_kind(" chill out RADIO ", 0, &played),
            (ChannelKind::Artist, vec!["topic-namesake".to_owned()])
        );
        assert_eq!(
            channel_kind("Racing Daily", 0, &played),
            (ChannelKind::Channel, vec![])
        );
        let blank: BTreeSet<String> = [fold("")].into();
        assert_eq!(
            channel_kind("  ", 3, &blank),
            (ChannelKind::Channel, vec![])
        );
    }
}
