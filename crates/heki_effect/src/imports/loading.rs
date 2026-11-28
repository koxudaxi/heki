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
use std::cell::RefCell;
use std::collections::BTreeMap;

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
    /// ([`super::ModuleImports::names_read`]; `*` alone past the budget of [`HeldNames`]: any
    /// public name of any module), `statements` its import statements with their sites: the
    /// name a `from` import takes, every public name a star import takes (`m.*`), and the
    /// attributes code reads off what an import binds (`import m` then `m.Name`, each step of
    /// `a.b.Name`), also through a module global the module body binds to it, or, where that
    /// value goes anywhere else (an argument, a return, an element), every public name of it.
    pub(super) fn names_read<'a>(
        &self,
        ir: &'a ModuleIr,
        statements: &[(Statement<'a>, &ImportSite)],
    ) -> BTreeMap<String, ReadTime> {
        let mut read = NamesRead::default();
        let mut held = HeldNames::of(ir);
        // What each import binds, by the span of its site, and by the module global it binds.
        let mut bases: FxHashMap<Span, Node> = FxHashMap::default();
        let mut bound: FxHashMap<&'a str, Vec<Node>> = FxHashMap::default();
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
            let base = held.name(base);
            if Some(imp.scope) == self.module_scope {
                bound.entry(imp.bound_name.as_str()).or_default().push(base);
            }
            bases.insert(site.span, base);
        }
        if bases.is_empty() {
            return read.0;
        }
        let mut globals: FxHashMap<&'a str, Node> = (bound.into_iter())
            .filter_map(|(name, nodes)| Some((name, held.any_of(nodes)?)))
            .collect();
        // The module body first: the globals it binds to what an import binds are known when
        // the functions read them.
        let (body, functions): (Vec<&IrFunction>, Vec<&IrFunction>) =
            ir.functions.iter().partition(|f| f.is_module_body);
        let walked = body.into_iter().chain(functions).all(|f| {
            let time = if self.loading.contains(&f.id) {
                ReadTime::Loading
            } else {
                ReadTime::Later
            };
            attributes_read(f, time, &bases, &mut globals, &mut held)
        });
        if !walked || !held.spell(&mut read) {
            // Past the budget, the module may read any public name of any module.
            read.add("*".to_owned(), ReadTime::Loading);
        }
        read.0
    }
}

/// What `f`, running at `time`, holds and reads of what the imports bind, kept in `held`:
/// `bases` by the span of the import, `globals` by the module global bound to it, which the
/// module body adds to. `false` past the budget of `held`.
fn attributes_read<'a>(
    f: &'a IrFunction,
    time: ReadTime,
    bases: &FxHashMap<Span, Node>,
    globals: &mut FxHashMap<&'a str, Node>,
    held: &mut HeldNames,
) -> bool {
    // What each value of `f` holds.
    let mut values: FxHashMap<VarId, Node> = FxHashMap::default();
    let escape = |held: &mut HeldNames, values: &FxHashMap<VarId, Node>, v: VarId| {
        if let Some(&of) = values.get(&v) {
            let every = held.attr(of, "*");
            held.read(every, time);
        }
    };
    for block in &f.blocks {
        for phi in &block.phis {
            let joined = (phi.incoming.iter())
                .filter_map(|(_, v)| values.get(v).copied())
                .collect();
            if let Some(node) = held.any_of(joined) {
                values.insert(phi.dst, node);
            }
        }
        for inst in &block.insts {
            if held.budget.spent() {
                return false;
            }
            match &inst.kind {
                InstKind::Import { dst, .. } => {
                    if let Some(&node) = bases.get(&inst.span) {
                        values.insert(*dst, node);
                    }
                }
                InstKind::LoadGlobal { dst, name, .. } | InstKind::LoadName { dst, name } => {
                    if let Some(&node) = globals.get(name.as_str()) {
                        values.insert(*dst, node);
                    }
                }
                InstKind::Copy { dst, src } => {
                    if let Some(&node) = values.get(src) {
                        values.insert(*dst, node);
                    }
                }
                InstKind::AttrLoad { dst, obj, attr } => {
                    if let Some(&of) = values.get(obj) {
                        let node = held.attr(of, attr);
                        held.read(node, time);
                        values.insert(*dst, node);
                    }
                }
                InstKind::MethodCall {
                    recv,
                    method,
                    args,
                    kwargs,
                    ..
                } => {
                    if let Some(&of) = values.get(recv) {
                        let node = held.attr(of, method);
                        held.read(node, time);
                    }
                    for &v in args.iter().chain(kwargs.iter().map(|(_, v)| v)) {
                        escape(held, &values, v);
                    }
                }
                InstKind::AttrStore { value, .. } => escape(held, &values, *value),
                InstKind::AttrDelete { .. } => {}
                InstKind::StoreGlobal { name, value } if f.is_module_body => {
                    if let Some(&node) = values.get(value) {
                        let was = globals.get(name.as_str()).copied();
                        if let Some(joined) = held.any_of(was.into_iter().chain([node]).collect()) {
                            globals.insert(name.as_str(), joined);
                        }
                    }
                }
                kind => {
                    for v in kind.operands() {
                        escape(held, &values, v);
                    }
                }
            }
        }
        if let Terminator::Return(Some(v)) = &block.terminator {
            escape(held, &values, *v);
        }
    }
    !held.budget.spent()
}

/// A node of [`HeldNames`].
type Node = u32;

/// What a value may hold of what the imports bind ([`HeldNames`]).
enum Held {
    /// One dotted name: a module an import binds, or an attribute path off one.
    Name(String),
    /// What any of these nodes holds.
    AnyOf(Vec<Node>),
    /// The names an `AnyOf` node holds, each followed by this attribute path.
    Path(Node, String),
}

/// What the values of a module may hold of what its imports bind, and which of them the module
/// reads. A value joining others (a phi, a module global bound more than once) holds one node
/// naming theirs, and an attribute off it one node naming the path: a chain of joins keeps one
/// node per join, not every name each join may hold. The names are spelled out once per node
/// and attribute path, for the nodes read ([`Self::spell`]). One budget for the module, which
/// the names made and the nodes kept spend: a module global joins what its functions hold.
struct HeldNames {
    budget: Budget,
    nodes: Vec<Held>,
    names: FxHashMap<String, Node>,
    paths: FxHashMap<(Node, String), Node>,
    /// The nodes whose names the module reads, each with when.
    reads: Vec<(Node, ReadTime)>,
}

impl HeldNames {
    fn of(ir: &ModuleIr) -> Self {
        Self {
            budget: Budget::of_functions("import_names_read", &ir.functions),
            nodes: Vec::new(),
            names: FxHashMap::default(),
            paths: FxHashMap::default(),
            reads: Vec::new(),
        }
    }

    fn push(&mut self, held: Held) -> Node {
        self.nodes.push(held);
        (self.nodes.len() - 1) as Node
    }

    /// The node of the dotted name `name`.
    fn name(&mut self, name: String) -> Node {
        self.budget.take(name.len());
        if let Some(&node) = self.names.get(&name) {
            return node;
        }
        let node = self.push(Held::Name(name.clone()));
        self.names.insert(name, node);
        node
    }

    /// The node of the attribute `attr` off what `of` holds.
    fn attr(&mut self, of: Node, attr: &str) -> Node {
        let key = match &self.nodes[of as usize] {
            Held::Name(name) => return self.name(format!("{name}.{attr}")),
            Held::Path(any_of, path) => (*any_of, format!("{path}.{attr}")),
            Held::AnyOf(_) => (of, attr.to_owned()),
        };
        self.budget.take(key.1.len());
        if let Some(&node) = self.paths.get(&key) {
            return node;
        }
        let node = self.push(Held::Path(key.0, key.1.clone()));
        self.paths.insert(key, node);
        node
    }

    /// The node holding what any of `nodes` holds; `None` for none.
    fn any_of(&mut self, mut nodes: Vec<Node>) -> Option<Node> {
        self.budget.take(nodes.len());
        nodes.sort_unstable();
        nodes.dedup();
        match nodes.as_slice() {
            [] => None,
            &[node] => Some(node),
            _ => Some(self.push(Held::AnyOf(nodes))),
        }
    }

    fn read(&mut self, node: Node, time: ReadTime) {
        self.reads.push((node, time));
    }

    /// The names the nodes read hold, each with the earliest time it is read, into `out`:
    /// `false` past the budget. One walk from every read, so a node reached with the same
    /// attribute path and time is visited once however many reads reach it.
    fn spell(&self, out: &mut NamesRead) -> bool {
        // The attribute paths the walk carries, interned: `None` for no path.
        let paths: RefCell<(Vec<String>, FxHashMap<String, u32>)> = RefCell::default();
        let after = |name: &str, path: Option<u32>| match path {
            Some(path) => format!("{name}.{}", paths.borrow().0[path as usize]),
            None => name.to_owned(),
        };
        let walked = reach(
            &self.budget,
            (self.reads.iter()).map(|&(node, time)| (node, None, time)),
            |(node, path, time), next| match &self.nodes[node as usize] {
                Held::Name(_) => {}
                Held::AnyOf(nodes) => next.extend(nodes.iter().map(|&n| (n, path, time))),
                Held::Path(any_of, first) => {
                    let joined = after(first, path);
                    self.budget.take(joined.len());
                    let mut paths = paths.borrow_mut();
                    let (spelled, ids) = &mut *paths;
                    let id = *ids.entry(joined).or_insert_with_key(|joined| {
                        spelled.push(joined.clone());
                        (spelled.len() - 1) as u32
                    });
                    next.push((*any_of, Some(id), time));
                }
            },
            |(node, path, time)| {
                if let Held::Name(name) = &self.nodes[node as usize] {
                    let name = after(name, path);
                    if !self.budget.take(name.len()) {
                        return Visit::Stop(());
                    }
                    out.add(name, time);
                }
                Visit::Expand
            },
        );
        matches!(walked, Answer::Known(None)) && !self.budget.spent()
    }
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
