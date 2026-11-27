//! `heki inspect`: what heki knows about a function, read from the index `heki check` builds
//! for the same configuration and selection. The effects it reaches, the inputs and shared
//! state it changes, what it keeps and returns, whether it is pure in heki's sense, who calls
//! it, what `heki check` reports in it, and the contracts naming it. [`Functions`] resolves the
//! symbols of a run and [`Inspector`] builds a report on each function from one analysis.

mod text;

pub use text::render_text;

use crate::eff_rules::effect_name;
use crate::{ContractRoles, DottedPattern, ModuleUnit, has_glob_syntax};
use heki_ast::{LineIndex, Span};
use heki_diagnostics::Diagnostic;
use heki_effect::{
    AttrKeyPath, CallHop, CallsByCaller, CellChange, EffectSummary, EntryKey, FuncKey, GuardTest,
    ParamChange, ProjectDecls, ProjectIndex, SharedState, SiteRole, Slot, TypedFieldWriteSite,
    func_qualname, is_stub,
};
use heki_ir::IrFunction;
use heki_kb::{EffectiveEcosystemKb, EffectiveSoundnessKb, ExecEffectKind};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The version of the JSON object; additions within it are additive only.
pub const INSPECT_VERSION: u32 = 1;
/// The most callers `--callers` lists, and the most direct callers otherwise.
pub const MAX_CALLERS: usize = 500;
pub const MAX_DIRECT_CALLERS: usize = 100;
/// The most functions heki cannot follow `purity.unknown_reach` lists.
pub const MAX_UNKNOWN: usize = 100;
/// The most candidates an ambiguous name lists.
pub const MAX_CANDIDATES: usize = 20;
/// The most suggestions an unknown name lists.
const MAX_SUGGESTIONS: usize = 5;

/// What `heki inspect` is asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Symbol {
    /// A dotted name: a fully qualified name, a dotted suffix, or a glob.
    Name(String),
    /// The function whose definition contains a line of a file.
    Line { path: PathBuf, line: u32 },
    /// A name matched among the functions of one file's module.
    InFile { path: PathBuf, name: String },
}

/// Why a symbol cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolError {
    Empty,
    /// A file without a line or a name.
    BarePath(String),
}

impl std::fmt::Display for SymbolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "the symbol is empty"),
            Self::BarePath(p) => write!(
                f,
                "`{p}` names a file: give a line (`{p}:12`) or a name (`{p}::f`)"
            ),
        }
    }
}

fn is_python_file(text: &str) -> bool {
    text.ends_with(".py") || text.ends_with(".pyi")
}

impl Symbol {
    /// `path.py::name` when the text before the first `::` is a Python file, `path.py:LINE`
    /// when the text after the last `:` is a line number and the text before it a Python file
    /// (a Windows drive letter stays in the path), a dotted name otherwise.
    pub fn parse(text: &str) -> Result<Self, SymbolError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(SymbolError::Empty);
        }
        if let Some((path, name)) = text.split_once("::")
            && is_python_file(path)
            && !name.is_empty()
        {
            return Ok(Self::InFile {
                path: PathBuf::from(path),
                name: name.to_owned(),
            });
        }
        if let Some((path, line)) = text.rsplit_once(':')
            && is_python_file(path)
            && let Ok(line) = line.parse::<u32>()
        {
            return Ok(Self::Line {
                path: PathBuf::from(path),
                line,
            });
        }
        if is_python_file(text) || text.ends_with("::") {
            return Err(SymbolError::BarePath(text.trim_end_matches(':').to_owned()));
        }
        Ok(Self::Name(text.to_owned()))
    }
}

// ------------------------------------------------------------------------------ resolution

/// What a symbol names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Function {
        key: FuncKey,
        /// How the text named it: `None` for an exact fully qualified name.
        from: Option<&'static str>,
    },
    /// Every function a glob matches, when it names each ([`GlobMatches::Each`]).
    Functions {
        keys: Vec<FuncKey>,
        from: &'static str,
    },
    Ambiguous(Vec<FuncKey>),
    Unknown(Vec<FuncKey>),
    /// A class: its methods.
    Class(String, Vec<FuncKey>),
    /// A module: its top-level functions.
    Module(String, Vec<FuncKey>),
}

/// What a glob that matches several functions names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobMatches {
    /// One function: the glob is ambiguous.
    One,
    /// Each function it matches.
    Each,
}

/// The summarized functions a symbol can name, each from its defining file, sorted by key with
/// its fully qualified name: collected once for every symbol of a run.
pub struct Functions<'i> {
    index: &'i ProjectIndex,
    decls: &'i ProjectDecls,
    keys: Vec<(&'i FuncKey, String)>,
}

impl<'i> Functions<'i> {
    pub fn new(index: &'i ProjectIndex, decls: &'i ProjectDecls) -> Self {
        let mut keys: Vec<&FuncKey> = index
            .files
            .iter()
            .flat_map(|(path, f)| {
                f.local_functions
                    .keys()
                    .filter(move |k| index.file_of(k) == Some(path.as_path()))
            })
            .collect();
        keys.sort();
        Self {
            index,
            decls,
            keys: keys.into_iter().map(|k| (k, k.fqn())).collect(),
        }
    }

    /// The functions of `module` (every function for `None`).
    fn of_module(&self, module: Option<&str>) -> &[(&'i FuncKey, String)] {
        let Some(module) = module else {
            return &self.keys;
        };
        let start = self
            .keys
            .partition_point(|(k, _)| k.module.as_str() < module);
        let len = self.keys[start..].partition_point(|(k, _)| k.module == module);
        &self.keys[start..start + len]
    }

    /// The function whose fully qualified name, or within a module whose qualname, is `text`.
    /// Of two functions whose fully qualified names are the same text, the one of the shorter
    /// module sorts first and is the one named.
    fn exact(&self, text: &str, within: Option<&str>) -> Option<&'i FuncKey> {
        let find = |module: &str, qualname: &str| {
            let at = self.keys.binary_search_by(|(k, _)| {
                (k.module.as_str(), k.qualname.as_str()).cmp(&(module, qualname))
            });
            at.ok().map(|i| self.keys[i].0)
        };
        match within {
            Some(module) => find(module, text),
            None => text
                .match_indices('.')
                .find_map(|(i, _)| find(&text[..i], &text[i + 1..])),
        }
    }

    /// Resolve a dotted name: an exact fully qualified name wins, else every key whose fully
    /// qualified name or qualname the text matches as a contract pattern does
    /// ([`DottedPattern`]). `within` limits the keys to one module.
    pub fn resolve_name(&self, text: &str, within: Option<&str>, globs: GlobMatches) -> Resolution {
        if let Some(k) = self.exact(text, within) {
            return Resolution::Function {
                key: k.clone(),
                from: within.map(|_| "name"),
            };
        }
        let keys = self.of_module(within);
        let found: Vec<FuncKey> = match DottedPattern::new(text, "inspect") {
            Ok(p) => keys
                .iter()
                .filter(|(k, fqn)| p.matches(fqn) || p.matches(&k.qualname))
                .map(|(k, _)| (*k).clone())
                .collect(),
            Err(_) => Vec::new(),
        };
        let from = if within.is_some() {
            "name"
        } else if has_glob_syntax(text) {
            "glob"
        } else {
            "suffix"
        };
        match found.len() {
            1 => {
                return Resolution::Function {
                    key: found.into_iter().next().expect("one candidate"),
                    from: Some(from),
                };
            }
            n if n > 1 && globs == GlobMatches::Each && has_glob_syntax(text) => {
                return Resolution::Functions { keys: found, from };
            }
            n if n > 1 => return Resolution::Ambiguous(found),
            _ => {}
        }
        if let Some(class) = named_class(self.decls, text, within) {
            return Resolution::Class(class.clone(), self.decls.methods_of(&class));
        }
        if within.is_none() && self.index.files.values().any(|f| f.module_name == text) {
            let functions = self
                .of_module(Some(text))
                .iter()
                .filter(|(k, _)| !k.qualname.contains('.'))
                .map(|(k, _)| (*k).clone())
                .collect();
            return Resolution::Module(text.to_owned(), functions);
        }
        let last = text.rsplit(['.', ':']).next().unwrap_or(text);
        let name_of = |k: &FuncKey| k.qualname.rsplit('.').next().unwrap_or_default().to_owned();
        let mut names: Vec<String> = keys.iter().map(|(k, _)| name_of(k)).collect();
        names.sort_unstable();
        names.dedup();
        // The functions named `last` in other modules, else those whose names look like it.
        let wanted: Vec<&str> = if names.iter().any(|n| n == last) {
            vec![last]
        } else {
            heki_env::problem::nearest(last, names.iter().map(String::as_str))
        };
        let suggestions = wanted
            .iter()
            .flat_map(|name| keys.iter().filter(move |(k, _)| name_of(k) == *name))
            .take(MAX_SUGGESTIONS)
            .map(|(k, _)| (*k).clone())
            .collect();
        Resolution::Unknown(suggestions)
    }
}

/// The project class `text` names: its fully qualified name, or (within a module, or as a
/// dotted suffix) its qualname.
fn named_class(decls: &ProjectDecls, text: &str, within: Option<&str>) -> Option<String> {
    let fqn = match within {
        Some(module) => format!("{module}.{text}"),
        None => text.to_owned(),
    };
    decls.is_project_class(&fqn).then_some(fqn)
}

/// The function whose definition contains `line` of `unit`: the innermost IR function that is
/// neither a class body nor a lambda or generator expression whose span contains the line, or
/// the module body (which runs the class bodies).
pub fn resolve_line(unit: &ModuleUnit<'_>, line: u32) -> FuncKey {
    let ir = unit.input.ir;
    let innermost = ir
        .functions
        .iter()
        .filter(|f| !f.is_module_body && !f.is_class_body && !f.name.starts_with('<'))
        .filter(|f| {
            let (start, end) = lines_of(unit, f);
            (start..=end).contains(&line)
        })
        .min_by_key(|f| f.span.end - f.span.start);
    let qualname = innermost.map_or("<module>", func_qualname);
    FuncKey {
        module: unit.input.module_name.clone(),
        qualname: qualname.to_owned(),
    }
}

/// The first and last line of a function's definition, decorators included.
fn lines_of(unit: &ModuleUnit<'_>, f: &IrFunction) -> (u32, u32) {
    (
        unit.lines.line(f.span.start),
        unit.lines
            .line(f.span.end.saturating_sub(1).max(f.span.start)),
    )
}

// ---------------------------------------------------------------------------------- report

/// One hop of a call chain: the call at `path:line` of the hop before it (the function itself
/// for the first) calls `calls`, one of `candidates` the call may run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Hop {
    /// The file of the call, relative to the project root.
    pub path: String,
    /// The line of the call.
    pub line: u32,
    /// The function the call runs, or the first of its candidates.
    pub calls: String,
    /// How many functions the call may run.
    pub candidates: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Location {
    /// The file, relative to the project root.
    pub path: String,
    /// The line.
    pub line: u32,
    /// The byte column.
    pub column: u32,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct InspectReport {
    /// The version of this object: 1.
    pub version: u32,
    /// The version of heki that wrote the report.
    pub heki: &'static str,
    /// The project root, as `heki check` prints paths.
    pub root: String,
    /// The symbol, as it was given.
    pub query: String,
    /// How the symbol named the function: `name` (a dotted suffix), `glob`, `suffix`, or `line` (`path.py:LINE`); none for an exact fully qualified name.
    #[schemars(extend("enum" = ["name", "glob", "suffix", "line", null]))]
    pub resolved_from: Option<&'static str>,
    /// Whether every file of the project was analyzed: false when `errors` is not empty.
    pub complete: bool,
    /// The files heki could not read or parse.
    pub errors: Vec<String>,
    /// What the function is.
    pub function: FunctionInfo,
    /// Whether the function is pure, and why heki cannot tell.
    pub purity: Purity,
    /// The effects the function has, directly or through the functions it calls.
    pub effects: Vec<EffectInfo>,
    /// What the function changes.
    pub changes: Changes,
    /// What the function keeps.
    pub retains: Retains,
    /// The container parameters the function only reads.
    pub only_reads: OnlyReads,
    /// What the function returns.
    pub returns: Returns,
    /// The functions that call it.
    pub callers: CallersInfo,
    /// The reports of `heki check` in the function, and with `--callers` those above it.
    pub reports: Vec<Report>,
    /// The roles the function has in the declared contracts.
    pub contracts: Vec<ContractRole>,
    /// The registrations that may reach the function with a parameter bound to the registering object.
    pub registered: Vec<RegisteredContext>,
}

/// A registration that may reach the function with one of its parameters bound to the
/// registering object, under the condition the name a lookup selects the function by meets
/// (`heki_effect::dispatch`). Advisory: no other fact, report, or fix assumes it.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RegisteredContext {
    /// The parameter bound to the registering object.
    pub param: String,
    /// The position of that parameter.
    pub index: u8,
    /// The registering object's class, the parameter's type under the context.
    #[serde(rename = "type")]
    pub class: String,
    /// The factory call of the registration, in the function registering.
    pub registration: Site,
    pub condition: Condition,
    /// The call of the registered slot of the table's row.
    pub called_at: Site,
    /// The lookup by name, and the call of what it finds.
    pub selected_at: Site,
    pub invoked_at: Site,
    /// What the function changes of the parameter under the context: its `changes.params`
    /// record with the parameter of that type (empty: no change).
    pub changes: Vec<ParamChangeInfo>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Site {
    /// The function the site is in.
    pub function: String,
    /// The file, relative to the project root.
    pub path: String,
    /// The line.
    pub line: u32,
    /// The byte column.
    pub column: u32,
}

/// The name looked up equals the function's name.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Condition {
    /// The expression the name is computed from, in the values the row's call passes, or the
    /// constant.
    pub selector: String,
    /// The name the function is selected by.
    pub equals: String,
    /// The name is not a constant: other names may select other functions, or none.
    pub others_open: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct FunctionInfo {
    /// The fully qualified name.
    pub fqn: String,
    /// The module that defines it.
    pub module: String,
    /// The name within the module.
    pub qualname: String,
    /// The file, relative to the project root.
    pub path: String,
    /// The first line of the first definition.
    pub line: u32,
    /// The last line of the first definition.
    pub end_line: u32,
    /// The first line of every definition (`if ...: def f` / `else: def f`).
    pub definitions: Vec<u32>,
    /// `module_body`, `nested`, `cached_property`, `property`, `classmethod`, `staticmethod`, `method`, or `function`.
    #[schemars(extend("enum" = ["module_body", "nested", "cached_property", "property", "classmethod", "staticmethod", "method", "function"]))]
    pub kind: &'static str,
    /// Whether it is `async def`.
    #[serde(rename = "async")]
    pub is_async: bool,
    /// Whether it is a generator.
    pub generator: bool,
    /// Whether it is abstract.
    #[serde(rename = "abstract")]
    pub is_abstract: bool,
    /// Whether only a stub file declares it.
    pub stub: bool,
    /// The qualified name of its class; none for a function outside a class.
    pub class: Option<String>,
    /// Its decorators, as written.
    pub decorators: Vec<String>,
    /// Its parameters, in order.
    pub params: Vec<String>,
    /// The call that runs it in a thread or process pool; none when no call does.
    pub offloaded_by: Option<String>,
    /// Whether a caller drives the coroutine to its end without an event loop.
    pub driven_synchronously: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Purity {
    /// `pure`, `impure`, or `unknown`.
    #[schemars(extend("enum" = ["pure", "impure", "unknown"]))]
    pub verdict: &'static str,
    /// Why the verdict is `unknown`: `unresolved_calls`, `untracked_writes`, or `no_body`; for `impure`, the effects found.
    pub because: Vec<&'static str>,
    /// The first entry of `unknown_reach`, whatever the verdict.
    pub unknown_at: Option<UnknownAt>,
    /// The functions heki cannot follow.
    pub unknown_reach: UnknownReach,
    /// The assumptions the verdict rests on.
    pub assumes: Vec<&'static str>,
}

/// The functions heki cannot follow (calls it cannot resolve, or a body only a stub declares)
/// among the function and the callees whose facts run in it, nearest first.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct UnknownReach {
    /// At most [`MAX_UNKNOWN`].
    pub entries: Vec<UnknownAt>,
    /// More functions are reached than `entries` lists.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct UnknownAt {
    /// The function heki cannot follow.
    pub fqn: String,
    /// The calls from the function to it.
    pub chain: Vec<Hop>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct EffectInfo {
    /// The effect, by its name in the catalogs (`blocking_net`).
    pub effect: &'static str,
    /// Whether it blocks the event loop.
    pub blocking: bool,
    /// Whether the function has it itself, not only through a callee.
    pub direct: bool,
    /// When the body runs: `called`, `awaited`, `iterated`, or `result_iterated`.
    #[schemars(extend("enum" = ["called", "awaited", "iterated", "result_iterated"]))]
    pub runs: &'static str,
    /// The tests that decide whether the effect happens.
    pub guards: Vec<GuardInfo>,
    /// The parameters the effect happens once per item of; none when it does not.
    pub once_per: Option<Vec<String>>,
    /// The calls from the function to the effect.
    pub chain: Vec<Hop>,
    /// The subclasses whose methods have the effect, run only when a parameter is an instance
    /// of one.
    pub via_subclasses: Vec<SubclassChain>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct GuardInfo {
    /// The parameter the test is about.
    pub param: String,
    /// The attribute of the parameter the test is about; none for the parameter itself.
    pub attr: Option<String>,
    /// `truthy`, `is_none`, `isinstance`, `hasattr`, or `catalog`.
    #[schemars(extend("enum" = ["truthy", "is_none", "isinstance", "hasattr", "catalog"]))]
    pub test: &'static str,
    /// `isinstance`: the classes tested.
    pub classes: Vec<String>,
    /// `hasattr`: the attribute tested.
    pub has: Option<String>,
    /// Whether the effect happens when the test holds.
    pub holds: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct SubclassChain {
    /// The parameter the subclass's method is called on (`self` for the method's own).
    pub param: String,
    /// The subclass.
    pub class: String,
    /// The calls from the function to the effect through it.
    pub chain: Vec<Hop>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Changes {
    /// What the function changes of its parameters.
    pub params: Vec<ParamChangeInfo>,
    /// The attributes of `self` it changes.
    pub self_attrs: Vec<SelfAttrChange>,
    /// What it invalidates under an `invalidation` contract.
    pub invalidates: Vec<String>,
    /// The attributes of classes it writes; none when heki does not follow typed writes.
    pub class_attrs: Option<Vec<ClassAttrWrite>>,
    /// Why `class_attrs` is none: what turns the analysis on.
    pub class_attrs_needs: Option<&'static str>,
    /// The module globals and closure variables it changes.
    pub globals: Vec<GlobalChange>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ParamChangeInfo {
    /// The parameter.
    pub param: String,
    /// The position of the parameter.
    pub index: u8,
    /// `""`: the object itself; `a`: its attribute `a`.
    pub what: Vec<String>,
    /// The keys or attributes changed, as `a` or `["k"]`.
    pub slots: Vec<String>,
    /// Whether the function changes it itself, not only through a callee.
    pub direct: bool,
    /// The calls from the function to the change.
    pub chain: Vec<Hop>,
    /// Where the change is; none when it is in a callee.
    pub at: Option<Location>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct SelfAttrChange {
    /// The attribute.
    pub attr: String,
    /// The constant key changed within the attribute; none for the attribute itself.
    pub key: Option<String>,
    /// Whether the function changes it itself, not only through a callee.
    pub direct: bool,
    /// The calls from the function to the change.
    pub chain: Vec<Hop>,
    /// Where the change is; none when it is in a callee.
    pub at: Option<Location>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ClassAttrWrite {
    /// The qualified name of the class.
    pub class: String,
    /// The attribute.
    pub attr: String,
    /// How it is written: `direct_attr`, `attr_delete`, `container_mut`, `alias_container_mut`, `set_attr`, `dict_direct`, `indirect_helper`, `item_mut`, or `indirect_item_mut`.
    #[schemars(extend("enum" = ["direct_attr", "attr_delete", "container_mut", "alias_container_mut", "set_attr", "dict_direct", "indirect_helper", "item_mut", "indirect_item_mut"]))]
    pub kind: &'static str,
    /// Whether the written object is new in the function.
    pub fresh: bool,
    /// The entries of the attribute's container the function's own and passed item writes
    /// change: `"init"` for a constant key, the parameter's name for a key its callers pass
    /// (`key`), `?` for a key heki cannot tell.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    /// Whether every write changes one of `keys`: none changes the container as a whole
    /// (`extras.clear()` next to `extras["init"] = v` does).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub entries_only: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct GlobalChange {
    /// The global or closure variable.
    pub name: String,
    /// The function whose variable the closure cell is (`None`: a module global).
    pub cell_of: Option<String>,
    /// Whether the function changes it itself, not only through a callee.
    pub direct: bool,
    /// The calls from the function to the change.
    pub chain: Vec<Hop>,
    /// Where the change is.
    pub at: Location,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Retains {
    /// The parameters the function stores.
    pub stored: Vec<String>,
    /// The parameters it keeps (stores, captures, yields, or returns); none without an ownership contract.
    pub kept: Option<Vec<String>>,
    /// Why `kept` is none: what turns the analysis on.
    pub kept_needs: Option<&'static str>,
}

/// The parameters annotated with a mutable container (`list`, `dict`, `set`, their `typing`
/// forms, `MutableSequence`, `MutableMapping`, `MutableSet`), and what the function does with
/// them (`heki_effect::ContainerParams`).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OnlyReads {
    /// The parameters annotated with a mutable container.
    pub containers: Vec<String>,
    /// Those the function only reads: it and every function it hands one to neither change it
    /// in place nor keep it (store, capture, yield, or return it), and every call it is handed
    /// to resolves to such a function or to a library call Layer 1 lists as reading it.
    pub params: Vec<String>,
    /// Those whose every use in the function's own body the read-only type (`Sequence`,
    /// `Mapping`, `AbstractSet`) allows.
    pub read_only_uses: Vec<String>,
    /// Those whose name the function binds once (`if p is None: p = []` aside).
    pub bound_once: Vec<String>,
    /// By parameter the function only reads, annotated or not, the kinds of read that reach its
    /// object here and in the functions it is handed to (`heki_effect::ReadKinds`): `item
    /// access`, `iteration`, ..., then the methods called by name.
    pub reads: BTreeMap<String, Vec<&'static str>>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Returns {
    /// `generator`, `coroutine`, or `value`.
    #[schemars(extend("enum" = ["generator", "coroutine", "value"]))]
    pub kind: &'static str,
    /// Where the returned object comes from; none when heki does not know.
    pub origins: Option<OriginsInfo>,
    /// Whether it is a new object: `own`, `copy`, or `no`; none without an ownership contract.
    #[schemars(extend("enum" = ["own", "copy", "no", null]))]
    pub new_object: Option<&'static str>,
    /// The producer whose read-only object it returns; none otherwise.
    pub shared_with: Option<SharedWith>,
    /// The attributes of `self` it returns.
    pub self_attrs: Vec<String>,
    /// The effects iterating the returned object has.
    pub iterated_effects: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OriginsInfo {
    /// Whether it may be a new object.
    pub new: bool,
    /// Whether it may be an object something else holds.
    pub held: bool,
    /// The parameters it may be.
    pub params: Vec<String>,
    /// The parameters whose items it may be.
    pub elements_of: Vec<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct SharedWith {
    /// The function that produces the read-only object.
    pub producer: String,
    /// The calls from the function to it.
    pub chain: Vec<Hop>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct CallersInfo {
    /// How many callers there are.
    pub count: usize,
    /// How many levels of callers are listed.
    pub depth: usize,
    /// Whether `entries` lists fewer than there are.
    pub truncated: bool,
    /// The callers, nearest first.
    pub entries: Vec<CallerEntry>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct CallerEntry {
    /// The caller.
    pub fqn: String,
    /// The level: 1 for a direct caller.
    pub depth: usize,
    /// The function it calls, for a caller above the direct ones.
    pub via: Option<String>,
    /// The file of the call, relative to the project root.
    pub path: String,
    /// The line of the call.
    pub line: u32,
    /// What the caller does with the result: `call`, `property`, `iterate`, `return`, or `through`.
    #[schemars(extend("enum" = ["call", "property", "iterate", "return", "through"]))]
    pub role: &'static str,
    /// Whether the call is awaited.
    pub awaited: bool,
    /// Whether the call is in a loop.
    pub in_loop: bool,
    /// How many functions the call may run.
    pub candidates: usize,
}

/// A diagnostic of `heki check` with what the baseline makes of it and where it is.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Report {
    #[serde(flatten)]
    pub diagnostic: Diagnostic,
    /// `allowed` / `regression` with a baseline, `None` without.
    #[schemars(extend("enum" = ["allowed", "regression", null]))]
    pub baseline: Option<&'static str>,
    /// `in` the function, or in a caller and `through` it.
    #[schemars(extend("enum" = ["in", "through"]))]
    #[serde(rename = "where")]
    pub place: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ContractRole {
    /// The contract table: `write-owners`, `read-owners`, `call-owners`, `effect-boundaries`, `readonly-returns`, `invalidation`, `import-boundaries`, or `ecosystem`.
    #[schemars(extend("enum" = ["write-owners", "read-owners", "call-owners", "effect-boundaries", "readonly-returns", "invalidation", "import-boundaries", "ecosystem"]))]
    pub kind: &'static str,
    /// The name of the contract.
    pub name: String,
    /// What the function is in the contract, for instance `allowed-writer` or `reported`.
    pub roles: Vec<&'static str>,
}

/// What the reports are built from: the analysis, and what `heki check` reports in the files of
/// the functions inspected. The callbacks give the parts that differ between callers.
pub struct InspectInputs<'a> {
    pub index: &'a ProjectIndex,
    pub decls: &'a ProjectDecls,
    pub kb: &'a EffectiveEcosystemKb,
    /// Layer 1, for summarizing a module again under a registration context.
    pub soundness: Option<&'a EffectiveSoundnessKb>,
    /// The project root, absolute.
    pub root: &'a Path,
    /// Whether an ownership contract is declared (`retains.kept`, `returns.new_object`).
    pub ownership: bool,
    /// The IR of the files of the functions inspected, for their definitions; a file that does
    /// not parse has none.
    pub units: &'a [ModuleUnit<'a>],
    /// The project-relative path of a file.
    pub rel_of: &'a dyn Fn(&Path) -> String,
    /// The text of a file, for the lines of hops in it.
    pub text_of: &'a dyn Fn(&Path) -> Option<String>,
    /// The diagnostics `heki check` reports in the files of the functions inspected (and with
    /// `callers`, in their callers' files), in its order, each with its baseline status.
    pub reports: Vec<(Diagnostic, Option<&'static str>)>,
    /// The roles of functions in the contracts.
    pub contracts: &'a ContractRoles<'a>,
    /// Files that could not be read or parsed.
    pub errors: Vec<String>,
}

/// What was asked: the text, and how far up to list callers.
pub struct InspectQuery<'q> {
    pub text: &'q str,
    pub resolved_from: Option<&'static str>,
    /// `--callers`: transitive callers up to `depth` levels and the reports above the function.
    pub callers: bool,
    pub depth: usize,
}

const ASSUMES: &[&str] = &[
    "libraries outside the ecosystem catalog have no effects",
    "objects of unknown origin changed in place are not tracked",
];
const TYPED_WRITES_NEED: &str =
    "an ownership contract, MUT108, MUT112, or MUT120 propagates writes to other objects";
const OWNERSHIP_NEEDS: &str =
    "an ownership contract (write-owners, read-owners, effect-boundaries, invalidation)";

/// The lines of the files hops and sites are in, each file's computed once.
struct Lines<'a> {
    text_of: &'a dyn Fn(&Path) -> Option<String>,
    files: RefCell<BTreeMap<PathBuf, Option<LineIndex>>>,
}

impl Lines<'_> {
    fn line_col(&self, path: &Path, offset: u32) -> (u32, u32) {
        let mut files = self.files.borrow_mut();
        let lines = files
            .entry(path.to_path_buf())
            .or_insert_with(|| (self.text_of)(path).map(|text| LineIndex::from_source(&text)));
        lines.as_ref().map_or((0, 0), |l| l.line_col(offset))
    }
}

struct Builder<'a, 'i> {
    inputs: &'a InspectInputs<'i>,
    lines: Lines<'a>,
    calls: RefCell<CallsByCaller<'i>>,
    /// How many project functions each call site may run, by file, caller, and position,
    /// indexed one file at a time on the first hop into it.
    candidates: RefCell<FxHashMap<&'i Path, CandidatesByCall<'i>>>,
    units: FxHashMap<&'a Path, &'a ModuleUnit<'i>>,
    /// The positions in [`InspectInputs::reports`] of the diagnostics of each file.
    reports_by_file: FxHashMap<&'a Path, Vec<usize>>,
    /// The registration contexts of the project, joined on the first report that asks.
    contexts: std::cell::OnceCell<Vec<heki_effect::dispatch::Context>>,
}

/// How many project functions each call site of one file may run, by caller and position.
type CandidatesByCall<'i> = FxHashMap<(&'i FuncKey, Span), usize>;

impl<'i> Builder<'_, 'i> {
    /// The IR of `path`, a file of a function inspected (`None` when it does not parse).
    fn unit(&self, path: &Path) -> Option<&ModuleUnit<'i>> {
        self.units.get(path).copied()
    }

    fn rel(&self, path: &Path) -> String {
        (self.inputs.rel_of)(path)
    }

    fn location(&self, path: &Path, span: Span) -> Location {
        let (line, column) = self.lines.line_col(path, span.start);
        Location {
            path: self.rel(path),
            line,
            column,
        }
    }

    /// How many project functions the call at `span` of `caller` in `path` may run (1 for a
    /// call that is not a project call site).
    fn candidates_at(&self, path: &Path, caller: &FuncKey, span: Span) -> usize {
        let index: &'i ProjectIndex = self.inputs.index;
        let (file_path, file) = index
            .files
            .get_key_value(path)
            .expect("call chains run through indexed files");
        let mut by_file = self.candidates.borrow_mut();
        let sites = by_file.entry(file_path.as_path()).or_insert_with(|| {
            let mut sites = FxHashMap::default();
            for s in &file.call_sites {
                sites
                    .entry((&s.caller, s.span))
                    .or_insert_with(|| s.project_callees().len().max(1));
            }
            sites
        });
        sites.get(&(caller, span)).copied().unwrap_or(1)
    }

    fn hop(&self, hop: &CallHop) -> Hop {
        let candidates = self.candidates_at(&hop.path, &hop.caller, hop.span);
        Hop {
            path: self.rel(&hop.path),
            line: self.lines.line_col(&hop.path, hop.span.start).0,
            calls: hop.callee.clone(),
            candidates,
        }
    }

    fn hops<'h>(&self, chain: impl IntoIterator<Item = &'h CallHop>) -> Vec<Hop> {
        chain.into_iter().map(|h| self.hop(h)).collect()
    }
}

/// The name of parameter `i` (`#i` past the known names).
fn param_name(i: u8, names: &[String]) -> String {
    names
        .get(usize::from(i))
        .cloned()
        .unwrap_or_else(|| format!("#{i}"))
}

/// The names of the positions of `key`'s facts: its parameters, then the enclosing functions'
/// parameters it reads through closure cells ([`heki_effect::CapturedParams`]).
fn with_captured_params(
    decls: &ProjectDecls,
    key: &FuncKey,
    mut names: Vec<String>,
) -> Vec<String> {
    let Some(captured) = decls
        .function_decl(key)
        .and_then(|d| d.captured_params.as_deref())
    else {
        return names;
    };
    while names.len() < usize::from(captured.first) {
        names.push(format!("#{}", names.len()));
    }
    names.extend(captured.params.iter().map(|(owner, i)| {
        let owner = FuncKey {
            module: key.module.clone(),
            qualname: owner.clone(),
        };
        decls
            .function_decl(&owner)
            .and_then(|d| d.param_names.get(usize::from(*i)).cloned())
            .unwrap_or_else(|| format!("#{i}"))
    }));
    names
}

/// The names of the parameters in `mask`.
fn param_names(mask: u64, names: &[String]) -> Vec<String> {
    (0..64u8)
        .filter(|i| mask & (1u64 << i) != 0)
        .map(|i| param_name(i, names))
        .collect()
}

fn slot_text(slot: &Slot) -> String {
    match slot {
        Slot::Attr(a) => a.clone(),
        Slot::Key(k) => format!("[{k:?}]"),
    }
}

/// The reports on the functions of one analysis ([`Self::report`]): they share what they look up
/// in it, the calls of each function, the lines of each file, the candidates of each call site,
/// and the diagnostics of each file.
pub struct Inspector<'a, 'i>(Builder<'a, 'i>);

impl<'a, 'i> Inspector<'a, 'i> {
    pub fn new(inputs: &'a InspectInputs<'i>) -> Self {
        let index = inputs.index;
        assert!(
            index.opt_in_facts().shared_state,
            "heki inspect reads an index with heki_rules::inspect_facts"
        );
        let mut reports_by_file: FxHashMap<&Path, Vec<usize>> = FxHashMap::default();
        for (i, (d, _)) in inputs.reports.iter().enumerate() {
            reports_by_file.entry(d.path.as_path()).or_default().push(i);
        }
        Self(Builder {
            inputs,
            lines: Lines {
                text_of: inputs.text_of,
                files: RefCell::default(),
            },
            calls: RefCell::new(index.calls_by_caller()),
            candidates: RefCell::default(),
            units: inputs
                .units
                .iter()
                .map(|u| (u.input.path.as_path(), u))
                .collect(),
            reports_by_file,
            contexts: std::cell::OnceCell::new(),
        })
    }

    /// The report on `key`, a summarized function.
    pub fn report(&self, key: &FuncKey, query: &InspectQuery<'_>) -> InspectReport {
        let b = &self.0;
        let inputs = b.inputs;
        let index = inputs.index;
        let summary = index
            .summary(key)
            .expect("the resolved function is summarized");
        let local = index
            .local_summary(key)
            .expect("the resolved function is summarized");
        let path = index
            .file_of(key)
            .expect("the resolved function is summarized");
        let decl = inputs.decls.function_decl(key);
        let own_params: Vec<String> = decl.map(|d| d.param_names.clone()).unwrap_or_default();
        let function = function_info(b, key, path, summary, &own_params);
        let params = with_captured_params(inputs.decls, key, own_params);
        let effects = effects(b, summary, &params);
        let changes = changes(b, key, summary, local, &params);
        let retains = Retains {
            stored: param_names(summary.escapes_params, &params),
            kept: inputs
                .ownership
                .then(|| param_names(summary.retains_params, &params)),
            kept_needs: (!inputs.ownership).then_some(OWNERSHIP_NEEDS),
        };
        let containers = summary
            .container_params
            .as_deref()
            .cloned()
            .unwrap_or_default();
        let only_reads = OnlyReads {
            containers: param_names(containers.annotated, &params),
            params: param_names(summary.only_read_containers(), &params),
            read_only_uses: param_names(containers.abstract_use, &params),
            bound_once: param_names(containers.bound_once, &params),
            reads: (0..64u8)
                .filter(|i| summary.only_read_mask() & (1u64 << i) != 0)
                .map(|i| (param_name(i, &params), summary.read_kinds_of(i).names()))
                .filter(|(_, kinds)| !kinds.is_empty())
                .collect(),
        };
        let returns = returns(b, summary, &params);
        let callers = callers(b, key, query);
        let reports = reports(b, key, path, query, &callers);
        let shown: Vec<&Diagnostic> = reports.iter().map(|r| &r.diagnostic).collect();
        let contracts = inputs.contracts.of(key, &shown);
        let purity = purity(b, key, summary, &effects, &changes, function.stub);
        let registered = self.registered(key, &params);
        InspectReport {
            version: INSPECT_VERSION,
            heki: env!("CARGO_PKG_VERSION"),
            root: inputs.root.to_string_lossy().into_owned(),
            query: query.text.to_owned(),
            resolved_from: query.resolved_from,
            complete: inputs.errors.is_empty(),
            errors: inputs.errors.clone(),
            function,
            purity,
            effects,
            changes,
            retains,
            only_reads,
            returns,
            callers: callers.info,
            reports,
            contracts,
            registered,
        }
    }

    /// The registration contexts reaching `key`, each with the change record of its parameter
    /// under the context (one analysis per parameter and class).
    fn registered(&self, key: &FuncKey, params: &[String]) -> Vec<RegisteredContext> {
        let b = &self.0;
        let (index, decls) = (b.inputs.index, b.inputs.decls);
        let all = b
            .contexts
            .get_or_init(|| heki_effect::dispatch::contexts(index, decls));
        let mut under: Vec<((u8, &str), Vec<ParamChangeInfo>)> = Vec::new();
        let mut out = Vec::new();
        for c in all.iter().filter(|c| c.handler == *key) {
            let premise = (c.param, c.class.as_str());
            let changes = match under.iter().find(|(p, _)| *p == premise) {
                Some((_, changes)) => changes.clone(),
                None => {
                    let changes = self.changes_under(key, c, params);
                    under.push((premise, changes.clone()));
                    changes
                }
            };
            let site = |function: &FuncKey, span: Span| {
                let at = b.location(
                    index.file_of(function).expect("a summarized function"),
                    span,
                );
                Site {
                    function: function.fqn(),
                    path: at.path,
                    line: at.line,
                    column: at.column,
                }
            };
            out.push(RegisteredContext {
                param: param_name(c.param, params),
                index: c.param,
                class: c.class.clone(),
                registration: site(&c.registered_in, c.registration),
                condition: Condition {
                    selector: c.selector.clone(),
                    equals: c.equals.clone(),
                    others_open: c.others_open,
                },
                called_at: site(&c.called_in, c.called_at),
                selected_at: site(&c.selected_in, c.selected_at),
                invoked_at: site(&c.invoked_in, c.invoked_at),
                changes,
            });
        }
        out
    }

    /// The `changes.params` record of the context's parameter in an index where it has the
    /// context's type.
    fn changes_under(
        &self,
        key: &FuncKey,
        c: &heki_effect::dispatch::Context,
        params: &[String],
    ) -> Vec<ParamChangeInfo> {
        let inputs = self.0.inputs;
        let path = inputs.index.file_of(key).expect("a summarized function");
        let unit = self
            .0
            .unit(path)
            .expect("the file of an inspected function");
        let scratch = (inputs.index).with_param_type(
            &unit.input,
            inputs.kb,
            inputs.soundness,
            key,
            c.param,
            &c.class,
        );
        let scratch_inputs = InspectInputs {
            index: &scratch,
            decls: inputs.decls,
            kb: inputs.kb,
            soundness: inputs.soundness,
            root: inputs.root,
            ownership: inputs.ownership,
            units: inputs.units,
            rel_of: inputs.rel_of,
            text_of: inputs.text_of,
            reports: Vec::new(),
            contracts: inputs.contracts,
            errors: Vec::new(),
        };
        let under = Inspector::new(&scratch_inputs);
        let summary = scratch.summary(key).expect("a summarized function");
        let local = scratch.local_summary(key).expect("a summarized function");
        let changes = changes(&under.0, key, summary, local, params);
        (changes.params.into_iter())
            .filter(|p| p.index == c.param)
            .collect()
    }
}

fn function_info(
    b: &Builder<'_, '_>,
    key: &FuncKey,
    path: &Path,
    s: &EffectSummary,
    params: &[String],
) -> FunctionInfo {
    let decl = b.inputs.decls.function_decl(key);
    let definitions: Vec<(u32, u32)> = b
        .unit(path)
        .into_iter()
        .flat_map(|u| {
            u.input
                .ir
                .functions
                .iter()
                .filter(|f| func_qualname(f) == key.qualname && !f.is_class_body)
                .map(move |f| lines_of(u, f))
        })
        .collect();
    let (line, end_line) = definitions.first().copied().unwrap_or((0, 0));
    let kind = if key.qualname == "<module>" {
        "module_body"
    } else if key.qualname.contains("<locals>") {
        "nested"
    } else if s.is_cached_property {
        "cached_property"
    } else if decl.is_some_and(|d| d.is_property) {
        "property"
    } else if decl.is_some_and(|d| d.is_classmethod) {
        "classmethod"
    } else if decl.is_some_and(|d| d.is_staticmethod) {
        "staticmethod"
    } else if s.owner_class_qualname.is_some() {
        "method"
    } else {
        "function"
    };
    FunctionInfo {
        fqn: key.fqn(),
        module: key.module.clone(),
        qualname: key.qualname.clone(),
        path: b.rel(path),
        line,
        end_line,
        definitions: definitions.iter().map(|(start, _)| *start).collect(),
        kind,
        is_async: s.is_async,
        generator: s.is_generator,
        is_abstract: s.is_abstract(),
        stub: is_stub(path),
        class: s
            .owner_class_qualname
            .as_ref()
            .map(|c| format!("{}.{c}", key.module)),
        decorators: s.decorators.clone(),
        params: params.to_vec(),
        offloaded_by: decl.and_then(|d| d.offloaded_by.clone()),
        driven_synchronously: s.sync_driven_only,
    }
}

/// How the body whose facts these are runs: at the call, when awaited, when the generator it
/// returns is iterated, or when the object a plain function returns is iterated.
fn runs_of(s: &EffectSummary, result: bool) -> &'static str {
    if result {
        "result_iterated"
    } else if s.is_generator {
        "iterated"
    } else if s.is_async {
        "awaited"
    } else {
        "called"
    }
}

fn effects(b: &Builder<'_, '_>, s: &EffectSummary, params: &[String]) -> Vec<EffectInfo> {
    let mut out = Vec::new();
    for (facts, result) in [(Some(s), false), (s.iterated.as_deref(), true)] {
        let Some(facts) = facts else { continue };
        let all: BTreeSet<ExecEffectKind> = facts
            .direct_exec_effects
            .iter()
            .chain(&facts.transitive_exec_effects)
            .chain(facts.provided_effects.iter().flat_map(|r| r.keys()))
            .copied()
            .collect();
        for e in all {
            let guards = facts.exec_guards.get(&e);
            out.push(EffectInfo {
                effect: effect_name(e),
                blocking: e.blocks(),
                direct: facts.direct_exec_effects.contains(&e),
                runs: runs_of(s, result),
                guards: guards
                    .into_iter()
                    .flat_map(|g| &g.inputs)
                    .map(|g| GuardInfo {
                        param: param_name(g.subject.param, params),
                        attr: g.subject.attr.clone(),
                        test: match g.test {
                            GuardTest::Truthy => "truthy",
                            GuardTest::IsNone => "is_none",
                            GuardTest::IsInstance(_) => "isinstance",
                            GuardTest::HasAttr(_) => "hasattr",
                            GuardTest::Value(_) => "catalog",
                        },
                        classes: match &g.test {
                            GuardTest::IsInstance(c) => c.clone(),
                            _ => Vec::new(),
                        },
                        has: g.test.attr().map(str::to_owned),
                        holds: g.holds,
                    })
                    .collect(),
                once_per: guards
                    .and_then(|g| g.once_per)
                    .map(|mask| param_names(mask, params)),
                chain: b.hops(
                    facts
                        .exec_chains
                        .get(&e)
                        .into_iter()
                        .flatten()
                        .map(|h| &**h),
                ),
                via_subclasses: facts
                    .provided_effects
                    .iter()
                    .flat_map(|r| r.get(&e))
                    .flatten()
                    .map(|way| SubclassChain {
                        param: param_name(way.param, params),
                        class: way.provider.class().to_owned(),
                        chain: b.hops(way.chain.iter().map(|h| &**h)),
                    })
                    .collect(),
            });
        }
    }
    out
}

/// The write statement a typed write site stands for: its span in the function that writes.
fn write_of(t: &TypedFieldWriteSite) -> (Span, Option<&str>) {
    (t.span, t.via_chain.last().map(|h| h.callee.as_str()))
}

fn changes(
    b: &Builder<'_, '_>,
    key: &FuncKey,
    s: &EffectSummary,
    local: &EffectSummary,
    params: &[String],
) -> Changes {
    let index = b.inputs.index;
    let mut param_changes = Vec::new();
    // A parameter changed at a path (itself, an attribute), or in the container held in a slot
    // of it (`p["k"].append(x)`).
    let changed: BTreeSet<u8> = s
        .mutated_param_paths
        .keys()
        .chain(
            s.inplace_param_slots
                .iter()
                .filter(|(_, v)| !v.is_empty())
                .map(|(k, _)| k),
        )
        .copied()
        .collect();
    let no_paths = BTreeSet::new();
    for i in changed {
        let paths = s.mutated_param_paths.get(&i).unwrap_or(&no_paths);
        let name = param_name(i, params);
        let slots: Vec<String> = s
            .inplace_param_slots
            .get(&i)
            .into_iter()
            .flatten()
            .map(slot_text)
            .collect();
        let direct = local.mutated_param_paths.contains_key(&i)
            || local
                .inplace_param_slots
                .get(&i)
                .is_some_and(|v| !v.is_empty());
        let what = if paths.contains("") {
            Some(ParamChange::Itself)
        } else {
            s.inplace_param_slots
                .get(&i)
                .and_then(|slots| slots.first().cloned())
                .map(ParamChange::InSlot)
        };
        let chain = match what.filter(|_| !direct) {
            Some(what) => index.param_change_chain(&mut b.calls.borrow_mut(), key, i, &what),
            None => heki_effect::ChangeChain::default(),
        };
        let at = if direct {
            first_change_site(b, key, i)
        } else {
            chain
                .changed_in
                .as_ref()
                .zip(chain.param)
                .and_then(|(f, j)| first_change_site(b, f, j))
        };
        param_changes.push(ParamChangeInfo {
            param: name,
            index: i,
            what: paths.iter().cloned().collect(),
            slots,
            direct,
            chain: b.hops(&chain.hops),
            at,
        });
    }
    let self_attrs = s
        .mutates_self_attrs
        .iter()
        .map(|a| self_attr(b, key, local, a))
        .collect();
    let entry = |site: &TypedFieldWriteSite| match (&site.key, site.key_from) {
        (Some(k), _) => Some(format!("{k:?}")),
        (None, EntryKey::Param(p)) => Some(param_name(p, params)),
        (None, EntryKey::Unknown) => Some("?".to_owned()),
        (None, EntryKey::Written) => None,
    };
    let class_attrs = index.propagates_typed_writes().then(|| {
        // The facts of one attribute differ in the objects written too, which are not shown.
        let mut writes: Vec<ClassAttrWrite> = s
            .typed_field_writes
            .iter()
            .map(|w| {
                let sites: Vec<&TypedFieldWriteSite> = (s.typed_field_write_sites.iter())
                    .filter(|t| {
                        t.class_qualname == w.class_qualname && t.attr == w.attr && t.kind == w.kind
                    })
                    .collect();
                let keys: BTreeSet<String> = sites.iter().filter_map(|t| entry(t)).collect();
                // A callee's write keyed by its parameter is also kept as a write of the
                // container, next to the copy with the key the call passes, which names its
                // entry: only another write changes the container as a whole.
                let passed: FxHashSet<(Span, Option<&str>)> = (sites.iter())
                    .filter(|t| t.passed)
                    .map(|t| write_of(t))
                    .collect();
                let whole =
                    (sites.iter()).any(|t| entry(t).is_none() && !passed.contains(&write_of(t)));
                ClassAttrWrite {
                    class: w.class_qualname.clone(),
                    attr: w.attr.clone(),
                    kind: w.kind.as_str(),
                    fresh: w.fresh,
                    entries_only: !keys.is_empty() && !whole,
                    keys: keys.into_iter().collect(),
                }
            })
            .collect();
        writes.dedup_by(|a, b| {
            (&a.class, &a.attr, a.kind, a.fresh) == (&b.class, &b.attr, b.kind, b.fresh)
        });
        writes
    });
    Changes {
        params: param_changes,
        self_attrs,
        invalidates: s.invalidates_cached.iter().cloned().collect(),
        class_attrs_needs: class_attrs.is_none().then_some(TYPED_WRITES_NEED),
        class_attrs,
        globals: globals(b, key, s),
    }
}

/// The first of the own changes of parameter `j` of `func` (the function a chain ends at).
fn first_change_site(b: &Builder<'_, '_>, func: &FuncKey, j: u8) -> Option<Location> {
    let index = b.inputs.index;
    let span = index
        .local_summary(func)?
        .param_change_sites
        .get(&j)?
        .first()?;
    Some(b.location(index.file_of(func)?, *span))
}

fn self_attr(
    b: &Builder<'_, '_>,
    key: &FuncKey,
    local: &EffectSummary,
    a: &AttrKeyPath,
) -> SelfAttrChange {
    let index = b.inputs.index;
    let direct = local.mutates_self_attrs.contains(a);
    let chain = if direct {
        heki_effect::ChangeChain::default()
    } else {
        index.self_attr_chain(&mut b.calls.borrow_mut(), key, a)
    };
    // The spans of a method's own changes of `self` are not kept per attribute: one change is
    // the attribute's when it is the only one.
    let end = if direct {
        Some(key)
    } else {
        chain.changed_in.as_ref()
    };
    let at = end.and_then(|f| {
        let spans = index.local_summary(f)?.param_change_sites.get(&0)?;
        let only = spans.first().filter(|_| spans.len() == 1)?;
        Some(b.location(index.file_of(f)?, *only))
    });
    SelfAttrChange {
        attr: a.attr.clone(),
        key: a.const_key.clone(),
        direct,
        chain: b.hops(&chain.hops),
        at,
    }
}

fn globals(b: &Builder<'_, '_>, key: &FuncKey, s: &EffectSummary) -> Vec<GlobalChange> {
    let index = b.inputs.index;
    let mut out = changes_made_in(b, key, None, Vec::new())
        .into_iter()
        .filter(|g| {
            g.cell
                .as_ref()
                .is_none_or(|c| s.changes_cells().contains(c))
        })
        .map(|g| g.change)
        .collect::<Vec<_>>();
    // What reaches the function through calls: every function changing a module global, then
    // a closure cell, nearest first; one entry per global and cell, its first.
    let globals = s.changes_globals().then_some(None);
    let cells = s.changes_cells().iter().map(Some);
    for cell in globals.into_iter().chain(cells) {
        let chains = index.state_changes(&mut b.calls.borrow_mut(), key, cell);
        for chain in chains {
            let hops = b.hops(&chain.hops);
            for end in chain.changed_in.iter().filter(|end| *end != key) {
                out.extend(
                    changes_made_in(b, end, cell, hops.clone())
                        .into_iter()
                        .map(|g| g.change),
                );
            }
        }
    }
    let mut seen = FxHashSet::default();
    out.retain(|g| seen.insert((g.name.clone(), g.cell_of.clone())));
    out
}

/// A change of shared state [`changes_made_in`] lists, with the cell it changes.
struct MadeChange {
    change: GlobalChange,
    cell: Option<CellChange>,
}

/// The changes of shared state (of `cell`, or of every global and cell when `cell` is `None`
/// and `chain` is empty) that the body of `func` makes, reached through `chain`: its own, and
/// those of the callees it hands the state to, one hop further.
fn changes_made_in(
    b: &Builder<'_, '_>,
    func: &FuncKey,
    cell: Option<&CellChange>,
    chain: Vec<Hop>,
) -> Vec<MadeChange> {
    let index = b.inputs.index;
    let every = cell.is_none() && chain.is_empty();
    // A value whose roots the walk left open changes a global it does not name.
    let named =
        |state: &SharedState| !matches!(state, SharedState::Global(name) if name.is_empty());
    let wanted = |state: &SharedState| named(state) && (every || state.is_change_of(cell));
    let direct = chain.is_empty();
    let made = |state: &SharedState, chain: Vec<Hop>, at: Location, direct: bool| {
        let (name, cell_of, cell) = match state {
            SharedState::Global(name) => (name.clone(), None, None),
            SharedState::Cell(c) => (
                c.name.clone(),
                Some(format!("{}.{}", func.module, c.owner)),
                Some(c.clone()),
            ),
        };
        MadeChange {
            change: GlobalChange {
                name,
                cell_of,
                direct,
                chain,
                at,
            },
            cell,
        }
    };
    let path = index.file_of(func).expect("summarized");
    let local = index.local_summary(func).expect("summarized");
    let mut out: Vec<MadeChange> = local
        .changed_state()
        .iter()
        .filter(|(state, _)| wanted(state))
        .map(|(state, span)| made(state, chain.clone(), b.location(path, *span), direct))
        .collect();
    let handed = index.handed_changes_of(&mut b.calls.borrow_mut(), func);
    for h in handed.into_iter().filter(|h| wanted(h.state)) {
        let hop = CallHop {
            caller: func.clone(),
            callee: h.callee.callee.fqn(),
            span: h.site.span,
            path: path.to_path_buf(),
        };
        let mut hops = chain.clone();
        hops.push(b.hop(&hop));
        let at = first_change_site(b, &h.callee.callee, h.param)
            .unwrap_or_else(|| b.location(path, h.site.span));
        out.push(made(h.state, hops, at, false));
    }
    out
}

fn returns(b: &Builder<'_, '_>, s: &EffectSummary, params: &[String]) -> Returns {
    let o = s
        .return_origins
        .as_deref()
        .map(|r| r.returned.known)
        .unwrap_or_default();
    let known = o != heki_effect::Origins::default();
    Returns {
        // An async generator is iterated, never awaited.
        kind: if s.is_generator {
            "generator"
        } else if s.is_async {
            "coroutine"
        } else {
            "value"
        },
        origins: known.then(|| OriginsInfo {
            new: o.new,
            held: o.held,
            params: param_names(o.params, params),
            elements_of: param_names(o.elements, params),
        }),
        new_object: b.inputs.ownership.then(|| match s.returns_new.kind() {
            Some(heki_effect::NewKind::Own) => "own",
            Some(heki_effect::NewKind::Copy) => "copy",
            None => "no",
        }),
        shared_with: s.returns_protected.as_ref().map(|p| SharedWith {
            producer: p.producer.clone(),
            chain: b.hops(p.chain.iter().map(|h| &**h)),
        }),
        self_attrs: s.returns_self_attrs.iter().cloned().collect(),
        iterated_effects: s
            .iterated
            .iter()
            .flat_map(|i| {
                i.direct_exec_effects
                    .iter()
                    .chain(&i.transitive_exec_effects)
            })
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(effect_name)
            .collect(),
    }
}

/// The callers listed and the functions they are, for the reports above the function.
struct Found {
    info: CallersInfo,
    /// Every caller found with its file.
    functions: BTreeSet<(PathBuf, String)>,
}

fn callers(b: &Builder<'_, '_>, key: &FuncKey, query: &InspectQuery<'_>) -> Found {
    let index = b.inputs.index;
    let (depth, limit) = if query.callers {
        (query.depth, MAX_CALLERS)
    } else {
        (1, MAX_DIRECT_CALLERS)
    };
    let found = index.callers_within(&mut b.calls.borrow_mut(), key, depth, limit);
    let entries: Vec<CallerEntry> = found
        .sites
        .iter()
        .map(|c| {
            let call = c.sites[0];
            // A property read, else what the caller does with the result when it uses it.
            let used = c
                .sites
                .iter()
                .map(|s| &s.role)
                .find(|r| **r != SiteRole::Call);
            let role = match used {
                _ if is_property_read(b, c.callee) => "property",
                Some(SiteRole::Iterate) => "iterate",
                Some(SiteRole::Return) => "return",
                Some(_) => "through",
                None => "call",
            };
            let (line, _) = b.lines.line_col(c.path, call.span.start);
            CallerEntry {
                fqn: c.caller.fqn(),
                depth: c.depth,
                via: (c.callee != key).then(|| c.callee.fqn()),
                path: b.rel(c.path),
                line,
                role,
                awaited: call.is_awaited,
                in_loop: call.loop_depth > 0,
                candidates: call.project_callees().len(),
            }
        })
        .collect();
    Found {
        functions: found
            .sites
            .iter()
            .map(|c| (c.path.to_path_buf(), c.caller.qualname.clone()))
            .collect(),
        info: CallersInfo {
            count: entries.len(),
            depth,
            truncated: found.truncated,
            entries,
        },
    }
}

fn is_property_read(b: &Builder<'_, '_>, callee: &FuncKey) -> bool {
    b.inputs
        .decls
        .function_decl(callee)
        .is_some_and(|d| d.is_property)
}

/// The diagnostics in the function, and with `--callers` those of its callers whose related
/// locations pass through it: a call site whose project callees include it, or a location
/// inside its definition.
fn reports(
    b: &Builder<'_, '_>,
    key: &FuncKey,
    path: &Path,
    query: &InspectQuery<'_>,
    found: &Found,
) -> Vec<Report> {
    let index = b.inputs.index;
    let spans: Vec<Span> = b
        .unit(path)
        .into_iter()
        .flat_map(|u| &u.input.ir.functions)
        .filter(|f| func_qualname(f) == key.qualname && !f.is_class_body)
        .map(|f| f.span)
        .collect();
    let calls_it = |p: &Path, span: Span| {
        index.files.get(p).is_some_and(|file| {
            file.call_sites
                .iter()
                .any(|s| s.span == span && s.project_callees().iter().any(|bc| bc.callee == *key))
        })
    };
    let passes = |d: &Diagnostic| {
        d.related.iter().any(|r| {
            calls_it(&r.path, r.span)
                || (r.path == path
                    && spans
                        .iter()
                        .any(|s| s.start <= r.span.start && r.span.end <= s.end))
        })
    };
    // The diagnostics of the function's file and of its callers' files, in their order.
    let callers_files = found.functions.iter().map(|(p, _)| p.as_path());
    let files = std::iter::once(path).chain(callers_files.filter(|_| query.callers));
    let mut at: Vec<usize> = files
        .filter_map(|p| b.reports_by_file.get(p))
        .flatten()
        .copied()
        .collect();
    at.sort_unstable();
    at.dedup();
    let mut out = Vec::new();
    for (d, baseline) in at.into_iter().map(|i| &b.inputs.reports[i]) {
        let place = if d.path == path && d.qualname == key.qualname {
            "in"
        } else if query.callers
            && found
                .functions
                .contains(&(d.path.clone(), d.qualname.clone()))
            && passes(d)
        {
            "through"
        } else {
            continue;
        };
        let mut diagnostic = d.clone();
        diagnostic.path = PathBuf::from(b.rel(&d.path));
        for r in &mut diagnostic.related {
            r.path = PathBuf::from(b.rel(&r.path));
        }
        out.push(Report {
            diagnostic,
            baseline: *baseline,
            place,
        });
    }
    out
}

fn purity(
    b: &Builder<'_, '_>,
    key: &FuncKey,
    s: &EffectSummary,
    effects: &[EffectInfo],
    changes: &Changes,
    stub: bool,
) -> Purity {
    let index = b.inputs.index;
    let self_of_method = |c: &ParamChangeInfo| c.index == 0 && s.owner_class_qualname.is_some();
    // Impure rests on evidence; it wins over unknown.
    let impure: Vec<&'static str> = [
        ("effects", !effects.is_empty()),
        (
            "changes_params",
            changes.params.iter().any(|c| !self_of_method(c)),
        ),
        (
            "changes_self",
            !changes.self_attrs.is_empty()
                || !changes.invalidates.is_empty()
                || changes.params.iter().any(self_of_method),
        ),
        (
            "writes_objects",
            changes.class_attrs.iter().flatten().any(|w| !w.fresh),
        ),
        (
            "changes_globals",
            s.changes_globals() || !s.changes_cells().is_empty(),
        ),
    ]
    .into_iter()
    .filter_map(|(reason, holds)| holds.then_some(reason))
    .collect();
    let mut calls = b.calls.borrow_mut();
    let mut unknown = index.unknown_within(&mut calls, key, MAX_UNKNOWN + 1);
    let truncated = unknown.len() > MAX_UNKNOWN;
    unknown.truncate(MAX_UNKNOWN);
    let unknown_reach = UnknownReach {
        entries: unknown
            .iter()
            .filter_map(|u| {
                Some(UnknownAt {
                    fqn: u.changed_in.as_ref()?.fqn(),
                    chain: b.hops(&u.hops),
                })
            })
            .collect(),
        truncated,
    };
    let unknown_at = unknown_reach.entries.first().cloned();
    if !impure.is_empty() {
        return Purity {
            verdict: "impure",
            because: impure,
            unknown_at,
            unknown_reach,
            assumes: ASSUMES.to_vec(),
        };
    }
    let untracked = !index.propagates_typed_writes()
        && index
            .first_object_write(&mut calls, key)
            .changed_in
            .is_some();
    // The nearest function heki cannot follow: unresolved calls, or a body only a stub declares.
    let stub_below = unknown
        .first()
        .and_then(|u| index.file_of(u.changed_in.as_ref()?))
        .is_some_and(is_stub);
    let because: Vec<&'static str> = [
        ("unresolved_calls", unknown_at.is_some() && !stub_below),
        ("untracked_writes", untracked),
        ("no_body", stub || s.is_abstract() || stub_below),
    ]
    .into_iter()
    .filter_map(|(reason, holds)| holds.then_some(reason))
    .collect();
    Purity {
        verdict: if because.is_empty() {
            "pure"
        } else {
            "unknown"
        },
        because,
        unknown_at,
        unknown_reach,
        assumes: ASSUMES.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Symbol, SymbolError};
    use std::path::PathBuf;

    #[test]
    fn symbols_parse_as_names_lines_and_names_in_files() {
        let line = |p: &str, line| Symbol::Line {
            path: PathBuf::from(p),
            line,
        };
        let in_file = |p: &str, name: &str| Symbol::InFile {
            path: PathBuf::from(p),
            name: name.to_owned(),
        };
        let name = |n: &str| Symbol::Name(n.to_owned());
        for (text, expected) in [
            ("pkg.mod.f", Ok(name("pkg.mod.f"))),
            ("Class.method", Ok(name("Class.method"))),
            ("*.render", Ok(name("*.render"))),
            ("  f  ", Ok(name("f"))),
            ("app/users.py:42", Ok(line("app/users.py", 42))),
            ("stubs/users.pyi:7", Ok(line("stubs/users.pyi", 7))),
            (r"C:\app\m.py:12", Ok(line(r"C:\app\m.py", 12))),
            (
                "app/users.py::Loader.load",
                Ok(in_file("app/users.py", "Loader.load")),
            ),
            ("app/users.py::f:3", Ok(in_file("app/users.py", "f:3"))),
            // Not a Python file before the separator: a dotted name.
            ("mod:12", Ok(name("mod:12"))),
            ("app.py:x", Ok(name("app.py:x"))),
            (
                "app/users.py",
                Err(SymbolError::BarePath("app/users.py".to_owned())),
            ),
            (
                "app/users.py::",
                Err(SymbolError::BarePath("app/users.py".to_owned())),
            ),
            ("", Err(SymbolError::Empty)),
        ] {
            assert_eq!(Symbol::parse(text), expected, "{text}");
        }
    }
}
