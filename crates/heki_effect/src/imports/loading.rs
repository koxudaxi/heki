//! What the code that runs while a module loads may read of what its imports bind
//! ([`super::ImportSite::read_while_loading`]).

use super::{ImportForm, ImportSite, Named, ReadTime, Statement, Timing, class_bodies};
use crate::resolver::stores::exported_names;
use crate::{CallSite, CallTarget, ModuleInput};
use heki_ast::Span;
use heki_ir::walk::{Answer, Budget, Visit, reach};
use heki_ir::{FunctionId, InstKind, IrFunction, ModuleIr, Terminator, VarId};
use heki_semantic::{ModuleImport, NameResolution, Scope, ScopeKind};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeMap, BTreeSet};

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
    /// The functions of the module that may run while it loads ([`called_while_loading`]).
    called: FxHashSet<&'s str>,
    /// The IR functions that may run while the module loads: the module body, the functions
    /// of `called`, and the class bodies these run.
    loading: FxHashSet<FunctionId>,
}

impl<'s> LoadReads<'s> {
    /// Of `input`, whose call sites are `calls`.
    pub(super) fn of(input: &'s ModuleInput<'_>, calls: &'s [CallSite]) -> Self {
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
        let mut loading = FxHashSet::default();
        for f in (ir.functions.iter()).filter(|f| !f.is_class_body) {
            let called_here = called.contains(f.qualname.as_str());
            if !called_here && !f.is_module_body {
                continue;
            }
            let bodies = std::iter::once(f).chain(class_bodies(ir, f).into_iter().map(|c| c.body));
            for body in bodies {
                loading.insert(body.id);
                let insts = (body.blocks.iter()).flat_map(|b| &b.insts);
                for inst in insts.filter(|_| called_here) {
                    if let InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } =
                        &inst.kind
                    {
                        names.insert(name.as_str());
                    }
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
            called,
            loading,
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

impl LoadReads<'_> {
    /// When `statement`, at `site`, reads the module it names: while the module loads for a
    /// statement the module body or a class body it runs holds, one in a function that runs
    /// meanwhile, and a lazy one whose name such code reads; later for the others; never
    /// under `TYPE_CHECKING`.
    fn time_of(&self, statement: Statement<'_>, site: &ImportSite) -> Option<ReadTime> {
        let read_while_loading = matches!(statement, Statement::Import(imp) if self.reads(imp));
        match site.timing {
            Answer::Known(Timing::TypeChecking) => None,
            Answer::Known(Timing::Function) if !self.called.contains(site.qualname.as_str()) => {
                Some(ReadTime::Later)
            }
            Answer::Known(Timing::Lazy) if !read_while_loading => Some(ReadTime::Later),
            _ => Some(ReadTime::Loading),
        }
    }

    /// What the module reads of the namespaces of other modules
    /// ([`super::ModuleImports::names_read`]; `*` alone past the budget of
    /// [`attributes_read`]: any public name of any module), `statements` its import statements
    /// with their sites: the name a `from` import takes, every public name a star import takes
    /// (`m.*`), and the attributes code reads off what an import binds (`import m` then
    /// `m.Name`, each step of `a.b.Name`), also through a module global the module body binds to
    /// it, or, where that value goes anywhere else (an argument, a return, an element), every
    /// public name of it.
    pub(super) fn names_read<'a>(
        &self,
        ir: &'a ModuleIr,
        statements: &[(Statement<'a>, &ImportSite)],
    ) -> BTreeMap<String, ReadTime> {
        let mut read = NamesRead::default();
        // What each import binds, by the span of its site, and by the module global it binds.
        let mut bases: FxHashMap<Span, String> = FxHashMap::default();
        let mut globals: FxHashMap<&'a str, BTreeSet<String>> = FxHashMap::default();
        for &(statement, site) in statements {
            let Answer::Known(named) = &site.modules else {
                continue;
            };
            let [Named::Module(module)] = named.as_slice() else {
                continue;
            };
            let Some(time) = self.time_of(statement, site) else {
                continue;
            };
            let imp = match statement {
                Statement::Star(_) => {
                    read.add(format!("{module}.*"), time);
                    continue;
                }
                Statement::Import(imp) => imp,
            };
            let base = match &site.form {
                ImportForm::From(name) => {
                    let base = format!("{module}.{name}");
                    read.add(base.clone(), time);
                    base
                }
                _ if imp.has_alias => module.clone(),
                _ => module.split('.').next().unwrap_or(module).to_owned(),
            };
            if Some(imp.scope) == self.module_scope {
                (globals.entry(imp.bound_name.as_str()).or_default()).insert(base.clone());
            }
            bases.insert(site.span, base);
        }
        if bases.is_empty() {
            return read.0;
        }
        // The module body first: the globals it binds to what an import binds are known when
        // the functions read them.
        let (body, functions): (Vec<&IrFunction>, Vec<&IrFunction>) =
            ir.functions.iter().partition(|f| f.is_module_body);
        for f in body.into_iter().chain(functions) {
            let time = if self.loading.contains(&f.id) {
                ReadTime::Loading
            } else {
                ReadTime::Later
            };
            if !attributes_read(f, time, &bases, &mut globals, &mut read) {
                // Past the budget, the module may read any public name of any module.
                read.add("*".to_owned(), ReadTime::Loading);
                break;
            }
        }
        read.0
    }
}

/// The attributes `f`, running at `time`, reads off what the imports bind
/// ([`LoadReads::names_read`]): `bases` by the span of the import, `globals` by the module
/// global bound to it, which the module body adds to. `false` past the budget of `f`, which the
/// dotted names it makes spend (a chain of attributes makes names as long as the chain).
fn attributes_read<'a>(
    f: &'a IrFunction,
    time: ReadTime,
    bases: &FxHashMap<Span, String>,
    globals: &mut FxHashMap<&'a str, BTreeSet<String>>,
    read: &mut NamesRead,
) -> bool {
    let budget = Budget::of_functions("import_names_read", [f]);
    // What each variable of `f` may hold of what the imports bind, by dotted name.
    let mut held: FxHashMap<VarId, Vec<String>> = FxHashMap::default();
    let hold = |held: &mut FxHashMap<VarId, Vec<String>>, v: VarId, names: Vec<String>| {
        let size: usize = names.iter().map(String::len).sum();
        held.insert(v, names);
        budget.take(size)
    };
    let escape = |held: &FxHashMap<VarId, Vec<String>>, read: &mut NamesRead, v: VarId| {
        for base in held.get(&v).into_iter().flatten() {
            read.add(format!("{base}.*"), time);
        }
    };
    for block in &f.blocks {
        let joins_kept = block.phis.iter().all(|phi| {
            let mut joined: Vec<String> = (phi.incoming.iter())
                .flat_map(|(_, v)| held.get(v).into_iter().flatten())
                .cloned()
                .collect();
            joined.sort_unstable();
            joined.dedup();
            joined.is_empty() || hold(&mut held, phi.dst, joined)
        });
        let kept = joins_kept
            && block.insts.iter().all(|inst| match &inst.kind {
                InstKind::Import { dst, .. } => match bases.get(&inst.span) {
                    Some(base) => hold(&mut held, *dst, vec![base.clone()]),
                    None => true,
                },
                InstKind::LoadGlobal { dst, name, .. } | InstKind::LoadName { dst, name } => {
                    match globals.get(name.as_str()) {
                        Some(bound) => hold(&mut held, *dst, bound.iter().cloned().collect()),
                        None => true,
                    }
                }
                InstKind::Copy { dst, src } => match held.get(src).cloned() {
                    Some(bound) => hold(&mut held, *dst, bound),
                    None => true,
                },
                InstKind::AttrLoad { dst, obj, attr } => match held.get(obj) {
                    Some(bound) => {
                        let attrs: Vec<String> =
                            bound.iter().map(|b| format!("{b}.{attr}")).collect();
                        for a in &attrs {
                            read.add(a.clone(), time);
                        }
                        hold(&mut held, *dst, attrs)
                    }
                    None => true,
                },
                InstKind::MethodCall {
                    recv,
                    method,
                    args,
                    kwargs,
                    ..
                } => {
                    for base in held.get(recv).into_iter().flatten() {
                        read.add(format!("{base}.{method}"), time);
                    }
                    for &v in args.iter().chain(kwargs.iter().map(|(_, v)| v)) {
                        escape(&held, read, v);
                    }
                    true
                }
                InstKind::AttrStore { value, .. } => {
                    escape(&held, read, *value);
                    true
                }
                InstKind::AttrDelete { .. } => true,
                InstKind::StoreGlobal { name, value } if f.is_module_body => {
                    if let Some(bound) = held.get(value) {
                        let names = globals.entry(name.as_str()).or_default();
                        names.extend(bound.iter().cloned());
                    }
                    true
                }
                kind => {
                    for v in kind.operands() {
                        escape(&held, read, v);
                    }
                    true
                }
            });
        if !kept {
            return false;
        }
        if let Terminator::Return(Some(v)) = &block.terminator {
            escape(&held, read, *v);
        }
    }
    true
}

/// [`LoadReads::names_read`] as it is gathered: each name with the earliest time it is read.
#[derive(Default)]
struct NamesRead(BTreeMap<String, ReadTime>);

impl NamesRead {
    fn add(&mut self, name: String, time: ReadTime) {
        let earliest = self.0.entry(name).or_insert(time);
        *earliest = (*earliest).min(time);
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
