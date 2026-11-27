//! Python source files as text: decoded the way CPython decodes them before tokenizing
//! (PEP 3120 UTF-8 by default, a UTF-8 byte order mark, or the PEP 263 encoding declaration of
//! one of the first two lines), and encoded back in the same encoding when a fix rewrites them.

use std::io;
use std::path::Path;

/// A source encoding heki reads and writes. Other declared encodings are refused
/// ([`SourceError::Unsupported`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceEncoding {
    Utf8,
    Latin1,
    Ascii,
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("stream did not contain valid UTF-8")]
    InvalidUtf8,
    #[error("source declares encoding `{0}`, which heki does not read (UTF-8, Latin-1, ASCII)")]
    Unsupported(String),
    #[error("encoding problem: `{0}` with a UTF-8 byte order mark")]
    BomMismatch(String),
    #[error("source declared as ASCII contains a non-ASCII byte")]
    InvalidAscii,
    #[error("`{ch}` cannot be written in the source's declared {encoding:?} encoding")]
    Unencodable { ch: char, encoding: SourceEncoding },
}

impl From<SourceError> for io::Error {
    fn from(e: SourceError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, e)
    }
}

const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// The encoding name of a `coding[:=]` declaration in `line`, a comment line (CPython's
/// tokenizer: only whitespace before the `#`).
fn coding_declaration(line: &[u8]) -> Option<&[u8]> {
    let hash = line.iter().position(|&b| b == b'#')?;
    if !line[..hash]
        .iter()
        .all(|b| matches!(b, b' ' | b'\t' | b'\x0c'))
    {
        return None;
    }
    let comment = &line[hash..];
    (0..comment.len()).find_map(|i| {
        let rest = comment[i..].strip_prefix(b"coding")?;
        let rest = rest
            .strip_prefix(b":")
            .or_else(|| rest.strip_prefix(b"="))?;
        let start = rest.iter().position(|b| !matches!(b, b' ' | b'\t'))?;
        let rest = &rest[start..];
        let len = rest
            .iter()
            .position(|&b| !(b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')))
            .unwrap_or(rest.len());
        (len > 0).then(|| &rest[..len])
    })
}

/// A blank or comment-only line: only then can the second line declare the encoding.
fn blank_or_comment(line: &[u8]) -> bool {
    line.iter()
        .find(|b| !matches!(b, b' ' | b'\t' | b'\x0c'))
        .is_none_or(|b| matches!(b, b'#' | b'\r' | b'\n'))
}

/// The codec a declared name selects, after CPython's name normalization and codec aliases.
fn codec(name: &[u8]) -> Result<SourceEncoding, SourceError> {
    let raw = String::from_utf8_lossy(name).into_owned();
    // `codecs.lookup` folds case and collapses each run of other characters than letters,
    // digits, and `.` into one `_`.
    let mut normalized = String::new();
    for ch in raw.to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' {
            normalized.push(ch);
        } else if !normalized.ends_with('_') {
            normalized.push('_');
        }
    }
    let normalized = normalized.trim_matches('_');
    // The tokenizer's own shortcuts (`utf-8-sig` is UTF-8, `latin-1-...` is Latin-1).
    if normalized == "utf_8" || normalized.starts_with("utf_8_") {
        return Ok(SourceEncoding::Utf8);
    }
    for prefix in ["latin_1", "iso_8859_1", "iso_latin_1"] {
        if normalized == prefix || normalized.starts_with(&format!("{prefix}_")) {
            return Ok(SourceEncoding::Latin1);
        }
    }
    match normalized {
        "u8" | "utf" | "utf8" | "utf8_ucs2" | "utf8_ucs4" | "cp65001" => Ok(SourceEncoding::Utf8),
        "8859" | "cp819" | "csisolatin1" | "ibm819" | "iso8859" | "iso8859_1" | "iso_ir_100"
        | "l1" | "latin" | "latin1" => Ok(SourceEncoding::Latin1),
        "ascii" | "646" | "ansi_x3.4_1968" | "ansi_x3_4_1968" | "ansi_x3.4_1986" | "cp367"
        | "csascii" | "ibm367" | "iso646_us" | "iso_646.irv_1991" | "iso_ir_6" | "us"
        | "us_ascii" => Ok(SourceEncoding::Ascii),
        _ => Err(SourceError::Unsupported(raw)),
    }
}

/// The encoding of a source file's bytes (or of its text: the declaration is ASCII).
pub fn source_encoding(bytes: &[u8]) -> Result<SourceEncoding, SourceError> {
    let bom = bytes.starts_with(UTF8_BOM);
    let body = if bom { &bytes[UTF8_BOM.len()..] } else { bytes };
    let mut lines = body.split_inclusive(|&b| b == b'\n');
    let first = lines.next().unwrap_or_default();
    let declared = coding_declaration(first).or_else(|| {
        blank_or_comment(first)
            .then(|| lines.next().and_then(coding_declaration))
            .flatten()
    });
    let Some(name) = declared else {
        return Ok(SourceEncoding::Utf8);
    };
    let encoding = codec(name)?;
    if bom && encoding != SourceEncoding::Utf8 {
        return Err(SourceError::BomMismatch(
            String::from_utf8_lossy(name).into_owned(),
        ));
    }
    Ok(encoding)
}

/// Decode a Python source file as CPython does. A UTF-8 byte order mark stays in the text
/// (the parser skips it), so writing the text back keeps it.
pub fn decode_source(bytes: Vec<u8>) -> Result<String, SourceError> {
    match source_encoding(&bytes)? {
        SourceEncoding::Utf8 => String::from_utf8(bytes).map_err(|_| SourceError::InvalidUtf8),
        SourceEncoding::Ascii if !bytes.is_ascii() => Err(SourceError::InvalidAscii),
        SourceEncoding::Ascii => String::from_utf8(bytes).map_err(|_| SourceError::InvalidUtf8),
        SourceEncoding::Latin1 => Ok(bytes.into_iter().map(char::from).collect()),
    }
}

/// Encode source text in the encoding its own declaration names.
pub fn encode_source(text: &str) -> Result<Vec<u8>, SourceError> {
    encode_like(text, text)
}

/// Encode `text`, lines of the source text `source` or of an edit of it, in the encoding the
/// declaration of `source` names: the bytes those lines have in the file.
pub fn encode_like(source: &str, text: &str) -> Result<Vec<u8>, SourceError> {
    let encoding = source_encoding(source.as_bytes())?;
    let limit = match encoding {
        SourceEncoding::Utf8 => return Ok(text.as_bytes().to_vec()),
        SourceEncoding::Latin1 => 0xFF,
        SourceEncoding::Ascii => 0x7F,
    };
    text.chars()
        .map(|ch| {
            u8::try_from(u32::from(ch))
                .ok()
                .filter(|b| u32::from(*b) <= limit)
                .ok_or(SourceError::Unencodable { ch, encoding })
        })
        .collect()
}

/// Read and decode a Python source file.
pub fn read_source(path: &Path) -> io::Result<String> {
    Ok(decode_source(std::fs::read(path)?)?)
}

/// Encode and write a Python source file (nothing is written when the text cannot be encoded).
pub fn write_source(path: &Path, text: &str) -> io::Result<()> {
    std::fs::write(path, encode_source(text)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_follow_cpython_tokenizer_rules() {
        let enc = |src: &[u8]| source_encoding(src).ok();
        assert_eq!(enc(b"x = 1\n"), Some(SourceEncoding::Utf8));
        assert_eq!(enc(b"# coding: latin-1\n"), Some(SourceEncoding::Latin1));
        assert_eq!(
            enc(b"#!/usr/bin/env python\n# -*- coding: ISO-8859-1 -*-\n"),
            Some(SourceEncoding::Latin1)
        );
        assert_eq!(
            enc(b"# vim: set fileencoding=Latin_1 :\n"),
            Some(SourceEncoding::Latin1)
        );
        assert_eq!(enc(b"  \t# coding=utf8\n"), Some(SourceEncoding::Utf8));
        assert_eq!(enc(b"# coding: utf-8-sig\n"), Some(SourceEncoding::Utf8));
        assert_eq!(enc(b"# coding: US-ASCII\n"), Some(SourceEncoding::Ascii));
        assert_eq!(enc(b"\n# coding: l1\n"), Some(SourceEncoding::Latin1));
        // Only a comment line declares, and the second line only after a blank or comment one.
        assert_eq!(
            enc(b"y = 1  # coding: latin-1\n"),
            Some(SourceEncoding::Utf8)
        );
        assert_eq!(
            enc(b"x = 1\n# coding: latin-1\n"),
            Some(SourceEncoding::Utf8)
        );
        assert_eq!(
            enc(b"#\n#\n# coding: latin-1\n"),
            Some(SourceEncoding::Utf8)
        );
        assert_eq!(enc(b"# coding:\n"), Some(SourceEncoding::Utf8));
        assert!(matches!(
            source_encoding(b"# coding: cp1252\n"),
            Err(SourceError::Unsupported(n)) if n == "cp1252"
        ));
        assert!(matches!(
            source_encoding(b"\xEF\xBB\xBF# coding: latin-1\n"),
            Err(SourceError::BomMismatch(_))
        ));
        assert_eq!(
            enc(b"\xEF\xBB\xBF# coding: utf-8\n"),
            Some(SourceEncoding::Utf8)
        );
    }

    #[test]
    fn sources_round_trip_in_their_declared_encoding() {
        let latin1 = b"# coding: latin-1\n# caf\xE9\nx = '\xFF'\n".to_vec();
        let text = decode_source(latin1.clone()).unwrap();
        assert!(text.contains("caf\u{e9}") && text.contains('\u{FF}'));
        assert_eq!(encode_source(&text).unwrap(), latin1);
        assert!(matches!(
            encode_source(&format!("{text}y = '\u{20AC}'\n")),
            Err(SourceError::Unencodable { ch: '\u{20AC}', .. })
        ));

        let bom = b"\xEF\xBB\xBFx = '\xC3\xA9'\n".to_vec();
        assert_eq!(
            encode_source(&decode_source(bom.clone()).unwrap()).unwrap(),
            bom
        );
        assert!(matches!(
            decode_source(b"x = '\xE9'\n".to_vec()),
            Err(SourceError::InvalidUtf8)
        ));
        assert!(matches!(
            decode_source(b"# coding: ascii\nx = '\xE9'\n".to_vec()),
            Err(SourceError::InvalidAscii)
        ));
        let ascii = "# coding: ascii\nx = 1\n";
        assert_eq!(decode_source(ascii.into()).unwrap(), ascii);
        assert!(encode_source(&format!("{ascii}y = '\u{e9}'\n")).is_err());
        assert!(decode_source(b"# coding: cp1252\n".to_vec()).is_err());
    }
}
