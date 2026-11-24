//! Circular imports that fail (`IMP102`): the `from M import name` statements that raise
//! `ImportError` when a module of an import cycle is the first of its group imported in a
//! process, found by running the module bodies of the group in order through their import
//! sites ([`ImportGraph::circular_failures`]).
//!
//! A run claims a failure only from established facts: each hop of the chain runs whenever its
//! module loads and no handler may take what it raises, `name` is no submodule of `M`, `M` has
//! not bound it (nor `__getattr__`, nor any name) before the import it is paused at, and no other
//! module binds it. Whatever else may load a module of the group while the run goes on stops the
//! run: an import heki cannot place, one it cannot name, a module outside the group whose load
//! may load one of the group, and, when some function or lazy import of the project may load one,
//! any code a module body runs before the failing import.

use super::{ImportForm, ImportGraph, ImportHop, ModuleRef, Target, Timing};
use crate::resolver::ProjectDecls;
use heki_ir::walk::{Answer, BitSet, Budget, Closure};
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;
use std::rc::Rc;

/// A `from M import name` statement that raises `ImportError` when one of `entries` is the first
/// module of its import cycle group imported ([`ImportGraph::circular_failures`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CircularFailure {
    /// The module of the statement, and the index of its site in [`ImportGraph::sites`].
    pub module: u32,
    pub site: usize,
    /// The module `M` the statement imports the name from, still loading.
    pub from: u32,
    /// The modules whose import, first of the group, fails at the statement, ascending.
    pub entries: Vec<u32>,
    /// The hops of the first entry's load from the first module it runs (the entry, or a
    /// package of it that loads first) to `module`: each enters the module the next one leaves,
    /// and the one leaving `from` is where `from` is paused.
    pub chain: Vec<ImportHop>,
}

/// What a site may load anew ([`ImportGraph::may_load`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Load {
    Module(u32),
    /// Any module: heki cannot name what the site imports.
    Any,
}

/// Whether the site may run while its module loads: in the module body (or a class body it
/// runs), or a lazy import whose reads heki cannot place.
fn may_run_while_loading(timing: Answer<Timing>) -> bool {
    matches!(timing, Answer::Known(Timing::Body) | Answer::Open)
}

/// Whether the site may run after the module body has passed it: in a function, or a lazy
/// import, which loads at a read of its name, maybe in a function.
fn may_run_later(site: &super::ImportSite) -> bool {
    site.lazy
        || matches!(
            site.timing,
            Answer::Known(Timing::Function | Timing::Lazy) | Answer::Open
        )
}

impl ImportGraph<'_> {
    /// The `from M import name` statements of the project that raise `ImportError` when a
    /// module of their import cycle group (the modules importing each other through the sites
    /// that may run while a module loads) is the first of the group imported, each with the
    /// modules whose import does that. Each module of each group is imported first in turn: its
    /// parent packages load first, then the module bodies run in order through their sites, as
    /// the module docs describe. All runs share `budget`; past it, no further run reports.
    pub fn circular_failures(&self, decls: &ProjectDecls, budget: &Budget) -> Vec<CircularFailure> {
        let groups = self.cycle_groups(|site| may_run_while_loading(site.timing));
        if groups.is_empty() {
            return Vec::new();
        }
        let mut group_of: Vec<Option<u32>> = vec![None; self.modules.len()];
        for (g, group) in groups.iter().enumerate() {
            for &m in &group.members {
                group_of[m as usize] = Some(g as u32);
            }
        }
        let reach = Reach {
            graph: self,
            group_of: &group_of,
            any: groups.len(),
            code: groups.len() + 1,
            closure: Closure::new(),
            budget,
        };
        let Answer::Known(late) = reach.late() else {
            return Vec::new();
        };
        let mut found: BTreeMap<(u32, usize), CircularFailure> = BTreeMap::new();
        for (g, group) in groups.iter().enumerate() {
            let failing = self.failing_imports(decls, &group.members, &group_of);
            if failing.is_empty() {
                continue;
            }
            let run = GroupRun {
                graph: self,
                reach: &reach,
                budget,
                members: &group.members,
                late_loads: late.contains(g) || late.contains(reach.any),
                failing,
            };
            for &entry in &group.members {
                let failure = run.entry(entry);
                if budget.spent() {
                    return found.into_values().collect();
                }
                if let Some((module, site, from, chain)) = failure {
                    (found.entry((module, site)))
                        .or_insert_with(|| CircularFailure {
                            module,
                            site,
                            from,
                            entries: Vec::new(),
                            chain,
                        })
                        .entries
                        .push(entry);
                }
            }
        }
        found.into_values().collect()
    }

    /// The sites of the modules of a group (`members`, `group_of`) that raise `ImportError` if
    /// they run while the module they import a name from is loading and has not bound it, with
    /// that module and the name: `from M import name` of a module `M` of the group other than
    /// their own, which imports whenever its module loads and leaves the error to it
    /// ([`super::ImportSite::always_imports_uncaught`]); `name` is no module attribute the
    /// import system binds (a dunder), the project has no module `M.name` (Python falls back to
    /// a submodule of that name once it is loaded), and no other module binds it
    /// ([`ProjectDecls::global_bound_elsewhere`]).
    fn failing_imports<'s>(
        &'s self,
        decls: &ProjectDecls,
        members: &[u32],
        group_of: &[Option<u32>],
    ) -> FxHashMap<(u32, usize), (u32, &'s str)> {
        let mut out = FxHashMap::default();
        for &m in members {
            for (j, (site, target)) in self.sites(m).into_iter().flatten().enumerate() {
                let (ImportForm::From(name), Target::Module(ModuleRef::Project(from))) =
                    (&site.form, target)
                else {
                    continue;
                };
                if *from == m
                    || group_of[*from as usize] != group_of[m as usize]
                    || !site.always_imports_uncaught()
                    || (name.starts_with("__") && name.ends_with("__"))
                {
                    continue;
                }
                let module = self.name(ModuleRef::Project(*from));
                if decls.module(&format!("{module}.{name}")).is_none()
                    && !decls.global_bound_elsewhere(module, name)
                {
                    out.insert((m, j), (*from, name.as_str()));
                }
            }
        }
        out
    }

    /// Into `out`, what site `j` of the project module `m` may load anew ([`Load`]): the project
    /// modules its target may be, each after its parent packages, but `m` and the packages
    /// holding it, which loaded before `m` ran; any module when heki cannot name the target.
    fn may_load(&self, m: u32, j: usize, out: &mut Vec<Load>) {
        let target = &self.sites(m).expect("a module with sites")[j].1;
        self.target_loads(m, target, out);
    }

    fn target_loads(&self, m: u32, target: &Target, out: &mut Vec<Load>) {
        match target {
            Target::Module(t) => self.named_loads(m, self.name(*t), out),
            Target::PackageOrSubmodule { package, submodule } => {
                self.named_loads(m, self.name(*package), out);
                self.named_loads(m, submodule, out);
            }
            Target::AnyOf(targets) => {
                for t in targets {
                    self.target_loads(m, t, out);
                }
            }
            Target::Under(head) => {
                self.named_loads(m, head.trim_end_matches('.'), out);
                let start = self.modules.partition_point(|n| n.as_str() < head.as_str());
                let under = (self.modules[start..].iter())
                    .take_while(|n| n.starts_with(head.as_str()))
                    .count();
                out.extend(
                    (start..start + under)
                        .map(|i| i as u32)
                        .filter(|&i| !self.holds(ModuleRef::Project(i), m))
                        .map(Load::Module),
                );
            }
            Target::Missing(_) => {}
            Target::Open => out.push(Load::Any),
        }
    }

    /// Into `out`, the project's module `name` and its parent packages, but `m` and the
    /// packages holding it.
    fn named_loads(&self, m: u32, name: &str, out: &mut Vec<Load>) {
        let prefixes = name.match_indices('.').map(|(i, _)| &name[..i]);
        out.extend(
            (prefixes.chain([name]))
                .filter_map(|n| self.module(n))
                .filter(|&w| !self.holds(ModuleRef::Project(w), m))
                .map(Load::Module),
        );
    }
}

/// What loading each project module may load while it loads ([`Reach::of`]), through every
/// site that may run then, of any certainty, joined over the modules it reaches: bit `g` for a
/// module of group `g`, `any` when heki cannot name a module one may load, and `code` when one
/// of them, itself included, runs code in its body ([`super::ModuleImports::code_before`]).
struct Reach<'r, 'i> {
    graph: &'r ImportGraph<'i>,
    group_of: &'r [Option<u32>],
    any: usize,
    code: usize,
    closure: Closure<u32, BitSet>,
    budget: &'r Budget,
}

impl Reach<'_, '_> {
    fn of(&self, m: u32) -> Answer<Rc<BitSet>> {
        self.closure.ask(
            self.budget,
            m,
            |m| self.own(m),
            |m, out| {
                out.extend(self.loads_while_loading(m).into_iter().filter_map(|l| match l {
                    Load::Module(w) => Some(w),
                    Load::Any => None,
                }))
            },
        )
    }

    /// What the sites of `m` that may run while it loads may load ([`ImportGraph::may_load`]).
    fn loads_while_loading(&self, m: u32) -> Vec<Load> {
        let mut out = Vec::new();
        for (j, (site, _)) in self.graph.sites(m).into_iter().flatten().enumerate() {
            if may_run_while_loading(site.timing) {
                self.graph.may_load(m, j, &mut out);
            }
        }
        out
    }

    fn own(&self, m: u32) -> BitSet {
        let mut bits = BitSet::new();
        if let Some(g) = self.group_of[m as usize] {
            bits.insert(g as usize);
        }
        let Some(facts) = self.graph.facts(m) else {
            // A module heki has no import facts of may do anything.
            bits.insert(self.any);
            bits.insert(self.code);
            return bits;
        };
        if facts.code_before.is_some() {
            bits.insert(self.code);
        }
        if self.loads_while_loading(m).contains(&Load::Any) {
            bits.insert(self.any);
        }
        bits
    }

    /// What the sites that may run after their module body has passed them (in a function, or
    /// a lazy import) may load, joined: the groups of the modules they may load, and of what
    /// loading those may load, and `any`. Code that runs while a group's modules load may call
    /// such a function. Open past the budget.
    fn late(&self) -> Answer<BitSet> {
        let mut late = BitSet::new();
        let mut loads = Vec::new();
        for m in 0..self.graph.modules.len() as u32 {
            for (j, (site, _)) in self.graph.sites(m).into_iter().flatten().enumerate() {
                if !may_run_later(site) {
                    continue;
                }
                loads.clear();
                self.graph.may_load(m, j, &mut loads);
                for load in &loads {
                    let Load::Module(w) = *load else {
                        late.insert(self.any);
                        continue;
                    };
                    let Answer::Known(bits) = self.of(w) else {
                        return Answer::Open;
                    };
                    for bit in bits.iter().filter(|&b| b != self.code) {
                        late.insert(bit);
                    }
                }
            }
        }
        Answer::Known(late)
    }
}

/// The state of a module of the group while a run goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Unloaded,
    /// Running its body, at its site `at` (where it is paused while the site imports).
    Loading {
        at: usize,
    },
    Loaded,
}

/// A module body a run is in, and where.
struct Frame {
    m: u32,
    site: usize,
    /// The hops of the site at `site` and the next one to take, while it takes them.
    hops: Option<(Vec<ImportHop>, usize)>,
    /// The project's submodule a `from` import at `site` may load once its package has loaded
    /// ([`Target::PackageOrSubmodule`]).
    then: Option<u32>,
    /// The hop that started the module's load; `None` for one the entry's import starts.
    via: Option<ImportHop>,
}

/// What a run of a module body establishes ([`GroupRun::load`]).
enum Step {
    Loaded,
    /// A failing import: its module and site, the module it takes the name from, and the hops
    /// leading to it.
    Fails(u32, usize, u32, Vec<ImportHop>),
    /// Nothing further: what happens next is not established.
    Stops,
}

/// The runs of the entries of one group.
struct GroupRun<'r, 'i> {
    graph: &'r ImportGraph<'i>,
    reach: &'r Reach<'r, 'i>,
    budget: &'r Budget,
    members: &'r [u32],
    /// Some function or lazy import of the project may load a module of the group
    /// ([`Reach::late`]): code a module body runs may call it.
    late_loads: bool,
    /// [`ImportGraph::failing_imports`] of the group.
    failing: FxHashMap<(u32, usize), (u32, &'r str)>,
}

impl GroupRun<'_, '_> {
    /// The index of `m` among the members, `None` for a module outside the group.
    fn member(&self, m: u32) -> Option<usize> {
        self.members.binary_search(&m).ok()
    }

    /// Whether loading `w`, a module outside the group whose load may or may not run (it may
    /// have loaded before), may load a module of the group, or run code while some function may
    /// load one.
    fn may_disturb(&self, w: u32, group: u32) -> bool {
        match self.reach.of(w) {
            Answer::Known(bits) => {
                bits.contains(group as usize)
                    || bits.contains(self.reach.any)
                    || (self.late_loads && bits.contains(self.reach.code))
            }
            Answer::Open => true,
        }
    }

    /// Whether a load the run cannot place may change what the run establishes: a module of the
    /// group that has not loaded yet, a module outside it that may disturb the group
    /// ([`Self::may_disturb`]), or any module.
    fn disturbs(&self, load: Load, group: u32, state: &[State]) -> bool {
        match load {
            Load::Any => true,
            Load::Module(w) => match self.member(w) {
                Some(i) => state[i] == State::Unloaded,
                None => self.may_disturb(w, group),
            },
        }
    }

    /// The failing import, its module and site, the module the name comes from, and the hops
    /// leading to it, when `entry` is the first module of the group imported.
    fn entry(&self, entry: u32) -> Option<(u32, usize, u32, Vec<ImportHop>)> {
        let group = self.reach.group_of[entry as usize].expect("entries are members");
        let mut state = vec![State::Unloaded; self.members.len()];
        let parents = self.graph.parents(ModuleRef::Project(entry));
        let roots = (parents.into_iter().filter_map(ModuleRef::project)).chain([entry]);
        for root in roots {
            let Some(r) = self.member(root) else {
                if self.may_disturb(root, group) {
                    return None;
                }
                continue;
            };
            if state[r] != State::Unloaded {
                continue;
            }
            match self.load(root, group, &mut state) {
                Step::Loaded => {}
                Step::Fails(module, site, from, chain) => return Some((module, site, from, chain)),
                Step::Stops => return None,
            }
        }
        None
    }

    /// Run the body of `root`, a module of the group not loaded yet, with the loads its sites
    /// start, until it has loaded, a `from` import fails, or what happens next is not
    /// established.
    fn load(&self, root: u32, group: u32, state: &mut [State]) -> Step {
        let graph = self.graph;
        let index = |m: u32| self.member(m).expect("frames are members");
        let mut frames = vec![Frame {
            m: root,
            site: 0,
            hops: None,
            then: None,
            via: None,
        }];
        state[index(root)] = State::Loading { at: 0 };
        while let Some(frame) = frames.last_mut() {
            let m = frame.m;
            let (Some(facts), Some(sites)) = (graph.facts(m), graph.sites(m)) else {
                return Step::Stops;
            };
            let j = frame.site;
            if let Some((hops, next)) = &mut frame.hops {
                let Some(&hop) = hops.get(*next) else {
                    if let Some(w) = frame.then.take()
                        && self.disturbs(Load::Module(w), group, state)
                    {
                        return Step::Stops;
                    }
                    frame.hops = None;
                    frame.site += 1;
                    continue;
                };
                *next += 1;
                let Some(w) = self.member(hop.to) else {
                    if self.may_disturb(hop.to, group) {
                        return Step::Stops;
                    }
                    continue;
                };
                match state[w] {
                    State::Loaded => {}
                    State::Loading { at } => {
                        if !hop.parent
                            && let Some(&(from, name)) = self.failing.get(&(m, j))
                            && from == hop.to
                            && graph.facts(from).is_some_and(|f| !f.may_bind_before(name, at))
                        {
                            let chain = frames.iter().filter_map(|f| f.via).collect();
                            return Step::Fails(m, j, from, chain);
                        }
                    }
                    State::Unloaded => {
                        state[w] = State::Loading { at: 0 };
                        frames.push(Frame {
                            m: hop.to,
                            site: 0,
                            hops: None,
                            then: None,
                            via: Some(hop),
                        });
                    }
                }
                continue;
            }
            if j == sites.len() {
                // The rest of the body runs before the module has loaded.
                if self.late_loads && facts.code_before.is_some() {
                    return Step::Stops;
                }
                state[index(m)] = State::Loaded;
                frames.pop();
                continue;
            }
            if !self.budget.take(1) || (self.late_loads && facts.runs_code_before(j)) {
                return Step::Stops;
            }
            state[index(m)] = State::Loading { at: j };
            let (site, target) = &sites[j];
            if !may_run_while_loading(site.timing) {
                frame.site += 1;
                continue;
            }
            if site.always_imports_uncaught() && target.certain().is_some() {
                frame.hops = Some((graph.hops(m, j), 0));
                frame.then = match target {
                    Target::PackageOrSubmodule { submodule, .. } => (graph.module(submodule))
                        .filter(|&w| !graph.holds(ModuleRef::Project(w), m)),
                    _ => None,
                };
                continue;
            }
            // An import that may not run, or whose module heki cannot name: whatever it may
            // load.
            let mut loads = Vec::new();
            graph.may_load(m, j, &mut loads);
            if (loads.into_iter()).any(|load| self.disturbs(load, group, state)) {
                return Step::Stops;
            }
            if site.always_imports_uncaught() && matches!(target, Target::Missing(_)) {
                // The import raises whenever it runs: nothing after it runs.
                return Step::Stops;
            } else {
                frame.site += 1;
            }
        }
        Step::Loaded
    }
}
