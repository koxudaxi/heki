//! `MUT113` (`iterated-container-mutation`, opt-in): a dict, set, or deque of a method's own
//! instance changed while the method iterates it, by code outside the loop. The next step of the
//! iteration raises `RuntimeError` ([`heki_effect::iterations`]).
//!
//! Two kinds of code run while a method iterates:
//! - code running while a generator is suspended inside the iteration (at a `yield` from which
//!   the loop goes on, or inside a `yield from`);
//! - a function the loop's body calls (a change written in the body itself is the linters'
//!   loop-iterator-mutation check).
//!
//! What runs while a generator is suspended depends on where its generator object goes. A
//! private generator runs only where the project iterates it: a loop's body runs between its
//! steps (its calls on the generator's instance count), a consuming call (`list(...)`) runs
//! nothing, and a generator suspending inside such a loop is suspended with it. Once its object
//! outlives one use of it (stored, handed to code heki does not follow, or stepped by `next()` or
//! a loop that leaves early and used again), any method of the class family can run in between,
//! apart from initialization (`MUT101`'s two-phase initialization methods run before any
//! iteration) and private methods (they run for the methods calling them, which count). A public
//! generator is an iteration the class offers its callers, as `dict.items()` is: its public
//! methods changing the container are the rest of that interface, and changing it while iterating
//! is the caller's mistake, as for a builtin container. Only the changes made by reads count
//! there: property getters and the dunder methods of the read protocols (`__getitem__`,
//! `__len__`, `__contains__`, ...). The project's own uses of a public generator count as for a
//! private one.

use super::{Checked, Method, filter1_init, for_each_class_family, is_private, local_summary};
use crate::{CheckContext, ModuleUnit};
use heki_ast::Span;
use heki_diagnostics::{Diagnostic, RelatedLocation};
use heki_effect::iterations::changed_at;
use heki_effect::{CallSite, ContainerIteration, FileSummary, FuncKey, ProjectIndex, SiteRole};
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::Path;

/// Dunder methods of the read protocols: reading the object runs them.
const READ_DUNDERS: &[&str] = &[
    "__getitem__",
    "__missing__",
    "__len__",
    "__length_hint__",
    "__contains__",
    "__bool__",
    "__iter__",
    "__reversed__",
    "__repr__",
    "__str__",
    "__format__",
    "__hash__",
    "__eq__",
    "__ne__",
    "__lt__",
    "__le__",
    "__gt__",
    "__ge__",
    "__getattr__",
    "__getattribute__",
    "__copy__",
    "__deepcopy__",
    "__getstate__",
    "__reduce__",
    "__reduce_ex__",
];

/// Call sites of one module by `(caller qualname, span)`.
type SitesBySpan<'a> = FxHashMap<(String, Span), Vec<&'a CallSite>>;

pub(super) fn run(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.selection.is_enabled("MUT113") {
        return;
    }
    let mut sites: FxHashMap<&Path, SitesBySpan<'_>> = FxHashMap::default();
    let mut reported: FxHashSet<(FuncKey, Span, String)> = FxHashSet::default();
    for_each_class_family(cx, |methods, family, init_called| {
        let mut visited: FxHashSet<&FuncKey> = FxHashSet::default();
        // A checked file has one method per definition, and its summary covers all of them.
        let checked = methods.iter().filter_map(|g| {
            let Checked { unit, .. } = g.checked?;
            let first = visited.insert(&g.key) && cx.enabled("MUT113", unit);
            Some((g, unit, local_summary(cx.index, g).filter(|_| first)?))
        });
        for (g, unit, local) in checked {
            let mut exposure: Option<Exposure<'_>> = None;
            for it in &local.iteration_facts().container_iterations {
                if it.between.suspends && reported.insert((g.key.clone(), it.span, it.attr.clone()))
                {
                    let exposure = exposure.get_or_insert_with(|| Exposure::of(cx, &g.key));
                    let changing = changing_methods(cx, g, it, family, init_called, exposure);
                    report_suspended(cx, unit, g, it, &changing, out);
                }
                if it.between.calls.is_empty() {
                    continue;
                }
                let by_span = sites
                    .entry(unit.input.path.as_path())
                    .or_insert_with(|| cx.sites_by_span(unit));
                for span in &it.between.calls {
                    if reported.insert((g.key.clone(), *span, it.attr.clone())) {
                        report_called(cx, unit, g, it, *span, by_span, out);
                    }
                }
            }
        }
    });
}

/// What can run while a generator method is suspended, from where its generator objects go.
#[derive(Default)]
struct Exposure<'a> {
    /// A generator object outlives one of its caller's uses (`CallSite::result_held`): any
    /// method of the class family can run.
    held: bool,
    /// Callers outside the project can iterate it: it is public, or a public function hands it on
    /// or iterates it while suspending. The reads of the class family can run.
    offered: bool,
    /// The call sites in the bodies of the project's loops iterating it that run on the
    /// generator's own instance, with the files they are in.
    loop_calls: Vec<(&'a Path, &'a CallSite)>,
}

/// A function the object of a generator method reaches, and which of its parameters holds the
/// generator's instance (`None`: none of them is known to).
type Reached = (FuncKey, Option<u8>);

impl<'a> Exposure<'a> {
    fn of(cx: &CheckContext<'a>, generator: &FuncKey) -> Self {
        let mut out = Self::default();
        out.follow(cx, generator, Some(0), &mut FxHashSet::default());
        out
    }

    /// Add where the object `func` returns (a generator, or one it hands on) goes at each call of
    /// it, by the uses its caller records: returned, iterated (`IterationFacts::result_iterations`;
    /// by a generator suspending inside the iteration, too), and whether it outlives one of these
    /// uses (`CallSite::result_held`). A project function it is passed to is not followed.
    /// `instance` is the parameter of `func` holding the generator's instance.
    fn follow(
        &mut self,
        cx: &CheckContext<'a>,
        func: &FuncKey,
        instance: Option<u8>,
        visited: &mut FxHashSet<Reached>,
    ) {
        if visited.insert((func.clone(), instance)) {
            self.offered |= !is_private(&func.qualname);
            let callers = cx.index.reverse_call_graph.get(func).into_iter().flatten();
            let callers = callers.filter_map(|caller| {
                let (path, file) = cx.index.files.get_key_value(cx.index.file_of(caller)?)?;
                Some((caller, path.as_path(), file))
            });
            for (caller, path, file) in callers {
                self.follow_calls(cx, (func, instance), (caller, path, file), visited);
            }
        }
    }

    /// [`Self::follow`] at the calls of `func` in `caller`, a function of `file` (at `path`). A
    /// loop over a call's result runs its calls on the generator's instance where they bind the
    /// object the call binds to `instance` (`ResultIteration::on_arguments`).
    fn follow_calls(
        &mut self,
        cx: &CheckContext<'a>,
        (func, instance): (&FuncKey, Option<u8>),
        (caller, path, file): (&'a FuncKey, &'a Path, &'a FileSummary),
        visited: &mut FxHashSet<Reached>,
    ) {
        let calling = |s: &&CallSite| {
            s.caller == *caller && s.project_callees().iter().any(|bc| bc.callee == *func)
        };
        let sites: Vec<&CallSite> = file.call_sites.iter().filter(calling).collect();
        let result_iterations = file.local_functions.get(caller).map_or(&[][..], |s| {
            s.iteration_facts().result_iterations.as_slice()
        });
        let mut caller_sites: Option<FxHashMap<Span, Vec<&'a CallSite>>> = None;
        for call in sites.iter().filter(|s| s.role == SiteRole::Call) {
            if hands_on(file, caller, call.span) {
                let in_caller = instance.and_then(|p| bound_parameter(call, func, p));
                self.follow(cx, caller, in_caller, visited);
            }
            self.held |= call.result_held;
            for used in result_iterations.iter().filter(|u| u.call == call.span) {
                let on_instance = |i: usize| {
                    instance.is_some_and(|p| {
                        used.on_arguments
                            .get(i)
                            .is_some_and(|bits| (bits >> p) & 1 == 1)
                    })
                };
                let calls = used.between.calls.iter().enumerate();
                for (_, span) in calls.filter(|(i, _)| on_instance(*i)) {
                    let at = caller_sites.get_or_insert_with(|| sites_by_span(file, caller));
                    let found = at.get(span).into_iter().flatten();
                    self.loop_calls.extend(found.map(|site| (path, *site)));
                }
            }
        }
    }
}

/// The call sites of `caller`, a function of `file`, by span.
fn sites_by_span<'f>(
    file: &'f FileSummary,
    caller: &FuncKey,
) -> FxHashMap<Span, Vec<&'f CallSite>> {
    let mut out: FxHashMap<Span, Vec<&CallSite>> = FxHashMap::default();
    for site in file.call_sites.iter().filter(|s| s.caller == *caller) {
        out.entry(site.span).or_default().push(site);
    }
    out
}

/// Whether `caller`, a function of `file`, hands on the object its call at `span` returns: it
/// returns the object, or iterates it while suspending (a generator delegating to it). Its own
/// callers then run while a generator that object holds is suspended.
fn hands_on(file: &FileSummary, caller: &FuncKey, span: Span) -> bool {
    let returned = file
        .call_sites
        .iter()
        .any(|s| s.caller == *caller && s.role == SiteRole::Return && s.span == span);
    returned
        || file.local_functions.get(caller).is_some_and(|f| {
            f.iteration_facts()
                .result_iterations
                .iter()
                .any(|u| u.call == span && u.between.suspends)
        })
}

/// The generator methods suspending inside an iteration of their own containers whose objects
/// a call of one of `callees` may hand its caller: the callees themselves, and in turn the
/// callees of the calls whose results they hand on ([`hands_on`], the relation
/// [`Exposure::follow`] walks up).
pub(super) fn generators_reached<'a>(
    index: &'a ProjectIndex,
    callees: impl IntoIterator<Item = &'a FuncKey>,
) -> Vec<&'a FuncKey> {
    let mut stack: Vec<&FuncKey> = callees.into_iter().collect();
    let mut seen: FxHashSet<&FuncKey> = FxHashSet::default();
    let mut out = Vec::new();
    while let Some(f) = stack.pop() {
        if !seen.insert(f) {
            continue;
        }
        if suspends(index, f) {
            out.push(f);
        }
        for (file, s) in calls_of(index, f) {
            if s.role == SiteRole::Call && hands_on(file, f, s.span) {
                stack.extend(s.project_callees().iter().map(|bc| &bc.callee));
            }
        }
    }
    out
}

/// The project callees of `func`'s calls.
pub(super) fn callees_of<'i>(index: &'i ProjectIndex, func: &FuncKey) -> Vec<&'i FuncKey> {
    calls_of(index, func)
        .into_iter()
        .flat_map(|(_, s)| s.project_callees())
        .map(|bc| &bc.callee)
        .collect()
}

/// The call sites of `func`, with the summary of its file.
fn calls_of<'i>(index: &'i ProjectIndex, func: &FuncKey) -> Vec<(&'i FileSummary, &'i CallSite)> {
    let file = index.file_of(func).and_then(|p| index.files.get(p));
    file.into_iter()
        .flat_map(|file| {
            let own = file.call_sites.iter().filter(|s| s.caller == *func);
            own.map(move |s| (file, s))
        })
        .collect()
}

/// Whether `func` is a generator suspending inside an iteration of its own containers.
pub(super) fn suspends(index: &ProjectIndex, func: &FuncKey) -> bool {
    index.summary(func).is_some_and(|s| {
        s.iteration_facts()
            .container_iterations
            .iter()
            .any(|it| it.between.suspends)
    })
}

/// The parameter of the calling function that `call` binds, as itself, to parameter `p` of
/// `func`.
fn bound_parameter(call: &CallSite, func: &FuncKey, p: u8) -> Option<u8> {
    let bound = call
        .project_callees()
        .iter()
        .find(|bc| bc.callee == *func)?;
    match bound.params.get(usize::from(p))?.param {
        Some((i, None)) => Some(i),
        _ => None,
    }
}

/// A method changing the container a suspended generator iterates, and where it does.
struct Changing<'a> {
    key: &'a FuncKey,
    place: Option<(&'a Path, Span, String)>,
}

/// The methods of `family` that change the container `it` iterates and can run while the
/// generator `g` is suspended in it (`exposure`): the class's own methods first.
fn changing_methods<'m, 'a: 'm>(
    cx: &CheckContext<'a>,
    g: &Method<'a>,
    it: &ContainerIteration,
    family: &[&'m Method<'a>],
    init_called: &FxHashSet<FuncKey>,
    exposure: &Exposure<'a>,
) -> Vec<Changing<'m>> {
    let breaks = |key: &FuncKey| {
        cx.index
            .summary(key)
            .and_then(|s| s.iteration_facts().changes_self_containers.get(&it.attr))
            .is_some_and(|c| it.container.broken_by(*c))
    };
    let mut out: Vec<Changing<'m>> = Vec::new();
    let mut seen: FxHashSet<&FuncKey> = FxHashSet::default();
    for m in family {
        let counts = exposure.held && !is_private(m.name())
            || (exposure.held || exposure.offered) && reads(cx, m);
        if m.key == g.key || !counts || filter1_init(m, init_called) || !breaks(&m.key) {
            continue;
        }
        if seen.insert(&m.key) {
            let place = change_site(cx, m, it).map(|span| {
                (
                    m.path,
                    span,
                    format!("`{}` changes `self.{}`", m.key.qualname, it.attr),
                )
            });
            out.push(Changing { key: &m.key, place });
        }
    }
    let members: FxHashMap<&FuncKey, &'m Method<'a>> =
        family.iter().map(|m| (&m.key, *m)).collect();
    for &(path, site) in &exposure.loop_calls {
        for bc in site.project_callees() {
            let Some(m) = members.get(&bc.callee) else {
                continue;
            };
            if m.key != g.key && breaks(&m.key) && seen.insert(&m.key) {
                let message = format!(
                    "`{}` calls `{}` while iterating `{}`",
                    site.caller.qualname, m.key.qualname, g.key.qualname
                );
                out.push(Changing {
                    key: &m.key,
                    place: Some((path, site.span, message)),
                });
            }
        }
    }
    let owner = class_of(&g.key);
    out.sort_by_key(|c| {
        let own = c.key.module == g.key.module && class_of(c.key) == owner;
        (!own, c.key.clone())
    });
    out
}

/// A property getter, or a dunder method reading the object: code its callers do not see as a
/// change.
fn reads(cx: &CheckContext<'_>, m: &Method<'_>) -> bool {
    READ_DUNDERS.contains(&m.name())
        || cx
            .decls
            .function_decl(&m.key)
            .is_some_and(|d| d.is_property)
}

/// Where `m` changes the container `it` iterates: its own first change, or else its first call
/// that changes it.
fn change_site(cx: &CheckContext<'_>, m: &Method<'_>, it: &ContainerIteration) -> Option<Span> {
    if let Some(span) = local_summary(cx.index, m)
        .and_then(|s| {
            s.iteration_facts()
                .self_container_change_sites
                .get(&it.attr)
        })
        .and_then(|spans| spans.first())
    {
        return Some(*span);
    }
    cx.index
        .files
        .get(m.path)?
        .call_sites
        .iter()
        .filter(|s| s.caller == m.key)
        .filter(|s| {
            changed_at(cx.index, s)
                .iter()
                .any(|(_, attr, change)| *attr == it.attr && it.container.broken_by(*change))
        })
        .map(|s| s.span)
        .min()
}

/// The owner class of a method, module-relative.
fn class_of(key: &FuncKey) -> &str {
    key.qualname
        .rsplit_once('.')
        .map_or(key.qualname.as_str(), |(owner, _)| owner)
}

fn report_suspended(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    g: &Method<'_>,
    it: &ContainerIteration,
    changing: &[Changing<'_>],
    out: &mut Vec<Diagnostic>,
) {
    let Some(named) = changing.first() else {
        return;
    };
    let attr = &it.attr;
    let related: Vec<RelatedLocation> = changing
        .iter()
        .filter_map(|c| {
            let (path, span, message) = c.place.as_ref()?;
            Some(cx.related_at(path, *span, message.clone()))
        })
        .collect();
    if let Some(d) = cx.emit(
        out,
        "MUT113",
        unit,
        it.span,
        &g.key.qualname,
        format!("{}.{attr}", class_of(&g.key)),
        format!(
            "`{}` suspends at a `yield` while iterating `self.{attr}`, which `{}` changes in place, so resuming it after that change raises `RuntimeError`",
            g.key.qualname, named.key.qualname
        ),
        Some(
            "iterate over a snapshot taken before the loop (`list(...)` of what it iterates), or change the container only when no generator iterates it"
                .to_owned(),
        ),
        &[],
    ) {
        d.related = related;
    }
}

fn report_called(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    g: &Method<'_>,
    it: &ContainerIteration,
    span: Span,
    sites: &SitesBySpan<'_>,
    out: &mut Vec<Diagnostic>,
) {
    let attr = &it.attr;
    let mut changing: Vec<&FuncKey> = sites
        .get(&(g.key.qualname.clone(), span))
        .into_iter()
        .flatten()
        .flat_map(|site| changed_at(cx.index, site))
        .filter(|(_, a, change)| a == attr && it.container.broken_by(*change))
        .map(|(callee, _, _)| callee)
        .collect();
    changing.sort();
    changing.dedup();
    let Some(callee) = changing.first() else {
        return;
    };
    let related = cx.related_at(
        &unit.input.path,
        it.span,
        format!("`{}` iterates `self.{attr}` here", g.key.qualname),
    );
    if let Some(d) = cx.emit(
        out,
        "MUT113",
        unit,
        span,
        &g.key.qualname,
        format!("{}.{attr}", class_of(&g.key)),
        format!(
            "`{}` calls `{}`, which changes `self.{attr}` in place while `{}` iterates it, so the loop's next step raises `RuntimeError`",
            g.key.qualname, callee.qualname, g.key.qualname
        ),
        Some(
            "iterate over a snapshot taken before the loop (`list(...)` of what it iterates), or collect the changes and apply them after the loop"
                .to_owned(),
        ),
        &[],
    ) {
        d.related = vec![related];
    }
}
