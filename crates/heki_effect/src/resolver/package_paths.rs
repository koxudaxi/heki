//! Where Python finds the modules under a package: on its `__path__`, which starts as the
//! package's own directory, unless code changes it ([`ModuleDecls::path_changes`]). An import of
//! a name under a package whose path may lead outside the project may load a module installed
//! where the program runs ([`super::ProjectDecls::holds_modules_starting_with`]).

use super::{ModuleDecls, PathChange, imported_fqn};
use crate::ModuleInput;
use heki_ir::walk::{Budget, Closure, Def, Join, Use};
use heki_ir::{BinOpKind, FnIndex, InstKind, Terminator, VarId, comprehension_id};
use rustc_hash::FxHashMap;
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
        let held = paths.held();
        for (package, change) in paths.changes(&held) {
            let joined = out.entry(package.to_owned()).or_insert(change);
            *joined = (*joined).max(change);
        }
    }
    out
}

/// The packages a joined fact names: none, one (by its index among the packages whose paths a
/// function holds), or several.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Packages {
    #[default]
    None,
    One(u32),
    Several,
}

impl Join for Packages {
    fn join(&mut self, other: &Self) -> bool {
        let joined = match (*self, *other) {
            (_, Self::None) => *self,
            (Self::None, other) => other,
            (Self::One(a), Self::One(b)) if a == b => *self,
            _ => Self::Several,
        };
        let grew = joined != *self;
        *self = joined;
        grew
    }
}

/// What the uses of values holding a path do to it: the change they make whatever the package,
/// and the packages whose `__path__` they bind the value to, which leaves that package's path as
/// it is and makes any other package share its list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Uses {
    change: Option<PathChange>,
    bound: Packages,
}

impl Uses {
    fn changing(change: Option<PathChange>) -> Self {
        Self {
            change,
            bound: Packages::None,
        }
    }

    /// What the uses do to the path of the package `package` (an index, as in [`Packages`]).
    fn change_for(self, package: u32) -> Option<PathChange> {
        match self.bound {
            Packages::None => self.change,
            Packages::One(bound) if bound == package => self.change,
            _ => Some(PathChange::Replaced),
        }
    }
}

impl Join for Uses {
    fn join(&mut self, other: &Self) -> bool {
        let changed = other.change > self.change;
        self.change = self.change.max(other.change);
        self.bound.join(&other.bound) || changed
    }
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

    /// How the function changes the path of each package `held` names: the uses of the values
    /// holding it ([`Self::uses`]), joined over the values each one reaches through copies, phis,
    /// and `+=`, and what the function binds to it ([`Self::stored`]). The packages share one
    /// budget and one answer per value: values holding the paths of many packages are each
    /// decided once.
    fn changes<'h>(&self, held: &'h BTreeMap<String, Held>) -> Vec<(&'h str, PathChange)> {
        if held.is_empty() {
            return Vec::new();
        }
        let names: Vec<&str> = held.keys().map(String::as_str).collect();
        let package = |name: Option<&str>| match name.map(|n| names.binary_search(&n)) {
            Some(Ok(at)) => Packages::One(at as u32),
            _ => Packages::Several,
        };
        let mut roots: FxHashMap<VarId, Packages> = FxHashMap::default();
        for (at, h) in held.values().enumerate() {
            for v in h.loads.iter().chain(&h.stores) {
                roots.entry(*v).or_default().join(&Packages::One(at as u32));
            }
        }
        let budget =
            Budget::of_function("heki_effect::resolver::package_paths::changes", self.ix.f);
        let uses: Closure<VarId, Uses> = Closure::new();
        let holders: Closure<VarId, Packages> = Closure::new();
        let mut out = Vec::new();
        for (at, (name, h)) in held.iter().enumerate() {
            let at = at as u32;
            // A value holds the path of the package when every value it is made from does: a
            // load of it, or a value bound to it.
            let holds_path = |v: VarId| {
                holders
                    .ask(
                        &budget,
                        v,
                        |x| match roots.get(&x) {
                            Some(root) => *root,
                            None if self.sources(x).is_some() => Packages::None,
                            None => Packages::Several,
                        },
                        |x, next| {
                            if !roots.contains_key(&x) {
                                next.extend(self.sources(x).into_iter().flatten());
                            }
                        },
                    )
                    .map(|holder| *holder == Packages::One(at))
                    .open_means(false)
            };
            let by_uses = h.loads.iter().chain(&h.stores).map(|start| {
                uses.ask(
                    &budget,
                    *start,
                    |v| self.uses(v, &package),
                    |v, next| next.extend(self.aliases(v)),
                )
                .map(|joined| joined.change_for(at))
                .open_means(Some(PathChange::Replaced))
            });
            let by_stores = h.stores.iter().map(|w| Some(self.stored(*w, holds_path)));
            if let Some(change) = by_uses.chain(by_stores).fold(h.change, Option::max) {
                out.push((name.as_str(), change));
            }
        }
        out
    }

    /// The values `v` is made from when it holds the same object as they do: the source of a
    /// copy, the inputs of a phi, and the left operand of `+=`. `None` for any other value.
    fn sources(&self, v: VarId) -> Option<Vec<VarId>> {
        let f = self.ix.f;
        match self.ix.shape.def(v) {
            Def::Phi(b, i) => Some(
                f.block(b).phis[i as usize]
                    .incoming
                    .iter()
                    .map(|(_, x)| *x)
                    .collect(),
            ),
            Def::Inst(b, i) => match &f.block(b).insts[i as usize].kind {
                InstKind::Copy { src, .. } => Some(vec![*src]),
                InstKind::BinOp {
                    op: BinOpKind::Add,
                    lhs,
                    in_place: true,
                    ..
                } => Some(vec![*lhs]),
                _ => None,
            },
            Def::None | Def::Param => None,
        }
    }

    /// The values the uses of `v` give that hold the same object: copies, phis, and `+=` (which
    /// gives a list its own object back).
    fn aliases(&self, v: VarId) -> impl Iterator<Item = VarId> + '_ {
        let f = self.ix.f;
        self.ix
            .shape
            .readers(v)
            .iter()
            .filter_map(move |at| match *at {
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
            })
    }

    /// What the uses of `v`, a value holding a path, do to it ([`Self::use_of`]), joined.
    fn uses(&self, v: VarId, package: &impl Fn(Option<&str>) -> Packages) -> Uses {
        let mut joined = Uses::default();
        for at in self.ix.shape.readers(v) {
            joined.join(&self.use_of(*at, v, package));
        }
        joined
    }

    /// What the use `at` of `v`, a value holding a path, does to it. A copy, a phi, and `+=` give
    /// values holding it too, whose own uses decide. Reading it changes nothing; adding
    /// directories after its own appends; binding it to a package's `__path__` leaves that path
    /// as it is (what binding it does is [`Self::stored`]'s) and shares its list with any other;
    /// anything else may replace it, handing it to code heki does not follow included.
    /// `package` gives the index of a package by its name.
    fn use_of(&self, at: Use, v: VarId, package: &impl Fn(Option<&str>) -> Packages) -> Uses {
        let f = self.ix.f;
        let kind = match at {
            Use::Inst(b, i) => &f.block(b).insts[i as usize].kind,
            Use::Phi(..) => return Uses::default(),
            Use::Terminator(b) => {
                return Uses::changing(match f.block(b).terminator {
                    Terminator::Branch { .. } => None,
                    _ => Some(PathChange::Replaced),
                });
            }
        };
        let bound = |name: Option<&str>| Uses {
            change: None,
            bound: package(name),
        };
        Uses::changing(match kind {
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
            // A package binding `__path__` rebinds its own path, also to an imported list; in a
            // module that is not a package, an import binding the name only names the list it
            // imports, whose changes are followed through the import.
            InstKind::StoreGlobal { name, .. }
                if name == "__path__" && (self.own.is_some() || !self.binds_import(kind, v)) =>
            {
                return bound(self.own);
            }
            InstKind::AttrStore { obj, attr, value } if attr == "__path__" && *value == v => {
                return bound(self.imported(*obj).as_deref());
            }
            kind if self.binds_import(kind, v) || self.only_reads(kind, v) => None,
            _ => Some(PathChange::Replaced),
        })
    }

    /// Whether `kind` binds `v` to the name an import binds it to (`p` of `from plugins import
    /// __path__ as p`).
    fn binds_import(&self, kind: &InstKind, v: VarId) -> bool {
        match kind {
            InstKind::StoreGlobal { name, .. } => {
                matches!(
                    self.ix.shape.inst(v).map(|i| &i.kind),
                    Some(InstKind::Import { .. })
                ) && self.ix.f.var_name(v) == Some(name.as_str())
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
    /// (`__path__ + [d]`, `__path__ += [d]`, `pkgutil.extend_path(__path__, __name__)`, where
    /// `holds_path` tells the value holding the path) appends, and anything else replaces it.
    fn stored(&self, w: VarId, holds_path: impl Fn(VarId) -> bool) -> PathChange {
        let w = self.ix.shape.copy_root(w);
        let appended = match self.ix.shape.inst(w).map(|i| &i.kind) {
            Some(InstKind::BinOp {
                op: BinOpKind::Add,
                lhs,
                ..
            }) => holds_path(*lhs),
            Some(kind @ (InstKind::Call { args, .. } | InstKind::MethodCall { args, .. })) => {
                args.first().is_some_and(|a| holds_path(*a))
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
