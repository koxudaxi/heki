//! Isolated Python parser facade over `ruff_python_parser` / `ruff_python_ast`.
//!
//! Architecture Invariant (ADR-009):
//! No crate outside `heki_ast` may depend directly on `ruff_python_*`.
//! All downstream crates consume `heki_ast`'s stable span, AST, and PEP 810
//! lazy-import abstractions.

mod source;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use source::{
    SourceEncoding, SourceError, decode_source, encode_source, read_source, source_encoding,
    write_source,
};

pub use ruff_python_ast as raw_ast;
pub use ruff_text_size::{Ranged, TextRange, TextSize};

/// Fast byte-offset -> 1-based `(line, column)` table for a Python source file.
///
/// Lines end at `\n`, `\r\n`, or a lone `\r`, as Python's tokenizer (universal newlines) and
/// the LSP specification count them.
#[derive(Debug, Clone)]
pub struct LineIndex {
    line_starts: Vec<u32>,
}

/// Whether the byte at `i` of `source` ends a line: a `\n`, or a `\r` no `\n` follows (the
/// `\r` of `\r\n` stays on its line). The next line starts at `i + 1`.
pub fn ends_line(source: &[u8], i: usize) -> bool {
    match source.get(i) {
        Some(b'\n') => true,
        Some(b'\r') => source.get(i + 1) != Some(&b'\n'),
        _ => false,
    }
}

/// The line break `source` uses (that of its first line; `\n` without one), for lines an
/// edit inserts.
pub fn line_break(source: &str) -> &'static str {
    let bytes = source.as_bytes();
    match (0..bytes.len()).find(|&i| ends_line(bytes, i)) {
        Some(i) if bytes[i] == b'\r' => "\r",
        Some(i) if i > 0 && bytes[i - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
}

impl LineIndex {
    pub fn from_source(source: &str) -> Self {
        let bytes = source.as_bytes();
        let mut line_starts = vec![0u32];
        for idx in 0..bytes.len() {
            if ends_line(bytes, idx) {
                line_starts.push((idx + 1) as u32);
            }
        }
        Self { line_starts }
    }

    /// Byte offset where 1-based `line` starts (`None` past the last line).
    pub fn line_start(&self, line: u32) -> Option<u32> {
        self.line_starts
            .get(usize::try_from(line.checked_sub(1)?).ok()?)
            .copied()
    }

    /// The text of 1-based `line` of `source` without its line break.
    pub fn line_text<'s>(&self, source: &'s str, line: u32) -> Option<&'s str> {
        let start = self.line_start(line)? as usize;
        let end = self
            .line_start(line + 1)
            .map_or(source.len(), |next| next as usize);
        source
            .get(start..end)
            .map(|l| l.trim_end_matches(['\n', '\r']))
    }

    /// 1-based line number for a byte offset.
    pub fn line(&self, offset: u32) -> u32 {
        match self.line_starts.binary_search(&offset) {
            Ok(idx) => (idx + 1) as u32,
            Err(idx) => idx as u32,
        }
    }

    /// 1-based `(line, column)` pair (byte-based column) for a byte offset.
    pub fn line_col(&self, offset: u32) -> (u32, u32) {
        let line = self.line(offset);
        let start = self.line_starts[(line.saturating_sub(1)) as usize];
        (line, offset.saturating_sub(start) + 1)
    }
}

/// Byte-offset span in the original Python source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl From<TextRange> for Span {
    fn from(range: TextRange) -> Self {
        Self {
            start: range.start().to_u32(),
            end: range.end().to_u32(),
        }
    }
}

#[derive(Debug, Clone, Error)]
#[error("python syntax error at {span:?}: {message}")]
pub struct ParseError {
    pub span: Span,
    pub message: String,
}

/// Summary of an import statement extracted from the parsed AST,
/// including Python 3.15 PEP 810 (`lazy import` / `lazy from ... import ...`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportRecord {
    pub module: Option<String>,
    pub names: Vec<(String, Option<String>)>,
    pub level: u32,
    pub is_from: bool,
    pub is_lazy: bool,
    pub span: Span,
}

pub struct ParsedModule {
    parsed: ruff_python_parser::Parsed<ruff_python_ast::ModModule>,
}

impl ParsedModule {
    pub fn suite(&self) -> &[ruff_python_ast::Stmt] {
        &self.parsed.syntax().body
    }

    /// Return all syntax errors encountered during recovery parsing.
    pub fn errors(&self) -> Vec<ParseError> {
        self.parsed
            .errors()
            .iter()
            .map(|err| ParseError {
                span: Span::from(err.location),
                message: err.error.to_string(),
            })
            .collect()
    }

    /// Byte spans of every `#` comment token. Taken from the tokenizer, so a `#` inside a
    /// string literal or f-string never appears here (inline suppression scanning).
    pub fn comment_spans(&self) -> Vec<Span> {
        self.parsed
            .tokens()
            .iter()
            .filter(|t| t.kind() == ruff_python_ast::token::TokenKind::Comment)
            .map(|t| Span::from(t.range()))
            .collect()
    }

    /// Whether a `#` comment inserted at byte `offset` of `source`, the end of a line before its
    /// line break, is a comment of that line and changes nothing else: the offset is inside no
    /// token (a string or f-string spanning lines would take the text in), and the line does not
    /// end with a `\` continuation, which the comment would swallow, so that the next line
    /// stops continuing this one.
    pub fn takes_comment_at(&self, source: &str, offset: u32) -> bool {
        let token_around = |starts_before: &dyn Fn(u32) -> bool, at: u32| {
            self.parsed
                .tokens()
                .iter()
                .any(|t| starts_before(t.start().to_u32()) && at < t.end().to_u32())
        };
        // A backslash inside a token (a comment, a string) continues nothing.
        let continued = offset.checked_sub(1).is_some_and(|before| {
            source.as_bytes().get(before as usize) == Some(&b'\\')
                && !token_around(&|start| start <= before, before)
        });
        !continued && !token_around(&|start| start < offset, offset)
    }

    /// Extract top-level import records (used to verify PEP 810 `is_lazy` support).
    pub fn top_level_imports(&self) -> Vec<ImportRecord> {
        let mut out = Vec::new();
        for stmt in self.suite() {
            match stmt {
                ruff_python_ast::Stmt::Import(imp) => {
                    out.push(ImportRecord {
                        module: None,
                        names: imp
                            .names
                            .iter()
                            .map(|a| (a.name.to_string(), a.asname.as_ref().map(|n| n.to_string())))
                            .collect(),
                        level: 0,
                        is_from: false,
                        is_lazy: imp.is_lazy,
                        span: Span::from(imp.range),
                    });
                }
                ruff_python_ast::Stmt::ImportFrom(imp) => {
                    out.push(ImportRecord {
                        module: imp.module.as_ref().map(|m| m.to_string()),
                        names: imp
                            .names
                            .iter()
                            .map(|a| (a.name.to_string(), a.asname.as_ref().map(|n| n.to_string())))
                            .collect(),
                        level: imp.level,
                        is_from: true,
                        is_lazy: imp.is_lazy,
                        span: Span::from(imp.range),
                    });
                }
                _ => {}
            }
        }
        out
    }
}

/// The replacement-field expressions of an f-string or a t-string, in evaluation order: each
/// field's expression, then the fields of its format specification (`f"{x:{width}}"`), which
/// may nest again. Any other expression has none.
pub fn interpolated_expressions(expr: &ruff_python_ast::Expr) -> Vec<&ruff_python_ast::Expr> {
    use ruff_python_ast::{Expr, FStringPartRef, InterpolatedStringElements};

    fn push<'a>(elements: &'a InterpolatedStringElements, out: &mut Vec<&'a Expr>) {
        for field in elements.interpolations() {
            out.push(&field.expression);
            if let Some(spec) = &field.format_spec {
                push(&spec.elements, out);
            }
        }
    }

    let mut out = Vec::new();
    if let Some(fs) = expr.as_f_string_expr() {
        for part in fs.value.iter() {
            if let FStringPartRef::FString(f) = part {
                push(&f.elements, &mut out);
            }
        }
    }
    if let Some(ts) = expr.as_t_string_expr() {
        for t in ts.value.iter() {
            push(&t.elements, &mut out);
        }
    }
    out
}

/// The values a template string keeps (`Template.values`): its top-level replacement fields,
/// in source order (a format specification's fields become text).
pub fn template_values(ts: &ruff_python_ast::ExprTString) -> Vec<&ruff_python_ast::Expr> {
    ts.value
        .iter()
        .flat_map(|t| t.elements.interpolations().map(|i| &*i.expression))
        .collect()
}

/// Parse Python source text with Python 3.15 syntax enabled (including PEP 810 `lazy import`).
pub fn parse_module(source: &str) -> Result<ParsedModule, Vec<ParseError>> {
    let options = ruff_python_parser::ParseOptions::from(ruff_python_ast::PySourceType::Python)
        .with_target_version(ruff_python_ast::PythonVersion::PY315);
    let parsed = ruff_python_parser::parse_unchecked(source, options)
        .try_into_module()
        .expect("Python source type always parses into ModModule");
    let wrapper = ParsedModule { parsed };
    let errs = wrapper.errors();
    if errs.is_empty() {
        Ok(wrapper)
    } else {
        Err(errs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_and_pep810_lazy_imports() {
        let src = "\
import os
lazy import json
from pathlib import Path
lazy from collections.abc import Iterable as Iter
import os.path as osp

def nested():
    import sys
";
        let parsed = parse_module(src).expect("Python 3.15 PEP 810 lazy import must parse cleanly");
        let imports = parsed.top_level_imports();
        // Only top-level statements: the import inside `nested` is not one.
        assert_eq!(imports.len(), 5);
        assert!(!imports[0].is_lazy);
        assert!(imports[1].is_lazy);
        assert_eq!(imports[1].names[0].0, "json");
        assert!(!imports[2].is_lazy);
        assert!(imports[3].is_lazy);
        assert_eq!(imports[3].module.as_deref(), Some("collections.abc"));
        assert_eq!(
            imports[3].names[0],
            ("Iterable".to_string(), Some("Iter".to_string()))
        );
        assert_eq!(
            imports[4].names[0],
            ("os.path".to_string(), Some("osp".to_string()))
        );
    }

    #[test]
    fn lines_end_at_lf_crlf_and_lone_cr() {
        let src = "a\nb\r\nc\rd";
        let index = LineIndex::from_source(src);
        let at = |c: char| index.line_col(u32::try_from(src.find(c).unwrap()).unwrap());
        assert_eq!(
            [at('a'), at('b'), at('c'), at('d')],
            [(1, 1), (2, 1), (3, 1), (4, 1)]
        );
        let texts: Vec<Option<&str>> = (0..=5).map(|l| index.line_text(src, l)).collect();
        assert_eq!(
            texts,
            [None, Some("a"), Some("b"), Some("c"), Some("d"), None]
        );
        // A final line break starts an empty last line.
        assert_eq!(LineIndex::from_source("a\r").line_text("a\r", 2), Some(""));
        assert_eq!(
            ["a\nb\r", "a\r\nb", "a\rb\n", "a"].map(line_break),
            ["\n", "\r\n", "\r", "\n"]
        );
    }

    #[test]
    fn comments_fit_at_line_ends_outside_strings_and_continuations() {
        let src = "a = f(1)\nb = 1 + \\\n    2\nc = \"\"\"x\ny\"\"\"\nd = 1  # see \\\ne = (1,\n     2)\n";
        let parsed = parse_module(src).unwrap();
        let end_of = |line: &str| u32::try_from(src.find(line).unwrap() + line.len()).unwrap();
        let fits = |line: &str| parsed.takes_comment_at(src, end_of(line));
        assert!(fits("a = f(1)"));
        assert!(
            !fits("b = 1 + \\"),
            "a comment would swallow the continuation"
        );
        assert!(!fits("c = \"\"\"x"), "the line ends inside a string");
        assert!(
            fits("d = 1  # see \\"),
            "a backslash in a comment continues nothing"
        );
        assert!(fits("e = (1,"), "comments are fine inside brackets");
        assert!(fits("y\"\"\""));
    }

    #[test]
    fn comment_spans_skip_hashes_inside_strings() {
        let src = "x = \"# not a comment\"  # real: one\ny = f\"{'#'}\"\n# two\n";
        let parsed = parse_module(src).unwrap();
        let texts: Vec<&str> = parsed
            .comment_spans()
            .iter()
            .map(|s| &src[s.start as usize..s.end as usize])
            .collect();
        assert_eq!(texts, vec!["# real: one", "# two"]);
    }
}
