//! The strict modes of the ownership contracts (`unresolved = "report"`): `MUT205`
//! (`unresolved-guarded-write`, `write-owners`), `MUT206` (`unresolved-guarded-read`,
//! `read-owners`), and `MUT207` (`unresolved-guarded-call`, `call-owners`). `MUT201`, `MUT203`,
//! and `MUT204` report the accesses they can tie to a target class; these rules count the ones
//! they cannot: an attribute or method named like a target, accessed on an object of unknown
//! class (a parameter no caller types, a loop variable, an untyped call result, `Any`, a
//! protocol whose implementations heki cannot list), and an attribute name computed at run time
//! written on an instance of a target class or on an object of unknown class. Each is one
//! finding at the access, so a baseline
//! ratchets them down as annotations and refactorings make the accesses visible to the typed
//! rules.

use super::call_owners::CallOwnerRule;
use super::{
    Allowance, ContractTarget, MergedReports, OwnerRule, ReadOwnerRule, initializes_itself,
    runs_inside_allowed,
};
use crate::{CheckContext, SitesByCaller};
use heki_ast::Span;
use heki_diagnostics::Diagnostic;
use heki_effect::{Access, Construction, FuncKey, Unresolved};
use rustc_hash::{FxHashMap, FxHashSet};
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
    let mut classified: Classified<'_, &Unresolved> = Classified::default();
    let mut reports = MergedReports::default();
    for (key, s) in cx.defined_functions() {
        for w in &s.unresolved_write_sites {
            for (r, rule) in strict.iter().enumerate() {
                let hits: Vec<_> = rule
                    .targets
                    .iter()
                    .filter(|t| match &w.what {
                        Unresolved::Receiver(attr) => t.matches_attr(attr),
                        Unresolved::Name(class) => t.classes.contains(class),
                        Unresolved::Anything => true,
                    })
                    .collect();
                if hits.is_empty() || rule.allow_fresh && w.construction == Construction::Yes {
                    continue;
                }
                let writes_target = |class: &str| hits.iter().any(|t| t.classes.contains(class));
                // A computed name on a target class may be a class's own initialization
                // (`setattr(self, name, v)` in `__init__`), as for `MUT201`; an attribute on an
                // object of unknown class is tied to the classes its callers bind, which
                // `MUT201` reports at those callers. A computed name on an object of unknown
                // class is tied to the other classes its callers bind; `MUT201` sees no name at
                // a caller binding a target class, so such a caller leaves the write to this
                // rule.
                let (family, callers) = match &w.what {
                    Unresolved::Name(_) => (
                        hits.iter()
                            .flat_map(|t| t.classes.iter().cloned())
                            .collect(),
                        Callers::Ignored,
                    ),
                    Unresolved::Receiver(_) => (FxHashSet::default(), Callers::TieEveryClass),
                    Unresolved::Anything => (
                        FxHashSet::default(),
                        Callers::LeaveTargetClasses(&writes_target),
                    ),
                };
                let access = Unseen {
                    func: key,
                    obj: w.obj_params,
                };
                let counts = *classified
                    .entry((r, key, w.obj_params, &w.what))
                    .or_insert_with(|| {
                        access.counts(cx, &sites, &rule.allowance, &family, callers)
                    });
                if !counts {
                    continue;
                }
                let target = match &w.what {
                    Unresolved::Receiver(attr) => attr.clone(),
                    Unresolved::Name(class) => {
                        cx.decls.class_qualname(class).unwrap_or(class).to_owned()
                    }
                    Unresolved::Anything => "*".to_owned(),
                };
                let guarded = listed(hits.iter().map(|t| t.text.as_str()));
                let what = match &w.what {
                    Unresolved::Receiver(attr) => format!(
                        "writes `{attr}` ({}) on an object of unknown class",
                        w.kind.as_str()
                    ),
                    Unresolved::Name(_) => format!(
                        "writes an attribute named at run time ({}) on `{target}`",
                        w.kind.as_str()
                    ),
                    Unresolved::Anything => format!(
                        "writes an attribute named at run time ({}) on an object of unknown class",
                        w.kind.as_str()
                    ),
                };
                let judged = rule.allowance.judged(cx.decls, key);
                add(
                    cx,
                    &mut reports,
                    &judged,
                    w.span,
                    &target,
                    rule.note,
                    || {
                        format!(
                            "`{}` {what} {}: heki cannot tell whether it writes {guarded}",
                            judged.qualname,
                            rule.allowance.where_text(
                                "write owners",
                                "forbidden writers",
                                rule.name
                            )
                        )
                    },
                );
            }
        }
    }
    reports.emit(cx, "MUT205", out);
}

/// The targets of a rule that an attribute name matches, and the classes they cover.
type MatchedTargets<'r> = (Vec<&'r ContractTarget>, FxHashSet<&'r str>);

/// Whether an access is a finding of a strict rule ([`Unseen::counts`]), by the rule, the
/// function, the parameters its object may be, and what is accessed (`N`: the name read or
/// called, the attribute or class written, which decides the targets it matches): the accesses
/// of one name in one function share it, and each would walk the same callers.
type Classified<'k, N> = FxHashMap<(usize, &'k FuncKey, u64, N), bool>;

/// `MUT206`: reads of an attribute named like a target of a strict `read-owners` contract on
/// an object of unknown class.
pub(super) fn mut206(
    cx: &CheckContext<'_>,
    rules: &[ReadOwnerRule<'_>],
    out: &mut Vec<Diagnostic>,
) {
    let strict: Vec<&ReadOwnerRule<'_>> = rules.iter().filter(|r| r.report_unresolved).collect();
    if !cx.selection.is_enabled("MUT206") || strict.is_empty() {
        return;
    }
    let sites = cx.sites_by_caller();
    // An object of unknown class is no class's own `self`.
    let no_class = FxHashSet::default();
    // The targets of each rule an attribute name matches, and the classes they cover, found once
    // per rule and name rather than once per read.
    let mut matched: FxHashMap<(usize, &str), MatchedTargets<'_>> = FxHashMap::default();
    let mut classified: Classified<'_, &str> = Classified::default();
    let mut reports = MergedReports::default();
    for (key, s) in cx.defined_functions() {
        for a in &s.unresolved_access_sites {
            let Access::Read(kind) = a.access else {
                continue;
            };
            for (r, rule) in strict.iter().enumerate() {
                let (hits, family) = matched.entry((r, a.name.as_str())).or_insert_with(|| {
                    let hits: Vec<&ContractTarget> = rule
                        .targets
                        .iter()
                        .filter(|t| t.matches_attr(&a.name))
                        .collect();
                    let family = hits
                        .iter()
                        .flat_map(|t| t.classes.iter().map(String::as_str))
                        .collect();
                    (hits, family)
                });
                let reads_target = |class: &str| family.contains(class);
                let access = Unseen {
                    func: key,
                    obj: a.obj_params,
                };
                let callers = Callers::LeaveTargetClasses(&reads_target);
                if hits.is_empty()
                    || !*classified
                        .entry((r, key, a.obj_params, a.name.as_str()))
                        .or_insert_with(|| {
                            access.counts(cx, &sites, &rule.allowance, &no_class, callers)
                        })
                {
                    continue;
                }
                let guarded = listed(hits.iter().map(|t| t.text.as_str()));
                let judged = rule.allowance.judged(cx.decls, key);
                add(
                    cx,
                    &mut reports,
                    &judged,
                    a.span,
                    &a.name,
                    rule.note,
                    || {
                        format!(
                            "`{}` reads `{}` ({}) on an object of unknown class {}: heki cannot tell whether it reads {guarded}",
                            judged.qualname,
                            a.name,
                            kind.as_str(),
                            rule.allowance.where_text(
                                "read owners",
                                "forbidden readers",
                                rule.name
                            )
                        )
                    },
                );
            }
        }
    }
    reports.emit(cx, "MUT206", out);
}

/// `MUT207`: calls of a method named like a target of a strict `call-owners` contract on an
/// object of unknown class.
pub(super) fn mut207(
    cx: &CheckContext<'_>,
    rules: &[CallOwnerRule<'_>],
    out: &mut Vec<Diagnostic>,
) {
    let strict: Vec<&CallOwnerRule<'_>> = rules.iter().filter(|r| r.report_unresolved).collect();
    if !cx.selection.is_enabled("MUT207") || strict.is_empty() {
        return;
    }
    let sites = cx.sites_by_caller();
    // An object of unknown class is no class's own `self`.
    let no_class = FxHashSet::default();
    let mut classified: Classified<'_, &str> = Classified::default();
    let mut reports = MergedReports::default();
    for (key, s) in cx.defined_functions() {
        for a in s
            .unresolved_access_sites
            .iter()
            .filter(|a| a.access == Access::Call)
        {
            for (r, rule) in strict.iter().enumerate() {
                let hits: Vec<usize> = (0..rule.targets.patterns.len())
                    .filter(|&i| rule.targets.patterns[i].name.matches(&a.name))
                    .collect();
                let calls_target = |class: &str| {
                    hits.iter()
                        .any(|&i| rule.targets.calls_on(cx, i, class, &a.name))
                };
                let access = Unseen {
                    func: key,
                    obj: a.obj_params,
                };
                let callers = Callers::LeaveTargetClasses(&calls_target);
                if hits.is_empty()
                    || !*classified
                        .entry((r, key, a.obj_params, a.name.as_str()))
                        .or_insert_with(|| {
                            access.counts(cx, &sites, &rule.allowance, &no_class, callers)
                        })
                {
                    continue;
                }
                let guarded = listed(hits.iter().map(|&i| rule.targets.patterns[i].text.as_str()));
                let judged = rule.allowance.judged(cx.decls, key);
                add(
                    cx,
                    &mut reports,
                    &judged,
                    a.span,
                    &a.name,
                    rule.note,
                    || {
                        format!(
                            "`{}` calls `{}` on an object of unknown class {}: heki cannot tell whether it calls {guarded}",
                            judged.qualname,
                            a.name,
                            rule.allowance.where_text(
                                "call owners",
                                "forbidden callers",
                                rule.name
                            )
                        )
                    },
                );
            }
        }
    }
    reports.emit(cx, "MUT207", out);
}

/// One report at the access itself, its target the name accessed (or the class written).
fn add<'u>(
    cx: &CheckContext<'u>,
    reports: &mut MergedReports<'u>,
    key: &FuncKey,
    span: Span,
    target: &str,
    note: Option<&str>,
    message: impl FnOnce() -> String,
) {
    let Some(unit) = cx.index.file_of(key).and_then(|p| cx.unit(p)) else {
        return;
    };
    reports.add(
        unit,
        key,
        span,
        (target, target),
        (key.fqn(), span),
        note,
        message,
        Vec::new,
    );
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

/// What the callers binding a parameter tell about an access to its object.
#[derive(Clone, Copy)]
enum Callers<'f> {
    /// Nothing: the access is the function's own.
    Ignored,
    /// A write: an object of a known class ties it to that class, and `MUT201` reports the
    /// caller.
    TieEveryClass,
    /// A read or a call, which `MUT203` / `MUT204` do not follow into a parameter heki cannot
    /// type: an object of a class this accepts (one whose access may be of a target) leaves
    /// the access to the strict rule, one of another known class ties it.
    LeaveTargetClasses(&'f dyn Fn(&str) -> bool),
}

/// An access heki cannot tie to a class: in `func`, to the object of its parameters `obj`
/// (none: not a parameter on every path).
struct Unseen<'k> {
    func: &'k FuncKey,
    obj: u64,
}

impl Unseen<'_> {
    /// Whether the access is a finding of the strict rule, under `MUT201`'s rules for what is
    /// allowed: allowed functions and exempt methods, `forbidden-*`, a class's own
    /// initialization (of `family`), a function that only runs inside allowed functions (not
    /// with `attribution = "lexical"`, which judges the function holding the access), and what
    /// the callers bind to the object.
    fn counts(
        &self,
        cx: &CheckContext<'_>,
        sites: &SitesByCaller<'_>,
        allowance: &Allowance,
        family: &FxHashSet<String>,
        callers: Callers<'_>,
    ) -> bool {
        let judged = allowance.judged(cx.decls, self.func);
        // The parameters of a scope a function holds are none of the function's.
        let obj = if *judged == *self.func { self.obj } else { 0 };
        if allowance.allows(&judged)
            || !allowance.in_scope(&judged)
            || initializes_itself(cx, &judged, family, obj)
            || !allowance.is_lexical()
                && runs_inside_allowed(cx, sites, allowance, family, self.func, self.obj)
        {
            return false;
        }
        let leave = match callers {
            Callers::Ignored => return true,
            Callers::TieEveryClass => None,
            Callers::LeaveTargetClasses(accepts) => Some(accepts),
        };
        self.obj == 0 || unknown_above(cx, sites, allowance, self.func, self.obj, leave)
    }
}

/// Whether the object of `func`'s parameters `obj` comes, on some path of project callers
/// (outside the contract's allowed functions, unless it judges lexically), from an object heki
/// cannot rule out: one of
/// unknown class (a caller passes one that is not its own parameter, typed with a protocol
/// whose implementations heki cannot list, or the path ends in a function without project
/// callers), or an instance of a class `leave` accepts. Arguments of other project classes end
/// a path.
fn unknown_above(
    cx: &CheckContext<'_>,
    sites: &SitesByCaller<'_>,
    allowance: &Allowance,
    func: &FuncKey,
    obj: u64,
    leave: Option<&dyn Fn(&str) -> bool>,
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
            // A path through an allowed caller runs inside it; judged lexically, the access stays
            // the function's own, and the caller only tells the object's class.
            if !allowance.is_lexical() && allowance.allows(caller) {
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
                        if o.of_unlisted_protocol() {
                            return true;
                        }
                        if o.typed_instance.is_some() {
                            if leave.is_some_and(|l| o.instance_classes().any(|c| l(c))) {
                                return true;
                            }
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
