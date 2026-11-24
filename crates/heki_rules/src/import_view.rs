//! `heki inspect --imports MODULE`: the import facts of the project modules a name or glob
//! names (ADR-022), from the analysis `heki check` runs: each import site with when it runs and
//! what it loads, and what loading the module loads while it loads.

use crate::DottedPattern;
use crate::project::{Indexed, ParsedFile, rel_posix};
use heki_effect::ProjectDecls;
use heki_effect::code_kind::{CodeKind, code_kind};
use heki_effect::imports::{ImportForm, ImportGraph, ImportSite, ModuleRef, Target};
use heki_ir::walk::Answer;
use rustc_hash::FxHashMap;
use serde::Serialize;
use std::fmt::Write as _;
use std::path::Path;

/// The version of the JSON object of `heki inspect --imports --output-format json`.
pub const IMPORTS_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
pub struct ImportsReport {
    pub version: u32,
    pub query: String,
    pub modules: Vec<ModuleReport>,
}

/// One project module.
#[derive(Debug, Serialize)]
pub struct ModuleReport {
    pub module: String,
    pub path: String,
    /// `package`, `package_test`, `development`, or `open` ([`CodeKind`]).
    pub kind: CodeKind,
    /// `None`: heki has no import facts of the module (a typing stub, whose imports never
    /// run, or a file it could not read).
    pub imports: Option<Vec<SiteReport>>,
    /// What loading the module loads while it loads, at least ([`ImportGraph::loads`]);
    /// `None` when the walks ran out of budget.
    pub loads: Option<Loads>,
}

/// One import site.
#[derive(Debug, Serialize)]
pub struct SiteReport {
    pub line: u32,
    pub column: u32,
    /// `import`, `from`, `star`, or `by_name`.
    pub form: &'static str,
    /// The name a `from` import takes from its module.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The function the site is in (`<module>`: the module body and its class bodies).
    pub function: String,
    /// `body`, `lazy`, `function`, `type-checking`; `None` when heki cannot tell.
    pub timing: Option<&'static str>,
    pub guarded: bool,
    pub unconditional: bool,
    /// The module the site certainly loads.
    pub target: Option<String>,
    /// `project`, `stdlib`, or `third-party`, of `target`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<&'static str>,
    /// Other modules the site may load instead or as well: the submodule of a `from` import
    /// heki cannot decide, each module a by-name import may name (`pkg.plugins.*` for the
    /// modules under a package).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub may_load: Vec<String>,
    /// The module the site names that the project does not have below its package, or, empty,
    /// a relative import climbing past its top-level package.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Loads {
    pub project: Vec<String>,
    pub external: Vec<String>,
}

/// Why the query names no module.
#[derive(Debug)]
pub enum ImportsFailure {
    Invalid(String),
    Unknown(String),
}

impl std::fmt::Display for ImportsFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => f.write_str(message),
            Self::Unknown(query) => write!(f, "no module of the project is named `{query}`"),
        }
    }
}

/// The report on the modules `query` (a module name or a glob over module names) names.
pub fn imports_report(indexed: &Indexed, query: &str) -> Result<ImportsReport, ImportsFailure> {
    let pattern = DottedPattern::new(query, "inspect --imports")
        .map_err(|e| ImportsFailure::Invalid(e.to_string()))?;
    let index = &indexed.index;
    let decls: ProjectDecls = index.project_decls();
    let graph = ImportGraph::build(index.files.values(), &decls);
    let env = &indexed.project.env;
    let by_path: FxHashMap<&Path, &ParsedFile> = (indexed.project.files.iter())
        .map(|f| (f.path.as_path(), f))
        .collect();
    let mut wanted: Vec<(&heki_effect::FileSummary, u32)> = (index.files.values())
        .filter(|f| pattern.matches_whole(&f.module_name))
        .filter_map(|f| Some((f, graph.module(&f.module_name)?)))
        .collect();
    if wanted.is_empty() {
        return Err(ImportsFailure::Unknown(query.to_owned()));
    }
    // A module several files define is reported once, from the file that defines it.
    wanted.sort_by(|a, b| {
        let precedence = heki_effect::module_precedence;
        (a.1, precedence(&b.0.path)).cmp(&(b.1, precedence(&a.0.path)))
    });
    wanted.dedup_by_key(|(_, id)| *id);
    let budget = graph.budget("import_graph");
    let import_time = graph.import_time(&budget);
    let modules = wanted
        .into_iter()
        .map(|(file, id)| {
            let parsed = by_path.get(file.path.as_path());
            let rel = parsed.map_or_else(|| rel_posix(&file.path, &env.root), |p| p.rel.clone());
            let lines = parsed.map(|p| heki_ast::LineIndex::from_source(&p.source));
            let imports = graph.sites(id).zip(lines.as_ref()).map(|(sites, lines)| {
                (sites.iter())
                    .map(|(site, target)| site_report(&graph, lines, site, target))
                    .collect()
            });
            let project = graph.loaded_while_loading(&budget, id);
            let external = import_time.externals(&graph, id);
            let loads = match project.zip(external) {
                Answer::Known((project, external)) => Some(Loads {
                    project: (project.into_iter())
                        .filter(|m| matches!(m, ModuleRef::Project(_)))
                        .map(|m| graph.name(m).to_owned())
                        .collect(),
                    external: external.into_iter().map(str::to_owned).collect(),
                }),
                Answer::Open => None,
            };
            ModuleReport {
                module: file.module_name.clone(),
                kind: code_kind(&rel, &file.module_name, &env.layout, &env.cfg),
                path: rel,
                imports,
                loads,
            }
        })
        .collect();
    Ok(ImportsReport {
        version: IMPORTS_VERSION,
        query: query.to_owned(),
        modules,
    })
}

fn site_report(
    graph: &ImportGraph<'_>,
    lines: &heki_ast::LineIndex,
    site: &ImportSite,
    target: &Target,
) -> SiteReport {
    let (line, column) = lines.line_col(site.span.start);
    let (form, name) = match &site.form {
        ImportForm::Import => ("import", None),
        ImportForm::From(name) => ("from", Some(name.clone())),
        ImportForm::Star => ("star", None),
        ImportForm::ByName => ("by_name", None),
    };
    let certain = target.certain();
    let mut may_load = Vec::new();
    let mut missing = None;
    if !matches!(target, Target::Module(_)) {
        alternatives(graph, target, &mut may_load, &mut missing);
    }
    let group = certain.map(|m| match m {
        ModuleRef::Project(_) => "project",
        ModuleRef::External(_) => {
            let name = graph.name(m);
            if heki_env::is_stdlib_module(name.split('.').next().unwrap_or(name)) {
                "stdlib"
            } else {
                "third-party"
            }
        }
    });
    SiteReport {
        line,
        column,
        form,
        name,
        function: site.qualname.clone(),
        timing: match site.timing {
            Answer::Known(t) => Some(t.as_str()),
            Answer::Open => None,
        },
        guarded: site.guarded,
        unconditional: site.unconditional,
        target: certain.map(|m| graph.name(m).to_owned()),
        group,
        may_load,
        missing,
    }
}

/// What `target` may load besides what it certainly loads, and the module it names that the
/// project does not have.
fn alternatives(
    graph: &ImportGraph<'_>,
    target: &Target,
    may_load: &mut Vec<String>,
    missing: &mut Option<String>,
) {
    match target {
        Target::Module(m) => may_load.push(graph.name(*m).to_owned()),
        Target::PackageOrSubmodule { submodule, .. } => may_load.push(submodule.clone()),
        Target::AnyOf(targets) => {
            for t in targets {
                alternatives(graph, t, may_load, missing);
            }
        }
        Target::Under(head) => may_load.push(format!("{head}*")),
        Target::Missing(m) => *missing = Some(m.clone()),
        Target::Open => {}
    }
}

/// The text form of `report`.
pub fn render_imports_text(report: &ImportsReport) -> String {
    let mut out = String::new();
    for (i, m) in report.modules.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let _ = writeln!(out, "{} ({}, {} code)", m.module, m.path, m.kind.as_str());
        match &m.imports {
            None => out.push_str(
                "  imports: unknown (a typing stub, whose imports never run, or a file heki could not read)\n",
            ),
            Some(sites) if sites.is_empty() => out.push_str("  imports: none\n"),
            Some(sites) => {
                out.push_str("  imports:\n");
                for s in sites {
                    let _ = writeln!(out, "    {}", site_text(s));
                }
            }
        }
        match &m.loads {
            None => out.push_str("  loads while it loads: unknown (past the walks' budget)\n"),
            Some(loads) => {
                let list = |names: &[String]| {
                    if names.is_empty() {
                        "none".to_owned()
                    } else {
                        names.join(", ")
                    }
                };
                out.push_str("  loads while it loads, at least:\n");
                let _ = writeln!(out, "    project modules: {}", list(&loads.project));
                let _ = writeln!(out, "    other modules: {}", list(&loads.external));
            }
        }
    }
    out
}

fn site_text(s: &SiteReport) -> String {
    let what = match (&s.target, &s.missing) {
        (Some(target), _) => format!("{target} ({})", s.group.unwrap_or_default()),
        (None, Some(missing)) if missing.is_empty() => {
            "nothing: climbs past the top-level package".to_owned()
        }
        (None, Some(missing)) => format!("{missing}, which the project does not have"),
        (None, None) if s.may_load.is_empty() => "unknown".to_owned(),
        (None, None) => "one of the modules below".to_owned(),
    };
    let form = match (&s.name, s.form) {
        (Some(name), _) => format!("from-import of `{name}`"),
        (None, "star") => "star import".to_owned(),
        (None, "by_name") => "import by name".to_owned(),
        (None, _) => "import".to_owned(),
    };
    let mut notes = vec![s.timing.unwrap_or("timing unknown").to_owned()];
    if s.function != "<module>" {
        notes.push(format!("in `{}`", s.function));
    }
    if s.guarded {
        notes.push("guarded".to_owned());
    }
    if s.unconditional {
        notes.push("unconditional".to_owned());
    }
    let mut text = format!(
        "{}:{}  {form}: {what}  [{}]",
        s.line,
        s.column,
        notes.join(", ")
    );
    if !s.may_load.is_empty() {
        let _ = write!(text, "\n      may load: {}", s.may_load.join(", "));
    }
    text
}
