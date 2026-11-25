//! Where an edit adds an import of a standard-library module (`import collections` for the
//! `deque` rewrite, `from collections.abc import Sequence` for a read-only annotation that
//! `heki recommend` suggests): in sorted position among the module's top-level standard-library
//! imports, so that an isort-formatted file stays formatted (Ruff `I001`).
//!
//! The leading statements of the module (a docstring, then imports only) are read as text:
//! blocks of import statements separated by blank lines, the `__future__` block first. The new
//! import goes into the first block of standard-library imports, in the order that block (or,
//! without a hint there, another block of the module) already follows: straight imports before
//! `from` imports (isort's default), or both sorted by module (`force-sort-within-sections`).
//! Without such a block it starts one before the first block of other imports, and in a module
//! without imports it follows the docstring and `__future__` imports, with the blank lines isort
//! puts after an import block. Code that runs before the new import is never more than
//! imports, so nothing can call the rewritten function before the new import binds its name.

use crate::edits::{line_end, line_start, slice};
use heki_ast::Span;
use heki_ir::{ConstLiteral, InstKind, IrFunction};

/// One top-level import statement of the leading import region.
#[derive(Debug)]
struct ImportStmt {
    /// Offset of its first line (comments directly above it included).
    start: u32,
    /// Offset of the line after its last line.
    end: u32,
    /// The module it imports from (`os.path`, `.`, `__future__`), lowercased for sorting.
    module: String,
    is_from: bool,
}

/// The edit inserting `import collections` ([`import_edit`]).
pub(crate) fn collections_import(source: &str, module_fn: &IrFunction) -> Option<(Span, String)> {
    let import = NewImport {
        module: "collections",
        is_from: false,
        statement: "import collections",
    };
    import_edit(source, module_fn, &import)
}

/// An import statement of a standard-library module to add to a module.
pub struct NewImport<'a> {
    /// The module it imports from (`collections.abc`).
    pub module: &'a str,
    /// A `from` import.
    pub is_from: bool,
    /// The statement, without a line break (`from collections.abc import Sequence`).
    pub statement: &'a str,
}

/// The edit inserting `import` (an insertion, or the replacement of the blank lines that follow
/// it), or `None` when the module's leading statements leave no safe place.
pub fn import_edit(
    source: &str,
    module_fn: &IrFunction,
    import: &NewImport<'_>,
) -> Option<(Span, String)> {
    let br = heki_ast::line_break(source);
    let (docstring_end, region_end) = import_region(source, module_fn);
    let floor = magic_comments_end(source);
    let mut stmts = import_statements(source, module_fn, region_end, floor);
    // `from __future__` imports must stay the first statements: nothing goes before or
    // between them, whatever the sort order.
    let future_end = stmts
        .iter()
        .filter(|s| s.module == "__future__")
        .map(|s| s.end)
        .max();
    stmts.retain(|s| s.module != "__future__");
    let blocks = blocks(source, &stmts);
    let is_future = |s: &ImportStmt| s.module == "__future__";
    let force_sort = blocks.iter().any(|b| {
        b.iter()
            .skip_while(|s| !s.is_from)
            .any(|s| !s.is_from && !is_future(s))
    });
    let key = |s: &ImportStmt| {
        if force_sort {
            (false, s.module.clone(), s.is_from)
        } else {
            (s.is_from, s.module.clone(), false)
        }
    };
    let new_key = key(&ImportStmt {
        start: 0,
        end: 0,
        module: import.module.to_lowercase(),
        is_from: import.is_from,
    });
    let others: Vec<&Vec<&ImportStmt>> = blocks
        .iter()
        .filter(|b| !b.iter().all(|s| is_future(s)))
        .collect();
    let stdlib = others.iter().find(|b| {
        b.iter()
            .all(|s| !s.module.starts_with('.') && heki_env::is_stdlib_module(&s.module))
    });
    if let Some(block) = stdlib {
        let at = block
            .iter()
            .find(|s| key(s) > new_key)
            .map_or_else(|| block.last().map_or(0, |s| s.end), |s| s.start);
        return Some((
            Span { start: at, end: at },
            format!("{}{br}", import.statement),
        ));
    }
    if let Some(first) = others.first().and_then(|b| b.first()) {
        let at = first.start;
        return Some((
            Span { start: at, end: at },
            format!("{}{br}{br}", import.statement),
        ));
    }
    let after_statement = blocks
        .iter()
        .flatten()
        .map(|s| s.end)
        .chain(future_end)
        .max()
        .or(docstring_end);
    first_import_edit(source, br, after_statement, import.statement)
}

/// The edit for a module without other imports: after `after_statement`, the end of the
/// `__future__` block or the docstring, or at the top (below leading comments), followed by the
/// blank lines isort wants before the next statement.
fn first_import_edit(
    source: &str,
    br: &str,
    after_statement: Option<u32>,
    statement: &str,
) -> Option<(Span, String)> {
    let anchor = after_statement.unwrap_or_else(|| after_leading_comments(source));
    // A blank line after the docstring or the `__future__` block, none below leading comments.
    let separated = after_statement.is_some();
    // The rewritten function follows: there is a next statement.
    let next = next_statement(source, anchor)?;
    let line = slice(source, next, line_end(source, next)).unwrap_or_default();
    let opens_def = ["def ", "async def ", "class ", "@"]
        .iter()
        .any(|p| line.starts_with(p));
    let blank_lines = if opens_def { 2 } else { 1 };
    // Only blank lines are replaced (a comment above the next statement stays put).
    let replaced_end = blank_run_end(source, anchor, next);
    let mut text = String::new();
    if separated {
        text.push_str(br);
    }
    text.push_str(statement);
    text.push_str(&br.repeat(blank_lines + 1));
    Some((
        Span {
            start: anchor,
            end: replaced_end,
        },
        text,
    ))
}

/// `(end of the docstring statement's line, start of the first statement that is not an
/// import)`, both line offsets.
fn import_region(source: &str, module_fn: &IrFunction) -> (Option<u32>, u32) {
    let mut insts: Vec<_> = module_fn.blocks.iter().flat_map(|b| &b.insts).collect();
    insts.sort_by_key(|i| i.span.start);
    let mut docstring_end = None;
    let mut region_end = source.len() as u32;
    for (n, inst) in insts.iter().enumerate() {
        match &inst.kind {
            // A docstring: the first statement, a string whose value nothing uses.
            InstKind::Const {
                dst,
                literal: ConstLiteral::Str(_),
            } if n == 0 && insts.iter().all(|i| !i.kind.operands().contains(dst)) => {
                docstring_end = Some(next_line(source, inst.span.end));
            }
            InstKind::Import { .. } | InstKind::StoreGlobal { .. } => {}
            _ => {
                region_end = line_start(source, inst.span.start);
                break;
            }
        }
    }
    (docstring_end, region_end)
}

/// The top-level import statements that start before `region_end`, in source order, with the
/// comments directly above them (but none of the first lines a shebang or an encoding
/// declaration must keep: `floor`).
fn import_statements(
    source: &str,
    module_fn: &IrFunction,
    region_end: u32,
    floor: u32,
) -> Vec<ImportStmt> {
    let mut out: Vec<ImportStmt> = Vec::new();
    let mut aliases: Vec<u32> = module_fn
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter(|i| matches!(i.kind, InstKind::Import { .. }) && i.span.start < region_end)
        .map(|i| i.span.end)
        .collect();
    aliases.sort_unstable();
    for end in aliases {
        let first = statement_line(source, end);
        if let Some(last) = out.last_mut()
            && last.start <= first
            && first < last.end
        {
            last.end = last.end.max(statement_end(source, first, end));
            continue;
        }
        let head = slice(source, first, line_end(source, first)).unwrap_or_default();
        let (is_from, rest) = if let Some(r) = head.strip_prefix("from ") {
            (true, r)
        } else if let Some(r) = head.strip_prefix("import ") {
            (false, r)
        } else {
            continue;
        };
        let module: String = rest
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '.'))
            .collect();
        out.push(ImportStmt {
            start: attached_comments(source, first).max(floor),
            end: statement_end(source, first, end),
            module: module.to_lowercase(),
            is_from,
        });
    }
    out
}

/// Statements on consecutive lines (comments between them allowed) form one block.
fn blocks<'s>(source: &str, stmts: &'s [ImportStmt]) -> Vec<Vec<&'s ImportStmt>> {
    let mut out: Vec<Vec<&ImportStmt>> = Vec::new();
    for s in stmts {
        let joined = out.last().and_then(|b| b.last()).is_some_and(|prev| {
            slice(source, prev.end, s.start).is_some_and(|between| between.trim().is_empty())
                && prev.end == s.start
        });
        match out.last_mut() {
            Some(b) if joined => b.push(s),
            _ => out.push(vec![s]),
        }
    }
    out
}

/// Offset of the line after the one containing `pos`.
fn next_line(source: &str, pos: u32) -> u32 {
    let end = line_end(source, pos);
    (end as usize + 1).min(source.len()) as u32
}

/// The first line of the statement whose alias ends at `alias_end`: the nearest line above
/// that starts at column 0 and is not a comment.
fn statement_line(source: &str, alias_end: u32) -> u32 {
    let mut ls = line_start(source, alias_end.saturating_sub(1));
    loop {
        let text = slice(source, ls, line_end(source, ls)).unwrap_or_default();
        let top = text
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace() && c != '#' && c != ')');
        if top || ls == 0 {
            return ls;
        }
        ls = line_start(source, ls - 1);
    }
}

/// The offset after the statement that starts at `first` and has an alias ending at
/// `alias_end`: past a closing parenthesis when the statement opened one.
fn statement_end(source: &str, first: u32, alias_end: u32) -> u32 {
    let head = slice(source, first, alias_end).unwrap_or_default();
    let mut end = line_end(source, alias_end);
    if head.contains('(')
        && !head.contains(')')
        && let Some(close) = source[alias_end as usize..].find(')')
    {
        end = line_end(source, alias_end + close as u32);
    }
    (end as usize + 1).min(source.len()) as u32
}

/// `first`, moved up over the comment lines directly above it.
fn attached_comments(source: &str, first: u32) -> u32 {
    let mut start = first;
    while start > 0 {
        let prev = line_start(source, start - 1);
        let text = slice(source, prev, start).unwrap_or_default().trim();
        if !text.starts_with('#') {
            break;
        }
        start = prev;
    }
    start
}

/// The offset after the lines that must stay first: a shebang (line 1), and an encoding
/// declaration (line 1 or 2, PEP 263).
fn magic_comments_end(source: &str) -> u32 {
    let mut end = 0;
    for n in 0..2 {
        let line_end_at = line_end(source, end);
        let text = slice(source, end, line_end_at).unwrap_or_default();
        let magic = (n == 0 && text.starts_with("#!")) || is_encoding_declaration(text);
        if !magic {
            break;
        }
        end = (line_end_at as usize + 1).min(source.len()) as u32;
    }
    end
}

/// `# -*- coding: utf-8 -*-` / `# vim: set fileencoding=...`: a comment line matching
/// `coding[:=]\s*[-\w.]+` (PEP 263).
fn is_encoding_declaration(line: &str) -> bool {
    let t = line.trim_start_matches([' ', '\t', '\x0c']);
    t.starts_with('#')
        && t.match_indices("coding").any(|(i, _)| {
            let rest = &t[i + "coding".len()..];
            rest.strip_prefix([':', '='])
                .map(|r| r.trim_start_matches([' ', '\t']))
                .is_some_and(|r| r.starts_with(|c: char| c.is_alphanumeric() || "-_.".contains(c)))
        })
}

/// The offset after the comment lines a module starts with (a shebang, an encoding cookie, a
/// license header).
fn after_leading_comments(source: &str) -> u32 {
    let mut at = 0;
    while (at as usize) < source.len() {
        let end = line_end(source, at);
        let text = slice(source, at, end).unwrap_or_default().trim();
        if !text.starts_with('#') {
            break;
        }
        at = (end as usize + 1).min(source.len()) as u32;
    }
    at
}

/// The start of the first line at or after `from` that holds a statement (not blank, not a
/// comment).
fn next_statement(source: &str, from: u32) -> Option<u32> {
    let mut at = from;
    while (at as usize) < source.len() {
        let end = line_end(source, at);
        let text = slice(source, at, end).unwrap_or_default().trim();
        if !text.is_empty() && !text.starts_with('#') {
            return Some(at);
        }
        at = (end as usize + 1).min(source.len()) as u32;
    }
    None
}

/// The end of the blank lines that start at `from` (before `limit`).
fn blank_run_end(source: &str, from: u32, limit: u32) -> u32 {
    let mut at = from;
    while at < limit {
        let end = line_end(source, at);
        if !slice(source, at, end).unwrap_or_default().trim().is_empty() {
            break;
        }
        at = (end as usize + 1).min(source.len()) as u32;
    }
    at
}
