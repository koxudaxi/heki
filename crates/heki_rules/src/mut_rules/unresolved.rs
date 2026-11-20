//! `MUT205` (`unresolved-guarded-write`): the strict mode of `write-owners` contracts
//! (`unresolved = "report"`). `MUT201` reports the writes it can tie to a target class; this
//! rule counts the ones it cannot: an attribute named like a target written on an object of
//! unknown class (a parameter no caller types, a loop variable, an untyped call result), and an
//! attribute name computed at run time written on an instance of a target class. Each is one
//! finding at the write, so a baseline ratchets them down as annotations and refactorings make
//! the writes visible to `MUT201`.

use super::{MergedReports, OwnerRule, initializes_itself, runs_inside_allowed};
use crate::{CheckContext, SitesByCaller};
use heki_diagnostics::Diagnostic;
use heki_effect::{Construction, FuncKey, Unresolved};
use rustc_hash::FxHashSet;
use std::collections::VecDeque;

pub(super) fn mut205(cx: &CheckContext<'_>, rules: &[OwnerRule<'_>], out: &mut Vec<Diagnostic>) {
    if !cx.selection.is_enabled("MUT205") {
        return;
    }
    let strict: Vec<&OwnerRule<'_>> = rules.iter().filter(|r| r.report_unresolved).collect();
    if strict.is_empty() {
        return;
    }
    let sites = cx.sites_by_caller();
    let mut reports = MergedReports::default();
    for (key, s) in cx.defined_functions() {
        if s.unresolved_write_sites.is_empty() {
            continue;
        }
        let Some(unit) = cx.index.file_of(key).and_then(|p| cx.unit(p)) else {
            continue;
        };
        for w in &s.unresolved_write_sites {
            for rule in &strict {
                let hits: Vec<_> = rule
                    .targets
                    .iter()
                    .filter(|t| match &w.what {
                        Unresolved::Receiver(attr) => t.matches_attr(attr),
                        Unresolved::Name(class) => t.classes.contains(class),
                    })
                    .collect();
                if hits.is_empty()
                    || rule.allowance.allows(key)
                    || !rule.allowance.in_scope(key)
                    || rule.allow_fresh && w.construction == Construction::Yes
                {
                    continue;
                }
                let family: FxHashSet<String> = match &w.what {
                    Unresolved::Name(_) => hits
                        .iter()
                        .flat_map(|t| t.classes.iter().cloned())
                        .collect(),
                    Unresolved::Receiver(_) => FxHashSet::default(),
                };
                // A class's own initialization (`setattr(self, name, v)` in `__init__`), and a
                // function that only runs inside the owner's operations, as for `MUT201`.
                if initializes_itself(cx, key, &family, w.obj_params)
                    || runs_inside_allowed(cx, &sites, &rule.allowance, &family, key, w.obj_params)
                {
                    continue;
                }
                // A parameter every caller path binds to an object of a known class is tied to
                // it there (`MUT201` reports the typed callers).
                if matches!(w.what, Unresolved::Receiver(_))
                    && w.obj_params != 0
                    && !untyped_above(cx, &sites, rule, key, w.obj_params)
                {
                    continue;
                }
                let target = match &w.what {
                    Unresolved::Receiver(attr) => attr.clone(),
                    Unresolved::Name(class) => {
                        cx.decls.class_qualname(class).unwrap_or(class).to_owned()
                    }
                };
                let guarded = listed(hits.iter().map(|t| t.text.as_str()));
                reports.add(
                    unit,
                    key,
                    w.span,
                    &target,
                    (key.fqn(), w.span),
                    rule.note,
                    || {
                        let what = match &w.what {
                            Unresolved::Receiver(attr) => format!(
                                "writes `{attr}` ({}) on an object of unknown class",
                                w.kind.as_str()
                            ),
                            Unresolved::Name(_) => format!(
                                "writes an attribute named at run time ({}) on `{target}`",
                                w.kind.as_str()
                            ),
                        };
                        format!(
                            "`{}` {what} {}: heki cannot tell whether it writes {guarded}",
                            key.qualname,
                            rule.allowance.where_text(
                                "write owners",
                                "forbidden writers",
                                rule.name
                            )
                        )
                    },
                    Vec::new,
                );
            }
        }
    }
    reports.emit(cx, "MUT205", out);
}

/// `a`, `a` and `b`, `a`, `b` and 3 more.
fn listed<'t>(texts: impl Iterator<Item = &'t str>) -> String {
    let all: Vec<String> = texts.map(|t| format!("`{t}`")).collect();
    match all.len() {
        1 => all[0].clone(),
        2 => format!("{} or {}", all[0], all[1]),
        n => format!("{}, {}, or {} more", all[0], all[1], n - 2),
    }
}

/// Whether the object of `func`'s parameters `obj` comes from an object of unknown class on
/// some path of project callers (outside the contract's allowed functions): a caller passes one
/// that is not its own parameter, or the path ends in a function without project callers.
/// Arguments of a project class end a path: the write is tied to that class there.
fn untyped_above(
    cx: &CheckContext<'_>,
    sites: &SitesByCaller<'_>,
    rule: &OwnerRule<'_>,
    func: &FuncKey,
    obj: u64,
) -> bool {
    let mut queue = VecDeque::from([(func.clone(), obj)]);
    let mut visited: FxHashSet<(FuncKey, u64)> = FxHashSet::default();
    visited.insert((func.clone(), obj));
    while let Some((callee, obj)) = queue.pop_front() {
        let Some(callers) = cx
            .index
            .reverse_call_graph
            .get(&callee)
            .filter(|c| !c.is_empty())
        else {
            return true;
        };
        let mut callers: Vec<&FuncKey> = callers.iter().collect();
        callers.sort();
        for caller in callers {
            if rule.allowance.allows(caller) {
                continue;
            }
            for (_, site) in sites.get(caller).into_iter().flatten() {
                for bc in site
                    .project_callees()
                    .iter()
                    .filter(|bc| bc.callee == callee)
                {
                    for k in (0..64).filter(|k| obj & (1u64 << k) != 0) {
                        // No origin (trailing empty origins are dropped, a parameter may be left
                        // to its default): an object of unknown class.
                        let Some(o) = bc.params.get(k) else {
                            return true;
                        };
                        // A protocol whose implementations heki cannot list: no caller above
                        // ties the object to a class.
                        if o.of_unlisted_protocol() {
                            return true;
                        }
                        if o.typed_instance.is_some() {
                            continue;
                        }
                        let Some((p, None)) = o.param else {
                            return true;
                        };
                        let up = 1u64 << p;
                        if visited.insert((caller.clone(), up)) {
                            queue.push_back((caller.clone(), up));
                        }
                    }
                }
            }
        }
    }
    false
}
