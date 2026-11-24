//! `heki dump-open-premises [PATHS]...` (hidden): the reports `heki check` prints for the same
//! configuration and selection, after inline suppressions, each heuristic report of an exact
//! claim with the premises it left open (`heki_rules::premises::OpenCause`) and the conditions of
//! the exact tier that are no premise (`heki_rules::premises::ConditionCause`). Exit codes: 0,
//! or 2 when a file could not be analyzed (the reports of the others are still printed).

use crate::check::{load_and_index, selectors, shown_text};
use crate::{DumpOpenPremisesArgs, OutputFormat};
use anyhow::Result;
use heki_diagnostics::{Axis, Certainty, Diagnostic, OpenCondition, OpenPremise, OpenRecord};
use heki_rules::premises::{self, Closer};
use heki_rules::project::Facts;
use serde::Serialize;
use std::process::ExitCode;

#[derive(Serialize)]
struct Report<'d> {
    rule: &'d str,
    path: String,
    line: u32,
    column: u32,
    qualname: &'d str,
    certainty: Certainty,
    open_premises: Vec<Open<'d>>,
    /// The conditions of the exact tier that are no premise, left open.
    open_conditions: Vec<OpenConditionOut<'d>>,
}

#[derive(Serialize)]
struct OpenConditionOut<'d> {
    axis: Axis,
    condition: &'static str,
    cause: &'static str,
    closer: Option<Closer>,
    at: Option<At>,
    detail: Option<&'d str>,
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

impl At {
    fn of(at: &heki_diagnostics::RelatedLocation) -> Self {
        Self {
            path: shown_text(&at.path),
            line: at.line,
            column: at.column,
        }
    }
}

impl<'d> Open<'d> {
    fn of(p: &'d OpenPremise) -> Self {
        Self {
            premise: p.premise,
            cause: p.cause,
            closer: premises::cause(p.premise, p.cause).map(|c| c.closer),
            at: p.at.as_ref().map(At::of),
            detail: p.detail.as_deref(),
        }
    }
}

impl<'d> OpenConditionOut<'d> {
    fn of(c: &'d OpenCondition) -> Self {
        Self {
            axis: c.axis,
            condition: c.condition,
            cause: c.cause,
            closer: premises::condition_cause(c.condition, c.cause).map(|c| c.closer),
            at: c.at.as_ref().map(At::of),
            detail: c.detail.as_deref(),
        }
    }
}

impl<'d> Report<'d> {
    fn of(d: &'d Diagnostic) -> Self {
        Self {
            rule: &d.rule_code,
            path: shown_text(&d.path),
            line: d.line,
            column: d.column,
            qualname: &d.qualname,
            certainty: d.certainty,
            open_premises: d
                .open_records
                .iter()
                .filter_map(|r| match r {
                    OpenRecord::Premise(p) => Some(Open::of(p)),
                    OpenRecord::Condition(_) => None,
                })
                .collect(),
            open_conditions: d
                .open_records
                .iter()
                .filter_map(|r| match r {
                    OpenRecord::Condition(c) => Some(OpenConditionOut::of(c)),
                    OpenRecord::Premise(_) => None,
                })
                .collect(),
        }
    }
}

pub(crate) fn run(args: &DumpOpenPremisesArgs) -> Result<ExitCode> {
    let indexed = load_and_index(
        &args.paths,
        &args.analysis,
        &selectors(&args.analysis),
        OutputFormat::Json,
        None,
        Facts::Check,
    )?;
    let (diags, _) = indexed.diagnose_units(&indexed.project.units(), true)?;
    let reports: Vec<Report<'_>> = diags.iter().map(Report::of).collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({ "reports": reports }))?
    );
    let errors = crate::failure::of_files(&indexed.project.errors);
    crate::check::finish(OutputFormat::Json, &errors, ExitCode::SUCCESS)
}
