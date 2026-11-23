//! `MUT204` call ownership (`[[tool.heki.call-owners]]`), and the call-target matcher it shares
//! with the `calls` effect boundary of `EFF201`.
//!
//! Call sites are local facts of the index (`FileSummary::call_sites`, and the references that
//! count as calls, `FileSummary::reference_sites`); a call in a private helper is reported at
//! its callers by the walk `MUT203` uses (`delegated_reports`), so no summary carries call
//! facts.

use super::{
    AccessGroups, Allowance, AllowanceKeys, DelegatedReport, FunctionMatcher, InsideAllowed,
    MemberPattern, MergedReports, Named, References, allows_own_receiver, attribution,
    binds_receiver, counts_references, delegated_reports, named_protocols, project_classes,
    reports_unresolved,
};
use crate::{
    CheckContext, DottedPattern, NamePart, RuleConfigError, SitesByCaller, def_spellings,
    last_name, module_of, split_last_dot,
};
use globset::{GlobSet, GlobSetBuilder};
use heki_diagnostics::Diagnostic;
use heki_effect::{ArgValue, CallHop, CallSite, ContractClass, FuncKey, SiteRole};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;

/// One call target: a project function, a method (with its overrides in subclasses), or a class
/// (constructing it).
pub(crate) struct CallTargetPattern {
    /// The configured text (diagnostic target and baseline key).
    pub text: String,
    /// `classes` and the method's class family: an initializer of one of them calling the
    /// target on its own `self` initializes itself. A text naming no class (`reset`) has none:
    /// its classes are those whose methods it names ([`CallTargets::calls_on`], [`Families`]).
    family: FxHashSet<String>,
    /// The last name of the text (`replace_reference` of `DataType.replace_reference`): what a
    /// call on an object of unknown class names (`MUT207`).
    pub name: MemberPattern,
    /// The argument list of the text (`(is_dict=True)`, empty without one), as configured.
    list: String,
    /// The constants the list names: a call matches only when it passes each.
    arguments: Arguments,
}

/// The call targets of one contract (`call-owners.targets`, `effect-boundaries.call-targets`),
/// indexed so that the targets a call matches are looked up, not tried one by one: the cost of
/// a contract grows with its call sites, whatever its number of targets.
pub(crate) struct CallTargets {
    pub patterns: Vec<CallTargetPattern>,
    /// The functions and methods each target names, by target.
    pub(crate) functions: Arc<FunctionMatchers>,
    /// Each project class a target's whole text names, or a subclass of one: the targets
    /// constructing it calls.
    constructors: FxHashMap<String, Vec<usize>>,
}

impl CallTargets {
    /// The targets `site` calls, as the groups to report (indices into `patterns`): each target
    /// that every candidate matches, on its own (constructing a class matches by its class
    /// only: its `__init__` runs as part of it); or, when no single target covers every
    /// candidate but each candidate is a target (`A.reset` and `B.reset` both named,
    /// `obj: Base` whose `reset` is one of the two), one group of the targets the candidates
    /// match. Only the call itself calls (`SiteRole::Call`): the other sites of the object a
    /// call returns (where it is iterated, returned, or handed on) call nothing more.
    pub(crate) fn called(&self, site: &CallSite) -> Vec<Vec<usize>> {
        if site.role != SiteRole::Call {
            return Vec::new();
        }
        if let Some(class) = &site.constructs {
            let hits = self.constructors.get(class).map_or(&[][..], Vec::as_slice);
            let passing = hits.iter().filter(|&&i| self.passed(i, site));
            return passing.map(|&i| vec![i]).collect();
        }
        let per_callee: Vec<Vec<usize>> = judged_callees(site)
            .map(|callee| {
                let mut matching = self.functions.matching(callee);
                matching.retain(|&i| self.passed(i, site));
                matching
            })
            .collect();
        groups(&per_callee)
    }

    /// Whether `site` passes every constant the argument list of target `i` names (a target
    /// without one: always).
    fn passed(&self, i: usize, site: &CallSite) -> bool {
        self.patterns[i]
            .arguments
            .iter()
            .all(|(name, value)| site.argument(name) == Some(value))
    }

    /// What `site` calls of target `i`: the method the protocol typing the receiver declares
    /// ([`CallSite::declared`]), the class it constructs, or every candidate the target names.
    fn called_at(&self, i: usize, site: &CallSite) -> Vec<String> {
        if let Some(declared) = site.declared() {
            return vec![declared.fqn()];
        }
        match &site.constructs {
            Some(class) => vec![class.clone()],
            None => site
                .project_callees()
                .iter()
                .filter(|bc| self.functions.matchers[i].matches(&bc.callee))
                .map(|bc| bc.callee.fqn())
                .collect(),
        }
    }

    /// Whether calling `name` on an instance of the project class `class` may call target
    /// `i`: `class` is of the target's family, or the method `name` its MRO finds first is the
    /// target (`Model.reset` for the target `reset`).
    pub(crate) fn calls_on(
        &self,
        cx: &CheckContext<'_>,
        i: usize,
        class: &str,
        name: &str,
    ) -> bool {
        self.patterns[i].family.contains(class)
            || cx
                .decls
                .mro_method(class, name)
                .is_some_and(|k| self.functions.matchers[i].matches(&k))
    }

    /// Add to `out` the project classes defining the methods `site` calls that target `i`
    /// names.
    fn defining_classes(
        &self,
        cx: &CheckContext<'_>,
        i: usize,
        site: &CallSite,
        out: &mut Vec<String>,
    ) {
        for bc in site.project_callees() {
            if let Some((owner, _)) = bc.callee.qualname.rsplit_once('.')
                && self.functions.matchers[i].matches(&bc.callee)
            {
                let class = format!("{}.{owner}", bc.callee.module);
                if cx.decls.is_project_class(&class) {
                    out.push(class);
                }
            }
        }
    }
}

/// The class families of the groups of targets that sites call ([`CallTargets::called`]): the
/// targets' families with the classes defining the methods a site calls that they name, and
/// their subclasses (an initializer of one of them calling the method on its own `self`
/// initializes itself). Kept per group and defining classes, which many sites share, each with
/// its number among the contract's families and the answers its calls share ([`InsideAllowed`]).
#[derive(Default)]
struct Families<'k>(FxHashMap<FamilyKey, (usize, Rc<FxHashSet<String>>, InsideAllowed<'k>)>);

/// A group of targets (indices into the contract's targets) and the classes defining the
/// methods a site calls that they name, sorted.
type FamilyKey = (Vec<usize>, Vec<String>);

impl<'k> Families<'k> {
    fn at(
        &mut self,
        cx: &CheckContext<'_>,
        targets: &CallTargets,
        group: &[usize],
        site: &CallSite,
    ) -> (usize, Rc<FxHashSet<String>>, &mut InsideAllowed<'k>) {
        let mut defining = Vec::new();
        for &i in group {
            targets.defining_classes(cx, i, site, &mut defining);
        }
        defining.sort_unstable();
        defining.dedup();
        let id = self.0.len();
        let (id, family, inside) =
            self.0
                .entry((group.to_vec(), defining))
                .or_insert_with_key(|key| {
                    let mut family: FxHashSet<String> = group
                        .iter()
                        .flat_map(|&i| targets.patterns[i].family.iter().cloned())
                        .collect();
                    for class in &key.1 {
                        family.extend(cx.decls.subclasses_of(class));
                        family.insert(class.clone());
                    }
                    (id, Rc::new(family), InsideAllowed::default())
                });
        (*id, Rc::clone(family), inside)
    }
}

/// The functions a call contract judges `site` by: the method the protocol typing the
/// receiver declares ([`CallSite::declared`]), else the project functions it may call.
fn judged_callees(site: &CallSite) -> Box<dyn Iterator<Item = &FuncKey> + '_> {
    match site.declared() {
        Some(declared) => Box::new(std::iter::once(declared)),
        None => Box::new(site.project_callees().iter().map(|bc| &bc.callee)),
    }
}

/// The groups of [`CallTargets::called`] from the targets each candidate of a call matches
/// (each list ascending).
fn groups(per_callee: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut each = per_callee.iter();
    let mut every: BTreeSet<usize> = each.next().into_iter().flatten().copied().collect();
    let mut union = every.clone();
    for matching in each {
        every.retain(|i| matching.binary_search(i).is_ok());
        union.extend(matching);
    }
    if !every.is_empty() {
        return every.into_iter().map(|i| vec![i]).collect();
    }
    if !per_callee.is_empty() && per_callee.iter().all(|m| !m.is_empty()) {
        return vec![union.into_iter().collect()];
    }
    Vec::new()
}

/// The call targets of every `call-owners` contract, indexed together: the targets a call
/// matches are looked up once per call, whatever the number of contracts.
struct ContractCalls<'t> {
    functions: FunctionMatchers,
    /// `(contract, target)` of each matcher of `functions`.
    owners: Vec<(usize, usize)>,
    /// The `(contract, target)` pairs constructing each class calls.
    constructors: FxHashMap<String, Vec<(usize, usize)>>,
    /// The targets of each contract (the constants their argument lists name).
    contracts: Vec<&'t CallTargets>,
}

impl<'t> ContractCalls<'t> {
    fn new(contracts: impl Iterator<Item = &'t CallTargets>) -> Result<Self, RuleConfigError> {
        let contracts: Vec<&CallTargets> = contracts.collect();
        let mut matchers = Vec::new();
        let mut owners = Vec::new();
        let mut constructors: FxHashMap<String, Vec<(usize, usize)>> = FxHashMap::default();
        for (c, targets) in contracts.iter().enumerate() {
            for (t, m) in targets.functions.matchers.iter().enumerate() {
                matchers.push(m.clone());
                owners.push((c, t));
            }
            for (class, hits) in &targets.constructors {
                let pairs = constructors.entry(class.clone()).or_default();
                pairs.extend(hits.iter().map(|&t| (c, t)));
            }
        }
        Ok(Self {
            functions: FunctionMatchers::new(matchers, "call-owners.targets")?,
            owners,
            constructors,
            contracts,
        })
    }

    /// [`CallTargets::called`] of every contract `site` calls a target of, by contract.
    fn called(&self, site: &CallSite) -> Vec<(usize, Vec<Vec<usize>>)> {
        if site.role != SiteRole::Call {
            return Vec::new();
        }
        let mut by_contract: BTreeMap<usize, Vec<Vec<usize>>> = BTreeMap::new();
        let passed = |&(c, t): &(usize, usize)| self.contracts[c].passed(t, site);
        if let Some(class) = &site.constructs {
            for &(c, t) in self
                .constructors
                .get(class)
                .into_iter()
                .flatten()
                .filter(|p| passed(p))
            {
                by_contract.entry(c).or_default().push(vec![t]);
            }
            return by_contract.into_iter().collect();
        }
        let callees: Vec<&FuncKey> = judged_callees(site).collect();
        for (k, callee) in callees.iter().enumerate() {
            for m in self.functions.matching(callee) {
                let (c, t) = self.owners[m];
                if !passed(&(c, t)) {
                    continue;
                }
                let per_callee = by_contract
                    .entry(c)
                    .or_insert_with(|| vec![Vec::new(); callees.len()]);
                per_callee[k].push(t);
            }
        }
        by_contract
            .into_iter()
            .map(|(c, per_callee)| (c, groups(&per_callee)))
            .filter(|(_, g)| !g.is_empty())
            .collect()
    }
}

/// Function patterns indexed by the names they may match ([`FunctionMatcher`]): the matchers
/// a function satisfies are found from the dotted suffixes of its names, from its module, and
/// from its class.
#[derive(Debug)]
pub(crate) struct FunctionMatchers {
    matchers: Vec<FunctionMatcher>,
    /// The whole texts without a module part, by their names.
    names: PartIndex,
    /// The texts with a module part, by their modules and qualnames.
    modules: ModuleIndex,
    /// The classes of each `Class.method` matcher's class family, and the matchers of each.
    methods: FxHashMap<String, Vec<usize>>,
}

/// Parts of the matchers' texts ([`NamePart`]): the literal ones, with the matchers of each, and
/// the globs, with the matcher of each.
#[derive(Debug)]
struct PartIndex {
    literal: FxHashMap<String, Vec<usize>>,
    globs: GlobSet,
    glob_matchers: Vec<usize>,
}

impl PartIndex {
    /// Index `parts`, each with its matcher; their globs, each compiled already, may still be
    /// too many for one set (`GlobSet` under `context`).
    fn new<'p>(
        parts: impl Iterator<Item = (usize, &'p NamePart)>,
        context: &'static str,
    ) -> Result<Self, RuleConfigError> {
        let mut literal: FxHashMap<String, Vec<usize>> = FxHashMap::default();
        let mut globs = GlobSetBuilder::new();
        let mut glob_matchers = Vec::new();
        for (i, part) in parts {
            match part.glob() {
                Some(glob) => {
                    globs.add(glob.clone());
                    glob_matchers.push(i);
                }
                None => literal.entry(part.text().to_owned()).or_default().push(i),
            }
        }
        let globs = globs.build();
        Ok(Self {
            literal,
            globs: globs.map_err(|cause| RuleConfigError::GlobSet { context, cause })?,
            glob_matchers,
        })
    }

    /// Add to `out` the matchers whose part is `name` or a glob matching it.
    fn find(&self, name: &str, out: &mut Vec<usize>) {
        out.extend(self.literal.get(name).into_iter().flatten());
        out.extend(
            self.globs
                .matches(name)
                .into_iter()
                .map(|g| self.glob_matchers[g]),
        );
    }
}

/// The texts with a module part: by module part (each literal one, and each glob once), then by
/// qualname part, so that a function's module and qualname find them however many texts share a
/// module.
#[derive(Debug)]
struct ModuleIndex {
    literal: FxHashMap<String, PartIndex>,
    globs: GlobSet,
    /// The qualname parts of the texts under each glob of `globs`.
    glob_parts: Vec<PartIndex>,
}

impl ModuleIndex {
    /// Index the `(matcher, module part, qualname part)` of each text.
    fn new<'p>(
        parts: impl Iterator<Item = (usize, &'p NamePart, &'p NamePart)>,
        context: &'static str,
    ) -> Result<Self, RuleConfigError> {
        let mut literal: FxHashMap<&str, Vec<(usize, &NamePart)>> = FxHashMap::default();
        let mut globbed: Vec<(&globset::Glob, Vec<(usize, &NamePart)>)> = Vec::new();
        let mut glob_at: FxHashMap<&str, usize> = FxHashMap::default();
        for (i, module, qualname) in parts {
            match module.glob() {
                Some(glob) => {
                    let at = *glob_at.entry(module.text()).or_insert_with(|| {
                        globbed.push((glob, Vec::new()));
                        globbed.len() - 1
                    });
                    globbed[at].1.push((i, qualname));
                }
                None => literal
                    .entry(module.text())
                    .or_default()
                    .push((i, qualname)),
            }
        }
        let mut globs = GlobSetBuilder::new();
        let mut glob_parts = Vec::new();
        for (glob, qualnames) in globbed {
            globs.add(glob.clone());
            glob_parts.push(PartIndex::new(qualnames.into_iter(), context)?);
        }
        let literal = literal
            .into_iter()
            .map(|(module, qualnames)| {
                Ok((
                    module.to_owned(),
                    PartIndex::new(qualnames.into_iter(), context)?,
                ))
            })
            .collect::<Result<_, RuleConfigError>>()?;
        let globs = globs.build();
        Ok(Self {
            literal,
            globs: globs.map_err(|cause| RuleConfigError::GlobSet { context, cause })?,
            glob_parts,
        })
    }

    /// Add to `out` the matchers whose module part names `module` and whose qualname part is
    /// `qualname` or a glob matching it.
    fn find(&self, module: &str, qualname: &str, out: &mut Vec<usize>) {
        if let Some(parts) = self.literal.get(module) {
            parts.find(qualname, out);
        }
        for g in self.globs.matches(module) {
            self.glob_parts[g].find(qualname, out);
        }
    }
}

impl FunctionMatchers {
    pub(crate) fn new(
        matchers: Vec<FunctionMatcher>,
        context: &'static str,
    ) -> Result<Self, RuleConfigError> {
        let wholes = || matchers.iter().map(FunctionMatcher::whole).enumerate();
        let names = wholes()
            .filter(|(_, w)| w.module().is_none())
            .map(|(i, w)| (i, w.name()));
        let modules = wholes().filter_map(|(i, w)| Some((i, w.module()?, w.name())));
        let mut methods: FxHashMap<String, Vec<usize>> = FxHashMap::default();
        for (i, m) in matchers.iter().enumerate() {
            for class in m.method_family().into_iter().flatten() {
                methods.entry(class.clone()).or_default().push(i);
            }
        }
        Ok(Self {
            names: PartIndex::new(names, context)?,
            modules: ModuleIndex::new(modules, context)?,
            matchers,
            methods,
        })
    }

    /// The matchers without a module part whose whole text may name `name` or one of its
    /// dotted suffixes.
    fn naming(&self, name: &str, out: &mut Vec<usize>) {
        let suffixes =
            std::iter::once(name).chain(name.match_indices('.').map(|(i, _)| &name[i + 1..]));
        for suffix in suffixes {
            self.names.find(suffix, out);
        }
    }

    /// The matchers `key` satisfies ([`FunctionMatcher::matches`]), ascending.
    pub(crate) fn matching(&self, key: &FuncKey) -> Vec<usize> {
        let mut found = Vec::new();
        let fqn = key.fqn();
        for (fqn, qualname) in def_spellings(&fqn, &key.qualname) {
            self.naming(&fqn, &mut found);
            self.naming(&qualname, &mut found);
            self.modules.find(&key.module, &qualname, &mut found);
        }
        if let Some((owner, _)) = key.qualname.rsplit_once('.') {
            let class = format!("{}.{owner}", key.module);
            found.extend(self.methods.get(&class).into_iter().flatten());
        }
        found.sort_unstable();
        found.dedup();
        found.retain(|&i| self.matchers[i].matches(key));
        found
    }

    /// Whether `key` satisfies some matcher.
    pub(crate) fn any(&self, key: &FuncKey) -> bool {
        !self.matching(key).is_empty()
    }

    /// The matchers naming `symbol`, a function, class, or module attribute outside the
    /// project ([`FunctionMatcher::names_external`]), ascending: looked up by the whole name,
    /// never by its suffixes.
    pub(crate) fn naming_external(&self, symbol: &str) -> Vec<usize> {
        let mut found = Vec::new();
        // A text with a module part names project definitions only.
        self.names.find(symbol, &mut found);
        found.sort_unstable();
        found.dedup();
        found.retain(|&i| self.matchers[i].names_external(symbol));
        found
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.matchers.is_empty()
    }

    /// The matchers whose whole text names the project class `class`.
    fn naming_class(&self, class: &ContractClass) -> Vec<usize> {
        let qualname = class.qualname.as_deref().unwrap_or(&class.fqn);
        let mut found = Vec::new();
        for (fqn, qualname) in def_spellings(&class.fqn, qualname) {
            self.naming(&fqn, &mut found);
            if let Some(module) = module_of(&fqn, &qualname) {
                self.modules.find(module, &qualname, &mut found);
            }
        }
        found.sort_unstable();
        found.dedup();
        found.retain(|&i| self.matchers[i].names_def(&class.fqn, qualname));
        found
    }
}

/// The configured texts of a group of [`CallTargets::called`]: the diagnostic target
/// (`A.reset|B.reset`) and the message's name of what is called (``A.reset` or `B.reset``).
/// With the call `site`, a glob target is named by the functions or the class the site calls of
/// it ([`super::names_members`]); an `EFF201` report stands for every site of its target and
/// names the target.
pub(crate) fn group_texts(
    targets: &CallTargets,
    group: &[usize],
    site: Option<&CallSite>,
) -> (String, String) {
    let text = |i: usize| targets.patterns[i].text.as_str();
    let texts: Vec<&str> = group.iter().map(|&i| text(i)).collect();
    // In the order first named, each once.
    let mut named: Vec<String> = Vec::new();
    let mut seen: FxHashSet<String> = FxHashSet::default();
    for &i in group {
        let pattern = &targets.patterns[i];
        let names = match site {
            Some(site) if super::names_members(pattern.name_part()) => {
                let called = targets.called_at(i, site).into_iter();
                called
                    .map(|name| format!("{name}{}", pattern.list))
                    .collect()
            }
            _ => vec![text(i).to_owned()],
        };
        for name in names {
            if seen.insert(name.clone()) {
                named.push(format!("`{name}`"));
            }
        }
    }
    (texts.join("|"), named.join(" or "))
}

/// Whether a class a contract names stands for its subclasses too: a class target of
/// `call-owners` (`subclasses`), and an `allowed-protocols` pattern (`protocol-subclasses`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Subclasses {
    /// The class and its subclasses: constructing a subclass calls a class target.
    Covered,
    /// The class itself.
    Excluded,
}

impl Subclasses {
    /// The value of a `subclasses`-style key (`true` when unset).
    pub(crate) fn of(value: Option<bool>) -> Self {
        if value == Some(false) {
            Self::Excluded
        } else {
            Self::Covered
        }
    }
}

impl CallTargetPattern {
    /// The text without its argument list: the name of what it calls.
    fn name_part(&self) -> &str {
        &self.text[..self.text.len() - self.list.len()]
    }
}

/// The constants an argument list names, by parameter.
pub(crate) type Arguments = Vec<(String, ArgValue)>;

/// A call target's text split into the name it matches and the constants its argument list
/// names (`pkg.types.DataType(is_dict=True)`; none for a text without a list). The name never
/// holds a parenthesis (no dotted name or glob does), so the list starts at the first one.
pub(crate) fn split_arguments<'t>(
    text: &'t str,
    context: &'static str,
) -> Result<(&'t str, Option<Arguments>), RuleConfigError> {
    let Some(open) = text.find('(') else {
        return Ok((text, None));
    };
    let invalid = |reason: &'static str| RuleConfigError::InvalidArguments {
        target: text.to_owned(),
        context,
        reason,
    };
    let mut rest = text[open + 1..]
        .strip_suffix(')')
        .ok_or_else(|| invalid("the list does not end the text"))?
        .trim_start();
    let mut arguments: Arguments = Vec::new();
    loop {
        let (name, after) = rest
            .split_once('=')
            .filter(|(name, _)| is_identifier(name.trim()))
            .ok_or_else(|| invalid("an entry is not `parameter=value`"))?;
        let name = name.trim();
        if arguments.iter().any(|(n, _)| n == name) {
            return Err(invalid("a parameter is named twice"));
        }
        let (value, after) =
            literal_prefix(after.trim_start()).ok_or_else(|| invalid("a value is no constant"))?;
        arguments.push((name.to_owned(), value));
        let after = after.trim_start();
        if after.is_empty() {
            return Ok((&text[..open], Some(arguments)));
        }
        rest = after
            .strip_prefix(',')
            .ok_or_else(|| invalid("entries are not separated by commas"))?
            .trim_start();
    }
}

/// Whether `name` is an identifier of ASCII letters, digits, and underscores.
fn is_identifier(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The constant at the start of `text` and what follows it: `True`, `False`, `None`, an integer,
/// or a string in single or double quotes without a backslash.
fn literal_prefix(text: &str) -> Option<(ArgValue, &str)> {
    if let Some(quote) = text.chars().next().filter(|c| matches!(c, '"' | '\'')) {
        let (value, rest) = text[1..].split_once(quote)?;
        return (!value.contains('\\')).then(|| (ArgValue::Str(value.to_owned()), rest));
    }
    let end = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(text.len());
    let (word, rest) = text.split_at(end);
    let value = match word {
        "True" => ArgValue::Bool(true),
        "False" => ArgValue::Bool(false),
        "None" => ArgValue::None,
        _ => ArgValue::Int(word.parse().ok()?),
    };
    Some((value, rest))
}

/// The last name of the call target `text` as a member of the classes the rest names.
fn member_name(
    text: &str,
    all_classes: &[ContractClass],
    context: &'static str,
) -> Result<MemberPattern, RuleConfigError> {
    let holder = match split_last_dot(text) {
        Some((holder, _)) => Some(DottedPattern::new(holder, context)?),
        None => None,
    };
    MemberPattern::new(last_name(text), holder.as_ref(), all_classes, context)
}

/// Parse call targets (`call-owners.targets`, `effect-boundaries.call-targets`).
pub(crate) fn parse_call_targets(
    cx: &CheckContext<'_>,
    all_classes: &[ContractClass],
    texts: &[String],
    context: &'static str,
    subclasses: Subclasses,
) -> Result<CallTargets, RuleConfigError> {
    let mut patterns = Vec::new();
    let mut matchers = Vec::new();
    for text in texts {
        let (name, arguments) = split_arguments(text, context)?;
        let mut functions = FunctionMatcher::new(cx, all_classes, name, context)?;
        if arguments.is_some() {
            functions = functions.project_only();
        }
        patterns.push(CallTargetPattern {
            text: text.clone(),
            family: functions.method_family().cloned().unwrap_or_default(),
            name: member_name(name, all_classes, context)?,
            list: text[name.len()..].to_owned(),
            arguments: arguments.unwrap_or_default(),
        });
        matchers.push(functions);
    }
    let functions = Arc::new(FunctionMatchers::new(matchers, context)?);
    // The classes each target names, found class by class.
    let mut constructors: FxHashMap<String, Vec<usize>> = FxHashMap::default();
    for class in all_classes {
        let naming = functions.naming_class(class);
        if naming.is_empty() {
            continue;
        }
        let subclasses_of = match subclasses {
            Subclasses::Covered => cx.decls.subclasses_of(&class.fqn),
            Subclasses::Excluded => Vec::new(),
        };
        for i in naming {
            for named in std::iter::once(class.fqn.clone()).chain(subclasses_of.iter().cloned()) {
                patterns[i].family.insert(named.clone());
                constructors.entry(named).or_default().push(i);
            }
        }
    }
    for targets in constructors.values_mut() {
        targets.sort_unstable();
        targets.dedup();
    }
    Ok(CallTargets {
        patterns,
        functions,
        constructors,
    })
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

/// What a report says `site` does with its target: subclasses it (a base of a class
/// statement), reads it (a reference site using it for anything else), hands it on (another
/// reference site), constructs it, or calls it.
pub(crate) fn call_verb(site: &CallSite, reference: bool) -> &'static str {
    if site.is_base {
        "subclasses"
    } else if site.is_read {
        "reads"
    } else if reference {
        "hands on"
    } else if site.constructs.is_some() {
        "constructs"
    } else {
        "calls"
    }
}

/// Whether `site` calls, or names (`references`), a method through a value typed with a protocol
/// `allowance` allows (`allowed-protocols`) that declares it: the receiver's class is the
/// protocol, and the method is the protocol's declaration ([`CallSite::declared`]) or, for a
/// reference, which keeps none, the one the site names.
fn through_allowed_protocol(cx: &CheckContext<'_>, allowance: &Allowance, site: &CallSite) -> bool {
    let Some(bc) = site.project_callees().first() else {
        return false;
    };
    let Some(protocol) = bc.params.first().and_then(|o| o.typed_instance.as_deref()) else {
        return false;
    };
    let method = site.declared().unwrap_or(&bc.callee);
    let name = method
        .qualname
        .rsplit('.')
        .next()
        .unwrap_or(&method.qualname);
    allowance.allows_protocol(cx.decls, protocol, name)
}

/// Parameters of the caller the object `site` looks the called method up on may be: its
/// receiver (`self._m()`, `cls._m()`, `super()._m()`), or, for a call binding no receiver of its
/// own (`Class._m(self)`), the object every function a call contract judges it by binds as its
/// receiver. The first argument of a call of a static method or of a function is no receiver.
fn looked_up_params(cx: &CheckContext<'_>, site: &CallSite) -> u64 {
    let binds = judged_callees(site).all(|k| cx.decls.function_decl(k).is_some_and(binds_receiver));
    site.recv_params() | if binds { receiver_params(site) } else { 0 }
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
    /// How a report names the contract ([`heki_env::contract_label`]).
    pub(super) label: String,
    pub(super) targets: CallTargets,
    pub(super) allowance: Allowance,
    pub(super) note: Option<&'r str>,
    /// `unresolved = "report"`: calls heki cannot tie to a class are reported (`MUT207`).
    pub(super) report_unresolved: bool,
    /// The references that count as calls of a target (`references`).
    pub(super) references: References,
}

/// `MUT204`: calls of `call-owners` targets outside their allowed callers, reported like the
/// reads of `MUT203` (private helpers hand them to their callers; functions that only run
/// inside allowed contexts and a class's own initialization are not reported).
pub(crate) fn mut204<'c>(
    cx: &CheckContext<'c>,
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
    for (index, rule) in cx.config.call_owners.iter().enumerate() {
        let context = "call-owners.targets";
        let subclasses = Subclasses::of(rule.subclasses);
        let targets = parse_call_targets(cx, &all_classes, &rule.targets, context, subclasses)?;
        // The targets may call each other: a call of the guarded operation is checked where it
        // is made, so what the operation does inside is part of it.
        let guarded = Arc::clone(&targets.functions);
        rules.push(CallOwnerRule {
            label: heki_env::contract_label("call-owners", index, &rule.name),
            targets,
            allowance: Allowance::new(&AllowanceKeys {
                allowed: (&rule.allowed_callers, "call-owners.allowed-callers"),
                forbidden: (&rule.forbidden_callers, "call-owners.forbidden-callers"),
                exempt: (&rule.exempt_methods, "call-owners.exempt-methods"),
            })?
            .with_targets(guarded)
            .with_attribution(attribution(
                rule.attribution.as_deref(),
                "call-owners",
                &rule.name,
            )?)
            .with_own_receiver(allows_own_receiver(
                rule.own_receiver.as_deref(),
                "call-owners",
                &rule.name,
            )?)
            .with_protocols(named_protocols(
                cx,
                &all_classes,
                (&rule.allowed_protocols, rule.protocol_subclasses),
                "call-owners.allowed-protocols",
            )?),
            note: rule.message.as_deref(),
            report_unresolved: reports_unresolved(
                rule.unresolved.as_deref(),
                "call-owners",
                &rule.name,
            )?,
            references: counts_references(rule.references.as_deref(), "call-owners", &rule.name)?,
        });
    }
    super::unresolved::mut207(cx, &rules, out);
    if !calls {
        return Ok(());
    }
    let sites = cx.sites_by_caller();
    let references = if rules.iter().any(|r| r.references != References::Ignored) {
        cx.reference_sites_by_caller()
    } else {
        SitesByCaller::default()
    };
    let mut callers: Vec<&FuncKey> = sites.keys().chain(references.keys()).copied().collect();
    callers.sort();
    callers.dedup();
    let contracts = ContractCalls::new(rules.iter().map(|r| &r.targets))?;
    let mut reports = MergedReports::default();
    let mut families: Vec<Families> = rules.iter().map(|_| Families::default()).collect();
    for key in callers {
        let caller = key.fqn();
        let mut access_groups = AccessGroups::new();
        let calls = sites.get(key).into_iter().flatten().map(|s| (s, false));
        let handed = references.get(key).into_iter().flatten().map(|s| (s, true));
        for (&(path, site), reference) in calls.chain(handed) {
            let called = contracts.called(site).into_iter();
            for (c, groups) in
                called.filter(|(c, _)| !reference || rules[*c].references.counts(site))
            {
                let rule = &rules[c];
                if through_allowed_protocol(cx, &rule.allowance, site) {
                    continue;
                }
                for group in groups {
                    let (target, called) = group_texts(&rule.targets, &group, Some(site));
                    let (family_id, family, inside) =
                        families[c].at(cx, &rule.targets, &group, site);
                    let obj = receiver_params(site);
                    let looked_up = looked_up_params(cx, site);
                    let walk = || {
                        delegated_reports(
                            cx,
                            &sites,
                            (&rule.allowance, &family),
                            (key, obj, looked_up),
                            inside,
                        )
                    };
                    let add = |reports: &mut MergedReports<'c, usize>,
                               r: &DelegatedReport,
                               sites| {
                        let Some(unit) = cx.index.file_of(&r.func).and_then(|p| cx.unit(p)) else {
                            return;
                        };
                        let name = Named {
                            target: &target,
                            named: &called,
                            rank: group[0],
                            contract: c,
                        };
                        reports.add(
                            unit,
                            &r.func,
                            r.span(site.span),
                            (c, name),
                            sites,
                            rule.note,
                            || {
                                let what = call_verb(site, reference);
                                let via = if r.chain.is_empty() {
                                    String::new()
                                } else {
                                    format!(" via `{caller}`")
                                };
                                format!(
                                    "`{}` {what} {called}{via} {}",
                                    r.func.qualname,
                                    rule.allowance.where_text(
                                        "call owners",
                                        "forbidden callers",
                                        &rule.label
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
                    };
                    let access = (obj, looked_up, c, family_id, target.clone(), called.clone());
                    let site_id = (caller.clone(), site.span);
                    access_groups.report(&mut reports, access, site_id, walk, add);
                }
            }
        }
    }
    reports.emit(cx, "MUT204", out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::split_arguments;
    use heki_effect::ArgValue;

    #[test]
    fn an_argument_list_names_constants_by_parameter() {
        let text = |t: &str| ArgValue::Str(t.to_owned());
        for (target, name, arguments) in [
            ("pkg.DataType", "pkg.DataType", None),
            ("pkg:*", "pkg:*", None),
            (
                "pkg.DataType(is_dict=True)",
                "pkg.DataType",
                Some(vec![("is_dict", ArgValue::Bool(true))]),
            ),
            (
                "f( a = False , b=None,c=-3, d=42 )",
                "f",
                Some(vec![
                    ("a", ArgValue::Bool(false)),
                    ("b", ArgValue::None),
                    ("c", ArgValue::Int(-3)),
                    ("d", ArgValue::Int(42)),
                ]),
            ),
            (
                r#"f(kind="a, b=(c)", other='say "x"')"#,
                "f",
                Some(vec![
                    ("kind", text("a, b=(c)")),
                    ("other", text(r#"say "x""#)),
                ]),
            ),
            (
                r#"pkg.*_flags(_kind="")"#,
                "pkg.*_flags",
                Some(vec![("_kind", text(""))]),
            ),
        ] {
            let expected = arguments.map(|list| {
                list.into_iter()
                    .map(|(n, v)| (n.to_owned(), v))
                    .collect::<Vec<_>>()
            });
            assert_eq!(
                split_arguments(target, "call-owners.targets").unwrap(),
                (name, expected),
                "{target}"
            );
        }
    }
}
