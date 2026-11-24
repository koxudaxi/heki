//! `IMP301` and `IMP302`: a distribution the project declares only for development, loaded while
//! a module of its package loads, and one it declares only in extras, loaded while a base module
//! (a top-level package, or a module an entry point names) loads. Each import is reported where
//! it is written, with the chain of imports from the module whose load runs it. What loading a
//! module loads comes from `heki_effect::imports` ([`ImportGraph::loaded_from`]), and which
//! distributions provide a module and how the project declares them from
//! [`Distributions::declared_only`].

use super::{hop_text, when_text};
use crate::{CheckContext, ProjectLayout, RuleSelection};
use heki_diagnostics::Diagnostic;
use heki_effect::ProjectIndex;
use heki_effect::code_kind::CodeKind;
use heki_effect::imports::{DeclaredOnly, Distributions, ImportGraph, ImportHop, ModuleRef, Only};
use heki_effect::resolver::ProjectDecls;
use heki_env::HekiConfig;
use heki_kb::EffectiveEcosystemKb;
use rustc_hash::FxHashMap;
use std::cell::OnceCell;
use std::path::PathBuf;

/// The two rules, and what each judges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// `IMP301`: what the package modules load, of distributions declared only for development.
    Development,
    /// `IMP302`: what the base modules load, of distributions declared only in extras.
    Extras,
}

impl Rule {
    /// The rules `selection` runs.
    fn selected(selection: &RuleSelection) -> Vec<Self> {
        ([Self::Development, Self::Extras].into_iter())
            .filter(|r| selection.is_enabled(r.code()))
            .collect()
    }

    fn code(self) -> &'static str {
        match self {
            Self::Development => "IMP301",
            Self::Extras => "IMP302",
        }
    }

    fn only(self) -> Only {
        match self {
            Self::Development => Only::Development,
            Self::Extras => Only::Extras,
        }
    }
}

/// One import of a distribution declared only for development or only in extras, and the
/// module whose load runs it.
struct Finding {
    /// The module of the import, and the import's index among its sites.
    module: u32,
    site: usize,
    /// The module the import loads, outside the project.
    imported: String,
    declared: DeclaredOnly,
    /// The module whose load runs the import: `module` itself, or the start of `chain`.
    start: u32,
    /// The imports from `start` to `module`.
    chain: Vec<ImportHop>,
}

/// The kind of code of each project module of a graph, decided on first use: only a project
/// importing a distribution the rules judge needs them.
struct Kinds<'k> {
    of: &'k dyn Fn(u32) -> CodeKind,
    count: u32,
    kinds: OnceCell<Vec<CodeKind>>,
}

impl<'k> Kinds<'k> {
    fn new(graph: &ImportGraph<'_>, of: &'k dyn Fn(u32) -> CodeKind) -> Self {
        Self {
            of,
            count: graph.module_count(),
            kinds: OnceCell::new(),
        }
    }

    /// By module id.
    fn all(&self) -> &[CodeKind] {
        self.kinds
            .get_or_init(|| (0..self.count).map(self.of).collect())
    }
}

/// The imports of `rule` in `graph`: each import that runs whenever its module loads, whose
/// failure leaves the module, of a module outside the project whose distributions `declared`
/// says are declared only as `rule` judges; reported once the load of a module it judges
/// ([`starts`]) runs the import. Development code stops the walk: an installed package does not
/// have it, so its imports never run there (`IMP104` reports the import of it).
fn findings(
    graph: &ImportGraph<'_>,
    config: &HekiConfig,
    declared: &Distributions<'_>,
    rule: Rule,
    kinds: &Kinds<'_>,
) -> Vec<Finding> {
    // Per module outside the project, how its distributions are declared.
    let mut only: FxHashMap<ModuleRef, Option<DeclaredOnly>> = FxHashMap::default();
    let mut judged: Vec<(u32, usize, ModuleRef)> = Vec::new();
    for m in 0..graph.module_count() {
        for (j, (site, target)) in graph.sites(m).into_iter().flatten().enumerate() {
            let Some(e @ ModuleRef::External(_)) = target.certain() else {
                continue;
            };
            if !site.always_imports_uncaught() {
                continue;
            }
            let found = only
                .entry(e)
                .or_insert_with(|| declared.declared_only(graph.name(e)));
            if found.as_ref().is_some_and(|d| d.only == rule.only()) {
                judged.push((m, j, e));
            }
        }
    }
    // Per module, its first import of each set of distributions: the module loads them there,
    // and its other imports of them (the names of one `from` statement, other modules of the
    // same distribution) say no more.
    let mut candidates: FxHashMap<u32, Vec<(usize, ModuleRef)>> = FxHashMap::default();
    for (m, j, e) in judged {
        let sites = candidates.entry(m).or_default();
        if !sites.iter().any(|(_, seen)| only[seen] == only[&e]) {
            sites.push((j, e));
        }
    }
    if candidates.is_empty() {
        return Vec::new();
    }
    let kinds = kinds.all();
    let enters = |m: u32| kinds[m as usize] != CodeKind::Development;
    let loaded = graph.loaded_from(starts(graph, config, rule, kinds), enters);
    let mut out = Vec::new();
    for &m in &loaded.order {
        let Some(sites) = candidates.get(&m) else {
            continue;
        };
        let chain = loaded.chain(m);
        let start = chain.first().map_or(m, |hop| hop.from);
        for &(site, e) in sites {
            out.push(Finding {
                module: m,
                site,
                imported: graph.name(e).to_owned(),
                declared: only[&e]
                    .clone()
                    .expect("a candidate's distributions are declared"),
                start,
                chain: chain.clone(),
            });
        }
    }
    out
}

/// The modules whose load `rule` judges, ascending: for `IMP301` the package modules, for
/// `IMP302` the base modules: the top-level package modules, and the modules the entry points
/// name with the packages holding them, which load first. `kinds` by module id.
fn starts(
    graph: &ImportGraph<'_>,
    config: &HekiConfig,
    rule: Rule,
    kinds: &[CodeKind],
) -> Vec<u32> {
    let package = |m: u32| kinds[m as usize] == CodeKind::Package;
    let mut out: Vec<u32> = match rule {
        Rule::Development => (0..graph.module_count()).filter(|&m| package(m)).collect(),
        Rule::Extras => {
            let top_level = (0..graph.module_count())
                .filter(|&m| !graph.name(ModuleRef::Project(m)).contains('.') && package(m));
            let entry_points = (config.declared.iter())
                .flat_map(|d| &d.entry_points)
                .filter_map(|point| graph.module(&point.module))
                .flat_map(|m| {
                    let parents = graph.parents(ModuleRef::Project(m));
                    (parents.into_iter().filter_map(ModuleRef::project)).chain([m])
                });
            top_level.chain(entry_points).collect()
        }
    };
    out.sort_unstable();
    out.dedup();
    out
}

/// `IMP301` and `IMP302`: the imports of distributions declared only for development that a
/// package module's load runs, and of distributions declared only in extras that a base
/// module's load runs, in the checked modules.
pub(crate) fn imp301_imp302(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    let rules = Rule::selected(cx.selection);
    if rules.is_empty() || cx.units.is_empty() {
        return;
    }
    let graph = ImportGraph::build(cx.index.files.values(), cx.decls);
    let Some(declared) = Distributions::of(cx.config, cx.kb, graph.loose_names()) else {
        return;
    };
    let kind = |m: u32| cx.code_kind(graph.path(m), graph.name(ModuleRef::Project(m)));
    let kinds = Kinds::new(&graph, &kind);
    for rule in rules {
        for finding in findings(&graph, cx.config, &declared, rule, &kinds) {
            report(cx, &graph, rule, &finding, out);
        }
    }
}

/// `` `a` `` , `` `a` and `b` ``, `` `a`, `b`, and `c` ``.
fn listed(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
    match quoted.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, [first])) => format!("{first} and {last}"),
        Some((last, rest)) => format!("{}, and {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// How the report says where the project declares the distributions of `declared`: "`pytest`,
/// which provides it, only in the development group `test`".
fn declared_text(declared: &DeclaredOnly) -> String {
    let names: Vec<&str> = (declared.distributions.iter())
        .map(|(d, _)| d.as_str())
        .collect();
    let mut places: Vec<&str> = (declared.distributions.iter())
        .flat_map(|(_, places)| places.iter().map(String::as_str))
        .collect();
    places.sort_unstable();
    places.dedup();
    let provides = if names.len() == 1 {
        "which provides it"
    } else {
        "which may provide it"
    };
    let place = match (declared.only, places.len()) {
        (Only::Development, 1) => "the development group",
        (Only::Development, _) => "the development groups",
        (Only::Extras, 1) => "the extra",
        (Only::Extras, _) => "the extras",
    };
    format!(
        "{}, {provides}, only in {place} {}",
        listed(&names),
        listed(&places)
    )
}

/// The report of `finding` at its import, with the imports from the module whose load runs it
/// as related locations.
fn report(
    cx: &CheckContext<'_>,
    graph: &ImportGraph<'_>,
    rule: Rule,
    finding: &Finding,
    out: &mut Vec<Diagnostic>,
) {
    let Some(unit) = cx.unit(graph.path(finding.module)) else {
        return;
    };
    let (site, _) = &graph.sites(finding.module).expect("a finding is at a site")[finding.site];
    let name = |m: u32| graph.name(ModuleRef::Project(m));
    let (module, start) = (name(finding.module), name(finding.start));
    let imported = &finding.imported;
    let declared = declared_text(&finding.declared);
    let names: Vec<&str> = (finding.declared.distributions.iter())
        .map(|(d, _)| d.as_str())
        .collect();
    let mut message = format!(
        "`{}` imports `{imported}`{}",
        site.qualname,
        when_text(site, Some(module))
    );
    let (message, help) = match rule {
        Rule::Development => {
            if finding.start != finding.module {
                message.push_str(&format!(
                    ", which the package module `{start}` loads while it loads"
                ));
            }
            message.push_str(&format!(", and the project declares {declared}"));
            let help = format!(
                "declare {} in `[project] dependencies`, or import `{imported}` only inside the \
                 functions that use it",
                listed(&names)
            );
            (message, help)
        }
        Rule::Extras => {
            if finding.start != finding.module {
                message.push_str(&format!(", which importing `{start}` loads"));
            }
            let needs = if finding.declared.distributions.len() == 1 {
                "it"
            } else {
                "them"
            };
            message.push_str(&format!(
                ", and the project declares {declared}: `import {start}` needs {needs}"
            ));
            let help = format!(
                "import `{imported}` only inside the functions that use it, catch its \
                 `ImportError` and name the extra to install, or declare {} in `[project] \
                 dependencies`",
                listed(&names)
            );
            (message, help)
        }
    };
    let related = (finding.chain.iter())
        .map(|hop| {
            let (site, message) = hop_text(
                graph,
                hop.from,
                hop.site,
                ModuleRef::Project(hop.to),
                hop.parent,
            );
            cx.related_at(graph.path(hop.from), site.span, message)
        })
        .collect();
    if let Some(d) = cx.emit(
        out,
        rule.code(),
        unit,
        site.span,
        &site.qualname,
        names.join("|"),
        message,
        Some(help),
        &[],
    ) {
        d.related = related;
    }
}

/// The files `IMP301` and `IMP302`, as `selection` selects them, report in: the file of each
/// import they report. A change to which these are, or to any file's imports, changes the
/// reports of the files before and after it (the language server re-checks them).
pub fn dependency_report_files(
    selection: &RuleSelection,
    config: &HekiConfig,
    kb: &EffectiveEcosystemKb,
    layout: ProjectLayout<'_>,
    index: &ProjectIndex,
    decls: &ProjectDecls,
) -> Vec<PathBuf> {
    let rules = Rule::selected(selection);
    if rules.is_empty() {
        return Vec::new();
    }
    let graph = ImportGraph::build(index.files.values(), decls);
    let Some(declared) = Distributions::of(config, kb, graph.loose_names()) else {
        return Vec::new();
    };
    let kind = |m: u32| layout.code_kind(graph.path(m), graph.name(ModuleRef::Project(m)), config);
    let kinds = Kinds::new(&graph, &kind);
    let mut files: Vec<PathBuf> = (rules.into_iter())
        .flat_map(|rule| findings(&graph, config, &declared, rule, &kinds))
        .map(|finding| graph.path(finding.module).to_path_buf())
        .collect();
    files.sort();
    files.dedup();
    files
}

#[cfg(test)]
mod tests {
    use super::listed;

    #[test]
    fn names_as_a_message_lists_them() {
        for (names, text) in [
            (&[][..], ""),
            (&["a"][..], "`a`"),
            (&["a", "b"][..], "`a` and `b`"),
            (&["a", "b", "c"][..], "`a`, `b`, and `c`"),
        ] {
            assert_eq!(listed(names), text, "{names:?}");
        }
    }
}
