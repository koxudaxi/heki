//! heki diagnostics → LSP diagnostics and code actions (docs/lsp.md).
//!
//! - `Diagnostic.message` → the LSP message, with ` [heuristic]` after it for a heuristic rule
//!   (`data.certainty` says it for every rule), and the rule's page as `codeDescription`;
//!   `Diagnostic.related` →
//!   `DiagnosticRelatedInformation` (every call-chain hop), then `Diagnostic.help` as one more
//!   entry at the diagnostic's own range (`help: ...`), as rust-analyzer shows a `help:` child.
//! - `Diagnostic.fix` of `safe` + `unconditional` rules → `quickfix`; size-dependent `OPT`
//!   suggestions → `refactor.rewrite.heki`; a fix whose result would not parse is withheld.
//! - Every diagnostic → a `quickfix` appending `# heki: ignore[CODE]` to its line, unless no
//!   comment fits there (the line ends inside a string or with a `\` continuation).

use heki_ast::{LineIndex, Span};
use heki_diagnostics::{Diagnostic, fix_python_source};
use heki_env::Severity;
use heki_rules::find_rule;
use std::collections::HashMap;
use std::path::Path;
use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeDescription, DiagnosticRelatedInformation, DiagnosticSeverity,
    DocumentChanges, Location, NumberOrString, OneOf, OptionalVersionedTextDocumentIdentifier,
    Position, Range, TextDocumentEdit, TextEdit, Url, WorkspaceEdit,
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
    let lines = LineIndex::from_source(text);
    let line = lines.line(offset as u32);
    let line_start = lines.line_start(line).unwrap_or(0) as usize;
    Position {
        line: line - 1,
        character: text[line_start..offset].encode_utf16().count() as u32,
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
    let own_range = range(text, d.span);
    let help = d.help.as_ref().and_then(|help| {
        Some(DiagnosticRelatedInformation {
            location: Location {
                uri: Url::from_file_path(&d.path).ok()?,
                range: own_range,
            },
            message: format!("help: {help}"),
        })
    });
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
        .chain(help)
        .collect();
    let page = find_rule(&d.rule_code).and_then(|r| Url::parse(&r.page_url()).ok());
    tower_lsp::lsp_types::Diagnostic {
        range: own_range,
        severity: Some(severity(d.severity)),
        code: Some(NumberOrString::String(d.rule_code.clone())),
        code_description: page.map(|href| CodeDescription { href }),
        source: Some("heki".into()),
        message: match d.certainty.label() {
            Some(label) => format!("{} [{label}]", d.message),
            None => d.message.clone(),
        },
        related_information: (!related.is_empty()).then_some(related),
        data: Some(serde_json::json!({ "certainty": d.certainty })),
        ..Default::default()
    }
}

fn touches(a: &Range, b: &Range) -> bool {
    let le = |x: &Position, y: &Position| (x.line, x.character) <= (y.line, y.character);
    le(&a.start, &b.end) && le(&b.start, &a.end)
}

/// Where `# heki: ignore[CODE]` goes to suppress a diagnostic on 1-based `line`: the end of the
/// line, when a comment fits there (`ParsedModule::takes_comment_at`). `None` when it does not:
/// the line ends inside a string or with a `\` continuation, or the text does not parse. A
/// suppression applies to its own line only, so no other place would do.
fn suppression_point(
    text: &str,
    lines: &LineIndex,
    parsed: Option<&heki_ast::ParsedModule>,
    line: u32,
) -> Option<Position> {
    let start = lines.line_start(line)?;
    let line_text = lines.line_text(text, line)?;
    let end = start + u32::try_from(line_text.len()).ok()?;
    parsed?.takes_comment_at(text, end).then(|| Position {
        line: line - 1,
        character: line_text.encode_utf16().count() as u32,
    })
}

/// The document code actions edit: its URI and, for a client applying versioned edits, the
/// version the edits were computed on (the client rejects them for any other).
pub struct EditTarget<'u> {
    pub uri: &'u Url,
    pub version: Option<i32>,
}

impl EditTarget<'_> {
    fn edit(&self, edits: Vec<TextEdit>) -> WorkspaceEdit {
        match self.version {
            Some(version) => WorkspaceEdit {
                document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: self.uri.clone(),
                        version: Some(version),
                    },
                    edits: edits.into_iter().map(OneOf::Left).collect(),
                }])),
                ..Default::default()
            },
            None => WorkspaceEdit {
                changes: Some(HashMap::from([(self.uri.clone(), edits)])),
                ..Default::default()
            },
        }
    }
}

/// Whether the client may apply `fix` to `text` as it is: its edits apply and the result
/// parses, as `heki check --fix` requires of a pass (`fix_python_source`). A fix that would
/// leave the file invalid Python is a heki bug; its action is withheld.
fn applies_cleanly(text: &str, fix: &[heki_diagnostics::TextEdit]) -> bool {
    let pass = fix_python_source("", text, &[fix]);
    pass.refused.is_none() && !pass.applied.is_empty()
}

/// Code actions for the diagnostics of the document touching `range`.
pub fn code_actions(
    target: &EditTarget<'_>,
    text: &str,
    diags: &[Diagnostic],
    lsp_diags: &[tower_lsp::lsp_types::Diagnostic],
    request: &Range,
) -> Vec<CodeAction> {
    let mut out = Vec::new();
    let lines = LineIndex::from_source(text);
    let parsed = heki_ast::parse_module(text).ok();
    for (d, ld) in diags.iter().zip(lsp_diags) {
        if !touches(&ld.range, request) {
            continue;
        }
        if let Some(fix) = d.fix.as_ref().filter(|fix| applies_cleanly(text, fix)) {
            let unconditional = find_rule(&d.rule_code).is_some_and(|m| m.auto_fix_default);
            let edits = fix
                .iter()
                .map(|e| TextEdit {
                    range: range(text, e.span),
                    new_text: e.replacement.clone(),
                })
                .collect();
            out.push(CodeAction {
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
                edit: Some(target.edit(edits)),
                is_preferred: Some(unconditional),
                ..Default::default()
            });
        }
        let Some(at) = suppression_point(text, &lines, parsed.as_ref(), d.line) else {
            continue;
        };
        out.push(CodeAction {
            title: format!(
                "Suppress {} with `# heki: ignore[{}]`",
                d.rule_code, d.rule_code
            ),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![ld.clone()]),
            edit: Some(target.edit(vec![TextEdit {
                range: Range { start: at, end: at },
                new_text: format!("  # heki: ignore[{}]", d.rule_code),
            }])),
            ..Default::default()
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_count_utf16_units() {
        let text = "a = 1\nb = \"£😀\" + c\n";
        let c = text.find("c\n").unwrap() as u32;
        assert_eq!(
            position(text, c),
            Position {
                line: 1,
                character: 12
            }
        );
        // An offset inside a multi-byte character snaps back to its start.
        let e = text.find('£').unwrap() as u32 + 1;
        assert_eq!(position(text, e).character, 5);
        assert_eq!(position(text, 10_000).line, 2);
    }

    #[test]
    fn fixes_whose_result_would_not_parse_are_withheld() {
        let text =
            "def f(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n";
        let at = |s: &str| u32::try_from(text.find(s).unwrap()).unwrap();
        let span = Span {
            start: at("x = x"),
            end: at("x = x") + 11,
        };
        let diag = |replacement: &str| Diagnostic {
            path: "a.py".into(),
            line: 4,
            column: 9,
            span,
            qualname: "f".into(),
            rule_code: "OPT101".into(),
            rule_slug: "loop-list-concat".into(),
            target: "x".into(),
            severity: Severity::Error,
            certainty: heki_diagnostics::Certainty::Exact,
            message: "m".into(),
            help: None,
            evidence: None,
            blocker: None,
            related: Vec::new(),
            fix: Some(vec![heki_diagnostics::TextEdit {
                span,
                replacement: replacement.into(),
            }]),
            sites: 1,
            fold_key: None,
        };
        let uri = Url::parse("file:///a.py").unwrap();
        let target = EditTarget {
            uri: &uri,
            version: None,
        };
        let actions = |d: &Diagnostic| {
            let lsp = diagnostic(d, text, &|_| None);
            let titles: Vec<String> = code_actions(
                &target,
                text,
                std::slice::from_ref(d),
                std::slice::from_ref(&lsp),
                &lsp.range,
            )
            .into_iter()
            .map(|a| a.title)
            .collect();
            titles
        };
        assert_eq!(
            actions(&diag("x.append(i)")),
            [
                "Apply the safe OPT101 fix",
                "Suppress OPT101 with `# heki: ignore[OPT101]`"
            ]
        );
        // Only the suppression is left for a fix that would break the file.
        assert_eq!(
            actions(&diag("x.append(i")),
            ["Suppress OPT101 with `# heki: ignore[OPT101]`"]
        );
    }

    #[test]
    fn positions_count_cr_and_crlf_line_breaks() {
        let text = "a\rb\r\nc";
        let at = |c: char| position(text, text.find(c).unwrap() as u32);
        assert_eq!(
            [at('a'), at('b'), at('c')].map(|p| (p.line, p.character)),
            [(0, 0), (1, 0), (2, 0)]
        );
    }
}
