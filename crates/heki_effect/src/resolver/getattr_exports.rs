//! What a module's `__getattr__` (PEP 562) serves under the names no statement of the module
//! binds ([`ModuleDecls::getattr_exports`]). Python calls the hook for such a name, with the name;
//! heki follows the hook for a literal name where its code decides what the name gives: a branch
//! testing the name against literals (`if name == "Base":`, `name in ("Base", "Model")`, `name
//! in _EXPORTS`), or a value read from a table of the module keyed by the name (`_EXPORTS[name]`,
//! `_EXPORTS.get(name)`, also tested `is None`; a table being a module-level dict display of
//! string keys nothing changes in place, [`ModuleDecls::tables`]). The name is served what every
//! `return` it may reach gives: the table's value, what calling it or a function of the module
//! gives by the function's declarations (a class it defines in its body, also behind
//! `functools.lru_cache`, or the class object its annotation names), or the value any other
//! `return` gives, whatever the name. A `return` of anything else, of nothing, or of different
//! objects leaves the name unknown.
//!
//! A return may also hand the table's value to a function of the module that calls it when it
//! is a function object (`return _load(name, _EXPORTS[name])`, where `_load` returns `entry()`
//! past `isinstance(entry, str)` and `isinstance(entry, tuple)` tests): that serves what calling
//! the value gives.
//!
//! The hook may also be what a project function returns (`__getattr__, __dir__ =
//! make_hooks(__name__, _EXPORTS)`, [`GetattrExports::made_by`]): the function's module records
//! what the hook serves for a key of the table it takes, read through a closure cell bound to
//! that parameter, and under the literal names its code decides ([`hook_factories`]), and the
//! project puts the call's arguments in ([`Instance`]).
//!
//! Like the return facts, it is pass 1: inferred against the module's own declarations
//! completed against themselves, so it stays a function of the module's text. What other
//! modules change is the declarations' to see ([`ProjectDecls::served_by_getattr`]).

use super::{
    FunctionDecl, GetattrExports, HookArg, HookParam, KeyServes, MadeBy, MadeHook, ModuleDecls,
    ProjectDecls, Symbol, Ty, TypeCtx, complete_decls,
};
use crate::ModuleInput;
use crate::infer::ParamCells;
use heki_ir::walk::{Answer, Budget, Facts, FnShape, Join, KeySet, Sparse, Visit, fixpoint, reach};
use heki_ir::{
    BlockId, BuiltinKind, CompareOpKind, ConstLiteral, FunctionKind, GlobalStores, InstKind,
    IrFunction, ModuleIr, Terminator, UnaryOpKind, VarId,
};
use heki_kb::EffectiveEcosystemKb;
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

/// [`ModuleDecls::getattr_exports`] of the module `input` declares as `decls`, after its return
/// facts: `None` for a module serving no name heki follows, and past the budget. A module binding
/// names behind the IR (`exec`, a change through `globals()`, also the hook caching what it
/// serves) may bind a name before the hook runs, and one binding the hook's name again
/// (`__getattr__ = other`) may hold another hook: they serve none.
pub(super) fn getattr_exports(
    input: &ModuleInput<'_>,
    decls: &ModuleDecls,
    kb: &EffectiveEcosystemKb,
) -> Option<GetattrExports> {
    let hook_bound_again = decls.rebound_definitions.contains("__getattr__")
        || decls.imports.contains_key("__getattr__")
        || decls.aliases.contains_key("__getattr__")
        || decls.alias_paths.contains_key("__getattr__");
    if input.ir.constants.dynamic || hook_bound_again {
        return None;
    }
    let hook_global = format!("{}.__getattr__", decls.module_name);
    let Some((hook, name)) = heki_ir::module_getattr_hook(input.ir) else {
        let made = made_by(input, decls)?;
        return Some(GetattrExports {
            served: BTreeMap::new(),
            globals: [hook_global].into(),
            made_by: Some(Box::new(made)),
        });
    };
    let shape = FnShape::new(hook);
    decides_names(&shape, name).then_some(())?;
    let completed = own_completed(input, decls, kb);
    let cx = TypeCtx::new(input, &completed, kb);
    let decider = Decider {
        ir: input.ir,
        cx: &cx,
        project: &completed,
        module: &decls.module_name,
    };
    let decided = decider.hook_serves(&shape, name, None, &[])?;
    // A name the module binds only under `if TYPE_CHECKING:` is no name it binds at run time.
    let unbound = |key: &str| !decls.binds(key) || decls.type_checking_only.contains(key);
    let mut globals = own_globals(&shape, decls);
    globals.push(hook_global);
    let mut served = BTreeMap::new();
    for (key, (ty, read)) in decided.served {
        if unbound(&key) {
            globals.extend(read);
            served.insert(key, ty);
        }
    }
    globals.sort();
    globals.dedup();
    (!served.is_empty()).then_some(())?;
    Some(GetattrExports {
        served,
        globals: globals.into(),
        made_by: None,
    })
}

/// The module's own declarations completed against themselves, which the hooks are read against.
fn own_completed(
    input: &ModuleInput<'_>,
    decls: &ModuleDecls,
    kb: &EffectiveEcosystemKb,
) -> ProjectDecls {
    let own = ProjectDecls::new([decls.clone()]);
    ProjectDecls::new([complete_decls(input, decls.clone(), &own, kb)])
}

/// The call of a project function that the module binds its `__getattr__` to, once, at module
/// level and outside any loop ([`GetattrExports::made_by`]): `make_hook(...)`,
/// `make_hooks(...)[0]`, or a target of unpacking what it returns (`__getattr__, __dir__ =
/// make_hooks(...)`), passing no `*` or `**` unpacking. As for a `def`, no code of the module
/// loads the hook.
fn made_by(input: &ModuleInput<'_>, decls: &ModuleDecls) -> Option<MadeBy> {
    let ir = input.ir;
    let loads_hook = |kind: &InstKind| {
        matches!(kind, InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. }
            if name == "__getattr__")
    };
    let loaded = (ir.functions.iter()).any(|f| {
        f.blocks
            .iter()
            .flat_map(|b| &b.insts)
            .any(|i| loads_hook(&i.kind))
    });
    if loaded || !GlobalStores::of(&ir.functions).once("__getattr__") {
        return None;
    }
    let body = ir.function(ir.module_fn);
    let shape = FnShape::new(body);
    let value = (body.blocks.iter())
        .filter(|b| shape.loop_depth(b.id) == 0)
        .flat_map(|b| &b.insts)
        .find_map(|i| match &i.kind {
            InstKind::StoreGlobal { name, value } if name == "__getattr__" => Some(*value),
            _ => None,
        })?;
    // The module body holds its names in values: the global a value is, by the store of it.
    let mut stored: FxHashMap<VarId, &String> = FxHashMap::default();
    for i in body.blocks.iter().flat_map(|b| &b.insts) {
        if let InstKind::StoreGlobal { name, value } = &i.kind {
            stored.entry(shape.copy_root(*value)).or_insert(name);
        }
    }
    let global = |v: VarId| {
        let root = shape.copy_root(v);
        match &shape.inst(root)?.kind {
            InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } => Some(name),
            _ => stored.get(&root).copied(),
        }
    };
    let mut call = shape.copy_root(value);
    let mut item = None;
    if let InstKind::SubscriptLoad {
        obj,
        index,
        is_slice: false,
        ..
    } = &shape.inst(call)?.kind
    {
        item = Some(u8::try_from(shape.int_const(shape.copy_root(*index))?).ok()?);
        call = shape.copy_root(*obj);
    }
    let InstKind::Call {
        callee_var: Some(callee),
        args,
        kwargs,
        star_from: None,
        ..
    } = &shape.inst(call)?.kind
    else {
        return None;
    };
    let callee = global(*callee)?;
    let factory = match decls.imports.get(callee) {
        Some(target) => target.clone(),
        None => (decls.functions.contains_key(callee))
            .then(|| format!("{}.{callee}", decls.module_name))?,
    };
    let arg = |a: VarId| match shape.inst(shape.copy_root(a)).map(|i| &i.kind) {
        Some(InstKind::Const { literal, .. }) => HookArg::Literal(literal.clone()),
        _ => global(a).map_or(HookArg::Other, |name| HookArg::Global(name.clone())),
    };
    let kwargs = (kwargs.iter())
        .map(|(key, a)| Some((key.clone()?, arg(*a))))
        .collect::<Option<Vec<_>>>()?;
    Some(MadeBy {
        factory,
        item,
        args: args.iter().map(|a| arg(*a)).collect(),
        kwargs,
    })
}

/// The entries of a table: each key with the type of its value ([`super::Table`]).
type Entries<'t> = &'t [(Box<str>, Option<Ty>)];

/// The functions taking a name a factory returns ([`returned_hooks`]).
type ReturnedHooks<'m> = Vec<(Option<u8>, &'m IrFunction, VarId)>;

/// The key standing for every key of a factory's table in the walk over a hook it makes: no
/// name a module binds or a literal spells it.
const ANY_KEY: &str = "\0key";

/// What a hook decides ([`Decider::hook_serves`]).
#[derive(Default)]
struct Decided {
    /// By literal name, the named object or class object served, with the module globals
    /// deciding it beyond those the hook reads.
    served: BTreeMap<String, (Ty, Vec<String>)>,
    /// What a key of the factory's table gets.
    keyed: Option<KeyServes>,
    /// The literal names the hook's tests name, and those it decides.
    literals: BTreeSet<String>,
}

/// Reads hooks against the declarations of their module.
struct Decider<'d> {
    ir: &'d ModuleIr,
    cx: &'d TypeCtx<'d>,
    project: &'d ProjectDecls,
    module: &'d str,
}

impl Decider<'_> {
    /// What the hook `shape` serves under the names it decides, given in `name`, with the
    /// closure cell `table_cell` read as a table of any keys ([`ANY_KEY`]); `None` past the
    /// budget, which covers the hook, the module body, and `more`.
    fn hook_serves(
        &self,
        shape: &FnShape<'_>,
        name: VarId,
        table_cell: Option<&str>,
        more: &[&IrFunction],
    ) -> Option<Decided> {
        let hook = shape.f;
        let any_keys: [(Box<str>, Option<Ty>); 1] =
            [(Box::from(ANY_KEY), Some(Ty::Ref(ANY_KEY.to_owned())))];
        let module_body = &self.ir.functions[self.ir.module_fn.0 as usize];
        let called = loaded_functions(self.ir, hook);
        let budget = Budget::of_functions(
            "heki_effect::resolver::getattr_exports",
            [hook, module_body]
                .into_iter()
                .chain(called)
                .chain(more.iter().copied()),
        );
        let reads = Hook {
            shape,
            name,
            decider: self,
            table_cell: table_cell.map(|cell| (cell, &any_keys[..])),
            budget: &budget,
            dispatches: RefCell::default(),
        };
        let reaching = reads.names_reaching(&budget).map(Some).open_means(None)?;
        let returns: Vec<(Option<VarId>, &Reaching<'_>)> = (shape.rpo().iter().enumerate())
            .filter_map(|(i, b)| match hook.block(*b).terminator {
                Terminator::Return(value) => Some((value, reaching.get(i)?)),
                _ => None,
            })
            .collect();
        // The names the hook decides: those a branch lets reach a `return`, and the keys of the
        // tables a `return` any name reaches reads.
        let mut names: BTreeSet<&str> = BTreeSet::new();
        for (value, reach) in &returns {
            match reach {
                Reaching::Only(only) => names.extend(only.iter().copied()),
                Reaching::AllBut(but) => names.extend(
                    (value.and_then(|v| reads.keyed_return(v)).into_iter())
                        .flat_map(|(keyed, _)| keyed.entries.iter().map(|(key, _)| &**key))
                        .filter(|key| !but.contains(key)),
                ),
            }
        }
        let mut served: BTreeMap<&str, Served> = names
            .into_iter()
            .map(|key| (key, Served::Nothing))
            .collect();
        let inferred = self.cx.infer_types(hook);
        for (value, reach) in &returns {
            let keys: Vec<&str> = match reach {
                Reaching::Only(only) => (only.iter().copied())
                    .filter(|key| served.contains_key(key))
                    .collect(),
                Reaching::AllBut(but) => (served.keys().copied())
                    .filter(|key| !but.contains(key))
                    .collect(),
            };
            for key in keys {
                budget.take(1).then_some(())?;
                let given = match value {
                    Some(v) => reads.value_for(*v, key, |v| inferred.ty(v).cloned()),
                    // `return` alone, or the end of the body: `None`.
                    None => Served::Open,
                };
                let slot = served.get_mut(key)?;
                *slot = std::mem::replace(slot, Served::Nothing).join(given);
            }
        }
        let mut out = Decided {
            literals: reads.tested_literals(),
            ..Decided::default()
        };
        for (key, served) in served {
            if key == ANY_KEY {
                out.keyed = match served {
                    Served::Entry(serves) => Some(serves),
                    _ => None,
                };
                continue;
            }
            out.literals.insert(key.to_owned());
            if let Served::Gives(ty @ (Ty::Ref(_) | Ty::Class(_)), read) = served {
                out.served.insert(key.to_owned(), (ty, read));
            }
        }
        Some(out)
    }
}

/// The functions of the module the hook `f` loads by name, which a walk of the hook may read.
fn loaded_functions<'m>(ir: &'m ModuleIr, f: &IrFunction) -> Vec<&'m IrFunction> {
    let mut out: Vec<&IrFunction> = (f.blocks.iter().flat_map(|b| &b.insts))
        .filter_map(|i| match &i.kind {
            InstKind::LoadGlobal { name, .. } => own_function(ir, name),
            _ => None,
        })
        .collect();
    out.sort_by_key(|g| g.id);
    out.dedup_by_key(|g| g.id);
    out
}

/// The module's only plain `def` of the name `name` at module level: undecorated, so the name
/// holds what the statement made, unless something binds it again.
fn own_function<'m>(ir: &'m ModuleIr, name: &str) -> Option<&'m IrFunction> {
    let mut named = (ir.functions.iter())
        .filter(|f| f.qualname == name && f.enclosing == Some(ir.module_fn) && !f.is_class_body);
    let f = named.next()?;
    let plain = f.kind == FunctionKind::Sync && f.decorators.is_empty();
    (plain && named.next().is_none()).then_some(f)
}

/// Whether the hook may decide a name heki follows: it compares its name parameter `name` with
/// something, or reads a value keyed by it.
fn decides_names(shape: &FnShape<'_>, name: VarId) -> bool {
    let is_name = |v: VarId| shape.copy_root(v) == name;
    shape
        .f
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .any(|i| match &i.kind {
            InstKind::Compare { lhs, rhs, .. } => is_name(*lhs) || is_name(*rhs),
            InstKind::SubscriptLoad { index, .. } => is_name(*index),
            InstKind::MethodCall { method, args, .. } => {
                method == "get" && args.first().is_some_and(|a| is_name(*a))
            }
            _ => false,
        })
}

/// The names that may reach a block ([`Hook::names_reaching`]): only those listed, or every
/// name but those listed (every name at the entry).
#[derive(Debug, Clone, PartialEq)]
enum Reaching<'n> {
    Only(KeySet<&'n str>),
    AllBut(KeySet<&'n str>),
}

impl<'n> Reaching<'n> {
    fn all() -> Self {
        Reaching::AllBut(KeySet::new())
    }

    /// The names of both `self` and `test`, the names a test lets take an edge.
    fn within(&self, test: &Self) -> Self {
        let minus = |a: &KeySet<&'n str>, b: &KeySet<&'n str>| {
            a.iter().filter(|n| !b.contains(n)).copied().collect()
        };
        match (self, test) {
            (Reaching::Only(a), Reaching::Only(b)) => {
                Reaching::Only(a.iter().filter(|n| b.contains(n)).copied().collect())
            }
            (Reaching::Only(a), Reaching::AllBut(b)) => Reaching::Only(minus(a, b)),
            (Reaching::AllBut(a), Reaching::Only(b)) => Reaching::Only(minus(b, a)),
            (Reaching::AllBut(a), Reaching::AllBut(b)) => {
                let mut both = a.clone();
                both.join(b);
                Reaching::AllBut(both)
            }
        }
    }
}

impl Join for Reaching<'_> {
    fn join(&mut self, other: &Self) -> bool {
        let joined = match (&*self, other) {
            (Reaching::Only(a), Reaching::Only(b)) => {
                let mut both = a.clone();
                both.join(b);
                Reaching::Only(both)
            }
            (Reaching::Only(a), Reaching::AllBut(b)) | (Reaching::AllBut(b), Reaching::Only(a)) => {
                Reaching::AllBut(b.iter().filter(|n| !a.contains(n)).copied().collect())
            }
            (Reaching::AllBut(a), Reaching::AllBut(b)) => {
                Reaching::AllBut(a.iter().filter(|n| b.contains(n)).copied().collect())
            }
        };
        let grew = joined != *self;
        *self = joined;
        grew
    }

    fn weight(&self) -> usize {
        match self {
            Reaching::Only(only) => only.weight(),
            Reaching::AllBut(but) => 1 + but.len(),
        }
    }
}

/// What the hook serves under one name so far: nothing yet (every `return` it reached raises
/// first), one object with the module globals deciding it, what a key of a factory's table gets
/// ([`ANY_KEY`]), or anything.
#[derive(Debug, Clone, PartialEq)]
enum Served {
    Nothing,
    Gives(Ty, Vec<String>),
    Entry(KeyServes),
    Open,
}

impl Served {
    fn join(self, other: Self) -> Self {
        match (self, other) {
            (Served::Nothing, s) | (s, Served::Nothing) => s,
            (Served::Gives(a, mut read), Served::Gives(b, more)) if a == b => {
                read.extend(more);
                Served::Gives(a, read)
            }
            (Served::Entry(a), Served::Entry(b)) if a == b => Served::Entry(a),
            _ => Served::Open,
        }
    }
}

/// How a value is read from a table by the name ([`Keyed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lookup {
    /// `table[name]`, which raises `KeyError` for a missing key.
    Subscript,
    /// `table.get(name)`, `None` for a missing key.
    Get,
    /// `table.get(name, default)`.
    GetOr,
}

/// A value read from a table by the hook's name: the table's entries, and how. A factory's
/// table holds one entry for any of its keys ([`ANY_KEY`]).
struct Keyed<'t> {
    entries: &'t [(Box<str>, Option<Ty>)],
    lookup: Lookup,
    any_keys: bool,
}

impl Keyed<'_> {
    /// The table's value under `key`: `Some(None)` for a value of no type heki knows, `None` for
    /// a missing key.
    fn entry(&self, key: &str) -> Option<Option<&Ty>> {
        (self.entries.iter())
            .find(|(k, _)| &**k == key)
            .map(|(_, t)| t.as_ref())
    }
}

/// The hook being read: its shape, its name parameter, the declarations of its module, and the
/// closure cell holding the table of a factory making it, with that table's one entry.
struct Hook<'h> {
    shape: &'h FnShape<'h>,
    name: VarId,
    decider: &'h Decider<'h>,
    table_cell: Option<(&'h str, Entries<'h>)>,
    budget: &'h Budget,
    /// [`Hook::calls_its_param`] by function and parameter.
    dispatches: RefCell<FxHashMap<(&'h str, usize), bool>>,
}

impl<'h> Hook<'h> {
    fn is_name(&self, v: VarId) -> bool {
        self.shape.copy_root(v) == self.name
    }

    /// The module global `v` loads (through copies).
    fn global_loaded(&self, v: VarId) -> Option<&'h str> {
        match &self.shape.inst(self.shape.copy_root(v))?.kind {
            InstKind::LoadGlobal { name, .. } => Some(name),
            _ => None,
        }
    }

    /// The entries of the table `v` loads: a table of the module, or the one of a factory
    /// (`true`).
    fn loaded_table(&self, v: VarId) -> Option<(Entries<'h>, bool)> {
        if let Some((cell, entries)) = self.table_cell
            && matches!(self.shape.inst(self.shape.copy_root(v)).map(|i| &i.kind),
                Some(InstKind::LoadDeref { name, .. }) if name == cell)
        {
            return Some((entries, true));
        }
        let d = self.decider;
        let global = self.global_loaded(v)?;
        Some((d.project.table(&format!("{}.{global}", d.module))?, false))
    }

    /// The value `v` is (through copies) when it is read from a table by the hook's name.
    fn keyed_value(&self, v: VarId) -> Option<Keyed<'h>> {
        let (table, lookup) = match &self.shape.inst(self.shape.copy_root(v))?.kind {
            InstKind::SubscriptLoad {
                obj,
                index,
                is_slice: false,
                ..
            } if self.is_name(*index) => (*obj, Lookup::Subscript),
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                star_from: None,
                ..
            } if method == "get" && kwargs.is_empty() => match args.as_slice() {
                [key] if self.is_name(*key) => (*recv, Lookup::Get),
                [key, _] if self.is_name(*key) => (*recv, Lookup::GetOr),
                _ => return None,
            },
            _ => return None,
        };
        let (entries, any_keys) = self.loaded_table(table)?;
        Some(Keyed {
            entries,
            lookup,
            any_keys,
        })
    }

    /// The value read from a table by the name that the returned value `v` is, or whose call
    /// it gives (`true`): calling it, or a function of the module calling what it is handed
    /// ([`Self::dispatched`]).
    fn keyed_return(&self, v: VarId) -> Option<(Keyed<'h>, bool)> {
        let called =
            (self.callee(v).and_then(|c| self.keyed_value(c))).or_else(|| self.dispatched(v));
        match called {
            Some(keyed) => Some((keyed, true)),
            None => Some((self.keyed_value(v)?, false)),
        }
    }

    /// The value read from a table by the name that the call `v` of a function of the module
    /// passes by position, when calling the function with a function object there gives what
    /// calling the object gives ([`Self::calls_its_param`]).
    fn dispatched(&self, v: VarId) -> Option<Keyed<'h>> {
        let InstKind::Call {
            callee_var: Some(callee),
            args,
            kwargs,
            star_from: None,
            ..
        } = &self.shape.inst(self.shape.copy_root(v))?.kind
        else {
            return None;
        };
        let function = self.global_loaded(*callee).filter(|_| kwargs.is_empty())?;
        let (param, keyed) =
            (args.iter().enumerate()).find_map(|(i, a)| Some((i, self.keyed_value(*a)?)))?;
        self.calls_its_param(function, param).then_some(keyed)
    }

    /// Whether calling the module's function `function` with a function object at position
    /// `param` returns what calling that object gives: every `return` reached is a call of the
    /// parameter passing nothing, where a test `isinstance(param, C)` naming only classes of
    /// data values (`str`, `tuple`, ...), which a function object is no instance of, takes its
    /// false way. Any other test takes both.
    fn calls_its_param(&self, function: &'h str, param: usize) -> bool {
        let d = self.decider;
        *(self.dispatches.borrow_mut().entry((function, param))).or_insert_with(|| {
            let own = matches!(
                d.project.symbol(&format!("{}.{function}", d.module)),
                Symbol::Function(..)
            );
            own && own_function(d.ir, function)
                .is_some_and(|f| calls_param(d, f, param, self.budget).is_some())
        })
    }

    /// The value the call `v` is (through copies) calls.
    fn callee(&self, v: VarId) -> Option<VarId> {
        match &self.shape.inst(self.shape.copy_root(v))?.kind {
            InstKind::Call {
                callee_var: Some(c),
                ..
            } => Some(*c),
            _ => None,
        }
    }

    /// The names that may take each edge of the branch on `cond`, then and else, when its test
    /// decides them: `None` for a test of anything else.
    fn test(&self, cond: VarId) -> Option<(Reaching<'h>, Reaching<'h>)> {
        let mut negated = false;
        let mut v = self.shape.copy_root(cond);
        loop {
            match &self.shape.inst(v)?.kind {
                InstKind::UnaryOp {
                    op: UnaryOpKind::Not,
                    operand,
                    ..
                } => {
                    negated = !negated;
                    v = self.shape.copy_root(*operand);
                }
                InstKind::Compare { op, lhs, rhs, .. } => {
                    let (holds, fails) = self.compared(*op, *lhs, *rhs)?;
                    return Some(if negated {
                        (fails, holds)
                    } else {
                        (holds, fails)
                    });
                }
                _ => return None,
            }
        }
    }

    /// The names that may make the comparison `lhs op rhs` true and those that may make it
    /// false: `==` and `in` hold for the literals tested and fail for every other name;
    /// `table.get(name) is not None` holds only for the keys of the table and fails for every
    /// name but the keys whose value heki knows to be no `None`.
    fn compared(
        &self,
        op: CompareOpKind,
        lhs: VarId,
        rhs: VarId,
    ) -> Option<(Reaching<'h>, Reaching<'h>)> {
        let (holds, fails) = match op {
            CompareOpKind::Eq | CompareOpKind::NotEq => {
                let literal = |name: VarId, other: VarId| {
                    let other = self.shape.copy_root(other);
                    self.is_name(name)
                        .then(|| self.shape.str_const(other))
                        .flatten()
                };
                let literal = literal(lhs, rhs).or_else(|| literal(rhs, lhs))?;
                let tested = KeySet::from_iter([literal]);
                (Reaching::Only(tested.clone()), Reaching::AllBut(tested))
            }
            CompareOpKind::In | CompareOpKind::NotIn => {
                let members = self.is_name(lhs).then(|| self.members(rhs)).flatten()?;
                (Reaching::Only(members.clone()), Reaching::AllBut(members))
            }
            CompareOpKind::Is | CompareOpKind::IsNot => {
                let is_none = |v: VarId| {
                    matches!(
                        self.shape.inst(self.shape.copy_root(v)).map(|i| &i.kind),
                        Some(InstKind::Const {
                            literal: ConstLiteral::None,
                            ..
                        })
                    )
                };
                let looked = if is_none(rhs) { lhs } else { rhs };
                let keyed = (self.keyed_value(looked))
                    .filter(|keyed| keyed.lookup == Lookup::Get && is_none(lhs) != is_none(rhs))?;
                let keys = keyed.entries.iter().map(|(key, _)| &**key).collect();
                let objects = (keyed.entries.iter())
                    .filter(|(_, t)| matches!(t, Some(Ty::Ref(_) | Ty::Class(_))))
                    .map(|(key, _)| &**key)
                    .collect();
                (Reaching::Only(keys), Reaching::AllBut(objects))
            }
            _ => return None,
        };
        let negated = matches!(
            op,
            CompareOpKind::NotEq | CompareOpKind::NotIn | CompareOpKind::Is
        );
        Some(if negated {
            (fails, holds)
        } else {
            (holds, fails)
        })
    }

    /// The string literals `name in v` tests the name against: a display of literals, or the
    /// keys of a table.
    fn members(&self, v: VarId) -> Option<KeySet<&'h str>> {
        match &self.shape.inst(self.shape.copy_root(v))?.kind {
            InstKind::AllocBuiltin {
                kind: BuiltinKind::Tuple | BuiltinKind::List | BuiltinKind::Set,
                elems,
                ..
            } => (elems.iter())
                .map(|e| self.shape.str_const(self.shape.copy_root(*e)))
                .collect(),
            _ => Some(
                (self.loaded_table(v)?.0.iter())
                    .map(|(key, _)| &**key)
                    .collect(),
            ),
        }
    }

    /// The names that may reach each block, by its position in reverse postorder: any name at
    /// the entry, and along each edge the names that reach its block and that the branch's
    /// test lets take it. An exception edge is taken by every name reaching its block.
    fn names_reaching(&self, budget: &Budget) -> Answer<Facts<Reaching<'h>>> {
        let f = self.shape.f;
        let rpo = self.shape.rpo();
        let node = |b: heki_ir::BlockId| self.shape.rpo_index(b).map(|i| i as usize);
        let tests: Vec<Option<(Reaching<'h>, Reaching<'h>)>> = (rpo.iter())
            .map(|b| match f.block(*b).terminator {
                Terminator::Branch { cond, .. } => self.test(cond),
                _ => None,
            })
            .collect();
        let transfer = |i: usize, facts: &Facts<Reaching<'h>>| {
            if i == 0 {
                return Some(Reaching::all());
            }
            let b = rpo[i];
            let mut entry: Option<Reaching<'h>> = None;
            let mut add = |names: Reaching<'h>| match &mut entry {
                Some(e) => {
                    e.join(&names);
                }
                None => entry = Some(names),
            };
            let reached = (self.shape.preds(b).iter())
                .filter_map(|&p| Some((p, node(p).and_then(|n| Some((n, facts.get(n)?)))?)));
            for (p, (n, reach)) in reached {
                let pb = f.block(p);
                match (&pb.terminator, &tests[n]) {
                    (
                        Terminator::Branch {
                            then_bb, else_bb, ..
                        },
                        Some((then_names, else_names)),
                    ) => {
                        if *then_bb == b {
                            add(reach.within(then_names));
                        }
                        if *else_bb == b {
                            add(reach.within(else_names));
                        }
                    }
                    (terminator, _) if terminator.successors().contains(&b) => add(reach.clone()),
                    _ => {}
                }
                if pb.exc_target == Some(b) {
                    add(reach.clone());
                }
            }
            entry
        };
        let readers = |i: usize, out: &mut Vec<usize>| {
            let block = f.block(rpo[i]);
            out.extend(
                (block.terminator.successors().into_iter())
                    .chain(block.exc_target)
                    .filter_map(node),
            );
        };
        // A block is visited once, then once per name a predecessor's names gain, and once
        // when they become any name.
        let names: usize = (tests.iter().flatten())
            .map(|(a, b)| a.weight() + b.weight())
            .sum();
        let most_preds = (rpo.iter().map(|b| self.shape.preds(*b).len()).max()).unwrap_or(0);
        let spec = Sparse {
            nodes: rpo.len(),
            readers: &readers,
            max_visits: u32::try_from(2 + most_preds * (2 + names)).unwrap_or(u32::MAX),
        };
        fixpoint(budget, spec, transfer)
    }

    /// What the hook serves under `key` when it returns `v`, with the module globals deciding
    /// it beyond those the hook reads: a value read from a table by the name, what calling such
    /// a value or a function of the module gives, or what `v` is whatever the name (`typed`).
    fn value_for(&self, v: VarId, key: &str, typed: impl Fn(VarId) -> Option<Ty>) -> Served {
        match self.keyed_return(v) {
            Some((keyed, called)) => match (keyed.entry(key), called) {
                // `None()` raises, and so does a missing key of a subscript.
                (None, true) if keyed.lookup != Lookup::GetOr => Served::Nothing,
                (None, false) if keyed.lookup == Lookup::Subscript => Served::Nothing,
                (Some(_), true) if keyed.any_keys => Served::Entry(KeyServes::Called),
                (Some(_), false) if keyed.any_keys => Served::Entry(KeyServes::Value),
                (Some(Some(Ty::Ref(f))), true) => self.called(f),
                (Some(Some(t)), false) => Served::Gives(t.clone(), Vec::new()),
                _ => Served::Open,
            },
            None => match self.own_callee(v) {
                Some(f) => self.called(&f),
                None => typed(v).map_or(Served::Open, |t| Served::Gives(t, Vec::new())),
            },
        }
    }

    /// What calling `f` gives by the declarations of its module, when it is one of its
    /// functions ([`super::FunctionDecl::declared_return_as_named`]).
    fn called(&self, f: &str) -> Served {
        let given = match self.decider.project.symbol(f) {
            Symbol::Function(_, decl) => call_gives(decl),
            _ => None,
        };
        given.map_or(Served::Open, |(t, read)| Served::Gives(t, read))
    }

    /// The function of the module the call `v` calls by its name, by fqn.
    fn own_callee(&self, v: VarId) -> Option<String> {
        let d = self.decider;
        let fqn = format!("{}.{}", d.module, self.global_loaded(self.callee(v)?)?);
        matches!(d.project.symbol(&fqn), Symbol::Function(..)).then_some(fqn)
    }

    /// The literal names the hook's tests name.
    fn tested_literals(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for b in self.shape.rpo() {
            let Terminator::Branch { cond, .. } = self.shape.f.block(*b).terminator else {
                continue;
            };
            for reach in self.test(cond).into_iter().flat_map(|(a, b)| [a, b]) {
                let (Reaching::Only(names) | Reaching::AllBut(names)) = reach;
                out.extend(
                    (names.iter())
                        .filter(|n| **n != ANY_KEY)
                        .map(|n| (*n).to_owned()),
                );
            }
        }
        out
    }
}

/// What calling the function `decl` declares gives by the declarations of its module
/// ([`FunctionDecl::declared_return_as_named`]), with the globals it was inferred from.
fn call_gives(decl: &FunctionDecl) -> Option<(Ty, Vec<String>)> {
    (decl.declared_return_as_named()).map(|(t, read)| (decl.call_type(t), read.to_vec()))
}

/// The module's own globals the hook `shape` reads (no imports, which name another module's
/// object) ([`GetattrExports::globals`]).
fn own_globals(shape: &FnShape<'_>, decls: &ModuleDecls) -> Vec<String> {
    let mut out: Vec<String> = (shape.f.blocks.iter())
        .flat_map(|b| &b.insts)
        .filter_map(|i| match &i.kind {
            InstKind::LoadGlobal { name, .. } if !decls.imports.contains_key(name) => {
                Some(format!("{}.{name}", decls.module_name))
            }
            _ => None,
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// [`Hook::calls_its_param`] of the function `f` and its parameter at `param`, walking the
/// blocks a function object there may reach, each once, within `budget`: `None` when it does
/// not hold.
fn calls_param(d: &Decider<'_>, f: &IrFunction, param: usize, budget: &Budget) -> Option<()> {
    let p = (f.params.get(param)).filter(|p| !p.is_vararg && !p.is_kwarg && !p.kw_only)?;
    let shape = FnShape::new(f);
    let is_param = |v: VarId| shape.copy_root(v) == p.var;
    let inferred = d.cx.infer_types(f);
    let classes = inferred.isinstance_args(f, d.project);
    // The way of the branch on `cond` a function object at the parameter takes, when the test
    // decides it: `Some(true)` for the true way.
    let taken = |cond: VarId| {
        let mut negated = false;
        let mut v = shape.copy_root(cond);
        loop {
            match &shape.inst(v)?.kind {
                InstKind::UnaryOp {
                    op: UnaryOpKind::Not,
                    operand,
                    ..
                } => {
                    negated = !negated;
                    v = shape.copy_root(*operand);
                }
                InstKind::Call {
                    callee_var: Some(c),
                    args,
                    kwargs,
                    star_from: None,
                    ..
                } if kwargs.is_empty()
                    && matches!(inferred.ty(*c), Some(Ty::Ref(n)) if n == "builtins.isinstance") =>
                {
                    let (object, class) = (args.first()?, args.get(1)?);
                    let data = (classes.get(class))
                        .is_some_and(|cs| cs.iter().all(|c| DATA_CLASSES.contains(&c.as_str())));
                    return (is_param(*object) && data).then_some(negated);
                }
                _ => return None,
            }
        }
    };
    let returns_call = |v: Option<VarId>| {
        v.is_some_and(|v| {
            matches!(
                shape.inst(shape.copy_root(v)).map(|i| &i.kind),
                Some(InstKind::Call { callee_var: Some(c), args, kwargs, star_from: None, .. })
                    if is_param(*c) && args.is_empty() && kwargs.is_empty()
            )
        })
    };
    let mut returns = 0usize;
    let next = |b: BlockId, out: &mut Vec<BlockId>| {
        let block = f.block(b);
        out.extend(block.exc_target);
        match block.terminator {
            Terminator::Branch {
                cond,
                then_bb,
                else_bb,
            } => match taken(cond) {
                Some(true) => out.push(then_bb),
                Some(false) => out.push(else_bb),
                None => out.extend([then_bb, else_bb]),
            },
            ref other => out.extend(other.successors()),
        }
    };
    // Stops at a `return` of anything else.
    let other_return = reach(budget, [BlockId(0)], next, |b| {
        match f.block(b).terminator {
            Terminator::Return(v) => {
                let calls = returns_call(v);
                returns += usize::from(calls);
                if calls {
                    Visit::Expand
                } else {
                    Visit::Stop(())
                }
            }
            _ => Visit::Expand,
        }
    });
    matches!(other_return, Answer::Known(None)).then_some(())?;
    (returns > 0).then_some(())
}

/// The builtin classes of data values, no instance of which is a function object.
const DATA_CLASSES: &[&str] = &[
    "builtins.str",
    "builtins.bytes",
    "builtins.bytearray",
    "builtins.tuple",
    "builtins.list",
    "builtins.dict",
    "builtins.set",
    "builtins.frozenset",
    "builtins.int",
    "builtins.float",
    "builtins.complex",
    "builtins.bool",
    "builtins.range",
    "builtins.memoryview",
];

/// [`ModuleDecls::hook_factories`] of the module `input` declares as `decls`: its plain
/// module-level functions every `return` of which gives the same functions that may take a
/// name (a function nested in it or one of the module, also as items of a tuple display), with
/// what each serves where heki follows it ([`made_hook`]).
pub(super) fn hook_factories(
    input: &ModuleInput<'_>,
    decls: &ModuleDecls,
    kb: &EffectiveEcosystemKb,
) -> Option<BTreeMap<String, Box<[MadeHook]>>> {
    let ir = input.ir;
    if ir.constants.dynamic {
        return None;
    }
    let fixed = |f: &IrFunction| {
        own_function(ir, &f.qualname).is_some_and(|g| g.id == f.id)
            && !decls.rebound_definitions.contains(&f.qualname)
            && !decls.imports.contains_key(&f.qualname)
    };
    let factories: Vec<(&IrFunction, ReturnedHooks<'_>)> = (ir.functions)
        .iter()
        .filter(|f| f.enclosing == Some(ir.module_fn) && fixed(f))
        .filter_map(|f| Some((f, returned_hooks(ir, f)?)))
        .collect();
    if factories.is_empty() {
        return None;
    }
    let completed = own_completed(input, decls, kb);
    let cx = TypeCtx::new(input, &completed, kb);
    let decider = Decider {
        ir,
        cx: &cx,
        project: &completed,
        module: &decls.module_name,
    };
    let cells = ParamCells::of(ir);
    let mut out = BTreeMap::new();
    for (factory, hooks) in factories {
        let made: Vec<MadeHook> = (hooks.into_iter())
            .filter_map(|(item, hook, name)| {
                made_hook(&decider, &cells, decls, factory, item, hook, name)
            })
            .collect();
        if !made.is_empty() {
            out.insert(factory.qualname.clone(), made.into());
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The functions that may take a name the factory `f` returns, by the item of the tuple it
/// returns (`None`: the value itself), with the parameter the name arrives in: what every
/// `return` of it gives alike.
fn returned_hooks<'m>(ir: &'m ModuleIr, f: &IrFunction) -> Option<ReturnedHooks<'m>> {
    let shape = FnShape::new(f);
    let hook = |v: VarId| match &shape.inst(shape.copy_root(v))?.kind {
        InstKind::MakeClosure {
            func_id: Some(id), ..
        } => {
            let g = ir.function(*id);
            let plain = g.kind == FunctionKind::Sync && !g.is_class_body;
            (plain && g.enclosing == Some(f.id)).then_some(g)
        }
        InstKind::LoadGlobal { name, .. } => own_function(ir, name),
        _ => None,
    };
    let mut returned: Option<Vec<(Option<u8>, &IrFunction)>> = None;
    for b in shape.rpo() {
        let Terminator::Return(v) = f.block(*b).terminator else {
            continue;
        };
        let v = shape.copy_root(v?);
        let hooks: Vec<(Option<u8>, &IrFunction)> = match &shape.inst(v)?.kind {
            InstKind::AllocBuiltin {
                kind: BuiltinKind::Tuple,
                elems,
                ..
            } if shape.first_starred(v).is_none() => (elems.iter().enumerate())
                .filter_map(|(i, e)| Some((Some(u8::try_from(i).ok()?), hook(*e)?)))
                .collect(),
            _ => vec![(None, hook(v)?)],
        };
        let same = |a: &[(Option<u8>, &IrFunction)], b: &[(Option<u8>, &IrFunction)]| {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.0 == y.0 && x.1.id == y.1.id)
        };
        (returned.as_deref())
            .is_none_or(|r| same(r, &hooks))
            .then_some(())?;
        returned = Some(hooks);
    }
    let out: Vec<_> = (returned?.into_iter())
        .filter_map(|(item, g)| {
            let first = g.params.first()?;
            let takes_name = !first.is_vararg && !first.is_kwarg && !first.kw_only;
            let decides = takes_name && decides_names(&FnShape::new(g), first.var);
            decides.then_some((item, g, first.var))
        })
        .collect();
    (!out.is_empty()).then_some(out)
}

/// What the hook `hook` the factory `factory` returns serves ([`MadeHook`]), its name arriving
/// in `name`: the table it reads by the name is one closure cell bound to a parameter of the
/// factory, which neither the factory nor any function nested in it changes ([`only_read`]);
/// the hook stores nothing but behind tests of the factory's parameters ([`store_guards`]).
fn made_hook(
    decider: &Decider<'_>,
    cells: &ParamCells,
    decls: &ModuleDecls,
    factory: &IrFunction,
    item: Option<u8>,
    hook: &IrFunction,
    name: VarId,
) -> Option<MadeHook> {
    let ir = decider.ir;
    let shape = FnShape::new(hook);
    let is_name = |v: VarId| shape.copy_root(v) == name;
    let param_of = |cell: &str| {
        (cells.param(ir, hook, cell))
            .filter(|(owner, _)| owner.id == factory.id)
            .map(|(_, p)| p)
    };
    let mut table: Option<(&str, u8)> = None;
    for inst in hook.blocks.iter().flat_map(|b| &b.insts) {
        let read = match &inst.kind {
            InstKind::SubscriptLoad {
                obj,
                index,
                is_slice: false,
                ..
            } if is_name(*index) => *obj,
            InstKind::MethodCall {
                recv, method, args, ..
            } if method == "get" && args.first().is_some_and(|a| is_name(*a)) => *recv,
            InstKind::Compare {
                op: CompareOpKind::In | CompareOpKind::NotIn,
                lhs,
                rhs,
                ..
            } if is_name(*lhs) => *rhs,
            _ => continue,
        };
        let Some(InstKind::LoadDeref { name: cell, .. }) =
            shape.inst(shape.copy_root(read)).map(|i| &i.kind)
        else {
            continue;
        };
        let param = param_of(cell)?;
        table.is_none_or(|(c, _)| c == cell).then_some(())?;
        table = Some((cell, param));
    }
    if let Some((_, param)) = table
        && !only_read(ir, cells, factory, param)
    {
        return None;
    }
    let false_params = store_guards(&shape, |cell| param_of(cell))?;
    let decided = decider.hook_serves(&shape, name, table.map(|(cell, _)| cell), &[factory])?;
    // Without what a key gets, which keys are among the literals is not known: nothing.
    let table = match table {
        Some((_, param)) => Some((hook_param(factory, param)?, decided.keyed?)),
        None => None,
    };
    (table.is_some() || !decided.served.is_empty()).then_some(())?;
    let mut globals = own_globals(&shape, decls);
    globals.push(format!("{}.{}", decls.module_name, factory.qualname));
    let served = (decided.served.into_iter())
        .map(|(key, (ty, read))| {
            globals.extend(read);
            (key, ty)
        })
        .collect();
    globals.sort();
    globals.dedup();
    Some(MadeHook {
        item,
        table,
        served,
        literals: decided.literals.into_iter().collect(),
        false_params: (false_params.into_iter())
            .map(|p| hook_param(factory, p))
            .collect::<Option<_>>()?,
        globals: globals.into(),
    })
}

/// The parameter of the factory `f` at `position`, as a call binds it.
fn hook_param(f: &IrFunction, position: u8) -> Option<HookParam> {
    let p = f.params.get(usize::from(position))?;
    (!p.is_vararg && !p.is_kwarg).then(|| HookParam {
        position,
        name: p.name.clone(),
        pos_only: p.pos_only,
        kw_only: p.kw_only,
        default: p.default.clone(),
    })
}

/// Whether the factory `factory` and every function nested in it only read the object its
/// parameter at `param` holds: test it, read an item, call `get`, `keys`, `values` or `items`
/// on it, unpack it into a display, or hand it to `len`, `sorted`, `list`, `tuple`, `set`,
/// `frozenset` or `iter`.
fn only_read(ir: &ModuleIr, cells: &ParamCells, factory: &IrFunction, param: u8) -> bool {
    let own = &factory.params[usize::from(param)];
    let nested = |g: &IrFunction| ir.enclosing_functions(g).any(|e| e.id == factory.id);
    (ir.functions.iter())
        .filter(|g| g.id == factory.id || nested(g))
        .all(|g| {
            let shape = FnShape::new(g);
            let holds = |v: VarId| {
                let root = shape.copy_root(v);
                (g.id == factory.id && root == own.var)
                    || matches!(shape.inst(root).map(|i| &i.kind),
                        Some(InstKind::LoadDeref { name, .. })
                            if (g.id == factory.id && *name == own.name)
                                || cells.param(ir, g, name)
                                    .is_some_and(|(o, p)| o.id == factory.id && p == param))
            };
            g.blocks.iter().all(|b| {
                let phis = (b.phis.iter()).all(|phi| phi.incoming.iter().all(|(_, x)| !holds(*x)));
                let returned = matches!(b.terminator, Terminator::Return(Some(x)) if holds(x));
                phis && !returned && b.insts.iter().all(|i| reads_only(&shape, &i.kind, &holds))
            })
        })
}

/// Whether the instruction `kind` only reads what the values `holds` names hold.
fn reads_only(shape: &FnShape<'_>, kind: &InstKind, holds: &impl Fn(VarId) -> bool) -> bool {
    let reads = kind.operands().into_iter().any(holds);
    !reads
        || match kind {
            InstKind::SubscriptLoad { index, .. } => !holds(*index),
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                ..
            } => {
                holds(*recv)
                    && matches!(method.as_str(), "get" | "keys" | "values" | "items")
                    && !args
                        .iter()
                        .chain(kwargs.iter().map(|(_, a)| a))
                        .any(|a| holds(*a))
            }
            InstKind::AllocBuiltin { dst, elems, .. } => {
                (elems.iter().enumerate()).all(|(i, e)| !holds(*e) || shape.is_starred(*dst, i))
            }
            InstKind::Call {
                callee,
                args,
                kwargs,
                ..
            } => {
                matches!(
                    callee.as_str(),
                    "len" | "sorted" | "list" | "tuple" | "set" | "frozenset" | "iter"
                ) && args.len() == 1
                    && kwargs.is_empty()
            }
            other => matches!(
                other,
                InstKind::Copy { .. } | InstKind::Compare { .. } | InstKind::MakeClosure { .. }
            ),
        }
}

/// The factory's parameters guarding a store of the hook `shape` ([`MadeHook::false_params`]):
/// a branch on the closure cell of one (`if cache:`) whose true way reaches a store that the
/// hook reaches no other way. `None` when the hook stores anything with every such parameter
/// false: a store of a name, an attribute, or an item, `setattr` or `delattr`, or a method
/// putting entries into a mapping.
fn store_guards(shape: &FnShape<'_>, param_of: impl Fn(&str) -> Option<u8>) -> Option<Vec<u8>> {
    let f = shape.f;
    let guard = |b: BlockId| match f.block(b).terminator {
        Terminator::Branch { cond, then_bb, .. } => {
            match &shape.inst(shape.copy_root(cond))?.kind {
                InstKind::LoadDeref { name, .. } => Some((param_of(name)?, then_bb)),
                _ => None,
            }
        }
        _ => None,
    };
    let stores = |b: BlockId| {
        f.block(b).insts.iter().any(|i| match &i.kind {
            InstKind::Call { callee, .. } => matches!(callee.as_str(), "setattr" | "delattr"),
            InstKind::MethodCall { method, .. } => matches!(
                method.as_str(),
                "update" | "setdefault" | "__setitem__" | "__setattr__" | "pop" | "clear"
            ),
            kind => kind.dst().is_none() && !matches!(kind, InstKind::ExitWith { .. }),
        })
    };
    // Whether the hook may store with the guards of every parameter but `open` false.
    let may_store = |open: Option<u8>| {
        let budget = Budget::of_function("heki_effect::resolver::getattr_exports::store_guards", f);
        let next = |b: BlockId, out: &mut Vec<BlockId>| {
            let cut = guard(b)
                .filter(|(p, _)| Some(*p) != open)
                .map(|(_, then)| then);
            out.extend(shape.succs(b).filter(|s| Some(*s) != cut));
        };
        let visit = |b: BlockId| {
            if stores(b) {
                Visit::Stop(())
            } else {
                Visit::Expand
            }
        };
        !matches!(
            reach(&budget, [BlockId(0)], next, visit),
            Answer::Known(None)
        )
    };
    if may_store(None) {
        return None;
    }
    let mut params: Vec<u8> = (shape.rpo().iter())
        .filter_map(|b| Some(guard(*b)?.0))
        .collect();
    params.sort_unstable();
    params.dedup();
    params.retain(|p| may_store(Some(*p)));
    Some(params)
}

/// The hook a module's call of a factory makes ([`ProjectDecls::served_by_getattr`]): the
/// factory's [`MadeHook`] with the call's arguments.
pub(super) struct Instance<'p> {
    module: &'p ModuleDecls,
    hook: &'p MadeHook,
    /// The module's table the call passes, by fqn, with its entries, and what a key gets.
    table: Option<(String, Entries<'p>, KeyServes)>,
}

impl<'p> Instance<'p> {
    /// The hook the call `made` of the module `m` makes from the factory's `hook`, when the
    /// call binds the table parameter to a table of `m` and each store guard to a false
    /// literal (or leaves it at such a default). Another module storing into the table is
    /// [`ProjectDecls::getattr_open`]'s to see.
    pub(super) fn of(m: &'p ModuleDecls, made: &'p MadeBy, hook: &'p MadeHook) -> Option<Self> {
        let false_literal = |literal: &ConstLiteral| {
            matches!(
                literal,
                ConstLiteral::None | ConstLiteral::Bool(false) | ConstLiteral::Int(0)
            )
        };
        let guarded = (hook.false_params.iter()).all(|p| match bound(made, p) {
            Bound::Arg(HookArg::Literal(literal)) | Bound::Default(Some(literal)) => {
                false_literal(literal)
            }
            _ => false,
        });
        guarded.then_some(())?;
        let table = match &hook.table {
            Some((param, serves)) => match bound(made, param) {
                Bound::Arg(HookArg::Global(name)) => {
                    let entries = m.tables.get(name)?;
                    Some((format!("{}.{name}", m.module_name), &entries[..], *serves))
                }
                _ => return None,
            },
            None => None,
        };
        Some(Self {
            module: m,
            hook,
            table,
        })
    }

    /// What the hook serves under `name`, with the module globals deciding it: under a key of
    /// the table, what its value is or calling it gives; under another literal name the hook
    /// decides, what its code gives. A key among the literals gets nothing.
    pub(super) fn serves(&self, name: &str) -> Option<(Ty, Vec<String>)> {
        let entry = (self.table.as_ref()).and_then(|(_, entries, serves)| {
            let (_, ty) = entries.iter().find(|(key, _)| &**key == name)?;
            Some((ty.as_ref(), *serves))
        });
        let given = match entry {
            Some(_) if self.hook.literals.iter().any(|l| l == name) => return None,
            Some((Some(Ty::Ref(f)), KeyServes::Called)) => call_gives(self.own_function(f)?)?,
            Some((Some(ty), KeyServes::Value)) => (ty.clone(), Vec::new()),
            Some(_) => return None,
            None => (self.hook.served.get(name)?.clone(), Vec::new()),
        };
        matches!(given.0, Ty::Ref(_) | Ty::Class(_)).then_some(given)
    }

    /// The module's function of the fqn `f`.
    fn own_function(&self, f: &str) -> Option<&'p FunctionDecl> {
        let qualname = f
            .strip_prefix(self.module.module_name.as_str())?
            .strip_prefix('.')?;
        self.module.functions.get(qualname).map(|decl| &**decl)
    }

    /// The globals deciding what the hook serves ([`ProjectDecls::getattr_open`]): the
    /// factory's, the table, and each value of the table with the globals what calling it
    /// gives was inferred from.
    pub(super) fn globals(&self) -> Vec<String> {
        let mut out = self.hook.globals.to_vec();
        if let Some((fqn, entries, _)) = &self.table {
            out.push(fqn.clone());
            for (key, ty) in entries.iter() {
                if let Some(Ty::Ref(f)) = ty {
                    out.push(f.clone());
                }
                out.extend(self.serves(key).into_iter().flat_map(|(_, read)| read));
            }
        }
        out
    }
}

/// What a call binds a parameter of a factory to.
enum Bound<'a> {
    Arg(&'a HookArg),
    /// Nothing passed: the default, when it is a literal.
    Default(Option<&'a ConstLiteral>),
}

fn bound<'a>(made: &'a MadeBy, param: &'a HookParam) -> Bound<'a> {
    let at = usize::from(param.position);
    if !param.kw_only && at < made.args.len() {
        return Bound::Arg(&made.args[at]);
    }
    match (made.kwargs.iter()).find(|(key, _)| !param.pos_only && *key == param.name) {
        Some((_, arg)) => Bound::Arg(arg),
        None => Bound::Default(param.default.as_ref()),
    }
}
