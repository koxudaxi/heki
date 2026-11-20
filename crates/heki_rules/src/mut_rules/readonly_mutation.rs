//! `MUT108` (`readonly-type-mutation`) and `MUT110` (`readonly-param-mutation`): in-place
//! changes of what standard types declare read-only, through one analysis. Type checkers stop
//! the rebinding of a frozen dataclass or `NamedTuple` field, a `Final` / `ReadOnly` attribute,
//! or a `ReadOnly` `TypedDict` key, but not an in-place change of the container it holds,
//! directly, through an alias, or in a helper (`MUT108`); and they trust a helper's signature, so
//! a `Sequence` / `Mapping` parameter handed to an unannotated or `Any` helper that changes it
//! goes unnoticed (`MUT110`).
//!
//! A change is reported where heki sees that the object is read-only: an own change at the
//! change, a helper's change at the call that hands the attribute (the object holding it, or the
//! read-only parameter) over, with the calls down to the change as related locations.

use super::{Allowance, initializes_itself, runs_inside_allowed};
use crate::{CheckContext, ModuleUnit, SitesByCaller};
use heki_ast::Span;
use heki_diagnostics::Diagnostic;
use heki_effect::{
    BoundCallee, CallHop, CallSite, EffectSummary, FuncKey, ProjectIndex, ReadOnlyAttr,
    ReadOnlyKind, SiteRole, Slot, WriteKind,
};
use rustc_hash::FxHashSet;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// What a function changes in place: its parameter itself (`None`) or a slot of it.
type Changed = Option<Slot>;

/// Write kinds `MUT108` reports. Rebinding and deletion belong to type checkers; `setattr` /
/// `object.__setattr__` and `__dict__` writes are deliberate bypasses.
const IN_PLACE: &[WriteKind] = &[
    WriteKind::ContainerMut,
    WriteKind::AliasContainerMut,
    WriteKind::IndirectHelper,
];

const FIX: &str = "copy it first or store an immutable type such as `tuple` / `frozenset`";

pub(crate) fn run(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    let (fields, params) = (
        cx.selection.is_enabled("MUT108"),
        cx.selection.is_enabled("MUT110"),
    );
    if !fields && !params {
        return;
    }
    debug_assert!(
        !fields || cx.index.propagates_typed_writes(),
        "`typed_writes` decides for MUT108"
    );
    let mut rule = Rule {
        cx,
        fields,
        params,
        sites: None,
        seen: FxHashSet::default(),
        none: Allowance::default(),
    };
    let mut paths: Vec<&Path> = cx.index.files.keys().map(|p| p.as_path()).collect();
    paths.sort();
    for path in paths {
        let Some(unit) = cx.unit(path) else {
            continue;
        };
        let file = &cx.index.files[path];
        let mut funcs: Vec<(&FuncKey, &EffectSummary)> = file
            .functions
            .iter()
            .filter(|(k, _)| fields && cx.index.file_of(k) == Some(path))
            .collect();
        funcs.sort_by(|a, b| a.0.cmp(b.0));
        for (key, s) in funcs {
            rule.own_changes(unit, key, s, out);
        }
        // Without `MUT108`, only the calls of the functions with a read-only parameter matter.
        // The call itself hands its arguments over; the other sites of the object it returns
        // (`SiteRole`) hand over nothing more.
        let readonly: FxHashSet<&str> = cx
            .decls
            .module(&file.module_name)
            .into_iter()
            .flat_map(|m| &m.functions)
            .filter(|(_, d)| params && d.readonly_params != 0)
            .map(|(q, _)| q.as_str())
            .collect();
        for site in file.call_sites.iter().filter(|s| {
            s.role == SiteRole::Call
                && (fields || readonly.contains(s.caller.qualname.as_str()))
                && cx.index.file_of(&s.caller) == Some(path)
        }) {
            rule.handed_over(unit, path, site, out);
        }
    }
}

/// One project callee of a call site, with its final summary.
struct Call<'s> {
    unit: &'s ModuleUnit<'s>,
    path: &'s Path,
    site: &'s CallSite,
    bc: &'s BoundCallee,
    callee: &'s EffectSummary,
}

struct Rule<'c, 'a> {
    cx: &'c CheckContext<'a>,
    /// `MUT108` (read-only attributes) runs.
    fields: bool,
    /// `MUT110` (read-only parameters) runs.
    params: bool,
    /// Built on the first need (`runs_inside_allowed`).
    sites: Option<SitesByCaller<'a>>,
    /// `(rule, function, span, target)` already reported.
    seen: FxHashSet<(&'static str, FuncKey, Span, String)>,
    /// No allowed writers: only a class's own initialization is allowed.
    none: Allowance,
}

impl Rule<'_, '_> {
    /// The function's own in-place changes of read-only attributes.
    fn own_changes(
        &mut self,
        unit: &ModuleUnit<'_>,
        key: &FuncKey,
        s: &EffectSummary,
        out: &mut Vec<Diagnostic>,
    ) {
        // Filling an instance the function constructs itself is its construction.
        let own = s
            .typed_field_write_sites
            .iter()
            .filter(|w| w.via_chain.is_empty() && !w.fresh);
        for w in own {
            let Some(ro) = self.protected(&w.class_qualname, &w.attr, w.kind) else {
                continue;
            };
            if self.initializes(key, &w.class_qualname, w.obj_params) {
                continue;
            }
            let (target, what) = self.what(&ro, &w.attr);
            let message = format!(
                "`{}` changes {what} in place ({}): {}; {FIX}",
                key.qualname,
                w.kind.as_str(),
                why(ro.kind)
            );
            self.emit(out, "MUT108", unit, key, w.span, target, message, &[]);
        }
    }

    /// `(class, attr)` is a read-only attribute and `kind` an in-place change.
    fn protected(&self, class: &str, attr: &str, kind: WriteKind) -> Option<ReadOnlyAttr> {
        IN_PLACE
            .contains(&kind)
            .then(|| self.cx.decls.readonly_attr(class, attr))
            .flatten()
    }

    /// The class family's own initialization of `self` (`__init__`, `__post_init__`, ...),
    /// directly or through helpers that only run inside it.
    fn initializes(&mut self, key: &FuncKey, class: &str, obj: u64) -> bool {
        let mut family: FxHashSet<String> =
            self.cx.decls.subclasses_of(class).into_iter().collect();
        family.insert(class.to_owned());
        if initializes_itself(self.cx, key, &family, obj) {
            return true;
        }
        let sites = self.sites.get_or_insert_with(|| self.cx.sites_by_caller());
        runs_inside_allowed(self.cx, sites, &self.none, &family, key, obj)
    }

    /// The target (`Class.attr`) and how a message names it.
    fn what(&self, ro: &ReadOnlyAttr, attr: &str) -> (String, String) {
        let class = self.cx.decls.class_qualname(&ro.class).unwrap_or(&ro.class);
        let target = format!("{class}.{attr}");
        let what = match ro.kind {
            ReadOnlyKind::Frozen => format!("the field `{target}` of a frozen instance"),
            ReadOnlyKind::Declared if self.cx.decls.is_typed_dict(&ro.class) => {
                format!("the `ReadOnly` key `{attr}` of `{class}`")
            }
            ReadOnlyKind::Declared => {
                format!("`{target}`, declared read-only (`Final` / `ReadOnly`),")
            }
        };
        (target, what)
    }

    /// What one call hands over that its callee changes in place: a read-only attribute (or the
    /// object holding it), or a read-only parameter of the caller.
    fn handed_over(
        &mut self,
        unit: &ModuleUnit<'_>,
        path: &Path,
        site: &CallSite,
        out: &mut Vec<Diagnostic>,
    ) {
        let cx = self.cx;
        // `MUT110` needs a read-only parameter of the caller (`run` hands over only such calls
        // when `MUT108` is off).
        let readonly = cx
            .decls
            .function_decl(&site.caller)
            .filter(|d| self.params && d.readonly_params != 0);
        let callees = site
            .project_callees()
            .iter()
            .filter_map(|bc| Some((bc, cx.index.summary(&bc.callee)?)));
        for (bc, callee) in callees {
            let call = Call {
                unit,
                path,
                site,
                bc,
                callee,
            };
            let annotated = cx
                .decls
                .function_decl(&bc.callee)
                .map_or(0, |d| d.readonly_params | d.mutable_params);
            for (k, o) in bc.params.iter().enumerate().take(64) {
                let k = u8::try_from(k).expect("at most 64 parameters");
                let itself = changes(callee, k, &None);
                // An instance the caller constructs (or an attribute of one) is filled before
                // anyone else holds it.
                let shared = self.fields && !o.fresh;
                if itself
                    && shared
                    && let Some((class, attr)) = &o.typed_field
                {
                    // `helper(obj.attr)`: the object is the caller's parameter `p` for `p.attr`.
                    let obj = match &o.param {
                        Some((p, Some(a))) if a == attr => 1u64 << p,
                        _ => 0,
                    };
                    self.handed_attr(&call, k, class, attr, None, obj, out);
                }
                // `helper(obj)`: the slots of the object the callee changes (keys of a
                // `TypedDict`, attributes of other classes).
                if shared && let Some(class) = &o.typed_instance {
                    let typed_dict = cx.decls.is_typed_dict(class);
                    for slot in callee.inplace_param_slots.get(&k).into_iter().flatten() {
                        let (Slot::Attr(name) | Slot::Key(name)) = slot;
                        if matches!(slot, Slot::Key(_)) == typed_dict {
                            let what = Some(slot.clone());
                            self.handed_attr(&call, k, class, name, what, o.may_params, out);
                        }
                    }
                }
                // A read-only parameter handed over as itself, to a parameter that no
                // annotation makes read-only (that function reports its own changes) or mutable
                // (a type checker reports the argument).
                if itself
                    && annotated & (1u64 << k) == 0
                    && let Some((p, None)) = &o.param
                    && let Some(decl) = readonly.filter(|d| d.readonly_params & (1u64 << p) != 0)
                {
                    let name = &decl.param_names[usize::from(*p)];
                    let message = format!(
                        "`{}` passes `{name}` to `{}`, which changes it in place: `{name}` is annotated read-only (`Sequence` / `Mapping`), so the caller was promised that its argument is only read, but the helper changes the caller's object; pass a copy (`list({name})` / `dict({name})`), or annotate the helper's parameter with a mutable type so that the type checker shows the mismatch",
                        site.caller.qualname,
                        bc.callee.fqn()
                    );
                    let chain = self.chain(&call, k, None);
                    let (caller, span) = (&site.caller, site.span);
                    self.emit(
                        out,
                        "MUT110",
                        unit,
                        caller,
                        span,
                        name.clone(),
                        message,
                        &chain,
                    );
                }
            }
        }
    }

    /// Report a read-only attribute that a call hands to a callee changing it (`what`) in place,
    /// unless the callee records the change itself (it, or a function it calls, sees the
    /// object's type and reports it) or the change is the class's own initialization.
    #[allow(clippy::too_many_arguments)]
    fn handed_attr(
        &mut self,
        call: &Call<'_>,
        k: u8,
        class: &str,
        attr: &str,
        what: Changed,
        obj: u64,
        out: &mut Vec<Diagnostic>,
    ) {
        let Some(ro) = self.cx.decls.readonly_attr(class, attr) else {
            return;
        };
        let recorded = call.callee.typed_field_writes.iter().any(|t| {
            t.attr == attr
                && !t.fresh
                && self
                    .protected(&t.class_qualname, &t.attr, t.kind)
                    .is_some_and(|r| r.class == ro.class)
        });
        if recorded || self.initializes(&call.site.caller, class, obj) {
            return;
        }
        let (target, named) = self.what(&ro, attr);
        let message = format!(
            "`{}` changes {named} in place via `{}` (indirect_helper): {}; {FIX}",
            call.site.caller.qualname,
            call.bc.callee.fqn(),
            why(ro.kind)
        );
        let chain = self.chain(call, k, what);
        let (unit, caller, span) = (call.unit, &call.site.caller, call.site.span);
        self.emit(out, "MUT108", unit, caller, span, target, message, &chain);
    }

    /// The hop of the call into its callee (whose parameter `k` changes `what` in place), then
    /// the shortest chain of calls down to the function whose own body makes the change.
    fn chain(&self, call: &Call<'_>, k: u8, what: Changed) -> Vec<CallHop> {
        let index = self.cx.index;
        let hop = |caller: &FuncKey, callee: &FuncKey, span: Span, path: &Path| CallHop {
            caller: caller.clone(),
            callee: callee.fqn(),
            span,
            path: path.to_path_buf(),
        };
        let start = &call.bc.callee;
        // Breadth-first states: (function, parameter, change, parent state, hop into it).
        let mut states: Vec<(FuncKey, u8, Changed, Option<usize>, CallHop)> = vec![(
            start.clone(),
            k,
            what.clone(),
            None,
            hop(&call.site.caller, start, call.site.span, call.path),
        )];
        let mut visited: FxHashSet<(FuncKey, u8, Changed)> = FxHashSet::default();
        visited.insert((start.clone(), k, what));
        let mut found = 0;
        let mut i = 0;
        while i < states.len() {
            let (func, j, what) = (states[i].0.clone(), states[i].1, states[i].2.clone());
            let path = index
                .file_of(&func)
                .expect("states are summarized functions");
            let file = &index.files[path];
            if file
                .local_functions
                .get(&func)
                .is_some_and(|local| changes(local, j, &what))
            {
                found = i;
                break;
            }
            for s in file
                .call_sites
                .iter()
                .filter(|s| s.caller == func && s.role == SiteRole::Call)
            {
                let callees = s
                    .project_callees()
                    .iter()
                    .filter_map(|c| Some((c, index.summary(&c.callee)?)));
                for (c, cs) in callees {
                    for (j2, o) in c.params.iter().enumerate().take(64) {
                        let j2 = u8::try_from(j2).expect("at most 64 parameters");
                        let as_itself = o.param == Some((j, None));
                        // The same change of the argument itself, the same slot of it, or the
                        // argument itself when it is that slot (`helper(p.a)`).
                        let next = match &what {
                            Some(_) if as_itself && changes(cs, j2, &what) => Some(what.clone()),
                            None => (as_itself && changes(cs, j2, &None)).then_some(None),
                            Some(slot) => (o.param_slot.as_ref() == Some(&(j, slot.clone()))
                                && changes(cs, j2, &None))
                            .then_some(None),
                        };
                        if let Some(next) = next
                            && visited.insert((c.callee.clone(), j2, next.clone()))
                        {
                            let into = hop(&func, &c.callee, s.span, path);
                            states.push((c.callee.clone(), j2, next, Some(i), into));
                        }
                    }
                }
            }
            i += 1;
        }
        let mut chain = Vec::new();
        let mut at = Some(found);
        while let Some(i) = at {
            chain.push(states[i].4.clone());
            at = states[i].3;
        }
        chain.reverse();
        chain
    }

    /// One report per `(rule, function, span, target)`.
    #[allow(clippy::too_many_arguments)]
    fn emit(
        &mut self,
        out: &mut Vec<Diagnostic>,
        code: &'static str,
        unit: &ModuleUnit<'_>,
        func: &FuncKey,
        span: Span,
        target: String,
        message: String,
        chain: &[CallHop],
    ) {
        if self.seen.insert((code, func.clone(), span, target.clone())) {
            let qualname = &func.qualname;
            self.cx
                .emit(out, code, unit, span, qualname, target, message, chain);
        }
    }
}

/// What the call chains of the reports read of the functions of one file besides their final
/// summaries ([`Rule::chain`] walks the functions below the call that hands an object over, in
/// any file): per function, the parameters its own body changes in place, the slots of them it
/// changes in place, and its project calls without positions (the language server re-checks the
/// files whose diagnostics show a call site that moved).
pub type ChainFacts =
    BTreeMap<FuncKey, (BTreeSet<u8>, BTreeMap<u8, BTreeSet<Slot>>, Vec<CallSite>)>;

/// The rules whose reports carry the call chains that read [`ChainFacts`]. Only the chains
/// (related locations) depend on them: whether a call is reported does not.
pub const CHAIN_RULES: &[&str] = &["MUT108", "MUT110"];

/// [`ChainFacts`] of the functions defined in `path` that a chain can pass through: those whose
/// final summary changes a parameter, or a slot of one, in place. A chain ends at the first
/// function whose own body makes the change and walks the calls of the functions before it, so
/// an edit that moves the change from one function to another, or changes those calls, changes
/// the chains of the reports above them while every final summary stays the same.
pub fn chain_facts(index: &ProjectIndex, path: &Path) -> ChainFacts {
    let mut out = ChainFacts::new();
    let Some(file) = index.files.get(path) else {
        return out;
    };
    for (key, local) in &file.local_functions {
        let walked = index.file_of(key) == Some(path)
            && index.summary(key).is_some_and(|s| {
                s.mutated_param_paths.values().any(|p| p.contains(""))
                    || s.inplace_param_slots
                        .values()
                        .any(|slots| !slots.is_empty())
            });
        if walked {
            let own = local
                .mutated_param_paths
                .iter()
                .filter(|(_, paths)| paths.contains(""))
                .map(|(j, _)| *j)
                .collect();
            let slots = local.inplace_param_slots.clone();
            out.insert(key.clone(), (own, slots, Vec::new()));
        }
    }
    for site in file
        .call_sites
        .iter()
        .filter(|s| s.role == SiteRole::Call && !s.project_callees().is_empty())
    {
        if let Some((_, _, calls)) = out.get_mut(&site.caller) {
            let mut site = site.clone();
            site.span = Span { start: 0, end: 0 };
            calls.push(site);
        }
    }
    out
}

/// Whether summary `s` changes its parameter `j` itself (`None`) or a slot of it in place.
fn changes(s: &EffectSummary, j: u8, what: &Changed) -> bool {
    match what {
        None => s
            .mutated_param_paths
            .get(&j)
            .is_some_and(|paths| paths.contains("")),
        Some(slot) => s
            .inplace_param_slots
            .get(&j)
            .is_some_and(|slots| slots.contains(slot)),
    }
}

/// Why an in-place change of a read-only attribute matters.
fn why(kind: ReadOnlyKind) -> &'static str {
    match kind {
        ReadOnlyKind::Frozen => {
            "the class promises immutable instances, so they get shared (module-level defaults, caches, the same value handed to several owners), and every holder sees the change and different `==` results"
        }
        ReadOnlyKind::Declared => {
            "the declaration promises that the value is only read once it is set, but every holder of the object sees the change"
        }
    }
}
