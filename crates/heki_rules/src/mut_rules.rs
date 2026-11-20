//! Family 1 rules: `MUT101`, `MUT106`, `MUT107`, `MUT108`, `MUT109`, `MUT110`, `MUT201`,
//! `MUT202`, `MUT203`, `MUT204` (docs/ANALYSIS_AND_KB_SPEC.md section 2.3).

use crate::{CheckContext, DottedPattern, ModuleUnit, RuleConfigError, SitesByCaller};
use heki_ast::Span;
use heki_diagnostics::Diagnostic;
use heki_effect::readonly::{self, Derivation};
use heki_effect::{
    AttrKeyPath, BoundCallee, CONTAINER_MUTATORS, CallHop, CallSite, Callee, EffectSummary, Facts,
    FuncKey, INITIALIZERS, ProjectDecls, ProjectIndex, ReturnStep, Shared, TypeCtx, Val, WriteKind,
    self_aliases,
};
use heki_env::HekiConfig;
use heki_ir::{CompareOpKind, Inst, InstKind, IrFunction, Terminator, VarId};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

// ==================================================================================== MUT101

/// Filter 1: initialization / two-phase initialization methods.
const INIT_METHODS: &[&str] = &[
    "__init__",
    "__post_init__",
    "model_post_init",
    "__setstate__",
    "__enter__",
    "__new__",
    "setup",
    "setUp",
    "postinit",
    "_open",
    "_setup",
];

/// A method of a project class. Methods of the checked files carry their IR (only they are
/// reported); the methods of the other indexed files take part in their class families through
/// their summaries.
struct Method<'a> {
    key: FuncKey,
    /// The file defining it.
    path: &'a Path,
    checked: Option<Checked<'a>>,
}

/// A method of a checked file: its unit and IR.
#[derive(Clone, Copy)]
struct Checked<'a> {
    unit: &'a ModuleUnit<'a>,
    f: &'a IrFunction,
}

impl Method<'_> {
    /// The method's own name (the last component of its qualname).
    fn name(&self) -> &str {
        self.key
            .qualname
            .rsplit('.')
            .next()
            .unwrap_or(&self.key.qualname)
    }
}

/// A private function or method name (`_name`, not a dunder), or a synthetic scope
/// (`<listcomp>`, `<genexpr>`, `<lambda>`, ...): neither is an API of its own, so either hands
/// its reports to its callers.
fn is_private(name: &str) -> bool {
    heki_effect::delegates_to_callers(name)
}

/// Visit every project class whose family has checked methods with its own methods, its
/// family (its methods plus those of every project subclass), and the private methods its
/// family's `__init__`s call on `self`. Classes are keyed by owner class fqn, so nested classes
/// stay separate. A family spans modules: what a checked method is reported for does not depend
/// on which other files are checked with it (the language server checks one).
fn for_each_class_family<'a>(
    cx: &CheckContext<'a>,
    mut visit: impl FnMut(&[Method<'a>], &[&Method<'a>], &FxHashSet<FuncKey>),
) {
    let mut by_class: BTreeMap<String, Vec<Method<'a>>> = BTreeMap::new();
    for unit in cx.units {
        for f in &unit.input.ir.functions {
            if f.is_class_body || f.is_module_body {
                continue;
            }
            let Some(owner) = &f.owner_class_qualname else {
                continue;
            };
            by_class
                .entry(format!("{}.{owner}", unit.input.module_name))
                .or_default()
                .push(Method {
                    key: CheckContext::key(unit, f),
                    path: &unit.input.path,
                    checked: Some(Checked { unit, f }),
                });
        }
    }
    let mut others: Vec<(&FuncKey, &Path, &str)> = Vec::new();
    for (path, file) in &cx.index.files {
        if cx.unit(path).is_some() {
            continue;
        }
        others.extend(file.local_functions.iter().filter_map(|(key, s)| {
            let owner = s.owner_class_qualname.as_deref()?;
            (cx.index.file_of(key) == Some(path.as_path())).then_some((key, path.as_path(), owner))
        }));
    }
    others.sort();
    for (key, path, owner) in others {
        by_class
            .entry(format!("{}.{owner}", key.module))
            .or_default()
            .push(Method {
                key: key.clone(),
                path,
                checked: None,
            });
    }
    for (class, methods) in &by_class {
        let mut family: Vec<&Method<'a>> = methods.iter().collect();
        for sub in cx.decls.subclasses_of(class) {
            family.extend(by_class.get(&sub).into_iter().flatten());
        }
        if family.iter().all(|m| m.checked.is_none()) {
            continue;
        }
        let init_called = private_methods_called_from_init(cx, &family);
        visit(methods, &family, &init_called);
    }
}

/// The method's own (local) summary: one write is one report, where it happens, so the
/// writes merged up from the helpers it calls do not count.
fn local_summary<'a>(cx: &CheckContext<'a>, m: &Method<'_>) -> Option<&'a EffectSummary> {
    cx.index
        .files
        .get(m.path)
        .and_then(|f| f.local_functions.get(&m.key))
}

/// Memo / cache dictionaries by name (`self._cache`, `self.memo`).
fn is_memo_attr(attr: &str) -> bool {
    let lower = attr.to_ascii_lowercase();
    lower.contains("cache") || lower.contains("memo")
}

/// Disjoint constant dict keys: every read and every write names a constant key, and no key
/// is both read and written (`self.params["outtmpl"]` vs `self.params["proxy"]`).
fn disjoint_keys<'k>(
    reads: impl IntoIterator<Item = &'k Option<String>>,
    writes: impl IntoIterator<Item = &'k Option<String>>,
) -> bool {
    let k_read: Option<BTreeSet<&String>> = reads.into_iter().map(Option::as_ref).collect();
    let k_write: Option<BTreeSet<&String>> = writes.into_iter().map(Option::as_ref).collect();
    matches!((k_read, k_write), (Some(kr), Some(kw)) if kr.is_disjoint(&kw))
}

pub(crate) fn mut101(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.selection.is_enabled("MUT101") {
        return;
    }
    let mut seen = FxHashSet::default();
    for_each_class_family(cx, |methods, family, init_called| {
        for p in methods {
            let Some(ps) = cx.index.summary(&p.key) else {
                continue;
            };
            if !ps.is_cached_property {
                continue;
            }
            let p_name = p.name();
            let mut reads: BTreeMap<&str, Vec<&Option<String>>> = BTreeMap::new();
            for r in &ps.reads_self_attrs {
                if r.attr != p_name {
                    reads.entry(&r.attr).or_default().push(&r.const_key);
                }
            }
            for m in family {
                // Only checked methods are reported.
                let Some(Checked { unit, f }) = m.checked else {
                    continue;
                };
                if m.key == p.key || filter1_init(m, init_called) {
                    continue;
                }
                let Some(ms) = cx.index.summary(&m.key) else {
                    continue;
                };
                // Filter 4: the method invalidates the cache.
                if ms.invalidates_cached.contains(p_name) || ms.invalidates_cached.contains("*") {
                    continue;
                }
                let Some(local) = local_summary(cx, m) else {
                    continue;
                };
                // A private helper whose every caller invalidates the property is covered.
                let covered_by_callers = is_private(m.name())
                    && cx
                        .index
                        .reverse_call_graph
                        .get(&m.key)
                        .is_some_and(|callers| {
                            !callers.is_empty()
                                && callers.iter().all(|c| {
                                    cx.index.summary(c).is_some_and(|cs| {
                                        cs.invalidates_cached.contains(p_name)
                                            || cs.invalidates_cached.contains("*")
                                    })
                                })
                        });
                if covered_by_callers {
                    continue;
                }
                for (attr, read_keys) in &reads {
                    let writes: Vec<&AttrKeyPath> = local
                        .mutates_self_attrs
                        .iter()
                        .filter(|w| w.attr == *attr)
                        .collect();
                    if writes.is_empty() {
                        continue;
                    }
                    // Filter 2: memo / cache dictionaries.
                    if is_memo_attr(attr) {
                        continue;
                    }
                    let mut own = self_attr_writes(f, attr);
                    // The cached value holds the attribute's object itself: it sees the changes
                    // made in place, and only rebinding the attribute leaves it behind.
                    if ps.returns_self_attrs.contains(*attr) {
                        own.retain(|w| w.rebinds);
                    }
                    if own.is_empty() || own.iter().all(|w| w.memo_shaped) {
                        continue;
                    }
                    // Filter 3: disjoint constant dict keys.
                    if disjoint_keys(
                        read_keys.iter().copied(),
                        writes.iter().map(|w| &w.const_key),
                    ) {
                        continue;
                    }
                    // An overriding `@cached_property` shares the instance `__dict__` slot:
                    // one report per property name.
                    if !seen.insert((m.key.clone(), attr.to_string(), p_name.to_owned())) {
                        continue;
                    }
                    let span = own[0].span;
                    let class_qualname = ps.owner_class_qualname.as_deref().unwrap_or_default();
                    cx.emit(
                        out,
                        "MUT101",
                        unit,
                        span,
                        &m.key.qualname,
                        format!("{class_qualname}.{attr}->{p_name}"),
                        format!(
                            "`{}` mutates `self.{attr}`, read by `@cached_property` `{p_name}`, without invalidating it (`del self.{p_name}`)",
                            m.key.qualname
                        ),
                        &[],
                    );
                }
            }
        }
    });
}

fn filter1_init(m: &Method<'_>, init_called: &FxHashSet<FuncKey>) -> bool {
    let name = m.name();
    INIT_METHODS.contains(&name)
        || name.starts_with("_init_")
        || name.starts_with("_configure_")
        || (is_private(name) && init_called.contains(&m.key))
}

/// The project classes whose methods' `MUT101` / `MUT107` reports read the summaries of the
/// methods `changed` (a class family spans modules, see [`for_each_class_family`]): a changed
/// `@cached_property` or `__hash__` (or a removed method) is read for its own family, a changed
/// initialization method for the families of its ancestors that have either. A changed writer
/// is reported in its own file only.
pub fn family_readers(
    index: &ProjectIndex,
    decls: &ProjectDecls,
    changed: &[FuncKey],
) -> BTreeSet<String> {
    let has_reported_reads = |class: &String| {
        decls.methods_of(class).iter().any(|k| {
            k.qualname.ends_with(".__hash__")
                || index.summary(k).is_some_and(|s| s.is_cached_property)
        })
    };
    let methods = changed.iter().filter_map(|key| {
        let (owner, name) = key.qualname.rsplit_once('.')?;
        let class = format!("{}.{owner}", key.module);
        decls.is_project_class(&class).then_some((key, class, name))
    });
    let mut roots = BTreeSet::new();
    for (key, class, name) in methods {
        if name == "__hash__" || index.summary(key).is_none_or(|s| s.is_cached_property) {
            roots.insert(class.clone());
        }
        if INIT_METHODS.contains(&name) {
            roots.extend(
                decls
                    .ancestors_of(&class)
                    .into_iter()
                    .chain([class])
                    .filter(has_reported_reads),
            );
        }
    }
    let mut out = BTreeSet::new();
    for root in roots {
        out.extend(decls.subclasses_of(&root));
        out.insert(root);
    }
    out
}

/// Private methods `__init__` of the class family calls on `self` directly.
fn private_methods_called_from_init(
    cx: &CheckContext<'_>,
    family: &[&Method<'_>],
) -> FxHashSet<FuncKey> {
    let mut out = FxHashSet::default();
    for init in family.iter().filter(|m| m.name() == "__init__") {
        let Some(file) = cx.index.files.get(init.path) else {
            continue;
        };
        for site in file
            .call_sites
            .iter()
            .filter(|s| s.caller == init.key && s.recv_is_self)
        {
            out.extend(
                site.project_callees()
                    .iter()
                    .filter(|c| is_private(&c.callee.qualname))
                    .map(|c| c.callee.clone()),
            );
        }
    }
    out
}

struct SelfWrite {
    span: Span,
    block: heki_ir::BlockId,
    /// `setdefault`, or a subscript store guarded by `if k not in self.<attr>:`.
    memo_shaped: bool,
    /// `self.<attr> = ...` / `del self.<attr>`: the attribute is rebound, not changed in place.
    rebinds: bool,
}

/// Writes to `self.<attr>` in `f`'s own body (none without a `self` parameter).
fn self_attr_writes(f: &IrFunction, attr: &str) -> Vec<SelfWrite> {
    let self_vars = f
        .params
        .first()
        .map(|p| self_aliases(f, p.var))
        .unwrap_or_default();
    let mut alias: FxHashSet<VarId> = FxHashSet::default();
    let mut defs: FxHashMap<VarId, &InstKind> = FxHashMap::default();
    for inst in f.blocks.iter().flat_map(|b| &b.insts) {
        if let Some(d) = inst.kind.dst() {
            defs.insert(d, &inst.kind);
        }
        if let InstKind::AttrLoad { dst, obj, attr: a } = &inst.kind
            && self_vars.contains(obj)
            && a == attr
        {
            alias.insert(*dst);
        }
    }
    loop {
        let before = alias.len();
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::Copy { dst, src } = &inst.kind
                && alias.contains(src)
            {
                alias.insert(*dst);
            }
        }
        if alias.len() == before {
            break;
        }
    }
    let guarded = |bb: heki_ir::BlockId| {
        f.blocks.iter().any(|p| match &p.terminator {
            Terminator::Branch { cond, then_bb, .. } if *then_bb == bb => matches!(
                defs.get(cond),
                Some(InstKind::Compare { op: CompareOpKind::NotIn, rhs, .. }) if alias.contains(rhs)
            ),
            _ => false,
        })
    };
    let mut out = Vec::new();
    for bb in &f.blocks {
        for inst in &bb.insts {
            // `(memo_shaped, rebinds)`.
            let w = match &inst.kind {
                InstKind::AttrStore { obj, attr: a, .. }
                | InstKind::AttrDelete { obj, attr: a }
                    if self_vars.contains(obj) && a == attr =>
                {
                    Some((false, true))
                }
                InstKind::SubscriptStore { obj, .. } if alias.contains(obj) => {
                    Some((guarded(bb.id), false))
                }
                InstKind::SubscriptDelete { obj, .. } if alias.contains(obj) => {
                    Some((false, false))
                }
                InstKind::MethodCall { recv, method, .. }
                    if alias.contains(recv) && CONTAINER_MUTATORS.contains(&method.as_str()) =>
                {
                    Some((method == "setdefault", false))
                }
                _ => None,
            };
            if let Some((memo_shaped, rebinds)) = w {
                out.push(SelfWrite {
                    span: inst.span,
                    block: bb.id,
                    memo_shaped,
                    rebinds,
                });
            }
        }
    }
    out.sort_by_key(|w| w.span);
    out
}

// ==================================================================================== MUT201

/// A `Class.attr` contract target (`write-owners`, `read-owners`, `effect-boundaries`).
pub(crate) struct ContractTarget {
    /// The configured text (diagnostic target and baseline key).
    pub text: String,
    attr: DottedPattern,
    /// Project classes (fqn) the class pattern names, plus all their subclasses: the
    /// target's class family.
    pub classes: FxHashSet<String>,
}

impl ContractTarget {
    pub fn matches(&self, class: &str, attr: &str) -> bool {
        self.classes.contains(class) && self.attr.matches(attr)
    }
}

/// Every project class (fqn), sorted.
pub(crate) fn project_classes(cx: &CheckContext<'_>) -> Vec<String> {
    let mut all: Vec<String> = cx
        .index
        .files
        .values()
        .flat_map(|f| {
            f.decls
                .classes
                .keys()
                .map(move |q| format!("{}.{q}", f.module_name))
        })
        .collect();
    all.sort();
    all
}

fn class_set(cx: &CheckContext<'_>, pattern: &DottedPattern, all: &[String]) -> FxHashSet<String> {
    let mut out = FxHashSet::default();
    for c in all.iter().filter(|c| pattern.matches(c)) {
        out.insert(c.clone());
        out.extend(cx.decls.subclasses_of(c));
    }
    out
}

/// Parse `Class.attr` targets: a target without a dot is `InvalidTarget { section }`, a bad
/// glob `Glob { context }`.
pub(crate) fn parse_targets(
    cx: &CheckContext<'_>,
    all: &[String],
    targets: &[String],
    section: &'static str,
    context: &'static str,
) -> Result<Vec<ContractTarget>, RuleConfigError> {
    let mut out = Vec::new();
    for t in targets {
        let Some((class, attr)) = t.rsplit_once('.') else {
            return Err(RuleConfigError::InvalidTarget {
                section,
                target: t.clone(),
            });
        };
        let class = DottedPattern::new(class, context)?;
        out.push(ContractTarget {
            text: t.clone(),
            attr: DottedPattern::new(attr, context)?,
            classes: class_set(cx, &class, all),
        });
    }
    Ok(out)
}

pub(crate) fn patterns(
    texts: &[String],
    context: &'static str,
) -> Result<Vec<DottedPattern>, RuleConfigError> {
    texts
        .iter()
        .map(|p| DottedPattern::new(p, context))
        .collect()
}

/// The functions a contract lets touch its targets: `allowed-*` patterns (matched against the
/// fqn and the qualname) and `exempt-methods` (a plain name matches the last qualname
/// component; an entry with a dot or a glob is a pattern like `allowed-*`). `forbidden-*`
/// patterns (matched like `allowed-*`) narrow where the contract applies: when set, only
/// accesses by them are reported, and the allowed functions stay exceptions inside them.
#[derive(Default)]
pub(crate) struct Allowance {
    allowed: Vec<DottedPattern>,
    exempt_names: FxHashSet<String>,
    forbidden: Vec<DottedPattern>,
    /// Functions the contract guards (`call-owners` targets): they are part of the guarded
    /// operation, whose calls are checked where they are made, so they may use each other.
    targets: Vec<FunctionMatcher>,
}

/// Project functions a `call-owners` / `call-targets` entry names: the whole text by fqn or
/// qualname (dotted suffixes, globs), and, for `Class.method`, the overrides of the method in
/// the class family the class part names.
pub(crate) struct FunctionMatcher {
    whole: DottedPattern,
    method: Option<(FxHashSet<String>, DottedPattern)>,
}

impl FunctionMatcher {
    pub(crate) fn new(
        cx: &CheckContext<'_>,
        all_classes: &[String],
        text: &str,
        context: &'static str,
    ) -> Result<Self, RuleConfigError> {
        let mut method = None;
        if let Some((class, name)) = text.rsplit_once('.') {
            let owners = class_set(cx, &DottedPattern::new(class, context)?, all_classes);
            if !owners.is_empty() {
                method = Some((owners, DottedPattern::new(name, context)?));
            }
        }
        Ok(Self {
            whole: DottedPattern::new(text, context)?,
            method,
        })
    }

    pub(crate) fn matches(&self, key: &FuncKey) -> bool {
        self.whole.matches(&key.fqn())
            || self.whole.matches(&key.qualname)
            || self.method.as_ref().is_some_and(|(family, name)| {
                key.qualname.rsplit_once('.').is_some_and(|(owner, last)| {
                    name.matches(last) && family.contains(&format!("{}.{owner}", key.module))
                })
            })
    }

    /// The class family of `Class.method` (empty for a function).
    pub(crate) fn method_family(&self) -> Option<&FxHashSet<String>> {
        self.method.as_ref().map(|(family, _)| family)
    }

    /// Every project class the whole text names, plus their subclasses.
    pub(crate) fn classes(
        &self,
        cx: &CheckContext<'_>,
        all_classes: &[String],
    ) -> FxHashSet<String> {
        class_set(cx, &self.whole, all_classes)
    }
}

/// The `allowed-*`, `forbidden-*`, and `exempt-methods` keys of one owner contract with their
/// configuration contexts (`write-owners.allowed-writers`, ...).
pub(crate) struct AllowanceKeys<'c> {
    pub allowed: (&'c [String], &'static str),
    pub forbidden: (&'c [String], &'static str),
    pub exempt: (&'c [String], &'static str),
}

impl Allowance {
    pub(crate) fn new(keys: &AllowanceKeys<'_>) -> Result<Self, RuleConfigError> {
        let mut out = Self {
            allowed: patterns(keys.allowed.0, keys.allowed.1)?,
            exempt_names: FxHashSet::default(),
            forbidden: patterns(keys.forbidden.0, keys.forbidden.1)?,
            targets: Vec::new(),
        };
        for e in keys.exempt.0 {
            if e.contains(['.', '*', '?', '[', '{']) {
                out.allowed.push(DottedPattern::new(e, keys.exempt.1)?);
            } else {
                out.exempt_names.insert(e.clone());
            }
        }
        Ok(out)
    }

    pub(crate) fn allows(&self, key: &FuncKey) -> bool {
        let name = key.qualname.rsplit('.').next().unwrap_or(&key.qualname);
        if self.exempt_names.contains(name) {
            return true;
        }
        let fqn = key.fqn();
        self.allowed
            .iter()
            .any(|p| p.matches(&fqn) || p.matches(&key.qualname))
            || self.targets.iter().any(|t| t.matches(key))
    }

    /// The same allowance, with the guarded functions `targets` allowed too.
    pub(crate) fn with_targets(self, targets: Vec<FunctionMatcher>) -> Self {
        Self { targets, ..self }
    }

    /// Whether the contract has `forbidden-*` patterns.
    pub(crate) fn has_forbidden(&self) -> bool {
        !self.forbidden.is_empty()
    }

    /// Whether an access by `key` falls under the contract: every function without
    /// `forbidden-*`, else the functions they name.
    pub(crate) fn in_scope(&self, key: &FuncKey) -> bool {
        if self.forbidden.is_empty() {
            return true;
        }
        let fqn = key.fqn();
        self.forbidden
            .iter()
            .any(|p| p.matches(&fqn) || p.matches(&key.qualname))
    }

    /// `outside the <owners> of `name``, or `inside the forbidden <who> of `name`` for a contract
    /// with `forbidden-*`.
    pub(crate) fn where_text(&self, owners: &str, forbidden: &str, name: &str) -> String {
        if self.has_forbidden() {
            format!("inside the {forbidden} of `{name}`")
        } else {
            format!("outside the {owners} of `{name}`")
        }
    }
}

/// A private helper delegates responsibility to its project callers: the call sites that run
/// its own body (`EffectSummary::body_facts`) and so receive its writes and reads. A private
/// `async def` only scheduled (never awaited) or a private generator whose generators all
/// escape keeps its reports.
fn delegates(cx: &CheckContext<'_>, sites: &SitesByCaller<'_>, key: &FuncKey) -> bool {
    is_private(&key.qualname)
        && cx
            .index
            .summary(key)
            .is_some_and(|s| !callers_of(cx, sites, key, s.body_facts()).is_empty())
}

/// One project call site that runs the facts `facts` of a function: the caller, the site and
/// the binding of the function there, and which of the caller's facts receive them (its body,
/// or what iterating the object it returns runs).
struct Caller<'a> {
    caller: &'a FuncKey,
    path: &'a Path,
    site: &'a CallSite,
    bc: &'a BoundCallee,
    into: Facts,
    /// The project function that iterates the function's result there (`SiteRole::Through`).
    via: Option<&'a FuncKey>,
}

/// The call sites running `facts` of `callee` ([`heki_effect::ProjectIndex::delivery`]), by
/// caller and in source order. A generator caller's body is its iteration (`Facts::Result`).
fn callers_of<'a>(
    cx: &CheckContext<'a>,
    sites: &SitesByCaller<'a>,
    callee: &FuncKey,
    facts: Facts,
) -> Vec<Caller<'a>> {
    let summary = cx.index.summary(callee);
    let mut callers: Vec<&FuncKey> = cx
        .index
        .reverse_call_graph
        .get(callee)
        .into_iter()
        .flatten()
        .collect();
    callers.sort();
    let mut out = Vec::new();
    for caller in callers {
        let caller_body = cx
            .index
            .summary(caller)
            .map_or(Facts::Body, EffectSummary::body_facts);
        for &(path, site) in sites.get(caller).into_iter().flatten() {
            for bc in site
                .project_callees()
                .iter()
                .filter(|bc| bc.callee == *callee)
            {
                let Some(d) = summary
                    .and_then(|s| cx.index.delivery(site, s))
                    .filter(|d| d.from == facts)
                else {
                    continue;
                };
                let into = match d.into {
                    Facts::Body => caller_body,
                    Facts::Result => Facts::Result,
                };
                out.push(Caller {
                    caller: &site.caller,
                    path,
                    site,
                    bc,
                    into,
                    via: d.via,
                });
            }
        }
    }
    out
}

/// A class's own initialization: `key` is an initializer (`INITIALIZERS`) of a class of the
/// target's `family`, and the accessed object may be its own instance (`obj_params` bit 0,
/// `self`): only an object that is provably not `self` (another parameter, a fresh instance,
/// an attribute) stays reported.
pub(crate) fn initializes_itself(
    cx: &CheckContext<'_>,
    key: &FuncKey,
    family: &FxHashSet<String>,
    obj_params: u64,
) -> bool {
    let name = key.qualname.rsplit('.').next().unwrap_or(&key.qualname);
    obj_params & 1 != 0
        && INITIALIZERS.contains(&name)
        && cx
            .index
            .summary(key)
            .and_then(|s| s.owner_class_qualname.as_deref())
            .is_some_and(|owner| family.contains(&format!("{}.{owner}", key.module)))
}

/// Whether `func` (about to be reported for an access to an object that may be its parameters
/// `obj`) only runs inside allowed contexts: it has project callers, and every path up the
/// reverse call graph reaches an allowed or exempt function, or an initializer of the target
/// family on its own `self`, before a function without callers. Such a function is part of the
/// owner's operations, like a hook that a store operation runs. The walk goes breadth first over
/// `(function, facts, object parameters)` states along the call sites that run those facts; a
/// function returning a generator is no context of the generator's body (it does not run it), so
/// the walk goes on to where the object it returns is iterated. Paths that only cycle add no
/// allowed context, so a function reached through cycles alone keeps its report. Initializers
/// end the walk negatively: constructor calls also bind the initializers of subclasses (CHA), so
/// their callers are not known exactly.
fn runs_inside_allowed(
    cx: &CheckContext<'_>,
    sites: &SitesByCaller<'_>,
    allowance: &Allowance,
    family: &FxHashSet<String>,
    func: &FuncKey,
    obj: u64,
) -> bool {
    let is_initializer =
        |k: &FuncKey| INITIALIZERS.contains(&k.qualname.rsplit('.').next().unwrap_or(&k.qualname));
    if is_initializer(func) {
        return false;
    }
    let facts = cx
        .index
        .summary(func)
        .map_or(Facts::Body, EffectSummary::body_facts);
    let mut queue = std::collections::VecDeque::from([(func.clone(), facts, obj)]);
    let mut visited: FxHashSet<(FuncKey, Facts, u64)> = FxHashSet::default();
    visited.insert((func.clone(), facts, obj));
    let mut allowed_contexts = false;
    while let Some((callee, facts, obj)) = queue.pop_front() {
        let callers = callers_of(cx, sites, &callee, facts);
        if callers.is_empty() {
            // A function without project callers, outside every allowed context.
            return false;
        }
        for c in callers {
            let caller_obj = c.bc.caller_params_of(obj);
            // A function returning the object does not run what iterating it runs.
            if c.into == Facts::Result {
                if visited.insert((c.caller.clone(), Facts::Result, caller_obj)) {
                    queue.push_back((c.caller.clone(), Facts::Result, caller_obj));
                }
                continue;
            }
            if allowance.allows(c.caller)
                || c.via.is_some_and(|p| allowance.allows(p))
                || initializes_itself(cx, c.caller, family, caller_obj)
            {
                allowed_contexts = true;
            } else if is_initializer(c.caller) {
                return false;
            } else if visited.insert((c.caller.clone(), c.into, caller_obj)) {
                queue.push_back((c.caller.clone(), c.into, caller_obj));
            }
        }
    }
    allowed_contexts
}

/// Whether `func`'s own summary holds the write `site` (as its caller sees it): the same write
/// at the same location, by `func` itself (`own`) or through its callees. A function iterating a
/// generator it is handed does not: the generator's writes reach the function that handed it
/// over, and only that one.
fn holds_write(
    cx: &CheckContext<'_>,
    func: &FuncKey,
    site: &heki_effect::TypedFieldWriteSite,
    own: bool,
) -> bool {
    let origin = site.via_chain.last().map(|h| h.callee.as_str());
    cx.index.summary(func).is_some_and(|s| {
        s.typed_field_write_sites.iter().any(|t| {
            t.class_qualname == site.class_qualname
                && t.attr == site.attr
                && t.kind == site.kind
                && t.span == site.span
                && if own {
                    t.via_chain.is_empty()
                } else {
                    t.via_chain.last().map(|h| h.callee.as_str()) == origin
                }
        })
    })
}

/// `base`, then `: note` when the contract sets a `message`.
pub(crate) fn with_note(base: String, note: Option<&str>) -> String {
    match note {
        Some(n) => format!("{base}: {n}"),
        None => base,
    }
}

/// One violation site: the function holding the write, read, or call (fqn) and its span there.
pub(crate) type SiteId = (String, Span);

/// The reports of a contract rule, merged by `(reporting function, span, target)`: the first
/// report of a key gives the message and the related chain, and every distinct violation site
/// behind the key counts (`Diagnostic::sites`), so a second write inside a private helper that is
/// reported at its caller is visible to the Ratchet baseline.
#[derive(Default)]
pub(crate) struct MergedReports<'u> {
    reports: BTreeMap<(FuncKey, Span, String), MergedReport<'u>>,
}

struct MergedReport<'u> {
    unit: &'u ModuleUnit<'u>,
    message: String,
    chain: Vec<CallHop>,
    sites: BTreeSet<SiteId>,
}

impl<'u> MergedReports<'u> {
    /// Add a report of `func` at `span` for `target`, standing for `site`; `message` and
    /// `chain` are only built for the first report of the key.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add(
        &mut self,
        unit: &'u ModuleUnit<'u>,
        func: &FuncKey,
        span: Span,
        target: &str,
        site: SiteId,
        message: impl FnOnce() -> String,
        chain: impl FnOnce() -> Vec<CallHop>,
    ) {
        self.reports
            .entry((func.clone(), span, target.to_owned()))
            .or_insert_with(|| MergedReport {
                unit,
                message: message(),
                chain: chain(),
                sites: BTreeSet::new(),
            })
            .sites
            .insert(site);
    }

    pub(crate) fn emit(self, cx: &CheckContext<'_>, code: &'static str, out: &mut Vec<Diagnostic>) {
        for ((func, span, target), r) in self.reports {
            let sites = u32::try_from(r.sites.len()).unwrap_or(u32::MAX);
            if let Some(d) = cx.emit(
                out,
                code,
                r.unit,
                span,
                &func.qualname,
                target,
                r.message,
                &r.chain,
            ) {
                d.sites = sites;
            }
        }
    }
}

struct OwnerRule<'r> {
    name: &'r str,
    targets: Vec<ContractTarget>,
    allowance: Allowance,
    forbid_cached: FxHashSet<String>,
    note: Option<&'r str>,
}

pub(crate) fn mut201(
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) -> Result<(), RuleConfigError> {
    if !cx.selection.is_enabled("MUT201") || cx.config.write_owners.is_empty() {
        return Ok(());
    }
    let all_classes = project_classes(cx);
    let mut rules = Vec::new();
    for rule in &cx.config.write_owners {
        let targets = parse_targets(
            cx,
            &all_classes,
            &rule.targets,
            "write-owners",
            "write-owners.targets",
        )?;
        let allowance = Allowance::new(&AllowanceKeys {
            allowed: (&rule.allowed_writers, "write-owners.allowed-writers"),
            forbidden: (&rule.forbidden_writers, "write-owners.forbidden-writers"),
            exempt: (&rule.exempt_methods, "write-owners.exempt-methods"),
        })?;
        let mut forbid_cached = FxHashSet::default();
        for p in &rule.forbid_cached_property_on {
            let p = DottedPattern::new(p, "write-owners.forbid-cached-property-on")?;
            forbid_cached.extend(class_set(cx, &p, &all_classes));
        }
        rules.push(OwnerRule {
            name: &rule.name,
            targets,
            allowance,
            forbid_cached,
            note: rule.message.as_deref(),
        });
    }

    let mut keys_by_fqn: FxHashMap<String, FuncKey> = FxHashMap::default();
    for file in cx.index.files.values() {
        for k in file.local_functions.keys() {
            keys_by_fqn.insert(k.fqn(), k.clone());
        }
    }

    let mut paths: Vec<_> = cx.index.files.keys().collect();
    paths.sort();
    let mut reports = MergedReports::default();
    let mut sites = None;
    for path in paths {
        let file = &cx.index.files[path];
        let Some(unit) = cx.unit(path) else {
            continue;
        };
        let mut funcs: Vec<_> = file.functions.iter().collect();
        funcs.sort_by(|a, b| a.0.cmp(b.0));
        for (key, s) in funcs {
            for site in &s.typed_field_write_sites {
                for rule in &rules {
                    for t in &rule.targets {
                        if !t.matches(&site.class_qualname, &site.attr) {
                            continue;
                        }
                        let mut chain: Vec<FuncKey> = vec![key.clone()];
                        chain.extend(
                            site.via_chain
                                .iter()
                                .filter_map(|h| keys_by_fqn.get(&h.callee).cloned()),
                        );
                        // Allowed writers and exempt methods anywhere on the chain; with
                        // `forbidden-writers`, a forbidden function must be on it.
                        if chain.iter().any(|k| rule.allowance.allows(k))
                            || !chain.iter().any(|k| rule.allowance.in_scope(k))
                        {
                            continue;
                        }
                        let origin = if site.kind == WriteKind::IndirectHelper && chain.len() >= 2 {
                            chain.len() - 2
                        } else {
                            chain.len() - 1
                        };
                        // Report at the outermost function: helpers in between must be private
                        // (they delegate) unless they do not hold the write themselves (a function
                        // iterating a generator it is handed runs the generator's writes without
                        // knowing them), and a delegating function leaves it to its callers.
                        // No initializer name is private, so on a reported chain only the
                        // outermost function can be the initializer of the written object.
                        let sites = sites.get_or_insert_with(|| cx.sites_by_caller());
                        let last = chain.len() - 1;
                        if !chain[1..=origin].iter().enumerate().all(|(i, k)| {
                            is_private(&k.qualname) || !holds_write(cx, k, site, i + 1 == last)
                        }) || delegates(cx, sites, key)
                            || initializes_itself(cx, key, &t.classes, site.obj_params)
                        {
                            continue;
                        }
                        // A function that only runs inside the owner's operations (every caller
                        // path starts in an allowed context) writes inside them.
                        if runs_inside_allowed(
                            cx,
                            sites,
                            &rule.allowance,
                            &t.classes,
                            key,
                            site.obj_params,
                        ) {
                            continue;
                        }
                        let span = site.via_chain.first().map_or(site.span, |h| h.span);
                        let writer = site
                            .via_chain
                            .last()
                            .map_or_else(|| key.fqn(), |h| h.callee.clone());
                        reports.add(
                            unit,
                            key,
                            span,
                            &t.text,
                            (writer, site.span),
                            || {
                                let via = site
                                    .via_chain
                                    .last()
                                    .map(|h| format!(" via `{}`", h.callee))
                                    .unwrap_or_default();
                                with_note(
                                    format!(
                                        "`{}` writes `{}` ({}){via} {}",
                                        key.qualname,
                                        t.text,
                                        site.kind.as_str(),
                                        rule.allowance.where_text(
                                            "write owners",
                                            "forbidden writers",
                                            rule.name
                                        )
                                    ),
                                    rule.note,
                                )
                            },
                            || site.via_chain.clone(),
                        );
                    }
                }
            }
        }
    }
    reports.emit(cx, "MUT201", out);

    // `forbid-cached-property-on`: a `@cached_property` defined on a guarded class.
    for unit in cx.units {
        for f in &unit.input.ir.functions {
            let Some(owner) = &f.owner_class_qualname else {
                continue;
            };
            if f.is_class_body || !f.has_decorator(&["cached_property"]) {
                continue;
            }
            let class = format!("{}.{owner}", unit.input.module_name);
            for rule in rules.iter().filter(|r| r.forbid_cached.contains(&class)) {
                cx.emit(
                    out,
                    "MUT201",
                    unit,
                    f.span,
                    &f.qualname,
                    format!("{owner}.{}", f.name),
                    with_note(
                        format!(
                            "`@cached_property` `{}` is defined on `{owner}`, which `{}` forbids caching on",
                            f.name, rule.name
                        ),
                        rule.note,
                    ),
                    &[],
                );
            }
        }
    }
    Ok(())
}

// ==================================================================================== MUT202

/// `readonly-returns` producers from the configuration.
#[derive(Debug)]
pub(crate) struct ReadOnlyProducers {
    rules: Vec<(DottedPattern, bool)>,
}

impl ReadOnlyProducers {
    pub fn new(config: &HekiConfig) -> Result<Self, RuleConfigError> {
        let mut rules = Vec::new();
        for r in &config.readonly_returns {
            for p in &r.producers {
                rules.push((DottedPattern::new(p, "readonly-returns.producers")?, r.deep));
            }
        }
        Ok(Self { rules })
    }

    pub(crate) fn deep_for(&self, fqn: &str) -> Option<bool> {
        let mut found = None;
        for (p, deep) in &self.rules {
            if p.matches(fqn) {
                found = Some(found.unwrap_or(false) || *deep);
            }
        }
        found
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// What `MUT202` reads besides the function it checks: the declared producers, and the names of
/// the functions whose call yields a protected value as a property getter could (one returning
/// a protected value, an `@lru_cache` function, or a declared property). Reading a property is a
/// call of its getter; only these names are resolved. The names come from the summaries, so a
/// check reads no other module's declarations for them.
pub(crate) struct ReadOnlyReturns {
    producers: ReadOnlyProducers,
    getters: FxHashSet<String>,
}

impl ReadOnlyReturns {
    pub fn new(cx: &CheckContext<'_>) -> Result<Self, RuleConfigError> {
        let producers = ReadOnlyProducers::new(cx.config)?;
        let property = |s: &EffectSummary| {
            s.decorators.iter().any(|d| {
                let last = d.rsplit('.').next().unwrap_or(d);
                matches!(last, "property" | "cached_property")
            })
        };
        let getters = cx
            .index
            .files
            .values()
            .flat_map(|f| &f.functions)
            .filter(|(k, s)| {
                s.returns_protected.is_some()
                    || s.is_lru_cache
                    || property(s) && producers.deep_for(&k.fqn()).is_some()
            })
            .map(|(k, _)| {
                k.qualname
                    .rsplit('.')
                    .next()
                    .unwrap_or(&k.qualname)
                    .to_owned()
            })
            .collect();
        Ok(Self { producers, getters })
    }
}

/// Read-only origin of a value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ro {
    producer: String,
    deep: bool,
    /// A shallow copy of a deep read-only value: its own top level may change, its
    /// elements / attributes are still shared.
    shallow_copy: bool,
    /// How the function got the value from the producer: the call of a function that returns
    /// it (a wrapper), then the wrapper's calls down to the producer (empty: a direct call).
    via: Vec<CallHop>,
}

impl Ro {
    fn new(producer: String, shared: Shared, via: Vec<CallHop>) -> Option<Self> {
        shared.any().then_some(Self {
            producer,
            deep: shared.elements,
            shallow_copy: !shared.object,
            via,
        })
    }

    fn shared(&self) -> Shared {
        Shared {
            object: !self.shallow_copy,
            elements: self.deep,
        }
    }

    /// The origin of a value derived from this one by `step`.
    fn derive(&self, step: ReturnStep) -> Option<Self> {
        Self::new(
            self.producer.clone(),
            step.apply(self.shared()),
            self.via.clone(),
        )
    }
}

/// One call instruction with its resolved callee.
struct CallParts<'i> {
    /// `recv.method(...)`: the receiver and the method name.
    recv: Option<(VarId, &'i str)>,
    args: &'i [VarId],
    kwargs: &'i [(Option<String>, VarId)],
    callee: Callee,
}

const MUTABLE_BUILTINS: &[&str] = &[
    "builtins.list",
    "builtins.dict",
    "builtins.set",
    "builtins.bytearray",
];

pub(crate) fn mut202(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    returns: &ReadOnlyReturns,
    out: &mut Vec<Diagnostic>,
) {
    if !cx.enabled("MUT202", unit) {
        return;
    }
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    for f in &unit.input.ir.functions {
        if f.is_class_body {
            continue;
        }
        Mut202Fn::new(cx, unit, &tcx, returns, f).run(out);
    }
}

struct Mut202Fn<'a> {
    cx: &'a CheckContext<'a>,
    unit: &'a ModuleUnit<'a>,
    tcx: &'a TypeCtx<'a>,
    returns: &'a ReadOnlyReturns,
    f: &'a IrFunction,
    key: FuncKey,
    types: FxHashMap<VarId, Val>,
    ro: FxHashMap<VarId, Ro>,
}

impl<'a> Mut202Fn<'a> {
    fn new(
        cx: &'a CheckContext<'a>,
        unit: &'a ModuleUnit<'a>,
        tcx: &'a TypeCtx<'a>,
        returns: &'a ReadOnlyReturns,
        f: &'a IrFunction,
    ) -> Self {
        Self {
            cx,
            unit,
            tcx,
            returns,
            f,
            key: CheckContext::key(unit, f),
            types: tcx.infer(f),
            ro: FxHashMap::default(),
        }
    }

    /// The parts of a call instruction (`None` for any other instruction).
    fn call_of<'i>(&self, kind: &'i InstKind) -> Option<CallParts<'i>> {
        match kind {
            InstKind::Call {
                callee_var,
                args,
                kwargs,
                ..
            } => Some(CallParts {
                recv: None,
                args,
                kwargs,
                callee: self
                    .tcx
                    .resolve_call(callee_var.and_then(|v| self.types.get(&v))),
            }),
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                ..
            } => Some(CallParts {
                recv: Some((*recv, method)),
                args,
                kwargs,
                callee: self.tcx.resolve_method(self.types.get(recv), method),
            }),
            _ => None,
        }
    }

    /// The read-only origin of a call's (or a property read's) result: a declared producer, an
    /// `@lru_cache` function, or a project function that returns one's value (its summary's
    /// `returns_protected`), reached through the call at `span`.
    fn producer(&self, callee: &Callee, span: Span) -> Option<Ro> {
        let direct =
            |producer: String, deep: bool| Ro::new(producer, Shared::produced(deep), vec![]);
        match callee {
            Callee::Project(keys, _) => keys.iter().find_map(|k| {
                let fqn = k.fqn();
                let summary = self.cx.index.summary(k);
                if summary.is_some_and(|s| s.is_lru_cache) {
                    return direct(fqn, true);
                }
                if let Some(d) = self.returns.producers.deep_for(&fqn) {
                    return direct(fqn, d);
                }
                let r = summary?.returns_protected.as_ref()?;
                let mut via = vec![CallHop {
                    caller: self.key.clone(),
                    callee: fqn,
                    span,
                    path: self.unit.input.path.clone(),
                }];
                via.extend(r.chain.iter().cloned());
                Ro::new(r.producer.clone(), r.shared, via)
            }),
            Callee::External(sym) => self
                .returns
                .producers
                .deep_for(sym)
                .and_then(|d| direct(sym.clone(), d)),
            Callee::TooMany | Callee::Unresolved => None,
        }
    }

    fn kwarg_true(&self, kwargs: &[(Option<String>, VarId)], name: &str) -> bool {
        kwargs
            .iter()
            .any(|(k, v)| k.as_deref() == Some(name) && self.f.const_bool(*v) == Some(true))
    }

    /// Value produced by one instruction from read-only operands (the derivations of
    /// `heki_effect::readonly`, which the return flow applies backward).
    fn transfer(&self, inst: &Inst) -> Option<Ro> {
        match &inst.kind {
            InstKind::Copy { src, .. } => self.ro.get(src).cloned(),
            InstKind::SubscriptLoad { obj, index, .. } => {
                let step = if heki_effect::consumption::is_unpacking(self.f, *index, inst.span) {
                    ReturnStep::TARGET_ITEM
                } else {
                    ReturnStep::ELEMENT
                };
                self.ro.get(obj)?.derive(step)
            }
            InstKind::AttrLoad { obj, attr, .. } => {
                if let Some(r) = self.ro.get(obj) {
                    return r.derive(ReturnStep::ELEMENT);
                }
                // `readonly-returns` may name a property: `obj.schema`.
                if let Some(Val::Inst(class)) = self.types.get(obj) {
                    let fqn = format!("{class}.{attr}");
                    if let Some(deep) = self.returns.producers.deep_for(&fqn) {
                        return Ro::new(fqn, Shared::produced(deep), vec![]);
                    }
                }
                // A property getter that returns a read-only producer's value.
                if !self.returns.getters.contains(attr.as_str()) {
                    return None;
                }
                let getter = self.tcx.resolve_attr_load(self.types.get(obj), attr)?;
                self.producer(&getter, inst.span)
            }
            InstKind::MakeIter { iterable, .. } => self.ro.get(iterable).cloned(),
            InstKind::IterNext { iter, .. } => self.ro.get(iter)?.derive(ReturnStep::ELEMENT),
            kind => {
                let call = self.call_of(kind)?;
                if let Some(r) = self.producer(&call.callee, inst.span) {
                    return Some(r);
                }
                let (operand, method) = match call.recv {
                    Some((recv, method)) if self.ro.contains_key(&recv) => {
                        (Some(recv), Some(method))
                    }
                    _ => (
                        call.args
                            .first()
                            .copied()
                            .filter(|a| self.ro.contains_key(a)),
                        None,
                    ),
                };
                let r = self.ro.get(&operand?)?;
                let symbol = match &call.callee {
                    Callee::External(s) => s.as_str(),
                    _ => "",
                };
                let deep = self.kwarg_true(call.kwargs, "deep");
                match readonly::call_derivation(method, symbol, self.cx.kb, deep)? {
                    Derivation::DeepCopy => None,
                    Derivation::Step(step) => r.derive(step),
                }
            }
        }
    }

    fn propagate(&mut self) {
        let rpo = self.f.rpo_blocks();
        for _ in 0..4 {
            let before = self.ro.len();
            for &bb in &rpo {
                let block = self.f.block(bb);
                for phi in &block.phis {
                    if self.ro.contains_key(&phi.dst) {
                        continue;
                    }
                    if let Some(r) = phi
                        .incoming
                        .iter()
                        .find_map(|(_, v)| self.ro.get(v))
                        .cloned()
                    {
                        self.ro.insert(phi.dst, r);
                    }
                }
                for inst in &block.insts {
                    let Some(dst) = inst.kind.dst() else {
                        continue;
                    };
                    if self.ro.contains_key(&dst) {
                        continue;
                    }
                    if let Some(r) = self.transfer(inst) {
                        self.ro.insert(dst, r);
                    }
                }
            }
            if self.ro.len() == before {
                break;
            }
        }
    }

    /// Read-only origin of `var` when mutating its own top level is a violation.
    fn guarded(&self, var: VarId) -> Option<&Ro> {
        self.ro.get(&var).filter(|r| !r.shallow_copy)
    }

    fn run(mut self, out: &mut Vec<Diagnostic>) {
        self.propagate();
        if self.ro.is_empty() {
            return;
        }
        let mut reported: BTreeSet<Span> = BTreeSet::new();
        for bb in &self.f.blocks {
            for inst in &bb.insts {
                let (hit, chain) = self.violation(inst);
                if let Some(r) = hit
                    && reported.insert(inst.span)
                {
                    // A value from a wrapper: the wrapper called here, then the hops down to the
                    // producer, then the hop to the mutating helper.
                    let through = r
                        .via
                        .first()
                        .map(|h| format!(" through `{}`", h.callee))
                        .unwrap_or_default();
                    let mut related = r.via.clone();
                    related.extend(chain);
                    self.cx.emit(
                        out,
                        "MUT202",
                        self.unit,
                        inst.span,
                        &self.key.qualname,
                        r.producer.clone(),
                        format!(
                            "`{}` mutates the shared object returned by read-only producer `{}`{through}",
                            self.key.qualname, r.producer
                        ),
                        &related,
                    );
                }
            }
        }
    }

    fn violation(&self, inst: &Inst) -> (Option<Ro>, Vec<CallHop>) {
        let direct = match &inst.kind {
            InstKind::AttrStore { obj, .. }
            | InstKind::AttrDelete { obj, .. }
            | InstKind::SubscriptStore { obj, .. }
            | InstKind::SubscriptDelete { obj, .. } => self.guarded(*obj),
            InstKind::MethodCall { recv, method, .. }
                if CONTAINER_MUTATORS.contains(&method.as_str()) =>
            {
                self.guarded(*recv)
            }
            InstKind::BinOp {
                lhs, in_place: true, ..
            } => self.guarded(*lhs).filter(|_| {
                matches!(self.types.get(lhs), Some(Val::Inst(t)) if MUTABLE_BUILTINS.contains(&t.as_str()))
            }),
            _ => None,
        };
        if direct.is_some() {
            return (direct.cloned(), Vec::new());
        }
        // Passing a read-only value to a project function that mutates that parameter.
        let Some(CallParts {
            recv,
            args,
            kwargs,
            callee: Callee::Project(keys, binds_recv),
        }) = self.call_of(&inst.kind)
        else {
            return (None, Vec::new());
        };
        let recv = recv.map(|(var, _)| var);
        for (key, callee) in self.summarized(&keys) {
            let decl = self
                .cx
                .decls
                .function_decl(key)
                .cloned()
                .unwrap_or_default();
            let offset = usize::from(binds_recv && !decl.is_staticmethod);
            let mut bound: Vec<(usize, VarId)> = Vec::new();
            if offset == 1
                && !decl.is_classmethod
                && let Some(r) = recv
            {
                bound.push((0, r));
            }
            bound.extend(args.iter().enumerate().map(|(i, v)| (i + offset, *v)));
            for (name, v) in kwargs {
                if let Some(pos) = name
                    .as_ref()
                    .and_then(|n| decl.param_names.iter().position(|p| p == n))
                {
                    bound.push((pos, *v));
                }
            }
            for (pos, var) in bound {
                let Some(r) = self.ro.get(&var) else {
                    continue;
                };
                // Summaries track parameters 0..64 (`mutated_param_paths` keys are `u8`).
                let paths = u8::try_from(pos)
                    .ok()
                    .and_then(|p| callee.mutated_param_paths.get(&p));
                let mutates = if r.shallow_copy {
                    paths.is_some_and(|p| p.iter().any(|path| !path.is_empty()))
                } else {
                    pos < 64 && callee.mutates_params & (1u64 << pos) != 0
                };
                if mutates {
                    let hop = CallHop {
                        caller: self.key.clone(),
                        callee: key.fqn(),
                        span: inst.span,
                        path: self.unit.input.path.clone(),
                    };
                    return (Some(r.clone()), vec![hop]);
                }
            }
        }
        (None, Vec::new())
    }

    /// The callees the index summarizes. One it does not (an index older than the
    /// declarations the callee was resolved through) is skipped, never assumed.
    fn summarized<'k>(&self, keys: &'k [FuncKey]) -> Vec<(&'k FuncKey, &'a EffectSummary)> {
        let mut out = Vec::new();
        for key in keys {
            let Some(summary) = self.cx.index.summary(key) else {
                continue;
            };
            out.push((key, summary));
        }
        out
    }
}

// ==================================================================================== MUT107

/// `MUT107`: a method of the class (or of a subclass) mutates an attribute `__hash__` reads
/// after initialization, so instances already stored in a `set` or as `dict` keys can no longer
/// be found. Reported at the method's first own write of the attribute, once per method and
/// attribute; the filters are `MUT101`'s (two-phase initialization, memo dictionaries,
/// disjoint constant keys) plus lazy initialization and `__hash__`'s own cache.
pub(crate) fn mut107(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.selection.is_enabled("MUT107") {
        return;
    }
    let mut seen = FxHashSet::default();
    for_each_class_family(cx, |methods, family, init_called| {
        let hashes: Vec<_> = methods
            .iter()
            .filter(|h| h.name() == "__hash__")
            .filter_map(|h| Some((h, cx.index.summary(&h.key)?)))
            .collect();
        if hashes.is_empty() {
            return;
        }
        let init_helpers = private_methods_named_in_initializers(cx, family);
        for (h, hs) in hashes {
            // Filter 5: what the hash computation writes itself (`self._hash = ...`, also
            // through its `self.helper()` calls) is its cache; a write elsewhere resets it.
            let cache: BTreeSet<&str> = hs
                .mutates_self_attrs
                .iter()
                .map(|a| a.attr.as_str())
                .collect();
            let mut reads: BTreeMap<&str, Vec<&Option<String>>> = BTreeMap::new();
            for r in &hs.reads_self_attrs {
                if !cache.contains(r.attr.as_str()) {
                    reads.entry(&r.attr).or_default().push(&r.const_key);
                }
            }
            // `__init_subclass__` / `__class_getitem__` are implicit class methods: they set
            // class attributes, not those of an instance.
            // Only checked methods are reported.
            let writers = family
                .iter()
                .filter_map(|m| Some((m, m.checked?)))
                .filter(|(m, _)| {
                    !IMPLICIT_CLASS_METHODS.contains(&m.name())
                        && m.name() != "__hash__"
                        && !filter1_init(m, init_called)
                        && !init_helpers.contains(&m.key)
                })
                .filter_map(|(m, checked)| Some((m, checked, local_summary(cx, m)?)));
            for (m, Checked { unit, f }, local) in writers {
                for (attr, read_keys) in &reads {
                    let writes: Vec<&AttrKeyPath> = local
                        .mutates_self_attrs
                        .iter()
                        .filter(|w| w.attr == *attr)
                        .collect();
                    // Filters 3 and 4: memo dictionaries and stored hashes (`self._hash`,
                    // which a deserialization hook recomputes), disjoint constant keys.
                    if writes.is_empty()
                        || is_memo_attr(attr)
                        || is_stored_hash(attr)
                        || disjoint_keys(
                            read_keys.iter().copied(),
                            writes.iter().map(|w| &w.const_key),
                        )
                    {
                        continue;
                    }
                    // Filters 2 and 3: lazy initialization, memo-shaped writes.
                    let lazy = lazy_init_branches(f, attr);
                    let first = self_attr_writes(f, attr).into_iter().find(|w| {
                        !w.memo_shaped && !lazy.iter().any(|b| dominates(f, *b, w.block))
                    });
                    let Some(write) = first else {
                        continue;
                    };
                    if !seen.insert((m.key.clone(), attr.to_string())) {
                        continue;
                    }
                    let class_qualname = hs.owner_class_qualname.as_deref().unwrap_or_default();
                    cx.emit(
                        out,
                        "MUT107",
                        unit,
                        write.span,
                        &m.key.qualname,
                        format!("{class_qualname}.{attr}->__hash__"),
                        format!(
                            "`{}` mutates `self.{attr}`, which `{}` reads: instances already stored in a `set` or used as `dict` keys can no longer be found",
                            m.key.qualname, h.key.qualname
                        ),
                        &[],
                    );
                }
            }
        }
    });
}

/// Dunder methods Python makes class methods without a decorator.
const IMPLICIT_CLASS_METHODS: &[&str] = &["__init_subclass__", "__class_getitem__"];

/// A stored hash value (`self._hash`, `self._hashcode`, `self.hash_value`): the hash itself, not
/// a field it is computed from.
fn is_stored_hash(attr: &str) -> bool {
    matches!(
        attr.trim_start_matches('_').to_ascii_lowercase().as_str(),
        "hash" | "hashcode" | "hash_code" | "hashvalue" | "hash_value"
    )
}

/// Filter 1 (`MUT107`) also covers the private methods the family's initialization methods
/// call by name on any receiver: `__new__` sets up the instance it allocates
/// (`self = object.__new__(cls)`, then `self.__setstate(state)`), which is no call on its own
/// `self`. The names come from the summaries (`EffectSummary::private_calls`): an initializer
/// of the family may be in another file.
fn private_methods_named_in_initializers<'m>(
    cx: &CheckContext<'_>,
    family: &[&'m Method<'_>],
) -> FxHashSet<&'m FuncKey> {
    let called: FxHashSet<&str> = family
        .iter()
        .filter(|m| INIT_METHODS.contains(&m.name()))
        .filter_map(|m| cx.index.summary(&m.key))
        .flat_map(|s| s.private_calls.iter().map(String::as_str))
        .collect();
    family
        .iter()
        .filter(|m| called.contains(m.name()))
        .map(|m| &m.key)
        .collect()
}

/// Filter 2 (`MUT107`): the initializing branches of the `None` / falsy guards of
/// `self.<attr>` in `f` (`heki_effect::guards::lazy_init_branches`, which the effect lane's
/// lazy initialization shares).
fn lazy_init_branches(f: &IrFunction, attr: &str) -> Vec<heki_ir::BlockId> {
    let self_vars = f
        .params
        .first()
        .map(|p| self_aliases(f, p.var))
        .unwrap_or_default();
    heki_effect::guards::lazy_init_branches(f)
        .into_iter()
        .filter(|b| b.attr == attr && self_vars.contains(&b.obj))
        .map(|b| b.init)
        .collect()
}

/// Whether every path from the entry of `f` to block `b` runs through block `d` (normal and
/// exception edges).
fn dominates(f: &IrFunction, d: heki_ir::BlockId, b: heki_ir::BlockId) -> bool {
    let mut seen = FxHashSet::default();
    let mut stack = vec![heki_ir::BlockId(0)];
    while let Some(cur) = stack.pop() {
        if cur == d || !seen.insert(cur) {
            continue;
        }
        if cur == b {
            return false;
        }
        let block = f.block(cur);
        stack.extend(block.terminator.successors());
        stack.extend(block.exc_target);
    }
    true
}

// ============================================================================ MUT108 / MUT110

mod readonly_mutation;

pub use readonly_mutation::{CHAIN_RULES, ChainFacts, chain_facts};

/// `MUT108` (in-place changes of frozen dataclass / `NamedTuple` / `dataclass_transform` fields,
/// `Final` / `ReadOnly` attributes, and `ReadOnly` `TypedDict` keys) and `MUT110` (`Sequence` /
/// `Mapping` parameters handed to functions that change them), through one analysis.
pub(crate) fn mut108_mut110(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    readonly_mutation::run(cx, out);
}

// ==================================================================================== MUT109

mod invalidation;

/// `MUT109`: writes to declared `invalidation` targets that some normal path leaves
/// unannounced (no call reaches one of the contract's invalidators).
pub(crate) fn mut109(
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) -> Result<(), RuleConfigError> {
    invalidation::run(cx, out)
}

// ==================================================================================== MUT106

mod iter_consumption;

/// `MUT106` (opt-in): a single-pass iterator or generator consumed a second time.
pub(crate) fn mut106(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    iter_consumption::run(cx, unit, out);
}

// ==================================================================================== MUT203

struct ReadOwnerRule<'r> {
    name: &'r str,
    targets: Vec<ContractTarget>,
    allowance: Allowance,
    note: Option<&'r str>,
}

/// Where one access (a read, a call) is reported.
pub(crate) struct DelegatedReport {
    pub func: FuncKey,
    pub span: Span,
    /// Hops from `func` down to the accessing function (empty: `func` accesses itself).
    pub chain: Vec<CallHop>,
}

/// `MUT203`: reads of `read-owners` targets outside their allowed readers. Read sites are
/// local facts; a read in a private helper with project callers is reported at the call sites
/// of its outermost non-private callers, found by walking the reverse call graph.
pub(crate) fn mut203(
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) -> Result<(), RuleConfigError> {
    if !cx.selection.is_enabled("MUT203") || cx.config.read_owners.is_empty() {
        return Ok(());
    }
    let all_classes = project_classes(cx);
    let mut rules = Vec::new();
    for rule in &cx.config.read_owners {
        rules.push(ReadOwnerRule {
            name: &rule.name,
            targets: parse_targets(
                cx,
                &all_classes,
                &rule.targets,
                "read-owners",
                "read-owners.targets",
            )?,
            allowance: Allowance::new(&AllowanceKeys {
                allowed: (&rule.allowed_readers, "read-owners.allowed-readers"),
                forbidden: (&rule.forbidden_readers, "read-owners.forbidden-readers"),
                exempt: (&rule.exempt_methods, "read-owners.exempt-methods"),
            })?,
            note: rule.message.as_deref(),
        });
    }
    let sites = cx.sites_by_caller();
    let mut reports = MergedReports::default();
    for (key, s) in cx.defined_functions() {
        for read in &s.typed_field_read_sites {
            for rule in &rules {
                for t in rule
                    .targets
                    .iter()
                    .filter(|t| t.matches(&read.class_qualname, &read.attr))
                {
                    let found = delegated_reports(
                        cx,
                        &sites,
                        &rule.allowance,
                        &t.classes,
                        key,
                        read.obj_params,
                        read.span,
                    );
                    for r in found {
                        let Some(unit) = cx.index.file_of(&r.func).and_then(|p| cx.unit(p)) else {
                            continue;
                        };
                        reports.add(
                            unit,
                            &r.func,
                            r.span,
                            &t.text,
                            (key.fqn(), read.span),
                            || {
                                let via = if r.chain.is_empty() {
                                    String::new()
                                } else {
                                    format!(" via `{}`", key.fqn())
                                };
                                with_note(
                                    format!(
                                        "`{}` reads `{}` ({}){via} {}",
                                        r.func.qualname,
                                        t.text,
                                        read.kind.as_str(),
                                        rule.allowance.where_text(
                                            "read owners",
                                            "forbidden readers",
                                            rule.name
                                        )
                                    ),
                                    rule.note,
                                )
                            },
                            || r.chain.clone(),
                        );
                    }
                }
            }
        }
    }
    reports.emit(cx, "MUT203", out);
    Ok(())
}

/// One state of the walk from an access up the reverse call graph.
struct WalkState {
    func: FuncKey,
    /// Which facts of `func` run the access: its body, or what iterating its result runs.
    facts: Facts,
    /// Parameters of `func` the accessed object may be.
    obj: u64,
    /// A function from the accessing one up to `func` that runs the access falls under
    /// `forbidden-*` (`Allowance::in_scope`).
    in_scope: bool,
    /// The state below (towards the accessing function) and the hops from `func` down to it.
    down: Option<(usize, Vec<CallHop>)>,
}

/// Where an access in `func` (at `span`, to an object that may be its parameters `obj`) is
/// reported: at the access itself, or — for a private helper with project callers running it —
/// at every call site of each outermost non-private caller, found breadth first over
/// `(function, facts, object parameters)` states (cycle-safe) along the call sites that run
/// those facts. A function returning a generator that accesses does not run the access: the
/// walk goes on to where the object it returns is iterated. Allowed functions end a path; an
/// initializer accessing its own instance and a function that only runs inside allowed
/// contexts (MUT201's rules) are not reported. With `forbidden-*`, a report needs a forbidden
/// function running the access between the reporting function and the access (both included).
pub(crate) fn delegated_reports(
    cx: &CheckContext<'_>,
    sites: &SitesByCaller<'_>,
    allowance: &Allowance,
    family: &FxHashSet<String>,
    func: &FuncKey,
    obj: u64,
    span: Span,
) -> Vec<DelegatedReport> {
    if allowance.allows(func) {
        return Vec::new();
    }
    let in_scope = allowance.in_scope(func);
    if !delegates(cx, sites, func) {
        if !in_scope
            || initializes_itself(cx, func, family, obj)
            || runs_inside_allowed(cx, sites, allowance, family, func, obj)
        {
            return Vec::new();
        }
        return vec![DelegatedReport {
            func: func.clone(),
            span,
            chain: Vec::new(),
        }];
    }
    let facts = cx
        .index
        .summary(func)
        .map_or(Facts::Body, EffectSummary::body_facts);
    let mut states = vec![WalkState {
        func: func.clone(),
        facts,
        obj,
        in_scope,
        down: None,
    }];
    let mut visited: FxHashSet<(FuncKey, Facts, u64, bool)> = FxHashSet::default();
    visited.insert((func.clone(), facts, obj, in_scope));
    let mut out = Vec::new();
    let mut i = 0;
    while i < states.len() {
        let (callee, facts, obj, below_in_scope) = (
            states[i].func.clone(),
            states[i].facts,
            states[i].obj,
            states[i].in_scope,
        );
        for c in callers_of(cx, sites, &callee, facts) {
            let caller_obj = c.bc.caller_params_of(obj);
            let hop = |from: &FuncKey, to: &FuncKey| CallHop {
                caller: from.clone(),
                callee: to.fqn(),
                span: c.site.span,
                path: c.path.to_path_buf(),
            };
            let hops = match c.via {
                None => vec![hop(c.caller, &callee)],
                Some(p) => vec![hop(c.caller, p), hop(p, &callee)],
            };
            if c.into == Facts::Result {
                // The function returning the object does not run the access.
                let state = (c.caller.clone(), Facts::Result, caller_obj, below_in_scope);
                if visited.insert(state) {
                    states.push(WalkState {
                        func: c.caller.clone(),
                        facts: Facts::Result,
                        obj: caller_obj,
                        in_scope: below_in_scope,
                        down: Some((i, hops)),
                    });
                }
                continue;
            }
            if allowance.allows(c.caller) || c.via.is_some_and(|p| allowance.allows(p)) {
                continue;
            }
            let in_scope = below_in_scope
                || allowance.in_scope(c.caller)
                || c.via.is_some_and(|p| allowance.in_scope(p));
            if delegates(cx, sites, c.caller) {
                if visited.insert((c.caller.clone(), c.into, caller_obj, in_scope)) {
                    states.push(WalkState {
                        func: c.caller.clone(),
                        facts: c.into,
                        obj: caller_obj,
                        in_scope,
                        down: Some((i, hops)),
                    });
                }
            } else if in_scope
                && !initializes_itself(cx, c.caller, family, caller_obj)
                && !runs_inside_allowed(cx, sites, allowance, family, c.caller, caller_obj)
            {
                let mut chain = hops;
                let mut j = i;
                while let Some((below, h)) = &states[j].down {
                    chain.extend(h.iter().cloned());
                    j = *below;
                }
                out.push(DelegatedReport {
                    func: c.caller.clone(),
                    span: c.site.span,
                    chain,
                });
            }
        }
        i += 1;
    }
    out
}

// ==================================================================================== MUT204

mod call_owners;

pub(crate) use call_owners::{CallTargetPattern, mut204, parse_call_targets, site_callee};
