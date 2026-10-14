//! Values pasted or piped to `sss`: the one rule every sign-in reads them
//! by.

use std::io::{self, BufRead};

use anyhow::{Context, Result};

use crate::error::{Kind, WithKind};

/// The trimmed, non-empty `lines` joined with no separator: a value copied
/// across several lines, whole.
pub fn join_pasted<'a>(lines: impl IntoIterator<Item = &'a str>) -> String {
    lines
        .into_iter()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect()
}

/// `result` of reading pasted or piped text: bytes that are not UTF-8 are
/// a [`Kind::Input`] error; any other failure gets `context`.
pub fn pasted_text<T>(result: io::Result<T>, context: impl FnOnce() -> String) -> Result<T> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::InvalidData => Err(e).with_kind(Kind::Input, || {
            format!("{}: not valid UTF-8 text", context())
        }),
        other => other.with_context(context),
    }
}

/// A value pasted or piped on `input`: its lines up to the first line that
/// is empty after trimming, or EOF, joined by [`join_pasted`].
pub fn read_pasted(input: &mut dyn BufRead) -> Result<String> {
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        let read = pasted_text(input.read_line(&mut line), || "reading the input".into())?;
        if read == 0 || line.trim().is_empty() {
            return Ok(join_pasted(lines.iter().map(String::as_str)));
        }
        lines.push(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_values_are_trimmed_and_joined_up_to_an_empty_line() {
        let mut input: &[u8] = b" ab \r\n\tcd\n \nef\n";
        assert_eq!(read_pasted(&mut input).unwrap(), "abcd");
        assert_eq!(input, b"ef\n");
        let mut input: &[u8] = b"one";
        assert_eq!(read_pasted(&mut input).unwrap(), "one");
        assert_eq!(join_pasted(["", " a", "b\r", "  "]), "ab");
    }

    #[test]
    fn input_that_is_not_utf8_is_an_input_error() {
        let mut input: &[u8] = b"tok\xe9n\n";
        let err = read_pasted(&mut input).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Input);
        assert_eq!(
            format!("{err:#}"),
            "reading the input: not valid UTF-8 text: stream did not contain valid UTF-8"
        );

        let other = pasted_text::<()>(Err(io::ErrorKind::NotFound.into()), || "reading f".into())
            .unwrap_err();
        assert_eq!(crate::error::kind(&other), Kind::Other);
        assert!(format!("{other:#}").starts_with("reading f: "), "{other:#}");
    }
}
