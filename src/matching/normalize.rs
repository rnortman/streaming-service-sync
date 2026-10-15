//! String normalization for matching: titles and names are folded to a
//! comparable form, and the version markers they carry (`(Live)`,
//! `- Remastered 2011`, `(feat. …)`) are lifted out into flags and a list
//! of featured names.

use std::collections::BTreeSet;

use serde::Serialize;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// A version marker found in a title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    Explicit,
    Clean,
    AlbumVersion,
    SingleVersion,
    Remaster,
    Live,
    Acoustic,
    Demo,
    RadioEdit,
}

impl Flag {
    /// Whether the flag marks a different performance or cut of a song, so
    /// that one side carrying it and the other not is a mismatch.
    pub fn is_variant(self) -> bool {
        matches!(self, Flag::Live | Flag::Acoustic | Flag::Demo)
    }
}

/// A title or name in comparable form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Normalized {
    /// Folded text with the flag-carrying parts removed.
    pub text: String,
    pub flags: BTreeSet<Flag>,
    /// Featured artists, folded, in the order given.
    pub featured: Vec<String>,
}

impl Normalized {
    /// The variant flags (`live`, `acoustic`, `demo`) on exactly one of the
    /// two sides.
    pub fn variant_mismatch(&self, other: &Normalized) -> bool {
        self.flags
            .symmetric_difference(&other.flags)
            .any(|f| f.is_variant())
    }
}

/// NFKD, combining marks stripped, lowercased, `&` read as `and`,
/// apostrophes dropped, every other non-alphanumeric run collapsed to one
/// space, trimmed.
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.nfkd().filter(|c| !is_combining_mark(*c)) {
        if matches!(c, '\'' | '\u{2019}' | '\u{2018}' | '`') {
            continue;
        }
        if c == '&' {
            push_word(&mut out, &mut space, "and");
            space = true;
            continue;
        }
        if c.is_alphanumeric() {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.extend(c.to_lowercase());
        } else {
            space = true;
        }
    }
    out
}

fn push_word(out: &mut String, space: &mut bool, word: &str) {
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(word);
    *space = false;
}

/// A track or album title: flag-carrying brackets and ` - ` suffixes are
/// removed into flags, a `feat.` part into the featured list. A bracket or
/// suffix that carries no flag stays part of the title.
pub fn title(text: &str) -> Normalized {
    let mut flags = BTreeSet::new();
    let mut featured = Vec::new();
    let kept = lift_brackets(text, |inner| classify(inner, &mut flags, &mut featured));

    let mut segments = kept.split(" - ");
    let mut main = segments.next().unwrap_or_default().to_owned();
    for segment in segments {
        if !classify(segment, &mut flags, &mut featured) {
            main.push_str(" - ");
            main.push_str(segment);
        }
    }

    let (main, tail) = split_feat(&main);
    featured.extend(tail);
    Normalized {
        text: fold(&main),
        flags,
        featured,
    }
}

/// An artist name: a `feat.` tail, bracketed or not, goes to the featured
/// list. No flags.
pub fn artist(text: &str) -> Normalized {
    let mut featured = Vec::new();
    let kept = lift_brackets(text, |inner| match feat_names(inner) {
        Some(names) => {
            featured.extend(names);
            true
        }
        None => false,
    });
    let (main, tail) = split_feat(&kept);
    featured.extend(tail);
    Normalized {
        text: fold(&main),
        flags: BTreeSet::new(),
        featured,
    }
}

/// `text` with every `(…)` or `[…]` whose content `lift` takes (returns
/// true for) removed, and the brackets of the rest dropped around their
/// content. An unclosed bracket and what follows it are kept as written.
fn lift_brackets(text: &str, mut lift: impl FnMut(&str) -> bool) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find(['(', '[']) {
        let close = if rest[open..].starts_with('(') {
            ')'
        } else {
            ']'
        };
        let Some(len) = rest[open + 1..].find(close) else {
            break;
        };
        let inner = &rest[open + 1..open + 1 + len];
        kept.push_str(&rest[..open]);
        if !lift(inner) {
            kept.push(' ');
            kept.push_str(inner);
        }
        kept.push(' ');
        rest = &rest[open + 1 + len + 1..];
    }
    kept.push_str(rest);
    kept
}

/// Reads one bracket's or suffix's content: featured names, or flags. True
/// when it carried either, so the caller drops it from the title.
fn classify(content: &str, flags: &mut BTreeSet<Flag>, featured: &mut Vec<String>) -> bool {
    if let Some(names) = feat_names(content) {
        featured.extend(names);
        return true;
    }
    let found: Vec<Flag> = content
        .split(['/', ',', ';'])
        .flat_map(|part| flags_in(&fold(part)))
        .collect();
    let any = !found.is_empty();
    flags.extend(found);
    any
}

fn flags_in(words: &str) -> Vec<Flag> {
    let padded = format!(" {words} ");
    let has = |w: &str| padded.contains(&format!(" {w} "));
    let mut found = Vec::new();
    if has("explicit") {
        found.push(Flag::Explicit);
    }
    if has("clean") {
        found.push(Flag::Clean);
    }
    if has("album version") {
        found.push(Flag::AlbumVersion);
    }
    if has("single version") {
        found.push(Flag::SingleVersion);
    }
    if has("remaster") || has("remastered") || has("remastering") {
        found.push(Flag::Remaster);
    }
    if is_live(words) {
        found.push(Flag::Live);
    }
    if has("acoustic") {
        found.push(Flag::Acoustic);
    }
    if has("demo") {
        found.push(Flag::Demo);
    }
    if has("radio edit") {
        found.push(Flag::RadioEdit);
    }
    found
}

/// `live` alone, `live at|from|in|on|version|recording|edit|mix …`, `live`
/// before a year, or a trailing `live` (`bbc live`) — not every phrase
/// that happens to start with the word (`live for you`).
fn is_live(words: &str) -> bool {
    const AFTER: [&str; 8] = [
        "at",
        "from",
        "in",
        "on",
        "version",
        "recording",
        "edit",
        "mix",
    ];
    let w: Vec<&str> = words.split(' ').collect();
    w.windows(2).any(|pair| {
        pair[0] == "live"
            && (AFTER.contains(&pair[1]) || pair[1].chars().all(|c| c.is_ascii_digit()))
    }) || w.last() == Some(&"live")
}

const FEAT_MARKERS: [&str; 5] = ["featuring ", "feat. ", "feat ", "ft. ", "ft "];

/// The featured names of a bracket that starts with a `feat.` marker.
fn feat_names(content: &str) -> Option<Vec<String>> {
    let lower = content.trim_start().to_lowercase();
    let marker = FEAT_MARKERS.iter().find(|m| lower.starts_with(*m))?;
    Some(split_names(&lower[marker.len()..]))
}

/// Splits `text` at its first ` feat. `-style marker outside brackets:
/// the part before, and the featured names after.
fn split_feat(text: &str) -> (String, Vec<String>) {
    let lower = text.to_lowercase();
    let at = FEAT_MARKERS
        .iter()
        .filter_map(|m| lower.find(&format!(" {m}")).map(|i| (i, m.len() + 1)))
        .min_by_key(|(i, _)| *i);
    match at {
        // Lowercasing can change byte lengths; fall back to the whole text
        // when the offset does not land on a boundary of the original.
        Some((i, len)) if lower.len() == text.len() && text.is_char_boundary(i) => {
            (text[..i].to_owned(), split_names(&lower[i + len..]))
        }
        _ => (text.to_owned(), Vec::new()),
    }
}

/// Splits a list of names on `,`, ` & `, ` and `, ` x ` and `/`; each name
/// folded, empty ones dropped.
fn split_names(text: &str) -> Vec<String> {
    let mut parts = vec![text.to_owned()];
    for sep in [",", " & ", " and ", " x ", "/"] {
        parts = parts
            .iter()
            .flat_map(|p| p.split(sep).map(str::to_owned).collect::<Vec<_>>())
            .collect();
    }
    parts
        .iter()
        .map(|p| fold(p))
        .filter(|p| !p.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(list: &[Flag]) -> BTreeSet<Flag> {
        list.iter().copied().collect()
    }

    #[test]
    fn fold_table() {
        let cases = [
            ("Beyoncé", "beyonce"),
            ("Simon & Garfunkel", "simon and garfunkel"),
            ("Don't Stop Me Now", "dont stop me now"),
            ("Don’t Stop", "dont stop"),
            ("JAY-Z", "jay z"),
            ("  Hello,   World!! ", "hello world"),
            ("Ｆｕｌｌｗｉｄｔｈ", "fullwidth"),
            ("Mötley Crüe", "motley crue"),
            ("AC/DC", "ac dc"),
            ("", ""),
        ];
        for (input, want) in cases {
            assert_eq!(fold(input), want, "{input:?}");
        }
    }

    #[test]
    fn title_table() {
        use Flag::*;
        let cases: [(&str, &str, &[Flag], &[&str]); 16] = [
            ("Yesterday", "yesterday", &[], &[]),
            ("Yesterday (Remastered 2009)", "yesterday", &[Remaster], &[]),
            ("Yesterday - Remastered 2009", "yesterday", &[Remaster], &[]),
            (
                "Here Comes the Sun - 2019 Mix",
                "here comes the sun 2019 mix",
                &[],
                &[],
            ),
            ("Song (Live)", "song", &[Live], &[]),
            ("Song (Live at Wembley 1986)", "song", &[Live], &[]),
            ("Song - Live", "song", &[Live], &[]),
            ("Song (Acoustic Version)", "song", &[Acoustic], &[]),
            ("Song [Demo]", "song", &[Demo], &[]),
            ("Song (Radio Edit)", "song", &[RadioEdit], &[]),
            ("Song (Explicit)", "song", &[Explicit], &[]),
            (
                "Song (Clean) [Album Version]",
                "song",
                &[AlbumVersion, Clean],
                &[],
            ),
            ("Song - Single Version", "song", &[SingleVersion], &[]),
            (
                "Song (feat. Artist B & Artist C)",
                "song",
                &[],
                &["artist b", "artist c"],
            ),
            ("Song ft. Artist B", "song", &[], &["artist b"]),
            (
                "(I Can't Get No) Satisfaction",
                "i cant get no satisfaction",
                &[],
                &[],
            ),
        ];
        for (input, text, fl, feat) in cases {
            let n = title(input);
            assert_eq!(n.text, text, "{input:?}");
            assert_eq!(n.flags, flags(fl), "{input:?}");
            assert_eq!(n.featured, feat, "{input:?}");
        }
    }

    #[test]
    fn live_is_not_read_from_any_phrase_starting_with_the_word() {
        assert!(title("Song (Live For You)").flags.is_empty());
        assert_eq!(title("Song (Live For You)").text, "song live for you");
        assert_eq!(title("Song (BBC Live)").flags, flags(&[Flag::Live]));
        assert_eq!(title("Song (Live 1977)").flags, flags(&[Flag::Live]));
        assert!(title("Alive").flags.is_empty());
    }

    #[test]
    fn several_flags_in_one_bracket() {
        let n = title("Song (Live / Remastered 2011)");
        assert_eq!(n.text, "song");
        assert_eq!(n.flags, flags(&[Flag::Live, Flag::Remaster]));
    }

    #[test]
    fn an_unclosed_bracket_is_left_as_text() {
        assert_eq!(title("Song (Live").text, "song live");
    }

    #[test]
    fn artist_table() {
        let cases: [(&str, &str, &[&str]); 7] = [
            ("Artist A", "artist a", &[]),
            ("Artist A [feat. Artist B]", "artist a", &["artist b"]),
            ("Artist A (UK)", "artist a uk", &[]),
            ("Artist A feat. Artist B", "artist a", &["artist b"]),
            (
                "Artist A (feat. Artist B, Artist C)",
                "artist a",
                &["artist b", "artist c"],
            ),
            ("Artist A Featuring Artist B", "artist a", &["artist b"]),
            ("Simon & Garfunkel", "simon and garfunkel", &[]),
        ];
        for (input, text, feat) in cases {
            let n = artist(input);
            assert_eq!(n.text, text, "{input:?}");
            assert_eq!(n.featured, feat, "{input:?}");
            assert!(n.flags.is_empty());
        }
    }

    #[test]
    fn variant_mismatch_counts_only_live_acoustic_and_demo() {
        let studio = title("Song");
        assert!(title("Song (Live)").variant_mismatch(&studio));
        assert!(studio.variant_mismatch(&title("Song (Acoustic)")));
        assert!(title("Song [Demo]").variant_mismatch(&studio));
        assert!(!title("Song (Remastered)").variant_mismatch(&studio));
        assert!(!title("Song (Explicit)").variant_mismatch(&studio));
        assert!(!title("Song (Live)").variant_mismatch(&title("Song - Live")));
    }
}
