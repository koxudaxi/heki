//! `EFF117` (`unlocked-write-of-guarded-state`, heuristic): shared state that a writer changes
//! holding a thread lock, changed here holding none, in code a thread heki resolves can run.
//!
//! The state and the locks are those of [`heki_effect::guarded`]. A write counts as made holding
//! a lock where the function holds one ([`LockPoints::held_at`], with the locks its callees
//! return holding), and where every call of the function is made holding one, up to
//! [`CALLER_DEPTH`] calls above it ([`GuardedView::entry_held`]): a generator expression made, or
//! a lambda called, inside `with lock:` runs holding the lock. Writes made only while an
//! object is built (constructors and the functions only they call) or in a child process
//! after a fork, and writes in test code, neither count as holding a lock nor are reported.
//!
//! A thread entry is a project function handed to a catalog API running it on another thread
//! (`threading.Thread(target=f)`, `asyncio.to_thread(f)`, a thread pool's `submit`), or the
//! `run` method of a `threading.Thread` subclass; the functions it reaches over project calls
//! can run on that thread. The report needs the unlocked writer or a locked writer to be one.

use crate::{CheckContext, ModuleUnit, SitesBySpan};
use heki_diagnostics::Diagnostic;
use heki_effect::guarded::{self, LockKey, LockPoints, StateKey, StateWrite, WriteKind};
use heki_effect::{
    FuncKey, ProjectDecls, ProjectIndex, TypeCtx, anchors_call_site, code_kind, ownership,
};
use heki_ir::walk::{Budget, FnShape};
use heki_ir::{Inst, InstKind, IrFunction, VarId};
use heki_kb::RunsOn;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeMap, BTreeSet};

/// How many calls above a function [`GuardedView::entry_held`] and
/// [`GuardedView::setup_only`] look.
pub const CALLER_DEPTH: usize = 2;

/// The names of the methods that build an object before anything else holds it.
const CONSTRUCTORS: &[&str] = &["__init__", "__new__", "__post_init__", "__init_subclass__"];

/// A write of a state made holding a named lock, outside construction and tests.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Guard {
    pub writer: FuncKey,
    pub lock: LockKey,
    pub kind: WriteKind,
    pub span: heki_ast::Span,
}

/// What `EFF117` reads of the whole project ([`Self::of`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuardedView {
    /// Each state a writer changes holding a named lock, with those writes, sorted.
    pub guards: BTreeMap<StateKey, Vec<Guard>>,
    /// The functions a thread heki resolves can run.
    pub threaded: BTreeSet<FuncKey>,
    /// The functions every call of which is made holding a thread lock, up to [`CALLER_DEPTH`]
    /// calls above; no thread entry is one.
    pub entry_held: BTreeSet<FuncKey>,
    /// The constructors and after-fork handlers, and the functions only they call, up to
    /// [`CALLER_DEPTH`] calls above.
    pub setup_only: BTreeSet<FuncKey>,
    /// The guarded attributes of instances by name, each with the classes writing it.
    by_attr: BTreeMap<String, Vec<String>>,
}

/// Whether the function `key` is test code, by its module's path.
fn in_tests(key: &FuncKey) -> bool {
    code_kind::is_test_code(
        &key.qualname,
        &format!("{}.py", key.module.replace('.', "/")),
    )
}

impl GuardedView {
    pub fn of(index: &ProjectIndex, decls: &ProjectDecls) -> Self {
        let mut callers: FxHashMap<&FuncKey, Vec<(&FuncKey, bool)>> = FxHashMap::default();
        let mut calls: FxHashMap<&FuncKey, Vec<&FuncKey>> = FxHashMap::default();
        let mut entries: FxHashSet<&FuncKey> = FxHashSet::default();
        let mut forks: FxHashSet<&FuncKey> = FxHashSet::default();
        for file in index.files.values() {
            for site in &file.call_sites {
                for bc in site.project_callees() {
                    (callers.entry(&bc.callee).or_default())
                        .push((&site.caller, site.under_thread_lock));
                    calls.entry(&site.caller).or_default().push(&bc.callee);
                }
            }
            for handoff in file.unawaited_handoffs().flat_map(|(_, h)| h) {
                match handoff.runs_on {
                    Some(RunsOn::Thread) => entries.extend(&handoff.callees),
                    Some(RunsOn::ForkChild) => forks.extend(&handoff.callees),
                    None => {}
                }
            }
            for key in file.functions.keys() {
                if let Some(class) = key.qualname.strip_suffix(".run") {
                    let fqn = format!("{}.{class}", key.module);
                    if decls.mro_of(&fqn).iter().any(|c| c == "threading.Thread") {
                        entries.insert(key);
                    }
                }
            }
        }
        // Index walks expand each function once per run: counted, never limited.
        let steps = Budget::unmetered("heki_rules::eff_rules::unlocked_writes::GuardedView");
        let mut threaded: BTreeSet<FuncKey> = BTreeSet::new();
        let mut queue: Vec<&FuncKey> = entries.iter().copied().collect();
        while let Some(f) = queue.pop() {
            steps.take(1);
            if threaded.insert(f.clone()) {
                queue.extend(calls.get(f).into_iter().flatten().copied());
            }
        }
        let is_setup = |f: &FuncKey| {
            forks.contains(f)
                || CONSTRUCTORS.contains(&f.qualname.rsplit('.').next().unwrap_or_default())
        };
        let (mut setup, mut held) = (Memo::default(), Memo::default());
        let (mut setup_only, mut entry_held) = (BTreeSet::new(), BTreeSet::new());
        for file in index.files.values() {
            for f in file.functions.keys() {
                steps.take(1);
                if setup.all_callers(f, 0, &callers, &Walk::Setup(&is_setup)) {
                    setup_only.insert(f.clone());
                }
                if held.all_callers(f, 0, &callers, &Walk::Held(&entries)) {
                    entry_held.insert(f.clone());
                }
            }
        }
        let mut guards: BTreeMap<StateKey, Vec<Guard>> = BTreeMap::new();
        for file in index.files.values() {
            for (key, s) in &file.functions {
                let Some(facts) = &s.guarded else { continue };
                if setup_only.contains(key) || in_tests(key) {
                    continue;
                }
                for w in &facts.locked_writes {
                    guards.entry(w.state.clone()).or_default().push(Guard {
                        writer: key.clone(),
                        lock: w.lock.clone(),
                        kind: w.kind,
                        span: w.span,
                    });
                }
            }
        }
        for g in guards.values_mut() {
            g.sort();
        }
        let mut by_attr: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for state in guards.keys() {
            if let StateKey::Attr { class, attr } = state {
                by_attr.entry(attr.clone()).or_default().push(class.clone());
            }
        }
        Self {
            guards,
            threaded,
            entry_held,
            setup_only,
            by_attr,
        }
    }

    /// The functions whose membership in a set of `self` and `other` differs.
    pub fn moved_functions<'v>(&'v self, other: &'v Self) -> impl Iterator<Item = &'v FuncKey> {
        let diff = |a: &'v BTreeSet<FuncKey>, b: &'v BTreeSet<FuncKey>| a.symmetric_difference(b);
        diff(&self.threaded, &other.threaded)
            .chain(diff(&self.entry_held, &other.entry_held))
            .chain(diff(&self.setup_only, &other.setup_only))
    }

    /// Whether the states guarded and their writers differ between `self` and `other`.
    pub fn guards_differ(&self, other: &Self) -> bool {
        let unplaced = |v: &Self| -> BTreeMap<StateKey, BTreeSet<(FuncKey, LockKey, WriteKind)>> {
            (v.guards.iter())
                .map(|(s, g)| {
                    let writes = g.iter().map(|g| (g.writer.clone(), g.lock.clone(), g.kind));
                    (s.clone(), writes.collect())
                })
                .collect()
        };
        unplaced(self) != unplaced(other)
    }

    /// The last names of the states guarded: an attribute's, a global's.
    fn state_names(&self) -> FxHashSet<&str> {
        (self.guards.keys())
            .map(|s| match s {
                StateKey::Attr { attr, .. } => attr.as_str(),
                StateKey::Global(fqn) => fqn.rsplit('.').next().unwrap_or(fqn),
            })
            .collect()
    }

    /// The guarding writes of `state`: of the same name, and for an attribute of an instance,
    /// written by a method of the same class, a base, or a subclass.
    fn guards_of<'v>(&'v self, decls: &ProjectDecls, state: &StateKey) -> Vec<&'v Guard> {
        match state {
            StateKey::Global(_) => self.guards.get(state).into_iter().flatten().collect(),
            StateKey::Attr { class, attr } => {
                let mro = decls.mro_of(class);
                let related = |c: &String| mro.contains(c) || decls.mro_of(c).contains(class);
                (self.by_attr.get(attr).into_iter().flatten())
                    .filter(|c| related(c))
                    .flat_map(|c| {
                        let state = StateKey::Attr {
                            class: c.clone(),
                            attr: attr.clone(),
                        };
                        self.guards.get(&state).into_iter().flatten()
                    })
                    .collect()
            }
        }
    }
}

/// What a walk up the callers of a function asks ([`Memo::all_callers`]).
enum Walk<'w, 'k> {
    /// Whether it runs only while objects are built: a function `setup` accepts, or one whose
    /// every caller does so.
    Setup(&'w dyn Fn(&FuncKey) -> bool),
    /// Whether every call of it is made holding a thread lock, or in a function that runs so;
    /// a thread entry (among these) runs holding nothing of its callers'.
    Held(&'w FxHashSet<&'k FuncKey>),
}

/// The answers of one [`Walk`], by function and depth.
#[derive(Default)]
struct Memo<'k>(FxHashMap<(&'k FuncKey, usize), bool>);

impl<'k> Memo<'k> {
    /// Whether `f`, `depth` calls above the function asked about, answers `walk`, looking at
    /// most [`CALLER_DEPTH`] calls above that function.
    fn all_callers(
        &mut self,
        f: &'k FuncKey,
        depth: usize,
        callers: &FxHashMap<&'k FuncKey, Vec<(&'k FuncKey, bool)>>,
        walk: &Walk<'_, 'k>,
    ) -> bool {
        match walk {
            Walk::Setup(setup) if setup(f) => return true,
            Walk::Held(entries) if entries.contains(f) => return false,
            _ => {}
        }
        if depth >= CALLER_DEPTH {
            return false;
        }
        if let Some(&done) = self.0.get(&(f, depth)) {
            return done;
        }
        // A call cycle back to `f` answers no until `f` is answered.
        self.0.insert((f, depth), false);
        let sites = callers.get(f).map_or(&[][..], Vec::as_slice);
        let answer = !sites.is_empty()
            && sites.iter().all(|&(caller, held)| {
                (held && matches!(walk, Walk::Held(_)))
                    || self.all_callers(caller, depth + 1, callers, walk)
            });
        self.0.insert((f, depth), answer);
        answer
    }
}

pub(super) fn run(
    cx: &CheckContext<'_>,
    view: &GuardedView,
    unit: &ModuleUnit<'_>,
    out: &mut Vec<Diagnostic>,
) {
    if !cx.enabled("EFF117", unit) || view.guards.is_empty() {
        return;
    }
    let names = view.state_names();
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    let mut sites = None;
    for f in unit.input.ir.functions.iter() {
        if f.is_class_body || f.is_module_body || !names_any(f, &names) {
            continue;
        }
        let key = CheckContext::key(unit, f);
        if view.setup_only.contains(&key)
            || view.entry_held.contains(&key)
            || code_kind::is_test_code(&key.qualname, &unit.rel_path)
        {
            continue;
        }
        let defs: FxHashMap<VarId, &Inst> = (f.blocks.iter().flat_map(|b| &b.insts))
            .filter_map(|i| Some((i.kind.dst()?, i)))
            .collect();
        let def = |v: VarId| defs.get(&v).copied();
        let types = tcx.infer(f);
        let live = ownership::Live::of(f, &defs);
        let writes: Vec<(StateWrite, Vec<&Guard>)> =
            guarded::state_writes(&tcx, f, &types, &def, &live)
                .into_iter()
                .map(|w| {
                    let guards = view.guards_of(cx.decls, &w.state);
                    (w, guards)
                })
                .filter(|(_, g)| !g.is_empty())
                .collect();
        if writes.is_empty() {
            continue;
        }
        let threaded = view.threaded.contains(&key);
        // The first guarding write of each state on a thread, or any when this one runs on one.
        let writes: Vec<(StateWrite, &Guard)> = (writes.into_iter())
            .filter_map(|(w, guards)| {
                let on_thread = |g: &&&Guard| threaded || view.threaded.contains(&g.writer);
                let guard = *guards.iter().find(on_thread)?;
                Some((w, guard))
            })
            .collect();
        if writes.is_empty() {
            continue;
        }
        let sites: &SitesBySpan<'_> = sites.get_or_insert_with(|| cx.sites_by_span(unit));
        let shape = FnShape::new(f);
        let taken = |_, _, inst: &Inst| taken_by_call(cx, sites, &key, f, inst);
        let points = LockPoints::of(&tcx, &shape, &types, &def, &live, &taken);
        let mut reported: FxHashSet<&StateKey> = FxHashSet::default();
        for (w, guard) in &writes {
            if points.held_at(w.at.0, w.at.1).is_empty() && reported.insert(&w.state) {
                report(cx, unit, &key, w, guard, threaded, out);
            }
        }
    }
}

/// Whether `f` reads, stores, or deletes an attribute or a global under one of `names`: only
/// then can it write a guarded state.
fn names_any(f: &IrFunction, names: &FxHashSet<&str>) -> bool {
    f.blocks
        .iter()
        .flat_map(|b| &b.insts)
        .any(|i| match &i.kind {
            InstKind::AttrStore { attr, .. }
            | InstKind::AttrDelete { attr, .. }
            | InstKind::AttrLoad { attr, .. } => names.contains(attr.as_str()),
            InstKind::StoreGlobal { name, .. }
            | InstKind::LoadGlobal { name, .. }
            | InstKind::Delete {
                name, global: true, ..
            } => names.contains(name.as_str()),
            _ => false,
        })
}

/// The locks the calls at `inst` return holding, in the names of `f`: a lock of `self` when the
/// call binds the callee's first parameter to `f`'s own, a global as it is.
fn taken_by_call(
    cx: &CheckContext<'_>,
    sites: &SitesBySpan<'_>,
    key: &FuncKey,
    f: &IrFunction,
    inst: &Inst,
) -> Vec<LockKey> {
    if !anchors_call_site(&inst.kind) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for site in sites.at(&key.qualname, inst.span) {
        for bc in site.project_callees() {
            let Some(facts) = cx
                .index
                .summary(&bc.callee)
                .and_then(|s| s.guarded.as_ref())
            else {
                continue;
            };
            for lock in &facts.returns_holding {
                out.push(match lock {
                    LockKey::SelfAttr(_)
                        if f.owner_class_qualname.is_some()
                            && bc.params.first().and_then(|p| p.param.as_ref())
                                == Some(&(0, None)) =>
                    {
                        lock.clone()
                    }
                    LockKey::Global(_) => lock.clone(),
                    _ => LockKey::Unnamed,
                });
            }
        }
    }
    out
}

fn report(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    key: &FuncKey,
    w: &StateWrite,
    guard: &Guard,
    threaded: bool,
    out: &mut Vec<Diagnostic>,
) {
    let state = w.state.display();
    let lock = guard.lock.name().unwrap_or_default();
    let on_thread = if threaded {
        format!("`{}` can run on another thread", key.qualname)
    } else {
        format!("`{}` can run on another thread", guard.writer.qualname)
    };
    let message = format!(
        "`{}` {} `{state}` where heki finds no lock held, while `{}` {} it holding `{lock}`; \
         {on_thread}, so the two writes can interleave and one can lose the other's update",
        key.qualname,
        w.kind.verb(),
        guard.writer.qualname,
        guard.kind.verb(),
    );
    let help = format!("hold `{lock}` for this write too");
    let d = cx
        .emit(
            out,
            "EFF117",
            unit,
            w.span,
            &key.qualname,
            format!("{state}->{lock}"),
            message,
            Some(help),
            &[],
        )
        .expect("`run` checked that EFF117 runs on the unit");
    if let Some(path) = cx.index.file_of(&guard.writer) {
        d.related = vec![cx.related_at(
            path,
            guard.span,
            format!(
                "`{}` {} `{state}` here holding `{lock}`",
                guard.writer.qualname,
                guard.kind.verb()
            ),
        )];
    }
}
