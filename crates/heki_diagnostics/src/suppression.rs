//! Inline suppressions (`# heki: ignore[...]`, `# noqa: ...`): the reports they suppress, and the
//! edit that takes codes out of one.

use crate::TextEdit;
use heki_ast::{LineIndex, Span};
use std::collections::BTreeMap;
use std::ops::Range;

/// How a suppression is spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressionForm {
    /// `# heki: ignore[...]`.
    Heki,
    /// `# noqa: ...`, which Ruff and flake8 read too: the codes that are not heki's are theirs.
    Noqa,
}

/// One suppression of a comment. A comment may hold several (`# type: ignore  # noqa: EFF101`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suppression {
    pub form: SuppressionForm,
    /// The 1-based line it applies to.
    pub line: u32,
    /// From its `#` to the end of its text.
    pub span: Span,
    /// The rules it names, as written; empty for the bare forms, which suppress every rule.
    pub codes: Vec<String>,
    /// The comment that holds it.
    comment: Span,
    /// From the first of `codes` to the last in the source.
    list: Span,
}

/// Parse inline suppressions (`# heki: ignore`, `# heki: ignore[EFF101, async-blocking-call]`,
/// and `# noqa` / `# noqa: EFF101` compatibility) from one comment directive.
/// `Some(empty)` suppresses every rule: only the bare forms do. A bracketed list without its
/// closing `]` or without a rule (`# heki: ignore[`, `# heki: ignore[]`) suppresses nothing.
pub fn parse_inline_ignores(comment: &str) -> Option<Vec<String>> {
    parse_directive(comment).map(|(_, codes, _)| codes.into_iter().map(str::to_owned).collect())
}

/// The form, the codes, and the byte range of the code list in `part` of one directive.
fn parse_directive(part: &str) -> Option<(SuppressionForm, Vec<&str>, Range<usize>)> {
    let offset = |s: &str| s.as_ptr() as usize - part.as_ptr() as usize;
    let trimmed = part.trim_start_matches('#').trim();
    if let Some(rest) = trimmed.strip_prefix("heki:") {
        let after = rest.trim().strip_prefix("ignore")?.trim();
        if after.is_empty() {
            return Some((SuppressionForm::Heki, Vec::new(), 0..0));
        }
        let (inner, _) = after.strip_prefix('[')?.split_once(']')?;
        let items = codes_in(inner, |c| c == ',');
        let list = offset(items.first()?)..offset(items.last()?) + items.last()?.len();
        return Some((SuppressionForm::Heki, items, list));
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower == "noqa" {
        return Some((SuppressionForm::Noqa, Vec::new(), 0..0));
    }
    if lower.starts_with("noqa:") {
        let items = codes_in(&trimmed["noqa:".len()..], |c| c == ',' || c.is_whitespace());
        let list = offset(items.first()?)..offset(items.last()?) + items.last()?.len();
        return Some((SuppressionForm::Noqa, items, list));
    }
    None
}

/// The codes of a list separated by `sep`: slices of `list`.
fn codes_in(list: &str, sep: impl Fn(char) -> bool) -> Vec<&str> {
    list.split(sep)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Whether a code of a suppression names the rule (`code` or `slug`, case-insensitive). `*`
/// names every rule.
fn names(item: &str, code: &str, slug: &str) -> bool {
    item == "*" || item.eq_ignore_ascii_case(code) || item.eq_ignore_ascii_case(slug)
}

/// The suppressions of one file, by line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InlineIgnores {
    suppressions: Vec<Suppression>,
    by_line: BTreeMap<u32, Vec<usize>>,
}

impl InlineIgnores {
    /// Build from the file's comment token spans (`heki_ast::ParsedModule::comment_spans`), so
    /// a `#` inside a string literal is never read as a comment. One comment may carry several
    /// directives (`# type: ignore  # heki: ignore[EFF101]`).
    pub fn from_comments(source: &str, comments: &[Span]) -> Self {
        let index = LineIndex::from_source(source);
        let mut out = Self::default();
        for &comment in comments {
            let Some(text) = source.get(comment.start as usize..comment.end as usize) else {
                continue;
            };
            let line = index.line(comment.start);
            let hashes: Vec<usize> = text.match_indices('#').map(|(i, _)| i).collect();
            for (n, &hash) in hashes.iter().enumerate() {
                let part = &text[hash + 1..hashes.get(n + 1).copied().unwrap_or(text.len())];
                let Some((form, codes, list)) = parse_directive(part) else {
                    continue;
                };
                let at = |i: usize| comment.start + u32::try_from(hash + 1 + i).unwrap_or(0);
                out.by_line
                    .entry(line)
                    .or_default()
                    .push(out.suppressions.len());
                out.suppressions.push(Suppression {
                    form,
                    line,
                    span: Span {
                        start: at(0) - 1,
                        end: at(part.trim_end().len()),
                    },
                    codes: codes.into_iter().map(str::to_owned).collect(),
                    comment,
                    list: Span {
                        start: at(list.start),
                        end: at(list.end),
                    },
                });
            }
        }
        out
    }

    pub fn suppressions(&self) -> &[Suppression] {
        &self.suppressions
    }

    /// The suppressions (indices into [`Self::suppressions`]) on `line` that suppress the rule
    /// (`code` or `slug`, case-insensitive).
    pub fn suppressing<'a>(
        &'a self,
        line: u32,
        code: &'a str,
        slug: &'a str,
    ) -> impl Iterator<Item = usize> + 'a {
        self.by_line
            .get(&line)
            .into_iter()
            .flatten()
            .copied()
            .filter(move |&i| {
                let codes = &self.suppressions[i].codes;
                codes.is_empty() || codes.iter().any(|item| names(item, code, slug))
            })
    }

    /// True when `line` suppresses the rule (`code` or `slug`, case-insensitive).
    pub fn is_suppressed(&self, line: u32, code: &str, slug: &str) -> bool {
        self.suppressing(line, code, slug).next().is_some()
    }
}

impl Suppression {
    /// The edit that leaves the codes at `unused` (indices into `codes`) out: the list without
    /// them, or, when none is left, the whole suppression with the blanks before it, and its line
    /// when the comment is alone on it.
    pub fn without(&self, source: &str, unused: &[usize]) -> TextEdit {
        let kept: Vec<&str> = (0..self.codes.len())
            .filter(|i| !unused.contains(i))
            .map(|i| self.codes[i].as_str())
            .collect();
        if !kept.is_empty() {
            return TextEdit {
                span: self.list,
                replacement: kept.join(", "),
            };
        }
        let bytes = source.as_bytes();
        let blanks_before = |mut at: usize, floor: usize| {
            while at > floor && matches!(bytes[at - 1], b' ' | b'\t') {
                at -= 1;
            }
            at
        };
        let (comment, own) = (self.comment, self.span);
        let (start, end) = (own.start as usize, own.end as usize);
        let rest = source.get(end..comment.end as usize).unwrap_or_default();
        let removed = if own.start > comment.start {
            blanks_before(start, comment.start as usize)..end
        } else if let Some(next) = rest.find('#') {
            // Another directive or note follows in the comment, and keeps its place.
            start..end + next
        } else {
            let line_start = (0..start)
                .rev()
                .find(|&i| heki_ast::ends_line(bytes, i))
                .map_or(0, |i| i + 1);
            let from = blanks_before(start, line_start);
            let comment_end = comment.end as usize;
            if from == line_start {
                let line_end = (comment_end..source.len())
                    .find(|&i| heki_ast::ends_line(bytes, i))
                    .map_or(source.len(), |i| i + 1);
                line_start..line_end
            } else {
                from..comment_end
            }
        };
        let span = |r: Range<usize>| Span {
            start: u32::try_from(r.start).unwrap_or(own.start),
            end: u32::try_from(r.end).unwrap_or(own.end),
        };
        TextEdit {
            span: span(removed),
            replacement: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The comment spans of `src` as the tokenizer gives them, for sources whose `#` all start
    /// comments.
    fn comments(src: &str) -> Vec<Span> {
        let mut out = Vec::new();
        let mut offset = 0;
        for line in src.split_inclusive('\n') {
            if let Some(hash) = line.find('#') {
                let end = line.trim_end_matches(['\n', '\r']).len();
                out.push(Span {
                    start: u32::try_from(offset + hash).unwrap(),
                    end: u32::try_from(offset + end).unwrap(),
                });
            }
            offset += line.len();
        }
        out
    }

    #[test]
    fn leaving_codes_out_keeps_the_others_and_the_rest_of_the_comment() {
        for (src, unused, fixed) in [
            ("x = 1  # heki: ignore[EFF101]\n", &[0][..], "x = 1\n"),
            (
                "x = 1  # heki: ignore[EFF101 , MUT201]\n",
                &[0],
                "x = 1  # heki: ignore[MUT201]\n",
            ),
            (
                "x = 1  # noqa: EFF101, F401\n",
                &[0],
                "x = 1  # noqa: F401\n",
            ),
            ("x = 1  # NOQA:EFF101 F401\n", &[0], "x = 1  # NOQA:F401\n"),
            (
                "x = 1  # type: ignore  # heki: ignore[EFF101]\n",
                &[0],
                "x = 1  # type: ignore\n",
            ),
            (
                "x = 1  # heki: ignore[EFF101]  # type: ignore\n",
                &[0],
                "x = 1  # type: ignore\n",
            ),
            (
                "x = 1  # heki: ignore[EFF101] slow on purpose\n",
                &[0],
                "x = 1\n",
            ),
            ("a = 1\n    # noqa: EFF101\nb = 2\n", &[0], "a = 1\nb = 2\n"),
            ("x = 1  # heki: ignore[EFF101]", &[0], "x = 1"),
            ("x = 1 # heki: ignore[EFF101]\r\n", &[0], "x = 1\r\n"),
        ] {
            let ignores = InlineIgnores::from_comments(src, &comments(src));
            assert_eq!(ignores.suppressions().len(), 1, "{src:?}");
            let edit = ignores.suppressions()[0].without(src, unused);
            let mut out = src.to_owned();
            out.replace_range(
                edit.span.start as usize..edit.span.end as usize,
                &edit.replacement,
            );
            assert_eq!(out, fixed, "{src:?}");
        }
    }

    #[test]
    fn a_comment_may_hold_suppressions_of_both_forms() {
        let src = "x = 1  # heki: ignore[EFF101]  # noqa: F401, MUT201\n";
        let ignores = InlineIgnores::from_comments(src, &comments(src));
        let forms: Vec<_> = ignores
            .suppressions()
            .iter()
            .map(|s| (s.form, s.codes.join(",")))
            .collect();
        assert_eq!(
            forms,
            [
                (SuppressionForm::Heki, "EFF101".to_owned()),
                (SuppressionForm::Noqa, "F401,MUT201".to_owned())
            ]
        );
        let at: Vec<usize> = ignores
            .suppressing(1, "MUT201", "write-owner-violation")
            .collect();
        assert_eq!(at, [1]);
    }
}
