//! `heki inspect <SYMBOL> [PATHS]...`: the report of one function, from the analysis `heki
//! check` runs for the same configuration and selection. With `-` for the symbol, the symbols
//! are read from standard input, one per line, and every function they name is reported on from
//! that one analysis, as an array. Exit codes: 0 inspected, 2 when a symbol names no single
//! function or a file could not be analyzed (the reports are still printed).

use crate::check::{baseline_path, load_and_index, shown_errors, warn_files_not_analyzed};
use crate::{InspectArgs, OutputFormat, ReportFormat};
use anyhow::{Context, Result};
use heki_diagnostics::{Diagnostic, RatchetBaseline, path_text};
use heki_effect::{FuncKey, ProjectDecls, ProjectIndex};
use heki_env::{Docs, Problem};
use heki_rules::import_view::{ImportsFailure, imports_report, render_imports_text};
use heki_rules::inspect::{
    Functions, GlobMatches, InspectInputs, InspectQuery, InspectReport, Inspector, MAX_CALLERS,
    MAX_CANDIDATES, Resolution, Symbol, render_text, resolve_line,
};
use heki_rules::project::{CommandPaths, Facts, Indexed, ParsedFile, canonical, regressed};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use serde_json::json;
use std::cell::OnceCell;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The symbol that reads the symbols from standard input.
const FROM_STDIN: &str = "-";

/// Why the symbol names no single function, as the JSON error object says it.
struct Failure {
    kind: &'static str,
    message: String,
    /// The functions to choose from (`candidates`, `suggestions`, `methods`, `functions`).
    listed: Option<(&'static str, Vec<FuncKey>)>,
}

impl Failure {
    /// The failure as the command line reports it, with what to do about it.
    fn problem(&self) -> Problem {
        let problem = Problem::new(self.message.clone()).docs(Docs::Inspect);
        let names = || {
            self.listed
                .iter()
                .flat_map(|(_, keys)| keys)
                .map(FuncKey::fqn)
                .collect()
        };
        match self.kind {
            "invalid" => problem.help(SYMBOL_FORMS),
            "outside_project" => problem.help(
                "name a Python file `heki check` checks, relative to the working directory, or pass its directory as a path",
            ),
            "unknown" => problem
                .did_you_mean(names())
                .help(SYMBOL_FORMS)
                .help("`heki inspect` takes a function of the checked files: add their directory to the paths if it is elsewhere"),
            _ => problem.help("inspect one of the functions listed below by its full name"),
        }
    }
}

const SYMBOL_FORMS: &str = "name a function as `module.function`, `module.Class.method`, `path.py::NAME`, or `path.py:LINE`";

/// The functions a symbol names, each with how the text named it, or why it names none.
type Outcome = std::result::Result<Vec<(FuncKey, Option<&'static str>)>, Failure>;

/// One element of the JSON output: a report, or the error object of a symbol.
#[derive(Serialize)]
#[serde(untagged)]
enum Entry {
    Report(Box<InspectReport>),
    Error(serde_json::Value),
}

pub(crate) fn run(args: &InspectArgs) -> Result<ExitCode> {
    crate::check::recording_steps(args.walk_steps.as_deref(), || {
        if args.imports {
            run_imports(args)
        } else {
            run_function(args)
        }
    })
}

/// `--imports`: the import facts of the modules the symbol names. Exit codes as for a
/// function: 2 when it names no module of the project, or a file could not be analyzed.
fn run_imports(args: &InspectArgs) -> Result<ExitCode> {
    let json = args.output_format == ReportFormat::Json;
    let paths = CommandPaths::of(&args.paths);
    let indexed = load_and_index(
        &paths.analyzed,
        &args.analysis,
        OutputFormat::Full,
        None,
        Facts::Imports,
    )?;
    warn_files_not_analyzed(&paths, &indexed.project, OutputFormat::Full);
    let errors = shown_errors(&indexed.project.errors);
    let problems = crate::failure::of_files(&indexed.project.errors);
    let report = match imports_report(&indexed, &args.symbol) {
        Ok(report) => report,
        Err(failure) => {
            let kind = match failure {
                ImportsFailure::Invalid(_) => "invalid",
                ImportsFailure::Unknown(_) => "unknown",
            };
            let failure = Failure {
                kind,
                message: failure.to_string(),
                listed: None,
            };
            if json {
                let error = error_object(&args.symbol, &failure, &[], &errors);
                println!("{}", serde_json::to_string_pretty(&error)?);
            }
            print_failure(&failure, &[]);
            return crate::check::finish(OutputFormat::Full, &problems, ExitCode::from(2));
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_imports_text(&report));
    }
    crate::check::finish(OutputFormat::Full, &problems, ExitCode::SUCCESS)
}

fn run_function(args: &InspectArgs) -> Result<ExitCode> {
    let each = args.symbol == FROM_STDIN;
    let queries = if each {
        symbols_from_stdin()?
    } else {
        vec![args.symbol.clone()]
    };
    let symbols: Vec<_> = queries.iter().map(|q| Symbol::parse(q)).collect();
    // A symbol that does not parse names nothing to analyze the project for.
    let indexed = symbols
        .iter()
        .any(std::result::Result::is_ok)
        .then(|| {
            let format = OutputFormat::Full;
            let paths = CommandPaths::of(&args.paths);
            let indexed = load_and_index(
                &paths.analyzed,
                &args.analysis,
                format,
                None,
                Facts::Inspect,
            )?;
            warn_files_not_analyzed(&paths, &indexed.project, format);
            Ok::<_, anyhow::Error>(indexed)
        })
        .transpose()?;
    let (errors, problems) = indexed.as_ref().map_or_else(Default::default, |i| {
        let errors = &i.project.errors;
        (shown_errors(errors), crate::failure::of_files(errors))
    });
    let decls = indexed.as_ref().map(|i| i.index.project_decls());
    let files = indexed.as_ref().map(Files::new);
    let functions = indexed
        .as_ref()
        .zip(decls.as_ref())
        .map(|(i, decls)| Functions::new(&i.index, decls));
    let globs = if each {
        GlobMatches::Each
    } else {
        GlobMatches::One
    };
    let outcomes: Vec<Outcome> = queries
        .iter()
        .zip(symbols)
        .map(|(query, symbol)| {
            let symbol = symbol.map_err(|e| Failure {
                kind: "invalid",
                message: e.to_string(),
                listed: None,
            })?;
            let analyzed = files.as_ref().zip(functions.as_ref());
            let (files, functions) =
                analyzed.expect("the project is analyzed when a symbol parses");
            named(query, resolve(files, functions, &symbol, globs)?)
        })
        .collect();
    let asked: Vec<Asked<'_>> = queries
        .iter()
        .zip(&outcomes)
        .filter_map(|(query, outcome)| Some((query, outcome.as_ref().ok()?)))
        .flat_map(|(query, keys)| {
            keys.iter()
                .map(move |(key, from)| (query.as_str(), key, *from))
        })
        .collect();
    let reports = match (&indexed, &decls, &files) {
        (Some(indexed), Some(decls), Some(files)) if !asked.is_empty() => {
            build(args, indexed, decls, files, &asked, &errors)?
        }
        _ => Vec::new(),
    };
    let failed = print(
        args,
        each,
        &queries,
        &outcomes,
        reports,
        files.as_ref(),
        &errors,
    );
    let code = if failed? {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    };
    crate::check::finish(OutputFormat::Full, &problems, code)
}

/// Print the report on each function the symbols name, in their order, and why each symbol
/// that names no single function does not: the JSON object (an array of them for `-`), or a
/// text section per function and the failures on stderr. Whether a symbol failed.
fn print(
    args: &InspectArgs,
    each: bool,
    queries: &[String],
    outcomes: &[Outcome],
    reports: Vec<InspectReport>,
    files: Option<&Files<'_>>,
    errors: &[String],
) -> Result<bool> {
    let json = args.output_format == ReportFormat::Json;
    let mut reports = reports.into_iter();
    let mut entries = Vec::new();
    let mut sections = false;
    let mut failed = false;
    for (query, outcome) in queries.iter().zip(outcomes) {
        match outcome {
            Ok(keys) => {
                for report in reports.by_ref().take(keys.len()) {
                    if json {
                        entries.push(Entry::Report(Box::new(report)));
                        continue;
                    }
                    if std::mem::replace(&mut sections, true) {
                        println!();
                    }
                    print!("{}", render_text(&report));
                }
            }
            Err(failure) => {
                failed = true;
                let listed = listed(files, failure);
                if json {
                    entries.push(Entry::Error(error_object(query, failure, &listed, errors)));
                }
                print_failure(failure, &listed);
            }
        }
    }
    if json {
        let out = if each {
            serde_json::to_string_pretty(&entries)?
        } else {
            serde_json::to_string_pretty(&entries[0])?
        };
        println!("{out}");
    }
    Ok(failed)
}

/// The symbols on standard input, one per line, blank lines left out.
fn symbols_from_stdin() -> Result<Vec<String>> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("could not read the symbols from standard input")?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
}

/// The files of the project by path, and by canonical path once a symbol names a file.
struct Files<'p> {
    index: &'p ProjectIndex,
    files: &'p [ParsedFile],
    by_path: FxHashMap<&'p Path, &'p ParsedFile>,
    by_canonical: OnceCell<FxHashMap<PathBuf, &'p ParsedFile>>,
}

impl<'p> Files<'p> {
    fn new(indexed: &'p Indexed) -> Self {
        let files = &indexed.project.files;
        Self {
            index: &indexed.index,
            files,
            by_path: files.iter().map(|f| (f.path.as_path(), f)).collect(),
            by_canonical: OnceCell::new(),
        }
    }

    /// The file of the project `path` names (as given, relative to the working directory).
    fn named(&self, path: &Path) -> Option<&'p ParsedFile> {
        let wanted = canonical(path).ok()?;
        let by_canonical = self.by_canonical.get_or_init(|| {
            let mut out = FxHashMap::default();
            for f in self.files {
                if let Ok(p) = canonical(&f.path) {
                    out.entry(p).or_insert(f);
                }
            }
            out
        });
        by_canonical.get(&wanted).copied()
    }

    /// The file of the project at `path`.
    fn at(&self, path: &Path) -> Option<&'p ParsedFile> {
        self.by_path.get(path).copied()
    }

    /// The file that defines `key`.
    fn of(&self, key: &FuncKey) -> Option<&'p ParsedFile> {
        self.at(self.index.file_of(key)?)
    }
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
    files: &Files<'_>,
    functions: &Functions<'_>,
    symbol: &Symbol,
    globs: GlobMatches,
) -> std::result::Result<Resolution, Failure> {
    Ok(match symbol {
        Symbol::Name(text) => functions.resolve_name(text, None, globs),
        Symbol::InFile { path, name } => {
            let file = files.named(path).ok_or_else(|| outside(path))?;
            functions.resolve_name(name, Some(&file.module_name), globs)
        }
        Symbol::Line { path, line } => {
            let file = files.named(path).ok_or_else(|| outside(path))?;
            Resolution::Function {
                key: resolve_line(&file.unit(), *line),
                from: Some("line"),
            }
        }
    })
}

/// The functions `query` names, or why it names no single function.
fn named(query: &str, resolution: Resolution) -> Outcome {
    Err(match resolution {
        Resolution::Function { key, from } => return Ok(vec![(key, from)]),
        Resolution::Functions { keys, from } => {
            return Ok(keys.into_iter().map(|k| (k, Some(from))).collect());
        }
        Resolution::Ambiguous(c) => Failure {
            kind: "ambiguous",
            message: format!("ambiguous symbol `{query}`: {} functions match", c.len()),
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
            message: format!("no function of the project is named `{query}`"),
            listed: Some(("suggestions", suggestions)),
        },
    })
}

/// The functions a failure lists, as many as it shows.
fn listed(files: Option<&Files<'_>>, failure: &Failure) -> Vec<serde_json::Value> {
    failure
        .listed
        .iter()
        .flat_map(|(_, keys)| keys.iter().take(MAX_CANDIDATES))
        .map(|k| candidate(files, k))
        .collect()
}

/// The JSON object of a symbol that names no single function.
fn error_object(
    query: &str,
    failure: &Failure,
    listed: &[serde_json::Value],
    errors: &[String],
) -> serde_json::Value {
    let problem = failure.problem();
    let mut error = json!({
        "kind": failure.kind,
        "message": failure.message,
        "help": problem.help,
        "docs": problem.docs,
    });
    if let Some((name, keys)) = &failure.listed {
        error[*name] = json!(listed);
        error["truncated"] = json!(keys.len() > MAX_CANDIDATES);
    }
    if failure.kind == "unknown" {
        error["errors"] = json!(errors);
    }
    json!({ "version": heki_rules::inspect::INSPECT_VERSION, "query": query, "error": error })
}

/// Print why a symbol names no single function, and the functions to choose from.
fn print_failure(failure: &Failure, listed: &[serde_json::Value]) {
    crate::failure::print(OutputFormat::Full, &failure.problem());
    for c in listed {
        eprintln!(
            "  {}  {}:{}",
            c["fqn"].as_str().unwrap_or_default(),
            c["path"].as_str().unwrap_or_default(),
            c["line"]
        );
    }
}

/// A function to choose from: its name, where it is defined, and its kind.
fn candidate(files: Option<&Files<'_>>, key: &FuncKey) -> serde_json::Value {
    let file = files.and_then(|files| files.of(key));
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

/// A function to report on: the text naming it, the function, and how the text named it.
type Asked<'q> = (&'q str, &'q FuncKey, Option<&'static str>);

/// The reports on the functions `asked`, from one diagnosis of their files (and with
/// `--callers`, of their callers' files) with their baseline status, and one compilation of the
/// contracts.
fn build<'p>(
    args: &InspectArgs,
    indexed: &'p Indexed,
    decls: &ProjectDecls,
    files: &Files<'p>,
    asked: &[Asked<'_>],
    errors: &[String],
) -> Result<Vec<InspectReport>> {
    let index = &indexed.index;
    let env = &indexed.project.env;
    let depth = if args.callers {
        usize::from(args.depth)
    } else {
        1
    };
    let mut checked: Vec<&ParsedFile> = Vec::new();
    let mut seen: FxHashSet<&Path> = FxHashSet::default();
    let mut add = |f: &'p ParsedFile| {
        if seen.insert(f.path.as_path()) {
            checked.push(f);
        }
    };
    let mut calls = index.calls_by_caller();
    for (_, key, _) in asked {
        files.of(key).into_iter().for_each(&mut add);
        if args.callers {
            let found = index.callers_within(&mut calls, key, depth, MAX_CALLERS);
            found
                .sites
                .iter()
                .filter_map(|c| files.at(c.path))
                .for_each(&mut add);
        }
    }
    let units: Vec<_> = checked.iter().map(|f| f.unit()).collect();
    let (diags, _) = indexed.diagnose_units(&units, false)?;
    let reports = with_baseline(args, indexed, diags)?;
    let (cfg, kb, soundness) = (&env.cfg, &env.kb, &env.soundness_kb);
    let contracts =
        heki_rules::contract_roles(cfg, kb, soundness, index, decls, &indexed.selection)?;
    let rel_of = |p: &Path| files.at(p).map_or_else(|| path_text(p), |f| f.rel.clone());
    let text_of = |p: &Path| files.at(p).map(|f| f.source.clone());
    let root = canonical(&env.root).unwrap_or_else(|_| env.root.clone());
    let inputs = InspectInputs {
        index,
        decls,
        kb: &env.kb,
        root: &root,
        ownership: heki_rules::declares_ownership(&env.cfg),
        units: &units,
        rel_of: &rel_of,
        text_of: &text_of,
        reports,
        contracts: &contracts,
        errors: errors.to_vec(),
    };
    let inspector = Inspector::new(&inputs);
    Ok(asked
        .iter()
        .map(|(text, key, from)| {
            let query = InspectQuery {
                text,
                resolved_from: *from,
                callers: args.callers,
                depth,
            };
            inspector.report(key, &query)
        })
        .collect())
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
