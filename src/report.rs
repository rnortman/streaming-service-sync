//! The `report` command: `report.md`, what a member reads after a
//! migration — the export, the matches, what needs a person, the
//! thumbs-down, what has no Spotify counterpart, what was not pushed and
//! what push did. It reads local files only.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use crate::config::{Config, PushConfig};
use crate::matching::model::{self as matches, MatchEntry, Method, Status};
use crate::matching::run::{self as matching, ReviewItem};
use crate::pandora::export;
use crate::pandora::model::{self, Normalized, SeedKind};
use crate::push::{self, Ledger, Matches, NEVER_PUSHED_KINDS, Part, PlaylistEntry, Wanted};
use crate::repo::{self, ProfilePaths};
use crate::source::Exports;
use crate::spotify::ids::{self, IdKind, SpotifyId};
use crate::youtube::model as youtube_model;
use crate::youtube::report as youtube_report;
use crate::youtube::select as youtube_select;

/// What `sss report` reports: the file it wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReportFile {
    pub path: PathBuf,
}

impl ReportFile {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        writeln!(out, "{}", self.path.display())
    }
}

/// `sss report`: writes `report.md` of profile `profile` from its
/// normalized exports, its match entries and its push ledger.
pub fn report(profile: &str, paths: &ProfilePaths, config: &Config) -> Result<ReportFile> {
    let exports = Exports::load(profile, paths)?;
    let run_id = export::latest_complete_run(paths)?.map(|m| m.run_id);
    let youtube_runs = if exports.youtube.is_some() {
        youtube_model::read_runs(paths)?
    } else {
        Vec::new()
    };
    let matches = matches::read_all_latest(paths)?;
    let ledger = Ledger::load(&paths.spotify_state())?;
    // A track in the match set only as a thumbs-down belongs to the
    // thumbs-down section, never to the review list.
    let reviewed = Config {
        push: PushConfig {
            thumbs_down_playlist: false,
            ..config.push.clone()
        },
        ..config.clone()
    };
    let review = matching::review(&matches, &matching::select(&exports, &reviewed), None);
    let doc = Document {
        profile,
        paths,
        config,
        exports: &exports,
        run_id: run_id.as_deref(),
        youtube_runs: &youtube_runs,
        matches: &matches,
        ledger: &ledger,
        review: &review.items,
    };
    let path = paths.report();
    repo::write_atomic(&path, doc.render().as_bytes())?;
    Ok(ReportFile { path })
}

/// Everything the report is written from.
struct Document<'a> {
    profile: &'a str,
    paths: &'a ProfilePaths,
    config: &'a Config,
    exports: &'a Exports,
    /// The latest complete Pandora export run.
    run_id: Option<&'a str>,
    /// The complete YouTube import runs, oldest archive first.
    youtube_runs: &'a [String],
    matches: &'a Matches,
    ledger: &'a Ledger,
    review: &'a [ReviewItem],
}

/// `text` safe to place in a Markdown line: characters Markdown would read
/// as formatting are escaped and line breaks become spaces.
pub(crate) fn md(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|' | '~' | '#' => {
                out.push('\\');
                out.push(c);
            }
            '\n' | '\r' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// The open.spotify.com address of a track, artist or album URI; anything
/// else is returned as it is.
fn open_url(uri: &str) -> String {
    SpotifyId::of_uri(uri).map_or_else(|| uri.to_owned(), |id| id.open_url())
}

/// A Markdown link to a Spotify search address. `url` may hold an unbalanced
/// `(` or `)`; the `<…>` destination keeps it inside the link.
fn search_link(text: &str, url: &str) -> String {
    format!("[{text}](<{url}>)")
}

/// `"<title>" by <artist>`, either part left out when unknown.
fn title_by(title: Option<&str>, artist: Option<&str>) -> String {
    let mut text = match title {
        Some(t) => format!("\"{}\"", md(t)),
        None => "an untitled item".to_owned(),
    };
    if let Some(a) = artist {
        text.push_str(&format!(" by {}", md(a)));
    }
    text
}

pub(crate) fn line(out: &mut String, text: impl AsRef<str>) {
    out.push_str(text.as_ref());
    out.push('\n');
}

impl Document<'_> {
    fn render(&self) -> String {
        let mut out = String::new();
        line(&mut out, format!("# {}: to Spotify", md(self.profile)));
        line(&mut out, "");
        line(
            &mut out,
            "Written by `sss report` from the data repository alone; run it again after \
             `sss match` or `sss push` to bring it up to date.",
        );
        for section in [
            Self::export_summary,
            Self::match_summary,
            Self::needs_a_human,
            Self::thumbs_down,
            Self::without_counterpart,
            Self::not_pushed,
            Self::youtube_not_pushed,
            Self::push_summary,
        ] {
            // A section about a source the profile does not hold writes
            // nothing and leaves no gap.
            let mut text = String::new();
            section(self, &mut text);
            if !text.is_empty() {
                line(&mut out, "");
                out.push_str(&text);
            }
        }
        out
    }

    fn export_summary(&self, out: &mut String) {
        line(out, "## Export");
        if let Some(e) = &self.exports.pandora {
            line(out, "");
            line(out, "### Pandora");
            line(out, "");
            self.pandora_summary(e, out);
        }
        if let Some(e) = &self.exports.youtube {
            line(out, "");
            line(out, "### YouTube");
            line(out, "");
            youtube_report::export_summary(e, self.youtube_runs, &self.config.youtube, out);
        }
    }

    fn pandora_summary(&self, e: &Normalized, out: &mut String) {
        let s = model::summary(e, self.run_id.unwrap_or("unknown"));
        line(out, format!("- Latest complete export run: `{}`", s.run_id));
        let thumbprint = e.stations.iter().filter(|s| s.is_thumbprint).count();
        let shuffle = e.stations.iter().filter(|s| s.is_shuffle).count();
        line(
            out,
            format!(
                "- Stations: {} ({thumbprint} Thumbprint, {shuffle} shuffle)",
                s.stations
            ),
        );
        line(out, format!("- Thumbs up: {}", s.thumbs_up));
        line(out, format!("- Thumbs down: {}", s.thumbs_down));
        let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
        for p in &e.playlists {
            *kinds
                .entry(p.linked_type.as_deref().unwrap_or("member's own"))
                .or_default() += 1;
        }
        let kinds: Vec<String> = kinds
            .iter()
            .map(|(k, n)| format!("{}: {n}", md(k)))
            .collect();
        let kinds = if kinds.is_empty() {
            String::new()
        } else {
            format!(" ({})", kinds.join(", "))
        };
        line(out, format!("- Playlists: {}{kinds}", s.playlists));
        line(
            out,
            format!(
                "- Collection: {} tracks, {} albums, {} artists",
                s.collection_tracks, s.collection_albums, s.collection_artists
            ),
        );
        line(
            out,
            format!(
                "- Bookmarks: {} tracks, {} albums",
                e.bookmarks.tracks.len(),
                e.bookmarks.albums.len()
            ),
        );
    }

    fn match_summary(&self, out: &mut String) {
        let items = matching::select(self.exports, self.config);
        line(out, "## Matches");
        line(out, "");
        line(
            out,
            "Counted over the items push would send with the current `[push]` settings.",
        );
        line(out, "");
        let statuses: Vec<&str> = Status::ALL.map(Status::as_str).to_vec();
        let methods: Vec<String> = Method::ALL
            .iter()
            .map(|m| format!("by {}", m.as_str()))
            .collect();
        line(
            out,
            format!(
                "| | items | {} | not matched yet | {} |",
                statuses.join(" | "),
                methods.join(" | ")
            ),
        );
        line(
            out,
            format!(
                "|---|---:|{}",
                "---:|".repeat(Status::ALL.len() + 1 + Method::ALL.len())
            ),
        );
        for kind in IdKind::ALL {
            let entries = self.matches.of(kind);
            let ids = items.of(kind).keys();
            let latest: Vec<&MatchEntry> = ids.clone().filter_map(|id| entries.get(id)).collect();
            let mut cells = vec![kind.plural().to_owned(), ids.count().to_string()];
            for status in Status::ALL {
                cells.push(
                    latest
                        .iter()
                        .filter(|e| e.status == status)
                        .count()
                        .to_string(),
                );
            }
            cells.push((items.of(kind).len() - latest.len()).to_string());
            for method in Method::ALL {
                let n = latest.iter().filter(|e| e.method == Some(method)).count();
                cells.push(n.to_string());
            }
            line(out, format!("| {} |", cells.join(" | ")));
        }
    }

    fn needs_a_human(&self, out: &mut String) {
        line(out, "## Needs a human");
        line(out, "");
        if self.review.is_empty() {
            line(
                out,
                "Nothing: every item is matched, rejected or not yet run.",
            );
            return;
        }
        line(
            out,
            format!(
                "Items the matcher left undecided, could not match, or matched with low \
                 confidence. For each, either record the right Spotify item with \
                 `sss match set --profile {p} <id> <spotify link>` or record that Spotify \
                 has none with `sss match reject --profile {p} <id>`.",
                p = self.profile
            ),
        );
        for item in self.review {
            line(out, "");
            let mut head = format!(
                "- {} `{}`: {}",
                title_by(item.title.as_deref(), item.artist.as_deref()),
                item.item_id,
                item.status.as_str()
            );
            if let Some(c) = item.confidence {
                head.push_str(&format!(", {} confidence", c.as_str()));
            }
            line(out, head);
            if let Some(album) = &item.album {
                line(out, format!("  - Album: {}", md(album)));
            }
            if let Some(track) = self
                .exports
                .pandora
                .as_ref()
                .and_then(|e| e.tracks.get(&item.item_id))
                && let Some(d) = track.duration_s
            {
                line(out, format!("  - Length: {}:{:02}", d / 60, d % 60));
            }
            let video_id = youtube_select::video_id_of(&item.item_id);
            if let Some(d) = video_id
                .and_then(|id| self.exports.youtube.as_ref()?.videos.get(id))
                .and_then(|v| v.duration_s)
            {
                line(out, format!("  - Length: {}:{:02}", d / 60, d % 60));
            }
            for c in &item.candidates {
                let mut text = format!("[{}]({})", md(&c.name), open_url(&c.uri));
                if !c.artists.is_empty() {
                    text.push_str(&format!(" by {}", md(&c.artists.join(", "))));
                }
                if let Some(a) = &c.album {
                    text.push_str(&format!(" on {}", md(a)));
                }
                line(out, format!("  - Candidate: {text} (score {})", c.score));
            }
            let mut links = search_link("Search Spotify", &item.search_url);
            if let Some(id) = video_id {
                links.push_str(&format!(
                    " · [Watch on YouTube]({})",
                    youtube_report::watch_url(id)
                ));
            }
            line(out, format!("  - {links}"));
            line(
                out,
                format!(
                    "  - `sss match set --profile {p} {id} <spotify link>` or \
                     `sss match reject --profile {p} {id}`",
                    p = self.profile,
                    id = item.item_id
                ),
            );
        }
    }

    fn thumbs_down(&self, out: &mut String) {
        let Some(export) = &self.exports.pandora else {
            return;
        };
        line(out, "## Thumbs down");
        line(out, "");
        let stations: Vec<_> = export
            .stations
            .iter()
            .filter(|s| s.feedback.iter().any(|f| !f.positive))
            .collect();
        if stations.is_empty() {
            line(out, "None.");
            return;
        }
        line(
            out,
            "Spotify takes no thumbs-down from another service. To keep one of these out of \
             your listening, find it with its search link, open its menu (⋯) and choose \
             \"Hide song\" where Spotify offers it, in a playlist or on a radio station.",
        );
        if !self.config.push.thumbs_down_playlist {
            line(out, "");
            line(
                out,
                "They are not matched or pushed: `[push] thumbs_down_playlist = true` in \
                 config.toml makes `sss match` match them and `sss push` keep a private \
                 reference playlist of them.",
            );
        }
        for station in stations {
            line(out, "");
            let name = station.name.as_deref().unwrap_or(&station.station_id);
            line(out, format!("### {}", md(name)));
            line(out, "");
            for f in station.feedback.iter().filter(|f| !f.positive) {
                let id = f.track_pandora_id.as_deref();
                let track = id.and_then(|id| export.tracks.get(id));
                let title = track.and_then(|t| t.title.as_deref());
                let artist = track.and_then(|t| t.artist.as_deref());
                let mut text = format!(
                    "- {}: {}",
                    title_by(title, artist),
                    search_link("search", &matching::search_url(title, artist))
                );
                if let Some(uri) = id
                    .and_then(|id| self.matches.tracks.get(id))
                    .and_then(MatchEntry::matched_uri)
                {
                    text.push_str(&format!(", [on Spotify]({})", open_url(uri)));
                }
                line(out, text);
            }
        }
    }

    fn without_counterpart(&self, out: &mut String) {
        let Some(export) = &self.exports.pandora else {
            return;
        };
        line(out, "## Seeds without a Spotify counterpart");
        line(out, "");
        let mut lines = Vec::new();
        for station in &export.stations {
            let station_name = md(station.name.as_deref().unwrap_or(&station.station_id));
            for seed in &station.seeds {
                let seed_name = md(seed.name.as_deref().unwrap_or("unnamed"));
                match seed.kind {
                    SeedKind::Genre => lines.push(format!(
                        "- Genre {seed_name} (station {station_name}): Spotify has no genre \
                         to follow"
                    )),
                    SeedKind::Artist => {
                        let entry = seed
                            .pandora_id
                            .as_deref()
                            .and_then(|id| self.matches.artists.get(id));
                        if let Some(e) = entry
                            && matches!(e.status, Status::Unmatched | Status::Rejected)
                        {
                            lines.push(format!(
                                "- Artist {seed_name} `{}` (station {station_name}): {}",
                                e.item_id,
                                e.status.as_str()
                            ));
                        }
                    }
                    SeedKind::Track | SeedKind::Other => {}
                }
            }
        }
        if lines.is_empty() {
            line(out, "None.");
        }
        for l in lines {
            line(out, l);
        }
    }

    fn not_pushed(&self, out: &mut String) {
        let Some(export) = &self.exports.pandora else {
            return;
        };
        line(out, "## Pandora playlists not pushed");
        line(out, "");
        let mut kinds: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for p in &export.playlists {
            if let Some(kind) = p.linked_type.as_deref()
                && !push::playlist_is_pushed(p, &self.config.push)
            {
                let (playlists, tracks) = kinds.entry(kind).or_default();
                *playlists += 1;
                *tracks += p.items.len();
            }
        }
        if kinds.is_empty() {
            line(out, "None.");
            return;
        }
        for (kind, (playlists, tracks)) in kinds {
            let why = if NEVER_PUSHED_KINDS.contains(&kind) {
                "never pushed: the station playlists and thumbs already carry them"
            } else {
                "playlists Pandora made; `[push] linked_playlists = true` in config.toml \
                 pushes them"
            };
            line(
                out,
                format!(
                    "- {}: {playlists} playlists, {tracks} tracks — {why}",
                    md(kind)
                ),
            );
        }
    }

    fn youtube_not_pushed(&self, out: &mut String) {
        if let Some(e) = &self.exports.youtube {
            youtube_report::not_pushed(e, self.matches, self.config, out);
        }
    }

    /// The line under the "most played" playlist: the current top N beside
    /// what the playlist holds.
    fn most_played_line(&self, want: Option<&Wanted>, added: &HashSet<&str>, out: &mut String) {
        let Some(e) = &self.exports.youtube else {
            return;
        };
        let top = youtube_select::most_played(e, &self.config.youtube).len();
        let matched = want.map_or(0, |w| w.uris.len());
        let held = want.map_or(0, |w| {
            w.uris.iter().filter(|u| added.contains(u.as_str())).count()
        });
        line(
            out,
            format!(
                "  - Now the {} most played: {top} music videos, {matched} matched, {held} of \
                 them added or found by sss; the playlist only grows, so it also keeps earlier entrants",
                self.config.youtube.most_played
            ),
        );
    }

    fn push_summary(&self, out: &mut String) {
        line(out, "## Push");
        line(out, "");
        let ledger = self.ledger;
        if ledger.playlists.is_empty() && ledger.library.is_empty() {
            line(
                out,
                "Nothing pushed yet: run `sss push --dry-run`, then `sss push`.",
            );
            return;
        }
        let wanted: BTreeMap<String, Wanted> = Part::ALL
            .into_iter()
            .flat_map(|part| push::wanted_playlists(part, self.exports, self.matches, self.config))
            .map(|w| (w.key.clone(), w))
            .collect();
        let mut pending = Vec::new();
        line(out, "### Playlists");
        line(out, "");
        for (key, entry) in &ledger.playlists {
            match entry {
                PlaylistEntry::Created {
                    spotify_id,
                    name,
                    items,
                    ..
                } => {
                    let want = wanted.get(key);
                    let name = name
                        .as_deref()
                        .or(want.map(|w| w.name.as_str()))
                        .unwrap_or(key);
                    let added: HashSet<&str> = items.iter().map(String::as_str).collect();
                    let rest = want.map_or(0, |w| {
                        w.uris
                            .iter()
                            .filter(|u| !added.contains(u.as_str()))
                            .count()
                    });
                    line(
                        out,
                        format!(
                            "- [{}]({}) `{key}`: {} added or found by sss; {rest} matched and not \
                             yet recorded (added by the next `sss push` unless already in the \
                             playlist)",
                            md(name),
                            ids::playlist_url(spotify_id),
                            items.len()
                        ),
                    );
                    if key == youtube_select::MOST_PLAYED_KEY {
                        self.most_played_line(want, &added, out);
                    }
                }
                PlaylistEntry::Pending { name, at } => {
                    line(out, format!("- {} `{key}`: pending since {at}", md(name)));
                    pending.push(push::pending_instruction(
                        key,
                        &md(name),
                        at,
                        &md(&self.paths.spotify_state().display().to_string()),
                    ));
                }
            }
        }
        let uncreated: Vec<&Wanted> = wanted
            .values()
            .filter(|w| !ledger.playlists.contains_key(&w.key))
            .collect();
        for w in &uncreated {
            line(
                out,
                format!(
                    "- {} `{}`: not created yet; the next `sss push` makes it with {} tracks",
                    md(&w.name),
                    w.key,
                    w.uris.len()
                ),
            );
        }
        if ledger.playlists.is_empty() && uncreated.is_empty() {
            line(out, "None.");
        }
        line(out, "");
        line(out, "### Library");
        line(out, "");
        // A key that is not a track, artist or album URI (a hand edit) is
        // counted as "other".
        let mut by_kind: BTreeMap<Option<IdKind>, usize> = BTreeMap::new();
        for uri in ledger.library.keys() {
            *by_kind
                .entry(SpotifyId::of_uri(uri).map(|id| id.kind))
                .or_default() += 1;
        }
        if by_kind.is_empty() {
            line(out, "Nothing saved yet.");
        } else {
            let counts: Vec<String> = IdKind::ALL
                .map(Some)
                .into_iter()
                .chain([None])
                .filter_map(|k| {
                    let n = by_kind.get(&k)?;
                    Some(format!("{n} {}", k.map_or("other", IdKind::plural)))
                })
                .collect();
            line(
                out,
                format!(
                    "Saved by sss or found already saved: {} (artists are follows).",
                    counts.join(", ")
                ),
            );
        }
        if !pending.is_empty() {
            line(out, "");
            line(out, "### Pending playlists");
            for instruction in pending {
                line(out, "");
                line(out, instruction);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matching::model::{Candidate, Confidence};
    use crate::pandora::model::{Feedback, Playlist, Seed, Station, Track};
    use crate::push::LibraryEntry;
    use crate::repo::Repo;
    use serde_json::Value;

    const AT: &str = "2026-10-15T00:00:00Z";

    /// A Spotify id: `c` 22 times.
    fn sid(c: char) -> String {
        c.to_string().repeat(22)
    }

    /// `spotify:<kind>:<id>` with the id `c` 22 times.
    fn uri(kind: &str, c: char) -> String {
        format!("spotify:{kind}:{}", sid(c))
    }

    fn track(id: &str, title: &str, artist: &str) -> Track {
        Track {
            pandora_id: id.into(),
            title: Some(title.into()),
            artist: Some(artist.into()),
            duration_s: Some(185),
            ..Track::default()
        }
    }

    fn thumb(track: &str, positive: bool) -> Feedback {
        Feedback {
            feedback_id: format!("F-{track}"),
            track_pandora_id: Some(track.into()),
            positive,
            created: None,
            station_id: "41".into(),
        }
    }

    fn seed(kind: SeedKind, id: &str, name: &str) -> Seed {
        Seed {
            kind,
            pandora_id: Some(id.into()),
            name: Some(name.into()),
        }
    }

    fn playlist(id: &str, name: &str, kind: Option<&str>, items: &[&str]) -> Playlist {
        Playlist {
            pandora_id: id.into(),
            name: Some(name.into()),
            description: None,
            created: None,
            linked_type: kind.map(str::to_owned),
            items: items.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// Station `ST:9` "Morning" seeded with artist `AR:1`, artist `AR:2`
    /// and genre "Jazz", thumbing up `TR:a` and `TR:b` and down `TR:d`
    /// (only thumbs-down) and `TR:b` (up as well); the member's playlist
    /// `PL:1` of `TR:a`, a `Personalized` playlist of two tracks and a
    /// `StationThumbs` one.
    fn export() -> Normalized {
        let station = Station {
            station_id: "41".into(),
            pandora_id: Some("ST:9".into()),
            name: Some("Morning".into()),
            created: None,
            last_played: None,
            is_thumbprint: false,
            is_shuffle: false,
            genres: Vec::new(),
            positive_feedback_count: None,
            negative_feedback_count: None,
            seeds: vec![
                seed(SeedKind::Artist, "AR:1", "The Band"),
                seed(SeedKind::Artist, "AR:2", "Nobody Known"),
                seed(SeedKind::Genre, "G:1", "Jazz"),
            ],
            feedback: vec![
                thumb("TR:a", true),
                thumb("TR:b", true),
                thumb("TR:d", false),
                thumb("TR:b", false),
            ],
        };
        let tracks = [
            track("TR:a", "Up Song", "The Band"),
            track("TR:b", "Both Ways", "The Band"),
            track("TR:d", "Down * Song", "The Band"),
            track("TR:c", "Made For You", "The Band"),
        ];
        Normalized {
            stations: vec![station],
            playlists: vec![
                playlist("PL:1", "Mine", None, &["TR:a"]),
                playlist("PL:2", "For You", Some("Personalized"), &["TR:c", "TR:a"]),
                playlist("PL:3", "Thumbs", Some("StationThumbs"), &["TR:a"]),
            ],
            tracks: tracks
                .into_iter()
                .map(|t| (t.pandora_id.clone(), t))
                .collect(),
            ..Normalized::default()
        }
    }

    fn entry(id: &str, status: Status, uri: Option<&str>) -> MatchEntry {
        MatchEntry {
            item_id: id.into(),
            status,
            spotify_uri: uri.map(str::to_owned),
            method: Some(Method::Exact),
            confidence: Some(Confidence::High),
            candidates: Vec::new(),
            queries: Vec::new(),
            decided_at: AT.into(),
            session: None,
            reason: None,
        }
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        paths: ProfilePaths,
    }

    /// Profile alice with [`export`] written and `TR:a` matched, `TR:b`
    /// ambiguous with one candidate, `TR:d` (thumbs-down only) unmatched,
    /// `AR:1` matched and `AR:2` unmatched by an `external` decision.
    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        // Underscores after a `/` would open emphasis if left unescaped.
        let paths = Repo::at(tmp.path().join("_sss_data_"))
            .profile("alice")
            .unwrap();
        model::write(&paths, &export()).unwrap();
        let tracks = paths.track_matches();
        matches::append(
            &tracks,
            &entry("TR:a", Status::Matched, Some(&uri("track", 'a'))),
        )
        .unwrap();
        let mut ambiguous = entry("TR:b", Status::Ambiguous, None);
        ambiguous.method = None;
        ambiguous.confidence = None;
        ambiguous.candidates = vec![Candidate {
            uri: uri("track", 'b'),
            name: "Both Ways (Live)".into(),
            artists: vec!["The Band".into()],
            album: Some("Live at Home".into()),
            score: 0.81,
            ..Candidate::default()
        }];
        matches::append(&tracks, &ambiguous).unwrap();
        matches::append(&tracks, &entry("TR:d", Status::Unmatched, None)).unwrap();
        let artists = paths.artist_matches();
        matches::append(
            &artists,
            &entry("AR:1", Status::Matched, Some(&uri("artist", '1'))),
        )
        .unwrap();
        let mut external = entry("AR:2", Status::Unmatched, None);
        external.method = Some(Method::External);
        external.confidence = Some(Confidence::Medium);
        matches::append(&artists, &external).unwrap();
        Fixture { _tmp: tmp, paths }
    }

    fn written(f: &Fixture, push: &PushConfig) -> String {
        let config = Config {
            push: push.clone(),
            ..Config::default()
        };
        let report = report("alice", &f.paths, &config).unwrap();
        assert_eq!(report.path, f.paths.report());
        std::fs::read_to_string(&report.path).unwrap()
    }

    /// The text of the `## <heading>` section of `doc`, up to the next one.
    fn section<'a>(doc: &'a str, heading: &str) -> &'a str {
        let start = doc
            .find(&format!("\n## {heading}\n"))
            .unwrap_or_else(|| panic!("no section {heading}:\n{doc}"));
        let rest = &doc[start + 1..];
        let end = rest[1..].find("\n## ").map_or(rest.len(), |i| i + 2);
        &rest[..end]
    }

    #[test]
    fn every_section_is_written_with_its_counts() {
        let f = fixture();
        let doc = written(&f, &PushConfig::default());
        assert!(doc.starts_with("# alice: to Spotify\n"), "{doc}");
        let export = section(&doc, "Export");
        assert!(
            export.contains("- Latest complete export run: `unknown`"),
            "{export}"
        );
        assert!(export.contains("- Stations: 1 (0 Thumbprint, 0 shuffle)"));
        assert!(export.contains("- Thumbs up: 2\n- Thumbs down: 2\n"));
        assert!(
            export.contains("- Playlists: 3 (Personalized: 1, StationThumbs: 1, member's own: 1)"),
            "{export}"
        );

        let summary = section(&doc, "Matches");
        // Tracks push sends: TR:a, TR:b (thumbs up), not TR:d or TR:c.
        assert!(
            summary.contains("| by llm | by manual | by external |"),
            "{summary}"
        );
        assert!(
            summary.contains("| tracks | 2 | 1 | 0 | 1 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |"),
            "{summary}"
        );
        assert!(
            summary.contains("| artists | 2 | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 1 |"),
            "{summary}"
        );
        assert!(summary.contains("| albums | 0 | 0 |"), "{summary}");

        let human = section(&doc, "Needs a human");
        assert!(
            human.contains("- \"Both Ways\" by The Band `TR:b`: ambiguous"),
            "{human}"
        );
        assert!(human.contains("  - Length: 3:05"), "{human}");
        assert!(
            human.contains(&format!(
                "  - Candidate: [Both Ways (Live)](https://open.spotify.com/track/{}) by The Band \
                 on Live at Home (score 0.81)",
                sid('b')
            )),
            "{human}"
        );
        assert!(
            human.contains(
                "  - [Search Spotify](<https://open.spotify.com/search/Both%20Ways%20The%20Band>)"
            ),
            "{human}"
        );
        assert!(
            human.contains(
                "`sss match set --profile alice TR:b <spotify link>` or \
                 `sss match reject --profile alice TR:b`"
            ),
            "{human}"
        );
        assert!(
            human.contains("\"Nobody Known\" `AR:2`: unmatched"),
            "{human}"
        );

        let seeds = section(&doc, "Seeds without a Spotify counterpart");
        assert!(seeds.contains("- Genre Jazz (station Morning)"), "{seeds}");
        assert!(
            seeds.contains("- Artist Nobody Known `AR:2` (station Morning): unmatched"),
            "{seeds}"
        );
        assert!(!seeds.contains("The Band"), "{seeds}");

        let not_pushed = section(&doc, "Pandora playlists not pushed");
        assert!(
            not_pushed.contains("- Personalized: 1 playlists, 2 tracks — playlists Pandora made; `[push] linked_playlists = true`"),
            "{not_pushed}"
        );
        assert!(
            not_pushed.contains("- StationThumbs: 1 playlists, 1 tracks — never pushed"),
            "{not_pushed}"
        );

        let pushed = section(&doc, "Push");
        assert!(pushed.contains("Nothing pushed yet"), "{pushed}");
    }

    #[test]
    fn a_thumbs_down_only_track_is_listed_under_thumbs_down_and_never_for_a_person() {
        let f = fixture();
        for thumbs_down_playlist in [false, true] {
            let push = PushConfig {
                thumbs_down_playlist,
                ..PushConfig::default()
            };
            let doc = written(&f, &push);
            let human = section(&doc, "Needs a human");
            assert!(!human.contains("TR:d"), "{human}");
            let down = section(&doc, "Thumbs down");
            assert!(down.contains("### Morning"), "{down}");
            assert!(
                down.contains(
                    "- \"Down \\* Song\" by The Band: \
                     [search](<https://open.spotify.com/search/Down%20*%20Song%20The%20Band>)\n"
                ),
                "{down}"
            );
            // Thumbed up as well: still listed, with its Spotify link once
            // it is matched.
            assert!(
                down.contains("- \"Both Ways\" by The Band: [search]"),
                "{down}"
            );
            assert!(down.contains("\"Hide song\""), "{down}");
            assert_eq!(
                down.contains("thumbs_down_playlist = true` in"),
                !thumbs_down_playlist,
                "{down}"
            );
        }

        matches::append(
            &f.paths.track_matches(),
            &entry("TR:d", Status::Matched, Some(&uri("track", 'd'))),
        )
        .unwrap();
        let doc = written(&f, &PushConfig::default());
        assert!(
            section(&doc, "Thumbs down").contains(&format!(
                ", [on Spotify](https://open.spotify.com/track/{})\n",
                sid('d')
            )),
            "{doc}"
        );
    }

    #[test]
    fn the_push_summary_links_playlists_counts_saves_and_quotes_pending_entries() {
        let f = fixture();
        let mut ledger = Ledger::default();
        ledger.playlists.insert(
            "ST:9".into(),
            PlaylistEntry::Created {
                spotify_id: sid('9'),
                snapshot_id: None,
                name: None,
                items: Vec::new(),
                at: None,
            },
        );
        ledger.playlists.insert(
            "PL:1".into(),
            PlaylistEntry::Pending {
                name: "Mine [*live*]".into(),
                at: AT.into(),
            },
        );
        for key in [
            uri("track", 'a'),
            uri("artist", '1'),
            uri("artist", '2'),
            "edited by hand".into(),
        ] {
            ledger.library.insert(key, LibraryEntry { at: AT.into() });
        }
        ledger.save(&f.paths.spotify_state()).unwrap();
        let doc = written(&f, &PushConfig::default());
        let pushed = section(&doc, "Push");
        assert!(
            pushed.contains(&format!(
                "- [Morning](https://open.spotify.com/playlist/{}) `ST:9`: 0 added or found by sss; \
                 1 matched and not yet recorded (added by the next `sss push` unless already \
                 in the playlist)",
                sid('9')
            )),
            "{pushed}"
        );
        assert!(
            pushed.contains("- Mine \\[\\*live\\*\\] `PL:1`: pending since 2026-10-15T00:00:00Z"),
            "{pushed}"
        );
        assert!(
            pushed.contains("Saved by sss or found already saved: 1 tracks, 2 artists, 1 other"),
            "{pushed}"
        );
        assert!(
            pushed.contains(
                "Playlist `PL:1` (\"Mine \\[\\*live\\*\\]\") may have been created at \
                 2026-10-15T00:00:00Z without being recorded."
            ),
            "{pushed}"
        );
        assert!(
            pushed.contains(
                "/\\_sss\\_data\\_/profiles/alice/spotify/state.json with \
                 `\"PL:1\": {\"status\":\"created\",\"spotify_id\":\"<id>\"}`"
            ),
            "{pushed}"
        );
    }

    #[test]
    fn a_search_link_keeps_unbalanced_parentheses_inside_it() {
        let f = fixture();
        let mut e = export();
        e.tracks.get_mut("TR:d").unwrap().title = Some("Smile :)".into());
        e.tracks.get_mut("TR:b").unwrap().title = Some("Song (Part 1".into());
        model::write(&f.paths, &e).unwrap();
        let doc = written(&f, &PushConfig::default());
        let down = section(&doc, "Thumbs down");
        assert!(
            down.contains(
                "- \"Smile :)\" by The Band: \
                 [search](<https://open.spotify.com/search/Smile%20:)%20The%20Band>)\n"
            ),
            "{down}"
        );
        let human = section(&doc, "Needs a human");
        assert!(
            human.contains(
                "  - [Search Spotify](<https://open.spotify.com/search/Song%20(Part%201%20The%20Band>)\n"
            ),
            "{human}"
        );
    }

    #[test]
    fn the_thumbs_down_playlist_is_listed_only_under_its_switch() {
        let f = fixture();
        matches::append(
            &f.paths.track_matches(),
            &entry("TR:d", Status::Matched, Some(&uri("track", 'd'))),
        )
        .unwrap();
        let mut ledger = Ledger::default();
        ledger
            .library
            .insert(uri("artist", '1'), LibraryEntry { at: AT.into() });
        ledger.save(&f.paths.spotify_state()).unwrap();
        for thumbs_down_playlist in [false, true] {
            let push = PushConfig {
                thumbs_down_playlist,
                ..PushConfig::default()
            };
            let doc = written(&f, &push);
            let pushed = section(&doc, "Push");
            assert_eq!(
                pushed.contains(
                    "- Pandora thumbs-down (reference) `thumbs-down`: not created yet; the next \
                     `sss push` makes it with 1 tracks\n"
                ),
                thumbs_down_playlist,
                "{pushed}"
            );
        }
    }

    #[test]
    fn a_playlist_not_created_yet_is_named_with_its_track_count() {
        let f = fixture();
        let mut ledger = Ledger::default();
        ledger
            .library
            .insert(uri("artist", '1'), LibraryEntry { at: AT.into() });
        ledger.save(&f.paths.spotify_state()).unwrap();
        let doc = written(&f, &PushConfig::default());
        let pushed = section(&doc, "Push");
        assert!(
            pushed.contains(
                "- Morning `ST:9`: not created yet; the next `sss push` makes it with 1 tracks"
            ),
            "{pushed}"
        );
        assert!(
            pushed.contains("- Mine `PL:1`: not created yet"),
            "{pushed}"
        );
        assert!(!pushed.contains("### Pending"), "{pushed}");
    }

    #[test]
    fn the_run_id_is_the_latest_complete_run() {
        let f = fixture();
        for (run_id, complete) in [("20261014T000000Z", true), ("20261015T000000Z", false)] {
            let run = f.paths.raw_run_dir(run_id);
            let manifest = export::Manifest {
                run_id: run_id.into(),
                started: AT.into(),
                finished: AT.into(),
                requests: 1,
                steps: export::Steps::default(),
                complete,
            };
            repo::write_json(&run.join(repo::MANIFEST_FILE), &manifest).unwrap();
        }
        let doc = written(&f, &PushConfig::default());
        assert!(
            doc.contains("- Latest complete export run: `20261014T000000Z`"),
            "{doc}"
        );
    }

    #[test]
    fn no_export_is_a_config_error_naming_pandora_export() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Repo::at(tmp.path()).profile("alice").unwrap();
        let err = report("alice", &paths, &Config::default()).unwrap_err();
        assert_eq!(crate::error::kind(&err), crate::error::Kind::Config);
        assert!(format!("{err:#}").contains("sss pandora export"), "{err:#}");
        assert!(!paths.report().exists());
    }

    /// A YouTube video of kind `song` from a Topic channel, played `plays`
    /// times.
    fn youtube_video(id: &str, title: &str, plays: u32, duration_s: Option<u32>) -> Value {
        serde_json::json!({
            "video_id": id, "title": title, "artists": ["The Band"], "album": null,
            "duration_s": duration_s,
            "channel": {"name": "The Band - Topic", "channel_id": null},
            "kind": "song", "evidence": ["topic-channel"], "title_source": "history",
            "youtube_title": title, "plays": plays, "music_app_plays": plays,
            "removed": false, "first_played": "2026-01-01T00:00:00.000Z",
            "last_played": "2026-01-02T00:00:00.000Z", "in_library": false,
            "liked": false, "available": null, "category_id": null, "topics": []
        })
    }

    #[test]
    fn a_mixed_profile_reports_both_sources_youtube_links_and_the_most_played_top_n() {
        let f = fixture();
        let videos = serde_json::json!({
            "calmCALM123": youtube_video("calmCALM123", "Calm Waters", 2, None),
            "odd0ODD0123": youtube_video("odd0ODD0123", "Odd One", 1, Some(222)),
        });
        let youtube = youtube_model::Normalized {
            videos: serde_json::from_value(videos).unwrap(),
            ..youtube_model::Normalized::default()
        };
        youtube_model::write(&f.paths, &youtube).unwrap();
        repo::write_json(&f.paths.youtube_runs(), &["20260101T000000Z"]).unwrap();
        let tracks = f.paths.track_matches();
        matches::append(
            &tracks,
            &entry("YV:calmCALM123", Status::Matched, Some(&uri("track", 'y'))),
        )
        .unwrap();
        let mut ambiguous = entry("YV:odd0ODD0123", Status::Ambiguous, None);
        ambiguous.method = None;
        ambiguous.confidence = None;
        matches::append(&tracks, &ambiguous).unwrap();
        let mut ledger = Ledger::default();
        ledger.playlists.insert(
            youtube_select::MOST_PLAYED_KEY.into(),
            PlaylistEntry::Created {
                spotify_id: sid('m'),
                snapshot_id: None,
                name: Some("YouTube Music: most played".into()),
                items: vec![uri("track", 'y')],
                at: None,
            },
        );
        ledger.save(&f.paths.spotify_state()).unwrap();

        let doc = written(&f, &PushConfig::default());
        let export = section(&doc, "Export");
        assert!(export.contains("### Pandora\n"), "{export}");
        assert!(
            export.contains(
                "### YouTube\n\n- Latest complete import run: `20260101T000000Z` (playlists, \
                 library, subscriptions)\n"
            ),
            "{export}"
        );
        let human = section(&doc, "Needs a human");
        assert!(
            human.contains("- \"Odd One\" by The Band `YV:odd0ODD0123`: ambiguous\n"),
            "{human}"
        );
        assert!(human.contains("  - Length: 3:42\n"), "{human}");
        assert!(
            human.contains(
                "  - [Search Spotify](<https://open.spotify.com/search/Odd%20One%20The%20Band>) \
                 · [Watch on YouTube](https://www.youtube.com/watch?v=odd0ODD0123)\n"
            ),
            "{human}"
        );
        assert!(
            section(&doc, "YouTube playlists not pushed").contains("None."),
            "{doc}"
        );
        let pushed = section(&doc, "Push");
        assert!(
            pushed.contains(
                "`youtube:most-played`: 1 added or found by sss; 0 matched and not yet \
                 recorded (added by the next `sss push` unless already in the playlist)\n  - \
                 Now the 100 most played: 2 music videos, 1 matched, 1 of them added or found \
                 by sss;"
            ),
            "{pushed}"
        );
    }

    #[test]
    fn markdown_in_names_is_escaped_and_uris_become_links() {
        assert_eq!(md("a*b_[c]`d`\nnext"), "a\\*b\\_\\[c\\]\\`d\\` next");
        assert_eq!(
            open_url(&uri("album", 'x')),
            format!("https://open.spotify.com/album/{}", sid('x'))
        );
        assert_eq!(open_url("local:thing"), "local:thing");
        assert_eq!(open_url("spotify:track:short"), "spotify:track:short");
        assert_eq!(title_by(None, Some("X")), "an untitled item by X");
    }
}
