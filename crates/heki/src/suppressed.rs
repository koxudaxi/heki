//! `heki check --report-suppressed`: the reports that inline suppressions hid, as a JSON file.

use crate::check::Touched;
use crate::failure;
use anyhow::Result;
use heki_ast::LineIndex;
use heki_diagnostics::{BaselineVersion, SuppressionForm, baseline_key};
use heki_rules::project::{Hidden, ParsedFile, Project};
use rustc_hash::FxHashMap;
use schemars::JsonSchema;
use serde::Serialize;
use std::path::Path;

/// The version of the document; it changes only when a field is removed or changes meaning.
const SCHEMA_VERSION: u32 = 1;

/// The file `heki check --report-suppressed` writes.
#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct Document {
    /// The version of the file: 1.
    #[schemars(extend("const" = 1))]
    schema_version: u32,
    /// The reports the inline suppressions hid, sorted by file, function, code, target, and
    /// position.
    suppressed: Vec<Entry>,
}

/// The JSON Schema of the file, as `docs/schemas/report-suppressed-1.json` publishes it.
pub(crate) fn schema() -> serde_json::Value {
    heki_env::published_schema::<Document>("report-suppressed-1", "heki check --report-suppressed")
}

/// One report an inline suppression hid.
#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct Entry {
    /// The code of the rule whose report was hidden.
    code: String,
    /// Relative to the project root, with forward slashes.
    file: String,
    /// The qualified name of the function the report is in; none at module level.
    function: Option<String>,
    /// The target of the report's Ratchet baseline key, which the baseline counts it under.
    target: String,
    /// The line of the hidden report.
    line: u32,
    /// The byte column of the hidden report.
    column: u32,
    /// The suppression that hid it.
    directive: Directive,
}

/// The suppression that hid a report.
#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct Directive {
    /// `heki-ignore` (names rules), `heki-ignore-bare`, `noqa-code` (names rules), or `noqa-bare`.
    #[schemars(extend("enum" = ["heki-ignore", "heki-ignore-bare", "noqa-code", "noqa-bare"]))]
    kind: &'static str,
    /// The file of the comment, relative to the project root, with forward slashes.
    file: String,
    /// The line of the comment.
    line: u32,
    /// The byte column of the comment.
    column: u32,
}

fn kind(form: SuppressionForm, names_rules: bool) -> &'static str {
    match (form, names_rules) {
        (SuppressionForm::Heki, true) => "heki-ignore",
        (SuppressionForm::Heki, false) => "heki-ignore-bare",
        (SuppressionForm::Noqa, true) => "noqa-code",
        (SuppressionForm::Noqa, false) => "noqa-bare",
    }
}

/// The document of the reports `hidden` in `project`: those a run with `touched` shows, as the
/// run's output covers them. A baseline changes nothing in it: it counts reports, and a hidden
/// report is none.
fn document(hidden: &[Hidden], project: &Project, touched: Option<&Touched>) -> Document {
    let files: FxHashMap<&Path, (&ParsedFile, LineIndex)> = project
        .files
        .iter()
        .map(|f| (f.path.as_path(), (f, LineIndex::from_source(&f.source))))
        .collect();
    let mut suppressed: Vec<Entry> = hidden
        .iter()
        .filter(|h| Touched::shows(touched, &h.report))
        .filter_map(|h| {
            let d = &h.report;
            let (file, lines) = files.get(d.path.as_path())?;
            let key = baseline_key(d, file.rel.clone(), BaselineVersion::V3);
            let (line, column) = lines.line_col(h.by.span.start);
            Some(Entry {
                code: d.rule_code.clone(),
                file: file.rel.clone(),
                function: Some(d.qualname.clone()).filter(|q| !q.is_empty()),
                target: key.target,
                line: d.line,
                column: d.column,
                directive: Directive {
                    kind: kind(h.by.form, !h.by.codes.is_empty()),
                    file: file.rel.clone(),
                    line,
                    column,
                },
            })
        })
        .collect();
    suppressed.sort_by(|a, b| {
        let key = |e: &Entry| {
            (
                (e.file.clone(), e.function.clone(), e.code.clone()),
                (e.target.clone(), e.line, e.column),
                (e.directive.line, e.directive.column, e.directive.kind),
            )
        };
        key(a).cmp(&key(b))
    });
    Document {
        schema_version: SCHEMA_VERSION,
        suppressed,
    }
}

/// Write the document of `hidden` to `path`.
pub(crate) fn write(
    path: &Path,
    hidden: &[Hidden],
    project: &Project,
    touched: Option<&Touched>,
) -> Result<()> {
    let text = serde_json::to_string_pretty(&document(hidden, project, touched))? + "\n";
    std::fs::write(path, text).map_err(|e| failure::unwritable(path, &e))?;
    Ok(())
}
