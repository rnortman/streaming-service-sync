//! The items a write command takes — tracks, albums, artists, playlists —
//! given as Spotify URIs, open.spotify.com addresses, bare track ids or
//! source item ids (`TR:…`, `YV:…`, …) resolved through the latest match,
//! from the command line and from a `--from-file` list; and what every
//! write of such items shares: the prelude that resolves them, the
//! progress line per chunk and the echo of each source item id.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::{self, BufRead, Write};
use std::path::Path;

use anyhow::{Context as _, Result};

use crate::error::{Kind, PartialWrite};
use crate::fail;
use crate::input::pasted_text;
use crate::matching::model::{self, MatchEntry, Status};
use crate::repo::ProfilePaths;
use crate::spotify::ids::{self, IdKind};

/// The kinds of Spotify entity a write command takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Track,
    Album,
    Artist,
    Playlist,
}

impl ItemKind {
    /// The type segment of a URI: `track`, `album`, `artist`, `playlist`.
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Track => "track",
            ItemKind::Album => "album",
            ItemKind::Artist => "artist",
            ItemKind::Playlist => "playlist",
        }
    }

    fn plural(self) -> &'static str {
        match self {
            ItemKind::Track => "tracks",
            ItemKind::Album => "albums",
            ItemKind::Artist => "artists",
            ItemKind::Playlist => "playlists",
        }
    }

    fn of_segment(segment: &str) -> Option<Self> {
        match segment {
            "track" => Some(ItemKind::Track),
            "album" => Some(ItemKind::Album),
            "artist" => Some(ItemKind::Artist),
            "playlist" => Some(ItemKind::Playlist),
            _ => None,
        }
    }
}

impl From<IdKind> for ItemKind {
    fn from(kind: IdKind) -> Self {
        match kind {
            IdKind::Track => ItemKind::Track,
            IdKind::Album => ItemKind::Album,
            IdKind::Artist => ItemKind::Artist,
        }
    }
}

impl fmt::Display for ItemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a playlist's contents commands take.
pub const PLAYLIST_ITEMS: &[ItemKind] = &[ItemKind::Track];

/// What `sss library add|remove` take.
pub const LIBRARY_ITEMS: &[ItemKind] = &[
    ItemKind::Track,
    ItemKind::Album,
    ItemKind::Artist,
    ItemKind::Playlist,
];

/// The guidance every refusal of an item ends with.
const ITEM_FORMS: &str = "give a Spotify URI (spotify:track:<id>), an https://open.spotify.com \
                          address, a bare 22-character track id or a source item id (TR:…, \
                          YV:…, AR:…, AL:…)";

/// A write's items as asked: those given on the command line and in
/// `--from-file`, and whether it is a dry run.
pub struct Items<'a> {
    pub args: &'a [String],
    pub from_file: Option<&'a Path>,
    /// Make the reads and none of the writes.
    pub dry_run: bool,
}

/// One item as given and the Spotify URI it resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The text as given, trimmed.
    pub given: String,
    /// `spotify:<kind>:<id>`.
    pub uri: String,
    pub kind: ItemKind,
}

impl Resolved {
    /// Whether the item was given as a source item id.
    pub fn is_item_id(&self) -> bool {
        IdKind::of_item_id(&self.given).is_some()
    }
}

/// `tracks`, `tracks and albums`, `tracks, albums, artists and playlists`.
fn listing(takes: &[ItemKind]) -> String {
    let names: Vec<&str> = takes.iter().map(|k| k.plural()).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `a` or `an`, for a kind's name.
fn article(word: &str) -> &'static str {
    match word.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u') => "an",
        _ => "a",
    }
}

/// The lines of a `--from-file` list, each with its 1-based line number:
/// `-` reads `input` to EOF, anything else is a file read whole. Lines are
/// trimmed; empty lines and lines starting with `#` are skipped. Text that
/// is not UTF-8 is a [`Kind::Input`] error; a missing file is
/// [`Kind::Other`].
pub fn read_list(from_file: &Path, input: &mut dyn BufRead) -> Result<Vec<(usize, String)>> {
    let text = if from_file == Path::new("-") {
        let mut text = String::new();
        pasted_text(input.read_to_string(&mut text), || {
            "reading the item list from stdin".into()
        })?;
        text
    } else {
        pasted_text(std::fs::read_to_string(from_file), || {
            format!("reading {}", from_file.display())
        })?
    };
    Ok(text
        .lines()
        .map(str::trim)
        .enumerate()
        .filter(|(_, l)| !l.is_empty() && !l.starts_with('#'))
        .map(|(n, l)| (n + 1, l.to_owned()))
        .collect())
}

/// The latest match entries of each kind, read the first time an item id
/// of that kind is resolved.
struct Matches<'a> {
    paths: &'a ProfilePaths,
    by_kind: BTreeMap<IdKind, BTreeMap<String, MatchEntry>>,
}

impl Matches<'_> {
    fn of(&mut self, kind: IdKind) -> Result<&BTreeMap<String, MatchEntry>> {
        if !self.by_kind.contains_key(&kind) {
            let entries = model::read_latest(&model::file(self.paths, kind))?;
            self.by_kind.insert(kind, entries);
        }
        Ok(&self.by_kind[&kind])
    }
}

/// The URI the match of source item `text` records, when its kind is one
/// of `takes`.
fn resolve_item_id(
    text: &str,
    kind: IdKind,
    takes: &[ItemKind],
    matches: &mut Matches,
) -> Result<Resolved> {
    let item_kind = ItemKind::from(kind);
    if !takes.contains(&item_kind) {
        fail!(
            Kind::Input,
            "{text} is {} {kind} item; this command takes {}",
            article(kind.as_str()),
            listing(takes)
        );
    }
    let Some(entry) = matches.of(kind)?.get(text) else {
        fail!(
            Kind::Input,
            "{text} has no match entry; run `sss match` first"
        );
    };
    if let Some(uri) = entry.matched_uri() {
        let (segment, id) = ids::split(uri, ITEM_FORMS)
            .with_context(|| format!("the match recorded for {text}"))?;
        if segment.as_deref() != Some(kind.as_str()) {
            fail!(
                Kind::Input,
                "{text} is matched to {uri}, which is not {} {kind}; `sss match set` records \
                 another",
                article(kind.as_str())
            );
        }
        return Ok(Resolved {
            given: text.to_owned(),
            uri: format!("spotify:{kind}:{id}"),
            kind: item_kind,
        });
    }
    match entry.status {
        Status::Ambiguous => fail!(
            Kind::Input,
            "{text} is ambiguous; run `sss match` or `sss match set`"
        ),
        Status::LlmFailed => fail!(Kind::Input, "{text} is llm_failed; run `sss match`"),
        status @ (Status::Unmatched | Status::Rejected | Status::Matched) => fail!(
            Kind::Input,
            "{text} is {}: it has no Spotify counterpart recorded; `sss match set` records one",
            status.as_str()
        ),
    }
}

/// `text` as a URI, address or bare id (a track), when its kind is one of
/// `takes`.
fn resolve_spotify(text: &str, takes: &[ItemKind]) -> Result<Resolved> {
    let takes_text = listing(takes);
    if text.starts_with("spotify:local:") {
        fail!(
            Kind::Input,
            "{text:?} is a local file, which Spotify's API cannot add or save; this command takes \
             {takes_text}"
        );
    }
    let (segment, id) = ids::split(text, ITEM_FORMS)?;
    let segment = segment.unwrap_or_else(|| ItemKind::Track.as_str().to_owned());
    let kind = match ItemKind::of_segment(&segment) {
        Some(kind) if takes.contains(&kind) => kind,
        _ => fail!(
            Kind::Input,
            "{text:?} is {} {segment}; this command takes {takes_text}",
            article(&segment)
        ),
    };
    Ok(Resolved {
        given: text.to_owned(),
        uri: format!("spotify:{kind}:{id}"),
        kind,
    })
}

/// Every item of `args`, then every line of `from_file` ([`read_list`]),
/// resolved to a Spotify URI of a kind in `takes`, in order. A source item
/// id is looked up in its kind's match file before anything else is tried;
/// anything else is a URI, an open.spotify.com address or a bare track id.
/// The first item that does not resolve ends the call with [`Kind::Input`].
pub fn resolve_items(
    paths: &ProfilePaths,
    takes: &[ItemKind],
    args: &[String],
    from_file: Option<&Path>,
    input: &mut dyn BufRead,
) -> Result<Vec<Resolved>> {
    let mut matches = Matches {
        paths,
        by_kind: BTreeMap::new(),
    };
    let mut resolve = |text: &str| -> Result<Resolved> {
        let text = text.trim();
        match IdKind::of_item_id(text) {
            Some(kind) => resolve_item_id(text, kind, takes, &mut matches),
            None => resolve_spotify(text, takes),
        }
    };
    let mut resolved = args
        .iter()
        .map(|a| resolve(a))
        .collect::<Result<Vec<_>>>()?;
    if let Some(path) = from_file {
        for (n, line) in read_list(path, input)? {
            resolved.push(resolve(&line).with_context(|| format!("{} line {n}", path.display()))?);
        }
    }
    Ok(resolved)
}

/// `items` with every URI kept at its first occurrence, and how many were
/// dropped.
pub fn dedupe(items: Vec<Resolved>) -> (Vec<Resolved>, usize) {
    let before = items.len();
    let mut seen = HashSet::new();
    let kept: Vec<Resolved> = items
        .into_iter()
        .filter(|r| seen.insert(r.uri.clone()))
        .collect();
    let dropped = before - kept.len();
    (kept, dropped)
}

/// The URI each source item id among `items` resolved to, for a report's
/// `resolved`: `None` when no source item id was given.
pub fn resolved(items: &[Resolved]) -> Option<BTreeMap<String, String>> {
    let ids: BTreeMap<String, String> = items
        .iter()
        .filter(|r| r.is_item_id())
        .map(|r| (r.given.clone(), r.uri.clone()))
        .collect();
    Some(ids).filter(|m| !m.is_empty())
}

/// The URIs of `items`, in order.
pub fn uris(items: &[Resolved]) -> Vec<String> {
    items.iter().map(|r| r.uri.clone()).collect()
}

/// A write's items resolved, each once.
pub struct Given {
    /// What each source item id resolved to; `None` when none was given.
    pub resolved: Option<BTreeMap<String, String>>,
    pub duplicates_dropped: usize,
    /// The items kept, in order.
    pub items: Vec<Resolved>,
    /// Their URIs.
    pub uris: Vec<String>,
}

/// Resolves `items` to the kinds of `takes` (no request): an empty list is
/// kind `input`; a URI given twice is kept at its first occurrence.
pub fn given(
    paths: &ProfilePaths,
    takes: &[ItemKind],
    items: &Items,
    input: &mut dyn BufRead,
) -> Result<Given> {
    let all = resolve_items(paths, takes, items.args, items.from_file, input)?;
    if all.is_empty() {
        fail!(Kind::Input, "no items given");
    }
    let resolved = resolved(&all);
    let (kept, duplicates_dropped) = dedupe(all);
    Ok(Given {
        resolved,
        duplicates_dropped,
        uris: uris(&kept),
        items: kept,
    })
}

/// The URIs a chunked write has landed so far, with a `<what> n of m`
/// line on `err` per chunk.
pub struct Progress<'a> {
    what: &'static str,
    total: usize,
    done: Vec<String>,
    err: &'a mut dyn Write,
}

impl<'a> Progress<'a> {
    /// `what` is the past-tense verb of the lines and of a
    /// [`PartialWrite`]; `total` the URIs the write is for.
    pub fn new(what: &'static str, total: usize, err: &'a mut dyn Write) -> Self {
        Progress {
            what,
            total,
            done: Vec::new(),
            err,
        }
    }

    /// Records `chunk` as landed and prints the line.
    pub fn landed(&mut self, chunk: &[String]) {
        self.done.extend_from_slice(chunk);
        // Lines on `err` are for people: a failed write of one does not
        // end the command.
        let _ = writeln!(
            self.err,
            "{} {} of {}",
            self.what,
            self.done.len(),
            self.total
        );
    }

    /// `source`, as a [`PartialWrite`] when a chunk had landed.
    pub fn failed(self, source: anyhow::Error) -> anyhow::Error {
        PartialWrite::after(self.what, self.done, self.total, source)
    }
}

/// `  (TR:1 is spotify:track:…)` per source item id.
pub fn write_resolved(
    out: &mut dyn Write,
    resolved: Option<&BTreeMap<String, String>>,
) -> io::Result<()> {
    for (given, uri) in resolved.into_iter().flatten() {
        writeln!(out, "  ({given} is {uri})")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error;
    use crate::repo::Repo;

    const TRACK_ID: &str = "4uLU6hMCjMI75M1A2tKUQC";
    const ALBUM_ID: &str = "6akEvsycLGftJxYudPjmqK";

    fn paths(tmp: &Path) -> ProfilePaths {
        Repo::at(tmp.join("data")).profile("alice").unwrap()
    }

    fn entry(item: &str, status: Status, uri: Option<&str>) -> MatchEntry {
        MatchEntry {
            status,
            spotify_uri: uri.map(str::to_owned),
            ..MatchEntry::manual_reject(item, "2026-10-19T00:00:00Z".into())
        }
    }

    fn write_matches(paths: &ProfilePaths, kind: IdKind, entries: &[MatchEntry]) {
        let file = model::file(paths, kind);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let lines: Vec<String> = entries
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect();
        std::fs::write(file, lines.join("\n") + "\n").unwrap();
    }

    fn resolve(paths: &ProfilePaths, takes: &[ItemKind], args: &[&str]) -> Result<Vec<Resolved>> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        resolve_items(paths, takes, &args, None, &mut &b""[..])
    }

    fn uris_of(resolved: &[Resolved]) -> Vec<&str> {
        resolved.iter().map(|r| r.uri.as_str()).collect()
    }

    fn refusal(result: Result<Vec<Resolved>>) -> String {
        let err = result.unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input, "{err:#}");
        format!("{err:#}")
    }

    #[test]
    fn progress_prints_a_line_per_chunk_and_wraps_a_later_failure() {
        let mut err = Vec::new();
        let progress = Progress::new("saved", 3, &mut err);
        let none = progress.failed(anyhow::anyhow!("refused"));
        assert!(none.downcast_ref::<PartialWrite>().is_none());
        let mut progress = Progress::new("saved", 3, &mut err);
        progress.landed(&["a".into(), "b".into()]);
        let partial = progress.failed(anyhow::anyhow!("refused"));
        let partial = partial
            .downcast_ref::<PartialWrite>()
            .expect("a partial write");
        assert_eq!(partial.done, ["a", "b"]);
        assert_eq!(partial.total, 3);
        assert_eq!(String::from_utf8(err).unwrap(), "saved 2 of 3\n");
    }

    #[test]
    fn uris_links_and_bare_ids_resolve_to_uris() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let got = resolve(
            &p,
            LIBRARY_ITEMS,
            &[
                &format!(" spotify:track:{TRACK_ID} "),
                &format!("https://open.spotify.com/intl-de/album/{ALBUM_ID}?si=x"),
                TRACK_ID,
                &format!("spotify:playlist:{ALBUM_ID}"),
                &format!("spotify:artist:{ALBUM_ID}"),
            ],
        )
        .unwrap();
        assert_eq!(
            uris_of(&got),
            [
                format!("spotify:track:{TRACK_ID}"),
                format!("spotify:album:{ALBUM_ID}"),
                format!("spotify:track:{TRACK_ID}"),
                format!("spotify:playlist:{ALBUM_ID}"),
                format!("spotify:artist:{ALBUM_ID}"),
            ]
        );
        assert_eq!(got[1].kind, ItemKind::Album);
        assert_eq!(got[0].given, format!("spotify:track:{TRACK_ID}"));
        assert_eq!(resolved(&got), None);
    }

    #[test]
    fn matched_item_ids_resolve_before_any_bare_id_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let track = format!("spotify:track:{TRACK_ID}");
        write_matches(
            &p,
            IdKind::Track,
            &[
                entry("TR:123", Status::Ambiguous, None),
                entry("TR:123", Status::Matched, Some(&track)),
                entry("YV:abc", Status::Matched, Some(&track)),
            ],
        );
        let got = resolve(&p, PLAYLIST_ITEMS, &["TR:123", "YV:abc"]).unwrap();
        assert_eq!(uris_of(&got), [track.as_str(), track.as_str()]);
        assert!(got.iter().all(Resolved::is_item_id));
        assert_eq!(
            resolved(&got),
            Some(BTreeMap::from([
                ("TR:123".to_owned(), track.clone()),
                ("YV:abc".to_owned(), track.clone()),
            ]))
        );
    }

    #[test]
    fn an_item_id_without_a_usable_match_is_refused_by_status() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        write_matches(
            &p,
            IdKind::Track,
            &[
                entry("TR:1", Status::Ambiguous, None),
                entry("TR:2", Status::Unmatched, None),
                entry("TR:3", Status::Rejected, None),
                entry("TR:4", Status::LlmFailed, None),
                entry("TR:5", Status::Matched, None),
            ],
        );
        for (item, needle) in [
            (
                "TR:1",
                "TR:1 is ambiguous; run `sss match` or `sss match set`",
            ),
            (
                "TR:2",
                "TR:2 is unmatched: it has no Spotify counterpart recorded; `sss match set` \
                 records one",
            ),
            ("TR:3", "TR:3 is rejected: it has no Spotify counterpart"),
            ("TR:4", "TR:4 is llm_failed; run `sss match`"),
            ("TR:5", "TR:5 is matched: it has no Spotify counterpart"),
            ("TR:9", "TR:9 has no match entry; run `sss match` first"),
        ] {
            let msg = refusal(resolve(&p, PLAYLIST_ITEMS, &[item]));
            assert!(msg.contains(needle), "{item}: {msg}");
        }
    }

    #[test]
    fn a_missing_match_file_is_no_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let msg = refusal(resolve(&paths(tmp.path()), LIBRARY_ITEMS, &["AL:7"]));
        assert_eq!(msg, "AL:7 has no match entry; run `sss match` first");
    }

    #[test]
    fn kinds_the_command_does_not_take_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        for (item, needle) in [
            ("AR:1", "AR:1 is an artist item; this command takes tracks"),
            ("CO:1", "CO:1 is an artist item"),
            (
                &format!("spotify:album:{ALBUM_ID}") as &str,
                "is an album; this command takes tracks",
            ),
            (
                &format!("https://open.spotify.com/playlist/{ALBUM_ID}"),
                "is a playlist; this command takes tracks",
            ),
            (
                &format!("spotify:episode:{ALBUM_ID}"),
                "is an episode; this command takes tracks",
            ),
            (
                "spotify:local:Artist:Album:Song:180",
                "is a local file, which Spotify's API cannot add or save",
            ),
            ("spotify:track:short", "does not carry a Spotify id"),
            ("Baby Shark", "does not carry a Spotify id"),
        ] {
            let msg = refusal(resolve(&p, PLAYLIST_ITEMS, &[item]));
            assert!(msg.contains(needle), "{item}: {msg}");
        }
        let msg = refusal(resolve(
            &p,
            LIBRARY_ITEMS,
            &[&format!("spotify:show:{ALBUM_ID}")],
        ));
        assert!(
            msg.contains("is a show; this command takes tracks, albums, artists and playlists"),
            "{msg}"
        );
    }

    #[test]
    fn a_match_of_the_wrong_kind_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let album = format!("spotify:album:{ALBUM_ID}");
        write_matches(
            &p,
            IdKind::Track,
            &[entry("TR:1", Status::Matched, Some(&album))],
        );
        let msg = refusal(resolve(&p, PLAYLIST_ITEMS, &["TR:1"]));
        assert!(msg.contains("which is not a track"), "{msg}");
    }

    #[test]
    fn the_file_list_skips_blank_and_comment_lines_and_follows_the_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let file = tmp.path().join("uris.txt");
        std::fs::write(
            &file,
            format!("# merge\n\n  spotify:album:{ALBUM_ID}  \n\t\n{TRACK_ID}\r\n"),
        )
        .unwrap();
        let args = vec![format!("spotify:artist:{ALBUM_ID}")];
        let got = resolve_items(&p, LIBRARY_ITEMS, &args, Some(&file), &mut &b""[..]).unwrap();
        assert_eq!(
            uris_of(&got),
            [
                format!("spotify:artist:{ALBUM_ID}"),
                format!("spotify:album:{ALBUM_ID}"),
                format!("spotify:track:{TRACK_ID}"),
            ]
        );

        // A refusal names the item's line in the file, comments and blank
        // lines counted.
        std::fs::write(&file, format!("# merge\n\n{TRACK_ID}\nspotify:track:x\n")).unwrap();
        let err = resolve_items(&p, LIBRARY_ITEMS, &[], Some(&file), &mut &b""[..]).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert!(format!("{err:#}").contains("uris.txt line 4: "), "{err:#}");
    }

    #[test]
    fn a_dash_reads_the_list_from_stdin_to_eof() {
        let tmp = tempfile::tempdir().unwrap();
        let text = format!("{TRACK_ID}\n\nspotify:album:{ALBUM_ID}\n");
        let mut input = text.as_bytes();
        let got = resolve_items(
            &paths(tmp.path()),
            LIBRARY_ITEMS,
            &[],
            Some(Path::new("-")),
            &mut input,
        )
        .unwrap();
        assert_eq!(got.len(), 2);
        assert!(input.is_empty());
    }

    #[test]
    fn a_missing_or_non_utf8_file_fails_by_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("none.txt");
        let err = read_list(&missing, &mut &b""[..]).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Other);
        assert!(format!("{err:#}").starts_with("reading "), "{err:#}");
        let bad = tmp.path().join("bad.txt");
        std::fs::write(&bad, b"spotify:track:\xe9\n").unwrap();
        let err = read_list(&bad, &mut &b""[..]).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        let err = read_list(Path::new("-"), &mut &b"\xff\n"[..]).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
    }

    #[test]
    fn duplicates_are_kept_once_and_counted() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let track = format!("spotify:track:{TRACK_ID}");
        let got = resolve(
            &p,
            LIBRARY_ITEMS,
            &[&track, &format!("spotify:album:{ALBUM_ID}"), TRACK_ID],
        )
        .unwrap();
        assert_eq!(got.len(), 3);
        let (kept, dropped) = dedupe(got);
        assert_eq!(dropped, 1);
        assert_eq!(
            uris(&kept),
            [track.clone(), format!("spotify:album:{ALBUM_ID}")]
        );
        assert_eq!(kept[0].given, track);
    }
}
