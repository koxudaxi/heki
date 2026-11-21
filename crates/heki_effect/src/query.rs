//! Walks over the index that answer a question about one function: IR-free, each with a visited
//! set per question and linear in the states it visits (docs/development.md, "Analysis cost").

use crate::{CallHop, CallSite, EffectSummary, FuncKey, ProjectIndex, SiteRole, Slot};
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::Path;

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

/// The calls of a function that run its callees' bodies (`SiteRole::Call`), in the order of
/// its file's call sites. Each file is indexed by caller once, on the first function of it a
/// walk enters, so a walk through many functions of one module does not rescan its sites.
struct CallsOf<'i> {
    index: &'i ProjectIndex,
    by_file: FxHashMap<&'i Path, FxHashMap<&'i FuncKey, Vec<&'i CallSite>>>,
}

impl<'i> CallsOf<'i> {
    fn new(index: &'i ProjectIndex) -> Self {
        Self {
            index,
            by_file: FxHashMap::default(),
        }
    }

    /// `func`'s defining file and its calls there.
    fn of(&mut self, func: &FuncKey) -> Option<(&'i Path, &[&'i CallSite])> {
        let path = self.index.file_of(func)?;
        let file = self.index.files.get(path)?;
        let calls = self.by_file.entry(path).or_insert_with(|| {
            let mut by_caller: FxHashMap<&FuncKey, Vec<&CallSite>> = FxHashMap::default();
            for site in file.call_sites.iter().filter(|s| s.role == SiteRole::Call) {
                by_caller.entry(&site.caller).or_default().push(site);
            }
            by_caller
        });
        Some((path, calls.get(func).map_or(&[][..], Vec::as_slice)))
    }
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
}

impl ProjectIndex {
    /// The shortest chain of calls from `start`, whose parameter `param` changes `what` in place,
    /// down to the first function whose own (local) summary makes the change: breadth first over
    /// the calls that hand the argument on as itself, or the slot as the argument itself
    /// (`helper(p.a)`), in the order of each file's call sites.
    pub fn param_change_chain(
        &self,
        start: &FuncKey,
        param: u8,
        what: &ParamChange,
    ) -> ChangeChain {
        let hop = |caller: &FuncKey, callee: &FuncKey, site: &CallSite, path: &Path| CallHop {
            caller: caller.clone(),
            callee: callee.fqn(),
            span: site.span,
            path: path.to_path_buf(),
        };
        let mut calls = CallsOf::new(self);
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
            let (path, sites) = calls.of(&func).expect("states are summarized functions");
            if self.files[path]
                .local_functions
                .get(&func)
                .is_some_and(|local| local.changes_param(j, &what))
            {
                found = Some(i);
                break;
            }
            for s in sites {
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
        }
    }
}
