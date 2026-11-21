//! Where Python finds the modules under a package: on its `__path__`, which starts as the
//! package's own directory, unless code changes it ([`ModuleDecls::path_changes`]). An import of
//! a name under a package whose path may lead outside the project may load a module installed
//! where the program runs ([`super::ProjectDecls::holds_modules_starting_with`]).

use super::{ModuleDecls, PathChange, imported_fqn};
use crate::ModuleInput;
use heki_ir::walk::{Answer, Budget, Use, Visit, reach};
use heki_ir::{BinOpKind, FnIndex, InstKind, Terminator, VarId, comprehension_id};
use rustc_hash::FxHashSet;
use std::collections::BTreeMap;

/// The functions of the standard library that read a package path they are given and leave it
/// as it is (`extend_path` returns a new list).
const PATH_READERS: [&str; 3] = [
    "pkgutil.extend_path",
    "pkgutil.iter_modules",
    "pkgutil.walk_packages",
];

/// The list methods that leave their list as it is.
const READING_METHODS: [&str; 3] = ["copy", "count", "index"];

/// [`ModuleDecls::path_changes`] of the module `input`, whose other declarations `decls` holds:
/// in each function, what the uses of the values holding a package's path do to it.
pub(super) fn path_changes(
    input: &ModuleInput<'_>,
    decls: &ModuleDecls,
) -> BTreeMap<String, PathChange> {
    let mut out: BTreeMap<String, PathChange> = BTreeMap::new();
    for f in &input.ir.functions {
        let paths = Paths {
            ix: FnIndex::new(f, &input.ir.constants),
            decls,
            own: input.is_package.then_some(input.module_name.as_str()),
        };
        for (package, held) in paths.held() {
            if let Some(change) = paths.change(&package, &held) {
                let joined = out.entry(package).or_insert(change);
                *joined = (*joined).max(change);
            }
        }
    }
    out
}

/// What one function does with the path of one package.
#[derive(Default)]
struct Held {
    /// The values loads of it give (`__path__`, `plugins.__path__`).
    loads: Vec<VarId>,
    /// The values the function binds to it (`__path__ = ...`, `plugins.__path__ = ...`).
    stores: Vec<VarId>,
    /// What the function does to it otherwise: deleting it, or declaring the package a namespace
    /// with `pkg_resources.declare_namespace`.
    change: Option<PathChange>,
}

/// The paths of packages in one function of a module.
struct Paths<'a> {
    ix: FnIndex<'a>,
    decls: &'a ModuleDecls,
    /// The module's name when it is a package: its own `__path__` is a global of its own.
    own: Option<&'a str>,
}

impl Paths<'_> {
    /// What the function does with the path of each package it names.
    fn held(&self) -> BTreeMap<String, Held> {
        let mut out: BTreeMap<String, Held> = BTreeMap::new();
        let own = self.own.map(str::to_owned);
        for inst in self.ix.f.blocks.iter().flat_map(|b| &b.insts) {
            let (package, held_as) = match &inst.kind {
                InstKind::StoreGlobal { name, value } if name == "__path__" => {
                    (own.clone(), Some(*value))
                }
                InstKind::AttrStore { obj, attr, value } if attr == "__path__" => {
                    (self.imported(*obj), Some(*value))
                }
                InstKind::Delete {
                    name, global: true, ..
                } if name == "__path__" => (own.clone(), None),
                InstKind::AttrDelete { obj, attr } if attr == "__path__" => {
                    (self.imported(*obj), None)
                }
                kind => {
                    if let Some(package) = self.declared_namespace(kind) {
                        let held = out.entry(package).or_default();
                        held.change = held.change.max(Some(PathChange::Appended));
                    } else if let Some((dst, package)) = kind
                        .dst()
                        .and_then(|dst| Some((dst, self.loaded_path(kind, dst)?)))
                    {
                        out.entry(package).or_default().loads.push(dst);
                    }
                    continue;
                }
            };
            let Some(package) = package else {
                continue;
            };
            let held = out.entry(package).or_default();
            match held_as {
                Some(value) => held.stores.push(value),
                None => held.change = Some(PathChange::Replaced),
            }
        }
        out
    }

    /// The package whose path the load `kind` gives `dst`: the module's own `__path__`, or the
    /// `__path__` of a module its imports name (`plugins.__path__`, `p` of `from plugins import
    /// __path__ as p`).
    fn loaded_path(&self, kind: &InstKind, dst: VarId) -> Option<String> {
        let imports_path = |name: &str| {
            self.decls
                .imports
                .get(name)
                .is_some_and(|target| target.ends_with(".__path__"))
        };
        let imported = match kind {
            InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } => {
                if !imports_path(name) {
                    return self.own.filter(|_| name == "__path__").map(str::to_owned);
                }
                true
            }
            InstKind::Import { .. } => self.ix.f.var_name(dst).is_some_and(imports_path),
            InstKind::AttrLoad { attr, .. } => attr == "__path__",
            _ => false,
        };
        if !imported {
            return None;
        }
        let fqn = self.imported(dst)?;
        fqn.strip_suffix(".__path__").map(str::to_owned)
    }

    /// The module `obj` names through the module's imports.
    fn imported(&self, obj: VarId) -> Option<String> {
        imported_fqn(&self.ix, &self.decls.imports, obj, 0)
    }

    /// The package a call of `pkg_resources.declare_namespace` declares: the module's own name
    /// (`__name__` in a package the module does not rebind it in) or a constant. It orders the
    /// package's portions as `sys.path` lists them, and heki takes the project's directories to
    /// come first there, as for every top-level name.
    fn declared_namespace(&self, kind: &InstKind) -> Option<String> {
        let (InstKind::Call { callee, args, .. }
        | InstKind::MethodCall {
            method: callee,
            args,
            ..
        }) = kind
        else {
            return None;
        };
        if !callee.ends_with("declare_namespace")
            || self.called(kind).as_deref() != Some("pkg_resources.declare_namespace")
        {
            return None;
        }
        let name = *args.first()?;
        match &self.ix.shape.inst(name)?.kind {
            InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. }
                if name == "__name__" && !self.decls.unknown_globals.contains(name) =>
            {
                self.own.map(str::to_owned)
            }
            _ => self.ix.shape.str_const(name).map(str::to_owned),
        }
    }

    /// How the function changes the path of `package`, which it holds as `held`: through the
    /// values holding it ([`reach`] over copies, phis, and `+=`), the uses of each
    /// ([`Self::use_change`]), and what the function binds to it ([`Self::stored`]).
    fn change(&self, package: &str, held: &Held) -> Option<PathChange> {
        let shape = &self.ix.shape;
        let budget = Budget::of_function("heki_effect::resolver::package_paths::change", self.ix.f);
        let mut values: FxHashSet<VarId> = FxHashSet::default();
        let mut change = held.change;
        let replaced = reach(
            &budget,
            held.loads.iter().chain(&held.stores).copied(),
            |v, next| next.extend(shape.readers(v).iter().filter_map(|at| self.alias(*at, v))),
            |v| {
                values.insert(v);
                let by_uses = shape
                    .readers(v)
                    .iter()
                    .map(|at| self.use_change(package, *at, v))
                    .max()
                    .flatten();
                change = change.max(by_uses);
                match by_uses {
                    Some(PathChange::Replaced) => Visit::Stop(()),
                    _ => Visit::Expand,
                }
            },
        );
        match replaced {
            Answer::Known(None) => held
                .stores
                .iter()
                .map(|w| Some(self.stored(*w, &values)))
                .fold(change, Option::max),
            Answer::Known(Some(())) | Answer::Open => Some(PathChange::Replaced),
        }
    }

    /// The value the use `at` of `v` gives that holds the same path: a copy, a phi, and `+=`
    /// (which gives a list its own object back).
    fn alias(&self, at: Use, v: VarId) -> Option<VarId> {
        let f = self.ix.f;
        match at {
            Use::Phi(b, i) => Some(f.block(b).phis[i as usize].dst),
            Use::Inst(b, i) => match &f.block(b).insts[i as usize].kind {
                InstKind::Copy { dst, .. } => Some(*dst),
                InstKind::BinOp {
                    dst,
                    op: BinOpKind::Add,
                    lhs,
                    in_place: true,
                    ..
                } if *lhs == v => Some(*dst),
                _ => None,
            },
            Use::Terminator(_) => None,
        }
    }

    /// What the use `at` of `v`, a value holding the path of `package`, does to the path. A copy,
    /// a phi, and `+=` give values holding it too, whose own uses decide. Reading it changes
    /// nothing; adding directories after its own appends; anything else may replace it,
    /// handing it to code heki does not follow included.
    fn use_change(&self, package: &str, at: Use, v: VarId) -> Option<PathChange> {
        let f = self.ix.f;
        let kind = match at {
            Use::Inst(b, i) => &f.block(b).insts[i as usize].kind,
            Use::Phi(..) => return None,
            Use::Terminator(b) => {
                return match f.block(b).terminator {
                    Terminator::Branch { .. } => None,
                    _ => Some(PathChange::Replaced),
                };
            }
        };
        match kind {
            InstKind::Copy { .. }
            | InstKind::MakeIter { .. }
            | InstKind::Compare { .. }
            | InstKind::UnaryOp { .. }
            | InstKind::Convert { .. }
            | InstKind::Format { .. }
            | InstKind::BuildString { .. }
            | InstKind::SubscriptLoad { .. }
            | InstKind::BinOp {
                in_place: false, ..
            } => None,
            InstKind::BinOp {
                op: BinOpKind::Add,
                lhs,
                ..
            } if *lhs == v => Some(PathChange::Appended),
            InstKind::MethodCall { recv, method, .. } if *recv == v => match method.as_str() {
                "append" | "extend" => Some(PathChange::Appended),
                m if READING_METHODS.contains(&m) => None,
                _ => Some(PathChange::Replaced),
            },
            InstKind::MakeGenExpr {
                outer_iter,
                captured,
                ..
            } => (*outer_iter != v || captured.contains(&v)).then_some(PathChange::Replaced),
            kind if self.binds_path(package, kind, v) || self.only_reads(kind, v) => None,
            _ => Some(PathChange::Replaced),
        }
    }

    /// Whether `kind` binds `v` to the path of `package` (what binding it does is
    /// [`Self::stored`]'s), or to the name an import binds `v` to (`p` of `from plugins import
    /// __path__ as p`).
    fn binds_path(&self, package: &str, kind: &InstKind, v: VarId) -> bool {
        match kind {
            InstKind::StoreGlobal { name, .. } if name == "__path__" => self.own == Some(package),
            InstKind::StoreGlobal { name, .. } => {
                matches!(
                    self.ix.shape.inst(v).map(|i| &i.kind),
                    Some(InstKind::Import { .. })
                ) && self.ix.f.var_name(v) == Some(name.as_str())
            }
            InstKind::AttrStore { obj, attr, value } => {
                attr == "__path__" && *value == v && self.imported(*obj).as_deref() == Some(package)
            }
            _ => false,
        }
    }

    /// Whether the call `kind` only reads `v`: a builtin that only reads its arguments, a
    /// comprehension function iterating it, or one of [`PATH_READERS`].
    fn only_reads(&self, kind: &InstKind, v: VarId) -> bool {
        match kind {
            InstKind::Call {
                callee_var, callee, ..
            } if self.ix.builtin(*callee_var, callee).is_some() => true,
            // A comprehension function iterates its first argument; the others are the names its
            // body reads.
            InstKind::Call {
                callee_var,
                callee,
                args,
                ..
            } if comprehension_id(*callee_var, callee).is_some() => args
                .split_first()
                .is_some_and(|(first, rest)| *first == v && !rest.contains(&v)),
            _ => self
                .called(kind)
                .is_some_and(|c| PATH_READERS.contains(&c.as_str())),
        }
    }

    /// How binding `w` to the path changes it: the path with directories after its own
    /// (`__path__ + [d]`, `__path__ += [d]`, `pkgutil.extend_path(__path__, __name__)`, `values`
    /// holding the path) appends, and anything else replaces it.
    fn stored(&self, w: VarId, values: &FxHashSet<VarId>) -> PathChange {
        let w = self.ix.shape.copy_root(w);
        let appended = match self.ix.shape.inst(w).map(|i| &i.kind) {
            Some(InstKind::BinOp {
                op: BinOpKind::Add,
                lhs,
                ..
            }) => values.contains(lhs),
            Some(kind @ (InstKind::Call { args, .. } | InstKind::MethodCall { args, .. })) => {
                args.first().is_some_and(|a| values.contains(a))
                    && self.called(kind).as_deref() == Some("pkgutil.extend_path")
            }
            _ => false,
        };
        if appended {
            PathChange::Appended
        } else {
            PathChange::Replaced
        }
    }

    /// The function the call `kind` calls, by the fqn the module's imports give it
    /// (`pkgutil.extend_path`, `extend_path` after `from pkgutil import extend_path`), also on
    /// the module a call of the builtin `__import__` with a constant name gives
    /// (`__import__("pkgutil").extend_path`: the top-level package of the name).
    fn called(&self, kind: &InstKind) -> Option<String> {
        let shape = &self.ix.shape;
        match kind {
            InstKind::Call {
                callee_var: Some(callee),
                ..
            } => self.imported(*callee),
            InstKind::MethodCall { recv, method, .. } => {
                let module = self
                    .imported(*recv)
                    .or_else(|| match &shape.inst(*recv)?.kind {
                        InstKind::Call {
                            callee_var: Some(callee),
                            args,
                            ..
                        } if args.len() == 1 && self.calls_builtin_import(*callee) => {
                            Some(shape.str_const(args[0])?.split('.').next()?.to_owned())
                        }
                        _ => None,
                    })?;
                Some(format!("{module}.{method}"))
            }
            _ => None,
        }
    }

    /// Whether `callee` is the builtin `__import__`, a name the module binds no global to.
    fn calls_builtin_import(&self, callee: VarId) -> bool {
        matches!(
            self.ix.shape.inst(callee).map(|i| &i.kind),
            Some(InstKind::LoadGlobal { name, .. }) if name == "__import__"
                && self.decls.resolve_dotted(name, false).is_none()
                && !self.decls.unknown_globals.contains(name)
        )
    }
}
