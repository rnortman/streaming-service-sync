//! The YouTube parts of `report.md`: the export summary — history span and
//! gaps, plays by product and kind, the most played, what could not be
//! classified, the per-playlist counts — and the playlists not pushed.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{Config, YoutubeConfig};
use crate::push::Matches;
use crate::report::{line, md};
use crate::youtube::classify::{Product, VideoKind, evidence};
use crate::youtube::model::{self, Normalized, Playlist, Special, Video};
use crate::youtube::select::{self, NotPushed};
use crate::youtube::takeout::ItemsSource;

/// Rows of the most-played lists.
const TOP: usize = 20;
/// Channels listed per by-channel table; the rest are summed in one line.
const CHANNELS: usize = 30;
/// The shortest stretch without plays the summary names, in days.
const GAP_DAYS: i64 = 30;

/// The watch address of `video_id`.
pub fn watch_url(video_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={video_id}")
}

fn kind_label(kind: VideoKind) -> &'static str {
    match kind {
        VideoKind::Song => "song",
        VideoKind::MusicVideo => "music video",
        VideoKind::Video => "video",
        VideoKind::Unknown => "unknown",
    }
}

/// The date part of a canonical instant.
fn date(at: &str) -> &str {
    at.get(..10).unwrap_or(at)
}

/// A video with no title and no evidence that the Data API has not been
/// asked about: nothing says what it is, and `sss youtube enrich` would ask.
fn without_evidence(v: &Video) -> bool {
    v.title.is_none() && v.evidence.is_empty() && v.available.is_none()
}

/// A video the Data API was asked about and did not return.
fn gone(v: &Video) -> bool {
    v.available == Some(false)
}

/// The `### YouTube` subsection of the export summary; `runs` are the
/// complete import runs, oldest archive first.
pub fn export_summary(
    data: &Normalized,
    runs: &[String],
    config: &YoutubeConfig,
    out: &mut String,
) {
    match runs.last() {
        Some(latest) => line(
            out,
            format!("- Latest complete import run: `{latest}` (playlists, library, subscriptions)"),
        ),
        None => line(out, "- Latest complete import run: none"),
    }
    if !runs.is_empty() {
        let ids: Vec<String> = runs.iter().map(|r| format!("`{r}`")).collect();
        line(
            out,
            format!(
                "- History from {} import runs, unioned: {}",
                runs.len(),
                ids.join(", ")
            ),
        );
    }
    history_lines(data, out);
    catalogue_lines(data, out);
    most_played(data, config, out);
    unclassified(data, out);
    let removed_music_app = data
        .videos
        .values()
        .filter(|v| v.removed && music_app_only(v))
        .count();
    by_channel(
        data,
        |v| music_app_only(v) && !v.removed,
        "Typed music by the Music app alone",
        "Typed music because they were played in the Music app, which also plays podcasts and \
         shared videos; a podcast here is a `sss match reject YV:…` away, or `sss youtube \
         enrich` settles it.",
        (removed_music_app > 0).then(|| {
            format!(
                "{removed_music_app} removed videos are typed this way too; Takeout withheld \
                 their channel, so they are not in the table."
            )
        }),
        out,
    );
    playlists(data, out);
}

/// The "Not classified" table: `unknown` videos YouTube still has, by
/// channel; `enrich` is named only while some of them are unasked, and the
/// gone ones are counted on a line of their own.
fn unclassified(data: &Normalized, out: &mut String) {
    let unknown = || {
        data.videos
            .values()
            .filter(|v| v.kind == VideoKind::Unknown)
    };
    let unasked = unknown().any(|v| v.available.is_none());
    let vanished = unknown().filter(|v| gone(v)).count();
    let remedy = if unasked {
        "`sss youtube api-key` and `sss youtube enrich` would classify those not yet asked \
         about"
    } else {
        "the YouTube Data API was asked about them and did not classify them"
    };
    by_channel(
        data,
        |v| v.kind == VideoKind::Unknown && !gone(v),
        "Not classified",
        &format!(
            "Videos no Takeout signal types as music or not (`unknown`), by channel. They are \
             neither matched nor pushed; {remedy}, and `[youtube] include_unknown = true` in \
             config.toml matches them as they are."
        ),
        (vanished > 0).then(|| {
            format!(
                "{vanished} more unclassified videos are gone from YouTube (the Data API did \
                 not return them); nothing will classify them, so they are not in the table."
            )
        }),
        out,
    );
}

fn history_lines(data: &Normalized, out: &mut String) {
    let counted: Vec<&model::Play> = data.history.iter().filter(|p| p.counts()).collect();
    match (counted.first(), counted.last()) {
        (Some(first), Some(last)) => line(
            out,
            format!(
                "- History: {} records, {} plays from {} to {}",
                data.history.len(),
                counted.len(),
                date(&first.at),
                date(&last.at)
            ),
        ),
        _ => line(
            out,
            format!("- History: {} records, no plays", data.history.len()),
        ),
    }
    for (before, after) in model::gaps(&data.history, GAP_DAYS) {
        line(
            out,
            format!("  - no plays from {} to {}", date(&before), date(&after)),
        );
    }
    let music_app = counted
        .iter()
        .filter(|p| p.product == Product::YoutubeMusic)
        .count();
    line(
        out,
        format!(
            "- Plays by app: YouTube Music {music_app}, YouTube {}",
            counted.len() - music_app
        ),
    );
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for p in &counted {
        if let Some(v) = p.video_id.as_deref().and_then(|id| data.videos.get(id)) {
            *by_kind.entry(kind_label(v.kind)).or_default() += 1;
        }
    }
    let kinds: Vec<String> = [
        VideoKind::Song,
        VideoKind::MusicVideo,
        VideoKind::Video,
        VideoKind::Unknown,
    ]
    .map(kind_label)
    .iter()
    .map(|k| format!("{k} {}", by_kind.get(k).copied().unwrap_or(0)))
    .collect();
    line(out, format!("- Plays by kind: {}", kinds.join(", ")));
    let removed: Vec<&Video> = data.videos.values().filter(|v| v.removed).collect();
    let removed_plays: u32 = removed.iter().map(|v| v.plays).sum();
    line(
        out,
        format!(
            "- {} removed videos, {removed_plays} plays, titles withheld by Takeout",
            removed.len()
        ),
    );
}

fn catalogue_lines(data: &Normalized, out: &mut String) {
    let count = |k: VideoKind| data.videos.values().filter(|v| v.kind == k).count();
    line(
        out,
        format!(
            "- Videos: {} ({} songs, {} music videos, {} videos, {} unknown)",
            data.videos.len(),
            count(VideoKind::Song),
            count(VideoKind::MusicVideo),
            count(VideoKind::Video),
            count(VideoKind::Unknown)
        ),
    );
    line(
        out,
        format!("- Library songs: {}", data.library.songs.len()),
    );
    let has_liked_list = data
        .playlists
        .iter()
        .any(|p| matches!(p.special, Some(Special::LikedVideos | Special::LikedMusic)));
    if has_liked_list {
        line(out, format!("- Liked: {}", data.library.liked.len()));
    } else {
        line(out, "- Liked: no likes in this archive");
    }
    line(out, format!("- Playlists: {}", data.playlists.len()));
    let artists = data
        .subscriptions
        .iter()
        .filter(|c| c.kind == crate::youtube::classify::ChannelKind::Artist)
        .count();
    line(
        out,
        format!(
            "- Subscriptions: {} ({artists} artists)",
            data.subscriptions.len()
        ),
    );
}

fn most_played(data: &Normalized, config: &YoutubeConfig, out: &mut String) {
    let songs = select::by_plays(data, config);
    if songs.is_empty() {
        return;
    }
    line(out, "");
    line(out, "#### Most played songs");
    line(out, "");
    line(
        out,
        "Ranked as the most-played playlist ranks them, over the videos push may send. Plays \
         count per video: a song heard both as its Topic entry and as its official video is \
         two rows.",
    );
    line(out, "");
    for v in songs.iter().take(TOP) {
        let title = v.title.as_deref().map_or("untitled".to_owned(), md);
        let by = v
            .artists
            .first()
            .map(|a| format!(" by {}", md(a)))
            .unwrap_or_default();
        line(
            out,
            format!(
                "- \"{title}\"{by} `{}`: {} plays",
                select::video_item_id(&v.video_id),
                v.plays
            ),
        );
    }
    let mut artists: BTreeMap<&str, u32> = BTreeMap::new();
    for v in &songs {
        if let Some(a) = v.artists.first() {
            *artists.entry(a.as_str()).or_default() += v.plays;
        }
    }
    let mut artists: Vec<(&str, u32)> = artists.into_iter().collect();
    artists.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    if artists.is_empty() {
        return;
    }
    line(out, "");
    line(out, "#### Most played artists");
    line(out, "");
    for (name, plays) in artists.iter().take(TOP) {
        line(out, format!("- {}: {plays} plays", md(name)));
    }
}

/// Whether the Music app is the only evidence that `video` is music.
fn music_app_only(video: &Video) -> bool {
    video
        .evidence
        .iter()
        .map(String::as_str)
        .eq([evidence::MUSIC_APP])
}

/// A table of the videos `pick` selects, grouped by channel name, largest
/// first, then `note`; nothing when it selects none and there is no note.
fn by_channel(
    data: &Normalized,
    pick: impl Fn(&Video) -> bool,
    heading: &str,
    intro: &str,
    note: Option<String>,
    out: &mut String,
) {
    let mut groups: BTreeMap<Option<&str>, (usize, u32)> = BTreeMap::new();
    for v in data.videos.values().filter(|v| pick(v)) {
        let (videos, plays) = groups
            .entry(v.channel.as_ref().map(|c| c.name.as_str()))
            .or_default();
        *videos += 1;
        *plays += v.plays;
    }
    if groups.is_empty() && note.is_none() {
        return;
    }
    let mut groups: Vec<(Option<&str>, (usize, u32))> = groups.into_iter().collect();
    groups.sort_by(|a, b| (b.1).cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    line(out, "");
    line(out, format!("#### {heading}"));
    line(out, "");
    line(out, intro);
    if !groups.is_empty() {
        line(out, "");
        line(out, "| channel | videos | plays |");
        line(out, "|---|---:|---:|");
    }
    for (channel, (videos, plays)) in groups.iter().take(CHANNELS) {
        let name = channel.map_or(
            "no channel (seen only in a playlist, or removed)".to_owned(),
            md,
        );
        line(out, format!("| {name} | {videos} | {plays} |"));
    }
    if groups.len() > CHANNELS {
        let rest = &groups[CHANNELS..];
        let videos: usize = rest.iter().map(|g| g.1.0).sum();
        let plays: u32 = rest.iter().map(|g| g.1.1).sum();
        line(
            out,
            format!("| {} more channels | {videos} | {plays} |", rest.len()),
        );
    }
    if let Some(note) = note {
        line(out, "");
        line(out, note);
    }
}

fn playlists(data: &Normalized, out: &mut String) {
    if data.playlists.is_empty() {
        return;
    }
    line(out, "");
    line(out, "#### Playlists");
    line(out, "");
    line(
        out,
        "Per playlist: its distinct videos, how many are music, how many have no evidence \
         (seen nowhere but the playlist and not yet asked about: no title, never matched; \
         `sss youtube enrich` recovers those), and how many are gone from YouTube (the Data \
         API did not return them).",
    );
    line(out, "");
    for p in &data.playlists {
        let ids: BTreeSet<&str> = p.items.iter().map(|i| i.video_id.as_str()).collect();
        let videos: Vec<&Video> = ids.iter().filter_map(|id| data.videos.get(*id)).collect();
        let music = videos.iter().filter(|v| v.kind.is_music()).count();
        let bare = videos.iter().filter(|v| without_evidence(v)).count();
        let vanished = videos.iter().filter(|v| gone(v)).count();
        let id = p
            .playlist_id
            .as_deref()
            .map_or("no id".to_owned(), |id| format!("`{id}`"));
        let mut text = format!(
            "- {} {id}: {} videos, {music} music, {bare} with no evidence",
            md(&p.title),
            ids.len()
        );
        if vanished > 0 {
            text.push_str(&format!(", {vanished} gone from YouTube"));
        }
        match p.items_source {
            ItemsSource::File => {}
            ItemsSource::None => text.push_str("; items: none (no items file)"),
            ItemsSource::AmbiguousTitle => {
                text.push_str("; items: ambiguous\\_title (items file shared)");
            }
        }
        line(out, text);
    }
}

/// Why `playlist` is not pushed, or `None` when it is planned.
fn not_pushed_reason(
    playlist: &Playlist,
    planned: &BTreeSet<String>,
    config: &Config,
) -> Option<String> {
    let id = match select::push_decision(playlist, &config.youtube) {
        Ok(id) => id,
        Err(why) => {
            return Some(
                match why {
                    NotPushed::Special(Special::WatchLater) => "Watch later is never pushed",
                    NotPushed::Special(Special::LikedVideos | Special::LikedMusic) => {
                        "a liked list: its music goes to Liked Songs under `[push] liked_songs`"
                    }
                    NotPushed::NoId => {
                        "an items file no row of playlists.csv names, so it has no playlist id"
                    }
                    NotPushed::SharedItemsFile => {
                        "its items file is shared with a playlist of the same file name, so its \
                         items are unknown; rename one in YouTube and export again"
                    }
                    NotPushed::NoItemsFile => "no items file in the archive",
                    NotPushed::Skipped => "in `[youtube] skip_playlists`",
                }
                .to_owned(),
            );
        }
    };
    (!planned.contains(&select::playlist_key(id))).then(|| "no matched music".to_owned())
}

/// The `## YouTube playlists not pushed` section.
pub fn not_pushed(data: &Normalized, matches: &Matches, config: &Config, out: &mut String) {
    line(out, "## YouTube playlists not pushed");
    line(out, "");
    let planned: BTreeSet<String> = select::playlists(data, matches, config)
        .into_iter()
        .map(|w| w.key)
        .collect();
    let mut any = false;
    for p in &data.playlists {
        if let Some(why) = not_pushed_reason(p, &planned, config) {
            any = true;
            let id = p
                .playlist_id
                .as_deref()
                .map_or(String::new(), |id| format!(" `{id}`"));
            line(out, format!("- {}{id}: {why}", md(&p.title)));
        }
    }
    if !any {
        line(out, "None.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matching::model::{MatchEntry, Status};
    use crate::youtube::classify::ChannelKind;
    use crate::youtube::model::{
        Action, Channel, ChannelRef, Library, Play, PlaylistItem, TitleSource,
    };

    const PL_ROAD: &str = "PLaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PL_SKIP: &str = "PLssssssssssssssssssssssssssssssss";
    const PL_MIX: &str = "PLmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm";
    const PL_EMPTY: &str = "PLeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    const PL_LATER: &str = "PLwwwwwwwwwwwwwwwwwwwwwwwwwwwwwwww";

    fn play(at: &str, id: &str, product: Product, channel: Option<&str>) -> Play {
        Play {
            at: at.to_owned(),
            video_id: Some(id.to_owned()),
            url: Some(watch_url(id)),
            product,
            action: Action::Watched,
            title: channel.map(|_| format!("Song {id}")),
            channel: channel.map(|name| ChannelRef {
                name: name.to_owned(),
                channel_id: None,
            }),
            removed: channel.is_none(),
            ad: false,
        }
    }

    fn video(
        id: &str,
        kind: VideoKind,
        evidence: &[&str],
        plays: u32,
        channel: Option<&str>,
    ) -> Video {
        Video {
            video_id: id.to_owned(),
            title: channel.map(|_| format!("Song {id}")),
            artists: channel
                .map(|c| vec![c.trim_end_matches(" - Topic").to_owned()])
                .unwrap_or_default(),
            album: None,
            duration_s: None,
            channel: channel.map(|name| ChannelRef {
                name: name.to_owned(),
                channel_id: None,
            }),
            kind,
            evidence: evidence.iter().map(|e| (*e).to_owned()).collect(),
            title_source: if channel.is_some() {
                TitleSource::History
            } else {
                TitleSource::None
            },
            youtube_title: channel.map(|_| format!("Song {id}")),
            plays,
            music_app_plays: plays,
            removed: channel.is_none() && plays > 0,
            first_played: None,
            last_played: None,
            in_library: false,
            liked: false,
            available: None,
            category_id: None,
            topics: Vec::new(),
        }
    }

    fn playlist(id: Option<&str>, title: &str, source: ItemsSource, items: &[&str]) -> Playlist {
        Playlist {
            playlist_id: id.map(str::to_owned),
            title: title.to_owned(),
            created: None,
            updated: None,
            order: None,
            visibility: None,
            special: Special::of_title(title),
            items_source: source,
            items: items
                .iter()
                .map(|v| PlaylistItem {
                    video_id: (*v).to_owned(),
                    added_at: None,
                })
                .collect(),
        }
    }

    /// Two Topic songs (one played in the YouTube app), an unknown video, a
    /// podcast typed by the Music app, a removed video, a video seen only in
    /// a playlist; a 60-day gap; five playlists of every kind not pushed and
    /// one pushed.
    fn data() -> Normalized {
        let topic = Some("The Band - Topic");
        let history = vec![
            play(
                "2026-01-01T00:00:00.000Z",
                "song0000001",
                Product::YoutubeMusic,
                topic,
            ),
            play(
                "2026-01-02T00:00:00.000Z",
                "song0000001",
                Product::YoutubeMusic,
                topic,
            ),
            play(
                "2026-03-03T00:00:00.000Z",
                "song0000002",
                Product::Youtube,
                topic,
            ),
            play(
                "2026-03-04T00:00:00.000Z",
                "unkn0000001",
                Product::Youtube,
                Some("Paint Club"),
            ),
            play(
                "2026-03-05T00:00:00.000Z",
                "podc0000001",
                Product::YoutubeMusic,
                Some("Talk Show"),
            ),
            play(
                "2026-03-06T00:00:00.000Z",
                "gone0000001",
                Product::YoutubeMusic,
                None,
            ),
        ];
        let videos = [
            video(
                "song0000001",
                VideoKind::Song,
                &[evidence::TOPIC_CHANNEL, evidence::MUSIC_APP],
                2,
                topic,
            ),
            video(
                "song0000002",
                VideoKind::Song,
                &[evidence::TOPIC_CHANNEL],
                1,
                topic,
            ),
            video(
                "unkn0000001",
                VideoKind::Unknown,
                &[],
                1,
                Some("Paint Club"),
            ),
            video(
                "podc0000001",
                VideoKind::MusicVideo,
                &[evidence::MUSIC_APP],
                1,
                Some("Talk Show"),
            ),
            video(
                "gone0000001",
                VideoKind::MusicVideo,
                &[evidence::MUSIC_APP],
                1,
                None,
            ),
            video("bare0000001", VideoKind::Unknown, &[], 0, None),
        ];
        Normalized {
            history,
            videos: videos
                .into_iter()
                .map(|v| (v.video_id.clone(), v))
                .collect(),
            playlists: vec![
                playlist(
                    Some(PL_ROAD),
                    "Road Trip",
                    ItemsSource::File,
                    &["song0000001", "bare0000001", "song0000001"],
                ),
                playlist(
                    Some(PL_SKIP),
                    "Converted",
                    ItemsSource::File,
                    &["song0000002"],
                ),
                playlist(Some(PL_MIX), "Mix: 2024", ItemsSource::AmbiguousTitle, &[]),
                playlist(Some(PL_EMPTY), "Empty", ItemsSource::None, &[]),
                playlist(
                    Some(PL_LATER),
                    "Watch later",
                    ItemsSource::File,
                    &["song0000002"],
                ),
                playlist(None, "Old List", ItemsSource::File, &["song0000002"]),
                playlist(
                    Some("PLnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
                    "Nothing Matched",
                    ItemsSource::File,
                    &["unkn0000001"],
                ),
            ],
            subscriptions: vec![Channel {
                channel_id: "UCbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
                name: "The Band".to_owned(),
                url: None,
                kind: ChannelKind::Artist,
                evidence: vec![evidence::TOPIC_NAMESAKE.to_owned()],
                music_videos: 0,
            }],
            library: Library::default(),
        }
    }

    #[test]
    fn the_export_summary_counts_spans_gaps_and_tables() {
        let mut out = String::new();
        export_summary(
            &data(),
            &["20260101T000000Z".into(), "20260301T000000Z".into()],
            &YoutubeConfig::default(),
            &mut out,
        );
        for expected in [
            "- Latest complete import run: `20260301T000000Z` (playlists, library, subscriptions)\n",
            "- History from 2 import runs, unioned: `20260101T000000Z`, `20260301T000000Z`\n",
            "- History: 6 records, 6 plays from 2026-01-01 to 2026-03-06\n",
            "  - no plays from 2026-01-02 to 2026-03-03\n",
            "- Plays by app: YouTube Music 4, YouTube 2\n",
            "- Plays by kind: song 3, music video 2, video 0, unknown 1\n",
            "- 1 removed videos, 1 plays, titles withheld by Takeout\n",
            "- Videos: 6 (2 songs, 2 music videos, 0 videos, 2 unknown)\n",
            "- Liked: no likes in this archive\n",
            "- Playlists: 7\n",
            "- Subscriptions: 1 (1 artists)\n",
            "- \"Song song0000001\" by The Band `YV:song0000001`: 2 plays\n",
            "- The Band: 3 plays\n",
            "| Paint Club | 1 | 1 |\n",
            "| no channel (seen only in a playlist, or removed) | 1 | 0 |\n",
            "| Talk Show | 1 | 1 |\n",
            "#### Typed music by the Music app alone",
            "1 removed videos are typed this way too",
            "- Road Trip `PLaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa`: 2 videos, 1 music, 1 with no evidence\n",
            "- Mix: 2024 `PLmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm`: 0 videos, 0 music, 0 with no evidence; \
             items: ambiguous\\_title",
            "- Empty `PLeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee`: 0 videos, 0 music, 0 with no evidence; \
             items: none",
            "- Old List no id: 1 videos",
        ] {
            assert!(out.contains(expected), "{expected}\n---\n{out}");
        }
        // The song typed by Topic and the Music app is not a Music-app-only video.
        let music_app = &out[out.find("#### Typed music").unwrap()..];
        assert!(!music_app.contains("The Band - Topic"), "{music_app}");
        // A liked list present: the count instead of the sentence.
        let mut liked = data();
        liked.playlists.push(playlist(
            Some("PLllllllllllllllllllllllllllllllll"),
            "Liked videos",
            ItemsSource::File,
            &["song0000001"],
        ));
        liked.library.liked = vec!["song0000001".into()];
        let mut out = String::new();
        export_summary(&liked, &[], &YoutubeConfig::default(), &mut out);
        assert!(
            out.contains("- Latest complete import run: none\n"),
            "{out}"
        );
        assert!(out.contains("- Liked: 1\n"), "{out}");
        // The unclassified video is ranked only when push may send it.
        assert!(!out.contains("`YV:unkn0000001`: 1 plays"), "{out}");
        let unknown = YoutubeConfig {
            include_unknown: true,
            ..YoutubeConfig::default()
        };
        let mut out = String::new();
        export_summary(&data(), &[], &unknown, &mut out);
        assert!(out.contains("`YV:unkn0000001`: 1 plays\n"), "{out}");
    }

    #[test]
    fn a_video_the_api_did_not_return_is_gone_not_waiting_for_enrich() {
        let mut gone = data();
        gone.videos.get_mut("bare0000001").unwrap().available = Some(false);
        let mut out = String::new();
        export_summary(&gone, &[], &YoutubeConfig::default(), &mut out);
        for expected in [
            "- Road Trip `PLaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa`: 2 videos, 1 music, 0 with no \
             evidence, 1 gone from YouTube\n",
            "`sss youtube enrich` would classify those not yet asked about",
            "| Paint Club | 1 | 1 |\n",
            "1 more unclassified videos are gone from YouTube",
        ] {
            assert!(out.contains(expected), "{expected}\n---\n{out}");
        }
        assert!(!out.contains("| no channel"), "{out}");
        // Every unclassified video asked about: enrich is not offered.
        gone.videos.get_mut("unkn0000001").unwrap().available = Some(true);
        let mut out = String::new();
        export_summary(&gone, &[], &YoutubeConfig::default(), &mut out);
        assert!(
            out.contains("the YouTube Data API was asked about them and did not classify them"),
            "{out}"
        );
        assert!(!out.contains("would classify"), "{out}");
    }

    #[test]
    fn a_dense_history_lists_no_gap_and_a_long_channel_table_is_cut() {
        let mut d = data();
        d.history.retain(|p| p.at.starts_with("2026-03"));
        for n in 0..(CHANNELS + 2) {
            let v = video(
                &format!("many{n:07}"),
                VideoKind::Unknown,
                &[],
                1,
                Some(&format!("Channel {n:02}")),
            );
            d.videos.insert(v.video_id.clone(), v);
        }
        let mut out = String::new();
        export_summary(&d, &[], &YoutubeConfig::default(), &mut out);
        assert!(!out.contains("no plays from"), "{out}");
        // Paint Club, the playlist-only video and 32 channels: 34 groups, 30
        // shown; the playlist-only video, never played, sorts last.
        assert!(out.contains("| 4 more channels | 4 | 3 |\n"), "{out}");
    }

    #[test]
    fn every_playlist_not_pushed_is_listed_with_its_reason() {
        let mut config = Config::default();
        config.youtube.skip_playlists = vec![PL_SKIP.into()];
        let mut matches = Matches::default();
        for id in ["song0000001", "song0000002"] {
            let item_id = select::video_item_id(id);
            matches.tracks.insert(
                item_id.clone(),
                MatchEntry {
                    item_id,
                    status: Status::Matched,
                    spotify_uri: Some(format!("spotify:track:{id:0>22}")),
                    method: None,
                    confidence: None,
                    candidates: Vec::new(),
                    queries: Vec::new(),
                    decided_at: "2026-10-15T00:00:00Z".into(),
                    session: None,
                    reason: None,
                },
            );
        }
        let mut out = String::new();
        not_pushed(&data(), &matches, &config, &mut out);
        for expected in [
            "- Converted `PLssssssssssssssssssssssssssssssss`: in `[youtube] skip_playlists`\n",
            "- Mix: 2024 `PLmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm`: its items file is shared",
            "- Empty `PLeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee`: no items file in the archive\n",
            "- Watch later `PLwwwwwwwwwwwwwwwwwwwwwwwwwwwwwwww`: Watch later is never pushed\n",
            "- Old List: an items file no row of playlists.csv names",
            "- Nothing Matched `PLnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn`: no matched music\n",
        ] {
            assert!(out.contains(expected), "{expected}\n---\n{out}");
        }
        assert!(!out.contains("Road Trip"), "{out}");

        let mut d = data();
        d.playlists.retain(|p| p.title == "Road Trip");
        let mut out = String::new();
        not_pushed(&d, &matches, &config, &mut out);
        assert!(out.ends_with("None.\n"), "{out}");
    }
}
