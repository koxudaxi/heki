//! Source-text helpers for minimal byte-span edits (formatting and comments are preserved:
//! only the rewritten expression or statement changes).

/// `source[start..end]`, if both are character boundaries.
pub(crate) fn slice(source: &str, start: u32, end: u32) -> Option<&str> {
    source.get(start as usize..end as usize)
}

/// Offset of the first byte of the line containing `pos`.
pub(crate) fn line_start(source: &str, pos: u32) -> u32 {
    let pos = (pos as usize).min(source.len());
    source[..pos].rfind('\n').map_or(0, |i| i as u32 + 1)
}

/// Offset of the `\n` ending the line containing `pos` (or the end of the source).
pub(crate) fn line_end(source: &str, pos: u32) -> u32 {
    let pos = (pos as usize).min(source.len());
    source[pos..]
        .find('\n')
        .map_or(source.len() as u32, |i| (pos + i) as u32)
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
        assert!(is_identifier("acc_1") && !is_identifier("1x") && !is_identifier("a.b"));
        assert_eq!(squash("{ k : v ,\n }"), "{k:v,}");
    }
}
