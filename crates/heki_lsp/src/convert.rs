//! heki diagnostics → LSP diagnostics and code actions (docs/DETAILED_SPEC_M3_M7.md 5.2).
//!
//! - `Diagnostic.related` → `DiagnosticRelatedInformation` (every call-chain hop).
//! - `Diagnostic.fix` of `safe` + `unconditional` rules → `quickfix`; size-dependent `OPT`
//!   suggestions → `refactor.rewrite.heki`.
//! - Every diagnostic → a `quickfix` appending `# heki: ignore[CODE]` to its line.

use heki_ast::Span;
use heki_diagnostics::Diagnostic;
use heki_env::Severity;
use heki_rules::find_rule;
use std::collections::HashMap;
use std::path::Path;
use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, DiagnosticRelatedInformation,
    DiagnosticSeverity, Location, NumberOrString, Position, Range, TextEdit, Url, WorkspaceEdit,
};

/// Code action kind of size-dependent `OPT` rewrites.
pub const REWRITE_KIND: &str = "refactor.rewrite.heki";

/// Byte offset → LSP position (0-based line, UTF-16 character).
pub fn position(text: &str, offset: u32) -> Position {
    let offset = (offset as usize).min(text.len());
    let offset = (0..=offset)
        .rev()
        .find(|&o| text.is_char_boundary(o))
        .unwrap_or(0);
    let before = &text[..offset];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    Position {
        line: before.matches('\n').count() as u32,
        character: before[line_start..].encode_utf16().count() as u32,
    }
}

pub fn range(text: &str, span: Span) -> Range {
    Range {
        start: position(text, span.start),
        end: position(text, span.end),
    }
}

fn severity(s: Severity) -> DiagnosticSeverity {
    match s {
        Severity::Error => DiagnosticSeverity::ERROR,
        Severity::Warning => DiagnosticSeverity::WARNING,
    }
}

/// Convert one diagnostic of a document with text `text`. `text_of` gives the text of other
/// files (related call-chain hops).
pub fn diagnostic(
    d: &Diagnostic,
    text: &str,
    text_of: &dyn Fn(&Path) -> Option<String>,
) -> tower_lsp::lsp_types::Diagnostic {
    let related: Vec<DiagnosticRelatedInformation> = d
        .related
        .iter()
        .filter_map(|r| {
            let uri = Url::from_file_path(&r.path).ok()?;
            let other = text_of(&r.path)?;
            Some(DiagnosticRelatedInformation {
                location: Location {
                    uri,
                    range: range(&other, r.span),
                },
                message: r.message.clone(),
            })
        })
        .collect();
    tower_lsp::lsp_types::Diagnostic {
        range: range(text, d.span),
        severity: Some(severity(d.severity)),
        code: Some(NumberOrString::String(d.rule_code.clone())),
        source: Some("heki".into()),
        message: format!("{} ({})", d.message, d.rule_slug),
        related_information: (!related.is_empty()).then_some(related),
        ..Default::default()
    }
}

fn touches(a: &Range, b: &Range) -> bool {
    let le = |x: &Position, y: &Position| (x.line, x.character) <= (y.line, y.character);
    le(&a.start, &b.end) && le(&b.start, &a.end)
}

/// Code actions for the diagnostics of `uri` touching `range`.
pub fn code_actions(
    uri: &Url,
    text: &str,
    diags: &[Diagnostic],
    lsp_diags: &[tower_lsp::lsp_types::Diagnostic],
    request: &Range,
) -> Vec<CodeActionOrCommand> {
    let mut out = Vec::new();
    for (d, ld) in diags.iter().zip(lsp_diags) {
        if !touches(&ld.range, request) {
            continue;
        }
        if let Some(fix) = &d.fix {
            let unconditional = find_rule(&d.rule_code).is_some_and(|m| m.auto_fix_default);
            let edits = fix
                .iter()
                .map(|e| TextEdit {
                    range: range(text, e.span),
                    new_text: e.replacement.clone(),
                })
                .collect();
            out.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: if unconditional {
                    format!("Apply the safe {} fix", d.rule_code)
                } else {
                    format!(
                        "Apply the proven-safe {} rewrite (size-dependent)",
                        d.rule_code
                    )
                },
                kind: Some(if unconditional {
                    CodeActionKind::QUICKFIX
                } else {
                    CodeActionKind::new(REWRITE_KIND)
                }),
                diagnostics: Some(vec![ld.clone()]),
                edit: Some(WorkspaceEdit {
                    changes: Some(HashMap::from([(uri.clone(), edits)])),
                    ..Default::default()
                }),
                is_preferred: Some(unconditional),
                ..Default::default()
            }));
        }
        let line = d.line.saturating_sub(1) as usize;
        let end = text
            .lines()
            .nth(line)
            .map_or(0, |l| l.encode_utf16().count() as u32);
        let at = Position {
            line: line as u32,
            character: end,
        };
        out.push(CodeActionOrCommand::CodeAction(CodeAction {
            title: format!(
                "Suppress {} with `# heki: ignore[{}]`",
                d.rule_code, d.rule_code
            ),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![ld.clone()]),
            edit: Some(WorkspaceEdit {
                changes: Some(HashMap::from([(
                    uri.clone(),
                    vec![TextEdit {
                        range: Range { start: at, end: at },
                        new_text: format!("  # heki: ignore[{}]", d.rule_code),
                    }],
                )])),
                ..Default::default()
            }),
            ..Default::default()
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_count_utf16_units() {
        let text = "a = 1\nb = \"é😀\" + c\n";
        let c = text.find("c\n").unwrap() as u32;
        assert_eq!(
            position(text, c),
            Position {
                line: 1,
                character: 12
            }
        );
        // An offset inside a multi-byte character snaps back to its start.
        let e = text.find('é').unwrap() as u32 + 1;
        assert_eq!(position(text, e).character, 5);
        assert_eq!(position(text, 10_000).line, 2);
    }
}
