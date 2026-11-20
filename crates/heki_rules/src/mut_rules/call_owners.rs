//! `MUT204` call ownership (`[[tool.heki.call-owners]]`), and the call-target matcher it shares
//! with the `calls` effect boundary of `EFF201`.
//!
//! Call sites are local facts of the index (`FileSummary::call_sites`); a call in a private
//! helper is reported at its callers by the walk `MUT203` uses (`delegated_reports`), so no
//! summary carries call facts.

use super::{
    Allowance, AllowanceKeys, FunctionMatcher, MergedReports, delegated_reports, project_classes,
};
use crate::{CheckContext, RuleConfigError};
use heki_diagnostics::Diagnostic;
use heki_effect::{CallHop, CallSite, FuncKey, SiteRole};
use rustc_hash::FxHashSet;
use std::collections::BTreeSet;

/// One call target: a project function, a method (with its overrides in subclasses), or a class
/// (constructing it).
pub(crate) struct CallTargetPattern {
    /// The configured text (diagnostic target and baseline key).
    pub text: String,
    /// The functions and methods the text names.
    functions: FunctionMatcher,
    /// Classes the whole text names, plus their subclasses: constructing one calls the target.
    classes: FxHashSet<String>,
    /// `classes` and the method's class family: an initializer of one of them calling the
    /// target on its own `self` initializes itself.
    pub family: FxHashSet<String>,
}

impl CallTargetPattern {
    /// Whether `site` calls the target: it constructs a class of the target's family, or every
    /// project function it may call (each candidate of the class hierarchy) is the target or
    /// an override of it. A construction is matched by its class only: its `__init__` runs as
    /// part of it. Only the call itself calls (`SiteRole::Call`): the other sites of the object
    /// a call returns (where it is iterated, returned, or handed on) call nothing more.
    fn hit(&self, site: &CallSite) -> bool {
        if site.role != SiteRole::Call {
            return false;
        }
        match &site.constructs {
            Some(class) => self.classes.contains(class),
            None => {
                let callees = site.project_callees();
                !callees.is_empty() && callees.iter().all(|bc| self.functions.matches(&bc.callee))
            }
        }
    }
}

/// The targets of one contract that `site` calls, as the groups to report (indices into
/// `targets`): each target every candidate matches, on its own; or, when no single target
/// covers every candidate but each candidate is a target (`A.reset` and `B.reset` both named,
/// `obj: Base` whose `reset` is one of the two), one group of the targets the candidates match.
pub(crate) fn called_targets(targets: &[CallTargetPattern], site: &CallSite) -> Vec<Vec<usize>> {
    let whole: Vec<Vec<usize>> = targets
        .iter()
        .enumerate()
        .filter(|(_, t)| t.hit(site))
        .map(|(i, _)| vec![i])
        .collect();
    let callees = site.project_callees();
    if !whole.is_empty() || site.role != SiteRole::Call || site.constructs.is_some() {
        return whole;
    }
    let mut union = BTreeSet::new();
    for bc in callees {
        let matching: Vec<usize> = targets
            .iter()
            .enumerate()
            .filter(|(_, t)| t.functions.matches(&bc.callee))
            .map(|(i, _)| i)
            .collect();
        if matching.is_empty() {
            return whole;
        }
        union.extend(matching);
    }
    if union.is_empty() {
        return whole;
    }
    vec![union.into_iter().collect()]
}

/// The configured texts of a group of [`called_targets`]: the diagnostic target (`A.reset|B.reset`)
/// and the message's name of what is called (``A.reset` or `B.reset``).
pub(crate) fn group_texts(targets: &[CallTargetPattern], group: &[usize]) -> (String, String) {
    let texts: Vec<&str> = group.iter().map(|&i| targets[i].text.as_str()).collect();
    let named: Vec<String> = texts.iter().map(|t| format!("`{t}`")).collect();
    (texts.join("|"), named.join(" or "))
}

/// Parse call targets (`call-owners.targets`, `effect-boundaries.call-targets`).
pub(crate) fn parse_call_targets(
    cx: &CheckContext<'_>,
    all_classes: &[String],
    texts: &[String],
    context: &'static str,
) -> Result<Vec<CallTargetPattern>, RuleConfigError> {
    let mut out = Vec::new();
    for text in texts {
        let functions = FunctionMatcher::new(cx, all_classes, text, context)?;
        let classes = functions.classes(cx, all_classes);
        let mut family = classes.clone();
        family.extend(functions.method_family().into_iter().flatten().cloned());
        out.push(CallTargetPattern {
            text: text.clone(),
            functions,
            classes,
            family,
        });
    }
    Ok(out)
}

/// The callee shown for `site` in a call chain: the constructed class or the first candidate.
pub(crate) fn site_callee(site: &CallSite) -> String {
    site.constructs.clone().unwrap_or_else(|| {
        site.project_callees()
            .first()
            .map(|bc| bc.callee.fqn())
            .unwrap_or_default()
    })
}

/// Parameters of the caller the receiver of `site` may be (`self.m()`: bit 0); none for a
/// construction, whose object is fresh.
fn receiver_params(site: &CallSite) -> u64 {
    if site.constructs.is_some() {
        return 0;
    }
    site.project_callees()
        .first()
        .and_then(|bc| bc.params.first())
        .map_or(0, |o| o.may_params)
}

struct CallOwnerRule<'r> {
    name: &'r str,
    targets: Vec<CallTargetPattern>,
    allowance: Allowance,
    note: Option<&'r str>,
}

/// `MUT204`: calls of `call-owners` targets outside their allowed callers, reported like the
/// reads of `MUT203` (private helpers hand them to their callers; functions that only run
/// inside allowed contexts and a class's own initialization are not reported).
pub(crate) fn mut204(
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) -> Result<(), RuleConfigError> {
    if !cx.selection.is_enabled("MUT204") || cx.config.call_owners.is_empty() {
        return Ok(());
    }
    let all_classes = project_classes(cx);
    let mut rules = Vec::new();
    for rule in &cx.config.call_owners {
        let context = "call-owners.targets";
        // The targets may call each other: a call of the guarded operation is checked where it
        // is made, so what the operation does inside is part of it.
        let guarded = rule
            .targets
            .iter()
            .map(|t| FunctionMatcher::new(cx, &all_classes, t, context))
            .collect::<Result<Vec<_>, _>>()?;
        rules.push(CallOwnerRule {
            name: &rule.name,
            targets: parse_call_targets(cx, &all_classes, &rule.targets, context)?,
            allowance: Allowance::new(&AllowanceKeys {
                allowed: (&rule.allowed_callers, "call-owners.allowed-callers"),
                forbidden: (&rule.forbidden_callers, "call-owners.forbidden-callers"),
                exempt: (&rule.exempt_methods, "call-owners.exempt-methods"),
            })?
            .with_targets(guarded),
            note: rule.message.as_deref(),
        });
    }
    let sites = cx.sites_by_caller();
    let mut callers: Vec<&FuncKey> = sites.keys().copied().collect();
    callers.sort();
    let mut reports = MergedReports::default();
    for key in callers {
        for &(path, site) in &sites[key] {
            for rule in &rules {
                for group in called_targets(&rule.targets, site) {
                    let (target, called) = group_texts(&rule.targets, &group);
                    let family: FxHashSet<String> = group
                        .iter()
                        .flat_map(|&i| rule.targets[i].family.iter().cloned())
                        .collect();
                    let found = delegated_reports(
                        cx,
                        &sites,
                        &rule.allowance,
                        &family,
                        key,
                        receiver_params(site),
                        site.span,
                    );
                    for r in found {
                        let Some(unit) = cx.index.file_of(&r.func).and_then(|p| cx.unit(p)) else {
                            continue;
                        };
                        reports.add(
                            unit,
                            &r.func,
                            r.span,
                            &target,
                            (key.fqn(), site.span),
                            rule.note,
                            || {
                                let what = if site.constructs.is_some() {
                                    "constructs"
                                } else {
                                    "calls"
                                };
                                let via = if r.chain.is_empty() {
                                    String::new()
                                } else {
                                    format!(" via `{}`", key.fqn())
                                };
                                format!(
                                    "`{}` {what} {called}{via} {}",
                                    r.func.qualname,
                                    rule.allowance.where_text(
                                        "call owners",
                                        "forbidden callers",
                                        rule.name
                                    )
                                )
                            },
                            || {
                                // A delegated report ends its chain at the call itself.
                                let mut chain = r.chain.clone();
                                if !chain.is_empty() {
                                    chain.push(CallHop {
                                        caller: key.clone(),
                                        callee: site_callee(site),
                                        span: site.span,
                                        path: path.to_path_buf(),
                                    });
                                }
                                chain
                            },
                        );
                    }
                }
            }
        }
    }
    reports.emit(cx, "MUT204", out);
    Ok(())
}
