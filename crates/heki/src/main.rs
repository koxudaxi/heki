//! `heki` CLI — Ruff & `t-linter`-aligned interface for Python Effect & Alias semantic checking.

/// `eprintln!` through [`check::log_line`] for the output format of the run.
macro_rules! log {
    ($format:expr, $($arg:tt)*) => {
        $crate::check::log_line($format, &format!($($arg)*))
    };
}

mod check;
mod dump_facts;
mod dump_recommend_facts;
mod failure;
mod help;
mod inspect;
mod open_premises;
mod prompt;
mod recommend;
mod skill;
mod version;
mod watch;

use anyhow::Result;
use clap::parser::ValueSource;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use heki_ast::LineIndex;
use heki_env::{Docs, Problem};
use heki_rules::experimental::{
    EXPERIMENTAL_FEATURES, EXPERIMENTAL_POLICY, ExperimentalFeature, experimental_rule,
};
use heki_rules::precision::{self, Certification, Measurement};
use heki_rules::project::{FileError, Project, ProjectOptions};
use heki_rules::{Activation, CORE_RULES, RuleMeta, find_rule};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "heki",
    version,
    about = "Experimental static checker for Python that tracks side effects and mutations across function calls",
    after_long_help = help::HEKI.render()
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

/// `--color`: when `heki check` colors what it prints to stdout (Ruff's `--color`).
#[derive(Debug, Clone, Copy, ValueEnum)]
enum ColorChoice {
    Auto,
    Always,
    Never,
}

/// `--output-format-json-schema`: the JSON output whose JSON Schema `heki check` prints.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum JsonSchemaTarget {
    GeneratePrompt,
}

/// `--fail-on`: the least severity whose reports make `heki check` fail.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum FailOn {
    Error,
    Warning,
}

impl From<FailOn> for heki_env::Severity {
    fn from(f: FailOn) -> Self {
        match f {
            FailOn::Error => Self::Error,
            FailOn::Warning => Self::Warning,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run semantic checks across Python files or directories (Ruff-aligned).
    #[command(after_long_help = help::CHECK.render())]
    Check(CheckArgs),
    /// Start the incremental Language Server Protocol (LSP) server.
    #[command(visible_alias = "lsp", after_long_help = help::SERVER.render())]
    Server(ServerArgs),
    /// Explain a rule, its Proof Obligations, and counterexamples (`ruff rule` style).
    #[command(visible_alias = "explain", after_long_help = help::RULE.render())]
    Rule(RuleArgs),
    /// Manage interprocedural effect summary snapshots (`MUT103`).
    #[command(after_long_help = help::SNAPSHOT.render())]
    Snapshot(SnapshotArgs),
    /// List Assumption A1 hazards (`eval`, `exec`, `locals()`, frame introspection, functions too
    /// large to model) in a project.
    #[command(after_long_help = help::ASSUMPTIONS.render())]
    Assumptions(AssumptionsArgs),
    /// Experimental: what heki knows about one function (its effects, what it changes, keeps, and
    /// returns, whether it is pure, its callers, reports, and contracts).
    #[command(after_long_help = help::INSPECT.render())]
    Inspect(InspectArgs),
    /// Experimental: advice on the configuration of a project, with counts from its analysis, and
    /// the keys to add (it writes no file).
    #[command(after_long_help = help::RECOMMEND.render())]
    Recommend(RecommendArgs),
    /// Experimental: install heki's Agent Skill where a coding agent reads skills.
    Skill(SkillArgs),
    /// Print the version of this heki, and with `--changelog` what this release changed.
    #[command(after_long_help = help::VERSION.render())]
    Version(VersionArgs),
    /// Internal/testing: dump `heki_semantic` scope tree as JSON for CPython `symtable` verification.
    #[command(hide = true)]
    DumpSemantic(DumpSemanticArgs),
    /// Internal/testing: dump `heki_ir` SSA-CFG IR as JSON for debugging and verification.
    #[command(hide = true)]
    DumpIr(DumpSemanticArgs),
    /// Internal/testing: dump the path and loop facts of `heki_ir::facts` as JSON.
    #[command(hide = true)]
    DumpFacts(DumpSemanticArgs),
    /// Internal/testing: dump the facts `heki recommend` reads besides the rules as JSON.
    #[command(hide = true)]
    DumpRecommendFacts(DumpRecommendFactsArgs),
    /// Internal: dump the experimental features as JSON for `scripts/build_docs.py`.
    #[command(hide = true)]
    DumpExperimental,
    /// Internal/testing: dump the reports of `heki check`, with the premises each heuristic
    /// report of an exact claim left open, as JSON.
    #[command(hide = true)]
    DumpOpenPremises(DumpOpenPremisesArgs),
    /// Internal: dump the certifications and measurements as JSON for `scripts/build_docs.py`.
    #[command(hide = true)]
    DumpPrecision,
}

#[derive(Debug, Args)]
struct CheckArgs {
    /// Directories to analyze, and files to report on: a file is checked in its project, as
    /// `--touched` does from the project root (with directories, the directories are analyzed).
    /// A file outside any project is analyzed alone.
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
    /// Show and fail on only the diagnostics in one of these files or with a hop of their call
    /// chain (`related`) in one, such as the files a commit or a pull request changed, and let
    /// `--fix` and `--diff` rewrite only these files; the analysis still covers PATHS (put PATHS
    /// before the option). A path that is not an analyzed file is named in a warning.
    #[arg(
        long,
        value_name = "FILE",
        num_args = 1..,
        conflicts_with_all = ["watch", "update_baseline"]
    )]
    touched: Vec<PathBuf>,
    #[command(flatten)]
    analysis: AnalysisArgs,
    /// Exit with code 1 on reports of this severity or higher: `error` (the default), or
    /// `warning` to fail on warnings too (`lint.fail-on`).
    #[arg(long, value_enum)]
    fail_on: Option<FailOn>,
    /// Output format: `full`, or `github` in GitHub Actions (`GITHUB_ACTIONS=true`);
    /// `HEKI_OUTPUT_FORMAT` sets it when the option is not given.
    #[arg(
        long,
        value_enum,
        env = "HEKI_OUTPUT_FORMAT",
        hide_env = true,
        default_value_t = OutputFormat::Full,
        hide_default_value = true
    )]
    output_format: OutputFormat,
    /// When to color the `full` and `concise` output and `--diff`, as Ruff does: `auto` colors
    /// them on a terminal unless `NO_COLOR` is set (`FORCE_COLOR` or `CLICOLOR_FORCE` color them
    /// anywhere), `always`, or `never`.
    #[arg(long, value_enum, value_name = "WHEN", default_value_t = ColorChoice::Auto)]
    color: ColorChoice,
    /// Generate or shrink-update the Ratchet baseline file, from directories: the entries of
    /// files the paths leave out stay as they are.
    #[arg(long)]
    update_baseline: bool,
    /// Experimental: offline profile dump (collapsed stacks of CPython 3.15's profiler or py-spy, or `--gecko`
    /// JSON): apply the `size-dependent` fixes in its hot functions (in every function with
    /// `--include-size-dependent`).
    #[arg(long)]
    profile: Option<PathBuf>,
    /// Watch the directories given for file changes and re-run incrementally.
    #[arg(long)]
    watch: bool,
    /// Experimental: print a prompt for consulting a language model about this run instead of
    /// the report: the configuration, the rules that apply, the diagnostics, how to answer, and
    /// QUESTION when given (put PATHS before the option). `--output-format json` prints it as
    /// JSON. Writes no file and exits with 0.
    #[arg(
        long,
        value_name = "QUESTION",
        num_args = 0..=1,
        default_missing_value = "",
        conflicts_with_all = ["fix", "diff", "watch", "update_baseline", "profile", "include_size_dependent"]
    )]
    generate_prompt: Option<String>,
    /// Print the JSON Schema of a JSON output and exit: `generate-prompt`, the object of
    /// `--generate-prompt --output-format json`.
    #[arg(long, value_enum, value_name = "TARGET")]
    output_format_json_schema: Option<JsonSchemaTarget>,
    /// Internal: write the steps each owner's walks took per function to this JSON file
    /// (`scripts/check_walk_shapes.py`).
    #[arg(long, hide = true)]
    walk_steps: Option<PathBuf>,
}

/// The options that decide what `heki check` computes: the rule selection (whether typed writes
/// propagate), the configuration (the contracts), the target version, and the baseline (the
/// status of each report). Every command that reports from `heki check`'s analysis takes them.
#[derive(Debug, Args)]
struct AnalysisArgs {
    /// Comma-separated rule codes, prefixes, or slugs to enable.
    #[arg(long, value_delimiter = ',')]
    select: Vec<String>,
    /// Additional rules to enable on top of defaults/config.
    #[arg(long, value_delimiter = ',')]
    extend_select: Vec<String>,
    /// Comma-separated rule codes or slugs to ignore.
    #[arg(long, value_delimiter = ',')]
    ignore: Vec<String>,
    #[command(flatten)]
    heuristic: HeuristicArgs,
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
}

#[derive(Debug, Args)]
struct InspectArgs {
    /// A dotted name (`pkg.mod.Class.method`, a dotted suffix such as `Class.method`, or a
    /// glob), `path.py:LINE`, or `path.py::NAME`. `-` reads such symbols from standard input,
    /// one per line, and reports on every function they name, a glob on each function it
    /// matches, from one analysis: a JSON array, or one text section per function.
    symbol: String,
    /// The project to analyze, as for `heki check`.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,
    /// `text`, or `json`.
    #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
    output_format: ReportFormat,
    /// List the callers transitively, and the reports above the function.
    #[arg(long)]
    callers: bool,
    /// How many caller levels `--callers` lists.
    #[arg(long, requires = "callers", default_value_t = 3, value_parser = clap::value_parser!(u16).range(1..=100))]
    depth: u16,
    /// Show the imports of the project modules SYMBOL names (a module name or a glob over
    /// module names): each import with when it runs and what it loads, and what loading the
    /// module loads while it loads.
    #[arg(long, conflicts_with = "callers")]
    imports: bool,
    #[command(flatten)]
    analysis: AnalysisArgs,
    /// Internal: write the steps each owner's walks took to this JSON file
    /// (`crates/heki/tests/e2e_analysis_bounds.rs`).
    #[arg(long, hide = true)]
    walk_steps: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct RecommendArgs {
    /// The project to analyze, as for `heki check`.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,
    /// `text`, or `json` (one object, described by the JSON Schema its `$schema` names).
    #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
    output_format: ReportFormat,
    /// Print the JSON Schema of the object of `--output-format json` and exit.
    #[arg(long)]
    output_format_json_schema: bool,
    #[command(flatten)]
    analysis: AnalysisArgs,
}

#[derive(Debug, Args)]
struct ServerArgs {
    /// Use stdio transport (default LSP mode).
    #[arg(long, default_value_t = true)]
    stdio: bool,
    #[command(flatten)]
    heuristic: HeuristicArgs,
}

/// `--heuristic` / `--no-heuristic`, over `lint.heuristic`; the last one given wins.
#[derive(Debug, Args)]
struct HeuristicArgs {
    /// Also run the heuristic rules, whose reports can be wrong about the code, by default and
    /// for every prefix and `ALL` (`lint.heuristic = true`).
    #[arg(long, overrides_with = "no_heuristic")]
    heuristic: bool,
    /// Run a heuristic rule only when its code or name is selected (`lint.heuristic = false`).
    #[arg(long, overrides_with = "heuristic")]
    no_heuristic: bool,
}

impl HeuristicArgs {
    /// `None`: the configuration decides.
    fn value(&self) -> Option<bool> {
        match (self.heuristic, self.no_heuristic) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        }
    }
}

#[derive(Debug, Args)]
struct RuleArgs {
    /// Rule code (`EFF101`) or slug (`async-blocking-call`), or `--all` to list all rules.
    rule: Option<String>,
    #[arg(long)]
    all: bool,
    /// `text`, or `json` with each rule's metadata and documentation.
    #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
    output_format: ReportFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ReportFormat {
    Text,
    Json,
}

/// A rule as `heki rule --output-format json` prints it.
#[derive(serde::Serialize)]
struct RuleRecord {
    #[serde(flatten)]
    rule: &'static RuleMeta,
    /// Whether the rule runs with no configuration.
    default_enabled: bool,
    heuristic_note: Option<String>,
    experimental: Option<&'static ExperimentalFeature>,
    default_severity: heki_env::Severity,
    /// The premises of the rule's exact claim (`heki_rules::premises`), empty for a rule
    /// without one.
    premises: &'static [&'static heki_rules::premises::Premise],
    certification: Option<Certification>,
    /// The latest measurement of how often its reports are right about the code.
    measurement: Option<Measurement>,
}

impl RuleRecord {
    fn new(rule: &'static RuleMeta) -> Self {
        Self {
            rule,
            default_enabled: rule.runs_unnamed(false),
            heuristic_note: rule.heuristic_note(),
            experimental: experimental_rule(rule.code),
            default_severity: rule.default_severity(),
            premises: heki_rules::premises::of_rule(rule.code),
            certification: precision::certification(rule.code),
            measurement: precision::measurement(rule.code),
        }
    }
}

/// When a rule runs with no configuration, as `heki rule --all` lists it: `off` for a heuristic
/// rule that would be on once exact.
fn activation_word(rule: &RuleMeta) -> &'static str {
    match rule.activation {
        Activation::Default if rule.runs_unnamed(false) => "on",
        Activation::Default => "off",
        Activation::OptIn => "opt-in",
        Activation::Contract => "contract",
    }
}

#[derive(Debug, Args)]
struct SnapshotArgs {
    /// Directories to summarize; a file names its project, as for `heki check`.
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
struct DumpRecommendFactsArgs {
    /// The project to analyze, as for `heki check`.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,
    /// Print only the counts per project.
    #[arg(long)]
    totals: bool,
    #[command(flatten)]
    analysis: AnalysisArgs,
}

#[derive(Debug, Args)]
struct DumpSemanticArgs {
    /// Python source file to parse and dump semantic scopes for.
    file: PathBuf,
    #[arg(long, default_value = "py315")]
    target_version: String,
}

#[derive(Debug, Args)]
struct VersionArgs {
    /// Also print this release's section of the changelog, the one the binary was built with,
    /// without a network.
    #[arg(long)]
    changelog: bool,
}

#[derive(Debug, Args)]
struct SkillArgs {
    #[command(subcommand)]
    command: SkillCommand,
}

#[derive(Debug, Subcommand)]
enum SkillCommand {
    /// Copy the Agent Skill this heki was built with into the coding agent's skills directory.
    #[command(after_long_help = help::SKILL_INSTALL.render())]
    Install(SkillInstallArgs),
}

#[derive(Debug, Args)]
struct SkillInstallArgs {
    /// The coding agent: Claude Code reads `.claude/skills/`, Codex `.agents/skills/`.
    #[arg(long, value_enum)]
    agent: SkillAgent,
    /// `project`: under the working directory; `user`: under the home directory, for every
    /// project.
    #[arg(long, value_enum, default_value_t = SkillScope::Project)]
    scope: SkillScope,
    /// Replace the skill already installed there.
    #[arg(long)]
    overwrite: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SkillAgent {
    ClaudeCode,
    Codex,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SkillScope {
    Project,
    User,
}

#[derive(Debug, Args)]
struct DumpOpenPremisesArgs {
    /// The project to analyze, as for `heki check`.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,
    #[command(flatten)]
    analysis: AnalysisArgs,
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Commands::Check(args) => check::run(&args),
        Commands::Server(args) => {
            let selectors = heki_lsp::Selectors {
                heuristic: args.heuristic.value(),
                ..heki_lsp::Selectors::default()
            };
            Ok(if heki_lsp::run_stdio(selectors)? {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Commands::Rule(args) => {
            let json = args.output_format == ReportFormat::Json;
            let Some(q) = args.rule.as_deref().filter(|_| !args.all) else {
                if json {
                    let records: Vec<_> = CORE_RULES.iter().map(RuleRecord::new).collect();
                    println!("{}", serde_json::to_string_pretty(&records)?);
                } else {
                    for r in CORE_RULES {
                        println!(
                            "{:<8} {:<34} [{:<9} {:<8} fix={:<5}] {}",
                            r.code,
                            r.slug,
                            r.certainty.word(),
                            activation_word(r),
                            r.auto_fix_default,
                            r.summary
                        );
                    }
                }
                return Ok(ExitCode::SUCCESS);
            };
            let Some(r) = find_rule(q) else {
                let problem = Problem::new(format!("unknown rule `{q}`")).docs(Docs::Rules);
                return Err(heki_rules::selection::unknown_rule(problem, q, false).into());
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&RuleRecord::new(r))?);
            } else {
                // `ruff rule` layout: the rule's Markdown page, with a heuristic rule's note and
                // an experimental rule's note before its documentation.
                let heuristic = r.heuristic_note().map(|n| format!("{n}\n\n"));
                let experimental = experimental_rule(r.code).map(|f| {
                    format!(
                        "This rule is experimental. {} {EXPERIMENTAL_POLICY}\n\n",
                        f.reason
                    )
                });
                let note = format!(
                    "{}{}",
                    heuristic.unwrap_or_default(),
                    experimental.unwrap_or_default()
                );
                print!(
                    "# {} ({})\n\n{}\n\n{note}{}",
                    r.slug, r.code, r.summary, r.doc
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Snapshot(args) => check::run_snapshot(&args),
        Commands::Assumptions(args) => run_assumptions(args),
        Commands::Inspect(args) => inspect::run(&args),
        Commands::Recommend(args) => recommend::run(&args),
        Commands::Skill(SkillArgs {
            command: SkillCommand::Install(args),
        }) => skill::install(&args),
        Commands::Version(args) => version::run(args.changelog),
        Commands::DumpSemantic(args) => {
            let (src, parsed) = parse_file(&args.file)?;
            let ver = heki_env::PythonVersion::parse_tag(&args.target_version)
                .unwrap_or(heki_env::PythonVersion::PY315);
            let model =
                heki_semantic::analyze_module(&parsed, &src, heki_env::RuntimeProfile::from(ver));
            println!("{}", serde_json::to_string_pretty(&model)?);
            Ok(ExitCode::SUCCESS)
        }
        Commands::DumpIr(args) => {
            let (src, parsed) = parse_file(&args.file)?;
            let ver = heki_env::PythonVersion::parse_tag(&args.target_version)
                .unwrap_or(heki_env::PythonVersion::PY315);
            let profile = heki_env::RuntimeProfile::from(ver);
            let model = heki_semantic::analyze_module(&parsed, &src, profile);
            let ir = heki_ir::build_module_ir(&parsed, &src, &model, profile);
            println!("{}", serde_json::to_string_pretty(&ir)?);
            Ok(ExitCode::SUCCESS)
        }
        Commands::DumpFacts(args) => {
            let (src, parsed) = parse_file(&args.file)?;
            let ver = heki_env::PythonVersion::parse_tag(&args.target_version)
                .unwrap_or(heki_env::PythonVersion::PY315);
            let profile = heki_env::RuntimeProfile::from(ver);
            let model = heki_semantic::analyze_module(&parsed, &src, profile);
            let ir = heki_ir::build_module_ir(&parsed, &src, &model, profile);
            let kb = heki_kb::EffectiveEcosystemKb::load_builtin(&[])?;
            let input = heki_effect::ModuleInput {
                module_name: args
                    .file
                    .file_stem()
                    .map_or_else(|| "m".to_owned(), |s| s.to_string_lossy().into_owned()),
                path: args.file.clone(),
                is_package: false,
                root_is_package: false,
                content_hash: String::new(),
                semantic: &model,
                ir: &ir,
            };
            let facts = dump_facts::module(&input, &src, &kb);
            println!("{}", serde_json::to_string_pretty(&facts)?);
            Ok(ExitCode::SUCCESS)
        }
        Commands::DumpRecommendFacts(args) => dump_recommend_facts::run(&args),
        Commands::DumpOpenPremises(args) => open_premises::run(&args),
        Commands::DumpExperimental => {
            let registry = serde_json::json!({
                "policy": EXPERIMENTAL_POLICY,
                "features": EXPERIMENTAL_FEATURES,
            });
            println!("{}", serde_json::to_string_pretty(&registry)?);
            Ok(ExitCode::SUCCESS)
        }
        Commands::DumpPrecision => {
            let published = serde_json::json!({
                "certifications": precision::certifications().collect::<Vec<_>>(),
                "measurements": precision::measurements().collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&published)?);
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The source and syntax tree of `file`, for the `dump-*` commands.
fn parse_file(file: &Path) -> Result<(String, heki_ast::ParsedModule)> {
    let src = heki_ast::read_source(file).map_err(|e| FileError::unreadable(file, &e))?;
    let parsed =
        heki_ast::parse_module(&src).map_err(|errs| FileError::syntax(file, &src, &errs))?;
    Ok((src, parsed))
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
        working_dir: None,
        target_version: args.target_version.as_deref(),
        overrides: None,
        removed: None,
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
        &failure::of_files(&project.errors),
        ExitCode::SUCCESS,
    )
}

/// The command line. Without `--output-format` and `HEKI_OUTPUT_FORMAT`, `heki check` writes
/// GitHub Actions annotations in GitHub Actions, as basedpyright does: its job logs show them, and
/// the pull request shows them at the lines they are about.
fn parse_cli() -> Cli {
    let matches = Cli::command().get_matches();
    let mut cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    if let Commands::Check(args) = &mut cli.command
        && let Some((_, check)) = matches.subcommand()
        && check.value_source("output_format") == Some(ValueSource::DefaultValue)
        && std::env::var_os("GITHUB_ACTIONS").is_some_and(|v| v == "true")
    {
        args.output_format = OutputFormat::Github;
    }
    cli
}

/// The memory gate's allocator (`count-alloc`): it counts the bytes heki asks for, which do not
/// depend on the system allocator's caching or on the machine's memory pressure.
#[cfg(feature = "count-alloc")]
#[global_allocator]
static ALLOC: cap::Cap<std::alloc::System> = cap::Cap::new(std::alloc::System, usize::MAX);

fn main() -> ExitCode {
    // One thread, the main one, runs the parallel work of a counting build: a second thread's
    // timing would move the peak between runs.
    #[cfg(feature = "count-alloc")]
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .use_current_thread()
        .build_global()
        .expect("the global thread pool is built before any parallel work");
    let cli = parse_cli();
    let failures = failure::Failures::of(&cli.command);
    let code = match run(cli) {
        Ok(code) => code,
        Err(err) => {
            failures.fail(&err);
            ExitCode::from(2)
        }
    };
    #[cfg(feature = "count-alloc")]
    eprintln!(
        "heki heap: peak {} bytes, total {} bytes",
        ALLOC.max_allocated(),
        ALLOC.total_allocated()
    );
    code
}

#[cfg(test)]
mod tests {
    use super::{Cli, activation_word};
    use clap::CommandFactory;
    use heki_rules::experimental::{EXPERIMENTAL_FEATURES, ExperimentalKind};
    use heki_rules::{Activation, CORE_RULES, Certainty, RuleMeta, find_rule};
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

    /// What an entry can name: the rule codes, `heki <command>`, `heki <command> --<option>`,
    /// and the configuration keys. Output fields are not listed yet: an entry of that kind fails until
    /// they are.
    fn existing_targets() -> BTreeSet<Target> {
        let mut out: BTreeSet<Target> = CORE_RULES
            .iter()
            .map(|r| (ExperimentalKind::Rule, r.code.to_owned()))
            .collect();
        for command in Cli::command().get_subcommands() {
            let name = format!("heki {}", command.get_name());
            out.insert((ExperimentalKind::Command, name));
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

    #[test]
    fn the_rule_listing_says_when_each_rule_runs_with_no_configuration() {
        let rule = |certainty, activation| RuleMeta {
            certainty,
            activation,
            ..*find_rule("EFF102").unwrap()
        };
        for (certainty, activation, word) in [
            (Certainty::Exact, Activation::Default, "on"),
            (Certainty::Heuristic, Activation::Default, "off"),
            (Certainty::Heuristic, Activation::OptIn, "opt-in"),
            (Certainty::Heuristic, Activation::Contract, "contract"),
        ] {
            assert_eq!(activation_word(&rule(certainty, activation)), word);
        }
    }
}
