//! What the code that runs while a module loads may read of what its imports bind
//! ([`super::ImportSite::read_while_loading`]).

use super::class_bodies;
use crate::resolver::stores::exported_names;
use crate::{CallSite, CallTarget, ModuleInput};
use heki_ir::walk::{Budget, Visit, reach};
use heki_ir::{InstKind, ModuleIr};
use heki_semantic::{ModuleImport, NameResolution, Scope, ScopeKind};
use rustc_hash::{FxHashMap, FxHashSet};

/// The module globals that code running while the module loads may read: the module body with
/// the class bodies, comprehensions and type-parameter scopes it runs, where its decorators,
/// defaults, bases, and the annotations the target version evaluates in place are read (from
/// the semantic model); the module's functions that run meanwhile ([`called_while_loading`],
/// from their IR); and a star import of the module, which reads what its `__all__` lists. By
/// name, wherever the read is: a read after the module rebinds the name, or under
/// `if TYPE_CHECKING:`, counts too.
pub(super) struct LoadReads<'s> {
    names: FxHashSet<&'s str>,
    /// What the module's `__all__` lists ([`exported_names`]).
    exports: FxHashSet<String>,
    /// The module binds an `__all__` heki cannot read: a star import may read any name.
    any: bool,
    /// An import of another scope binds a class attribute.
    module_scope: Option<heki_semantic::ScopeId>,
}

impl<'s> LoadReads<'s> {
    /// Of `input`, whose call sites are `calls`.
    pub(super) fn of(input: &'s ModuleInput<'_>, calls: &[CallSite]) -> Self {
        let scopes = &input.semantic.scopes;
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); scopes.len()];
        for (i, scope) in scopes.iter().enumerate() {
            if let Some(parent) = scope.parent.and_then(|p| children.get_mut(p.0 as usize)) {
                parent.push(i);
            }
        }
        let module = scopes.iter().position(|s| s.kind == ScopeKind::Module);
        let mut names = FxHashSet::default();
        // Unmetered: each scope is visited once, and the walk is never open.
        reach(
            &Budget::unmetered("import_sites"),
            module,
            |i, out| out.extend((children[i].iter()).filter(|&&c| runs_with_its_block(&scopes[c]))),
            |i| {
                let scope = &scopes[i];
                let reads = (scope.reads.iter()).filter(|(_, r)| reads_a_global(scope, *r));
                names.extend(reads.map(|(name, _)| name.as_str()));
                names.extend(scope.inlined_comp_only_reads.iter().map(String::as_str));
                Visit::<()>::Expand
            },
        )
        .open_means(None);
        // A function reads a global by `LoadGlobal`, and a class body it runs reads one it
        // does not bind by `LoadGlobal` or `LoadName`.
        let ir = input.ir;
        let called = called_while_loading(ir, &input.module_name, calls);
        let functions = (ir.functions.iter())
            .filter(|f| !f.is_class_body && called.contains(f.qualname.as_str()));
        for f in functions {
            let bodies = std::iter::once(f).chain(class_bodies(ir, f).into_iter().map(|c| c.body));
            for inst in bodies.flat_map(|g| &g.blocks).flat_map(|b| &b.insts) {
                if let InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } =
                    &inst.kind
                {
                    names.insert(name.as_str());
                }
            }
        }
        let binds_all =
            module.is_some_and(|m| scopes[m].bindings.iter().any(|b| b.name == "__all__"));
        let listed = binds_all.then(|| exported_names(input));
        Self {
            names,
            any: matches!(listed, Some(None)),
            exports: (listed.flatten().into_iter())
                .flat_map(|names| names.into_vec())
                .collect(),
            module_scope: module.map(|m| scopes[m].id),
        }
    }

    /// Whether code running while the module loads may read what `import` binds: the name it
    /// binds in the module, or, for an import in a class body, the class attribute, which heki
    /// does not follow through attribute reads.
    pub(super) fn reads(&self, import: &ModuleImport) -> bool {
        let name = import.bound_name.as_str();
        Some(import.scope) != self.module_scope
            || self.any
            || self.names.contains(name)
            || self.exports.contains(name)
    }
}

/// Whether a read in `scope` resolved `resolution` may read a module global: any read of the
/// module body, and of a class body, whose own names fall back to the globals while unbound;
/// a global or builtin read elsewhere (the module may bind a builtin's name).
fn reads_a_global(scope: &Scope, resolution: NameResolution) -> bool {
    match resolution {
        NameResolution::Global | NameResolution::Builtin => true,
        NameResolution::Local => matches!(scope.kind, ScopeKind::Module | ScopeKind::Class),
        NameResolution::Enclosing => false,
    }
}

/// Whether a scope runs when the block holding it runs: a class body, a comprehension, and the
/// type-parameter scope of a generic `def` or `class` (its bases, and its annotations where the
/// target version evaluates them in place). A function, a lambda, and the lazily evaluated
/// annotation, type-variable and type-alias scopes run later.
fn runs_with_its_block(scope: &Scope) -> bool {
    matches!(
        scope.kind,
        ScopeKind::Class | ScopeKind::Comprehension | ScopeKind::TypeParameters
    )
}

/// The qualified names of the functions of `ir`, the module `module`, that run while it loads:
/// those the module body calls by the project callees of `calls`, and those its `def` and
/// `class` statements and those of its class bodies apply as decorators, then those these call,
/// and the functions nested in them (a decorator a factory returns, a closure it calls).
fn called_while_loading<'c>(
    ir: &'c ModuleIr,
    module: &str,
    calls: &'c [CallSite],
) -> FxHashSet<&'c str> {
    let mut callees: FxHashMap<&str, Vec<&str>> = FxHashMap::default();
    for site in calls.iter().filter(|site| site.caller.module == module) {
        let CallTarget::Project(bound) = &site.target else {
            continue;
        };
        let here = bound.iter().filter(|b| b.callee.module == module);
        (callees.entry(site.caller.qualname.as_str()).or_default())
            .extend(here.map(|b| b.callee.qualname.as_str()));
    }
    let mut functions: Vec<&str> = (ir.functions.iter())
        .filter(|f| !f.is_module_body)
        .map(|f| f.qualname.as_str())
        .collect();
    functions.sort_unstable();
    functions.dedup();
    let decorators = (ir.functions.iter())
        .filter(|f| !f.qualname.contains("<locals>"))
        .flat_map(|f| &f.decorators)
        .filter_map(|d| {
            functions
                .binary_search(&d.as_str())
                .ok()
                .map(|i| functions[i])
        });
    let mut called: FxHashSet<&str> = FxHashSet::default();
    // Unmetered: each function is visited once, and the walk is never open.
    reach(
        &Budget::unmetered("import_sites"),
        std::iter::once("<module>").chain(decorators),
        |caller, out| {
            let prefix = format!("{caller}.<locals>.");
            let from = functions.partition_point(|q| *q < prefix.as_str());
            let to = from + functions[from..].partition_point(|q| q.starts_with(&prefix));
            out.extend(callees.get(caller).into_iter().flatten());
            out.extend(&functions[from..to]);
        },
        |f| {
            called.insert(f);
            Visit::<()>::Expand
        },
    )
    .open_means(None);
    called.remove("<module>");
    called
}
