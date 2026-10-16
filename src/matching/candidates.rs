//! Candidate retrieval: the search queries for an item, run in order
//! through a [`Search`] until one yields an automatic decision.

use anyhow::Result;

use crate::matching::normalize;
use crate::matching::score::{self, Decision, Scored, TrackItem};
use crate::spotify::client::{SEARCH_LIMIT, SearchResults, Spotify};
use crate::spotify::ids::IdKind;

/// A Spotify search: up to ten results of `kind` for `q`.
pub trait Search {
    fn search(&mut self, q: &str, kind: IdKind) -> Result<SearchResults>;
}

impl Search for Spotify {
    fn search(&mut self, q: &str, kind: IdKind) -> Result<SearchResults> {
        Spotify::search(self, q, &[kind], SEARCH_LIMIT)
    }
}

/// What the queries for one item found and decided.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub scored: Scored,
    /// The queries run, in order.
    pub queries: Vec<String>,
}

/// The folded form of `text` for a query; the trimmed text itself when
/// folding leaves nothing (a title of punctuation only).
fn query_text(folded: String, raw: &str) -> String {
    if folded.is_empty() {
        raw.trim().replace('"', "")
    } else {
        folded
    }
}

/// The text queries for a track, in the order they are run:
/// `track:"…" artist:"…"`, with `album:"…"` when the item has an album, then
/// the bare title and artist. Titles and artists are folded with their
/// flag-carrying brackets removed.
pub fn track_text_queries(item: &TrackItem) -> Vec<String> {
    let title = query_text(normalize::title(&item.title).text, &item.title);
    let artist = query_text(normalize::artist(&item.artist).text, &item.artist);
    let mut queries = vec![format!("track:\"{title}\" artist:\"{artist}\"")];
    if let Some(album) = item.album.as_deref().filter(|a| !a.trim().is_empty()) {
        let album = query_text(normalize::title(album).text, album);
        queries.push(format!(
            "track:\"{title}\" artist:\"{artist}\" album:\"{album}\""
        ));
    }
    queries.push(format!("{title} {artist}"));
    queries
}

/// The album queries: `album:"…" artist:"…"`, then the bare name and
/// artist; without an artist, `album:"…"` and the bare name.
pub fn album_queries(name: &str, artist: &str) -> Vec<String> {
    let name = query_text(normalize::title(name).text, name);
    if artist.trim().is_empty() {
        return vec![format!("album:\"{name}\""), name];
    }
    let artist = query_text(normalize::artist(artist).text, artist);
    vec![
        format!("album:\"{name}\" artist:\"{artist}\""),
        format!("{name} {artist}"),
    ]
}

/// Runs `queries` in order, gathering every result, and re-decides after
/// each one; stops at the first automatic decision.
fn run_queries<T>(
    search: &mut dyn Search,
    kind: IdKind,
    queries: Vec<String>,
    pick: impl Fn(SearchResults) -> Vec<T>,
    decide: impl Fn(&[T]) -> Scored,
) -> Result<Found> {
    let mut gathered: Vec<T> = Vec::new();
    let mut run = Vec::new();
    let mut scored = decide(&gathered);
    for q in queries {
        gathered.extend(pick(search.search(&q, kind)?));
        run.push(q);
        scored = decide(&gathered);
        if matches!(scored.decision, Decision::Auto { .. }) {
            break;
        }
    }
    Ok(Found {
        scored,
        queries: run,
    })
}

/// Finds a track: `isrc:<code>` first when the item has an ISRC, then the
/// text queries.
pub fn find_track(search: &mut dyn Search, item: &TrackItem) -> Result<Found> {
    let mut queries = Vec::new();
    if let Some(code) = item.isrc.as_deref().filter(|c| !c.trim().is_empty()) {
        queries.push(format!("isrc:{}", code.trim()));
    }
    queries.extend(track_text_queries(item));
    run_queries(
        search,
        IdKind::Track,
        queries,
        |r| r.tracks,
        |c| score::decide_track(item, c),
    )
}

/// Finds an artist by name.
pub fn find_artist(search: &mut dyn Search, name: &str) -> Result<Found> {
    run_queries(
        search,
        IdKind::Artist,
        vec![name.trim().to_owned()],
        |r| r.artists,
        |c| score::decide_artist(name, c),
    )
}

/// Finds an album by name and artist.
pub fn find_album(search: &mut dyn Search, name: &str, artist: &str) -> Result<Found> {
    run_queries(
        search,
        IdKind::Album,
        album_queries(name, artist),
        |r| r.albums,
        |c| score::decide_album(name, artist, c),
    )
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;

    use super::*;

    /// A search that answers from a table of query → results and records
    /// every query it is asked; an unknown query finds nothing.
    #[derive(Default)]
    pub struct FakeSearch {
        pub answers: HashMap<String, SearchResults>,
        pub asked: Vec<String>,
        /// When set, every search fails with this kind.
        pub fail: Option<crate::error::Kind>,
    }

    impl Search for FakeSearch {
        fn search(&mut self, q: &str, _kind: IdKind) -> Result<SearchResults> {
            self.asked.push(q.to_owned());
            if let Some(kind) = self.fail {
                crate::fail!(kind, "search refused");
            }
            Ok(self.answers.get(q).cloned().unwrap_or_default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::FakeSearch;
    use super::*;
    use crate::matching::model::Method;
    use crate::spotify::client::{FoundAlbum, FoundArtist, FoundTrack};

    fn item() -> TrackItem {
        TrackItem {
            title: "Song (Remastered 2011)".into(),
            artist: "The Band feat. Guest".into(),
            album: Some("Record".into()),
            duration_s: Some(200),
            explicit_hint: false,
            isrc: Some("USAAA0400001".into()),
        }
    }

    fn track(uri: &str, name: &str, isrc: Option<&str>) -> FoundTrack {
        FoundTrack {
            uri: uri.into(),
            name: name.into(),
            artists: vec!["The Band".into()],
            album: Some("Record".into()),
            album_type: Some("album".into()),
            release_date: Some("2004-01-01".into()),
            duration_ms: Some(200_000),
            explicit: Some(false),
            isrc: isrc.map(Into::into),
        }
    }

    fn tracks(list: Vec<FoundTrack>) -> SearchResults {
        SearchResults {
            tracks: list,
            ..SearchResults::default()
        }
    }

    #[test]
    fn track_queries_are_folded_and_drop_flags() {
        assert_eq!(
            track_text_queries(&item()),
            [
                "track:\"song\" artist:\"the band\"",
                "track:\"song\" artist:\"the band\" album:\"record\"",
                "song the band",
            ]
        );
        let no_album = TrackItem {
            album: None,
            ..item()
        };
        assert_eq!(track_text_queries(&no_album).len(), 2);
        let odd = TrackItem {
            title: "?!".into(),
            album: None,
            ..item()
        };
        assert_eq!(track_text_queries(&odd)[1], "?! the band");
    }

    #[test]
    fn an_isrc_hit_stops_at_the_first_query() {
        let mut search = FakeSearch::default();
        search.answers.insert(
            "isrc:USAAA0400001".into(),
            tracks(vec![track("spotify:track:a", "Song", Some("usaaa0400001"))]),
        );
        let found = find_track(&mut search, &item()).unwrap();
        assert_eq!(search.asked, ["isrc:USAAA0400001"]);
        assert_eq!(found.queries, search.asked);
        assert_eq!(
            found.scored.decision,
            Decision::Auto {
                uri: "spotify:track:a".into(),
                method: Method::Isrc
            }
        );
    }

    #[test]
    fn text_queries_run_until_one_decides() {
        let mut search = FakeSearch::default();
        search.answers.insert(
            "track:\"song\" artist:\"the band\" album:\"record\"".into(),
            tracks(vec![track("spotify:track:b", "Song", None)]),
        );
        let found = find_track(&mut search, &item()).unwrap();
        assert_eq!(
            search.asked,
            [
                "isrc:USAAA0400001",
                "track:\"song\" artist:\"the band\"",
                "track:\"song\" artist:\"the band\" album:\"record\"",
            ]
        );
        assert_eq!(
            found.scored.decision,
            Decision::Auto {
                uri: "spotify:track:b".into(),
                method: Method::Exact
            }
        );
    }

    #[test]
    fn candidates_from_every_query_are_kept_when_none_decides() {
        let mut search = FakeSearch::default();
        search.answers.insert(
            "track:\"song\" artist:\"the band\"".into(),
            tracks(vec![track("spotify:track:c", "Song Instrumental", None)]),
        );
        search.answers.insert(
            "song the band".into(),
            tracks(vec![
                track("spotify:track:c", "Song Instrumental", None),
                track("spotify:track:d", "Song Reprise", None),
            ]),
        );
        let found = find_track(&mut search, &item()).unwrap();
        assert_eq!(found.queries.len(), 4);
        assert_eq!(found.scored.decision, Decision::Ambiguous);
        assert_eq!(found.scored.candidates.len(), 2, "deduplicated by URI");
    }

    #[test]
    fn nothing_found_is_unmatched() {
        let mut search = FakeSearch::default();
        let found = find_track(&mut search, &item()).unwrap();
        assert_eq!(found.scored.decision, Decision::Unmatched);
        assert_eq!(found.queries.len(), 4);
    }

    #[test]
    fn a_failed_search_ends_the_item() {
        let mut search = FakeSearch {
            fail: Some(crate::error::Kind::Quota),
            ..FakeSearch::default()
        };
        let err = find_track(&mut search, &item()).unwrap_err();
        assert_eq!(crate::error::kind(&err), crate::error::Kind::Quota);
        assert_eq!(search.asked.len(), 1);
    }

    #[test]
    fn artists_and_albums() {
        let mut search = FakeSearch::default();
        search.answers.insert(
            "Some Artist".into(),
            SearchResults {
                artists: vec![FoundArtist {
                    uri: "spotify:artist:a".into(),
                    name: "Some Artist".into(),
                }],
                ..SearchResults::default()
            },
        );
        let found = find_artist(&mut search, " Some Artist ").unwrap();
        assert_eq!(found.queries, ["Some Artist"]);
        assert!(matches!(found.scored.decision, Decision::Auto { .. }));

        search.answers.insert(
            "record the band".into(),
            SearchResults {
                albums: vec![FoundAlbum {
                    uri: "spotify:album:r".into(),
                    name: "Record".into(),
                    artists: vec!["The Band".into()],
                    album_type: Some("album".into()),
                    release_date: None,
                }],
                ..SearchResults::default()
            },
        );
        let found = find_album(&mut search, "Record", "The Band").unwrap();
        assert_eq!(
            found.queries,
            ["album:\"record\" artist:\"the band\"", "record the band"]
        );
        assert_eq!(album_queries("Record", " "), ["album:\"record\"", "record"]);
        assert_eq!(
            found.scored.decision,
            Decision::Auto {
                uri: "spotify:album:r".into(),
                method: Method::Exact
            }
        );
    }
}
