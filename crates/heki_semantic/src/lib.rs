//! Python scope tree, bindings, LEGB resolution, PEP 709/649/695/810 semantics,
//! module imports, and lightweight Class Hierarchy Analysis (CHA).
//!
//! Verified bidirectionally against CPython 3.11-3.15 `symtable` via `tests/symtable_verify/`.

use heki_ast::raw_ast::{self as ast, Expr, ExprContext, Pattern, Stmt, TypeParam};
use heki_ast::{LineIndex, ParsedModule, Ranged, Span};
use heki_env::RuntimeProfile;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScopeId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClassId(pub u32);

/// Python lexical scope kind (aligned with CPython `symtable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Module,
    Function,
    Class,
    Comprehension,
    TypeParameters,
    /// The lazily evaluated bound, constraints, or default of one PEP 695 / PEP 696 type
    /// parameter (`T: Base`, `T = int`).
    TypeVariable,
    /// The lazily evaluated value of a PEP 695 `type` statement.
    TypeAlias,
    Annotation,
}

/// Resolution category of a name read (verified against CPython `symtable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameResolution {
    Local,
    Enclosing,
    Global,
    Builtin,
}

/// A name a scope binds: assigned, imported, deleted, defined, or a parameter. A `global` /
/// `nonlocal` declaration alone binds nothing ([`Scope::global_decls`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub name: String,
    pub span: Span,
    pub is_parameter: bool,
    /// True when bound by Python 3.15 PEP 810 `lazy import` / `lazy from ... import ...`.
    pub is_lazy_import: bool,
}

/// Extracted module import entry (used by `heki_effect` and `heki_ir`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleImport {
    pub scope: ScopeId,
    pub module: Option<String>,
    pub imported_name: String,
    pub bound_name: String,
    /// An explicit `as` clause: `import a.b as x` binds `x` to `a.b`, while `import a.b`
    /// binds `a` to `a` (also when the alias spells the first component, `import a.b as a`).
    #[serde(default)]
    pub has_alias: bool,
    pub level: u32,
    pub is_from: bool,
    pub is_lazy: bool,
    pub in_type_checking: bool,
    /// A later statement of the module body that always runs rebinds `bound_name`
    /// (`from time import sleep`, then `def sleep(n): ...`): once the module has run, the name
    /// no longer holds the import.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub overridden: bool,
    pub span: Span,
}

/// Lightweight class metadata for CHA and strict nested-class isolation (`owner_class`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassInfo {
    pub id: ClassId,
    pub scope: ScopeId,
    pub name: String,
    /// Python's `__qualname__`: `Outer.Inner`, `factory.<locals>.Local`.
    pub qualname: String,
    /// The class whose body holds this class statement directly.
    pub parent_class: Option<ClassId>,
    pub bases: Vec<String>,
    /// Class keywords with the source text of their value (`metaclass=Meta`, `frozen=False`);
    /// `**kwargs` is left out.
    pub keywords: Vec<(String, String)>,
    /// The functions defined directly in the class body.
    pub methods: Vec<String>,
    pub span: Span,
}

/// Lexical scope node matching CPython `symtable` structure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub id: ScopeId,
    pub parent: Option<ScopeId>,
    pub kind: ScopeKind,
    pub name: String,
    pub line: u32,
    pub bindings: Vec<Binding>,
    /// Names declared `global` here, sorted (a PEP 572 walrus in a comprehension declares its
    /// target like the scope that receives it).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub global_decls: Vec<String>,
    /// Names declared `nonlocal` here, sorted (walrus targets as for `global_decls`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nonlocal_decls: Vec<String>,
    pub reads: Vec<(String, NameResolution)>,
    /// Reads occurring exclusively inside a PEP 709 inlined comprehension
    /// (CPython 3.12+ `symtable` drops `is_referenced()` on outer symbols read only there).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inlined_comp_only_reads: Vec<String>,
}

/// Per-module semantic analysis result (JSON-serializable for `symtable` differential verification).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticModel {
    pub has_future_annotations: bool,
    pub scopes: Vec<Scope>,
    pub imports: Vec<ModuleImport>,
    pub classes: Vec<ClassInfo>,
}

struct ScopeBuilder {
    id: ScopeId,
    parent: Option<ScopeId>,
    kind: ScopeKind,
    name: String,
    line: u32,
    /// Enclosing class name for Python private name mangling (`__x` -> `_Cls__x`).
    enclosing_class_name: Option<String>,
    bindings: Vec<Binding>,
    bound_names: FxHashSet<String>,
    /// Names bound exclusively as PEP 709 inlined comprehension targets (lexically isolated).
    comp_target_bindings: Vec<(String, Span)>,
    /// The PEP 709 inlined comprehensions open at the current point of this scope's body
    /// (indices into `Analyzer::comp_regions`), innermost last.
    open_comp_regions: Vec<usize>,
    /// The PEP 649 `__annotate__` scope of this module, class, or function body's variable
    /// annotations, once it has one.
    annotation_scope: Option<ScopeId>,
    /// A function body's `__annotate__` scope: never evaluated, so its own name loads are no
    /// references (names read inside its comprehensions and lambdas are).
    unevaluated: bool,
    /// The inlined comprehensions of the parent scope that lexically enclose this scope: their
    /// targets are cells this scope can capture (`[lambda: i for i in r]`), unlike every other
    /// comprehension target of the parent.
    enclosing_comp_regions: Vec<usize>,
    /// Names assigned via walrus (`:=`) inside a module-level inlined comprehension.
    module_walrus_globals: FxHashSet<String>,
    global_decls: FxHashSet<String>,
    nonlocal_decls: FxHashSet<String>,
    raw_reads: Vec<String>,
    raw_read_set: FxHashSet<String>,
    /// Reads that occurred OUTSIDE any PEP 709 inlined comprehension.
    non_comp_reads: FxHashSet<String>,
    /// Reads that occurred INSIDE a PEP 709 inlined comprehension.
    in_comp_reads: FxHashSet<String>,
    /// True for reads occurring inside an inlined class-body comprehension
    /// (which in Python 3.12+ PEP 709 cannot read the enclosing class namespace).
    class_comp_reads: FxHashSet<String>,
    /// Names bound by an inlined class-body comprehension itself (which ARE visible in it).
    class_comp_bindings: FxHashSet<String>,
}

impl ScopeBuilder {
    fn new(
        id: ScopeId,
        parent: Option<ScopeId>,
        kind: ScopeKind,
        name: String,
        line: u32,
        enclosing_class_name: Option<String>,
    ) -> Self {
        Self {
            id,
            parent,
            kind,
            name,
            line,
            enclosing_class_name,
            bindings: Vec::new(),
            bound_names: FxHashSet::default(),
            comp_target_bindings: Vec::new(),
            open_comp_regions: Vec::new(),
            annotation_scope: None,
            unevaluated: false,
            enclosing_comp_regions: Vec::new(),
            module_walrus_globals: FxHashSet::default(),
            global_decls: FxHashSet::default(),
            nonlocal_decls: FxHashSet::default(),
            raw_reads: Vec::new(),
            raw_read_set: FxHashSet::default(),
            non_comp_reads: FxHashSet::default(),
            in_comp_reads: FxHashSet::default(),
            class_comp_reads: FxHashSet::default(),
            class_comp_bindings: FxHashSet::default(),
        }
    }

    fn mangle(&self, name: &str) -> String {
        if name.starts_with("__")
            && !name.ends_with("__")
            && let Some(cls) = &self.enclosing_class_name
        {
            let stripped = cls.trim_start_matches('_');
            if !stripped.is_empty() {
                return format!("_{stripped}{name}");
            }
        }
        name.to_string()
    }

    fn add_binding(
        &mut self,
        raw_name: &str,
        span: Span,
        is_parameter: bool,
        is_lazy_import: bool,
    ) {
        let name = self.mangle(raw_name);
        if self.bound_names.insert(name.clone()) {
            self.bindings.push(Binding {
                name,
                span,
                is_parameter,
                is_lazy_import,
            });
        }
    }

    fn add_global(&mut self, raw_name: &str) {
        let name = self.mangle(raw_name);
        self.global_decls.insert(name);
    }

    fn add_nonlocal(&mut self, raw_name: &str) {
        let name = self.mangle(raw_name);
        self.nonlocal_decls.insert(name);
    }

    /// Declared `global` or `nonlocal` here: a comprehension target of the same name stays
    /// isolated instead of becoming a local of this scope.
    fn declares(&self, name: &str) -> bool {
        self.global_decls.contains(name) || self.nonlocal_decls.contains(name)
    }

    fn add_comp_target_binding(&mut self, name: String, span: Span) {
        if !self.comp_target_bindings.iter().any(|(n, _)| *n == name) {
            self.comp_target_bindings.push((name, span));
        }
    }

    fn in_inlined_comp(&self) -> bool {
        !self.open_comp_regions.is_empty()
    }

    fn add_read(&mut self, raw_name: &str, in_inlined_comp: bool, in_class_comp: bool) {
        if self.unevaluated && !in_inlined_comp {
            return;
        }
        let name = self.mangle(raw_name);
        if in_inlined_comp {
            self.in_comp_reads.insert(name.clone());
        } else {
            self.non_comp_reads.insert(name.clone());
        }
        if in_class_comp {
            // In PEP 709 class-body comprehension, reading a class-local var does not see the class scope.
            self.class_comp_reads.insert(name.clone());
        }
        if self.raw_read_set.insert(name.clone()) {
            self.raw_reads.push(name);
        }
    }
}

/// What a module-scope binding makes a name, for `TYPE_CHECKING` guards.
enum GuardBinding {
    /// `typing.TYPE_CHECKING` itself, or `False`.
    TypeChecking,
    /// Any other imported name (a module whose `TYPE_CHECKING` attribute a guard may read).
    Import,
    Other,
}

struct Analyzer<'a> {
    profile: RuntimeProfile,
    lines: LineIndex,
    has_future_annotations: bool,
    scopes: Vec<ScopeBuilder>,
    scope_stack: Vec<ScopeId>,
    class_stack: Vec<(ClassId, String)>,
    /// Python `__qualname__` of each enclosing `def` / `class`, and whether it is a function
    /// (whose nested definitions are `<locals>`).
    qualnames: Vec<(String, bool)>,
    imports: Vec<ModuleImport>,
    classes: Vec<ClassInfo>,
    in_type_checking: bool,
    /// Module names holding `typing.TYPE_CHECKING` (`from typing import TYPE_CHECKING as TC`,
    /// a re-export of it, or `TYPE_CHECKING = False`): false whenever the program runs.
    type_checking_names: FxHashSet<String>,
    /// Module names bound by an import, which `X.TYPE_CHECKING` guards read through
    /// (`typing.TYPE_CHECKING`); a class attribute of that name is no guard.
    imported_names: FxHashSet<String>,
    /// `from m import *` (only allowed at module level).
    has_star_import: bool,
    /// Target names of every PEP 709 inlined comprehension, by region index.
    comp_regions: Vec<FxHashSet<String>>,
    source: &'a str,
}

impl<'a> Analyzer<'a> {
    fn new(source: &'a str, profile: RuntimeProfile, suite: &[Stmt]) -> Self {
        let has_future_annotations = detect_future_annotations(suite);
        let root = ScopeBuilder::new(
            ScopeId(0),
            None,
            ScopeKind::Module,
            "top".to_string(),
            0,
            None,
        );
        Self {
            profile,
            lines: LineIndex::from_source(source),
            has_future_annotations,
            scopes: vec![root],
            scope_stack: vec![ScopeId(0)],
            class_stack: Vec::new(),
            qualnames: Vec::new(),
            imports: Vec::new(),
            classes: Vec::new(),
            in_type_checking: false,
            type_checking_names: FxHashSet::default(),
            imported_names: FxHashSet::default(),
            has_star_import: false,
            comp_regions: Vec::new(),
            source,
        }
    }

    /// PEP 709 inlines list / set / dict comprehensions (3.12+), except in the annotation
    /// scopes that can see a class body's names: annotation, type-parameter, type-variable, and
    /// type-alias scopes of a class body or of such a scope.
    fn inlines_comprehensions(&self) -> bool {
        self.profile.pep709_inlined_comprehensions && !self.sees_class_scope(self.cur_scope_id())
    }

    fn sees_class_scope(&self, id: ScopeId) -> bool {
        let sc = &self.scopes[id.0 as usize];
        let annotation_scope = matches!(
            sc.kind,
            ScopeKind::Annotation
                | ScopeKind::TypeParameters
                | ScopeKind::TypeVariable
                | ScopeKind::TypeAlias
        );
        annotation_scope
            && sc.parent.is_some_and(|p| {
                self.scopes[p.0 as usize].kind == ScopeKind::Class || self.sees_class_scope(p)
            })
    }

    fn cur_scope_id(&self) -> ScopeId {
        *self.scope_stack.last().expect("scope stack non-empty")
    }

    fn cur_scope_mut(&mut self) -> &mut ScopeBuilder {
        let id = self.cur_scope_id().0 as usize;
        &mut self.scopes[id]
    }

    /// `__qualname__` of a definition named `name` in the current block: `f.<locals>.C` in a
    /// function, `C.m` in a class body.
    fn child_qualname(&self, name: &str) -> String {
        match self.qualnames.last() {
            None => name.to_owned(),
            Some((outer, true)) => format!("{outer}.<locals>.{name}"),
            Some((outer, false)) => format!("{outer}.{name}"),
        }
    }

    /// The class whose body the current statement is directly in (not in one of its methods).
    fn cur_class_body(&self) -> Option<ClassId> {
        let (cid, _) = self.class_stack.last()?;
        (self.classes[cid.0 as usize].scope == self.cur_scope_id()).then_some(*cid)
    }

    fn cur_enclosing_class_name(&self) -> Option<String> {
        self.class_stack.last().map(|(_, name)| name.clone())
    }

    fn push_scope(&mut self, kind: ScopeKind, name: String, line: u32) -> ScopeId {
        let id = ScopeId(self.scopes.len() as u32);
        let parent = Some(self.cur_scope_id());
        let cls_name = self.cur_enclosing_class_name();
        let mut scope = ScopeBuilder::new(id, parent, kind, name, line, cls_name);
        scope.enclosing_comp_regions = self.cur_scope_mut().open_comp_regions.clone();
        self.scopes.push(scope);
        self.scope_stack.push(id);
        id
    }

    fn pop_scope(&mut self) {
        self.scope_stack.pop();
    }

    /// Find the target scope for a PEP 572 walrus (`:=`) binding:
    /// the innermost enclosing scope that is NOT a `Comprehension`.
    fn walrus_target_scope(&self) -> ScopeId {
        self.scope_stack
            .iter()
            .rev()
            .copied()
            .find(|sid| self.scopes[sid.0 as usize].kind != ScopeKind::Comprehension)
            .unwrap_or(ScopeId(0))
    }

    /// Bind the target `n` of a PEP 572 walrus (`n := value`) in its target scope.
    fn bind_walrus_target(&mut self, n: &ast::ExprName) {
        let cur_sid = self.cur_scope_id();
        let target_sid = self.walrus_target_scope();
        if self.scopes[target_sid.0 as usize].kind == ScopeKind::Module
            && (cur_sid != target_sid || self.scopes[cur_sid.0 as usize].in_inlined_comp())
        {
            // PEP 572 + PEP 709: walrus inside a module-level generator or inlined
            // comprehension writes a global dynamically without binding a static module local.
            if cur_sid != target_sid {
                let cur = &mut self.scopes[cur_sid.0 as usize];
                cur.add_binding(n.id.as_str(), Span::from(n.range), false, false);
                cur.add_global(n.id.as_str());
            } else {
                let mangled = self.scopes[cur_sid.0 as usize].mangle(n.id.as_str());
                self.scopes[cur_sid.0 as usize]
                    .module_walrus_globals
                    .insert(mangled);
            }
        } else if target_sid == cur_sid {
            self.scopes[cur_sid.0 as usize].add_binding(
                n.id.as_str(),
                Span::from(n.range),
                false,
                false,
            );
        } else {
            let target = &mut self.scopes[target_sid.0 as usize];
            target.add_binding(n.id.as_str(), Span::from(n.range), false, false);
            // The comprehension declares the target as the receiving function does: `global`
            // there writes the module global, otherwise the function's local.
            let target_is_global = target.global_decls.contains(&target.mangle(n.id.as_str()));
            let cur = &mut self.scopes[cur_sid.0 as usize];
            cur.add_binding(n.id.as_str(), Span::from(n.range), false, false);
            if target_is_global {
                cur.add_global(n.id.as_str());
            } else {
                cur.add_nonlocal(n.id.as_str());
            }
        }
    }

    /// Mark the module-scope imports a later module-body statement always rebinds.
    fn mark_overridden_imports(&mut self, suite: &[Stmt]) {
        let mut rebinds: FxHashMap<&str, u32> = FxHashMap::default();
        let mut names = Vec::new();
        for stmt in suite {
            names.clear();
            unconditional_bindings(stmt, &mut names);
            for name in &names {
                rebinds.insert(name, stmt.range().start().to_u32());
            }
        }
        for imp in self.imports.iter_mut().filter(|i| i.scope == ScopeId(0)) {
            imp.overridden = rebinds
                .get(imp.bound_name.as_str())
                .is_some_and(|&last| last > imp.span.end);
        }
    }

    /// Record what a module-scope statement binds `name` to, for the `TYPE_CHECKING` guards
    /// later statements test.
    fn note_guard_binding(&mut self, name: &str, binding: GuardBinding) {
        if self.cur_scope_id() != ScopeId(0) {
            return;
        }
        self.type_checking_names.remove(name);
        self.imported_names.remove(name);
        match binding {
            GuardBinding::TypeChecking => {
                self.type_checking_names.insert(name.to_owned());
            }
            GuardBinding::Import => {
                self.imported_names.insert(name.to_owned());
            }
            GuardBinding::Other => {}
        }
    }

    /// The value an `if` test has when the program runs, when it is a `TYPE_CHECKING` guard:
    /// `TYPE_CHECKING` is false, `not TYPE_CHECKING` true.
    fn runtime_guard_value(&self, test: &Expr) -> Option<bool> {
        match test {
            Expr::Name(n) => self
                .type_checking_names
                .contains(n.id.as_str())
                .then_some(false),
            Expr::Attribute(a) => (a.attr.as_str() == "TYPE_CHECKING"
                && matches!(a.value.as_ref(), Expr::Name(m) if self.imported_names.contains(m.id.as_str())))
            .then_some(false),
            Expr::UnaryOp(u) if u.op == ast::UnaryOp::Not => {
                self.runtime_guard_value(&u.operand).map(|v| !v)
            }
            _ => None,
        }
    }

    /// The names an assignment binds, for [`Self::note_guard_binding`]: `TYPE_CHECKING = False`
    /// holds the guard's run-time value.
    fn note_assignment_guards(&mut self, target: &Expr, value: Option<&Expr>) {
        match target {
            Expr::Name(n) => {
                let false_flag = n.id.as_str() == "TYPE_CHECKING"
                    && matches!(value, Some(Expr::BooleanLiteral(b)) if !b.value);
                let binding = if false_flag {
                    GuardBinding::TypeChecking
                } else {
                    GuardBinding::Other
                };
                self.note_guard_binding(n.id.as_str(), binding);
            }
            Expr::Tuple(t) => t
                .elts
                .iter()
                .for_each(|e| self.note_assignment_guards(e, None)),
            Expr::List(l) => l
                .elts
                .iter()
                .for_each(|e| self.note_assignment_guards(e, None)),
            Expr::Starred(st) => self.note_assignment_guards(&st.value, None),
            _ => {}
        }
    }

    fn visit_suite(&mut self, suite: &[Stmt]) {
        for stmt in suite {
            self.visit_stmt(stmt);
        }
    }

    /// The annotation of a variable annotation statement (`x: T`) of the current block.
    fn visit_variable_annotation(&mut self, annotation: &Expr) {
        if self.has_future_annotations {
            return;
        }
        if !self.profile.pep649_deferred_annotations {
            // Evaluated in place in module and class bodies; a function's are never evaluated,
            // but CPython's symtable still records their names.
            self.visit_expr(annotation);
            return;
        }
        // PEP 649: the block's annotations share one `__annotate__` scope, opened at its first
        // annotation (wherever it is nested in the block's statements).
        let block = self.cur_scope_id();
        let annotate = match self.scopes[block.0 as usize].annotation_scope {
            Some(id) => id,
            None => {
                let line = self.lines.line(annotation.range().start().to_u32());
                let id = self.push_scope(ScopeKind::Annotation, "__annotate__".to_string(), line);
                self.pop_scope();
                let block_kind = self.scopes[block.0 as usize].kind;
                // A function's annotations are never evaluated: the names they read directly
                // are no references (CPython 3.14 `ste_in_unevaluated_annotation`).
                self.scopes[id.0 as usize].unevaluated = block_kind == ScopeKind::Function;
                self.scopes[block.0 as usize].annotation_scope = Some(id);
                id
            }
        };
        self.scope_stack.push(annotate);
        self.visit_expr(annotation);
        self.scope_stack.pop();
    }

    /// `x: T` binds `x` even without a value (an unassigned local in a function); a
    /// parenthesized `(x): T` binds it only when it assigns a value. An attribute or subscript
    /// target evaluates its object.
    fn visit_ann_assign_target(&mut self, aa: &ast::StmtAnnAssign) {
        match aa.target.as_ref() {
            Expr::Name(n) => {
                if aa.simple || aa.value.is_some() {
                    self.cur_scope_mut().add_binding(
                        n.id.as_str(),
                        Span::from(n.range),
                        false,
                        false,
                    );
                }
            }
            target => self.visit_expr(target),
        }
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::FunctionDef(func) => self.visit_function_def(func),
            Stmt::ClassDef(cls) => self.visit_class_def(cls),
            Stmt::TypeAlias(ta) => {
                // The parser always produces a plain name for `type X = ...`.
                if let Expr::Name(name) = ta.name.as_ref() {
                    self.visit_type_alias(ta, name);
                }
            }
            Stmt::Return(ret) => {
                if let Some(v) = &ret.value {
                    self.visit_expr(v);
                }
            }
            Stmt::Delete(del) => {
                for t in &del.targets {
                    self.visit_expr(t);
                }
            }
            Stmt::Assign(assign) => {
                for t in &assign.targets {
                    self.visit_expr(t);
                    self.note_assignment_guards(t, Some(&assign.value));
                }
                self.visit_expr(&assign.value);
            }
            Stmt::AugAssign(aug) => {
                // CPython symtable marks `Name += ...` as assigned without adding a separate read flag.
                if let Expr::Name(n) = aug.target.as_ref() {
                    self.cur_scope_mut().add_binding(
                        n.id.as_str(),
                        Span::from(n.range),
                        false,
                        false,
                    );
                } else {
                    self.visit_expr(&aug.target);
                }
                self.visit_expr(&aug.value);
            }
            Stmt::AnnAssign(aa) => {
                self.visit_ann_assign_target(aa);
                if aa.value.is_some() {
                    self.note_assignment_guards(&aa.target, aa.value.as_deref());
                }
                self.visit_variable_annotation(&aa.annotation);
                if let Some(val) = &aa.value {
                    self.visit_expr(val);
                }
            }
            Stmt::For(f) => {
                self.visit_expr(&f.iter);
                self.visit_expr(&f.target);
                self.visit_suite(&f.body);
                self.visit_suite(&f.orelse);
            }
            Stmt::While(w) => {
                self.visit_expr(&w.test);
                self.visit_suite(&w.body);
                self.visit_suite(&w.orelse);
            }
            Stmt::If(if_stmt) => {
                let prev_tc = self.in_type_checking;
                // A clause runs only when every earlier test is false: after `not TYPE_CHECKING`
                // (true at run time) no later clause does.
                let mut reached_at_runtime = true;
                let clauses = std::iter::once((Some(&*if_stmt.test), &if_stmt.body)).chain(
                    if_stmt
                        .elif_else_clauses
                        .iter()
                        .map(|c| (c.test.as_ref(), &c.body)),
                );
                for (test, body) in clauses {
                    let value = test.and_then(|t| {
                        self.visit_expr(t);
                        self.runtime_guard_value(t)
                    });
                    self.in_type_checking = prev_tc || !reached_at_runtime || value == Some(false);
                    self.visit_suite(body);
                    if value == Some(true) {
                        reached_at_runtime = false;
                    }
                }
                self.in_type_checking = prev_tc;
            }
            Stmt::With(w) => {
                for item in &w.items {
                    self.visit_expr(&item.context_expr);
                    if let Some(opt) = &item.optional_vars {
                        self.visit_expr(opt);
                    }
                }
                self.visit_suite(&w.body);
            }
            Stmt::Match(m) => {
                self.visit_expr(&m.subject);
                for case in &m.cases {
                    self.visit_pattern(&case.pattern);
                    if let Some(guard) = &case.guard {
                        self.visit_expr(guard);
                    }
                    self.visit_suite(&case.body);
                }
            }
            Stmt::Raise(r) => {
                if let Some(exc) = &r.exc {
                    self.visit_expr(exc);
                }
                if let Some(cause) = &r.cause {
                    self.visit_expr(cause);
                }
            }
            Stmt::Try(t) => {
                self.visit_suite(&t.body);
                for h in &t.handlers {
                    let ast::ExceptHandler::ExceptHandler(eh) = h;
                    if let Some(ty) = &eh.type_ {
                        self.visit_expr(ty);
                    }
                    if let Some(name) = &eh.name {
                        self.cur_scope_mut().add_binding(
                            name.as_str(),
                            Span::from(name.range),
                            false,
                            false,
                        );
                    }
                    self.visit_suite(&eh.body);
                }
                self.visit_suite(&t.orelse);
                self.visit_suite(&t.finalbody);
            }
            Stmt::Assert(a) => {
                self.visit_expr(&a.test);
                if let Some(msg) = &a.msg {
                    self.visit_expr(msg);
                }
            }
            Stmt::Import(imp) => {
                let scope = self.cur_scope_id();
                for alias in &imp.names {
                    let bound = alias
                        .asname
                        .as_ref()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| alias.name.split('.').next().unwrap_or("").to_string());
                    self.cur_scope_mut().add_binding(
                        &bound,
                        Span::from(alias.range),
                        false,
                        imp.is_lazy,
                    );
                    self.note_guard_binding(&bound, GuardBinding::Import);
                    self.imports.push(ModuleImport {
                        scope,
                        module: None,
                        imported_name: alias.name.to_string(),
                        bound_name: bound,
                        has_alias: alias.asname.is_some(),
                        level: 0,
                        is_from: false,
                        is_lazy: imp.is_lazy,
                        in_type_checking: self.in_type_checking,
                        overridden: false,
                        span: Span::from(alias.range),
                    });
                }
            }
            Stmt::ImportFrom(imp) => {
                let scope = self.cur_scope_id();
                let module = imp.module.as_ref().map(|m| m.to_string());
                for alias in &imp.names {
                    if alias.name.as_str() == "*" {
                        self.has_star_import = true;
                        continue;
                    }
                    let bound = alias
                        .asname
                        .as_ref()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| alias.name.to_string());
                    self.cur_scope_mut().add_binding(
                        &bound,
                        Span::from(alias.range),
                        false,
                        imp.is_lazy,
                    );
                    let binding = if alias.name.as_str() == "TYPE_CHECKING" {
                        GuardBinding::TypeChecking
                    } else {
                        GuardBinding::Import
                    };
                    self.note_guard_binding(&bound, binding);
                    self.imports.push(ModuleImport {
                        scope,
                        module: module.clone(),
                        imported_name: alias.name.to_string(),
                        bound_name: bound,
                        has_alias: alias.asname.is_some(),
                        level: imp.level,
                        is_from: true,
                        is_lazy: imp.is_lazy,
                        in_type_checking: self.in_type_checking,
                        overridden: false,
                        span: Span::from(alias.range),
                    });
                }
            }
            Stmt::Global(g) => {
                for name in &g.names {
                    self.cur_scope_mut().add_global(name.as_str());
                }
            }
            Stmt::Nonlocal(nl) => {
                for name in &nl.names {
                    self.cur_scope_mut().add_nonlocal(name.as_str());
                }
            }
            Stmt::Expr(e) => self.visit_expr(&e.value),
            Stmt::Pass(_) | Stmt::Break(_) | Stmt::Continue(_) | Stmt::IpyEscapeCommand(_) => {}
        }
    }

    /// Line of the `def` / `async def` / `class` keyword, CPython's line of the definition:
    /// a decorated statement's range starts at its first decorator.
    fn definition_line(&self, stmt: heki_ast::TextRange, decorators: &[ast::Decorator]) -> u32 {
        let start = decorators.last().map_or(stmt.start().to_usize(), |d| {
            skip_trivia(self.source, d.range().end().to_usize())
        });
        self.lines.line(start as u32)
    }

    fn visit_function_def(&mut self, func: &ast::StmtFunctionDef) {
        let line = self.definition_line(func.range, &func.decorator_list);
        // Decorators and defaults are evaluated in the enclosing scope.
        for dec in &func.decorator_list {
            self.visit_expr(&dec.expression);
        }
        self.visit_param_defaults(&func.parameters);

        // Bind function name in enclosing scope (and record method on enclosing class for CHA).
        self.cur_scope_mut().add_binding(
            func.name.as_str(),
            Span::from(func.name.range),
            false,
            false,
        );
        self.note_guard_binding(func.name.as_str(), GuardBinding::Other);
        if let Some(cid) = self.cur_class_body() {
            self.classes[cid.0 as usize]
                .methods
                .push(func.name.to_string());
        }
        let qualname = self.child_qualname(func.name.as_str());

        // Optional PEP 695 type parameters scope.
        let has_type_params = func
            .type_params
            .as_ref()
            .is_some_and(|tp| !tp.type_params.is_empty());
        if let Some(tp) = &func.type_params
            && !tp.type_params.is_empty()
        {
            self.push_scope(ScopeKind::TypeParameters, func.name.to_string(), line);
            self.bind_type_params(tp);
        }

        // Parameter and return annotations.
        let func_anns = collect_func_annotations(func);
        if !self.has_future_annotations {
            if self.profile.pep649_deferred_annotations {
                // CPython 3.14+ (PEP 649) always opens a sibling `__annotate__` scope for `def`
                // whenever `from __future__ import annotations` is not active.
                self.push_scope(ScopeKind::Annotation, "__annotate__".to_string(), line);
                for ann in func_anns {
                    self.visit_expr(ann);
                }
                self.pop_scope();
            } else {
                for ann in func_anns {
                    self.visit_expr(ann);
                }
            }
        }

        // Function body scope.
        self.push_scope(ScopeKind::Function, func.name.to_string(), line);
        self.bind_parameters(&func.parameters);
        self.qualnames.push((qualname, true));
        self.visit_suite(&func.body);
        self.qualnames.pop();
        self.pop_scope();

        if has_type_params {
            self.pop_scope();
        }
    }

    fn visit_class_def(&mut self, cls: &ast::StmtClassDef) {
        let line = self.definition_line(cls.range, &cls.decorator_list);
        let parent_class = self.cur_class_body();
        let qualname = self.child_qualname(cls.name.as_str());
        for dec in &cls.decorator_list {
            self.visit_expr(&dec.expression);
        }
        self.cur_scope_mut().add_binding(
            cls.name.as_str(),
            Span::from(cls.name.range),
            false,
            false,
        );
        self.note_guard_binding(cls.name.as_str(), GuardBinding::Other);

        let has_type_params = cls
            .type_params
            .as_ref()
            .is_some_and(|tp| !tp.type_params.is_empty());
        if let Some(tp) = &cls.type_params
            && !tp.type_params.is_empty()
        {
            self.push_scope(ScopeKind::TypeParameters, cls.name.to_string(), line);
            self.bind_type_params(tp);
        }

        let mut bases = Vec::new();
        let mut keywords = Vec::new();
        if let Some(args) = &cls.arguments {
            for arg in &args.args {
                if let Some(b) = expr_dotted_name(arg) {
                    bases.push(b);
                }
                self.visit_expr(arg);
            }
            for kw in &args.keywords {
                if let Some(name) = &kw.arg {
                    let value = self.source[kw.value.range()].to_owned();
                    keywords.push((name.to_string(), value));
                }
                self.visit_expr(&kw.value);
            }
        }

        let class_id = ClassId(self.classes.len() as u32);

        self.class_stack.push((class_id, cls.name.to_string()));
        let scope = self.push_scope(ScopeKind::Class, cls.name.to_string(), line);
        self.qualnames.push((qualname.clone(), false));
        self.classes.push(ClassInfo {
            id: class_id,
            scope,
            name: cls.name.to_string(),
            qualname,
            parent_class,
            bases,
            keywords,
            methods: Vec::new(),
            span: Span::from(cls.range),
        });
        self.visit_suite(&cls.body);
        self.qualnames.pop();
        self.pop_scope();
        self.class_stack.pop();

        if has_type_params {
            self.pop_scope();
        }
    }

    fn visit_type_alias(&mut self, ta: &ast::StmtTypeAlias, name: &ast::ExprName) {
        let line = self.lines.line(ta.range.start().to_u32());
        self.cur_scope_mut()
            .add_binding(name.id.as_str(), Span::from(name.range), false, false);
        let alias_name = name.id.to_string();

        let has_type_params = ta
            .type_params
            .as_ref()
            .is_some_and(|tp| !tp.type_params.is_empty());
        if let Some(tp) = &ta.type_params
            && !tp.type_params.is_empty()
        {
            self.push_scope(ScopeKind::TypeParameters, alias_name.clone(), line);
            self.bind_type_params(tp);
        }
        self.push_scope(ScopeKind::TypeAlias, alias_name, line);
        self.visit_expr(&ta.value);
        self.pop_scope();
        if has_type_params {
            self.pop_scope();
        }
    }

    fn bind_type_params(&mut self, tp: &ast::TypeParams) {
        for param in &tp.type_params {
            let (name, bound, default) = match param {
                TypeParam::TypeVar(tv) => (&tv.name, tv.bound.as_deref(), tv.default.as_deref()),
                TypeParam::TypeVarTuple(tvt) => (&tvt.name, None, tvt.default.as_deref()),
                TypeParam::ParamSpec(ps) => (&ps.name, None, ps.default.as_deref()),
            };
            self.cur_scope_mut()
                .add_binding(name.as_str(), Span::from(name.range), false, false);
            // The bound (or constraints) and the default are evaluated lazily, each in its own
            // scope named after the parameter.
            for lazy in [bound, default].into_iter().flatten() {
                let line = self.lines.line(param.range().start().to_u32());
                self.push_scope(ScopeKind::TypeVariable, name.to_string(), line);
                self.visit_expr(lazy);
                self.pop_scope();
            }
        }
    }

    fn visit_param_defaults(&mut self, params: &ast::Parameters) {
        for p in params
            .posonlyargs
            .iter()
            .chain(params.args.iter())
            .chain(params.kwonlyargs.iter())
        {
            if let Some(def) = &p.default {
                self.visit_expr(def);
            }
        }
    }

    fn bind_parameters(&mut self, params: &ast::Parameters) {
        for p in params
            .posonlyargs
            .iter()
            .chain(params.args.iter())
            .chain(params.kwonlyargs.iter())
        {
            self.cur_scope_mut().add_binding(
                p.parameter.name.as_str(),
                Span::from(p.parameter.name.range),
                true,
                false,
            );
        }
        if let Some(v) = &params.vararg {
            self.cur_scope_mut().add_binding(
                v.name.as_str(),
                Span::from(v.name.range),
                true,
                false,
            );
        }
        if let Some(k) = &params.kwarg {
            self.cur_scope_mut().add_binding(
                k.name.as_str(),
                Span::from(k.name.range),
                true,
                false,
            );
        }
    }

    fn visit_comprehension(
        &mut self,
        name: &str,
        range: heki_ast::TextRange,
        generators: &[ast::Comprehension],
        inlined: bool,
        visit_elts: impl FnOnce(&mut Self),
    ) {
        if let Some(first) = generators.first() {
            self.visit_expr(&first.iter);
        }
        if inlined {
            let region = self.comp_regions.len();
            self.comp_regions.push(FxHashSet::default());
            self.cur_scope_mut().open_comp_regions.push(region);
            for (idx, g) in generators.iter().enumerate() {
                if idx > 0 {
                    self.visit_expr(&g.iter);
                }
                self.visit_expr(&g.target);
                for cond in &g.ifs {
                    self.visit_expr(cond);
                }
            }
            visit_elts(self);
            self.cur_scope_mut().open_comp_regions.pop();
        } else {
            let line = self.lines.line(range.start().to_u32());
            self.push_scope(ScopeKind::Comprehension, name.to_string(), line);
            for (idx, g) in generators.iter().enumerate() {
                if idx > 0 {
                    self.visit_expr(&g.iter);
                }
                self.visit_expr(&g.target);
                for cond in &g.ifs {
                    self.visit_expr(cond);
                }
            }
            visit_elts(self);
            self.pop_scope();
        }
    }

    fn visit_pattern(&mut self, pat: &Pattern) {
        match pat {
            Pattern::MatchValue(mv) => self.visit_expr(&mv.value),
            Pattern::MatchSingleton(_) => {}
            Pattern::MatchSequence(ms) => {
                for p in &ms.patterns {
                    self.visit_pattern(p);
                }
            }
            Pattern::MatchMapping(mm) => {
                for k in &mm.keys {
                    self.visit_expr(k);
                }
                for p in &mm.patterns {
                    self.visit_pattern(p);
                }
                if let Some(rest) = &mm.rest {
                    self.cur_scope_mut().add_binding(
                        rest.as_str(),
                        Span::from(rest.range),
                        false,
                        false,
                    );
                }
            }
            Pattern::MatchClass(mc) => {
                self.visit_expr(&mc.cls);
                for p in &mc.arguments.patterns {
                    self.visit_pattern(p);
                }
                for kw in &mc.arguments.keywords {
                    self.visit_pattern(&kw.pattern);
                }
            }
            Pattern::MatchStar(ms) => {
                if let Some(name) = &ms.name {
                    self.cur_scope_mut().add_binding(
                        name.as_str(),
                        Span::from(name.range),
                        false,
                        false,
                    );
                }
            }
            Pattern::MatchAs(ma) => {
                if let Some(p) = &ma.pattern {
                    self.visit_pattern(p);
                }
                if let Some(name) = &ma.name {
                    self.cur_scope_mut().add_binding(
                        name.as_str(),
                        Span::from(name.range),
                        false,
                        false,
                    );
                }
            }
            Pattern::MatchOr(mo) => {
                for p in &mo.patterns {
                    self.visit_pattern(p);
                }
            }
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Name(n) => match n.ctx {
                ExprContext::Store | ExprContext::Del => {
                    let sid = self.cur_scope_id().0 as usize;
                    let sc = &mut self.scopes[sid];
                    match sc.open_comp_regions.last() {
                        Some(&region) => {
                            let name = sc.mangle(n.id.as_str());
                            if sc.kind == ScopeKind::Class {
                                sc.class_comp_bindings.insert(name.clone());
                            }
                            self.comp_regions[region].insert(name.clone());
                            sc.add_comp_target_binding(name, Span::from(n.range));
                        }
                        None => sc.add_binding(n.id.as_str(), Span::from(n.range), false, false),
                    }
                }
                ExprContext::Load | ExprContext::Invalid => {
                    let sc = self.cur_scope_mut();
                    let in_ic = sc.in_inlined_comp();
                    let in_cc = in_ic && sc.kind == ScopeKind::Class;
                    sc.add_read(n.id.as_str(), in_ic, in_cc);
                }
            },
            Expr::Named(named) => {
                self.visit_expr(&named.value);
                // The target is always a name (`InvalidNamedAssignmentTarget`).
                if let Expr::Name(n) = named.target.as_ref() {
                    self.bind_walrus_target(n);
                }
            }
            Expr::Lambda(lam) => {
                if let Some(params) = &lam.parameters {
                    self.visit_param_defaults(params);
                }
                let line = self.lines.line(lam.range.start().to_u32());
                self.push_scope(ScopeKind::Function, "<lambda>".to_string(), line);
                if let Some(params) = &lam.parameters {
                    self.bind_parameters(params);
                }
                self.visit_expr(&lam.body);
                self.pop_scope();
            }
            Expr::ListComp(lc) => {
                let inlined = self.inlines_comprehensions();
                self.visit_comprehension("<listcomp>", lc.range, &lc.generators, inlined, |a| {
                    a.visit_expr(&lc.elt);
                });
            }
            Expr::SetComp(sc) => {
                let inlined = self.inlines_comprehensions();
                self.visit_comprehension("<setcomp>", sc.range, &sc.generators, inlined, |a| {
                    a.visit_expr(&sc.elt);
                });
            }
            Expr::DictComp(dc) => {
                let inlined = self.inlines_comprehensions();
                self.visit_comprehension("<dictcomp>", dc.range, &dc.generators, inlined, |a| {
                    if let Some(k) = &dc.key {
                        a.visit_expr(k);
                    }
                    a.visit_expr(&dc.value);
                });
            }
            Expr::Generator(gen_expr) => {
                // Generator expressions ALWAYS create their own scope (even on 3.12+ PEP 709).
                self.visit_comprehension(
                    "<genexpr>",
                    gen_expr.range,
                    &gen_expr.generators,
                    false,
                    |a| {
                        a.visit_expr(&gen_expr.elt);
                    },
                );
            }
            Expr::BoolOp(b) => {
                for v in &b.values {
                    self.visit_expr(v);
                }
            }
            Expr::BinOp(b) => {
                self.visit_expr(&b.left);
                self.visit_expr(&b.right);
            }
            Expr::UnaryOp(u) => self.visit_expr(&u.operand),
            Expr::If(i) => {
                self.visit_expr(&i.test);
                self.visit_expr(&i.body);
                self.visit_expr(&i.orelse);
            }
            Expr::Dict(d) => {
                for item in &d.items {
                    if let Some(k) = &item.key {
                        self.visit_expr(k);
                    }
                    self.visit_expr(&item.value);
                }
            }
            Expr::Set(s) => {
                for e in &s.elts {
                    self.visit_expr(e);
                }
            }
            Expr::List(l) => {
                for e in &l.elts {
                    self.visit_expr(e);
                }
            }
            Expr::Tuple(t) => {
                for e in &t.elts {
                    self.visit_expr(e);
                }
            }
            Expr::Await(a) => self.visit_expr(&a.value),
            Expr::Yield(y) => {
                if let Some(v) = &y.value {
                    self.visit_expr(v);
                }
            }
            Expr::YieldFrom(y) => self.visit_expr(&y.value),
            Expr::Compare(c) => {
                for operand in &c.operands {
                    self.visit_expr(operand);
                }
            }
            Expr::Call(c) => {
                self.visit_expr(&c.func);
                for arg in &c.arguments.args {
                    self.visit_expr(arg);
                }
                for kw in &c.arguments.keywords {
                    self.visit_expr(&kw.value);
                }
            }
            Expr::FString(_) | Expr::TString(_) => {
                for field in heki_ast::interpolated_expressions(expr) {
                    self.visit_expr(field);
                }
            }
            Expr::Attribute(a) => self.visit_expr(&a.value),
            Expr::Subscript(s) => {
                self.visit_expr(&s.value);
                self.visit_expr(&s.slice);
            }
            Expr::Starred(s) => self.visit_expr(&s.value),
            Expr::Slice(s) => {
                if let Some(l) = &s.lower {
                    self.visit_expr(l);
                }
                if let Some(u) = &s.upper {
                    self.visit_expr(u);
                }
                if let Some(st) = &s.step {
                    self.visit_expr(st);
                }
            }
            Expr::StringLiteral(_)
            | Expr::BytesLiteral(_)
            | Expr::NumberLiteral(_)
            | Expr::BooleanLiteral(_)
            | Expr::NoneLiteral(_)
            | Expr::EllipsisLiteral(_)
            | Expr::IpyEscapeCommand(_) => {}
        }
    }

    fn resolve_all(self) -> SemanticModel {
        let module_globals = ModuleGlobals::of(&self.scopes, self.has_star_import);
        let resolver = Resolver {
            scopes: &self.scopes,
            comp_regions: &self.comp_regions,
            module_globals,
        };

        let scopes = self
            .scopes
            .iter()
            .map(|sc| {
                // Determine which PEP 709 inlined comprehension targets materialize as scope locals
                // vs stay isolated because the outer scope references a global/enclosing name of the same identifier.
                let mut effective_bindings = sc.bindings.clone();
                let mut effective_comp_locals = FxHashSet::default();
                for (cname, cspan) in &sc.comp_target_bindings {
                    if sc.bound_names.contains(cname) || sc.declares(cname) {
                        continue;
                    }
                    // If the enclosing scope itself reads `cname` OUTSIDE the comprehension
                    // (or assigns it via module-level comprehension walrus), preserve outer global/enclosing resolution.
                    if sc.non_comp_reads.contains(cname) || sc.module_walrus_globals.contains(cname)
                    {
                        continue;
                    }
                    effective_comp_locals.insert(cname.clone());
                    effective_bindings.push(Binding {
                        name: cname.clone(),
                        span: *cspan,
                        is_parameter: false,
                        is_lazy_import: false,
                    });
                }

                let mut reads = Vec::new();
                let mut inlined_comp_only_reads = Vec::new();
                for name in &sc.raw_reads {
                    let comp_only =
                        sc.in_comp_reads.contains(name) && !sc.non_comp_reads.contains(name);
                    // CPython 3.12+ PEP 709: class-body comprehension reads of class-bound names
                    // (not bound by the comprehension itself) do not resolve to the class scope.
                    let skip_self_class = sc.kind == ScopeKind::Class
                        && sc.class_comp_reads.contains(name)
                        && !sc.class_comp_bindings.contains(name);
                    if skip_self_class && sc.bound_names.contains(name) {
                        continue;
                    }
                    let res = if effective_comp_locals.contains(name) {
                        if sc.kind == ScopeKind::Module {
                            NameResolution::Global
                        } else {
                            NameResolution::Local
                        }
                    } else {
                        resolver.resolve(sc.id, name)
                    };
                    if comp_only {
                        inlined_comp_only_reads.push(name.clone());
                    }
                    reads.push((name.clone(), res));
                }
                reads.sort_by(|a, b| a.0.cmp(&b.0));
                inlined_comp_only_reads.sort();
                effective_bindings.sort_by(|a, b| a.name.cmp(&b.name));

                Scope {
                    id: sc.id,
                    parent: sc.parent,
                    kind: sc.kind,
                    name: sc.name.clone(),
                    line: sc.line,
                    bindings: effective_bindings,
                    global_decls: sorted(&sc.global_decls),
                    nonlocal_decls: sorted(&sc.nonlocal_decls),
                    reads,
                    inlined_comp_only_reads,
                }
            })
            .collect();

        SemanticModel {
            has_future_annotations: self.has_future_annotations,
            scopes,
            imports: self.imports,
            classes: self.classes,
        }
    }
}

/// The names a module's globals may hold, for telling a module global from a builtin.
struct ModuleGlobals {
    bound: FxHashSet<String>,
    /// `from m import *` binds names no static view lists: no read is known to be a builtin.
    star_import: bool,
}

impl ModuleGlobals {
    /// Module-scope bindings plus what nested scopes write into the module: a name declared
    /// `global` and bound there (`global state; state = 1` in a function), and walrus targets
    /// of module-level comprehensions.
    fn of(scopes: &[ScopeBuilder], star_import: bool) -> Self {
        let module = &scopes[0];
        let mut bound = module.bound_names.clone();
        bound.extend(module.module_walrus_globals.iter().cloned());
        for sc in &scopes[1..] {
            bound.extend(
                sc.global_decls
                    .iter()
                    .filter(|n| sc.bound_names.contains(*n))
                    .cloned(),
            );
        }
        Self { bound, star_import }
    }

    fn resolve(&self, name: &str) -> NameResolution {
        if self.star_import || self.bound.contains(name) {
            NameResolution::Global
        } else {
            NameResolution::Builtin
        }
    }
}

/// LEGB resolution of a name read over the finished scope tree.
struct Resolver<'s> {
    scopes: &'s [ScopeBuilder],
    comp_regions: &'s [FxHashSet<String>],
    module_globals: ModuleGlobals,
}

impl Resolver<'_> {
    fn scope(&self, id: ScopeId) -> &ScopeBuilder {
        &self.scopes[id.0 as usize]
    }

    fn resolve(&self, scope_id: ScopeId, name: &str) -> NameResolution {
        let sc = self.scope(scope_id);
        if sc.global_decls.contains(name) {
            return self.module_globals.resolve(name);
        }
        if sc.nonlocal_decls.contains(name) {
            return NameResolution::Enclosing;
        }
        if sc.kind == ScopeKind::Module {
            return self.module_globals.resolve(name);
        }
        if sc.bound_names.contains(name) {
            return NameResolution::Local;
        }

        // Walk enclosing scopes (Python LEGB rule: skip Class scopes!).
        let mut child = sc;
        while let Some(pid) = child.parent {
            let p = self.scope(pid);
            // A target of an inlined comprehension of `p` around `child` is that
            // comprehension's cell, whatever kind of scope `p` is.
            if child
                .enclosing_comp_regions
                .iter()
                .any(|r| self.comp_regions[*r].contains(name))
            {
                return NameResolution::Enclosing;
            }
            match p.kind {
                ScopeKind::Module => break,
                // A class body hides its names from nested scopes but gives them the implicit
                // `__class__` cell (what zero-argument `super()` reads).
                ScopeKind::Class if name == "__class__" => return NameResolution::Enclosing,
                ScopeKind::Class => {}
                ScopeKind::Function
                | ScopeKind::Comprehension
                | ScopeKind::TypeParameters
                | ScopeKind::TypeVariable
                | ScopeKind::TypeAlias
                | ScopeKind::Annotation => {
                    if p.global_decls.contains(name) {
                        return self.module_globals.resolve(name);
                    }
                    if p.nonlocal_decls.contains(name) || p.bound_names.contains(name) {
                        return NameResolution::Enclosing;
                    }
                }
            }
            child = p;
        }

        self.module_globals.resolve(name)
    }
}

/// The offset of the first token at or after `at`: whitespace, line continuations, and
/// comments skipped.
fn skip_trivia(source: &str, mut at: usize) -> usize {
    let bytes = source.as_bytes();
    while let Some(&b) = bytes.get(at) {
        match b {
            b' ' | b'\t' | b'\x0c' | b'\r' | b'\n' | b'\\' => at += 1,
            b'#' => {
                at = bytes[at..]
                    .iter()
                    .position(|&c| c == b'\n' || c == b'\r')
                    .map_or(bytes.len(), |n| at + n);
            }
            _ => break,
        }
    }
    at
}

fn sorted(names: &FxHashSet<String>) -> Vec<String> {
    let mut out: Vec<String> = names.iter().cloned().collect();
    out.sort();
    out
}

fn detect_future_annotations(suite: &[Stmt]) -> bool {
    for stmt in suite {
        match stmt {
            Stmt::Expr(e) if matches!(e.value.as_ref(), Expr::StringLiteral(_)) => continue,
            // `from .__future__ import x` is an ordinary relative import.
            Stmt::ImportFrom(imp)
                if imp.level == 0 && imp.module.as_deref() == Some("__future__") =>
            {
                if imp.names.iter().any(|a| a.name.as_str() == "annotations") {
                    return true;
                }
            }
            _ => break,
        }
    }
    false
}

fn collect_func_annotations(func: &ast::StmtFunctionDef) -> Vec<&Expr> {
    let mut out = Vec::new();
    let p = &func.parameters;
    for arg in p
        .posonlyargs
        .iter()
        .chain(p.args.iter())
        .chain(p.kwonlyargs.iter())
    {
        if let Some(ann) = &arg.parameter.annotation {
            out.push(ann.as_ref());
        }
    }
    if let Some(v) = &p.vararg
        && let Some(ann) = &v.annotation
    {
        out.push(ann.as_ref());
    }
    if let Some(k) = &p.kwarg
        && let Some(ann) = &k.annotation
    {
        out.push(ann.as_ref());
    }
    if let Some(ret) = &func.returns {
        out.push(ret.as_ref());
    }
    out
}

fn expr_dotted_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name(n) => Some(n.id.to_string()),
        Expr::Attribute(a) => Some(format!("{}.{}", expr_dotted_name(&a.value)?, a.attr)),
        Expr::Subscript(s) => expr_dotted_name(&s.value),
        _ => None,
    }
}

/// Build the lexical scope, binding, import, and CHA model for a parsed Python module.
pub fn analyze_module(
    module: &ParsedModule,
    source: &str,
    profile: RuntimeProfile,
) -> SemanticModel {
    let mut analyzer = Analyzer::new(source, profile, module.suite());
    analyzer.visit_suite(module.suite());
    analyzer.mark_overridden_imports(module.suite());
    analyzer.resolve_all()
}

/// The names a module-body statement always binds when it runs (not those of its nested
/// suites, which may not run).
fn unconditional_bindings<'s>(stmt: &'s Stmt, out: &mut Vec<&'s str>) {
    fn targets<'s>(e: &'s Expr, out: &mut Vec<&'s str>) {
        match e {
            Expr::Name(n) => out.push(n.id.as_str()),
            Expr::Tuple(t) => t.elts.iter().for_each(|e| targets(e, out)),
            Expr::List(l) => l.elts.iter().for_each(|e| targets(e, out)),
            Expr::Starred(st) => targets(&st.value, out),
            _ => {}
        }
    }
    match stmt {
        Stmt::FunctionDef(f) => out.push(f.name.as_str()),
        Stmt::ClassDef(c) => out.push(c.name.as_str()),
        Stmt::Assign(a) => a.targets.iter().for_each(|t| targets(t, out)),
        Stmt::AnnAssign(a) if a.value.is_some() => targets(&a.target, out),
        Stmt::AugAssign(a) => targets(&a.target, out),
        Stmt::TypeAlias(t) => targets(&t.name, out),
        Stmt::Delete(d) => d.targets.iter().for_each(|t| targets(t, out)),
        Stmt::Import(i) => out.extend(i.names.iter().map(|a| {
            a.asname
                .as_ref()
                .map_or_else(|| a.name.split('.').next().unwrap_or(""), |n| n.as_str())
        })),
        Stmt::ImportFrom(i) => out.extend(
            i.names
                .iter()
                .map(|a| a.asname.as_ref().unwrap_or(&a.name).as_str()),
        ),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use heki_ast::parse_module;
    use heki_env::PythonVersion;

    #[test]
    fn resolves_closures_pep709_pep649_pep810_and_cha() {
        let src = "\
import os
lazy import json
from typing import TYPE_CHECKING
if TYPE_CHECKING:
    import collections

class Base:
    pass

class Child(Base):
    class Nested:
        def f(self): pass
    def method(self, x: int) -> int:
        y = [i for i in range(x)]
        return len(y)
";
        let parsed = parse_module(src).unwrap();
        let m315 = analyze_module(&parsed, src, RuntimeProfile::from(PythonVersion::PY315));
        // PEP 810 lazy import tracked
        assert!(
            m315.imports
                .iter()
                .any(|i| i.bound_name == "json" && i.is_lazy)
        );
        // TYPE_CHECKING tracked
        assert!(
            m315.imports
                .iter()
                .any(|i| i.bound_name == "collections" && i.in_type_checking)
        );
        // Nested class isolated from Child
        assert_eq!(m315.classes.len(), 3);
        let child = m315.classes.iter().find(|c| c.name == "Child").unwrap();
        let nested = m315.classes.iter().find(|c| c.name == "Nested").unwrap();
        assert_eq!(child.bases, vec!["Base"]);
        assert_eq!(nested.parent_class, Some(child.id));
        assert_eq!(nested.qualname, "Child.Nested");
        // Python 3.11 vs 3.12 PEP 709 comprehension scope difference
        let m311 = analyze_module(&parsed, src, RuntimeProfile::from(PythonVersion::PY311));
        assert!(
            m311.scopes
                .iter()
                .any(|s| s.kind == ScopeKind::Comprehension)
        );
        assert!(
            !m315
                .scopes
                .iter()
                .any(|s| s.kind == ScopeKind::Comprehension)
        );
    }
}
