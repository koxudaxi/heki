//! The path facts of a function (`heki_ir::Paths`) as the effect lane reads them, with what the
//! catalog and the project's declarations know beyond the IR.

use crate::resolver::{Callee, Symbol, TypeCtx, Val};
use heki_ir::walk::{Answer, Budget};
use heki_ir::{FnIndex, InstKind, IrFunction, PathReader, Paths, VarId};
use rustc_hash::FxHashMap;

/// Run `read` on the path facts of `f`, charged to `owner`'s budget. The reader knows the
/// module's bindings, which members modules have (a catalog entry, or a project declaration),
/// which methods a value's class has (likewise), and which context managers never suppress an
/// exception: an instance of a class, or the
/// result of a call, whose catalog entry says `exit_suppresses = false` (`types` are the
/// inferred values of `f`).
pub fn with_paths<R>(
    cx: &TypeCtx<'_>,
    f: &IrFunction,
    types: &FxHashMap<VarId, Val>,
    owner: &'static str,
    read: impl FnOnce(&Paths<'_, '_>, &FnIndex<'_>) -> R,
) -> R {
    let ix = FnIndex::new(f, &cx.ir().constants);
    let member = |module: &str, attr: &str| {
        let fqn = format!("{module}.{attr}");
        cx.kb.lookup(&fqn).is_some() || !matches!(cx.project.symbol(&fqn), Symbol::External)
    };
    let suppresses = |mgr: VarId| -> Answer<bool> {
        let catalogued = |symbol: &str| cx.kb.lookup(symbol).and_then(|e| e.exit_suppresses);
        let by_class = match types.get(&mgr) {
            Some(Val::Inst(class)) => catalogued(class),
            _ => None,
        };
        let by_factory = || {
            let callee = match &ix.shape().inst(mgr)?.kind {
                InstKind::Call { callee_var, .. } => {
                    cx.resolve_call(callee_var.and_then(|v| types.get(&v)))
                }
                InstKind::MethodCall { recv, method, .. } => {
                    cx.resolve_method(types.get(recv), method)
                }
                _ => return None,
            };
            match callee {
                Callee::External(symbol) => catalogued(&symbol),
                _ => None,
            }
        };
        by_class
            .or_else(by_factory)
            .map_or(Answer::Open, Answer::Known)
    };
    // A method of the value's class, by the catalog (R3) or the project's code (R2). A project
    // definition counts only when no decorator replaces it: reading a property runs its getter,
    // which may raise.
    let attribute = |obj: VarId, attr: &str| match types.get(&obj) {
        Some(Val::Inst(class)) => {
            cx.kb.lookup(&format!("{class}.{attr}")).is_some()
                || cx
                    .project
                    .mro_method(class, attr)
                    .and_then(|k| cx.project.function_decl(&k))
                    .is_some_and(|d| d.called_as_defined)
        }
        _ => false,
    };
    let reader = PathReader {
        bindings: Some(cx.bindings()),
        module_member: &member,
        attribute_found: &attribute,
        exit_suppresses: &suppresses,
    };
    let budget = Budget::of_function(owner, f);
    let paths = Paths::new(&ix, &reader, &budget);
    read(&paths, &ix)
}
