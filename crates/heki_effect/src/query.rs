//! Walks over the index that answer a question about one function: IR-free, each with a visited
//! set per question and linear in the states it visits (docs/development.md, "Analysis cost").

use crate::{
    AttrKeyPath, BlindSpotReach, BoundCallee, CallHop, CallSite, CellChange, EffectSummary, Facts,
    FuncKey, ProjectIndex, Runner, SharedState, SiteRole, Slot,
};
use heki_ir::walk::{Answer, Budget, Closure, Visit, reach, reach_breadth_first};
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

/// What a function changes in place: its parameter itself (`None`) or a slot of it.
pub type ParamChange = Option<Slot>;

impl EffectSummary {
    /// Whether the summary changes its parameter `j` itself (`None`) or the container held in a
    /// slot of it (`Some`) in place.
    pub fn changes_param(&self, j: u8, what: &ParamChange) -> bool {
        match what {
            None => self
                .mutated_param_paths
                .get(&j)
                .is_some_and(|paths| paths.contains("")),
            Some(slot) => self
                .inplace_param_slots
                .get(&j)
                .is_some_and(|slots| slots.contains(slot)),
        }
    }
}

/// The call sites of each function in the order of its file's call sites, indexed by caller
/// one file at a time, on the first function of the file a walk needs the calls of. The walks of
/// one run share it ([`ProjectIndex::calls_by_caller`]), so neither a walk through many
/// functions of one module nor many walks into one module rescan its sites.
pub struct CallsByCaller<'i> {
    index: &'i ProjectIndex,
    by_file: FxHashMap<&'i Path, FxHashMap<&'i FuncKey, Vec<&'i CallSite>>>,
}

impl<'i> CallsByCaller<'i> {
    /// The call sites of `func`, a summarized function, in its defining file.
    fn of(&mut self, func: &FuncKey) -> &[&'i CallSite] {
        let path = self
            .index
            .file_of(func)
            .expect("call sites are asked of summarized functions");
        let file = &self.index.files[path];
        let calls = self.by_file.entry(path).or_insert_with(|| {
            let mut by_caller: FxHashMap<&FuncKey, Vec<&CallSite>> = FxHashMap::default();
            for site in &file.call_sites {
                by_caller.entry(&site.caller).or_default().push(site);
            }
            by_caller
        });
        calls.get(func).map_or(&[][..], Vec::as_slice)
    }
}

/// Shared state a call hands to a callee that changes it ([`ProjectIndex::handed_changes_of`]).
#[derive(Debug, Clone, Copy)]
pub struct HandedChange<'s> {
    pub site: &'s CallSite,
    pub callee: &'s BoundCallee,
    /// The callee's parameter the state is.
    pub param: u8,
    pub state: &'s SharedState,
}

impl SharedState {
    /// Whether this is a module global (`cell` `None`) or the closure cell `cell`.
    pub fn is_change_of(&self, cell: Option<&CellChange>) -> bool {
        match (cell, self) {
            (None, SharedState::Global(_)) => true,
            (Some(c), SharedState::Cell(d)) => c == d,
            _ => false,
        }
    }
}

/// A call of a function [`ProjectIndex::callers_within`] found.
#[derive(Debug, Clone)]
pub struct CallerSite<'i> {
    /// The calling function.
    pub caller: &'i FuncKey,
    /// The function it calls here: the start, or a caller found one level below.
    pub callee: &'i FuncKey,
    /// 1 for a direct caller of the start.
    pub depth: usize,
    /// The caller's file.
    pub path: &'i Path,
    /// The sites at the call's position: the call, then the uses of its result (iterated,
    /// returned, handed on).
    pub sites: Vec<&'i CallSite>,
}

/// [`ProjectIndex::callers_within`].
#[derive(Debug, Clone, Default)]
pub struct Callers<'i> {
    /// Level by level, each level by callee, caller, and position.
    pub sites: Vec<CallerSite<'i>>,
    /// The walk stopped at its limit of sites.
    pub truncated: bool,
}

/// One state of [`ProjectIndex::param_change_chain`]'s breadth-first walk: a function whose
/// parameter changes `what`, the state it was reached from, and the call into it.
struct ChainState {
    func: FuncKey,
    param: u8,
    what: ParamChange,
    parent: Option<usize>,
    hop: Option<CallHop>,
}

/// [`ProjectIndex::param_change_chain`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeChain {
    /// One hop per call, from the start function down to the function whose own body makes the
    /// change; empty when the start function makes it itself or no body is found.
    pub hops: Vec<CallHop>,
    /// The function whose own body makes the change (the start function when `hops` is
    /// empty), `None` when no function on the way makes it in its own body.
    pub changed_in: Option<FuncKey>,
    /// [`ProjectIndex::param_change_chain`]: the parameter of `changed_in` whose change it is.
    pub param: Option<u8>,
}

impl ProjectIndex {
    /// An empty [`CallsByCaller`] for the walks of one run.
    pub fn calls_by_caller(&self) -> CallsByCaller<'_> {
        CallsByCaller {
            index: self,
            by_file: FxHashMap::default(),
        }
    }

    /// [`BlindSpotReach`]: one walk from every `async def` over the calls that run what they
    /// hand on on the caller's thread, and one from every call inside a loop or once per item,
    /// each state visited once per walk. The states are a function's facts of one kind, its body
    /// or what iterating its result runs ([`Facts`]), as calls hand them on ([`Self::delivery`]):
    /// a helper returning a generator it does not iterate runs no generator body where it is
    /// called, only where its result is iterated.
    pub fn blind_spot_reach(&self) -> BlindSpotReach {
        let mut calls = self.calls_by_caller();
        let mut async_defs = Vec::new();
        let mut in_loops = Vec::new();
        for file in self.files.values() {
            for (func, s) in &file.functions {
                if s.is_async {
                    async_defs.push((func, self.own_facts(func)));
                }
            }
            for site in &file.call_sites {
                if site.loop_depth > 0 || site.runner == Runner::PerItem {
                    in_loops.extend(self.handed_on(site, self.own_facts(&site.caller)));
                }
            }
        }
        BlindSpotReach {
            on_event_loop: self
                .run_from(&mut calls, async_defs, |site| site.runner != Runner::Worker),
            below_loop: self.run_from(&mut calls, in_loops, |_| true),
        }
    }

    /// The functions whose own body runs, itself or through what its calls hand on, a call heki
    /// is blind to (`FileSummary::blind_spots`), a call it cannot resolve or an A1 hazard
    /// (`EffectSummary::unknown_calls`), or a function whose body it does not know (defined only
    /// in a stub): what they do is not all seen. One closure over the facts calls hand on, as
    /// [`Self::blind_spot_reach`] walks them, each state expanded once.
    pub fn reaching_blind_spots(&self) -> FxHashSet<FuncKey> {
        let mut calls = self.calls_by_caller();
        let closure: Closure<(&FuncKey, Facts), bool> = Closure::new();
        let budget = Budget::unmetered("reaching_blind_spots");
        let blind = |(func, facts): (&FuncKey, Facts)| {
            let path = self.file_of(func);
            facts == self.own_facts(func)
                && (self.summary(func).is_some_and(|s| s.unknown_calls)
                    || path.is_some_and(crate::is_stub)
                    || path.is_some_and(|p| {
                        (self.files[p].blind_spots.as_ref()).is_some_and(|b| b.contains_key(func))
                    }))
        };
        let mut out = FxHashSet::default();
        for file in self.files.values() {
            for func in file.functions.keys() {
                let start = (func, self.own_facts(func));
                let reaches = closure.ask(&budget, start, blind, |(func, facts), next| {
                    for site in calls.of(func) {
                        next.extend(self.handed_on(site, facts));
                    }
                });
                if reaches == Answer::Known(Rc::new(true)) {
                    out.insert(func.clone());
                }
            }
        }
        out
    }

    /// The facts the calls of `func`'s own body run in: its body, or, for a generator
    /// function, what iterating its result runs.
    fn own_facts(&self, func: &FuncKey) -> Facts {
        if self.summary(func).is_some_and(|s| s.is_generator) {
            Facts::Result
        } else {
            Facts::Body
        }
    }

    /// The facts of its project callees that `site` runs in its caller's facts `into`.
    fn handed_on<'s>(
        &'s self,
        site: &'s CallSite,
        into: Facts,
    ) -> impl Iterator<Item = (&'s FuncKey, Facts)> {
        let own = self.own_facts(&site.caller);
        site.project_callees().iter().filter_map(move |bc| {
            let d = self.delivery(site, self.summary(&bc.callee)?)?;
            let lands = if d.into == Facts::Body {
                own
            } else {
                Facts::Result
            };
            (lands == into).then_some((&bc.callee, d.from))
        })
    }

    /// The functions whose own body runs in `starts` or in what a call `follows` accepts hands
    /// on from one of them, transitively.
    fn run_from<'i>(
        &'i self,
        calls: &mut CallsByCaller<'i>,
        starts: Vec<(&'i FuncKey, Facts)>,
        follows: impl Fn(&CallSite) -> bool,
    ) -> FxHashSet<FuncKey> {
        let mut out = FxHashSet::default();
        let _ = reach::<_, ()>(
            &Budget::unmetered("blind_spot_reach"),
            starts,
            |(func, facts), next| {
                for site in calls.of(func) {
                    if follows(site) {
                        next.extend(self.handed_on(site, facts));
                    }
                }
            },
            |(func, facts)| {
                if facts == self.own_facts(func) {
                    out.insert(func.clone());
                }
                Visit::Expand
            },
        );
        out
    }

    /// The shortest chain of calls from `start`, whose parameter `param` changes `what` in place,
    /// down to the first function whose own (local) summary makes the change: breadth first over
    /// the calls that hand the argument on as itself, or the slot as the argument itself
    /// (`helper(p.a)`), in the order of each file's call sites.
    pub fn param_change_chain(
        &self,
        calls: &mut CallsByCaller<'_>,
        start: &FuncKey,
        param: u8,
        what: &ParamChange,
    ) -> ChangeChain {
        self.param_change_chain_entering(calls, start, param, what, &|_, _| true)
    }

    /// [`Self::param_change_chain`] entering only the callee parameters `enters` admits (`MUT110`
    /// leaves out a helper whose parameter is annotated, which is checked on its own).
    pub fn param_change_chain_entering(
        &self,
        calls: &mut CallsByCaller<'_>,
        start: &FuncKey,
        param: u8,
        what: &ParamChange,
        enters: &dyn Fn(&FuncKey, u8) -> bool,
    ) -> ChangeChain {
        let hop = |caller: &FuncKey, callee: &FuncKey, site: &CallSite, path: &Path| CallHop {
            caller: caller.clone(),
            callee: callee.fqn(),
            span: site.span,
            path: path.to_path_buf(),
        };
        let mut states = vec![ChainState {
            func: start.clone(),
            param,
            what: what.clone(),
            parent: None,
            hop: None,
        }];
        let mut visited: FxHashSet<(FuncKey, u8, ParamChange)> = FxHashSet::default();
        visited.insert((start.clone(), param, what.clone()));
        let mut found = None;
        let mut i = 0;
        while i < states.len() {
            let (func, j, what) = (
                states[i].func.clone(),
                states[i].param,
                states[i].what.clone(),
            );
            let path = self
                .file_of(&func)
                .expect("states are summarized functions");
            if self.files[path]
                .local_functions
                .get(&func)
                .is_some_and(|local| local.changes_param(j, &what))
            {
                found = Some(i);
                break;
            }
            let sites = calls.of(&func);
            for &s in sites.iter().filter(|s| s.role == SiteRole::Call) {
                let callees = s
                    .project_callees()
                    .iter()
                    .filter_map(|c| Some((c, self.summary(&c.callee)?)));
                for (c, cs) in callees {
                    for (j2, o) in c.params.iter().enumerate().take(64) {
                        let j2 = u8::try_from(j2).expect("at most 64 parameters");
                        let as_itself = o.param == Some((j, None));
                        // The same change of the argument itself, the same slot of it, or the
                        // argument itself when it is that slot (`helper(p.a)`).
                        let next = match &what {
                            Some(_) if as_itself && cs.changes_param(j2, &what) => {
                                Some(what.clone())
                            }
                            None => (as_itself && cs.changes_param(j2, &None)).then_some(None),
                            Some(slot) => (o.param_slot.as_ref() == Some(&(j, slot.clone()))
                                && cs.changes_param(j2, &None))
                            .then_some(None),
                        };
                        if let Some(next) = next
                            && enters(&c.callee, j2)
                            && visited.insert((c.callee.clone(), j2, next.clone()))
                        {
                            states.push(ChainState {
                                func: c.callee.clone(),
                                param: j2,
                                what: next,
                                parent: Some(i),
                                hop: Some(hop(&func, &c.callee, s, path)),
                            });
                        }
                    }
                }
            }
            i += 1;
        }
        let mut hops = Vec::new();
        let mut at = found;
        while let Some(i) = at {
            hops.extend(states[i].hop.clone());
            at = states[i].parent;
        }
        hops.reverse();
        ChangeChain {
            hops,
            changed_in: found.map(|i| states[i].func.clone()),
            param: found.map(|i| states[i].param),
        }
    }
}

impl ProjectIndex {
    /// The calls of `func` and, breadth first up to `depth` levels, of the callers found: each
    /// function expanded once, at most `limit` calls (the sites at one position are one call).
    pub fn callers_within<'i>(
        &'i self,
        calls: &mut CallsByCaller<'i>,
        func: &FuncKey,
        depth: usize,
        limit: usize,
    ) -> Callers<'i> {
        let mut out = Callers::default();
        let (start, _) = self
            .func_files
            .get_key_value(func)
            .expect("callers are asked of summarized functions");
        let mut visited: FxHashSet<&FuncKey> = FxHashSet::from_iter([start]);
        let mut level = vec![start];
        for d in 1..=depth {
            let mut next = Vec::new();
            for &callee in &level {
                let mut callers: Vec<&FuncKey> = self
                    .reverse_call_graph
                    .get(callee)
                    .into_iter()
                    .flatten()
                    .collect();
                callers.sort();
                for caller in callers {
                    let path = self.func_files[caller].as_path();
                    let calling = calls
                        .of(caller)
                        .iter()
                        .filter(|s| s.project_callees().iter().any(|bc| bc.callee == *callee));
                    let first = out.sites.len();
                    for &site in calling {
                        if let Some(c) = out.sites[first..]
                            .iter_mut()
                            .find(|c| c.sites[0].span == site.span)
                        {
                            c.sites.push(site);
                            continue;
                        }
                        if out.sites.len() == limit {
                            out.truncated = true;
                            return out;
                        }
                        out.sites.push(CallerSite {
                            caller,
                            callee,
                            depth: d,
                            path,
                            sites: vec![site],
                        });
                    }
                    if visited.insert(caller) {
                        next.push(caller);
                    }
                }
            }
            level = next;
        }
        out
    }

    /// The chain from the method `start`, whose final summary changes `self.<attr>`, down to
    /// the function whose own body makes the change: breadth first over calls on `self` that
    /// change it, to a method that changes it itself or that hands `self.<attr>` to a call
    /// changing it, followed down through [`Self::param_change_chain`].
    pub fn self_attr_chain(
        &self,
        calls: &mut CallsByCaller<'_>,
        start: &FuncKey,
        attr: &AttrKeyPath,
    ) -> ChangeChain {
        let runs_here = |site: &CallSite, callee: &EffectSummary| {
            self.delivery(site, callee)
                .is_some_and(|d| d.into == Facts::Body)
        };
        let found = self.nearest(
            calls,
            start,
            |site, callee| {
                site.recv_is_self
                    && callee.mutates_self_attrs.contains(attr)
                    && runs_here(site, callee)
            },
            |key, sites| {
                self.local_summary(key)
                    .is_some_and(|l| l.mutates_self_attrs.contains(attr))
                    || sites.iter().any(|s| self.hands_on(s, attr).is_some())
            },
        );
        // A function handing the object on: the hop into the callee, then the callee's chain.
        let changes_itself = |f: &FuncKey| {
            self.local_summary(f)
                .is_some_and(|l| l.mutates_self_attrs.contains(attr))
        };
        let through = found
            .changed_in
            .clone()
            .filter(|f| !changes_itself(f))
            .and_then(|f| {
                let path = self.file_of(&f)?;
                let sites = calls.of(&f).to_vec();
                let (site, (bc, j, what)) = sites
                    .iter()
                    .find_map(|s| Some((s, self.hands_on(s, attr)?)))?;
                let hop = CallHop {
                    caller: f.clone(),
                    callee: bc.callee.fqn(),
                    span: site.span,
                    path: path.to_path_buf(),
                };
                let below = self.param_change_chain(calls, &bc.callee, j, &what);
                let mut hops = found.hops.clone();
                hops.push(hop);
                hops.extend(below.hops);
                Some(ChangeChain {
                    hops,
                    changed_in: below.changed_in.or_else(|| Some(bc.callee.clone())),
                    param: below.param.or(Some(j)),
                })
            });
        through.unwrap_or(found)
    }

    /// A callee of `site` handed `self.<attr>` of the calling method that changes it
    /// (`helper(self.items)`), with the parameter and the change its chain follows.
    fn hands_on<'s>(
        &self,
        site: &'s CallSite,
        attr: &AttrKeyPath,
    ) -> Option<(&'s BoundCallee, u8, ParamChange)> {
        site.project_callees().iter().find_map(|bc| {
            let callee = self.summary(&bc.callee).filter(|c| {
                self.delivery(site, c)
                    .is_some_and(|d| d.into == Facts::Body)
            })?;
            bc.params.iter().enumerate().take(64).find_map(|(j, o)| {
                let j = u8::try_from(j).expect("at most 64 parameters");
                let paths = callee.mutated_param_paths.get(&j)?;
                if o.self_attr.as_deref() != Some(attr.attr.as_str()) {
                    return None;
                }
                // The argument is the attribute's object: changed itself, or a slot of it.
                let what = if paths.contains("") {
                    None
                } else {
                    callee
                        .inplace_param_slots
                        .get(&j)
                        .and_then(|slots| slots.first().cloned())
                };
                Some((bc, j, what))
            })
        })
    }

    /// The functions whose own calls heki cannot all resolve (`unknown_calls`), or whose body it
    /// does not know (defined only in a stub), among `start` and the callees whose facts run in
    /// it (in its body, or in what iterating its result runs), nearest first, each with the
    /// shortest chain to it: at most `limit`. A call cycle stopped at its recomputation budget is
    /// unknown as a whole: each of its functions is found itself.
    pub fn unknown_within(
        &self,
        calls: &mut CallsByCaller<'_>,
        start: &FuncKey,
        limit: usize,
    ) -> Vec<ChangeChain> {
        self.found_within(
            calls,
            start,
            |site, callee| self.delivery(site, callee).is_some(),
            |key, _| {
                self.summary(key).is_some_and(|s| s.unknown_calls)
                    || self.file_of(key).is_some_and(crate::is_stub)
            },
            limit,
        )
    }

    /// Every function whose own body changes a module global (`cell` `None`) or the closure
    /// cell `cell`, or hands it to a callee that changes it, among `start` and the callees whose
    /// facts run in it and hand that change on, each with the shortest chain to it.
    pub fn state_changes(
        &self,
        calls: &mut CallsByCaller<'_>,
        start: &FuncKey,
        cell: Option<&CellChange>,
    ) -> Vec<ChangeChain> {
        let hands_on = |s: &EffectSummary| match cell {
            Some(c) => s.changes_cells().contains(c),
            None => s.changes_globals(),
        };
        self.found_within(
            calls,
            start,
            |site, callee| hands_on(callee) && self.delivery(site, callee).is_some(),
            |key, sites| {
                let own = self.local_summary(key).is_some_and(|local| {
                    local
                        .changed_state()
                        .iter()
                        .any(|(state, _)| state.is_change_of(cell))
                });
                own || self
                    .handed_changes(key, sites)
                    .iter()
                    .any(|h| h.state.is_change_of(cell))
            },
            usize::MAX,
        )
    }

    /// [`Self::handed_changes`] of the calls of `func`.
    pub fn handed_changes_of<'i>(
        &'i self,
        calls: &mut CallsByCaller<'i>,
        func: &FuncKey,
    ) -> Vec<HandedChange<'i>> {
        let sites = calls.of(func).to_vec();
        self.handed_changes(func, &sites)
    }

    /// The shared state `func`'s calls `sites` hand to a callee whose facts running there change
    /// that argument (`helper(CACHE)`, `REGISTRY.add(x)`): the propagation of
    /// `SharedStateFacts::handed`, call by call.
    fn handed_changes<'s>(
        &'s self,
        func: &FuncKey,
        sites: &[&'s CallSite],
    ) -> Vec<HandedChange<'s>> {
        let Some(local) = self
            .local_summary(func)
            .filter(|l| l.shared_state.is_some())
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for &site in sites {
            for (ci, bc) in site.project_callees().iter().enumerate() {
                let facts = self
                    .summary(&bc.callee)
                    .and_then(|callee| callee.facts(self.delivery(site, callee)?.from));
                for h in local.handed_at(site.span, ci) {
                    if facts.is_some_and(|f| f.changes_argument(h.param)) {
                        out.push(HandedChange {
                            site,
                            callee: bc,
                            param: h.param,
                            state: &h.state,
                        });
                    }
                }
            }
        }
        out
    }

    /// The nearest function, among `start` and the callees whose facts run in it, whose own
    /// typed writes include one on an object that none of its parameters is and that it does
    /// not construct: with typed writes left where they are made, its callers do not see it.
    pub fn first_object_write(
        &self,
        calls: &mut CallsByCaller<'_>,
        start: &FuncKey,
    ) -> ChangeChain {
        self.nearest(
            calls,
            start,
            |site, callee| self.delivery(site, callee).is_some(),
            |key, _| {
                self.local_summary(key).is_some_and(|local| {
                    local
                        .typed_field_write_sites
                        .iter()
                        .any(|w| w.obj_params == 0 && !w.fresh)
                })
            },
        )
    }

    /// The summary of `func` before propagation (its own body's facts).
    pub fn local_summary(&self, func: &FuncKey) -> Option<&EffectSummary> {
        let path = self.file_of(func)?;
        self.files[path].local_functions.get(func).map(|s| &**s)
    }

    /// Breadth first from `start` over the calls `enters` accepts (given the site and the
    /// callee's final summary), the first function `found` accepts, with the hops to it (an
    /// empty chain without `changed_in` when there is none).
    fn nearest(
        &self,
        calls: &mut CallsByCaller<'_>,
        start: &FuncKey,
        enters: impl Fn(&CallSite, &EffectSummary) -> bool,
        found: impl Fn(&FuncKey, &[&CallSite]) -> bool,
    ) -> ChangeChain {
        self.found_within(calls, start, enters, found, 1)
            .pop()
            .unwrap_or_default()
    }

    /// Breadth first from `start` over the calls `enters` accepts, each function `found`
    /// accepts with the shortest hops to it, in the order reached: the first `limit`. Each
    /// function is entered once.
    fn found_within<'i>(
        &self,
        calls: &mut CallsByCaller<'i>,
        start: &FuncKey,
        enters: impl Fn(&CallSite, &EffectSummary) -> bool,
        found: impl Fn(&FuncKey, &[&CallSite]) -> bool,
        limit: usize,
    ) -> Vec<ChangeChain> {
        let (start, _) = calls
            .index
            .func_files
            .get_key_value(start)
            .expect("walks start at summarized functions");
        let calls = RefCell::new(calls);
        // The hop into each function from the first function listing it: the nearest one.
        let mut into: FxHashMap<&'i FuncKey, (&'i FuncKey, CallHop)> = FxHashMap::default();
        let mut reached: Vec<&'i FuncKey> = Vec::new();
        let _ = reach_breadth_first::<_, ()>(
            &Budget::unmetered("found_within"),
            [start],
            |func, next| {
                let path = self
                    .file_of(func)
                    .expect("functions reached are summarized");
                for &site in calls.borrow_mut().of(func) {
                    for bc in site.project_callees() {
                        if self
                            .summary(&bc.callee)
                            .is_some_and(|callee| enters(site, callee))
                        {
                            into.entry(&bc.callee).or_insert_with(|| {
                                let hop = CallHop {
                                    caller: func.clone(),
                                    callee: bc.callee.fqn(),
                                    span: site.span,
                                    path: path.to_path_buf(),
                                };
                                (func, hop)
                            });
                            next.push(&bc.callee);
                        }
                    }
                }
            },
            |func| {
                if !found(func, calls.borrow_mut().of(func)) {
                    return Visit::Expand;
                }
                reached.push(func);
                if reached.len() == limit {
                    Visit::Stop(())
                } else {
                    Visit::Expand
                }
            },
        );
        let chain_to = |func: &FuncKey| {
            let mut hops = Vec::new();
            let mut at = func;
            while at != start {
                let (caller, hop) = &into[at];
                hops.push(hop.clone());
                at = caller;
            }
            hops.reverse();
            ChangeChain {
                hops,
                changed_in: Some(func.clone()),
                param: None,
            }
        };
        reached.into_iter().map(chain_to).collect()
    }
}
