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
    /// The parameter of `changed_in` whose change it is.
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
