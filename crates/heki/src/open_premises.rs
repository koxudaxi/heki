//! `heki dump-open-premises [PATHS]...` (hidden): the reports `heki check` prints for the same
//! configuration and selection, after inline suppressions, each heuristic report of an exact
//! claim with the premises it left open (`heki_rules::premises::OpenCause`). Exit codes: 0, or
//! 2 when a file could not be analyzed (the reports of the others are still printed).

use crate::check::{load_and_index, shown_errors, shown_text};
use crate::{DumpOpenPremisesArgs, OutputFormat};
use anyhow::Result;
use heki_diagnostics::{Certainty, Diagnostic, OpenPremise};
use heki_rules::premises::{self, Closer};
use serde::Serialize;
use std::process::ExitCode;

#[derive(Serialize)]
struct Report<'d> {
    rule: &'d str,
    /// The exact rule whose claim a heuristic side reports (`premises::exact_side_of`).
    exact_side: Option<&'static str>,
    path: String,
    line: u32,
    column: u32,
    qualname: &'d str,
    certainty: Certainty,
    open_premises: Vec<Open<'d>>,
}

#[derive(Serialize)]
struct Open<'d> {
    premise: &'static str,
    cause: &'static str,
    closer: Option<Closer>,
    at: Option<At>,
    detail: Option<&'d str>,
}

#[derive(Serialize)]
struct At {
    path: String,
    line: u32,
    column: u32,
}

impl<'d> Open<'d> {
    fn of(p: &'d OpenPremise) -> Self {
        Self {
            premise: p.premise,
            cause: p.cause,
            closer: premises::cause(p.premise, p.cause).map(|c| c.closer),
            at: p.at.as_ref().map(|at| At {
                path: shown_text(&at.path),
                line: at.line,
                column: at.column,
            }),
            detail: p.detail.as_deref(),
        }
    }
}

impl<'d> Report<'d> {
    fn of(d: &'d Diagnostic) -> Self {
        Self {
            rule: &d.rule_code,
            exact_side: premises::exact_side_of(&d.rule_code),
            path: shown_text(&d.path),
            line: d.line,
            column: d.column,
            qualname: &d.qualname,
            certainty: d.certainty,
            open_premises: d.open_premises.iter().map(Open::of).collect(),
        }
    }
}

pub(crate) fn run(args: &DumpOpenPremisesArgs) -> Result<ExitCode> {
    let indexed = load_and_index(&args.paths, &args.analysis, OutputFormat::Json, None, false)?;
    let (diags, _) = indexed.diagnose_units(&indexed.project.units(), true)?;
    let reports: Vec<Report<'_>> = diags.iter().map(Report::of).collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({ "reports": reports }))?
    );
    let errors = shown_errors(&indexed.project.errors);
    crate::check::finish(OutputFormat::Json, &errors, ExitCode::SUCCESS)
}
