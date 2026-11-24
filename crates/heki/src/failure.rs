//! What the command line prints when a command cannot do what it was asked
//! ([`heki_env::Problem`]): on stderr in the layout of a report (`error:`, where, the source
//! line, `= help:`, `= docs:`), and, when the command writes JSON, also as `{"error": ...}` on
//! stdout. Every failure of every command goes through [`Failures::fail`] or [`print`], so all of
//! them carry the same parts.

use crate::check::shown_path;
use crate::{Commands, OutputFormat, ReportFormat};
use heki_diagnostics::{Stylesheet, file_line, github_escape, path_text, render_problem};
use heki_env::{ConfigError, Explain, Problem};
use heki_rules::project::{FileError, ProjectError, ProjectOptions};
use std::path::{Path, PathBuf};

/// How a command reports its failure: in its output format, with the configuration values it
/// is about found in the configuration file it reads.
pub(crate) struct Failures {
    format: OutputFormat,
    /// The paths and `--config` of a command that reads the project's configuration.
    project: Option<(Vec<PathBuf>, Option<PathBuf>)>,
}

impl Failures {
    pub(crate) fn of(command: &Commands) -> Self {
        let report = |format: ReportFormat| match format {
            ReportFormat::Json => OutputFormat::Json,
            ReportFormat::Text => OutputFormat::Full,
        };
        let project =
            |paths: &[PathBuf], config: Option<&PathBuf>| Some((paths.to_vec(), config.cloned()));
        let (format, project) = match command {
            Commands::Check(a) => (
                a.output_format,
                project(&a.paths, a.analysis.config.as_ref()),
            ),
            Commands::Inspect(a) => (
                report(a.output_format),
                project(&a.paths, a.analysis.config.as_ref()),
            ),
            Commands::Recommend(a) => (
                report(a.output_format),
                project(&a.paths, a.analysis.config.as_ref()),
            ),
            Commands::Snapshot(a) => (OutputFormat::Full, project(&a.paths, a.config.as_ref())),
            Commands::Assumptions(a) => {
                let format = if a.json {
                    OutputFormat::Json
                } else {
                    OutputFormat::Full
                };
                (format, project(&a.paths, None))
            }
            Commands::DumpRecommendFacts(a) => (
                OutputFormat::Full,
                project(&a.paths, a.analysis.config.as_ref()),
            ),
            Commands::DumpOpenPremises(a) => (
                OutputFormat::Full,
                project(&a.paths, a.analysis.config.as_ref()),
            ),
            Commands::Rule(a) => (report(a.output_format), None),
            _ => (OutputFormat::Full, None),
        };
        Self { format, project }
    }

    /// Print why the command failed (it then exits with 2).
    pub(crate) fn fail(&self, err: &anyhow::Error) {
        let config_file = self.project.as_ref().and_then(|(paths, config)| {
            ProjectOptions {
                paths,
                config: config.as_deref(),
                ..ProjectOptions::default()
            }
            .config_file()
        });
        let problem = explain(err).locate(config_file.as_deref());
        if matches!(self.format, OutputFormat::Json) {
            let error = serde_json::json!({ "error": shown(&problem) });
            println!(
                "{}",
                serde_json::to_string_pretty(&error).expect("a problem serializes")
            );
        }
        print(self.format, &problem);
    }
}

/// The problem `err` describes: the explanation of the first error of its chain that has one,
/// else its message.
pub(crate) fn explain(err: &anyhow::Error) -> Problem {
    err.chain()
        .find_map(explained)
        .unwrap_or_else(|| Problem::new(format!("{err:#}")))
}

fn explained(cause: &(dyn std::error::Error + 'static)) -> Option<Problem> {
    macro_rules! first_of {
        ($($error:ty),* $(,)?) => {
            None$(.or_else(|| cause.downcast_ref::<$error>().map(Explain::explain)))*
        };
    }
    first_of!(
        Problem,
        ProjectError,
        ConfigError,
        FileError,
        heki_rules::SelectionError,
        heki_rules::RuleConfigError,
        heki_rules::recommend::RecommendError,
        heki_rules::SnapshotError,
        heki_diagnostics::BaselineError,
        heki_kb::KbError,
        heki_lsp::EngineError,
    )
}

/// `problem` with its path relative to the working directory, as reports write paths.
fn shown(problem: &Problem) -> Problem {
    let mut problem = problem.clone();
    if let Some(at) = &mut problem.location {
        at.path = shown_path(&at.path);
    }
    problem
}

/// Print `problem` to stderr in the layout of a report. In `github` output the text is escaped
/// as in annotations, so text from the project never starts a line of its own.
pub(crate) fn print(format: OutputFormat, problem: &Problem) {
    let mut problem = shown(problem);
    if matches!(format, OutputFormat::Github) {
        problem.message = github_escape(&problem.message);
        problem.help = problem.help.iter().map(|h| github_escape(h)).collect();
        if let Some(at) = &mut problem.location {
            at.path = PathBuf::from(github_escape(&path_text(&at.path)));
        }
    }
    eprint!(
        "{}",
        render_problem(&problem, &Stylesheet::plain(), &file_line)
    );
}

/// The problems of files that could not be read, parsed, or fixed.
pub(crate) fn of_files<'a>(errors: impl IntoIterator<Item = &'a FileError>) -> Vec<Problem> {
    errors.into_iter().map(Explain::explain).collect()
}

/// A file the command cannot write.
pub(crate) fn unwritable(path: &Path, cause: &std::io::Error) -> Problem {
    Problem::new(format!("cannot write the file: {cause}"))
        .at_file(path)
        .help("make the file and its directory writable, or name another path")
}
