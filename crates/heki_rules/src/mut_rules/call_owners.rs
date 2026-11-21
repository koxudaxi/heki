//! `MUT204` call ownership (`[[tool.heki.call-owners]]`), and the call-target matcher it shares
//! with the `calls` effect boundary of `EFF201`.
//!
//! Call sites are local facts of the index (`FileSummary::call_sites`); a call in a private
//! helper is reported at its callers by the walk `MUT203` uses (`delegated_reports`), so no
//! summary carries call facts.

use super::{
    Allowance, AllowanceKeys, FunctionMatcher, MergedReports, delegated_reports, project_classes,
    reports_unresolved,
};
use crate::{CheckContext, DottedPattern, RuleConfigError};
use heki_diagnostics::Diagnostic;
use heki_effect::{CallHop, CallSite, FuncKey, SiteRole};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeSet;
use std::rc::Rc;

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
    /// target on its own `self` initializes itself. A text naming no class (`reset`) has none:
    /// its classes are those whose methods it names ([`Self::calls_on`], [`Families`]).
    family: FxHashSet<String>,
    /// The last name of the text (`replace_reference` of `DataType.replace_reference`): what a
    /// call on an object of unknown class names (`MUT207`).
    pub name: DottedPattern,
}

impl CallTargetPattern {
    /// What `site` calls of the target: the class it constructs, or every candidate the target
    /// names.
    fn called_at(&self, site: &CallSite) -> Vec<String> {
        match &site.constructs {
            Some(class) => vec![class.clone()],
            None => site
                .project_callees()
                .iter()
                .filter(|bc| self.functions.matches(&bc.callee))
                .map(|bc| bc.callee.fqn())
                .collect(),
        }
    }

    /// Whether calling `name` on an instance of the project class `class` may call the target:
    /// `class` is of the target's family, or the method `name` its MRO finds first is the
    /// target (`Model.reset` for the target `reset`).
    pub(crate) fn calls_on(&self, cx: &CheckContext<'_>, class: &str, name: &str) -> bool {
        self.family.contains(class)
            || cx
                .decls
                .mro_method(class, name)
                .is_some_and(|k| self.functions.matches(&k))
    }

    /// Add to `out` the project classes defining the methods `site` calls that the target
    /// names.
    fn defining_classes(&self, cx: &CheckContext<'_>, site: &CallSite, out: &mut Vec<String>) {
        for bc in site.project_callees() {
            if let Some((owner, _)) = bc.callee.qualname.rsplit_once('.')
                && self.functions.matches(&bc.callee)
            {
                let class = format!("{}.{owner}", bc.callee.module);
                if cx.decls.is_project_class(&class) {
                    out.push(class);
                }
            }
        }
    }

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

/// The class families of the groups of targets that sites call ([`called_targets`]): the
/// targets' families with the classes defining the methods a site calls that they name, and
/// their subclasses (an initializer of one of them calling the method on its own `self`
/// initializes itself). Kept per group and defining classes, which many sites share.
#[derive(Default)]
struct Families(FxHashMap<FamilyKey, Rc<FxHashSet<String>>>);

/// A group of targets (indices into the contract's targets) and the classes defining the
/// methods a site calls that they name, sorted.
type FamilyKey = (Vec<usize>, Vec<String>);

impl Families {
    fn at(
        &mut self,
        cx: &CheckContext<'_>,
        targets: &[CallTargetPattern],
        group: &[usize],
        site: &CallSite,
    ) -> Rc<FxHashSet<String>> {
        let mut defining = Vec::new();
        for &i in group {
            targets[i].defining_classes(cx, site, &mut defining);
        }
        defining.sort_unstable();
        defining.dedup();
        let key = (group.to_vec(), defining);
        if let Some(family) = self.0.get(&key) {
            return Rc::clone(family);
        }
        let mut family: FxHashSet<String> = group
            .iter()
            .flat_map(|&i| targets[i].family.iter().cloned())
            .collect();
        for class in &key.1 {
            family.extend(cx.decls.subclasses_of(class));
            family.insert(class.clone());
        }
        let family = Rc::new(family);
        self.0.insert(key, Rc::clone(&family));
        family
    }
}

/// The configured texts of a group of [`called_targets`]: the diagnostic target (`A.reset|B.reset`)
/// and the message's name of what is called (``A.reset` or `B.reset``). With the call `site`,
/// a glob target is named by the functions or the class the site calls of it
/// ([`super::names_members`]); an `EFF201` report stands for every site of its target and
/// names the target.
pub(crate) fn group_texts(
    targets: &[CallTargetPattern],
    group: &[usize],
    site: Option<&CallSite>,
) -> (String, String) {
    let texts: Vec<&str> = group.iter().map(|&i| targets[i].text.as_str()).collect();
    // In the order first named, each once.
    let mut named: Vec<String> = Vec::new();
    let mut seen: FxHashSet<String> = FxHashSet::default();
    for &i in group {
        let names = match site {
            Some(site) if super::names_members(&targets[i].text) => targets[i].called_at(site),
            _ => vec![targets[i].text.clone()],
        };
        for name in names {
            if seen.insert(name.clone()) {
                named.push(format!("`{name}`"));
            }
        }
    }
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
        let name = text
            .rsplit_once('.')
            .map_or(text.as_str(), |(_, last)| last);
        out.push(CallTargetPattern {
            text: text.clone(),
            functions,
            classes,
            family,
            name: DottedPattern::new(name, context)?,
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

pub(super) struct CallOwnerRule<'r> {
    pub(super) name: &'r str,
    pub(super) targets: Vec<CallTargetPattern>,
    pub(super) allowance: Allowance,
    pub(super) note: Option<&'r str>,
    /// `unresolved = "report"`: calls heki cannot tie to a class are reported (`MUT207`).
    pub(super) report_unresolved: bool,
}

/// `MUT204`: calls of `call-owners` targets outside their allowed callers, reported like the
/// reads of `MUT203` (private helpers hand them to their callers; functions that only run
/// inside allowed contexts and a class's own initialization are not reported).
pub(crate) fn mut204(
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) -> Result<(), RuleConfigError> {
    let (calls, unresolved) = (
        cx.selection.is_enabled("MUT204"),
        cx.selection.is_enabled("MUT207"),
    );
    if !calls && !unresolved || cx.config.call_owners.is_empty() {
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
            report_unresolved: reports_unresolved(
                rule.unresolved.as_deref(),
                "call-owners",
                &rule.name,
            )?,
        });
    }
    super::unresolved::mut207(cx, &rules, out);
    if !calls {
        return Ok(());
    }
    let sites = cx.sites_by_caller();
    let mut callers: Vec<&FuncKey> = sites.keys().copied().collect();
    callers.sort();
    let mut reports = MergedReports::default();
    let mut families: Vec<Families> = rules.iter().map(|_| Families::default()).collect();
    for key in callers {
        for &(path, site) in &sites[key] {
            for (rule, families) in rules.iter().zip(&mut families) {
                for group in called_targets(&rule.targets, site) {
                    let (target, called) = group_texts(&rule.targets, &group, Some(site));
                    let family = families.at(cx, &rule.targets, &group, site);
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
                            (&target, &called),
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
