//! `IMP104` development code imported by package code: each import that runs (in the module
//! body, lazily, or in a function) of a module of the package the project distributes, whose
//! module is development code: a test module outside the package's tree or, in a src layout, a
//! module outside the source roots. One report per function and module imported, at its first
//! import: the names of one `from` statement are one import. Which module an import loads is
//! decided by `heki_effect::imports`, and which kind of code a module is by
//! `heki_effect::code_kind`.

use super::when_text;
use crate::CheckContext;
use heki_diagnostics::Diagnostic;
use heki_effect::code_kind::{CodeKind, is_test_module};
use heki_effect::imports::{ImportGraph, ModuleRef, Timing};
use heki_ir::walk::Answer;
use rustc_hash::FxHashSet;

/// `IMP104`: the imports of development code that the checked package modules make.
pub(crate) fn imp104(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.selection.is_enabled("IMP104") || cx.units.is_empty() {
        return;
    }
    let checked: FxHashSet<&str> = (cx.units.iter())
        .map(|u| u.input.module_name.as_str())
        .collect();
    // The statements of the checked modules only, as `import-boundaries` resolves them: a check
    // reads the declarations their imports name, and no others.
    let graph = ImportGraph::resolving(cx.index.files.values(), cx.decls, |m| checked.contains(m));
    // The checked package modules, each with its unit; a module several files define is
    // checked in its owner.
    let packages = (cx.units.iter()).filter_map(|unit| {
        let module = unit.input.module_name.as_str();
        let m = graph.module(module)?;
        let owner = graph.path(m) == unit.input.path.as_path();
        (owner && cx.code_kind(&unit.input.path, module) == CodeKind::Package)
            .then_some((unit, module, m))
    });
    for (unit, module, m) in packages {
        let mut reported: FxHashSet<(&str, u32)> = FxHashSet::default();
        for (site, target) in graph.sites(m).into_iter().flatten() {
            let runs = matches!(
                site.timing,
                Answer::Known(Timing::Body | Timing::Lazy | Timing::Function)
            );
            let Some(ModuleRef::Project(d)) = target.certain().filter(|_| runs && !site.caught)
            else {
                continue;
            };
            let (imported, path) = (graph.name(ModuleRef::Project(d)), graph.path(d));
            if cx.code_kind(path, imported) != CodeKind::Development
                || !reported.insert((site.qualname.as_str(), d))
            {
                continue;
            }
            let rel = cx.rel_path(path);
            let place = if is_test_module(&rel) {
                "test code outside the package"
            } else {
                "a module outside the source roots"
            };
            let message = format!(
                "`{}` imports `{imported}`{}, {place} (`{rel}`) that the installed package does \
                 not include",
                site.qualname,
                when_text(site, Some(module)),
            );
            let help = "move what the package needs from it into the package, or import it only \
                from tests and development scripts";
            cx.emit(
                out,
                "IMP104",
                unit,
                site.span,
                &site.qualname,
                imported.to_owned(),
                message,
                Some(help.to_owned()),
                &[],
            );
        }
    }
}
