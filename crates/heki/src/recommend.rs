//! `heki recommend [PATHS]...` (experimental): advice on the configuration of a project, from the
//! analysis `heki check` runs on it ([`heki_rules::recommend`]). Exit codes: 0 the report was
//! printed, 2 configuration, I/O, or syntax errors (the report of the other files is still
//! printed). It writes no file.

use crate::check::{
    baseline_path, finish, load, print_diff, selectors, shown_dir, shown_errors, shown_text,
    warn_files_not_analyzed,
};
use crate::{AnalysisArgs, OutputFormat, RecommendArgs, RecommendFocus, ReportFormat};
use anyhow::Result;
use heki_diagnostics::{RatchetBaseline, Stylesheet};
use heki_rules::project::CommandPaths;
use heki_rules::recommend::{Advice, Focus, Request, recommend};
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
    let request = Request {
        root,
        project,
        current,
        snapshot,
        baseline,
        paths: &args.paths,
        analyzed_paths: &paths.analyzed,
        check_options: check_options(&args.analysis),
        shown: &shown_text,
        errors: errors.clone(),
        all: args.all,
        focus: args.focus.map(|RecommendFocus::Types| Focus::Types),
        diff: args.diff,
    };
    let Advice { report, edited } = recommend(request)?;
    if args.diff {
        // A patch tool reads stdout: no color, and the summary on stderr.
        for f in &edited {
            print_diff(&f.path, &f.text, &f.edited, &Stylesheet::plain())?;
        }
        let (exact, heuristic) = report.suggestions.totals();
        let left_out = match heuristic {
            0 => String::new(),
            n => format!(
                "; the {n} heuristic one(s) are not in it ({} --focus types --all lists them)",
                heki_rules::recommend::analysis_command(&report.analyzed, "recommend")
            ),
        };
        eprintln!(
            "The diff holds {exact} exact suggestion group(s) in {} file(s){left_out}.",
            edited.len()
        );
    } else {
        match args.output_format {
            ReportFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
            ReportFormat::Text => print!("{}", report.content),
        }
    }
    finish(OutputFormat::Full, &problems, ExitCode::SUCCESS)
}

/// The options of `heki check` that make it analyze and select as this run did, as the words of
/// a command line: what the command naming the counted reports has to carry.
fn check_options(args: &AnalysisArgs) -> Vec<String> {
    let mut words = Vec::new();
    let mut option = |name: &str, value: String| {
        words.push(name.to_owned());
        words.push(value);
    };
    for (name, codes) in [
        ("--select", &args.select),
        ("--extend-select", &args.extend_select),
        ("--ignore", &args.ignore),
    ] {
        if !codes.is_empty() {
            option(name, codes.join(","));
        }
    }
    for (name, path) in [
        ("--config", &args.config),
        ("--baseline", &args.baseline),
        ("--snapshot", &args.snapshot),
    ] {
        if let Some(path) = path {
            option(name, path.to_string_lossy().into_owned());
        }
    }
    if let Some(version) = &args.target_version {
        option("--target-version", version.clone());
    }
    match args.heuristic.value() {
        Some(true) => words.push("--heuristic".to_owned()),
        Some(false) => words.push("--no-heuristic".to_owned()),
        None => {}
    }
    words
}
