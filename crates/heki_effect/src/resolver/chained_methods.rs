//! The methods of a class body that call the next definition of their name along the MRO on
//! every path to a return ([`super::ClassDecl::chains_to_next`]), for the names the ecosystem
//! catalog lists as methods through which a library call has its effect
//! (`heki_kb::EcosystemEntry::effect_methods`: a model's `save`, which `QuerySet.create` runs).
//! Such an override still runs the library's method each time it returns.

use super::init_containers::Paths;
use super::{LocalNames, ModuleDecls, NameBinding, is_summarized_function, self_aliases};
use crate::ModuleInput;
use heki_ir::walk::{Budget, FnShape};
use heki_ir::{FunctionKind, InstKind, IrFunction, VarId};
use heki_kb::EffectiveEcosystemKb;
use rustc_hash::FxHashMap;

/// [`super::ClassDecl::chains_to_next`] of the module's classes: a plain method (no decorator,
/// which may replace it) taking the instance as its first parameter, the only binding of its
/// name in the class body, of a module that binds no `super` of its own.
pub(super) fn chained_methods(
    input: &ModuleInput<'_>,
    decls: &mut ModuleDecls,
    kb: &EffectiveEcosystemKb,
) {
    let names = &kb.effect_method_names;
    if names.is_empty()
        || decls.star_imports.is_some()
        || !matches!(
            decls.lookup("super", false, &LocalNames::default()),
            NameBinding::Unbound
        )
    {
        return;
    }
    let functions = &input.ir.functions;
    // How many times each class body binds each of the names (a second `def`, an assignment).
    let mut bindings: FxHashMap<(&str, &str), usize> = FxHashMap::default();
    for body in functions.iter().filter(|f| f.is_class_body) {
        for inst in body.blocks.iter().flat_map(|b| &b.insts) {
            if let Some(name) = inst.kind.dst().and_then(|d| body.var_name(d))
                && names.contains(name)
            {
                *bindings.entry((body.qualname.as_str(), name)).or_default() += 1;
            }
        }
    }
    for f in functions {
        let (Some(owner), Some(this)) = (&f.owner_class_qualname, f.params.first()) else {
            continue;
        };
        if !names.contains(&f.name)
            || f.qualname != format!("{owner}.{}", f.name)
            || bindings.get(&(owner.as_str(), f.name.as_str())) != Some(&1)
            || f.kind != FunctionKind::Sync
            || !is_summarized_function(f)
            || !f.decorators.is_empty()
            || this.is_vararg
            || this.is_kwarg
            || this.kw_only
        {
            continue;
        }
        if calls_next_on_every_return(f, this.var, owner)
            && let Some(class) = decls.classes.get_mut(owner)
        {
            class.chains_to_next.insert(f.name.clone());
        }
    }
}

/// Whether every path from the entry of the method `f` of the class `owner` to a `return` calls
/// the method of its own name on `super()`, or on `super(Owner, self)` with `this` the
/// instance. Past the budget, it is not known to.
fn calls_next_on_every_return(f: &IrFunction, this: VarId, owner: &str) -> bool {
    let shape = FnShape::new(f);
    let selfs = self_aliases(f, this);
    let names_owner = |v: VarId| matches!(shape.inst(v).map(|i| &i.kind), Some(InstKind::LoadGlobal { name, .. }) if name == owner);
    let next_of_self = |recv: VarId| {
        matches!(
            shape.inst(shape.copy_root(recv)).map(|i| &i.kind),
            Some(InstKind::Call {
                callee,
                callee_var: Some(called),
                args,
                kwargs,
                star_from: None,
                ..
            }) if callee == "super"
                && kwargs.is_empty()
                && matches!(
                    shape.inst(*called).map(|i| &i.kind),
                    Some(InstKind::LoadGlobal { name, .. }) if name == "super"
                )
                && (args.is_empty()
                    || matches!(args[..], [class, instance] if names_owner(class) && selfs.contains(&instance)))
        )
    };
    let calls: Vec<(heki_ir::BlockId, usize)> = (f.blocks.iter())
        .flat_map(|b| (b.insts.iter().enumerate()).map(move |(i, inst)| (b.id, i, inst)))
        .filter(|(_, _, inst)| {
            matches!(&inst.kind, InstKind::MethodCall { recv, method, .. } if *method == f.name && next_of_self(*recv))
        })
        .map(|(b, i, _)| (b, i))
        .collect();
    let budget = Budget::of_function("resolver::chained_methods", f);
    Paths::of(&shape, &budget)
        .map(|paths| calls.iter().any(|at| paths.on_every_return(*at)))
        .open_means(false)
}
