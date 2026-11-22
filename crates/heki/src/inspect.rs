//! `heki inspect <SYMBOL> [PATHS]...`: the report of one function, from the analysis `heki
//! check` runs for the same configuration and selection. Exit codes: 0 inspected, 2 when the
//! symbol names no single function or a file could not be analyzed (the report is still
//! printed).

use crate::check::{baseline_path, load_and_index, shown_errors};
use crate::{InspectArgs, OutputFormat, ReportFormat};
use anyhow::Result;
use heki_diagnostics::{Diagnostic, RatchetBaseline, path_text};
use heki_effect::{FuncKey, ProjectDecls};
use heki_rules::inspect::{
    InspectInputs, InspectQuery, MAX_CANDIDATES, Resolution, Symbol, inspect, render_text,
    resolve_line, resolve_name,
};
use heki_rules::project::{Facts, Indexed, ParsedFile, canonical, regressed};
use rustc_hash::FxHashMap;
use serde_json::json;
use std::path::Path;
use std::process::ExitCode;

/// Why the symbol names no single function, as the JSON error object says it.
struct Failure {
    kind: &'static str,
    message: String,
    /// The functions to choose from (`candidates`, `suggestions`, `methods`, `functions`).
    listed: Option<(&'static str, Vec<FuncKey>)>,
}

pub(crate) fn run(args: &InspectArgs) -> Result<ExitCode> {
    let json = args.output_format == ReportFormat::Json;
    let symbol = match Symbol::parse(&args.symbol) {
        Ok(s) => s,
        Err(e) => {
            let failure = Failure {
                kind: "invalid",
                message: e.to_string(),
                listed: None,
            };
            return fail(args, json, &failure, None, &[]);
        }
    };
    let indexed = load_and_index(
        &args.paths,
        &args.analysis,
        OutputFormat::Full,
        None,
        Facts::Inspect,
    )?;
    let errors = shown_errors(&indexed.project.errors);
    let decls = indexed.index.project_decls();
    let resolution = match resolve(&indexed, &decls, &symbol) {
        Ok(r) => r,
        Err(failure) => return fail(args, json, &failure, Some(&indexed), &errors),
    };
    let failure = match resolution {
        Resolution::Function { key, from } => {
            let report = build(args, &indexed, &decls, &key, from, errors.clone())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", render_text(&report));
            }
            return crate::check::finish(OutputFormat::Full, &errors, ExitCode::SUCCESS);
        }
        Resolution::Ambiguous(c) => Failure {
            kind: "ambiguous",
            message: format!(
                "ambiguous symbol `{}`: {} functions match",
                args.symbol,
                c.len()
            ),
            listed: Some(("candidates", c)),
        },
        Resolution::Class(class, methods) => Failure {
            kind: "class",
            message: format!("`{class}` is a class: inspect one of its methods"),
            listed: Some(("methods", methods)),
        },
        Resolution::Module(module, functions) => Failure {
            kind: "module",
            message: format!("`{module}` is a module: inspect one of its functions"),
            listed: Some(("functions", functions)),
        },
        Resolution::Unknown(suggestions) => Failure {
            kind: "unknown",
            message: format!("no function of the project is named `{}`", args.symbol),
            listed: Some(("suggestions", suggestions)),
        },
    };
    fail(args, json, &failure, Some(&indexed), &errors)
}

/// The file of the project `path` names (as given, relative to the working directory).
fn project_file<'p>(indexed: &'p Indexed, path: &Path) -> Option<&'p ParsedFile> {
    let wanted = canonical(path).ok()?;
    indexed
        .project
        .files
        .iter()
        .find(|f| canonical(&f.path).is_ok_and(|p| p == wanted))
}

fn outside(path: &Path) -> Failure {
    Failure {
        kind: "outside_project",
        message: format!(
            "`{}` is not a Python file of the project (outside it, excluded, or not found)",
            path_text(path)
        ),
        listed: None,
    }
}

fn resolve(
    indexed: &Indexed,
    decls: &ProjectDecls,
    symbol: &Symbol,
) -> std::result::Result<Resolution, Failure> {
    let index = &indexed.index;
    Ok(match symbol {
        Symbol::Name(text) => resolve_name(index, decls, text, None),
        Symbol::InFile { path, name } => {
            let file = project_file(indexed, path).ok_or_else(|| outside(path))?;
            resolve_name(index, decls, name, Some(&file.module_name))
        }
        Symbol::Line { path, line } => {
            let file = project_file(indexed, path).ok_or_else(|| outside(path))?;
            Resolution::Function {
                key: resolve_line(&file.unit(), *line),
                from: Some("line"),
            }
        }
    })
}

/// Print why the symbol names no function and exit with 2.
fn fail(
    args: &InspectArgs,
    json: bool,
    failure: &Failure,
    indexed: Option<&Indexed>,
    errors: &[String],
) -> Result<ExitCode> {
    let listed: Vec<serde_json::Value> = failure
        .listed
        .iter()
        .flat_map(|(_, keys)| keys.iter().take(MAX_CANDIDATES))
        .map(|k| candidate(indexed, k))
        .collect();
    if json {
        let mut error = json!({ "kind": failure.kind, "message": failure.message });
        if let Some((name, keys)) = &failure.listed {
            error[*name] = json!(listed);
            error["truncated"] = json!(keys.len() > MAX_CANDIDATES);
        }
        if failure.kind == "unknown" {
            error["errors"] = json!(errors);
        }
        let out = json!({ "version": heki_rules::inspect::INSPECT_VERSION, "query": args.symbol, "error": error });
        println!("{}", serde_json::to_string_pretty(&out)?);
    }
    eprintln!("heki: {}", failure.message);
    for c in &listed {
        eprintln!(
            "  {}  {}:{}",
            c["fqn"].as_str().unwrap_or_default(),
            c["path"].as_str().unwrap_or_default(),
            c["line"]
        );
    }
    for e in errors {
        eprintln!("error: {e}");
    }
    Ok(ExitCode::from(2))
}

/// A function to choose from: its name, where it is defined, and its kind.
fn candidate(indexed: Option<&Indexed>, key: &FuncKey) -> serde_json::Value {
    let file = indexed.and_then(|i| {
        let path = i.index.file_of(key)?;
        i.project.files.iter().find(|f| f.path == path)
    });
    let line = file.and_then(|f| {
        let lines = heki_ast::LineIndex::from_source(&f.source);
        f.ir.functions
            .iter()
            .find(|g| heki_effect::func_qualname(g) == key.qualname)
            .map(|g| lines.line(g.span.start))
    });
    let kind = if key.qualname.contains('.') {
        "method"
    } else {
        "function"
    };
    json!({
        "fqn": key.fqn(),
        "path": file.map(|f| f.rel.clone()),
        "line": line,
        "kind": kind,
    })
}

/// The report on `key`, with the reports of its file (and with `--callers`, of its callers'
/// files) and their baseline status.
fn build(
    args: &InspectArgs,
    indexed: &Indexed,
    decls: &ProjectDecls,
    key: &FuncKey,
    from: Option<&'static str>,
    errors: Vec<String>,
) -> Result<heki_rules::inspect::InspectReport> {
    let project = &indexed.project;
    let index = &indexed.index;
    let env = &project.env;
    let by_path: FxHashMap<&Path, &ParsedFile> = project
        .files
        .iter()
        .map(|f| (f.path.as_path(), f))
        .collect();
    let target = index.file_of(key).and_then(|p| by_path.get(p).copied());
    let depth = if args.callers {
        usize::from(args.depth)
    } else {
        1
    };
    let mut files: Vec<&ParsedFile> = target.into_iter().collect();
    if args.callers {
        let mut calls = index.calls_by_caller();
        let found = index.callers_within(&mut calls, key, depth, heki_rules::inspect::MAX_CALLERS);
        for c in found.sites {
            if let Some(f) = by_path.get(c.path)
                && !files.iter().any(|g| g.path == f.path)
            {
                files.push(f);
            }
        }
    }
    let units: Vec<_> = files.iter().map(|f| f.unit()).collect();
    let (diags, _) = indexed.diagnose_units(&units, false)?;
    let reports = with_baseline(args, indexed, diags)?;
    let roles = |key| {
        let (cfg, kb, soundness) = (&env.cfg, &env.kb, &env.soundness_kb);
        heki_rules::contract_roles(cfg, kb, soundness, index, decls, &indexed.selection, key)
    };
    let contracts = roles(key)?;
    let target_unit = target.map(ParsedFile::unit);
    let rel_of = |p: &Path| {
        by_path
            .get(p)
            .map_or_else(|| path_text(p), |f| f.rel.clone())
    };
    let text_of = |p: &Path| by_path.get(p).map(|f| f.source.clone());
    let root = canonical(&env.root).unwrap_or_else(|_| env.root.clone());
    let inputs = InspectInputs {
        index,
        decls,
        kb: &env.kb,
        root: &root,
        ownership: heki_rules::declares_ownership(&env.cfg),
        unit: target_unit.as_ref(),
        rel_of: &rel_of,
        text_of: &text_of,
        reports,
        contracts,
        errors,
    };
    let query = InspectQuery {
        text: &args.symbol,
        resolved_from: from,
        callers: args.callers,
        depth,
    };
    Ok(inspect(&inputs, key, &query))
}

/// Each diagnostic with its status in the baseline `heki check` uses (`None` without one): a
/// report the baseline allows, or a regression.
fn with_baseline(
    args: &InspectArgs,
    indexed: &Indexed,
    diags: Vec<Diagnostic>,
) -> Result<Vec<(Diagnostic, Option<&'static str>)>> {
    let Some(path) = baseline_path(&args.analysis, &indexed.project.env) else {
        return Ok(diags.into_iter().map(|d| (d, None)).collect());
    };
    let baseline = RatchetBaseline::load(&path)?;
    let regressed = regressed(&baseline, &diags, &indexed.project.files);
    Ok(diags
        .into_iter()
        .zip(regressed)
        .map(|(d, regressed)| {
            let status = if regressed { "regression" } else { "allowed" };
            (d, Some(status))
        })
        .collect())
}
