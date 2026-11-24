//! `IMP101` import cycles: every group of the project's modules that import each other through
//! the imports of the timing classes `imports.cycle-edges` names, reported once at the import
//! closing a shortest cycle of the group. The groups and their cycles come from
//! `heki_effect::imports` ([`ImportGraph::cycle_groups`]).

use super::{hop_text, when_text};
use crate::{CheckContext, RuleConfigError};
use heki_diagnostics::Diagnostic;
use heki_effect::ProjectIndex;
use heki_effect::imports::{CycleGroup, ImportGraph, ImportHop, ModuleRef, Timing};
use heki_effect::resolver::ProjectDecls;
use heki_env::HekiConfig;
use heki_ir::walk::Answer;
use std::path::PathBuf;

/// The timing classes of the imports `IMP101` follows (`imports.cycle-edges`): by default the
/// imports that run while a module loads.
fn cycle_edges(config: &HekiConfig) -> Result<Vec<Timing>, RuleConfigError> {
    let Some(names) = &config.imports.cycle_edges else {
        return Ok(vec![Timing::Body]);
    };
    (names.iter())
        .map(|name| {
            (Timing::ALL.into_iter())
                .find(|t| t.as_str() == name)
                .ok_or_else(|| RuleConfigError::UnknownValue {
                    table: "imports",
                    key: "cycle-edges",
                    value: name.clone(),
                    expected: "`body`, `lazy`, `function`, or `type-checking`",
                    values: &Timing::NAMES,
                })
        })
        .collect()
}

/// The cycle groups of `graph` through the imports of the timing classes `edges`: an import
/// whose timing heki cannot tell is no edge.
fn groups(graph: &ImportGraph<'_>, edges: &[Timing]) -> Vec<CycleGroup> {
    graph.cycle_groups(|site| matches!(site.timing, Answer::Known(t) if edges.contains(&t)))
}

/// `IMP101`: each group of modules importing each other, at the import closing its cycle when
/// that import's module is checked.
pub(crate) fn imp101(
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) -> Result<(), RuleConfigError> {
    let edges = cycle_edges(cx.config)?;
    if !cx.selection.is_enabled("IMP101") || cx.units.is_empty() {
        return Ok(());
    }
    let graph = ImportGraph::build(cx.index.files.values(), cx.decls);
    for group in groups(&graph, &edges) {
        report(cx, &graph, &group.members, &group.cycle, out);
    }
    Ok(())
}

/// The report of the group `members` at the last hop of `cycle`, which closes it.
fn report(
    cx: &CheckContext<'_>,
    graph: &ImportGraph<'_>,
    members: &[u32],
    cycle: &[ImportHop],
    out: &mut Vec<Diagnostic>,
) {
    let (closing, steps) = cycle.split_last().expect("a cycle has hops");
    let Some(unit) = cx.unit(graph.path(closing.from)) else {
        return;
    };
    let (site, target) =
        &graph.sites(closing.from).expect("a cycle steps from sites")[closing.site];
    let name = |m: u32| graph.name(ModuleRef::Project(m));
    let imported = graph.name(target.certain().expect("a hop's site loads a module"));
    let when = when_text(site, Some(name(closing.from)));
    let first = if closing.parent {
        format!(", which loads its package `{}` first and", name(closing.to))
    } else {
        ", which".to_owned()
    };
    let path: Vec<String> = (cycle.iter())
        .map(|hop| format!("`{}`", name(hop.from)))
        .chain([format!("`{}`", name(closing.to))])
        .collect();
    let mut message = format!(
        "`{}` imports `{imported}`{when}{first} closes the import cycle {}",
        site.qualname,
        path.join(" -> ")
    );
    if members.len() > cycle.len() {
        message.push_str(&format!(
            ", one of the cycles of {} modules that import each other",
            members.len()
        ));
    }
    let target = (members.iter())
        .map(|&m| name(m))
        .collect::<Vec<_>>()
        .join(", ");
    let help = "break the cycle at one of its imports: move what the modules share into a module \
        none of them imports, or import the name only where it is used";
    let related = (steps.iter())
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
        "IMP101",
        unit,
        site.span,
        &site.qualname,
        target,
        message,
        Some(help.to_owned()),
        &[],
    ) {
        d.related = related;
    }
}

/// The files `IMP101` reports in under `config`: the file of the import closing the cycle of
/// each group. A change to which these are, or to any file's imports, changes the reports of
/// the files before and after it (the language server re-checks them).
pub fn import_cycle_files(
    config: &HekiConfig,
    index: &ProjectIndex,
    decls: &ProjectDecls,
) -> Result<Vec<PathBuf>, RuleConfigError> {
    let edges = cycle_edges(config)?;
    let graph = ImportGraph::build(index.files.values(), decls);
    let mut files: Vec<PathBuf> = (groups(&graph, &edges).iter())
        .filter_map(|group| group.cycle.last())
        .map(|closing| graph.path(closing.from).to_path_buf())
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}
