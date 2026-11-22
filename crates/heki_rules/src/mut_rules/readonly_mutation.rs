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

use super::{Allowance, facts_run_at, initializes_itself, runs_inside_allowed};
use crate::premises;
use crate::{CheckContext, ModuleUnit, SitesByCaller};
use heki_ast::Span;
use heki_diagnostics::{Diagnostic, OpenPremise, OpenRecord, RelatedLocation};
use heki_effect::{
    BoundCallee, CallHop, CallSite, CallsByCaller, EffectSummary, FuncKey, ParamChange,
    ProjectIndex, ReadOnlyAttr, ReadOnlyKind, SiteRole, Slot, WriteKind,
};
use rustc_hash::FxHashSet;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// What a function changes in place: its parameter itself (`None`) or a slot of it.
type Changed = ParamChange;

/// Write kinds that change an attribute's object in place, which `MUT108` reports and `MUT112`
/// follows. Rebinding and deletion belong to type checkers; `setattr` / `object.__setattr__` and
/// `__dict__` writes are deliberate bypasses.
pub(super) const IN_PLACE: &[WriteKind] = &[
    WriteKind::ContainerMut,
    WriteKind::AliasContainerMut,
    WriteKind::IndirectHelper,
];

/// The help of every `MUT108` report.
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
        calls: cx.index.calls_by_caller(),
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
            .map(|(k, s)| (k, &**s))
            .collect();
        funcs.sort_by(|a, b| a.0.cmp(b.0));
        for (key, s) in funcs {
            rule.own_changes(unit, key, s, out);
        }
        // Without `MUT108`, only the calls of the functions with a read-only parameter matter.
        // A call hands its arguments over; they are changed where the callee's facts run
        // (`facts_run_at`): at the call for a plain function, where the generator it returns is
        // iterated for a generator function (never, when it is discarded).
        let readonly: FxHashSet<&str> = cx
            .decls
            .module(&file.module_name)
            .into_iter()
            .flat_map(|m| &m.functions)
            .filter(|(_, d)| params && d.readonly_params != 0)
            .map(|(q, _)| q.as_str())
            .collect();
        for site in file.call_sites.iter().filter(|s| {
            (fields || readonly.contains(s.caller.qualname.as_str()))
                && cx.index.file_of(&s.caller) == Some(path)
        }) {
            rule.handed_over(unit, path, site, out);
        }
    }
}

/// A `MUT110` report of one call, emitted once every callee of the call is read.
struct HandedParam {
    name: String,
    message: String,
    help: String,
    chain: Vec<CallHop>,
    /// The line that changes the caller's object, which makes the report exact.
    witness: Option<RelatedLocation>,
    /// Why there is no such line, when the run records open premises.
    open: Vec<OpenPremise>,
}

/// One project callee of a call site, with the facts of it that run there.
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
    /// The calls the reports' chains walk, indexed once for every report.
    calls: CallsByCaller<'a>,
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
                "`{}` changes {what} in place ({})",
                key.qualname,
                w.kind.as_str()
            );
            self.emit(out, "MUT108", unit, key, w.span, target, message, FIX, &[]);
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
        let callees = site.project_callees().iter().filter_map(|bc| {
            let summary = cx.index.summary(&bc.callee)?;
            Some((bc, facts_run_at(cx.index, site, summary)?))
        });
        let mut handed: Vec<HandedParam> = Vec::new();
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
                let itself = callee.changes_param(k, &None);
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
                    // A helper whose parameter is annotated, read-only or mutable, is checked on
                    // its own: a change reached only through one is not this call's report.
                    let unannotated = |f: &FuncKey, j: u8| {
                        cx.decls.function_decl(f).is_none_or(|d| {
                            (d.readonly_params | d.mutable_params) & (1u64 << j) == 0
                        })
                    };
                    let (chain, (changer, j), found) = self.chain(&call, k, None, &unannotated);
                    if !found && self.chain(&call, k, None, &|_, _| true).2 {
                        continue;
                    }
                    // The line where the function at the end of the chain changes the object its
                    // parameter `j` holds, which is the caller's parameter whenever it runs. The
                    // chain does not follow a change made where a returned generator is
                    // iterated: without its line the claim is open, and the report heuristic.
                    let witness = self.inplace_site(&changer, j).map(|(path, span)| {
                        let what =
                            format!("`{}` changes the object in place here", changer.qualname);
                        cx.related_at(path, span, what)
                    });
                    // "Read-only parameter" and "Passed as itself" hold for every report.
                    let open = if cx.open_premises && witness.is_none() {
                        let (cause, about) = if found {
                            (
                                &premises::NOT_A_BUILTIN_CONTAINER_CHANGE,
                                Some(changer.fqn()),
                            )
                        } else {
                            (&premises::NO_CHANGING_BODY, None)
                        };
                        vec![cause.open_at(None, about.map(|f| format!("`{f}`")))]
                    } else {
                        Vec::new()
                    };
                    let (caller, callee) = (&site.caller.qualname, bc.callee.fqn());
                    let message = match &witness {
                        Some(w) => format!(
                            "`{caller}` passes its read-only parameter `{name}` to `{callee}`; whenever line {} of `{}` runs, it changes the caller's object in place",
                            w.line, changer.qualname,
                        ),
                        None => format!(
                            "`{caller}` passes its read-only parameter `{name}` to `{callee}`, which changes it in place"
                        ),
                    };
                    let help = format!(
                        "pass a copy (`list({name})` / `dict({name})`), or annotate the helper's parameter with a mutable type"
                    );
                    handed.push(HandedParam {
                        name: name.clone(),
                        message,
                        help,
                        chain,
                        witness,
                        open,
                    });
                }
            }
        }
        // One report per parameter: the one naming a line when any callee has one.
        handed.sort_by_key(|h| h.witness.is_none());
        for h in handed {
            let (caller, span) = (&site.caller, site.span);
            let target = h.name;
            if let Some(d) = self.emit(
                out, "MUT110", unit, caller, span, target, h.message, &h.help, &h.chain,
            ) {
                d.certainty = premises::tier("MUT110", h.witness.is_some()).at_most(d.certainty);
                if let Some(w) = h.witness {
                    d.related.push(w);
                }
                d.open_records = h.open.into_iter().map(OpenRecord::from).collect();
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
            "`{}` changes {named} in place via `{}` (indirect_helper)",
            call.site.caller.qualname,
            call.bc.callee.fqn()
        );
        let (chain, _, _) = self.chain(call, k, what, &|_, _| true);
        let (unit, caller, span) = (call.unit, &call.site.caller, call.site.span);
        self.emit(
            out, "MUT108", unit, caller, span, target, message, FIX, &chain,
        );
    }

    /// The first of the function's own in-place changes of its parameter `j` itself
    /// (`EffectSummary::param_inplace_sites`), with the function's file.
    fn inplace_site(&self, func: &FuncKey, j: u8) -> Option<(&Path, Span)> {
        let index = self.cx.index;
        let path = index.file_of(func)?;
        let local = index.files.get(path)?.local_functions.get(func)?;
        let first = local.param_inplace_sites.get(&j)?.first()?;
        Some((path, *first))
    }

    /// The hop of the call into its callee (whose parameter `k` changes `what` in place), then
    /// the shortest chain of calls down to the function whose own body makes the change, entering
    /// only the parameters `enters` admits ([`ProjectIndex::param_change_chain_entering`]), and
    /// that function with its parameter (the callee and `k` when no body on the way makes it);
    /// whether such a body was found.
    fn chain(
        &mut self,
        call: &Call<'_>,
        k: u8,
        what: Changed,
        enters: &dyn Fn(&FuncKey, u8) -> bool,
    ) -> (Vec<CallHop>, (FuncKey, u8), bool) {
        let into = CallHop {
            caller: call.site.caller.clone(),
            callee: call.bc.callee.fqn(),
            span: call.site.span,
            path: call.path.to_path_buf(),
        };
        let start = &call.bc.callee;
        let below =
            self.cx
                .index
                .param_change_chain_entering(&mut self.calls, start, k, &what, enters);
        let found = below.changed_in.is_some();
        let end = (
            below.changed_in.unwrap_or_else(|| start.clone()),
            below.param.unwrap_or(k),
        );
        (
            std::iter::once(into).chain(below.hops).collect(),
            end,
            found,
        )
    }

    /// One report per `(rule, function, span, target)`.
    #[allow(clippy::too_many_arguments)]
    fn emit<'o>(
        &mut self,
        out: &'o mut Vec<Diagnostic>,
        code: &'static str,
        unit: &ModuleUnit<'_>,
        func: &FuncKey,
        span: Span,
        target: String,
        message: String,
        help: &str,
        chain: &[CallHop],
    ) -> Option<&'o mut Diagnostic> {
        let first = self.seen.insert((code, func.clone(), span, target.clone()));
        let qualname = &func.qualname;
        let help = Some(help.to_owned());
        first
            .then(|| {
                self.cx.emit(
                    out, code, unit, span, qualname, target, message, help, chain,
                )
            })
            .flatten()
    }
}

/// What the call chains of the reports read of the functions of one file besides their final
/// summaries ([`ProjectIndex::param_change_chain`] walks the functions below the call that hands
/// an object over, in any file): per function, the parameters its own body changes in place, the slots of them it
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
            calls.push(site.unplaced());
        }
    }
    out
}
