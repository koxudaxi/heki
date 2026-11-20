//! Isolated Python parser facade over `ruff_python_parser` / `ruff_python_ast`.
//!
//! Architecture Invariant (ADR-009):
//! No crate outside `heki_ast` may depend directly on `ruff_python_*`.
//! All downstream crates consume `heki_ast`'s stable span, AST, and PEP 810
//! lazy-import abstractions.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use ruff_python_ast as raw_ast;
pub use ruff_text_size::{Ranged, TextRange, TextSize};

/// Fast byte-offset -> 1-based `(line, column)` table for a Python source file.
#[derive(Debug, Clone)]
pub struct LineIndex {
    line_starts: Vec<u32>,
}

impl LineIndex {
    pub fn from_source(source: &str) -> Self {
        let mut line_starts = vec![0u32];
        for (idx, b) in source.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push((idx + 1) as u32);
            }
        }
        Self { line_starts }
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

/// Source-level parse error reported by the Python parser.
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

/// Parsed Python module wrapper.
pub struct ParsedModule {
    parsed: ruff_python_parser::Parsed<ruff_python_ast::ModModule>,
}

impl ParsedModule {
    /// Access the underlying module body (for `heki_semantic` / `heki_ir` lowering).
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
    match expr {
        Expr::FString(fs) => {
            for part in fs.value.iter() {
                if let FStringPartRef::FString(f) = part {
                    push(&f.elements, &mut out);
                }
            }
        }
        Expr::TString(ts) => {
            for t in ts.value.iter() {
                push(&t.elements, &mut out);
            }
        }
        _ => {}
    }
    out
}

/// The top-level replacement-field expressions of an f-string or a t-string (without the
/// fields of their format specifications), in source order.
pub fn replacement_fields(expr: &ruff_python_ast::Expr) -> Vec<&ruff_python_ast::Expr> {
    use ruff_python_ast::{Expr, FStringPartRef};

    let mut out = Vec::new();
    match expr {
        Expr::FString(fs) => {
            for part in fs.value.iter() {
                if let FStringPartRef::FString(f) = part {
                    out.extend(f.elements.interpolations().map(|i| &*i.expression));
                }
            }
        }
        Expr::TString(ts) => {
            for t in ts.value.iter() {
                out.extend(t.elements.interpolations().map(|i| &*i.expression));
            }
        }
        _ => {}
    }
    out
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
