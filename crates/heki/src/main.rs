//! `heki` CLI — Ruff & `t-linter`-aligned interface for Python Effect & Alias semantic checking.

mod check;
mod watch;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use heki_ast::LineIndex;
use heki_env::HekiConfig;
use heki_rules::{CORE_RULES, find_rule};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "heki",
    version,
    about = "Effect & Alias semantic checker for Python (the 3rd layer alongside Ruff and ty)"
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
    /// List Assumption A1/A2 violations across a project.
    Assumptions(AssumptionsArgs),
    /// Internal/testing: dump `heki_semantic` scope tree as JSON for CPython `symtable` verification.
    #[command(hide = true)]
    DumpSemantic(DumpSemanticArgs),
    /// Internal/testing: dump `heki_ir` SSA-CFG IR as JSON for debugging and verification.
    #[command(hide = true)]
    DumpIr(DumpSemanticArgs),
}

#[derive(Debug, Args)]
struct CheckArgs {
    /// Files or directories to check.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,
    /// Apply `safe` + `unconditional` fixes (`OPT101`/`OPT102`).
    #[arg(long)]
    fix: bool,
    /// Also allow `size-dependent` `safe` fixes (or combine with `--profile`).
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
    /// Explicit configuration file (`heki.toml` or `pyproject.toml`).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Target Python version (`py312`, `py313`, `py314`, `py315`).
    #[arg(long)]
    target_version: Option<String>,
    /// Ratchet baseline TOML file (line-number-independent).
    #[arg(long)]
    baseline: Option<PathBuf>,
    /// Generate or shrink-update the Ratchet baseline file.
    #[arg(long)]
    update_baseline: bool,
    /// Optional offline profile dump (`py-spy --collapsed` / Tachyon `--gecko`).
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
}

#[derive(Debug, Args)]
struct SnapshotArgs {
    /// Files or directories to summarize.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,
    /// Write `.heki-summary-snapshot.json` at the project root (default: print it).
    #[arg(long)]
    write: bool,
    /// Explicit configuration file (`heki.toml` or `pyproject.toml`).
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
        Commands::Server(_args) => {
            heki_lsp::run_stdio()?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Rule(args) => {
            if args.all || args.rule.is_none() {
                for r in CORE_RULES {
                    println!(
                        "{:<8} {:<34} [default={:<5} fix={:<5}] {}",
                        r.code, r.slug, r.default_enabled, r.auto_fix_default, r.summary
                    );
                }
                return Ok(ExitCode::SUCCESS);
            }
            let q = args.rule.unwrap();
            match find_rule(&q) {
                Some(r) => {
                    println!("{} ({})\n{}", r.code, r.slug, r.summary);
                    Ok(ExitCode::SUCCESS)
                }
                None => {
                    eprintln!("unknown rule: {q}");
                    Ok(ExitCode::from(2))
                }
            }
        }
        Commands::Snapshot(args) => check::run_snapshot(&args),
        Commands::Assumptions(args) => run_assumptions(args),
        Commands::DumpSemantic(args) => {
            let src = std::fs::read_to_string(&args.file)?;
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
            let src = std::fs::read_to_string(&args.file)?;
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
    }
}

fn collect_python_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for p in paths {
        if p.is_file() {
            if p.extension().is_some_and(|e| e == "py") {
                files.push(p.clone());
            }
            continue;
        }
        if !p.exists() {
            continue;
        }
        let mut builder = ignore::WalkBuilder::new(p);
        builder.add_custom_ignore_filename(".hekiignore");
        for entry in builder.build().flatten() {
            let ep = entry.path();
            if ep.is_file() && ep.extension().is_some_and(|e| e == "py") {
                files.push(ep.to_path_buf());
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

#[derive(serde::Serialize)]
struct AssumptionRecord {
    path: String,
    line: u32,
    column: u32,
    qualname: String,
    kind: String,
}

fn run_assumptions(args: AssumptionsArgs) -> Result<ExitCode> {
    // `paths` defaults to `.`.
    let root = args.paths.first().map_or(Path::new("."), PathBuf::as_path);
    let cfg = HekiConfig::load_from_dir(if root.is_dir() {
        root
    } else {
        root.parent().unwrap_or(Path::new("."))
    })?;
    let profile = args
        .target_version
        .as_deref()
        .and_then(heki_env::PythonVersion::parse_tag)
        .map(heki_env::RuntimeProfile::from)
        .unwrap_or_else(|| cfg.runtime_profile());

    let files = collect_python_files(&args.paths);
    let per_file: Vec<(usize, Vec<AssumptionRecord>, Option<String>)> = files
        .par_iter()
        .map(|path| {
            let src = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(e) => return (0, Vec::new(), Some(format!("{}: {e}", path.display()))),
            };
            let parsed = match heki_ast::parse_module(&src) {
                Ok(p) => p,
                Err(errs) => {
                    return (
                        0,
                        Vec::new(),
                        Some(format!(
                            "{}: {} syntax error(s)",
                            path.display(),
                            errs.len()
                        )),
                    );
                }
            };
            let semantic = heki_semantic::analyze_module(&parsed, &src, profile);
            let ir = heki_ir::build_module_ir(&parsed, &src, &semantic, profile);
            let lines = LineIndex::from_source(&src);
            let fn_count = ir.functions.len();
            let mut records = Vec::new();
            for (func, hazard) in ir.all_a1_hazards() {
                let (line, column) = lines.line_col(hazard.span.start);
                records.push(AssumptionRecord {
                    path: path.display().to_string(),
                    line,
                    column,
                    qualname: func.qualname.clone(),
                    kind: hazard.kind.clone(),
                });
            }
            (fn_count, records, None)
        })
        .collect();

    let mut total_fns = 0usize;
    let mut all_records = Vec::new();
    let mut errors = Vec::new();
    for (fns, recs, err) in per_file {
        total_fns += fns;
        all_records.extend(recs);
        if let Some(e) = err {
            errors.push(e);
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
            files.len(),
            total_fns,
            all_records.len(),
            errors.len()
        );
    }
    for e in &errors {
        eprintln!("warning: {e}");
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("heki error: {err:#}");
            ExitCode::from(2)
        }
    }
}
