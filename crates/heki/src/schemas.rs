//! `heki check --output-format-json-schema TARGET`: the JSON Schema of one JSON format heki
//! writes, as `docs/schemas/` publishes it (`scripts/build_docs.py` writes the files from here).
//!
//! Every schema is derived from the types that serialize the format. A format that has no
//! `version` field has the version in its name: a change that breaks a reader makes a new schema
//! with the next number and keeps the old one.

use crate::inspect::{Entry, InspectError};
use crate::{AssumptionRecord, JsonSchemaTarget, RuleRecord};
use heki_diagnostics::{BaselineVersion, Diagnostic};
use heki_env::published_schema;
use heki_rules::import_view::ImportsReport;
use heki_rules::snapshot::SummarySnapshot;
use schemars::JsonSchema;
use serde_json::Value;

/// `heki rule` prints one rule, or an array of every rule.
#[derive(JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum RuleOutput {
    One(RuleRecord),
    All(Vec<RuleRecord>),
}

/// `heki inspect` prints one report or error, or with `-` an array of them.
#[derive(JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum InspectOutput {
    One(Entry),
    Each(Vec<Entry>),
}

/// `heki inspect --imports` prints a report, or the error of a name that names no module.
#[derive(JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum ImportsOutput {
    Report(ImportsReport),
    Error(InspectError),
}

/// The schema of `target`, pretty printed.
pub(crate) fn json_schema(target: JsonSchemaTarget) -> String {
    use JsonSchemaTarget as T;
    let schema: Value = match target {
        T::GeneratePrompt => return include_str!("generate_prompt.schema.json").to_owned(),
        T::Check1 => {
            published_schema::<Vec<Diagnostic>>("check-1", "heki check --output-format json")
        }
        T::ReportSuppressed1 => crate::suppressed::schema(),
        T::SummarySnapshot1 => published_schema::<SummarySnapshot>(
            "summary-snapshot-1",
            "heki snapshot, the summary snapshot file",
        ),
        T::Baseline1 => heki_diagnostics::baseline_schema(BaselineVersion::V1),
        T::Baseline2 => heki_diagnostics::baseline_schema(BaselineVersion::V2),
        T::Baseline3 => heki_diagnostics::baseline_schema(BaselineVersion::V3),
        T::Inspect1 => {
            published_schema::<InspectOutput>("inspect-1", "heki inspect --output-format json")
        }
        T::InspectImports1 => published_schema::<ImportsOutput>(
            "inspect-imports-1",
            "heki inspect --imports --output-format json",
        ),
        T::Rule1 => published_schema::<RuleOutput>("rule-1", "heki rule --output-format json"),
        T::Assumptions1 => {
            published_schema::<Vec<AssumptionRecord>>("assumptions-1", "heki assumptions --json")
        }
        T::Error1 => published_schema::<crate::failure::ErrorOutput>(
            "error-1",
            "What a failed command writes with --output-format json",
        ),
    };
    let mut text = serde_json::to_string_pretty(&schema).expect("a schema serializes");
    text.push('\n');
    text
}
