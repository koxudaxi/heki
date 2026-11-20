//! `heki` CLI — Ruff & `t-linter`-aligned interface for Python Effect & Alias semantic checking.

/// `eprintln!` through [`check::log_line`] for the output format of the run.
macro_rules! log {
    ($format:expr, $($arg:tt)*) => {
        $crate::check::log_line($format, &format!($($arg)*))
    };
}

mod check;
mod watch;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use heki_ast::LineIndex;
use heki_rules::experimental::{
    EXPERIMENTAL_FEATURES, EXPERIMENTAL_POLICY, ExperimentalFeature, experimental_rule,
};
use heki_rules::project::{Project, ProjectOptions};
use heki_rules::{CORE_RULES, RuleMeta, find_rule};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "heki",
    version,
    about = "Experimental static checker for Python that tracks side effects and mutations across function calls"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Full,
    Concise,
    Json,
    Github,
    Sarif,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run semantic checks across Python files or directories (Ruff-aligned).
    Check(CheckArgs),
    /// Start the incremental Language Server Protocol (LSP) server.
    #[command(visible_alias = "lsp")]
    Server(ServerArgs),
    /// Explain a rule, its Proof Obligations, and counterexamples (`ruff rule` style).
    #[command(visible_alias = "explain")]
    Rule(RuleArgs),
    /// Manage interprocedural effect summary snapshots (`MUT103`).
    Snapshot(SnapshotArgs),
    /// List Assumption A1 hazards (`eval`, `exec`, `locals()`, frame introspection, functions too
    /// large to model) in a project.
    Assumptions(AssumptionsArgs),
    /// Internal/testing: dump `heki_semantic` scope tree as JSON for CPython `symtable` verification.
    #[command(hide = true)]
    DumpSemantic(DumpSemanticArgs),
    /// Internal/testing: dump `heki_ir` SSA-CFG IR as JSON for debugging and verification.
    #[command(hide = true)]
    DumpIr(DumpSemanticArgs),
    /// Internal: dump the experimental features as JSON for `scripts/build_docs.py`.
    #[command(hide = true)]
    DumpExperimental,
}

#[derive(Debug, Args)]
struct CheckArgs {
    /// Files or directories to check.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,
    /// Apply `safe` + `unconditional` fixes (`OPT101`/`OPT102`).
    #[arg(long)]
    fix: bool,
    /// Also apply the proven-safe `size-dependent` fixes, in every function.
    #[arg(long)]
    include_size_dependent: bool,
    /// Preview fixes as a unified diff without writing files.
    #[arg(long)]
    diff: bool,
    /// Comma-separated rule codes, prefixes, or slugs to enable.
    #[arg(long, value_delimiter = ',')]
    select: Vec<String>,
    /// Additional rules to enable on top of defaults/config.
    #[arg(long, value_delimiter = ',')]
    extend_select: Vec<String>,
    /// Comma-separated rule codes or slugs to ignore.
    #[arg(long, value_delimiter = ',')]
    ignore: Vec<String>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Full)]
    output_format: OutputFormat,
    /// Explicit configuration file: a `pyproject.toml` (its `[tool.heki]` table) or a file
    /// with the keys at its top level, like `heki.toml`.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Target Python version (`py311`, `py312`, `py313`, `py314`, `py315`).
    #[arg(long)]
    target_version: Option<String>,
    /// Ratchet baseline TOML file (line-number-independent).
    #[arg(long)]
    baseline: Option<PathBuf>,
    /// Generate or shrink-update the Ratchet baseline file.
    #[arg(long)]
    update_baseline: bool,
    /// Experimental: offline profile dump (collapsed stacks of CPython 3.15's profiler or py-spy, or `--gecko`
    /// JSON): apply the `size-dependent` fixes in its hot functions (in every function with
    /// `--include-size-dependent`).
    #[arg(long)]
    profile: Option<PathBuf>,
    /// Watch for file changes and re-run incrementally.
    #[arg(long)]
    watch: bool,
}

#[derive(Debug, Args)]
struct ServerArgs {
    /// Use stdio transport (default LSP mode).
    #[arg(long, default_value_t = true)]
    stdio: bool,
}

#[derive(Debug, Args)]
struct RuleArgs {
    /// Rule code (`EFF101`) or slug (`async-blocking-call`), or `--all` to list all rules.
    rule: Option<String>,
    #[arg(long)]
    all: bool,
    /// `text`, or `json` with each rule's metadata and documentation.
    #[arg(long, value_enum, default_value_t = RuleFormat::Text)]
    output_format: RuleFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum RuleFormat {
    Text,
    Json,
}

/// A rule as `heki rule --output-format json` prints it.
#[derive(serde::Serialize)]
struct RuleRecord {
    #[serde(flatten)]
    rule: &'static RuleMeta,
    experimental: Option<&'static ExperimentalFeature>,
    default_severity: heki_env::Severity,
}

impl RuleRecord {
    fn new(rule: &'static RuleMeta) -> Self {
        Self {
            rule,
            experimental: experimental_rule(rule.code),
            default_severity: rule.default_severity(),
        }
    }
}

#[derive(Debug, Args)]
struct SnapshotArgs {
    /// Files or directories to summarize.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,
    /// Write `.heki-summary-snapshot.json` at the project root (default: print it).
    #[arg(long)]
    write: bool,
    /// Explicit configuration file: a `pyproject.toml` (its `[tool.heki]` table) or a file
    /// with the keys at its top level, like `heki.toml`.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Target Python version (`py311`..`py315`).
    #[arg(long)]
    target_version: Option<String>,
}

#[derive(Debug, Args)]
struct AssumptionsArgs {
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,
    /// Target Python version (`py311`, `py312`, `py313`, `py314`, `py315`).
    #[arg(long)]
    target_version: Option<String>,
    /// Output machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct DumpSemanticArgs {
    /// Python source file to parse and dump semantic scopes for.
    file: PathBuf,
    #[arg(long, default_value = "py315")]
    target_version: String,
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Commands::Check(args) => check::run(&args),
        Commands::Server(_args) => Ok(if heki_lsp::run_stdio()? {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        }),
        Commands::Rule(args) => {
            let json = args.output_format == RuleFormat::Json;
            let Some(q) = args.rule.as_deref().filter(|_| !args.all) else {
                if json {
                    let records: Vec<_> = CORE_RULES.iter().map(RuleRecord::new).collect();
                    println!("{}", serde_json::to_string_pretty(&records)?);
                } else {
                    for r in CORE_RULES {
                        println!(
                            "{:<8} {:<34} [default={:<5} fix={:<5}] {}",
                            r.code, r.slug, r.default_enabled, r.auto_fix_default, r.summary
                        );
                    }
                }
                return Ok(ExitCode::SUCCESS);
            };
            let Some(r) = find_rule(q) else {
                eprintln!("unknown rule: {q}");
                return Ok(ExitCode::from(2));
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&RuleRecord::new(r))?);
            } else {
                // `ruff rule` layout: the rule's Markdown page, with an experimental rule's note
                // before its documentation.
                let note = experimental_rule(r.code)
                    .map(|f| {
                        format!(
                            "This rule is experimental. {} {EXPERIMENTAL_POLICY}\n\n",
                            f.reason
                        )
                    })
                    .unwrap_or_default();
                print!(
                    "# {} ({})\n\n{}\n\n{note}{}",
                    r.slug, r.code, r.summary, r.doc
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Snapshot(args) => check::run_snapshot(&args),
        Commands::Assumptions(args) => run_assumptions(args),
        Commands::DumpSemantic(args) => {
            let src = heki_ast::read_source(&args.file)?;
            let parsed = heki_ast::parse_module(&src)
                .map_err(|errs| anyhow::anyhow!("parse errors: {errs:?}"))?;
            let ver = heki_env::PythonVersion::parse_tag(&args.target_version)
                .unwrap_or(heki_env::PythonVersion::PY315);
            let model =
                heki_semantic::analyze_module(&parsed, &src, heki_env::RuntimeProfile::from(ver));
            println!("{}", serde_json::to_string_pretty(&model)?);
            Ok(ExitCode::SUCCESS)
        }
        Commands::DumpIr(args) => {
            let src = heki_ast::read_source(&args.file)?;
            let parsed = heki_ast::parse_module(&src)
                .map_err(|errs| anyhow::anyhow!("parse errors: {errs:?}"))?;
            let ver = heki_env::PythonVersion::parse_tag(&args.target_version)
                .unwrap_or(heki_env::PythonVersion::PY315);
            let profile = heki_env::RuntimeProfile::from(ver);
            let model = heki_semantic::analyze_module(&parsed, &src, profile);
            let ir = heki_ir::build_module_ir(&parsed, &src, &model, profile);
            println!("{}", serde_json::to_string_pretty(&ir)?);
            Ok(ExitCode::SUCCESS)
        }
        Commands::DumpExperimental => {
            let registry = serde_json::json!({
                "policy": EXPERIMENTAL_POLICY,
                "features": EXPERIMENTAL_FEATURES,
            });
            println!("{}", serde_json::to_string_pretty(&registry)?);
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[derive(serde::Serialize)]
struct AssumptionRecord {
    path: String,
    line: u32,
    column: u32,
    qualname: String,
    kind: String,
}

/// `heki assumptions`: the A1 hazards of the files `heki check` would check (the same
/// discovery, configuration, and target version), with the same exit code 2 for paths that do not
/// exist and files that cannot be read or parsed.
fn run_assumptions(args: AssumptionsArgs) -> Result<ExitCode> {
    let project = Project::load(&ProjectOptions {
        paths: &args.paths,
        config: None,
        target_version: args.target_version.as_deref(),
        overrides: None,
    })?;
    let mut total_fns = 0usize;
    let mut all_records = Vec::new();
    for file in &project.files {
        let lines = LineIndex::from_source(&file.source);
        total_fns += file.ir.functions.len();
        for (func, hazard) in file.ir.all_a1_hazards() {
            let (line, column) = lines.line_col(hazard.span.start);
            all_records.push(AssumptionRecord {
                path: check::shown_text(&file.path),
                line,
                column,
                qualname: func.qualname.clone(),
                kind: hazard.kind.clone(),
            });
        }
    }
    all_records.sort_by(|a, b| {
        (&a.path, a.line, a.column, &a.kind).cmp(&(&b.path, b.line, b.column, &b.kind))
    });

    if args.json {
        println!("{}", serde_json::to_string_pretty(&all_records)?);
    } else {
        for r in &all_records {
            println!(
                "{}:{}:{} [{}] A1 hazard: {}",
                r.path, r.line, r.column, r.qualname, r.kind
            );
        }
        eprintln!(
            "Checked {} file(s) ({} IR function(s)): {} A1 hazard(s), {} parse/read error(s).",
            project.files.len() + project.errors.len(),
            total_fns,
            all_records.len(),
            project.errors.len()
        );
    }
    check::finish(
        OutputFormat::Full,
        &check::shown_errors(&project.errors),
        ExitCode::SUCCESS,
    )
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let format = match &cli.command {
        Commands::Check(args) => args.output_format,
        _ => OutputFormat::Full,
    };
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            log!(format, "heki error: {err:#}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;
    use heki_rules::CORE_RULES;
    use heki_rules::experimental::{EXPERIMENTAL_FEATURES, ExperimentalKind};
    use std::collections::BTreeSet;

    type Target = (ExperimentalKind, String);

    /// Every key of `value` with its tables (`lint.select`); a list of tables has no default
    /// entry to take keys from.
    fn config_keys(value: &serde_json::Value, prefix: &str, out: &mut BTreeSet<Target>) {
        for (key, inner) in value.as_object().into_iter().flatten() {
            let path = format!("{prefix}{key}");
            config_keys(inner, &format!("{path}."), out);
            out.insert((ExperimentalKind::ConfigKey, path));
        }
    }

    /// What an entry can name: the rule codes, `heki <command> --<option>`, and the
    /// configuration keys. Output fields are not listed yet: an entry of that kind fails until
    /// they are.
    fn existing_targets() -> BTreeSet<Target> {
        let mut out: BTreeSet<Target> = CORE_RULES
            .iter()
            .map(|r| (ExperimentalKind::Rule, r.code.to_owned()))
            .collect();
        for command in Cli::command().get_subcommands() {
            for long in command.get_arguments().filter_map(|a| a.get_long()) {
                let option = format!("heki {} --{long}", command.get_name());
                out.insert((ExperimentalKind::CliOption, option));
            }
        }
        let config = serde_json::to_value(heki_env::HekiConfig::default()).unwrap();
        config_keys(&config, "", &mut out);
        out
    }

    #[test]
    fn experimental_entries_name_existing_rules_options_and_keys() {
        let existing = existing_targets();
        for key in ["lint.relax-async-in-tests", "write-owners"] {
            assert!(
                existing.contains(&(ExperimentalKind::ConfigKey, key.to_owned())),
                "{key}"
            );
        }
        // A behavior is described in words: there is no name to look up.
        for f in EXPERIMENTAL_FEATURES
            .iter()
            .filter(|f| f.kind != ExperimentalKind::Behavior)
        {
            assert!(
                existing.contains(&(f.kind, f.target.to_owned())),
                "{}: no {:?} `{}`",
                f.id,
                f.kind,
                f.target
            );
        }
    }
}
