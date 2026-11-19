//! Module names, import maps, project declarations, and the 4-priority lightweight receiver
//! type resolution (Advisory lane only; never reports type errors).
//!
//! Priorities (docs/DETAILED_SPEC_M3_M7.md section 2.3):
//! 1. Exact: local construction and direct imports (`LoadGlobal`, `Import`, `AttrLoad` on a
//!    module, calls of classes, `AllocBuiltin`, `Copy`).
//! 2. Typed: PEP 484 parameter / local / return annotations, `self` / `cls`.
//! 3. Typed: `self.attr` types collected from `__init__` / `__post_init__` and annotations.
//! 4. Heuristic: CHA over project subclasses, at most `CHA_CANDIDATE_LIMIT` candidates.

use crate::{FuncKey, ModuleInput};
use heki_ast::Span;
use heki_ir::{BuiltinKind, FunctionKind, Inst, InstKind, IrFunction, ModuleIr, VarId};
use heki_kb::{EffectiveEcosystemKb, ExecEffectKind};
use heki_semantic::{ModuleImport, ScopeKind, SemanticModel};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path};

/// CHA fan-out limit: more candidates than this leave the call unresolved (`unknown_calls`).
pub const CHA_CANDIDATE_LIMIT: usize = 8;

/// Upper bound on re-export alias chasing (`pkg.__init__` re-exporting `pkg.sub.Foo`).
const MAX_ALIAS_CHASE: usize = 8;

const BUILTIN_TYPES: &[&str] = &[
    "list",
    "dict",
    "set",
    "frozenset",
    "tuple",
    "str",
    "bytes",
    "bytearray",
    "int",
    "float",
    "bool",
    "object",
    "memoryview",
    "range",
];

// ------------------------------------------------------------------------------ module names

/// `src/` layout: some `root/src/<pkg>/__init__.py` exists.
pub fn detect_src_layout(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root.join("src")) else {
        return false;
    };
    entries
        .flatten()
        .any(|e| e.path().join("__init__.py").is_file())
}

/// Module name of `rel` (relative to the project root): `pkg/mod.py` -> `pkg.mod`,
/// `pkg/__init__.py` -> `pkg` (package). With `src_layout`, a leading `src/` is stripped.
/// Returns `(module_name, is_package)`.
pub fn module_name_for_path(rel: &Path, src_layout: bool) -> Option<(String, bool)> {
    let mut parts: Vec<String> = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str()?.to_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if src_layout && parts.first().map(String::as_str) == Some("src") {
        parts.remove(0);
    }
    let file = parts.pop()?;
    let stem = file
        .strip_suffix(".py")
        .or_else(|| file.strip_suffix(".pyi"))?;
    let is_package = stem == "__init__";
    if !is_package {
        parts.push(stem.to_owned());
    }
    if parts.is_empty() {
        return None;
    }
    Some((parts.join("."), is_package))
}

/// Resolve the module part of `from <level dots><module> import ...` inside `current`.
/// A package `__init__` is its own base; a plain module's base is its parent package.
pub fn resolve_relative_module(
    current: &str,
    current_is_package: bool,
    level: u32,
    module: Option<&str>,
) -> Option<String> {
    if level == 0 {
        return module.map(str::to_owned);
    }
    let mut parts: Vec<&str> = current.split('.').collect();
    if !current_is_package {
        parts.pop();
    }
    for _ in 1..level {
        parts.pop()?;
    }
    let base = parts.join(".");
    match (base.is_empty(), module) {
        (true, None) => None,
        (true, Some(m)) => Some(m.to_owned()),
        (false, None) => Some(base),
        (false, Some(m)) => Some(format!("{base}.{m}")),
    }
}

/// Fully qualified symbol bound by one import.
fn import_target(imp: &ModuleImport, module_name: &str, is_package: bool) -> Option<String> {
    if !imp.is_from {
        let top = imp
            .imported_name
            .split('.')
            .next()
            .unwrap_or(&imp.imported_name);
        // `import a.b.c` binds `a`; `import a.b.c as x` binds `a.b.c`.
        return Some(if imp.bound_name == top {
            top.to_owned()
        } else {
            imp.imported_name.clone()
        });
    }
    let base = resolve_relative_module(module_name, is_package, imp.level, imp.module.as_deref())?;
    Some(format!("{base}.{}", imp.imported_name))
}

// ------------------------------------------------------------------------------ declarations

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassDecl {
    /// Base classes, resolved within the defining module (canonicalized on lookup).
    pub bases: Vec<String>,
    /// Priority 3 `attr -> class fqn` (instance attribute types).
    pub attr_types: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionDecl {
    pub is_async: bool,
    pub is_staticmethod: bool,
    pub is_classmethod: bool,
    pub is_method: bool,
    /// Resolved return annotation (class fqn), Advisory only.
    pub return_type: Option<String>,
    pub param_names: Vec<String>,
}

/// Per-module declarations other modules need (stored in `FileSummary`, IR-free).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleDecls {
    pub module_name: String,
    pub is_package: bool,
    /// Module-scope runtime imports: `bound name -> fqn`.
    pub imports: BTreeMap<String, String>,
    /// `if TYPE_CHECKING:` imports, usable for annotation resolution only.
    pub type_checking_imports: BTreeMap<String, String>,
    /// Class qualname -> declaration.
    pub classes: BTreeMap<String, ClassDecl>,
    /// Function qualname -> declaration (module body excluded).
    pub functions: BTreeMap<String, FunctionDecl>,
    /// Module global `name -> class fqn` of the instance it holds (`client = httpx.Client()`).
    pub global_types: BTreeMap<String, String>,
}

impl ModuleDecls {
    /// Resolve a dotted name written in this module (runtime imports, then local definitions,
    /// then builtin types). `type_checking` also consults `if TYPE_CHECKING:` imports.
    fn resolve_dotted(&self, text: &str, type_checking: bool) -> Option<String> {
        let (first, rest) = match text.split_once('.') {
            Some((f, r)) => (f, Some(r)),
            None => (text, None),
        };
        let join = |base: &str| match rest {
            Some(r) => format!("{base}.{r}"),
            None => base.to_owned(),
        };
        if let Some(t) = self.imports.get(first) {
            return Some(join(t));
        }
        if type_checking && let Some(t) = self.type_checking_imports.get(first) {
            return Some(join(t));
        }
        if self.classes.contains_key(first) || self.functions.contains_key(first) {
            return Some(format!("{}.{text}", self.module_name));
        }
        if rest.is_none() && BUILTIN_TYPES.contains(&first) {
            return Some(format!("builtins.{first}"));
        }
        None
    }

    /// Priority 2 annotation normalization: `Optional[T]` / `T | None` / `Annotated[T, ...]`
    /// / quoted `"T"` -> `T`; `list[...]` -> `builtins.list`; resolved to a fqn.
    pub fn resolve_annotation(&self, text: &str) -> Option<String> {
        let mut t = text.trim();
        loop {
            let unquoted = t
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| t.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')));
            if let Some(u) = unquoted {
                t = u.trim();
                continue;
            }
            let parts = split_top_level(t, '|');
            if parts.len() > 1 {
                let non_none: Vec<&str> = parts.into_iter().filter(|p| *p != "None").collect();
                if non_none.len() != 1 {
                    return None;
                }
                t = non_none[0];
                continue;
            }
            if let Some((head, inner)) = subscript(t) {
                let last = head.rsplit('.').next().unwrap_or(head);
                match last {
                    "Optional" | "Annotated" | "Final" | "ClassVar" | "Required"
                    | "NotRequired" | "ReadOnly" => {
                        t = split_top_level(inner, ',').first().copied()?;
                        continue;
                    }
                    "Union" => {
                        let non_none: Vec<&str> = split_top_level(inner, ',')
                            .into_iter()
                            .filter(|p| *p != "None")
                            .collect();
                        if non_none.len() != 1 {
                            return None;
                        }
                        t = non_none[0];
                        continue;
                    }
                    _ => {}
                }
                return self.resolve_type_head(head);
            }
            return self.resolve_type_head(t);
        }
    }

    fn resolve_type_head(&self, head: &str) -> Option<String> {
        if head.is_empty()
            || head == "None"
            || !head
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        {
            return None;
        }
        let typing_alias = |name: &str| match name {
            "List" => Some("list"),
            "Dict" | "DefaultDict" | "OrderedDict" => Some("dict"),
            "Set" => Some("set"),
            "FrozenSet" => Some("frozenset"),
            "Tuple" => Some("tuple"),
            _ => None,
        };
        let last = head.rsplit('.').next().unwrap_or(head);
        let resolved = self.resolve_dotted(head, true);
        if let Some(r) = &resolved {
            if let Some(stripped) = r.strip_prefix("typing.")
                && let Some(b) = typing_alias(stripped)
            {
                return Some(format!("builtins.{b}"));
            }
            return resolved;
        }
        typing_alias(last).map(|b| format!("builtins.{b}"))
    }
}

/// Split at `sep` outside brackets / parentheses, trimming parts.
fn split_top_level(text: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth -= 1,
            _ if c == sep && depth == 0 => {
                out.push(text[start..i].trim());
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(text[start..].trim());
    out
}

/// `Head[inner]` -> `(Head, inner)`.
fn subscript(text: &str) -> Option<(&str, &str)> {
    let open = text.find('[')?;
    let inner = text.strip_suffix(']')?.get(open + 1..)?;
    Some((text[..open].trim(), inner))
}

/// Declarations of every project module plus derived indexes.
#[derive(Debug, Clone, Default)]
pub struct ProjectDecls {
    modules: FxHashMap<String, ModuleDecls>,
    /// Class fqn -> direct project subclasses.
    subclasses: FxHashMap<String, Vec<String>>,
}

/// What a dotted fqn names inside the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Symbol<'a> {
    Module,
    Class(&'a ClassDecl),
    Function(FuncKey, &'a FunctionDecl),
    Global(&'a str),
    External,
}

impl ProjectDecls {
    pub fn new(modules: impl IntoIterator<Item = ModuleDecls>) -> Self {
        let mut out = Self::default();
        for m in modules {
            out.modules.insert(m.module_name.clone(), m);
        }
        out.rebuild_subclasses();
        out
    }

    /// Replace (or add) one module's declarations; the subclass index is rebuilt because a
    /// base may be canonicalized through any module.
    pub fn replace(&mut self, decls: ModuleDecls) {
        self.modules.insert(decls.module_name.clone(), decls);
        self.rebuild_subclasses();
    }

    /// Forget a deleted module.
    pub fn remove(&mut self, module: &str) {
        if self.modules.remove(module).is_some() {
            self.rebuild_subclasses();
        }
    }

    fn rebuild_subclasses(&mut self) {
        let out = &*self;
        let mut subclasses: FxHashMap<String, Vec<String>> = FxHashMap::default();
        for m in out.modules.values() {
            for (qualname, class) in &m.classes {
                let fqn = format!("{}.{qualname}", m.module_name);
                for base in &class.bases {
                    subclasses
                        .entry(out.canonical(base))
                        .or_default()
                        .push(fqn.clone());
                }
            }
        }
        for subs in subclasses.values_mut() {
            subs.sort();
            subs.dedup();
        }
        self.subclasses = subclasses;
    }

    pub fn module(&self, name: &str) -> Option<&ModuleDecls> {
        self.modules.get(name)
    }

    /// Longest project-module prefix of `fqn` and the remaining dotted part.
    fn split_module<'s>(&self, fqn: &'s str) -> Option<(&ModuleDecls, &'s str)> {
        let mut end = fqn.len();
        loop {
            let prefix = &fqn[..end];
            if let Some(m) = self.modules.get(prefix) {
                let rest = fqn.get(end + 1..).unwrap_or("");
                return Some((m, rest));
            }
            end = prefix.rfind('.')?;
        }
    }

    /// Chase re-exports (`pkg.Foo` where `pkg/__init__.py` does `from .sub import Foo`).
    pub fn canonical(&self, fqn: &str) -> String {
        let mut cur = fqn.to_owned();
        for _ in 0..MAX_ALIAS_CHASE {
            let Some((m, rest)) = self.split_module(&cur) else {
                break;
            };
            if rest.is_empty() {
                break;
            }
            let (first, tail) = match rest.split_once('.') {
                Some((f, t)) => (f, Some(t)),
                None => (rest, None),
            };
            if m.classes.contains_key(first) || m.functions.contains_key(first) {
                break;
            }
            let Some(target) = m.imports.get(first) else {
                break;
            };
            let next = match tail {
                Some(t) => format!("{target}.{t}"),
                None => target.clone(),
            };
            if next == cur {
                break;
            }
            cur = next;
        }
        cur
    }

    pub(crate) fn symbol(&self, fqn: &str) -> Symbol<'_> {
        let Some((m, rest)) = self.split_module(fqn) else {
            return Symbol::External;
        };
        if rest.is_empty() {
            return Symbol::Module;
        }
        if let Some(c) = m.classes.get(rest) {
            return Symbol::Class(c);
        }
        if let Some(f) = m.functions.get(rest) {
            let key = FuncKey {
                module: m.module_name.clone(),
                qualname: rest.to_owned(),
            };
            return Symbol::Function(key, f);
        }
        if let Some(t) = m.global_types.get(rest) {
            return Symbol::Global(t);
        }
        Symbol::External
    }

    fn class(&self, fqn: &str) -> Option<&ClassDecl> {
        match self.symbol(fqn) {
            Symbol::Class(c) => Some(c),
            _ => None,
        }
    }

    pub fn is_project_class(&self, fqn: &str) -> bool {
        self.class(fqn).is_some()
    }

    /// Every project class that (transitively) inherits from `class`, sorted.
    pub fn subclasses_of(&self, class: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = FxHashSet::default();
        let mut stack = vec![self.canonical(class)];
        while let Some(c) = stack.pop() {
            for sub in self.subclasses.get(&c).into_iter().flatten() {
                if seen.insert(sub.clone()) {
                    out.push(sub.clone());
                    stack.push(sub.clone());
                }
            }
        }
        out.sort();
        out
    }

    /// Depth-first, left-to-right base linearization (project and external classes).
    fn mro(&self, class: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = FxHashSet::default();
        let mut stack = vec![self.canonical(class)];
        while let Some(c) = stack.pop() {
            if !seen.insert(c.clone()) {
                continue;
            }
            if let Some(decl) = self.class(&c) {
                for base in decl.bases.iter().rev() {
                    stack.push(self.canonical(base));
                }
            }
            out.push(c);
        }
        out
    }

    /// The method `name` defined directly in project class `class`.
    fn own_method(&self, class: &str, name: &str) -> Option<FuncKey> {
        let (m, rest) = self.split_module(class)?;
        let qualname = format!("{rest}.{name}");
        m.functions.contains_key(&qualname).then(|| FuncKey {
            module: m.module_name.clone(),
            qualname,
        })
    }

    pub fn function_decl(&self, key: &FuncKey) -> Option<&FunctionDecl> {
        self.modules.get(&key.module)?.functions.get(&key.qualname)
    }

    /// Priority 3: instance attribute type through the MRO.
    fn attr_type(&self, class: &str, attr: &str) -> Option<String> {
        self.mro(class)
            .iter()
            .find_map(|c| self.class(c)?.attr_types.get(attr).cloned())
    }

    /// Resolve `instance_of(class).method(...)`: MRO definition plus overriding project
    /// subclasses (Priority 4 CHA). External bases supply a Layer 2 symbol fallback.
    pub(crate) fn method_targets(&self, class: &str, method: &str, skip_self: bool) -> Callee {
        let class = self.canonical(class);
        if !self.is_project_class(&class) {
            return Callee::External(format!("{class}.{method}"));
        }
        let mro = self.mro(&class);
        let start = usize::from(skip_self);
        let mut found = Vec::new();
        let mut external = None;
        for c in mro.iter().skip(start) {
            if let Some(k) = self.own_method(c, method) {
                found.push(k);
                break;
            }
            if !self.is_project_class(c) && external.is_none() {
                external = Some(format!("{c}.{method}"));
            }
        }
        if !skip_self {
            let mut stack = vec![class.clone()];
            let mut seen = FxHashSet::default();
            while let Some(c) = stack.pop() {
                for sub in self.subclasses.get(&c).into_iter().flatten() {
                    if seen.insert(sub.clone()) {
                        if let Some(k) = self.own_method(sub, method) {
                            found.push(k);
                        }
                        stack.push(sub.clone());
                    }
                }
            }
        }
        found.sort();
        found.dedup();
        if found.len() > CHA_CANDIDATE_LIMIT {
            return Callee::TooMany;
        }
        if found.is_empty() {
            return external.map_or(Callee::Unresolved, Callee::External);
        }
        Callee::Project(found, true)
    }
}

/// Build the project declarations used for summarization: structural declarations of
/// `modules` (plus `others` already known), then Priority 3 attribute types and global types
/// inferred against them.
/// Declarations of one edited module completed against `cached` (the project's declarations,
/// holding this module's previous version), updating `cached` when they changed. The fast
/// path (completion against the previous version) is exact whenever the module's structure is
/// unchanged; any difference falls back to completing against the new structure.
pub(crate) fn update_module_decls(
    input: &ModuleInput<'_>,
    kb: &EffectiveEcosystemKb,
    cached: &mut ProjectDecls,
) -> bool {
    let structural = structural_decls(input);
    let fast = complete_decls(input, structural.clone(), cached, kb);
    if cached.module(&input.module_name) == Some(&fast) {
        return false;
    }
    cached.replace(structural.clone());
    let completed = complete_decls(input, structural, cached, kb);
    cached.replace(completed);
    true
}

pub(crate) fn project_decls_for(
    modules: &[ModuleInput<'_>],
    kb: &EffectiveEcosystemKb,
    others: Option<Vec<ModuleDecls>>,
) -> ProjectDecls {
    let structural: Vec<ModuleDecls> = modules.iter().map(structural_decls).collect();
    let others = others.unwrap_or_default();
    let pass1 = ProjectDecls::new(others.iter().cloned().chain(structural.iter().cloned()));
    let completed: Vec<ModuleDecls> = modules
        .iter()
        .zip(structural)
        .map(|(m, d)| complete_decls(m, d, &pass1, kb))
        .collect();
    ProjectDecls::new(others.into_iter().chain(completed))
}

/// Pass 1: imports, classes (bases), functions (signatures) — no inference.
fn structural_decls(input: &ModuleInput<'_>) -> ModuleDecls {
    let semantic = input.semantic;
    let module_scope = module_scope(semantic);
    let mut decls = ModuleDecls {
        module_name: input.module_name.clone(),
        is_package: input.is_package,
        ..ModuleDecls::default()
    };
    for imp in &semantic.imports {
        if Some(imp.scope) != module_scope {
            continue;
        }
        let Some(target) = import_target(imp, &input.module_name, input.is_package) else {
            continue;
        };
        if imp.in_type_checking {
            decls
                .type_checking_imports
                .insert(imp.bound_name.clone(), target);
        } else {
            decls.imports.insert(imp.bound_name.clone(), target);
        }
    }
    for class in &semantic.classes {
        decls
            .classes
            .insert(class.qualname.clone(), ClassDecl::default());
    }
    for f in &input.ir.functions {
        if !is_summarized_function(f) {
            continue;
        }
        decls
            .functions
            .insert(f.qualname.clone(), FunctionDecl::default());
    }
    // Resolve bases and annotations now that local names are known.
    for class in &semantic.classes {
        let bases = class
            .bases
            .iter()
            .filter_map(|b| {
                let head = b.split('[').next().unwrap_or(b).trim();
                decls.resolve_dotted(head, false)
            })
            .collect();
        if let Some(c) = decls.classes.get_mut(&class.qualname) {
            c.bases = bases;
        }
    }
    for f in &input.ir.functions {
        if !is_summarized_function(f) {
            continue;
        }
        let is_staticmethod = f.has_decorator(&["staticmethod"]);
        let decl = FunctionDecl {
            is_async: matches!(f.kind, FunctionKind::Async | FunctionKind::AsyncGenerator),
            is_staticmethod,
            is_classmethod: f.has_decorator(&["classmethod"]),
            is_method: f.owner_class_qualname.is_some(),
            return_type: f
                .return_annotation
                .as_deref()
                .and_then(|a| decls.resolve_annotation(a)),
            param_names: f.params.iter().map(|p| p.name.clone()).collect(),
        };
        decls.functions.insert(f.qualname.clone(), decl);
    }
    decls
}

/// Pass 2: Priority 3 class attribute types and module global instance types.
fn complete_decls(
    input: &ModuleInput<'_>,
    mut decls: ModuleDecls,
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
) -> ModuleDecls {
    let cx = TypeCtx::new(input, project, kb);
    for f in &input.ir.functions {
        if f.is_class_body {
            // Class-body annotations (`fields: list[Field]`, dataclass / pydantic fields).
            if let Some(class) = decls.classes.get_mut(&f.qualname) {
                for ann in &f.local_annotations {
                    if ann.target.contains('.') {
                        continue;
                    }
                    if let Some(t) = cx.module.resolve_annotation(&ann.annotation) {
                        class.attr_types.entry(ann.target.clone()).or_insert(t);
                    }
                }
            }
            continue;
        }
        if f.is_module_body {
            let types = cx.infer(f);
            for inst in f.blocks.iter().flat_map(|b| &b.insts) {
                if let InstKind::StoreGlobal { name, value } = &inst.kind
                    && let Some(Val::Inst(t)) = types.get(value)
                {
                    decls.global_types.insert(name.clone(), t.clone());
                }
            }
            continue;
        }
        let (Some(owner), Some(self_param)) = (&f.owner_class_qualname, f.params.first()) else {
            continue;
        };
        if f.has_decorator(&["staticmethod", "classmethod"]) {
            continue;
        }
        let Some(class) = decls.classes.get_mut(owner) else {
            continue;
        };
        let prefix = format!("{}.", self_param.name);
        for ann in &f.local_annotations {
            if let Some(attr) = ann.target.strip_prefix(&prefix)
                && !attr.contains('.')
                && let Some(t) = cx.module.resolve_annotation(&ann.annotation)
            {
                class.attr_types.insert(attr.to_owned(), t);
            }
        }
        if f.name != "__init__" && f.name != "__post_init__" {
            continue;
        }
        let types = cx.infer(f);
        let self_vars = self_aliases(f, self_param.var);
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::AttrStore { obj, attr, value } = &inst.kind
                && self_vars.contains(obj)
                && let Some(Val::Inst(t)) = types.get(value)
            {
                class
                    .attr_types
                    .entry(attr.clone())
                    .or_insert_with(|| t.clone());
            }
        }
    }
    decls
}

fn module_scope(semantic: &SemanticModel) -> Option<heki_semantic::ScopeId> {
    semantic
        .scopes
        .iter()
        .find(|s| s.kind == ScopeKind::Module)
        .map(|s| s.id)
}

/// Functions that get their own `FuncKey` summary (class bodies are folded away).
pub(crate) fn is_summarized_function(f: &IrFunction) -> bool {
    !f.is_class_body && !f.is_module_body
}

/// `FuncKey` qualname of an IR function (`<module>` for the module body).
pub fn func_qualname(f: &IrFunction) -> &str {
    if f.is_module_body {
        "<module>"
    } else {
        &f.qualname
    }
}

/// Variables that alias `root` through `Copy` and phis whose inputs all alias it.
pub fn self_aliases(f: &IrFunction, root: VarId) -> FxHashSet<VarId> {
    let mut set = FxHashSet::default();
    set.insert(root);
    loop {
        let before = set.len();
        for bb in &f.blocks {
            for phi in &bb.phis {
                if !phi.incoming.is_empty()
                    && phi
                        .incoming
                        .iter()
                        .all(|(_, v)| set.contains(v) || *v == phi.dst)
                {
                    set.insert(phi.dst);
                }
            }
            for inst in &bb.insts {
                if let InstKind::Copy { dst, src } = &inst.kind
                    && set.contains(src)
                {
                    set.insert(*dst);
                }
            }
        }
        if set.len() == before {
            return set;
        }
    }
}

// ------------------------------------------------------------------------------ type inference

/// Inferred value of an SSA variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Val {
    /// A named object: module, class, function, or external symbol (by fqn).
    Ref(String),
    /// An instance of the class `fqn`.
    Inst(String),
    /// `super()` inside a method of class `fqn`.
    Super(String),
}

/// Resolution of a call target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callee {
    /// Project functions; `true` when the receiver binds to the callee's first parameter.
    Project(Vec<FuncKey>, bool),
    External(String),
    TooMany,
    Unresolved,
}

/// Per-module receiver type inference and call resolution (Advisory lane).
pub struct TypeCtx<'a> {
    pub project: &'a ProjectDecls,
    pub module: &'a ModuleDecls,
    pub kb: &'a EffectiveEcosystemKb,
    module_name: String,
    ir: &'a ModuleIr,
    /// `Import` instruction span -> bound fqn (relative imports resolved).
    import_by_span: FxHashMap<Span, String>,
}

/// Declarations used when a module is missing from `ProjectDecls`.
static EMPTY_DECLS: ModuleDecls = ModuleDecls {
    module_name: String::new(),
    is_package: false,
    imports: BTreeMap::new(),
    type_checking_imports: BTreeMap::new(),
    classes: BTreeMap::new(),
    functions: BTreeMap::new(),
    global_types: BTreeMap::new(),
};

impl<'a> TypeCtx<'a> {
    pub fn new(
        input: &'a ModuleInput<'a>,
        project: &'a ProjectDecls,
        kb: &'a EffectiveEcosystemKb,
    ) -> Self {
        let mut import_by_span = FxHashMap::default();
        for imp in &input.semantic.imports {
            if imp.in_type_checking {
                continue;
            }
            if let Some(t) = import_target(imp, &input.module_name, input.is_package) {
                import_by_span.insert(imp.span, t);
            }
        }
        Self {
            project,
            module: project.module(&input.module_name).unwrap_or(&EMPTY_DECLS),
            kb,
            module_name: input.module_name.clone(),
            ir: input.ir,
            import_by_span,
        }
    }

    pub fn module_name(&self) -> &str {
        &self.module_name
    }

    pub fn ir(&self) -> &'a ModuleIr {
        self.ir
    }

    /// Fqn of the class owning method `f` (`module.Outer.Inner`).
    pub fn owner_fqn(&self, f: &IrFunction) -> Option<String> {
        f.owner_class_qualname
            .as_ref()
            .map(|q| format!("{}.{q}", self.module_name()))
    }

    /// Value of a module-level name read inside this module.
    fn global(&self, name: &str) -> Val {
        if let Some(t) = self.module.imports.get(name) {
            return self.named(t);
        }
        if self.module.classes.contains_key(name) || self.module.functions.contains_key(name) {
            return Val::Ref(format!("{}.{name}", self.module_name()));
        }
        if let Some(t) = self.module.global_types.get(name) {
            return Val::Inst(t.clone());
        }
        Val::Ref(format!("builtins.{name}"))
    }

    /// A named fqn: a project global holding an instance becomes that instance.
    fn named(&self, fqn: &str) -> Val {
        let fqn = self.project.canonical(fqn);
        match self.project.symbol(&fqn) {
            Symbol::Global(t) => Val::Inst(t.to_owned()),
            _ => Val::Ref(fqn),
        }
    }

    /// Instance produced by calling the object `fqn`, if known.
    fn call_result(&self, fqn: &str) -> Option<Val> {
        match self.project.symbol(fqn) {
            Symbol::Class(_) => return Some(Val::Inst(fqn.to_owned())),
            Symbol::Function(_, decl) => {
                return decl
                    .return_type
                    .as_ref()
                    .map(|t| Val::Inst(self.project.canonical(t)));
            }
            Symbol::Module | Symbol::Global(_) => return None,
            Symbol::External => {}
        }
        if let Some(b) = fqn.strip_prefix("builtins.") {
            return BUILTIN_TYPES
                .contains(&b)
                .then(|| Val::Inst(fqn.to_owned()));
        }
        let last = fqn.rsplit('.').next().unwrap_or(fqn);
        if last.chars().next().is_some_and(char::is_uppercase) {
            return Some(Val::Inst(fqn.to_owned()));
        }
        // Layer 2 constructor registered with a lowercase name (`boto3.client`).
        let entry = self.kb.lookup(fqn)?;
        (entry.effects.contains(&ExecEffectKind::UnpooledClientAlloc)
            && !entry.effects.contains(&ExecEffectKind::BlockingNet))
        .then(|| Val::Inst(fqn.to_owned()))
    }

    /// Resolve calling a value (`f(...)`).
    pub fn resolve_call(&self, callee: Option<&Val>) -> Callee {
        match callee {
            Some(Val::Ref(fqn)) => match self.project.symbol(fqn) {
                Symbol::Function(key, _) => Callee::Project(vec![key], false),
                Symbol::Class(_) => match self.project.method_targets(fqn, "__init__", false) {
                    Callee::Project(keys, _) => Callee::Project(keys, true),
                    _ => Callee::External(fqn.clone()),
                },
                Symbol::Module | Symbol::Global(_) => Callee::Unresolved,
                Symbol::External => Callee::External(fqn.clone()),
            },
            Some(Val::Inst(class)) => self.project.method_targets(class, "__call__", false),
            Some(Val::Super(_)) | None => Callee::Unresolved,
        }
    }

    /// Resolve `recv.method(...)`.
    pub fn resolve_method(&self, recv: Option<&Val>, method: &str) -> Callee {
        match recv {
            Some(Val::Ref(fqn)) => {
                if self.project.is_project_class(fqn) {
                    // Unbound call through the class: `Base.method(obj, ...)`.
                    return match self.project.method_targets(fqn, method, false) {
                        Callee::Project(keys, _) => Callee::Project(keys, false),
                        other => other,
                    };
                }
                let target = self.project.canonical(&format!("{fqn}.{method}"));
                self.resolve_call(Some(&Val::Ref(target)))
            }
            Some(Val::Inst(class)) => self.project.method_targets(class, method, false),
            Some(Val::Super(class)) => self.project.method_targets(class, method, true),
            None => Callee::Unresolved,
        }
    }

    /// Value returned by a resolved call target.
    fn callee_result(&self, callee: &Callee) -> Option<Val> {
        match callee {
            Callee::Project(keys, _) if keys.len() == 1 => {
                let key = &keys[0];
                let decl = self.project.function_decl(key)?;
                if key.qualname.ends_with(".__init__") {
                    return None;
                }
                decl.return_type
                    .as_ref()
                    .map(|t| Val::Inst(self.project.canonical(t)))
            }
            Callee::External(fqn) => self.call_result(fqn),
            _ => None,
        }
    }

    /// Forward inference of `var -> Val` over `f` (two RPO passes so loop phis settle).
    pub fn infer(&self, f: &IrFunction) -> FxHashMap<VarId, Val> {
        let mut types: FxHashMap<VarId, Val> = FxHashMap::default();
        let owner = self.owner_fqn(f);
        let is_static = f.has_decorator(&["staticmethod"]);
        let is_classmethod = f.has_decorator(&["classmethod"]);
        for (i, p) in f.params.iter().enumerate() {
            if i == 0
                && !is_static
                && let Some(owner) = &owner
            {
                let v = if is_classmethod {
                    Val::Ref(owner.clone())
                } else {
                    Val::Inst(owner.clone())
                };
                types.insert(p.var, v);
                continue;
            }
            if let Some(t) = p
                .annotation
                .as_deref()
                .and_then(|a| self.module.resolve_annotation(a))
            {
                types.insert(p.var, Val::Inst(self.project.canonical(&t)));
            }
        }
        let mut annotated: FxHashMap<&str, Val> = FxHashMap::default();
        for ann in &f.local_annotations {
            if ann.target.contains('.') {
                continue;
            }
            if let Some(t) = self.module.resolve_annotation(&ann.annotation) {
                annotated.insert(ann.target.as_str(), Val::Inst(self.project.canonical(&t)));
            }
        }
        let rpo = f.rpo_blocks();
        for _ in 0..2 {
            for &bb_id in &rpo {
                let bb = f.block(bb_id);
                for phi in &bb.phis {
                    let mut known = phi
                        .incoming
                        .iter()
                        .filter(|(_, v)| *v != phi.dst)
                        .map(|(_, v)| types.get(v));
                    if let Some(Some(first)) = known.next() {
                        let first = first.clone();
                        if known.all(|t| t == Some(&first)) {
                            types.insert(phi.dst, first);
                        }
                    }
                }
                for inst in &bb.insts {
                    let Some(dst) = inst.kind.dst() else {
                        continue;
                    };
                    let val = self.infer_inst(inst, &types, owner.as_deref());
                    let val =
                        val.or_else(|| f.var_name(dst).and_then(|n| annotated.get(n).cloned()));
                    if let Some(v) = val {
                        types.insert(dst, v);
                    }
                }
            }
        }
        types
    }

    fn infer_inst(
        &self,
        inst: &Inst,
        types: &FxHashMap<VarId, Val>,
        owner: Option<&str>,
    ) -> Option<Val> {
        match &inst.kind {
            InstKind::AllocBuiltin { kind, .. } => {
                let name = match kind {
                    BuiltinKind::List => "list",
                    BuiltinKind::Dict => "dict",
                    BuiltinKind::Set => "set",
                    BuiltinKind::Tuple => "tuple",
                    BuiltinKind::Str => "str",
                    BuiltinKind::Bytes => "bytes",
                    BuiltinKind::Int => "int",
                    BuiltinKind::Float => "float",
                    BuiltinKind::Bool => "bool",
                };
                Some(Val::Inst(format!("builtins.{name}")))
            }
            InstKind::DictLiteral { .. } => Some(Val::Inst("builtins.dict".to_owned())),
            InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } => {
                Some(self.global(name))
            }
            InstKind::Import { .. } => self.import_value(inst.span),
            InstKind::Copy { src, .. } => types.get(src).cloned(),
            InstKind::AttrLoad { obj, attr, .. } => match types.get(obj)? {
                Val::Ref(fqn) => Some(self.named(&format!("{fqn}.{attr}"))),
                Val::Inst(class) => self
                    .project
                    .attr_type(class, attr)
                    .map(|t| Val::Inst(self.project.canonical(&t))),
                Val::Super(_) => None,
            },
            InstKind::MakeClosure {
                func_id: Some(id), ..
            } => {
                let f = self.ir.function(*id);
                Some(Val::Ref(format!("{}.{}", self.module_name(), f.qualname)))
            }
            InstKind::Call {
                callee_var, args, ..
            } => {
                let callee_val = callee_var.and_then(|v| types.get(&v));
                match callee_val {
                    Some(Val::Ref(fqn)) if fqn == "builtins.super" && args.is_empty() => {
                        owner.map(|o| Val::Super(o.to_owned()))
                    }
                    Some(Val::Ref(fqn)) => self.call_result(fqn),
                    other => self.callee_result(&self.resolve_call(other)),
                }
            }
            InstKind::MethodCall { recv, method, .. } => match types.get(recv) {
                Some(Val::Ref(fqn)) if !self.project.is_project_class(fqn) => {
                    self.call_result(&self.project.canonical(&format!("{fqn}.{method}")))
                }
                other => self.callee_result(&self.resolve_method(other, method)),
            },
            // `with C() as c` binds `C().__enter__()`; clients / sessions / files return self.
            InstKind::EnterWith { mgr, .. } | InstKind::Await { value: mgr, .. } => {
                types.get(mgr).cloned()
            }
            _ => None,
        }
    }

    /// Value bound by an `Import` instruction (needs the span-keyed semantic import).
    pub fn import_value(&self, span: Span) -> Option<Val> {
        self.import_by_span.get(&span).map(|t| self.named(t))
    }
}
