//! `IMP102` circular imports that fail: each `from M import name` that raises `ImportError`
//! when a module of its import cycle is the first of the cycle imported, reported once with the
//! modules whose import does that. The statements and their chains come from
//! `heki_effect::imports` ([`ImportGraph::circular_failures`]).

use super::{chosen_cut, cut_help, hop_text};
use crate::CheckContext;
use heki_diagnostics::Diagnostic;
use heki_effect::ProjectIndex;
use heki_effect::imports::{CircularFailure, ImportGraph, ModuleRef, Timing};
use heki_effect::resolver::ProjectDecls;
use rustc_hash::FxHashSet;
use std::cell::OnceCell;
use std::path::PathBuf;

/// The most entry modules a message names; past it, it counts the others.
const NAMED_ENTRIES: usize = 3;

/// The failing imports of the project `index` and `decls` describe.
fn failures(graph: &ImportGraph<'_>, decls: &ProjectDecls) -> Vec<CircularFailure> {
    graph.circular_failures(decls, &graph.budget("import_failures"))
}

/// `IMP102`: each `from` import that fails when a module of its import cycle is imported first,
/// when its module is checked.
pub(crate) fn imp102(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.selection.is_enabled("IMP102") || cx.units.is_empty() {
        return;
    }
    let graph = ImportGraph::build(cx.index.files.values(), cx.decls);
    let failures = failures(&graph, cx.decls);
    if failures.is_empty() {
        return;
    }
    let groups = graph.loading_groups();
    let cuts: Vec<OnceCell<FxHashSet<(u32, usize)>>> =
        groups.iter().map(|_| OnceCell::new()).collect();
    let group_cuts = |g: u32| -> &FxHashSet<(u32, usize)> {
        cuts[g as usize].get_or_init(|| {
            let budget = graph.budget("import_cuts");
            let found = graph.loading_group_cuts(&groups[g as usize], &budget);
            found.open_means(Vec::new()).into_iter().collect()
        })
    };
    for failure in failures {
        report(cx, &graph, &group_cuts, &failure, out);
    }
}

/// The modules `entries` names, as a message lists them: `a`, `a` or `b`, `a`, `b`, or `c`, or
/// the first [`NAMED_ENTRIES`] and how many others.
fn entries_text(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
    match quoted.as_slice() {
        [one] => one.clone(),
        [a, b] => format!("{a} or {b}"),
        _ if quoted.len() <= NAMED_ENTRIES => {
            let (last, first) = quoted.split_last().expect("several");
            format!("{}, or {last}", first.join(", "))
        }
        _ => {
            let others = quoted.len() - NAMED_ENTRIES;
            let more = if others == 1 { "module" } else { "modules" };
            format!(
                "{}, or {others} other {more}",
                quoted[..NAMED_ENTRIES].join(", ")
            )
        }
    }
}

/// The report of `failure` at its import, with the hops of its first entry's load as related
/// locations.
fn report<'c>(
    cx: &CheckContext<'_>,
    graph: &ImportGraph<'_>,
    group_cuts: &impl Fn(u32) -> &'c FxHashSet<(u32, usize)>,
    failure: &CircularFailure,
    out: &mut Vec<Diagnostic>,
) {
    let Some(unit) = cx.unit(graph.path(failure.module)) else {
        return;
    };
    let (site, _) = &graph.sites(failure.module).expect("a failure is at a site")[failure.site];
    let name = &failure.name;
    let module = |m: u32| graph.name(ModuleRef::Project(m));
    let from = module(failure.from);
    let paused = (failure.chain.iter())
        .find(|hop| hop.from == failure.from)
        .expect("the chain leaves the module the name comes from");
    let (_, paused_target) =
        &graph.sites(failure.from).expect("a chain steps from sites")[paused.site];
    let paused_at = graph.name(
        paused_target
            .certain()
            .expect("a hop's site loads a module"),
    );
    let entries: Vec<&str> = failure.entries.iter().map(|&m| module(m)).collect();
    let message = format!(
        "`{}` imports `{name}` from `{from}` while `{from}` is still loading, at its import of \
         `{paused_at}`, and has not bound it: importing {} while no module of the import cycle \
         has loaded raises `ImportError` here",
        site.qualname,
        entries_text(&entries),
    );
    let help = cut(cx, graph, group_cuts(failure.group), failure).unwrap_or_else(|| {
        format!(
            "import `{name}` inside the function that uses it, import the module `{from}` and \
             read `{from}.{name}` where it is used, or bind `{name}` in `{from}` before it \
             imports `{paused_at}`"
        )
    });
    let related = (failure.chain.iter())
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
        "IMP102",
        unit,
        site.span,
        &site.qualname,
        format!("{from}.{name}"),
        message,
        Some(help),
        &[],
    ) {
        d.related = related;
    }
}

/// The help naming the import on every import cycle of `failure`'s group, `cuts` the sites one
/// search over the group finds there ([`ImportGraph::loading_group_cuts`]): the failing import,
/// or the first other on the failure's cycle from the import `from` is paused at. Both changes
/// take it out of the imports that run while a module loads.
fn cut(
    cx: &CheckContext<'_>,
    graph: &ImportGraph<'_>,
    cuts: &FxHashSet<(u32, usize)>,
    failure: &CircularFailure,
) -> Option<String> {
    if cuts.is_empty() {
        return None;
    }
    let sites: Vec<(u32, usize)> = (failure.cycle().iter())
        .map(|hop| (hop.from, hop.site))
        .filter(|site| cuts.contains(site))
        .collect();
    let reported = (failure.module, failure.site);
    let chosen = chosen_cut(cx, graph, &sites, Some(reported), |t| t == Timing::Body)?;
    Some(cut_help(
        graph,
        chosen,
        reported,
        "import cycle of these modules",
    ))
}

/// The files `IMP102` reports in: the file of each failing import. A change to which these are,
/// or to any file's imports, changes the reports of the files before and after it (the language
/// server re-checks them).
pub fn circular_failure_files(index: &ProjectIndex, decls: &ProjectDecls) -> Vec<PathBuf> {
    let graph = ImportGraph::build(index.files.values(), decls);
    let mut files: Vec<PathBuf> = (failures(&graph, decls).iter())
        .map(|failure| graph.path(failure.module).to_path_buf())
        .collect();
    files.sort();
    files.dedup();
    files
}

#[cfg(test)]
mod tests {
    use super::entries_text;

    #[test]
    fn entry_modules_as_a_message_names_them() {
        for (names, text) in [
            (&["a"][..], "`a`"),
            (&["a", "b"][..], "`a` or `b`"),
            (&["a", "b", "c"][..], "`a`, `b`, or `c`"),
            (
                &["a", "b", "c", "d"][..],
                "`a`, `b`, `c`, or 1 other module",
            ),
            (
                &["a", "b", "c", "d", "e"][..],
                "`a`, `b`, `c`, or 2 other modules",
            ),
        ] {
            assert_eq!(entries_text(names), text, "{names:?}");
        }
    }
}
