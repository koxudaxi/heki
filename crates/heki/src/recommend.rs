//! `heki recommend [PATHS]...` (experimental): advice on the configuration of a project, from the
//! analysis `heki check` runs on it ([`heki_rules::recommend`]). Exit codes: 0 the report was
//! printed, 2 configuration, I/O, or syntax errors (the report of the other files is still
//! printed). It writes no file.

use crate::check::{
    baseline_path, finish, load, selectors, shown_dir, shown_errors, shown_text,
    warn_files_not_analyzed,
};
use crate::{OutputFormat, RecommendArgs, ReportFormat};
use anyhow::Result;
use heki_diagnostics::RatchetBaseline;
use heki_rules::project::CommandPaths;
use heki_rules::recommend::{Request, recommend};
use std::process::ExitCode;

pub(crate) fn run(args: &RecommendArgs) -> Result<ExitCode> {
    if args.output_format_json_schema {
        let schema = heki_rules::recommend::json_schema();
        println!("{}", serde_json::to_string_pretty(&schema)?);
        return Ok(ExitCode::SUCCESS);
    }
    let paths = CommandPaths::of(&args.paths);
    let (project, current, snapshot) = load(
        &paths.analyzed,
        &args.analysis,
        &selectors(&args.analysis),
        OutputFormat::Full,
        None,
    )?;
    warn_files_not_analyzed(&paths, &project, OutputFormat::Full);
    let baseline = match baseline_path(&args.analysis, &project.env) {
        Some(path) => Some(RatchetBaseline::load(&path, heki_rules::reports_groups)?),
        None => None,
    };
    let errors = shown_errors(&project.errors);
    let problems = crate::failure::of_files(&project.errors);
    let root = shown_dir(&project.env.root);
    let report = recommend(Request {
        root,
        project,
        current,
        snapshot,
        baseline,
        paths: &args.paths,
        shown: &shown_text,
        errors: errors.clone(),
        all: args.all,
    })?;
    match args.output_format {
        ReportFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        ReportFormat::Text => print!("{}", report.content),
    }
    finish(OutputFormat::Full, &problems, ExitCode::SUCCESS)
}
