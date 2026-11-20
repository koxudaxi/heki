//! Family 1 rules: `MUT101`, `MUT106`, `MUT107`, `MUT201`, `MUT202`, `MUT203`
//! (docs/DETAILED_SPEC_M3_M7.md section 3.2).

use crate::{CheckContext, DottedPattern, ModuleUnit, RuleConfigError, SitesByCaller};
use heki_ast::Span;
use heki_diagnostics::Diagnostic;
use heki_effect::{
    AttrKeyPath, CONTAINER_MUTATORS, CallHop, Callee, EffectSummary, FuncKey, TypeCtx,
    TypedFieldReadSite, Val, WriteKind, self_aliases,
};
use heki_env::HekiConfig;
use heki_ir::{CompareOpKind, ConstLiteral, Inst, InstKind, IrFunction, Terminator, VarId};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeMap, BTreeSet};

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

struct Method<'a> {
    unit: &'a ModuleUnit<'a>,
    f: &'a IrFunction,
    key: FuncKey,
}

fn is_private(name: &str) -> bool {
    let last = name.rsplit('.').next().unwrap_or(name);
    last.starts_with('_') && !(last.starts_with("__") && last.ends_with("__"))
}

pub(crate) fn mut101(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.selection.is_enabled("MUT101") {
        return;
    }
    // Methods grouped by owner class fqn; nested classes keep their own qualname.
    let mut by_class: BTreeMap<String, Vec<Method<'_>>> = BTreeMap::new();
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
                    unit,
                    f,
                    key: CheckContext::key(unit, f),
                });
        }
    }
    let mut seen = FxHashSet::default();
    for (class, methods) in &by_class {
        let mut family: Vec<&Method<'_>> = methods.iter().collect();
        for sub in cx.decls.subclasses_of(class) {
            family.extend(by_class.get(&sub).into_iter().flatten());
        }
        let init_called = private_methods_called_from_init(cx, &family);
        for p in methods {
            let Some(ps) = cx.index.summary(&p.key) else {
                continue;
            };
            if !ps.is_cached_property {
                continue;
            }
            let mut reads: BTreeMap<&str, Vec<&Option<String>>> = BTreeMap::new();
            for r in &ps.reads_self_attrs {
                if r.attr != p.f.name {
                    reads.entry(&r.attr).or_default().push(&r.const_key);
                }
            }
            for m in &family {
                if m.key == p.key || filter1_init(m, &init_called) {
                    continue;
                }
                let Some(ms) = cx.index.summary(&m.key) else {
                    continue;
                };
                // Filter 4: the method invalidates the cache.
                if ms.invalidates_cached.contains(&p.f.name) || ms.invalidates_cached.contains("*")
                {
                    continue;
                }
                // One write is one report, where it happens: only the method's own writes
                // count (its local summary), not those merged up from the helpers it calls.
                let Some(local) = cx
                    .index
                    .files
                    .get(&m.unit.input.path)
                    .and_then(|f| f.local_functions.get(&m.key))
                else {
                    continue;
                };
                // A private helper whose every caller invalidates the property is covered.
                let covered_by_callers = is_private(&m.f.name)
                    && cx
                        .index
                        .reverse_call_graph
                        .get(&m.key)
                        .is_some_and(|callers| {
                            !callers.is_empty()
                                && callers.iter().all(|c| {
                                    cx.index.summary(c).is_some_and(|cs| {
                                        cs.invalidates_cached.contains(&p.f.name)
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
                    let lower = attr.to_ascii_lowercase();
                    if lower.contains("cache") || lower.contains("memo") {
                        continue;
                    }
                    let own = self_attr_writes(m.f, attr);
                    if own.is_empty() || own.iter().all(|w| w.memo_shaped) {
                        continue;
                    }
                    // Filter 3: disjoint constant dict keys.
                    let k_read: Option<BTreeSet<&String>> =
                        read_keys.iter().map(|k| k.as_ref()).collect();
                    let k_write: Option<BTreeSet<&String>> =
                        writes.iter().map(|w| w.const_key.as_ref()).collect();
                    if let (Some(kr), Some(kw)) = (&k_read, &k_write)
                        && kr.is_disjoint(kw)
                    {
                        continue;
                    }
                    // An overriding `@cached_property` shares the instance `__dict__` slot:
                    // one report per property name.
                    if !seen.insert((m.key.clone(), attr.to_string(), p.f.name.clone())) {
                        continue;
                    }
                    let span = own[0].span;
                    let class_qualname = p.f.owner_class_qualname.as_deref().unwrap_or_default();
                    cx.emit(
                        out,
                        "MUT101",
                        m.unit,
                        span,
                        &m.key.qualname,
                        format!("{class_qualname}.{attr}->{}", p.f.name),
                        format!(
                            "`{}` mutates `self.{attr}`, read by `@cached_property` `{}`, without invalidating it (`del self.{}`)",
                            m.key.qualname, p.f.name, p.f.name
                        ),
                        &[],
                    );
                }
            }
        }
    }
}

fn filter1_init(m: &Method<'_>, init_called: &FxHashSet<FuncKey>) -> bool {
    let name = m.f.name.as_str();
    INIT_METHODS.contains(&name)
        || name.starts_with("_init_")
        || name.starts_with("_configure_")
        || (is_private(name) && init_called.contains(&m.key))
}

/// Private methods `__init__` of the class family calls on `self` directly.
fn private_methods_called_from_init(
    cx: &CheckContext<'_>,
    family: &[&Method<'_>],
) -> FxHashSet<FuncKey> {
    let mut out = FxHashSet::default();
    for init in family.iter().filter(|m| m.f.name == "__init__") {
        let Some(file) = cx.index.files.get(&init.unit.input.path) else {
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
    /// `setdefault`, or a subscript store guarded by `if k not in self.<attr>:`.
    memo_shaped: bool,
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
            let w = match &inst.kind {
                InstKind::AttrStore { obj, attr: a, .. }
                | InstKind::AttrDelete { obj, attr: a }
                    if self_vars.contains(obj) && a == attr =>
                {
                    Some(false)
                }
                InstKind::SubscriptStore { obj, .. } if alias.contains(obj) => Some(guarded(bb.id)),
                InstKind::SubscriptDelete { obj, .. } if alias.contains(obj) => Some(false),
                InstKind::MethodCall { recv, method, .. }
                    if alias.contains(recv) && CONTAINER_MUTATORS.contains(&method.as_str()) =>
                {
                    Some(method == "setdefault")
                }
                _ => None,
            };
            if let Some(memo_shaped) = w {
                out.push(SelfWrite {
                    span: inst.span,
                    memo_shaped,
                });
            }
        }
    }
    out.sort_by_key(|w| w.span);
    out
}

// ==================================================================================== MUT201

const INITIALIZERS: &[&str] = &[
    "__init__",
    "__post_init__",
    "__new__",
    "model_post_init",
    "__setstate__",
];

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
/// component; an entry with a dot or a glob is a pattern like `allowed-*`).
struct Allowance {
    allowed: Vec<DottedPattern>,
    exempt_names: FxHashSet<String>,
}

impl Allowance {
    fn new(
        allowed: &[String],
        allowed_context: &'static str,
        exempt: &[String],
        exempt_context: &'static str,
    ) -> Result<Self, RuleConfigError> {
        let mut out = Self {
            allowed: patterns(allowed, allowed_context)?,
            exempt_names: FxHashSet::default(),
        };
        for e in exempt {
            if e.contains(['.', '*', '?', '[', '{']) {
                out.allowed.push(DottedPattern::new(e, exempt_context)?);
            } else {
                out.exempt_names.insert(e.clone());
            }
        }
        Ok(out)
    }

    fn allows(&self, key: &FuncKey) -> bool {
        let name = key.qualname.rsplit('.').next().unwrap_or(&key.qualname);
        if self.exempt_names.contains(name) {
            return true;
        }
        let fqn = key.fqn();
        self.allowed
            .iter()
            .any(|p| p.matches(&fqn) || p.matches(&key.qualname))
    }
}

/// A private helper with project callers delegates responsibility to its callers.
fn delegates(cx: &CheckContext<'_>, key: &FuncKey) -> bool {
    is_private(&key.qualname)
        && cx
            .index
            .reverse_call_graph
            .get(key)
            .is_some_and(|c| !c.is_empty())
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

/// `base`, then `: note` when the contract sets a `message`.
pub(crate) fn with_note(base: String, note: Option<&str>) -> String {
    match note {
        Some(n) => format!("{base}: {n}"),
        None => base,
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
        let allowance = Allowance::new(
            &rule.allowed_writers,
            "write-owners.allowed-writers",
            &rule.exempt_methods,
            "write-owners.exempt-methods",
        )?;
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
    let mut seen = FxHashSet::default();
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
                        // Allowed writers and exempt methods anywhere on the chain.
                        if chain.iter().any(|k| rule.allowance.allows(k)) {
                            continue;
                        }
                        let origin = if site.kind == WriteKind::IndirectHelper && chain.len() >= 2 {
                            chain.len() - 2
                        } else {
                            chain.len() - 1
                        };
                        // Report at the outermost function: helpers in between must be private
                        // (they delegate), and a delegating function leaves it to its callers.
                        // No initializer name is private, so on a reported chain only the
                        // outermost function can be the initializer of the written object.
                        if !chain[1..=origin].iter().all(|k| is_private(&k.qualname))
                            || delegates(cx, key)
                            || initializes_itself(cx, key, &t.classes, site.obj_params)
                        {
                            continue;
                        }
                        let span = site.via_chain.first().map_or(site.span, |h| h.span);
                        if !seen.insert((key.clone(), span, t.text.clone())) {
                            continue;
                        }
                        let via = site
                            .via_chain
                            .last()
                            .map(|h| format!(" via `{}`", h.callee))
                            .unwrap_or_default();
                        cx.emit(
                            out,
                            "MUT201",
                            unit,
                            span,
                            &key.qualname,
                            t.text.clone(),
                            with_note(
                                format!(
                                    "`{}` writes `{}` ({}){via} outside the write owners of `{}`",
                                    key.qualname,
                                    t.text,
                                    site.kind.as_str(),
                                    rule.name
                                ),
                                rule.note,
                            ),
                            &site.via_chain,
                        );
                    }
                }
            }
        }
    }

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

    fn deep_for(&self, fqn: &str) -> Option<bool> {
        let mut found = None;
        for (p, deep) in &self.rules {
            if p.matches(fqn) {
                found = Some(found.unwrap_or(false) || *deep);
            }
        }
        found
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
}

/// One call instruction with its resolved callee.
struct CallParts<'i> {
    /// `recv.method(...)`: the receiver and the method name.
    recv: Option<(VarId, &'i str)>,
    args: &'i [VarId],
    kwargs: &'i [(Option<String>, VarId)],
    callee: Callee,
}

/// Methods returning (views of) a container's elements.
const ELEMENT_ACCESSORS: &[&str] = &["get", "__getitem__", "values", "items"];
/// Methods returning a copy of their receiver.
const COPY_METHODS: &[&str] = &["copy", "model_copy", "__copy__", "__deepcopy__"];
const MUTABLE_BUILTINS: &[&str] = &[
    "builtins.list",
    "builtins.dict",
    "builtins.set",
    "builtins.bytearray",
];

pub(crate) fn mut202(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    producers: &ReadOnlyProducers,
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
        Mut202Fn::new(cx, unit, &tcx, producers, f).run(out);
    }
}

struct Mut202Fn<'a> {
    cx: &'a CheckContext<'a>,
    unit: &'a ModuleUnit<'a>,
    tcx: &'a TypeCtx<'a>,
    producers: &'a ReadOnlyProducers,
    f: &'a IrFunction,
    key: FuncKey,
    types: FxHashMap<VarId, Val>,
    defs: FxHashMap<VarId, &'a Inst>,
    ro: FxHashMap<VarId, Ro>,
}

impl<'a> Mut202Fn<'a> {
    fn new(
        cx: &'a CheckContext<'a>,
        unit: &'a ModuleUnit<'a>,
        tcx: &'a TypeCtx<'a>,
        producers: &'a ReadOnlyProducers,
        f: &'a IrFunction,
    ) -> Self {
        let mut defs = FxHashMap::default();
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let Some(d) = inst.kind.dst() {
                defs.insert(d, inst);
            }
        }
        Self {
            cx,
            unit,
            tcx,
            producers,
            f,
            key: CheckContext::key(unit, f),
            types: tcx.infer(f),
            defs,
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

    fn producer(&self, callee: &Callee) -> Option<Ro> {
        let make = |producer: String, deep: bool| Ro {
            producer,
            deep,
            shallow_copy: false,
        };
        match callee {
            Callee::Project(keys, _) => keys.iter().find_map(|k| {
                let fqn = k.fqn();
                if self.cx.index.summary(k).is_some_and(|s| s.is_lru_cache) {
                    return Some(make(fqn, true));
                }
                self.producers.deep_for(&fqn).map(|d| make(fqn, d))
            }),
            Callee::External(sym) => self.producers.deep_for(sym).map(|d| make(sym.clone(), d)),
            Callee::TooMany | Callee::Unresolved => None,
        }
    }

    fn kwarg_true(&self, kwargs: &[(Option<String>, VarId)], name: &str) -> bool {
        kwargs
            .iter()
            .any(|(k, v)| k.as_deref() == Some(name) && self.f.const_bool(*v) == Some(true))
    }

    /// Value produced by one instruction from read-only operands.
    fn transfer(&self, inst: &Inst) -> Option<Ro> {
        let elem = |r: &Ro| {
            (r.deep || r.shallow_copy).then(|| Ro {
                producer: r.producer.clone(),
                deep: true,
                shallow_copy: false,
            })
        };
        match &inst.kind {
            InstKind::Copy { src, .. } => self.ro.get(src).cloned(),
            InstKind::SubscriptLoad { obj, index, .. } => {
                let r = self.ro.get(obj)?;
                // Tuple / list unpacking (`a, b = producer()`) lowers to a constant-index
                // load spanning the target itself.
                let unpack = matches!(
                    self.defs.get(index),
                    Some(Inst {
                        kind: InstKind::Const {
                            literal: ConstLiteral::Int(_),
                            ..
                        },
                        span,
                    }) if *span == inst.span
                );
                if unpack && !r.shallow_copy {
                    return Some(Ro {
                        shallow_copy: false,
                        ..r.clone()
                    });
                }
                elem(r)
            }
            InstKind::AttrLoad { obj, attr, .. } => {
                if let Some(r) = self.ro.get(obj) {
                    return elem(r);
                }
                // `readonly-returns` may name a property: `obj.schema`.
                if let Some(Val::Inst(class)) = self.types.get(obj) {
                    let fqn = format!("{class}.{attr}");
                    return self.producers.deep_for(&fqn).map(|deep| Ro {
                        producer: fqn,
                        deep,
                        shallow_copy: false,
                    });
                }
                None
            }
            InstKind::MakeIter { iterable, .. } => self.ro.get(iterable).cloned(),
            InstKind::IterNext { iter, .. } => self.ro.get(iter).and_then(elem),
            kind => {
                let call = self.call_of(kind)?;
                if let Some(r) = self.producer(&call.callee) {
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
                let sanitizer = self.cx.kb.lookup(symbol).is_some_and(|e| e.copy_sanitizer)
                    || method.is_some_and(|m| COPY_METHODS.contains(&m));
                if sanitizer {
                    let deep_copy = symbol.ends_with("deepcopy")
                        || method == Some("__deepcopy__")
                        || (method == Some("model_copy") || symbol.ends_with(".model_copy"))
                            && self.kwarg_true(call.kwargs, "deep");
                    if deep_copy || !(r.deep || r.shallow_copy) {
                        return None;
                    }
                    return Some(Ro {
                        producer: r.producer.clone(),
                        deep: true,
                        shallow_copy: true,
                    });
                }
                match method {
                    Some(m) if ELEMENT_ACCESSORS.contains(&m) => elem(r),
                    _ => None,
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
                    self.cx.emit(
                        out,
                        "MUT202",
                        self.unit,
                        inst.span,
                        &self.key.qualname,
                        r.producer.clone(),
                        format!(
                            "`{}` mutates the shared object returned by read-only producer `{}`",
                            self.key.qualname, r.producer
                        ),
                        &chain,
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

/// `MUT107`: attributes read by `__hash__` mutated after initialization (scaffold).
pub(crate) fn mut107(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    let _ = (cx, out);
}

// ==================================================================================== MUT106

/// `MUT106`: a single-pass iterator consumed twice (scaffold).
pub(crate) fn mut106(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    let _ = (cx, unit, out);
}

// ==================================================================================== MUT203

struct ReadOwnerRule<'r> {
    name: &'r str,
    targets: Vec<ContractTarget>,
    allowance: Allowance,
    note: Option<&'r str>,
}

/// Where one read is reported.
struct ReadReport {
    func: FuncKey,
    span: Span,
    /// Hops from `func` down to the reading function (empty: `func` reads itself).
    chain: Vec<CallHop>,
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
            allowance: Allowance::new(
                &rule.allowed_readers,
                "read-owners.allowed-readers",
                &rule.exempt_methods,
                "read-owners.exempt-methods",
            )?,
            note: rule.message.as_deref(),
        });
    }
    let sites = cx.sites_by_caller();
    // Every function once, from its defining file.
    let mut funcs: Vec<(&FuncKey, &EffectSummary)> = cx
        .index
        .files
        .iter()
        .flat_map(|(path, f)| {
            f.functions
                .iter()
                .filter(move |(k, _)| cx.index.file_of(k) == Some(path.as_path()))
        })
        .collect();
    funcs.sort_by(|a, b| a.0.cmp(b.0));
    let mut seen = FxHashSet::default();
    for (key, s) in funcs {
        for read in &s.typed_field_read_sites {
            for rule in &rules {
                for t in rule
                    .targets
                    .iter()
                    .filter(|t| t.matches(&read.class_qualname, &read.attr))
                {
                    for r in read_reports(cx, &sites, &rule.allowance, &t.classes, key, read) {
                        if !seen.insert((r.func.clone(), r.span, t.text.clone())) {
                            continue;
                        }
                        let Some(unit) = cx.index.file_of(&r.func).and_then(|p| cx.unit(p)) else {
                            continue;
                        };
                        let via = if r.chain.is_empty() {
                            String::new()
                        } else {
                            format!(" via `{}`", key.fqn())
                        };
                        cx.emit(
                            out,
                            "MUT203",
                            unit,
                            r.span,
                            &r.func.qualname,
                            t.text.clone(),
                            with_note(
                                format!(
                                    "`{}` reads `{}` ({}){via} outside the read owners of `{}`",
                                    r.func.qualname,
                                    t.text,
                                    read.kind.as_str(),
                                    rule.name
                                ),
                                rule.note,
                            ),
                            &r.chain,
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

/// One state of the walk from a read up the reverse call graph.
struct WalkState {
    func: FuncKey,
    /// Parameters of `func` the read object may be.
    obj: u64,
    /// The state below (towards the reader) and the hop from `func` down to it.
    down: Option<(usize, CallHop)>,
}

/// Where a read in `reader` is reported: at the read itself, or — for a private helper with
/// project callers — at every call site of each outermost non-private caller, found breadth
/// first over `(function, read object parameters)` states (cycle-safe). Allowed functions end
/// a path; an initializer reading its own instance (MUT201's rule) is not reported.
fn read_reports(
    cx: &CheckContext<'_>,
    sites: &SitesByCaller<'_>,
    allowance: &Allowance,
    family: &FxHashSet<String>,
    reader: &FuncKey,
    read: &TypedFieldReadSite,
) -> Vec<ReadReport> {
    if allowance.allows(reader) {
        return Vec::new();
    }
    if !delegates(cx, reader) {
        if initializes_itself(cx, reader, family, read.obj_params) {
            return Vec::new();
        }
        return vec![ReadReport {
            func: reader.clone(),
            span: read.span,
            chain: Vec::new(),
        }];
    }
    let mut states = vec![WalkState {
        func: reader.clone(),
        obj: read.obj_params,
        down: None,
    }];
    let mut visited: FxHashSet<(FuncKey, u64)> = FxHashSet::default();
    visited.insert((reader.clone(), read.obj_params));
    let mut out = Vec::new();
    let mut i = 0;
    while i < states.len() {
        let (callee, obj) = (states[i].func.clone(), states[i].obj);
        let mut callers: Vec<&FuncKey> = cx
            .index
            .reverse_call_graph
            .get(&callee)
            .into_iter()
            .flatten()
            .filter(|c| !allowance.allows(c))
            .collect();
        callers.sort();
        for caller in callers {
            for (path, site) in sites.get(caller).into_iter().flatten() {
                for bc in site
                    .project_callees()
                    .iter()
                    .filter(|bc| bc.callee == callee)
                {
                    let caller_obj = bc.caller_params_of(obj);
                    let hop = CallHop {
                        caller: caller.clone(),
                        callee: callee.fqn(),
                        span: site.span,
                        path: path.to_path_buf(),
                    };
                    if delegates(cx, caller) {
                        if visited.insert((caller.clone(), caller_obj)) {
                            states.push(WalkState {
                                func: caller.clone(),
                                obj: caller_obj,
                                down: Some((i, hop)),
                            });
                        }
                    } else if !initializes_itself(cx, caller, family, caller_obj) {
                        let mut chain = vec![hop];
                        let mut j = i;
                        while let Some((below, h)) = &states[j].down {
                            chain.push(h.clone());
                            j = *below;
                        }
                        out.push(ReadReport {
                            func: caller.clone(),
                            span: site.span,
                            chain,
                        });
                    }
                }
            }
        }
        i += 1;
    }
    out
}
