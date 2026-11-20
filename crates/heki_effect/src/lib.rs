//! Advisory / Effect lane (`Family 1: MUT1xx/MUT2xx` & `Family 2: EFF/DATA`).
//!
//! Designed from Day 1 for **incremental LSP execution** (ADR-017):
//! - Per-file `FileSummary` is stored in memory; `IrFunction` bodies are NOT retained
//!   after summarization. A `FileSummary` keeps the function's *local* facts
//!   (`local_functions` + `call_sites`), so interprocedural propagation can be recomputed
//!   for any region of the project without IR.
//! - On file edits, `ProjectIndex::update_file` compares old vs new `EffectSummary`
//!   (**Early Cutoff**): if summaries did not change (>95% of edits), reverse call-graph
//!   propagation is skipped completely.
//!
//! Pipeline (docs/ANALYSIS_AND_KB_SPEC.md section 2):
//! 1. `resolver`: module names, import maps, project declarations, 4-priority receiver types, CHA.
//! 2. `summary`: local (one function) effect extraction into `EffectSummary` + `CallSite`s.
//! 3. `scc` + `callgraph`: Tarjan SCC reverse-topological worklist fixpoint.
//! 4. `report`: Boundary-Only Reporting and Root-Cause Grouping helpers.

mod callgraph;
pub mod consumption;
mod report;
pub mod resolver;
mod scc;
mod summary;

pub use report::{
    BoundaryReport, ROOT_CAUSE_GROUP_THRESHOLD, RootCauseGroup, boundary_reports,
    boundary_reports_in, group_by_root_cause,
};
pub use resolver::{
    Callee, ClassDecl, DeclKeys, DeclPasses, FunctionDecl, ModuleDecls, ProjectDecls, ReadOnlyAttr,
    ReadOnlyKind, TypeCtx, Val, detect_src_layout, func_qualname, module_name_for_path,
    resolve_relative_module, self_aliases,
};
pub use summary::{BLOCKING_EFFECTS, CONTAINER_MUTATORS};

use heki_ast::Span;
use heki_ir::ModuleIr;
use heki_kb::{EffectiveEcosystemKb, ExecEffectKind};
use heki_semantic::SemanticModel;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Canonical function key across files (stable across line movements).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FuncKey {
    pub module: String,
    pub qualname: String,
}

impl FuncKey {
    /// Dotted fully qualified name (`pkg.mod.Class.method`).
    pub fn fqn(&self) -> String {
        format!("{}.{}", self.module, self.qualname)
    }
}

/// Attribute / constant-key access path (distinguishes disjoint dict keys for `MUT101`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AttrKeyPath {
    pub attr: String,
    pub const_key: Option<String>,
}

/// Typed field write site used for declarative Write Ownership (`MUT201`).
/// Kept for compatibility; derived from `EffectSummary::typed_field_write_sites`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TypedFieldWrite {
    pub class_qualname: String,
    pub attr: String,
    pub kind: String,
}

/// One hop in an interprocedural call chain (for Boundary-Only & LSP RelatedLocation).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CallHop {
    pub caller: FuncKey,
    /// `FuncKey::fqn()` of a project function, or a Layer 2 symbol.
    pub callee: String,
    pub span: Span,
    pub path: PathBuf,
}

/// Write site detail for MUT201.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TypedFieldWriteSite {
    /// Fully qualified class name (`pkg.model.DataModel`).
    pub class_qualname: String,
    pub attr: String,
    pub kind: WriteKind,
    /// Span of the write (or of the helper call for `IndirectHelper`) in the file of the
    /// function that performs it: the summary owner when `via_chain` is empty, else the
    /// last hop's callee.
    pub span: Span,
    pub via_chain: Vec<CallHop>,
    /// Parameters of the summary owner the written object may be, as itself (bit 0: a
    /// method's own `self`), mapped through every hop. Empty for an object that is no
    /// parameter: a fresh instance, an attribute of one, a call result.
    pub obj_params: u64,
    /// The written object is an instance that the function holding the original write
    /// constructs itself (`Model(...)`, `cls(...)`, through copies) and fills before anyone
    /// else holds it (`MUT108`).
    pub fresh: bool,
}

/// Typed field read site for declarative Read Ownership (`MUT203`). A local fact: it is never
/// propagated to callers (the rule walks the reverse call graph instead), so every final
/// summary keeps exactly its own reads.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TypedFieldReadSite {
    /// Fully qualified class name of the read object (`pkg.model.Reference`).
    pub class_qualname: String,
    pub attr: String,
    pub kind: ReadKind,
    /// Parameters of the reading function the read object may be, as itself (bit 0: `self`).
    pub obj_params: u64,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadKind {
    /// `obj.attr`.
    DirectAttr,
    /// `getattr(obj, "attr"[, default])` / `object.__getattribute__(obj, "attr")`.
    GetAttr,
    /// `obj.__dict__["attr"]` / `obj.__dict__.get("attr")`.
    DictDirect,
}

impl ReadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectAttr => "direct_attr",
            Self::GetAttr => "getattr",
            Self::DictDirect => "dict_direct",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteKind {
    DirectAttr,
    AttrDelete,
    ContainerMut,
    AliasContainerMut,
    SetAttr,
    DictDirect,
    IndirectHelper,
}

/// A named slot of an object: an attribute (`obj.a`) or a constant string key (`obj["a"]`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Slot {
    Attr(String),
    Key(String),
}

impl WriteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectAttr => "direct_attr",
            Self::AttrDelete => "attr_delete",
            Self::ContainerMut => "container_mut",
            Self::AliasContainerMut => "alias_container_mut",
            Self::SetAttr => "set_attr",
            Self::DictDirect => "dict_direct",
            Self::IndirectHelper => "indirect_helper",
        }
    }
}

/// Unified interprocedural summary propagated bottom-up over Tarjan SCCs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectSummary {
    // Family 2: Execution Effect
    pub direct_exec_effects: BTreeSet<ExecEffectKind>,
    pub transitive_exec_effects: BTreeSet<ExecEffectKind>,
    /// Shortest known call chain reaching each effect (direct or transitive).
    pub exec_chains: BTreeMap<ExecEffectKind, Vec<CallHop>>,

    // Family 1: Mutation & State Effect (MUT1xx / MUT2xx)
    pub mutates_params: u64,
    pub escapes_params: u64,
    /// Parameters iterated to exhaustion on every path that returns normally when the body
    /// runs at the call (`MUT106`: passing a single-pass iterator consumes it).
    pub consumes_params: u64,
    /// Parameters passed (as themselves) to project calls only where the call runs on every
    /// path that returns normally: `consumes_params` propagates through those calls alone.
    pub unconditional_forwards: u64,
    /// `param index -> paths`: `""` = container mutation of the argument itself,
    /// `"a"` = write to / mutation of the argument's attribute `a`.
    pub mutated_param_paths: BTreeMap<u8, BTreeSet<String>>,
    /// `param index -> slots`: the containers held in these slots of the argument itself
    /// (`p.a`, `p["k"]`) are changed in place (`p.a.append(...)`, `p["k"][i] = v`), also through
    /// local copies and calls. Rebinding a slot, `setattr`, and `__dict__` writes do not count,
    /// nor do changes deeper inside (`p.a.b.append(...)`) (`MUT108`).
    pub inplace_param_slots: BTreeMap<u8, BTreeSet<Slot>>,
    pub reads_self_attrs: BTreeSet<AttrKeyPath>,
    pub mutates_self_attrs: BTreeSet<AttrKeyPath>,
    pub invalidates_cached: BTreeSet<String>,
    pub typed_field_writes: BTreeSet<TypedFieldWrite>,
    pub typed_field_write_sites: BTreeSet<TypedFieldWriteSite>,
    /// The function's own reads of project-class attributes (`MUT203`; never propagated).
    pub typed_field_read_sites: BTreeSet<TypedFieldReadSite>,
    /// Names of the private methods the function calls on any receiver (`obj._name(...)`),
    /// resolved or not: `MUT107` leaves the methods a class family's initializers call this
    /// way to initialization. A local fact: never propagated.
    pub private_calls: BTreeSet<String>,

    // Async offload & Sync-over-Async relaxation (dcg empirical findings)
    pub offloads_blocking_params: u64,
    pub sync_driven_only: bool,
    /// A call in this function (not its callees) could not be resolved, or CHA exceeded
    /// `resolver::CHA_CANDIDATE_LIMIT` candidates.
    pub unknown_calls: bool,

    // Function metadata
    pub is_async: bool,
    pub is_cached_property: bool,
    pub is_lru_cache: bool,
    /// Module-relative class qualname of a method (`Outer.Inner`), as in `IrFunction`.
    pub owner_class_qualname: Option<String>,
    pub decorators: Vec<String>,
}

impl EffectSummary {
    /// Copy with every source location zeroed: Early Cutoff compares facts, not positions,
    /// so an edit that only moves code does not notify callers.
    pub fn without_locations(&self) -> Self {
        let mut out = self.clone();
        for chain in out.exec_chains.values_mut() {
            for hop in chain {
                hop.span = Span { start: 0, end: 0 };
            }
        }
        out.typed_field_write_sites = out
            .typed_field_write_sites
            .into_iter()
            .map(|mut site| {
                site.span = Span { start: 0, end: 0 };
                for hop in &mut site.via_chain {
                    hop.span = Span { start: 0, end: 0 };
                }
                site
            })
            .collect();
        out.typed_field_read_sites = out
            .typed_field_read_sites
            .into_iter()
            .map(|mut site| {
                site.span = Span { start: 0, end: 0 };
                site
            })
            .collect();
        out
    }

    /// Location-insensitive equality used by Early Cutoff.
    pub fn same_facts(&self, other: &Self) -> bool {
        self.without_locations() == other.without_locations()
    }

    /// Equality of everything callers copy into their own summaries: the facts, and the
    /// positions of call chains and write sites. Only the positions of the function's own read
    /// sites are left out: read sites are never propagated.
    pub fn same_for_callers(&self, other: &Self) -> bool {
        let without_reads = |s: &Self| Self {
            typed_field_read_sites: BTreeSet::new(),
            ..s.clone()
        };
        without_reads(self) == without_reads(other) && self.same_facts(other)
    }

    pub(crate) fn refresh_typed_field_writes(&mut self) {
        self.typed_field_writes = self
            .typed_field_write_sites
            .iter()
            .map(|s| TypedFieldWrite {
                class_qualname: s.class_qualname.clone(),
                attr: s.attr.clone(),
                kind: s.kind.as_str().to_owned(),
            })
            .collect();
    }
}

/// Where a call argument comes from, as seen by the caller (used to map callee facts back).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgOrigin {
    /// Caller parameter `i` itself (`None`) or its attribute `a` (`Some(a)`).
    pub param: Option<(u8, Option<String>)>,
    /// A slot of caller parameter `i` itself, one level deep (`p.a`, `p["k"]`).
    pub param_slot: Option<(u8, Slot)>,
    /// `self.<attr>` of the calling method.
    pub self_attr: Option<String>,
    /// `obj.<attr>` where `obj` has project class type: `(class_fqn, attr)`.
    pub typed_field: Option<(String, String)>,
    /// An instance of project class `class_fqn`.
    pub typed_instance: Option<String>,
    /// Caller parameters the argument may be, as itself (through copies and phis).
    pub may_params: u64,
    /// The argument is an instance the caller constructs (`Model(...)`), or an attribute of one.
    pub fresh: bool,
}

impl ArgOrigin {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One resolved project callee with the caller-side origin of each callee parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundCallee {
    pub callee: FuncKey,
    /// Indexed by the callee's parameter position (receiver included for bound methods).
    pub params: Vec<ArgOrigin>,
}

impl BoundCallee {
    /// The caller parameters the objects bound to the callee parameters in `mask` may be (the
    /// mapping of `obj_params` across this call).
    pub fn caller_params_of(&self, mask: u64) -> u64 {
        callgraph::bound_params(mask, self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallTarget {
    /// Project functions (1 candidate, or up to `CHA_CANDIDATE_LIMIT` CHA candidates).
    Project(Vec<BoundCallee>),
    /// A Layer 2 catalog API (only recorded when it carries effects or offloads).
    Layer2 {
        symbol: String,
        /// Effects after the `is_awaited` filter.
        effects: BTreeSet<ExecEffectKind>,
        offloads_blocking_arg: Option<u8>,
        async_safe: bool,
    },
}

/// A call site retained in `FileSummary` (IR-free) for propagation and rule evaluation. Reading
/// a property (`obj.attr` whose `attr` is a getter) is one too: it calls the getter, at the read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSite {
    pub caller: FuncKey,
    pub target: CallTarget,
    pub is_awaited: bool,
    pub span: Span,
    /// Natural-loop nesting depth of the call (`EFF102` / `EFF103`).
    pub loop_depth: u16,
    /// Receiver is the calling method's `self` (`self.helper()`, `super().m()`, `self.prop`).
    pub recv_is_self: bool,
    /// The returned coroutine is driven by hand (`.send(None)` or a `run_sync` driver).
    pub drives_coroutine_manually: bool,
}

impl CallSite {
    /// The resolved project callees (none for a Layer 2 API).
    pub fn project_callees(&self) -> &[BoundCallee] {
        match &self.target {
            CallTarget::Project(callees) => callees,
            CallTarget::Layer2 { .. } => &[],
        }
    }

    /// Whether the body of the project callee summarized by `callee` runs at this call:
    /// calling an `async def` without awaiting or driving it only creates a coroutine.
    pub fn runs_body_of(&self, callee: &EffectSummary) -> bool {
        callgraph::runs_at_call(callee, self)
    }
}

/// Lightweight per-file summary retained in `ProjectIndex` (IR bodies are dropped).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSummary {
    pub path: PathBuf,
    pub module_name: String,
    pub content_hash: String,
    /// Final (interprocedurally propagated) summaries; filled by `ProjectIndex`.
    pub functions: FxHashMap<FuncKey, EffectSummary>,
    /// Project call edges `(caller, callee, span)` (reverse call graph source).
    pub call_edges: Vec<(FuncKey, FuncKey, Span)>,
    /// Local (one-function) summaries before propagation.
    pub local_functions: FxHashMap<FuncKey, EffectSummary>,
    pub call_sites: Vec<CallSite>,
    /// Declarations other modules need for name and receiver resolution.
    pub decls: ModuleDecls,
    /// What summarizing the file read of the project declarations (completing its own
    /// declarations included): a change of any of these keys can change the summary. Recorded
    /// for the language server's index only (`analyze_project_with_reads`, the incremental
    /// updates); empty otherwise.
    pub decl_reads: DeclKeys,
}

/// Result of an incremental file update (Early Cutoff indicator for `heki_lsp`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncrementalUpdate {
    /// True if any function's `EffectSummary` changed and callers need re-evaluation.
    pub summary_changed: bool,
    /// Functions (transitive callers) that depend on changed summaries.
    pub affected_callers: Vec<FuncKey>,
    /// The functions of the file (and of call cycles through it) whose facts changed or that
    /// disappeared, sorted.
    pub changed: Vec<FuncKey>,
}

/// One module handed to the analyzer. The IR is borrowed only during summarization.
pub struct ModuleInput<'a> {
    pub path: PathBuf,
    pub module_name: String,
    pub is_package: bool,
    pub content_hash: String,
    pub semantic: &'a SemanticModel,
    pub ir: &'a ModuleIr,
}

/// One execution effect that runs at a call site, with the shortest chain from the site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteEffect {
    pub effect: ExecEffectKind,
    /// Starts with the hop of this call site; ends at the Layer 2 sink.
    pub chain: Vec<CallHop>,
    /// Project callee the effect comes through (`None` for a Layer 2 API).
    pub callee: Option<FuncKey>,
    /// The effect comes from an awaited / driven `async def` callee (its own body).
    pub callee_is_async: bool,
}

/// Incremental project-wide summary & reverse call-graph index.
#[derive(Debug, Default)]
pub struct ProjectIndex {
    pub files: FxHashMap<PathBuf, FileSummary>,
    pub reverse_call_graph: FxHashMap<FuncKey, FxHashSet<FuncKey>>,
    /// Defining file of every summarized function.
    pub(crate) func_files: FxHashMap<FuncKey, PathBuf>,
}

/// True for `.pyi` typing stubs (prefer the sibling `.py` implementation when both exist).
pub(crate) fn is_stub(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "pyi")
}

/// Summarize every module of a project and propagate effects to a fixpoint.
pub fn analyze_project(modules: &[ModuleInput<'_>], kb: &EffectiveEcosystemKb) -> ProjectIndex {
    analyze(modules, kb, false)
}

/// [`analyze_project`] recording what each file's summary read of the project declarations
/// (`FileSummary::decl_reads`), for an index that incremental updates keep current.
pub fn analyze_project_with_reads(
    modules: &[ModuleInput<'_>],
    kb: &EffectiveEcosystemKb,
) -> ProjectIndex {
    analyze(modules, kb, true)
}

fn analyze(modules: &[ModuleInput<'_>], kb: &EffectiveEcosystemKb, track: bool) -> ProjectIndex {
    let (decls, own) = resolver::project_decls_for(modules, kb, None, track);
    let files = modules
        .iter()
        .zip(own)
        .map(|(m, own)| summary::summarize_module(m, &decls, kb, own, track))
        .collect();
    ProjectIndex::build(files)
}

impl ProjectIndex {
    /// Build an index from local file summaries and propagate the whole project.
    pub fn build(mut files: Vec<FileSummary>) -> Self {
        // Index stubs first so a sibling `.py` implementation always owns the module's symbols.
        files.sort_by_key(|f| !is_stub(&f.path));
        let mut index = Self::default();
        for mut file in files {
            file.functions = file.local_functions.clone();
            index.add_file_links(&file);
            index.files.insert(file.path.clone(), file);
        }
        let all: FxHashSet<FuncKey> = index.all_functions().collect();
        callgraph::propagate(&mut index, &all);
        callgraph::recompute_sync_driven(&mut index);
        index
    }

    /// Effects that run at `site` (in file `path`): the Layer 2 API's effects after the
    /// `is_awaited` filter, or each project callee's direct and transitive effects when its
    /// body runs there. Shared by Boundary-Only Reporting and the loop rules.
    pub fn site_effects(&self, path: &Path, site: &CallSite) -> Vec<SiteEffect> {
        let hop = |callee: String| CallHop {
            caller: site.caller.clone(),
            callee,
            span: site.span,
            path: path.to_path_buf(),
        };
        let mut out = Vec::new();
        match &site.target {
            CallTarget::Layer2 {
                symbol, effects, ..
            } => {
                for e in effects {
                    out.push(SiteEffect {
                        effect: *e,
                        chain: vec![hop(symbol.clone())],
                        callee: None,
                        callee_is_async: false,
                    });
                }
            }
            CallTarget::Project(callees) => {
                for bc in callees {
                    let Some(callee) = self
                        .summary(&bc.callee)
                        .filter(|callee| callgraph::runs_at_call(callee, site))
                    else {
                        continue;
                    };
                    let effects: BTreeSet<ExecEffectKind> = callee
                        .direct_exec_effects
                        .iter()
                        .chain(&callee.transitive_exec_effects)
                        .copied()
                        .collect();
                    for e in effects {
                        let mut chain = vec![hop(bc.callee.fqn())];
                        chain.extend(callee.exec_chains.get(&e).cloned().unwrap_or_default());
                        out.push(SiteEffect {
                            effect: e,
                            chain,
                            callee: Some(bc.callee.clone()),
                            callee_is_async: callee.is_async,
                        });
                    }
                }
            }
        }
        out
    }

    /// Final summary of `func`, if known.
    pub fn summary(&self, func: &FuncKey) -> Option<&EffectSummary> {
        self.files
            .get(self.func_files.get(func)?)?
            .functions
            .get(func)
    }

    /// Declarations of every indexed module (for re-summarizing an edited module), preferring
    /// `.py` implementations over `.pyi` stubs sharing a module name.
    pub fn project_decls(&self) -> ProjectDecls {
        ProjectDecls::new(self.preferred_decls())
    }

    /// [`Self::project_decls`] with its pass 1, for [`Self::reanalyze_module_cached`].
    pub fn decl_passes(&self) -> DeclPasses {
        DeclPasses::new(self.preferred_decls())
    }

    /// Every module's declarations, `.pyi` stubs first (a later `.py` sharing the module name
    /// replaces them).
    fn preferred_decls(&self) -> Vec<ModuleDecls> {
        let mut entries: Vec<(bool, ModuleDecls)> = self
            .files
            .values()
            .map(|f| (is_stub(&f.path), f.decls.clone()))
            .collect();
        entries.sort_by_key(|(stub, _)| !*stub);
        entries.into_iter().map(|(_, d)| d).collect()
    }

    /// Preferred declarations currently indexed for `module_name` (`.py` over `.pyi`).
    pub fn module_decls(&self, module_name: &str) -> Option<ModuleDecls> {
        let mut stub = None;
        for f in self.files.values().filter(|f| f.module_name == module_name) {
            if !is_stub(&f.path) {
                return Some(f.decls.clone());
            }
            stub = Some(f.decls.clone());
        }
        stub
    }

    /// Re-summarize one edited module against the index's declarations and update it.
    pub fn reanalyze_module(
        &mut self,
        input: &ModuleInput<'_>,
        kb: &EffectiveEcosystemKb,
    ) -> IncrementalUpdate {
        let others: Vec<(bool, ModuleDecls)> = self
            .files
            .values()
            .filter(|f| f.path != input.path)
            .map(|f| (is_stub(&f.path), f.decls.clone()))
            .collect();
        let (decls, own) =
            resolver::project_decls_for(std::slice::from_ref(input), kb, Some(others), true);
        let own = own.into_iter().next().expect("one module in, one out");
        let file = summary::summarize_module(input, &decls, kb, own, true);
        self.update_file(file)
    }

    /// [`Self::reanalyze_module`] against cached project declarations (both passes, kept up
    /// to date in place): the edit costs the size of the module unless its declarations
    /// changed. Also returns what changed in the declarations (empty: nothing), which every
    /// file whose summary or diagnostics read it depends on.
    pub fn reanalyze_module_cached(
        &mut self,
        input: &ModuleInput<'_>,
        kb: &EffectiveEcosystemKb,
        decls: &mut DeclPasses,
    ) -> (IncrementalUpdate, DeclKeys) {
        let masked_by_impl = is_stub(&input.path)
            && self.files.values().any(|f| {
                f.module_name == input.module_name && f.path != input.path && !is_stub(&f.path)
            });
        let (changed, own) = if masked_by_impl {
            (
                DeclKeys::default(),
                resolver::own_module_decls(input, kb, decls),
            )
        } else {
            let (changed, own, reads) = resolver::update_module_decls(input, kb, decls);
            (changed, (own, reads))
        };
        let file = summary::summarize_module(input, decls.completed(), kb, own, true);
        (self.update_file(file), changed)
    }

    /// Update a single file's summary and report whether downstream callers are affected.
    ///
    /// `new_file` carries local facts (`local_functions`, `call_sites`, `call_edges`); its
    /// final summaries are recomputed here. Callers are re-propagated only when a final
    /// summary of the file changed (location-insensitively) — **Early Cutoff**.
    pub fn update_file(&mut self, mut new_file: FileSummary) -> IncrementalUpdate {
        let path = new_file.path.clone();
        let old_file = self.files.remove(&path);
        let mut previous: FxHashMap<FuncKey, EffectSummary> = FxHashMap::default();
        let mut owned_before: FxHashSet<FuncKey> = FxHashSet::default();
        if let Some(old) = &old_file {
            for (k, v) in &old.functions {
                if self.func_files.get(k) == Some(&path) {
                    owned_before.insert(k.clone());
                    previous.insert(k.clone(), v.clone());
                }
            }
            self.remove_file_links(old);
        }
        new_file.functions = new_file.local_functions.clone();
        self.add_file_links(&new_file);
        let own: FxHashSet<FuncKey> = new_file
            .local_functions
            .keys()
            .filter(|k| self.func_files.get(*k) == Some(&path))
            .cloned()
            .collect();
        // `sync_driven_only` can only change for the file's functions and the callees of its
        // old and new call sites.
        let mut sync_candidates: FxHashSet<FuncKey> = own.clone();
        for file in old_file.iter().chain(std::iter::once(&new_file)) {
            sync_candidates.extend(file.local_functions.keys().cloned());
            for (_, callee, _) in &file.call_edges {
                sync_candidates.insert(callee.clone());
            }
        }
        self.files.insert(path, new_file);
        // A function the file stopped defining falls back to another file defining it (a
        // `.pyi` stub next to its `.py` implementation), as a full build links it.
        for func in owned_before.iter().filter(|f| !own.contains(*f)) {
            let fallback = self
                .files
                .values()
                .filter(|f| f.local_functions.contains_key(func))
                .map(|f| (is_stub(&f.path), &f.path))
                .min();
            if let Some((_, other)) = fallback {
                self.func_files.insert(func.clone(), other.clone());
            }
        }

        // Region 0: the file's functions plus every function sharing a (cross-file) SCC with
        // them, so cycles cannot keep stale effects of the old version alive.
        let mut region = callgraph::scc_closure(self, &own);
        for func in &region {
            if !own.contains(func)
                && let Some(s) = self.summary(func)
            {
                previous.insert(func.clone(), s.clone());
            }
        }
        callgraph::reset_to_local(self, &region);
        callgraph::propagate(self, &region);
        let sync_changed = callgraph::recompute_sync_driven_for(self, &sync_candidates);

        let mut changed: BTreeSet<FuncKey> = BTreeSet::new();
        // Same facts, but positions callers copy (call chains, write sites) moved.
        let mut moved: BTreeSet<FuncKey> = BTreeSet::new();
        for func in &region {
            let now = self.summary(func);
            match (previous.get(func), now) {
                (Some(old), Some(new)) if old.same_facts(new) => {
                    if !old.same_for_callers(new) {
                        moved.insert(func.clone());
                    }
                }
                _ => {
                    changed.insert(func.clone());
                }
            }
        }
        for func in owned_before {
            if !own.contains(&func) {
                changed.insert(func);
            }
        }
        changed.extend(sync_changed);

        // Callers whose facts depend on changed summaries (Early Cutoff when there are none),
        // then callers that only copy moved positions: their summaries are refreshed too, so
        // the index keeps what a full build computes, but no caller's facts change.
        let affected = self.callers_closure(&changed, &FxHashSet::default());
        let excluded: FxHashSet<FuncKey> = changed.iter().chain(&affected).cloned().collect();
        let relocated = self.callers_closure(&moved, &excluded);
        let summary_changed = !changed.is_empty();
        let changed: Vec<FuncKey> = changed.into_iter().collect();
        if affected.is_empty() && relocated.is_empty() {
            return IncrementalUpdate {
                summary_changed,
                affected_callers: Vec::new(),
                changed,
            };
        }
        region = affected.iter().chain(&relocated).cloned().collect();
        callgraph::reset_to_local(self, &region);
        callgraph::propagate(self, &region);
        callgraph::recompute_sync_driven_for(self, &sync_candidates);
        IncrementalUpdate {
            summary_changed,
            affected_callers: affected,
            changed,
        }
    }

    /// Transitive callers of `roots` on the reverse call graph, breadth first (sorted per
    /// level), without `roots` and `excluded`.
    fn callers_closure(
        &self,
        roots: &BTreeSet<FuncKey>,
        excluded: &FxHashSet<FuncKey>,
    ) -> Vec<FuncKey> {
        let mut visited: FxHashSet<FuncKey> = roots.iter().chain(excluded).cloned().collect();
        let mut queue: std::collections::VecDeque<FuncKey> = roots.iter().cloned().collect();
        let mut out = Vec::new();
        while let Some(cur) = queue.pop_front() {
            let mut callers: Vec<&FuncKey> = self
                .reverse_call_graph
                .get(&cur)
                .map(|s| s.iter().collect())
                .unwrap_or_default();
            callers.sort();
            for caller in callers {
                if visited.insert(caller.clone()) {
                    out.push(caller.clone());
                    queue.push_back(caller.clone());
                }
            }
        }
        out
    }

    /// Defining file of a summarized function.
    pub fn file_of(&self, func: &FuncKey) -> Option<&Path> {
        self.func_files.get(func).map(PathBuf::as_path)
    }

    /// Drop a deleted file: its functions disappear and their callers are re-propagated (the
    /// update of an empty module, so Early Cutoff and `affected_callers` work as for edits;
    /// an unknown path changes nothing).
    pub fn remove_file(&mut self, path: &Path) -> IncrementalUpdate {
        let empty = FileSummary {
            path: path.to_path_buf(),
            ..FileSummary::default()
        };
        let update = self.update_file(empty);
        self.files.remove(path);
        update
    }

    pub(crate) fn all_functions(&self) -> impl Iterator<Item = FuncKey> + '_ {
        self.files
            .values()
            .flat_map(|f| f.local_functions.keys().cloned())
    }

    fn add_file_links(&mut self, file: &FileSummary) {
        let file_is_stub = is_stub(&file.path);
        for func in file.local_functions.keys() {
            let keep_impl = file_is_stub && self.func_files.get(func).is_some_and(|p| !is_stub(p));
            if !keep_impl {
                self.func_files.insert(func.clone(), file.path.clone());
            }
        }
        for (caller, callee, _) in &file.call_edges {
            self.reverse_call_graph
                .entry(callee.clone())
                .or_default()
                .insert(caller.clone());
        }
    }

    fn remove_file_links(&mut self, file: &FileSummary) {
        for func in file.local_functions.keys() {
            if self.func_files.get(func) == Some(&file.path) {
                self.func_files.remove(func);
            }
        }
        for (caller, callee, _) in &file.call_edges {
            if let Some(set) = self.reverse_call_graph.get_mut(callee) {
                set.remove(caller);
                if set.is_empty() {
                    self.reverse_call_graph.remove(callee);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
