//! Deterministic scoring of search candidates and the decision each item's
//! scores lead to.
//!
//! A track's candidates are scored on title, artist, duration, album and
//! explicit agreement; [`TRACK_AUTO_RULES`] lists when the best one is
//! taken without asking anyone. Everything that is neither auto nor
//! clearly absent is left `ambiguous` for the LLM or a human.
//!
//! Three comparisons, for three jobs: [`text_similarity`] ranks titles and
//! album names, and rates a word-subset pair (`song` / `song instrumental`)
//! perfect; an auto decision on a title or album name therefore also needs
//! [`same_words`], so a word one side has and the other lacks always goes
//! to the LLM. Artist names use [`name_similarity`], which is never
//! subset-blind.

use std::cmp::Ordering;

use crate::matching::model::{Candidate, Method};
use crate::matching::normalize::{self, Normalized};
use crate::spotify::client::{FoundAlbum, FoundArtist, FoundTrack};

/// The most candidates an entry keeps.
pub const MAX_CANDIDATES: usize = 10;

/// A candidate whose title similarity is below this for every candidate
/// leaves the item `unmatched`.
pub const UNMATCHED_BELOW: f64 = 0.5;

/// A second candidate scoring within this of the top one, on a different
/// album, makes a track ambiguous.
pub const TIE_MARGIN: f64 = 0.02;

/// Subtracted from title similarity when `live`, `acoustic` or `demo` is on
/// one side only.
pub const VARIANT_PENALTY: f64 = 0.3;

/// A name similarity at or above this is an artist auto match.
pub const ARTIST_AUTO: f64 = 0.95;

/// Album auto thresholds: name, then artist.
pub const ALBUM_AUTO: (f64, f64) = (0.95, 0.90);

/// One row of the track auto-match table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutoRule {
    pub min_title: f64,
    pub min_artist: f64,
    /// The largest allowed `|duration_s * 1000 - duration_ms|`.
    pub max_delta_ms: u64,
    /// Whether the row applies only when the albums are equal.
    pub needs_album: bool,
}

/// A track's best candidate is taken (`matched`, method `exact`) when it
/// satisfies any row, its title has the Pandora title's words
/// ([`same_words`]), the Pandora record has a duration, and no tie
/// ([`TIE_MARGIN`]) holds.
pub const TRACK_AUTO_RULES: [AutoRule; 2] = [
    AutoRule {
        min_title: 0.95,
        min_artist: 0.90,
        max_delta_ms: 3_000,
        needs_album: false,
    },
    AutoRule {
        min_title: 0.95,
        min_artist: 0.90,
        max_delta_ms: 5_000,
        needs_album: true,
    },
];

/// The Pandora side of a track to match.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackItem {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_s: Option<u32>,
    pub explicit_hint: bool,
    pub isrc: Option<String>,
}

/// What the scores decide.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Take this URI, high confidence.
    Auto {
        uri: String,
        method: Method,
    },
    Ambiguous,
    Unmatched,
}

/// A decision and the candidates behind it, best first, at most
/// [`MAX_CANDIDATES`].
#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub decision: Decision,
    pub candidates: Vec<Candidate>,
}

/// One track candidate's component scores.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackScore {
    /// Title similarity before the variant penalty.
    pub title_raw: f64,
    /// Title similarity after the variant penalty.
    pub title: f64,
    /// Whether both titles have the same set of words.
    pub same_words: bool,
    pub artist: f64,
    pub delta_ms: Option<u64>,
    pub album_equal: bool,
    pub explicit_agrees: Option<bool>,
    /// The weighted total candidates are ranked by.
    pub total: f64,
}

/// Similarity of two folded strings in `[0, 1]`: the larger of
/// Jaro-Winkler and the token-set ratio. Empty input scores 0.
pub fn text_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    strsim::jaro_winkler(a, b).max(token_set_ratio(a, b))
}

/// Similarity of two folded names: Jaro-Winkler with a leading `the`
/// ignored. Stricter than [`text_similarity`], whose token-set ratio would
/// rate `queen` equal to `queen naija`.
pub fn name_similarity(a: &str, b: &str) -> f64 {
    let a = a.strip_prefix("the ").unwrap_or(a);
    let b = b.strip_prefix("the ").unwrap_or(b);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    strsim::jaro_winkler(a, b)
}

/// Whether two folded strings have the same set of words, in any order.
pub fn same_words(a: &str, b: &str) -> bool {
    use std::collections::BTreeSet;
    a.split_whitespace().collect::<BTreeSet<_>>() == b.split_whitespace().collect::<BTreeSet<_>>()
}

/// The token-set ratio: the shared tokens compared against each side's
/// shared-plus-own tokens, best of the three pairings, by normalized
/// Levenshtein.
pub fn token_set_ratio(a: &str, b: &str) -> f64 {
    use std::collections::BTreeSet;
    let ta: BTreeSet<&str> = a.split_whitespace().collect();
    let tb: BTreeSet<&str> = b.split_whitespace().collect();
    let join = |set: Vec<&str>| set.join(" ");
    let common = join(ta.intersection(&tb).copied().collect());
    let only_a = join(ta.difference(&tb).copied().collect());
    let only_b = join(tb.difference(&ta).copied().collect());
    let with = |rest: &str| {
        [common.as_str(), rest]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let (sa, sb) = (with(&only_a), with(&only_b));
    let ratio = |x: &str, y: &str| {
        if x.is_empty() || y.is_empty() {
            0.0
        } else {
            strsim::normalized_levenshtein(x, y)
        }
    };
    ratio(&common, &sa)
        .max(ratio(&common, &sb))
        .max(ratio(&sa, &sb))
}

/// The best similarity of any of `candidates` to the Pandora artist, the
/// `feat.` tail split off both.
pub fn artist_similarity(pandora: &str, candidates: &[String]) -> f64 {
    let ours = normalize::artist(pandora);
    candidates
        .iter()
        .map(|c| name_similarity(&ours.text, &normalize::artist(c).text))
        .fold(0.0, f64::max)
}

/// Scores one track candidate against the item.
pub fn score_track(item: &TrackItem, title: &Normalized, cand: &FoundTrack) -> TrackScore {
    let theirs = normalize::title(&cand.name);
    let title_raw = text_similarity(&title.text, &theirs.text);
    let same_words = same_words(&title.text, &theirs.text);
    let title_sim = if title.variant_mismatch(&theirs) {
        (title_raw - VARIANT_PENALTY).max(0.0)
    } else {
        title_raw
    };
    let artist = artist_similarity(&item.artist, &cand.artists);
    let delta_ms = match (item.duration_s, cand.duration_ms) {
        (Some(s), Some(ms)) => Some((u64::from(s) * 1000).abs_diff(ms)),
        _ => None,
    };
    let album_equal = match (&item.album, &cand.album) {
        (Some(a), Some(b)) => {
            let (a, b) = (normalize::title(a).text, normalize::title(b).text);
            !a.is_empty() && a == b
        }
        _ => false,
    };
    let explicit_agrees = cand.explicit.map(|e| e == item.explicit_hint);

    // Duration counts fully within 3 s and falls to nothing at 30 s; an
    // unknown duration counts half.
    let duration = match delta_ms {
        Some(d) if d <= 3_000 => 1.0,
        Some(d) => (1.0 - (d - 3_000) as f64 / 27_000.0).max(0.0),
        None => 0.5,
    };
    let explicit = match explicit_agrees {
        Some(true) => 1.0,
        Some(false) => 0.0,
        None => 0.5,
    };
    let total = 0.5 * title_sim
        + 0.3 * artist
        + 0.1 * duration
        + 0.05 * f64::from(u8::from(album_equal))
        + 0.05 * explicit;
    TrackScore {
        title_raw,
        title: title_sim,
        same_words,
        artist,
        delta_ms,
        album_equal,
        explicit_agrees,
        total,
    }
}

/// Whether `score` has [`same_words`] and satisfies a row of
/// [`TRACK_AUTO_RULES`].
pub fn passes_auto(score: &TrackScore) -> bool {
    let Some(delta) = score.delta_ms else {
        return false;
    };
    if !score.same_words {
        return false;
    }
    TRACK_AUTO_RULES.iter().any(|r| {
        score.title >= r.min_title
            && score.artist >= r.min_artist
            && delta <= r.max_delta_ms
            && (!r.needs_album || score.album_equal)
    })
}

/// The candidate whose ISRC equals `code` (ASCII case-insensitive): an
/// `album` release first, then the earliest release date.
pub fn isrc_pick<'a>(
    code: &str,
    candidates: impl IntoIterator<Item = &'a FoundTrack>,
) -> Option<&'a FoundTrack> {
    candidates
        .into_iter()
        .filter(|c| {
            c.isrc
                .as_deref()
                .is_some_and(|i| i.eq_ignore_ascii_case(code))
        })
        .min_by(|a, b| {
            let rank = |c: &FoundTrack| c.album_type.as_deref() != Some("album");
            rank(a)
                .cmp(&rank(b))
                .then_with(|| cmp_release(&a.release_date, &b.release_date))
        })
}

/// Earlier dates first, a missing date last.
fn cmp_release(a: &Option<String>, b: &Option<String>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Decides a track from its candidates (deduplicated by URI, first kept):
/// an ISRC hit first, then the auto table, the tie rule and the
/// unmatched/ambiguous fall-through.
pub fn decide_track(item: &TrackItem, candidates: &[FoundTrack]) -> Scored {
    let candidates = dedup(candidates, |c| &c.uri);
    let title = normalize::title(&item.title);
    let mut scored: Vec<(TrackScore, &FoundTrack)> = candidates
        .iter()
        .map(|c| (score_track(item, &title, c), *c))
        .collect();
    scored.sort_by(|a, b| b.0.total.total_cmp(&a.0.total));
    let kept: Vec<Candidate> = scored
        .iter()
        .take(MAX_CANDIDATES)
        .map(|(s, c)| track_candidate(c, s.total))
        .collect();

    if let Some(code) = item.isrc.as_deref()
        && let Some(hit) = isrc_pick(code, candidates.iter().copied())
    {
        return Scored {
            decision: Decision::Auto {
                uri: hit.uri.clone(),
                method: Method::Isrc,
            },
            candidates: kept,
        };
    }

    let decision = match scored.first() {
        None => Decision::Unmatched,
        Some(_) if scored.iter().all(|(s, _)| s.title_raw < UNMATCHED_BELOW) => Decision::Unmatched,
        Some((top, top_cand)) => {
            let tie = scored.iter().skip(1).any(|(s, c)| {
                top.total - s.total <= TIE_MARGIN && !same_album(&top_cand.album, &c.album)
            });
            if item.duration_s.is_some() && passes_auto(top) && !tie {
                Decision::Auto {
                    uri: top_cand.uri.clone(),
                    method: Method::Exact,
                }
            } else {
                Decision::Ambiguous
            }
        }
    };
    Scored {
        decision,
        candidates: kept,
    }
}

fn same_album(a: &Option<String>, b: &Option<String>) -> bool {
    let fold = |x: &Option<String>| x.as_deref().map(|s| normalize::title(s).text);
    fold(a) == fold(b)
}

/// Decides an artist: auto when exactly one candidate reaches
/// [`ARTIST_AUTO`], ambiguous when several do, unmatched when none
/// reaches [`UNMATCHED_BELOW`].
pub fn decide_artist(name: &str, candidates: &[FoundArtist]) -> Scored {
    let ours = normalize::artist(name).text;
    let mut scored: Vec<(f64, &FoundArtist)> = dedup(candidates, |c| &c.uri)
        .into_iter()
        .map(|c| (name_similarity(&ours, &normalize::artist(&c.name).text), c))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    let top: Vec<_> = scored.iter().filter(|(s, _)| *s >= ARTIST_AUTO).collect();
    let decision = match top.as_slice() {
        [(_, only)] => Decision::Auto {
            uri: only.uri.clone(),
            method: Method::Exact,
        },
        [] if scored.iter().all(|(s, _)| *s < UNMATCHED_BELOW) => Decision::Unmatched,
        _ => Decision::Ambiguous,
    };
    Scored {
        decision,
        candidates: scored
            .iter()
            .take(MAX_CANDIDATES)
            .map(|(s, c)| Candidate {
                uri: c.uri.clone(),
                name: c.name.clone(),
                score: round(*s),
                ..Candidate::default()
            })
            .collect(),
    }
}

/// Decides an album: auto when the best candidate's name reaches
/// [`ALBUM_AUTO`]`.0` with [`same_words`] and its artist `.1` with no
/// other candidate within
/// [`TIE_MARGIN`]; unmatched when every name is below
/// [`UNMATCHED_BELOW`]; ambiguous otherwise.
pub fn decide_album(name: &str, artist: &str, candidates: &[FoundAlbum]) -> Scored {
    let ours = normalize::title(name).text;
    let mut scored: Vec<(f64, f64, f64, &FoundAlbum)> = dedup(candidates, |c| &c.uri)
        .into_iter()
        .map(|c| {
            let n = text_similarity(&ours, &normalize::title(&c.name).text);
            let a = artist_similarity(artist, &c.artists);
            (0.6 * n + 0.4 * a, n, a, c)
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    let words_match = |c: &FoundAlbum| same_words(&ours, &normalize::title(&c.name).text);
    let decision = match scored.first() {
        None => Decision::Unmatched,
        Some(_) if scored.iter().all(|(_, n, _, _)| *n < UNMATCHED_BELOW) => Decision::Unmatched,
        Some((total, n, a, c)) => {
            let tie = scored.iter().skip(1).any(|(t, ..)| total - t <= TIE_MARGIN);
            if *n >= ALBUM_AUTO.0 && *a >= ALBUM_AUTO.1 && words_match(c) && !tie {
                Decision::Auto {
                    uri: c.uri.clone(),
                    method: Method::Exact,
                }
            } else {
                Decision::Ambiguous
            }
        }
    };
    Scored {
        decision,
        candidates: scored
            .iter()
            .take(MAX_CANDIDATES)
            .map(|(t, _, _, c)| Candidate {
                uri: c.uri.clone(),
                name: c.name.clone(),
                artists: c.artists.clone(),
                album_type: c.album_type.clone(),
                release_date: c.release_date.clone(),
                score: round(*t),
                ..Candidate::default()
            })
            .collect(),
    }
}

fn track_candidate(c: &FoundTrack, score: f64) -> Candidate {
    Candidate {
        uri: c.uri.clone(),
        name: c.name.clone(),
        artists: c.artists.clone(),
        album: c.album.clone(),
        album_type: c.album_type.clone(),
        release_date: c.release_date.clone(),
        duration_ms: c.duration_ms,
        explicit: c.explicit,
        isrc: c.isrc.clone(),
        score: round(score),
    }
}

/// Three decimals, enough to read and stable in the file.
fn round(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn dedup<T>(items: &[T], key: impl Fn(&T) -> &String) -> Vec<&T> {
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .filter(|i| seen.insert(key(i).clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(uri: &str, name: &str, artist: &str, album: &str, ms: u64) -> FoundTrack {
        FoundTrack {
            uri: format!("spotify:track:{uri}"),
            name: name.into(),
            artists: vec![artist.into()],
            album: Some(album.into()),
            album_type: Some("album".into()),
            release_date: Some("2000-01-01".into()),
            duration_ms: Some(ms),
            explicit: Some(false),
            isrc: None,
        }
    }

    fn item(title: &str, artist: &str, album: Option<&str>, secs: Option<u32>) -> TrackItem {
        TrackItem {
            title: title.into(),
            artist: artist.into(),
            album: album.map(str::to_owned),
            duration_s: secs,
            explicit_hint: false,
            isrc: None,
        }
    }

    fn auto(uri: &str, method: Method) -> Decision {
        Decision::Auto {
            uri: format!("spotify:track:{uri}"),
            method,
        }
    }

    #[test]
    fn similarity_basics() {
        assert_eq!(text_similarity("song", "song"), 1.0);
        assert_eq!(text_similarity("", "song"), 0.0);
        assert_eq!(token_set_ratio("the long road", "road the long"), 1.0);
        assert_eq!(token_set_ratio("yesterday", "yesterday once more"), 1.0);
        assert!(text_similarity("song", "entirely different") < UNMATCHED_BELOW);
        assert_eq!(name_similarity("the band", "band"), 1.0);
        assert!(name_similarity("queen", "queen naija") < ARTIST_AUTO);
        assert_eq!(
            artist_similarity(
                "Artist A feat. Artist B",
                &["Other".into(), "Artist A".into()]
            ),
            1.0
        );
    }

    #[test]
    fn auto_rule_rows() {
        let base = TrackScore {
            title_raw: 1.0,
            title: 1.0,
            same_words: true,
            artist: 1.0,
            delta_ms: Some(0),
            album_equal: false,
            explicit_agrees: None,
            total: 1.0,
        };
        // Row 1: title, artist and a 3 s delta, any album.
        for (title, artist, delta, album, want) in [
            (0.95, 0.90, 3_000, false, true),
            (0.949, 0.90, 0, false, false),
            (0.95, 0.899, 0, false, false),
            (0.95, 0.90, 3_001, false, false),
            // Row 2: up to 5 s when the albums are equal.
            (0.95, 0.90, 5_000, true, true),
            (0.95, 0.90, 5_001, true, false),
            (0.94, 0.90, 1_000, true, false),
        ] {
            let s = TrackScore {
                title,
                artist,
                delta_ms: Some(delta),
                album_equal: album,
                ..base
            };
            assert_eq!(passes_auto(&s), want, "{title} {artist} {delta} {album}");
        }
        // No duration on either side: never auto.
        assert!(!passes_auto(&TrackScore {
            delta_ms: None,
            ..base
        }));
        // A word on one side only: never auto, whatever the similarity.
        assert!(!passes_auto(&TrackScore {
            same_words: false,
            ..base
        }));
    }

    #[test]
    fn a_title_with_a_word_the_other_lacks_is_never_auto() {
        assert!(same_words("the long road", "road the long"));
        assert!(!same_words("song", "song instrumental"));
        let plain = item("Song", "Artist", None, Some(200));
        for name in [
            "Song (Instrumental)",
            "Song - Instrumental",
            "Song (Club Remix)",
            "Song (Karaoke Version)",
            "Song - Reprise",
            "Song Part 2",
        ] {
            let s = decide_track(&plain, &[track("a", name, "Artist", "Album", 200_500)]);
            assert_eq!(s.decision, Decision::Ambiguous, "{name}");
            assert_eq!(s.candidates.len(), 1, "{name}");
        }
        // The reverse direction: a Pandora version against a plain title.
        let s = decide_track(
            &item("Song (Instrumental)", "Artist", None, Some(200)),
            &[track("a", "Song", "Artist", "Album", 200_500)],
        );
        assert_eq!(s.decision, Decision::Ambiguous);
        // The original and the instrumental on one album, the instrumental
        // listed first: not auto-matched to the instrumental.
        let s = decide_track(
            &plain,
            &[
                track("i", "Song - Instrumental", "Artist", "Deluxe", 200_500),
                track("a", "Song", "Artist", "Deluxe", 200_500),
            ],
        );
        assert_eq!(s.decision, Decision::Ambiguous);
        // Word order and flags lifted into brackets are no obstacle.
        let s = decide_track(
            &item("Road The Long", "Artist", None, Some(200)),
            &[track(
                "a",
                "The Long Road - 2011 Remaster",
                "Artist",
                "Album",
                200_000,
            )],
        );
        assert_eq!(s.decision, auto("a", Method::Exact));
    }

    #[test]
    fn an_exact_title_artist_and_duration_is_auto() {
        let s = decide_track(
            &item("Song", "Artist", None, Some(200)),
            &[track("a", "Song", "Artist", "Album", 201_500)],
        );
        assert_eq!(s.decision, auto("a", Method::Exact));
        assert_eq!(s.candidates.len(), 1);
        assert!(s.candidates[0].score > 0.9);
    }

    #[test]
    fn duration_tolerance_depends_on_album_equality() {
        let cand = [track("a", "Song", "Artist", "Album", 204_000)];
        let without = decide_track(&item("Song", "Artist", Some("Other"), Some(200)), &cand);
        assert_eq!(without.decision, Decision::Ambiguous);
        let with = decide_track(&item("Song", "Artist", Some("Album"), Some(200)), &cand);
        assert_eq!(with.decision, auto("a", Method::Exact));
        let far = [track("a", "Song", "Artist", "Album", 206_000)];
        let too_far = decide_track(&item("Song", "Artist", Some("Album"), Some(200)), &far);
        assert_eq!(too_far.decision, Decision::Ambiguous);
    }

    #[test]
    fn a_tie_on_a_different_album_is_ambiguous() {
        let it = item("Song", "Artist", None, Some(200));
        let tie = decide_track(
            &it,
            &[
                track("a", "Song", "Artist", "Album", 200_000),
                track("b", "Song", "Artist", "Greatest Hits", 200_000),
            ],
        );
        assert_eq!(tie.decision, Decision::Ambiguous);
        assert_eq!(tie.candidates.len(), 2);
        // The same recording twice on one album is no tie.
        let same = decide_track(
            &it,
            &[
                track("a", "Song", "Artist", "Album", 200_000),
                track("b", "Song", "Artist", "Album", 200_000),
            ],
        );
        assert_eq!(same.decision, auto("a", Method::Exact));
        // A clearly weaker second candidate is no tie either.
        let clear = decide_track(
            &it,
            &[
                track("a", "Song", "Artist", "Album", 200_000),
                track("b", "Song", "Somebody Else", "Hits", 240_000),
            ],
        );
        assert_eq!(clear.decision, auto("a", Method::Exact));
    }

    #[test]
    fn a_live_cut_against_a_studio_record_is_penalised() {
        let it = item("Song", "Artist", None, Some(200));
        let s = decide_track(
            &it,
            &[track("a", "Song (Live)", "Artist", "Album", 200_000)],
        );
        assert_eq!(s.decision, Decision::Ambiguous);
        let studio = score_track(
            &it,
            &normalize::title("Song"),
            &track("a", "Song", "Artist", "Album", 200_000),
        );
        let live = score_track(
            &it,
            &normalize::title("Song"),
            &track("b", "Song - Live", "Artist", "Album", 200_000),
        );
        assert!((studio.title - live.title - VARIANT_PENALTY).abs() < 1e-9);
        assert_eq!(live.title_raw, 1.0);
        // Remaster is not a variant: still auto.
        let r = decide_track(
            &it,
            &[track(
                "a",
                "Song - Remastered 2011",
                "Artist",
                "Album",
                200_000,
            )],
        );
        assert_eq!(r.decision, auto("a", Method::Exact));
    }

    #[test]
    fn a_record_without_duration_is_never_auto() {
        let s = decide_track(
            &item("Song", "Artist", Some("Album"), None),
            &[track("a", "Song", "Artist", "Album", 200_000)],
        );
        assert_eq!(s.decision, Decision::Ambiguous);
        let none = decide_track(&item("Song", "Artist", None, None), &[]);
        assert_eq!(none.decision, Decision::Unmatched);
    }

    #[test]
    fn unmatched_when_nothing_or_nothing_close() {
        let it = item("Song", "Artist", None, Some(200));
        assert_eq!(decide_track(&it, &[]).decision, Decision::Unmatched);
        let far = decide_track(
            &it,
            &[track(
                "a",
                "Entirely Unrelated Words",
                "Artist",
                "Album",
                200_000,
            )],
        );
        assert_eq!(far.decision, Decision::Unmatched);
        assert_eq!(far.candidates.len(), 1);
        let wrong_artist = decide_track(&it, &[track("a", "Song", "Nobody", "Album", 200_000)]);
        assert_eq!(wrong_artist.decision, Decision::Ambiguous);
    }

    #[test]
    fn isrc_hits_prefer_albums_then_the_earliest_release() {
        let mut single = track("s", "Song", "Artist", "Song", 200_000);
        single.album_type = Some("single".into());
        single.release_date = Some("1999-01-01".into());
        single.isrc = Some("USAAA9900001".into());
        let mut late = track("l", "Song", "Artist", "Hits", 200_000);
        late.release_date = Some("2010-01-01".into());
        late.isrc = Some("USAAA9900001".into());
        let mut early = track("e", "Song", "Artist", "Album", 200_000);
        early.release_date = Some("2000-01-01".into());
        early.isrc = Some("usaaa9900001".into());
        let mut it = item("Song", "Artist", None, None);
        it.isrc = Some("USAAA9900001".into());
        let s = decide_track(&it, &[single, late, early]);
        assert_eq!(s.decision, auto("e", Method::Isrc));
        assert_eq!(s.candidates.len(), 3);
    }

    #[test]
    fn candidates_without_isrc_fall_through_to_text_scoring() {
        let mut it = item("Song", "Artist", None, Some(200));
        it.isrc = Some("USAAA9900001".into());
        let s = decide_track(&it, &[track("a", "Song", "Artist", "Album", 200_000)]);
        assert_eq!(s.decision, auto("a", Method::Exact));
        let mut other = track("b", "Song", "Artist", "Album", 200_000);
        other.isrc = Some("GBAAA0000002".into());
        let s = decide_track(&it, &[other]);
        assert_eq!(s.decision, auto("b", Method::Exact));
    }

    #[test]
    fn candidates_are_deduplicated_sorted_and_capped() {
        let it = item("Song", "Artist", None, Some(200));
        let mut cands: Vec<FoundTrack> = (0..12)
            .map(|i| {
                track(
                    &format!("x{i}"),
                    &format!("Song {i}"),
                    "Other",
                    "A",
                    100_000,
                )
            })
            .collect();
        cands.push(track("a", "Song", "Artist", "Album", 200_000));
        cands.push(track("a", "Song", "Artist", "Album", 200_000));
        let s = decide_track(&it, &cands);
        assert_eq!(s.candidates.len(), MAX_CANDIDATES);
        assert_eq!(s.candidates[0].uri, "spotify:track:a");
        assert_eq!(
            s.candidates
                .iter()
                .filter(|c| c.uri == "spotify:track:a")
                .count(),
            1
        );
        assert!(s.candidates.windows(2).all(|w| w[0].score >= w[1].score));
        assert_eq!(s.decision, auto("a", Method::Exact));
    }

    fn artist(uri: &str, name: &str) -> FoundArtist {
        FoundArtist {
            uri: format!("spotify:artist:{uri}"),
            name: name.into(),
        }
    }

    #[test]
    fn artist_decisions() {
        let one = decide_artist("Queen", &[artist("q", "Queen"), artist("n", "Queen Naija")]);
        assert_eq!(
            one.decision,
            Decision::Auto {
                uri: "spotify:artist:q".into(),
                method: Method::Exact
            }
        );
        assert_eq!(one.candidates[0].score, 1.0);
        assert!(one.candidates[0].artists.is_empty());
        let two = decide_artist("Band", &[artist("a", "Band"), artist("b", "The Band")]);
        assert_eq!(two.decision, Decision::Ambiguous);
        let none = decide_artist("Band", &[artist("a", "Xylophonic Quartet")]);
        assert_eq!(none.decision, Decision::Unmatched);
        assert_eq!(decide_artist("Band", &[]).decision, Decision::Unmatched);
        let close = decide_artist("Bandd", &[artist("a", "Band Of Horses")]);
        assert_eq!(close.decision, Decision::Ambiguous);
    }

    fn album(uri: &str, name: &str, artist: &str) -> FoundAlbum {
        FoundAlbum {
            uri: format!("spotify:album:{uri}"),
            name: name.into(),
            artists: vec![artist.into()],
            album_type: Some("album".into()),
            release_date: Some("2000".into()),
        }
    }

    #[test]
    fn album_decisions() {
        let a = decide_album(
            "Album",
            "Artist",
            &[
                album("a", "Album", "Artist"),
                album("b", "Other Record", "Artist"),
            ],
        );
        assert_eq!(
            a.decision,
            Decision::Auto {
                uri: "spotify:album:a".into(),
                method: Method::Exact
            }
        );
        assert_eq!(a.candidates[0].artists, ["Artist"]);
        assert_eq!(a.candidates[0].release_date.as_deref(), Some("2000"));
        let tie = decide_album(
            "Album",
            "Artist",
            &[
                album("a", "Album", "Artist"),
                album("b", "Album (Deluxe)", "Artist"),
            ],
        );
        assert_eq!(tie.decision, Decision::Ambiguous);
        let wrong_artist = decide_album("Album", "Artist", &[album("a", "Album", "Nobody")]);
        assert_eq!(wrong_artist.decision, Decision::Ambiguous);
        for name in ["Greatest Hits Vol. 2", "Greatest Hits (Deluxe)"] {
            let lone = decide_album("Greatest Hits", "Artist", &[album("a", name, "Artist")]);
            assert_eq!(lone.decision, Decision::Ambiguous, "{name}");
        }
        let none = decide_album("Album", "Artist", &[album("a", "Zzzz Qqqq", "Artist")]);
        assert_eq!(none.decision, Decision::Unmatched);
    }
}
