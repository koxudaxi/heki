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
//! Like the return facts, it is pass 1: inferred against the module's own declarations
//! completed against themselves, so it stays a function of the module's text (a factory another
//! module defines gives nothing there). What other modules change is the declarations' to see
//! ([`ProjectDecls::served_by_getattr`]).

use super::{GetattrExports, ModuleDecls, ProjectDecls, Symbol, Ty, TypeCtx, complete_decls};
use crate::ModuleInput;
use heki_ir::walk::{Answer, Budget, Facts, FnShape, Join, KeySet, Sparse, fixpoint};
use heki_ir::{BuiltinKind, CompareOpKind, ConstLiteral, InstKind, Terminator, UnaryOpKind, VarId};
use heki_kb::EffectiveEcosystemKb;
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
    let fixed = !input.ir.constants.dynamic && !hook_bound_again;
    let (hook, name) = heki_ir::module_getattr_hook(input.ir).filter(|_| fixed)?;
    let shape = FnShape::new(hook);
    decides_names(&shape, name).then_some(())?;
    let own = ProjectDecls::new([decls.clone()]);
    let completed = ProjectDecls::new([complete_decls(input, decls.clone(), &own, kb)]);
    let cx = TypeCtx::new(input, &completed, kb);
    let reads = Hook {
        shape: &shape,
        name,
        module: &decls.module_name,
        project: &completed,
    };
    let module_body = &input.ir.functions[input.ir.module_fn.0 as usize];
    let budget = Budget::of_functions(
        "heki_effect::resolver::getattr_exports",
        [hook, module_body],
    );
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
    // A name the module binds only under `if TYPE_CHECKING:` is no name it binds at run time.
    let unbound = |key: &&str| !decls.binds(key) || decls.type_checking_only.contains(*key);
    let mut served: BTreeMap<&str, Served> = (names.into_iter().filter(unbound))
        .map(|key| (key, Served::Nothing))
        .collect();
    let inferred = cx.infer_types(hook);
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
    let mut globals = reads.own_globals(decls);
    let mut out = BTreeMap::new();
    for (key, served) in served {
        if let Served::Gives(ty @ (Ty::Ref(_) | Ty::Class(_)), read) = served {
            globals.extend(read);
            out.insert(key.to_owned(), ty);
        }
    }
    globals.sort();
    globals.dedup();
    let served = (!out.is_empty()).then_some(out)?;
    Some(GetattrExports {
        served,
        globals: globals.into(),
    })
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
/// first), one object with the module globals deciding it, or anything.
#[derive(Debug, Clone, PartialEq)]
enum Served {
    Nothing,
    Gives(Ty, Vec<String>),
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

/// A value read from a table of the module by the hook's name: the table's entries, and how.
struct Keyed<'t> {
    entries: &'t [(Box<str>, Option<Ty>)],
    lookup: Lookup,
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

/// The hook being read: its shape, its name parameter, and the declarations of its module.
struct Hook<'h> {
    shape: &'h FnShape<'h>,
    name: VarId,
    module: &'h str,
    project: &'h ProjectDecls,
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

    /// The entries of the table of the module the global `v` loads.
    fn loaded_table(&self, v: VarId) -> Option<&'h [(Box<str>, Option<Ty>)]> {
        let global = self.global_loaded(v)?;
        self.project.table(&format!("{}.{global}", self.module))
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
        let entries = self.loaded_table(table)?;
        Some(Keyed { entries, lookup })
    }

    /// The value read from a table by the name that the returned value `v` is, or whose call
    /// it is (`true`).
    fn keyed_return(&self, v: VarId) -> Option<(Keyed<'h>, bool)> {
        match self.callee(v).and_then(|c| self.keyed_value(c)) {
            Some(keyed) => Some((keyed, true)),
            None => Some((self.keyed_value(v)?, false)),
        }
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
                self.loaded_table(v)?
                    .iter()
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
        let declared = match self.project.symbol(f) {
            Symbol::Function(_, decl) => (decl.declared_return_as_named())
                .map(|(t, read)| Served::Gives(decl.call_type(t), read.to_vec())),
            _ => None,
        };
        declared.unwrap_or(Served::Open)
    }

    /// The function of the module the call `v` calls by its name, by fqn.
    fn own_callee(&self, v: VarId) -> Option<String> {
        let fqn = format!("{}.{}", self.module, self.global_loaded(self.callee(v)?)?);
        matches!(self.project.symbol(&fqn), Symbol::Function(..)).then_some(fqn)
    }

    /// The module's own globals the hook reads (no imports, which name another module's
    /// object), and the hook's own name ([`GetattrExport::globals`]).
    fn own_globals(&self, decls: &ModuleDecls) -> Vec<String> {
        let mut out: Vec<String> = (self.shape.f.blocks.iter())
            .flat_map(|b| &b.insts)
            .filter_map(|i| match &i.kind {
                InstKind::LoadGlobal { name, .. } if !decls.imports.contains_key(name) => {
                    Some(format!("{}.{name}", self.module))
                }
                _ => None,
            })
            .chain([format!("{}.__getattr__", self.module)])
            .collect();
        out.sort();
        out.dedup();
        out
    }
}
