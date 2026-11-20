//! Source-text helpers for minimal byte-span edits (formatting and comments are preserved:
//! only the rewritten expression or statement changes).

/// `source[start..end]`, if both are character boundaries.
pub(crate) fn slice(source: &str, start: u32, end: u32) -> Option<&str> {
    source.get(start as usize..end as usize)
}

/// Offset of the first byte of the line containing `pos` (any byte offset, also one inside a
/// multi-byte character: a line break byte is never part of one). Lines end as
/// `heki_ast::ends_line` says: at `\n`, `\r\n`, or a lone `\r`.
pub(crate) fn line_start(source: &str, pos: u32) -> u32 {
    let pos = (pos as usize).min(source.len());
    (0..pos)
        .rev()
        .find(|&i| heki_ast::ends_line(source.as_bytes(), i))
        .map_or(0, |i| i as u32 + 1)
}

/// Offset of the line break ending the line containing `pos` (the `\n` of `\r\n`; the end of
/// the source without one): the next line starts after it.
pub(crate) fn line_end(source: &str, pos: u32) -> u32 {
    let pos = (pos as usize).min(source.len());
    (pos..source.len())
        .find(|&i| heki_ast::ends_line(source.as_bytes(), i))
        .map_or(source.len() as u32, |i| i as u32)
}

/// A simple statement can start at `start`: only indentation, a `;`, or a one-line compound
/// header ending in `:` precede it on its line. (Not a character boundary: `false`.)
pub(crate) fn starts_statement(source: &str, start: u32) -> bool {
    slice(source, line_start(source, start), start).is_some_and(|before| {
        let t = before.trim_end();
        t.is_empty() || t.ends_with(';') || t.ends_with(':')
    })
}

/// A simple statement ends at `end`: only whitespace, then the end of the line, a `;`, or a
/// comment follow it. (Not a character boundary: `false`.)
pub(crate) fn ends_statement(source: &str, end: u32) -> bool {
    slice(source, end, line_end(source, end)).is_some_and(|after| {
        let t = after.trim_start();
        t.is_empty() || t.starts_with('#') || t.starts_with(';')
    })
}

pub(crate) fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
        && chars.all(|c| c == '_' || c.is_alphanumeric())
}

/// `text` without any whitespace (layout-insensitive comparison of two source fragments).
pub(crate) fn squash(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Strip `prefix` (after optional whitespace) from `text`.
pub(crate) fn strip_token<'t>(text: &'t str, prefix: &str) -> Option<&'t str> {
    text.trim_start().strip_prefix(prefix)
}

/// `text` without the blanks around it, but still ending with its own line break when a
/// comment or a `\` continuation ends its last line: a bracket an edit puts after it must not
/// land inside the comment or the continued line.
pub(crate) fn trim_fragment(text: &str) -> &str {
    let text = text.trim_start();
    let body = text.trim_end();
    let last_line = body.rsplit(['\n', '\r']).next().unwrap_or(body);
    if body.ends_with('\\') || last_line.contains('#') {
        text
    } else {
        body
    }
}

/// [`trim_fragment`] without one trailing `,` (an element of a display, an argument).
pub(crate) fn trim_element(text: &str) -> &str {
    let e = trim_fragment(text);
    e.strip_suffix(',').map_or(e, str::trim_end)
}

/// Only blanks, comments, line continuations, and closing parentheses separate `pos` from a
/// `:`: `pos` ends the expression of a compound statement's header (`for x in (xs):`).
pub(crate) fn header_colon_follows(source: &str, pos: u32) -> bool {
    let Some(mut rest) = source.get(pos as usize..) else {
        return false;
    };
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '\\' || c == ')');
        match rest.strip_prefix('#') {
            Some(comment) => {
                rest = comment
                    .split_once(['\n', '\r'])
                    .map_or("", |(_, after)| after);
            }
            None => return rest.starts_with(':'),
        }
    }
}

/// `name = ` exactly: the text between an assignment target and its value.
pub(crate) fn is_plain_assign(between: &str) -> bool {
    between.trim() == "="
}

/// Leading whitespace of the line starting at `line_start`.
pub(crate) fn indentation(source: &str, line_start: u32) -> &str {
    let rest = &source[line_start as usize..];
    let n = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    &rest[..n]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statement_boundaries() {
        let src = "def f():\n    x = x + [1]  # c\n    if c: y = 2; z = 3\n";
        let x = src.find("x =").unwrap() as u32;
        assert!(starts_statement(src, x));
        let end = src.find("  # c").unwrap() as u32;
        assert!(ends_statement(src, end));
        let y = src.find("y =").unwrap() as u32;
        assert!(starts_statement(src, y));
        let z = src.find("z =").unwrap() as u32;
        assert!(starts_statement(src, z));
        let one = src.find("1]").unwrap() as u32;
        assert!(!starts_statement(src, one));
        assert!(!ends_statement(src, one));
        assert_eq!(indentation(src, line_start(src, x)), "    ");
        // Lone CR and CRLF end lines too; the next line starts after the line end.
        let mixed = "a = 1\r    b = 2\r\n  c = 3";
        let at = |t: &str| mixed.find(t).unwrap() as u32;
        assert_eq!(line_start(mixed, at("b =")), at("    b"));
        assert_eq!(line_start(mixed, at("c =")), at("  c"));
        assert_eq!(line_end(mixed, 0), at("\r    b"));
        assert_eq!(line_end(mixed, at("b =")) + 1, at("  c"));
        assert!(header_colon_follows("for x in (xs  # c\r    ) :", 12));
        assert_eq!(trim_fragment("v  # c\r  "), "v  # c\r  ");
        assert!(is_identifier("acc_1") && !is_identifier("1x") && !is_identifier("a.b"));
        assert_eq!(squash("{ k : v ,\n }"), "{k:v,}");
        let cafe = "import os as caf\u{e9}\nx = 1\n";
        let inside_e = cafe.find('\u{e9}').unwrap() as u32 + 1;
        assert_eq!(
            (line_start(cafe, inside_e), line_end(cafe, inside_e)),
            (0, 18)
        );
        assert!(header_colon_follows("for x in (xs  # c\n    ) :", 12));
        assert!(!header_colon_follows("[x for x in xs if x]", 14));
        assert!(!header_colon_follows("[x for x in xs  # c\n]", 14));
        assert!(!header_colon_follows("x", 5));
        assert_eq!(trim_element(" v, "), "v");
        assert_eq!(trim_element("\n  v  # c\n    "), "v  # c\n    ");
        assert_eq!(trim_element("v \\\n"), "v \\\n");
        assert_eq!(trim_element("v,  # c\n"), "v,  # c\n");
    }
}
