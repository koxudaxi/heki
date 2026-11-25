//! Circular imports that fail (`IMP102`): the `from M import name` statements that raise
//! `ImportError` when a module of an import cycle is the first of its group imported in a
//! process, found by running the module bodies of the group in order through their import
//! sites ([`ImportGraph::circular_failures`]).
//!
//! A run claims a failure only from established facts: each hop of the chain runs whenever its
//! module loads and no handler takes the plain `ImportError` it passes on, `name` is no
//! submodule of `M`, `M` has not bound it (nor `__getattr__`, nor any name) before the import it
//! is paused at, and no other module binds it. Whatever else may load a module of the group while
//! the run goes on stops the run: an import heki cannot place, one it cannot name, a module
//! outside the group whose load may load one of the group, and, when some function or lazy
//! import of the project may load one, any code a module body runs before the failing import.

use super::{ImportFailure, ImportForm, ImportGraph, ImportHop, ModuleRef, Named, Target, Timing};
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
    /// The module `M` the statement imports the name from, still loading, and the name.
    pub from: u32,
    pub name: String,
    /// The modules whose import fails at the statement when no module of the group has loaded,
    /// ascending: modules of the group, and packages holding one whose import loads the group.
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
    /// modules whose import does that. Each module of each group, and each package holding one
    /// whose import loads the group ([`GroupRun::entries`]), is imported first in turn: its
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
            uncounted: groups.len() + 2,
            closure: Closure::new(),
            certain: Closure::new(),
            budget,
        };
        let late = reach.late();
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
                group: g as u32,
                members: &group.members,
                late_loads: late.contains(g) || late.contains(reach.any),
                failing,
            };
            for entry in run.entries() {
                let failure = run.entry(entry);
                if budget.spent() {
                    return found.into_values().collect();
                }
                if let Some((module, site, from, chain)) = failure {
                    let name = run.failing[&(module, site)].1;
                    (found.entry((module, site)))
                        .or_insert_with(|| CircularFailure {
                            module,
                            site,
                            from,
                            name: name.to_owned(),
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
    /// their own, which takes the name from `M` itself (not a submodule `M.name`, which Python
    /// finds in `sys.modules` while it loads) and leaves the plain `ImportError` to its module
    /// ([`super::ImportSite::always_imports_leaving`]); `name` is no module attribute the
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
                    || !site.always_imports_leaving(ImportFailure::AnyImportError)
                    || (name.starts_with("__") && name.ends_with("__"))
                {
                    continue;
                }
                let module = self.name(ModuleRef::Project(*from));
                let named = matches!(&site.modules, Answer::Known(named)
                    if matches!(named.as_slice(), [Named::Module(n)] if n == module));
                if named
                    && decls.module(&format!("{module}.{name}")).is_none()
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
/// module of group `g`, `any` when heki cannot name a module one may load, `code` when one of
/// them, itself included, runs code in its body ([`super::ModuleImports::code_before`]), and
/// `uncounted` when one may load a module through no hop ([`ImportGraph::hops`]: a site heki
/// cannot name one module of, or the submodule a `from` import may be). And what it certainly
/// loads ([`Reach::certainly`]).
struct Reach<'r, 'i> {
    graph: &'r ImportGraph<'i>,
    group_of: &'r [Option<u32>],
    any: usize,
    code: usize,
    uncounted: usize,
    closure: Closure<u32, BitSet>,
    /// [`Reach::certainly`]'s.
    certain: Closure<u32, BitSet>,
    budget: &'r Budget,
}

impl Reach<'_, '_> {
    /// The groups of the modules loading `m` loads whenever it loads (bit `g`), through the
    /// sites that run on every path through a module body, outside lazy imports, whether or
    /// not a handler may take what they raise.
    fn certainly(&self, m: u32) -> Answer<Rc<BitSet>> {
        let graph = self.graph;
        self.certain.ask(
            self.budget,
            m,
            |m| {
                let mut bits = BitSet::new();
                if let Some(g) = self.group_of[m as usize] {
                    bits.insert(g as usize);
                }
                bits
            },
            |m, out| {
                for (j, (site, target)) in graph.sites(m).into_iter().flatten().enumerate() {
                    if site.timing == Answer::Known(Timing::Body)
                        && site.unconditional
                        && !site.lazy
                        && target.certain().is_some()
                    {
                        out.extend(graph.hops(m, j).iter().map(|hop| hop.to));
                    }
                }
            },
        )
    }

    fn of(&self, m: u32) -> Answer<Rc<BitSet>> {
        self.closure.ask(
            self.budget,
            m,
            |m| self.own(m),
            |m, out| {
                out.extend(
                    self.loads_while_loading(m)
                        .into_iter()
                        .filter_map(|l| match l {
                            Load::Module(w) => Some(w),
                            Load::Any => None,
                        }),
                )
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
        let mut loads = Vec::new();
        for (j, (site, target)) in self.graph.sites(m).into_iter().flatten().enumerate() {
            if !may_run_while_loading(site.timing) {
                continue;
            }
            loads.clear();
            self.graph.may_load(m, j, &mut loads);
            let hops = match target.certain() {
                Some(_) => self.graph.hops(m, j),
                None => Vec::new(),
            };
            for load in &loads {
                match *load {
                    Load::Any => {
                        bits.insert(self.any);
                    }
                    Load::Module(w) if !hops.iter().any(|hop| hop.to == w) => {
                        bits.insert(self.uncounted);
                    }
                    Load::Module(_) => {}
                }
            }
        }
        bits
    }

    /// What the sites that may run after their module body has passed them (in a function, or
    /// a lazy import) may load, joined: the groups of the modules they may load, and of what
    /// loading those may load, and `any`. Code that runs while a group's modules load may call
    /// such a function. What loading a module may load, past the budget, is any module.
    fn late(&self) -> BitSet {
        let mut anything = BitSet::new();
        anything.insert(self.any);
        let anything = Rc::new(anything);
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
                    let bits = match *load {
                        Load::Module(w) => self.of(w).open_means(Rc::clone(&anything)),
                        Load::Any => Rc::clone(&anything),
                    };
                    // The groups, and `any`.
                    for bit in bits.iter().filter(|&b| b <= self.any) {
                        late.insert(bit);
                    }
                }
            }
        }
        late
    }
}

/// The state of a module the run has entered. A module of the group it has not entered has not
/// loaded; one outside the group may have loaded before the entry was imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
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

/// What a run does with a module its import enters ([`GroupRun::entered`]).
enum Enter {
    /// Nothing: it has loaded, or it is loading and the import takes it as it is.
    Pass,
    /// Its body runs now.
    Run,
    Stops,
}

/// The runs of the entries of one group.
struct GroupRun<'r, 'i> {
    graph: &'r ImportGraph<'i>,
    reach: &'r Reach<'r, 'i>,
    budget: &'r Budget,
    group: u32,
    members: &'r [u32],
    /// Some function or lazy import of the project may load a module of the group
    /// ([`Reach::late`]): code a module body runs may call it.
    late_loads: bool,
    /// [`ImportGraph::failing_imports`] of the group.
    failing: FxHashMap<(u32, usize), (u32, &'r str)>,
}

impl GroupRun<'_, '_> {
    fn is_member(&self, m: u32) -> bool {
        self.members.binary_search(&m).is_ok()
    }

    /// The modules imported first in turn, ascending: the members, and the packages holding one
    /// outside the group whose import runs into the group whenever no module of it has loaded
    /// ([`Self::not_loaded_yet`]: `pkg/__init__.py` importing `pkg.core`).
    fn entries(&self) -> Vec<u32> {
        let mut out: Vec<u32> = self.members.to_vec();
        for &m in self.members {
            let parents = self.graph.parents(ModuleRef::Project(m));
            out.extend(
                (parents.into_iter().filter_map(ModuleRef::project))
                    .filter(|&p| !self.is_member(p) && self.not_loaded_yet(p)),
            );
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Whether loading `w`, a module outside the group whose load may or may not run (it may
    /// have loaded before), may load a module of the group, or run code while some function may
    /// load one.
    fn may_disturb(&self, w: u32) -> bool {
        let disturbs = |bits: Rc<BitSet>| {
            bits.contains(self.group as usize)
                || bits.contains(self.reach.any)
                || (self.late_loads && bits.contains(self.reach.code))
        };
        self.reach.of(w).map(disturbs).open_means(true)
    }

    /// Whether `w`, a module outside the group, has not loaded when the entry is imported,
    /// whatever ran before: its load imports a module of the group whenever it runs, so it has
    /// not run whole (no module of the group is imported before the entry), and it cannot be
    /// paused at an import that leads to the entry, since every way from its imports to the
    /// group is an import of a module body ([`Reach::of`]: none `uncounted`, none `any`, and no
    /// function or lazy import may load the group), which would put it in the group.
    fn not_loaded_yet(&self, w: u32) -> bool {
        let unloaded = |(may, must): (Rc<BitSet>, Rc<BitSet>)| {
            !self.late_loads
                && !may.contains(self.reach.any)
                && !may.contains(self.reach.uncounted)
                && must.contains(self.group as usize)
        };
        (self.reach.of(w).zip(self.reach.certainly(w)))
            .map(unloaded)
            .open_means(false)
    }

    /// What an import that runs here does with the module `w` it enters, in `state`.
    fn entered(&self, w: u32, state: &mut FxHashMap<u32, State>) -> Enter {
        let member = self.is_member(w);
        match state.get(&w).copied() {
            Some(State::Loaded | State::Loading { .. }) => Enter::Pass,
            None if member => Enter::Run,
            None if !self.may_disturb(w) => {
                // Loaded before or now, it changes nothing the run reads.
                state.insert(w, State::Loaded);
                Enter::Pass
            }
            None if self.not_loaded_yet(w) => Enter::Run,
            None => Enter::Stops,
        }
    }

    /// Whether a load the run cannot place may change what the run establishes: a module of the
    /// group that has not loaded yet, a module outside it that may disturb the group and that
    /// the run has not loaded ([`Self::may_disturb`]), or any module.
    fn disturbs(&self, load: Load, state: &FxHashMap<u32, State>) -> bool {
        match load {
            Load::Any => true,
            Load::Module(w) => match state.get(&w) {
                Some(State::Loaded | State::Loading { .. }) => false,
                None => self.is_member(w) || self.may_disturb(w),
            },
        }
    }

    /// The failing import, its module and site, the module the name comes from, and the hops
    /// leading to it, when `entry` is imported before any module of the group.
    fn entry(&self, entry: u32) -> Option<(u32, usize, u32, Vec<ImportHop>)> {
        let mut state: FxHashMap<u32, State> = FxHashMap::default();
        let parents = self.graph.parents(ModuleRef::Project(entry));
        let roots = (parents.into_iter().filter_map(ModuleRef::project)).chain([entry]);
        for root in roots {
            match self.entered(root, &mut state) {
                Enter::Pass => continue,
                Enter::Stops => return None,
                Enter::Run => {}
            }
            match self.load(root, &mut state) {
                Step::Loaded => {}
                Step::Fails(module, site, from, chain) => return Some((module, site, from, chain)),
                Step::Stops => return None,
            }
        }
        None
    }

    /// Run the body of `root`, which has not loaded yet, with the loads its sites start, until
    /// it has loaded, a `from` import fails, or what happens next is not established.
    fn load(&self, root: u32, state: &mut FxHashMap<u32, State>) -> Step {
        let graph = self.graph;
        let mut frames = vec![Frame {
            m: root,
            site: 0,
            hops: None,
            then: None,
            via: None,
        }];
        state.insert(root, State::Loading { at: 0 });
        while let Some(frame) = frames.last_mut() {
            let m = frame.m;
            // A module the run enters has import facts: one without may load any module.
            let facts = graph
                .facts(m)
                .expect("a module the run enters has import facts");
            let sites = graph.sites(m).expect("a module the run enters has sites");
            let j = frame.site;
            if let Some((hops, next)) = &mut frame.hops {
                let Some(&hop) = hops.get(*next) else {
                    if let Some(w) = frame.then.take()
                        && self.disturbs(Load::Module(w), state)
                    {
                        return Step::Stops;
                    }
                    frame.hops = None;
                    frame.site += 1;
                    continue;
                };
                *next += 1;
                let w = hop.to;
                if let Some(&State::Loading { at }) = state.get(&w)
                    && !hop.parent
                    && let Some(&(from, name)) = self.failing.get(&(m, j))
                    && from == w
                    && graph
                        .facts(from)
                        .is_some_and(|f| !f.may_bind_before(name, at))
                {
                    let chain = frames.iter().filter_map(|f| f.via).collect();
                    return Step::Fails(m, j, from, chain);
                }
                match self.entered(w, state) {
                    Enter::Pass => {}
                    Enter::Stops => return Step::Stops,
                    Enter::Run => {
                        state.insert(w, State::Loading { at: 0 });
                        frames.push(Frame {
                            m: w,
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
                state.insert(m, State::Loaded);
                frames.pop();
                continue;
            }
            if !self.budget.take(1) || (self.late_loads && facts.runs_code_before(j)) {
                return Step::Stops;
            }
            state.insert(m, State::Loading { at: j });
            let (site, target) = &sites[j];
            if !may_run_while_loading(site.timing) {
                frame.site += 1;
                continue;
            }
            // A hop the run follows leaves a plain `ImportError` raised below it to its module,
            // as the failing import's own site does.
            if site.always_imports_leaving(ImportFailure::AnyImportError)
                && target.certain().is_some()
            {
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
            if (loads.into_iter()).any(|load| self.disturbs(load, state)) {
                return Step::Stops;
            }
            if (target.failure()).is_some_and(|failure| site.always_imports_leaving(failure)) {
                // The import raises whenever it runs, and no handler takes it: nothing after
                // it runs.
                return Step::Stops;
            }
            frame.site += 1;
        }
        Step::Loaded
    }
}
