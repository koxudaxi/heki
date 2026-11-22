//! Project discovery and parsing shared by `heki check`, `heki snapshot`, `--watch`, and the
//! LSP server: configuration lookup, file discovery (`.gitignore` / `.hekiignore` / `exclude`),
//! parallel parse → semantic model → IR, inline-suppression filtering, and the index the
//! commands that report from `heki check`'s analysis build.

use crate::{
    CheckExtras, ModuleUnit, RuleConfigError, RuleSelection, SummarySnapshot, check_project,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use heki_ast::{LineIndex, Span};
use heki_diagnostics::{Diagnostic, InlineIgnores};
use heki_effect::{
    Contracts, ModuleInput, ModuleLayout, OptInFacts, ProjectIndex, TypedWrites, UnreadModule,
    analyze_project_for,
};
use heki_env::{ConfigError, HekiConfig, PythonVersion, RuntimeProfile};
use heki_ir::ModuleIr;
use heki_kb::{EcosystemPatch, EffectiveEcosystemKb, EffectiveSoundnessKb, KbError};
use heki_semantic::SemanticModel;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

/// Directories skipped during discovery unless `exclude` replaces this list (Ruff-style).
pub const DEFAULT_EXCLUDE: &[&str] = &[
    ".bzr",
    ".direnv",
    ".eggs",
    ".git",
    ".hg",
    ".mypy_cache",
    ".nox",
    ".pants.d",
    ".ruff_cache",
    ".svn",
    ".tox",
    ".venv",
    "__pypackages__",
    "_build",
    "buck-out",
    "build",
    "dist",
    "node_modules",
    "site-packages",
    "venv",
];

/// The files that hold a configuration and so mark the project root.
const CONFIG_MARKERS: &[&str] = &["heki.toml", ".heki.toml", "pyproject.toml"];

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Kb(#[from] KbError),
    #[error("unknown target version `{0}` (expected py311..py315)")]
    TargetVersion(String),
    #[error("invalid {what} glob `{pattern}`: {cause}")]
    Glob {
        pattern: String,
        what: &'static str,
        cause: globset::Error,
    },
    #[error("path `{0}` does not exist")]
    MissingPath(PathBuf),
    #[error("failed to walk `{path}`: {cause}")]
    Walk { path: PathBuf, cause: ignore::Error },
}

/// One parsed module (IR included: callers drop it once summarized when it must not stay
/// resident, e.g. the LSP server).
pub struct ParsedFile {
    /// Display path (`./pkg/a.py` -> `pkg/a.py`, absolute paths kept absolute).
    pub path: PathBuf,
    /// Project-relative POSIX path.
    pub rel: String,
    pub module_name: String,
    pub is_package: bool,
    /// [`ModuleLayout::root_is_package`] of the project.
    pub root_is_package: bool,
    pub source: String,
    pub semantic: SemanticModel,
    pub ir: ModuleIr,
    pub comments: Vec<Span>,
}

/// The module imports `builtins` or touches `__builtins__` when it runs: it may patch builtin
/// names, and then no module of the project can trust them (Assumption A1(b)). A typing stub
/// never runs (`import builtins` there only spells annotations).
pub fn runtime_patches_builtins(input: &ModuleInput<'_>) -> bool {
    !heki_effect::is_stub(&input.path) && heki_soundness::module_patches_builtins(input.ir)
}

impl ParsedFile {
    /// [`runtime_patches_builtins`] of this module.
    pub fn patches_builtins(&self) -> bool {
        runtime_patches_builtins(&self.input())
    }

    pub fn input(&self) -> ModuleInput<'_> {
        ModuleInput {
            path: self.path.clone(),
            module_name: self.module_name.clone(),
            is_package: self.is_package,
            root_is_package: self.root_is_package,
            content_hash: String::new(),
            semantic: &self.semantic,
            ir: &self.ir,
        }
    }

    pub fn unit(&self) -> ModuleUnit<'_> {
        ModuleUnit {
            input: self.input(),
            source: &self.source,
            rel_path: self.rel.clone(),
            lines: LineIndex::from_source(&self.source),
        }
    }
}

/// The absolute path of `path` with every symbolic link resolved: the one form in which heki
/// compares paths. On Windows it is written without the `\\?\` prefix that
/// `std::fs::canonicalize` adds, as editors and users write paths (`C:\...`, `\\server\share\...`);
/// Rust's file functions add that prefix again where a path needs it.
pub fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path).map(without_verbatim_prefix)
}

#[cfg(windows)]
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    use std::ffi::OsString;
    use std::path::Prefix;
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return path;
    };
    let mut plain = OsString::new();
    match prefix.kind() {
        Prefix::VerbatimDisk(drive) => plain.push(format!("{}:", char::from(drive))),
        Prefix::VerbatimUNC(server, share) => {
            plain.push(r"\\");
            plain.push(server);
            plain.push(r"\");
            plain.push(share);
        }
        _ => return path,
    }
    let mut out = PathBuf::from(plain);
    out.push(components.as_path());
    out
}

#[cfg(not(windows))]
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    path
}

/// The canonical form of `path`, or `path` itself when it cannot be resolved (it does not exist,
/// e.g. an unsaved document, or no longer exists).
fn canonical_or_given(path: &Path) -> PathBuf {
    canonical(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Project-relative POSIX path (falls back to the display path outside the root).
pub fn rel_posix(path: &Path, root: &Path) -> String {
    let abs = canonical_or_given(path);
    let rel = abs.strip_prefix(root).unwrap_or(path);
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Display form of a discovered path (`./pkg/a.py` -> `pkg/a.py`).
pub fn display_path(path: &Path) -> PathBuf {
    path.strip_prefix(".")
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

/// The project root of a run from `working_dir` whose first path is `start` (a file or a
/// directory): the nearest directory at or above `start` holding a configuration file
/// ([`CONFIG_MARKERS`]), searched up to the top of the enclosing Git working tree (the directory
/// holding `.git`; a configuration above it belongs to another project). Without one, the root is
/// `working_dir` when that holds `start`, as Ruff and ty take it, but never a directory above the
/// top of the working tree: then the top itself. Outside both, it is `start`'s own directory.
pub fn find_root(start: &Path, working_dir: &Path) -> PathBuf {
    let start = canonical_or_given(start);
    let dir = if start.is_dir() {
        start.clone()
    } else {
        start.parent().map(Path::to_path_buf).unwrap_or_default()
    };
    let mut git_top = None;
    for d in dir.ancestors() {
        if CONFIG_MARKERS.iter().any(|m| d.join(m).exists()) {
            return d.to_path_buf();
        }
        if d.join(".git").exists() {
            git_top = Some(d);
            break;
        }
    }
    let working_dir = canonical_or_given(working_dir);
    let holds_start = dir.starts_with(&working_dir);
    match git_top {
        Some(top) if !(holds_start && working_dir.starts_with(top)) => top.to_path_buf(),
        _ if holds_start => working_dir,
        _ => dir,
    }
}

fn glob_set(patterns: &[String], what: &'static str) -> Result<GlobSet, ProjectError> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        // `generated` also excludes everything below it.
        for pat in [p.clone(), format!("{p}/**")] {
            b.add(Glob::new(&pat).map_err(|cause| ProjectError::Glob {
                pattern: p.clone(),
                what,
                cause,
            })?);
        }
    }
    b.build().map_err(|cause| ProjectError::Glob {
        pattern: patterns.join(", "),
        what,
        cause,
    })
}

/// Excluded when the project-relative path or any of its components matches.
fn is_excluded(rel: &str, set: &GlobSet) -> bool {
    set.is_match(rel) || rel.split('/').any(|c| set.is_match(c))
}

/// The `exclude` / `extend-exclude` set of a configuration.
pub fn exclude_set(cfg: &HekiConfig) -> Result<GlobSet, ProjectError> {
    let mut exclude: Vec<String> = cfg
        .exclude
        .clone()
        .unwrap_or_else(|| DEFAULT_EXCLUDE.iter().map(|s| (*s).to_owned()).collect());
    exclude.extend(cfg.extend_exclude.iter().cloned());
    glob_set(&exclude, "exclude")
}

/// The files whose changes change which files discovery finds (`.hekiignore` and the ignore
/// files the walk reads besides it).
pub const IGNORE_FILES: &[&str] = &[".gitignore", ".ignore", ".hekiignore"];

/// The walk of a directory `collect_files` makes: hidden files and directories, `.gitignore`
/// (inside a Git repository), `.ignore`, and `.hekiignore` files leave entries out.
fn walk(dir: &Path) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(dir);
    builder.add_custom_ignore_filename(".hekiignore");
    builder
}

/// A file the walk keeps when no ignore file or `exclude` leaves it out.
fn walked_python(path: &Path, root: &Path, exclude: &GlobSet) -> bool {
    path.extension().is_some_and(|e| e == "py" || e == "pyi")
        && !is_excluded(&rel_posix(path, root), exclude)
}

/// Python files under `paths` (explicitly named files are always included, Ruff semantics).
pub fn collect_files(
    paths: &[PathBuf],
    root: &Path,
    exclude: &GlobSet,
) -> Result<Vec<PathBuf>, ProjectError> {
    let mut files = Vec::new();
    for p in paths {
        if !p.exists() {
            return Err(ProjectError::MissingPath(p.clone()));
        }
        if p.is_file() {
            files.push(p.clone());
            continue;
        }
        let mut builder = walk(p);
        // An excluded directory is never entered (it may not even be readable).
        let (dir_root, dir_exclude) = (root.to_path_buf(), exclude.clone());
        builder.filter_entry(move |e| {
            !e.file_type().is_some_and(|t| t.is_dir())
                || !is_excluded(&rel_posix(e.path(), &dir_root), &dir_exclude)
        });
        for entry in builder.build() {
            let entry = entry.map_err(|cause| ProjectError::Walk {
                path: p.clone(),
                cause,
            })?;
            let ep = entry.path();
            if ep.is_file() && walked_python(ep, root, exclude) {
                files.push(ep.to_path_buf());
            }
        }
    }
    let mut seen = FxHashSet::default();
    files.retain(|f| seen.insert(canonical_or_given(f)));
    files.sort();
    Ok(files)
}

/// Whether [`collect_files`] over the directory `dir` finds `file` (both absolute): the same
/// walk, entering only the directories on the way to `file`. Of a file that does not exist (an
/// unsaved document, maybe in a directory that does not exist either), the walk judges the part
/// of the path on the disk; the rest only by its names (hidden ones are skipped) and `exclude`:
/// the ignore patterns naming it apply once it is on the disk.
pub fn discovers(dir: &Path, root: &Path, exclude: &GlobSet, file: &Path) -> bool {
    if !file.starts_with(dir) || !walked_python(file, root, exclude) {
        return false;
    }
    let Some(on_disk) = file
        .ancestors()
        .take_while(|a| a.starts_with(dir))
        .find(|a| a.exists())
        .map(Path::to_path_buf)
    else {
        return false;
    };
    let unsaved_hidden = file.strip_prefix(&on_disk).is_ok_and(|rest| {
        rest.components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
    });
    if unsaved_hidden {
        return false;
    }
    if on_disk == dir {
        return true;
    }
    let target = on_disk.clone();
    let mut builder = walk(dir);
    builder.filter_entry(move |e| target.starts_with(e.path()));
    builder
        .build()
        .flatten()
        .any(|e| e.path() == on_disk && e.depth() > 0)
}

/// Parse `source` as the module at `path` (`Err`: its first syntax error).
pub fn parse_source(
    path: &Path,
    root: &Path,
    source: String,
    layout: &ModuleLayout,
    runtime: RuntimeProfile,
) -> Result<ParsedFile, FileError> {
    let parsed = heki_ast::parse_module(&source).map_err(|errs| {
        let index = LineIndex::from_source(&source);
        let first = &errs[0];
        FileError {
            path: display_path(path),
            position: Some(index.line_col(first.span.start)),
            message: format!("syntax error: {} ({} error(s))", first.message, errs.len()),
        }
    })?;
    let (rel, module_name, is_package) = module_identity(path, root, layout);
    let semantic = heki_semantic::analyze_module(&parsed, &source, runtime);
    let ir = heki_ir::build_module_ir(&parsed, &source, &semantic, runtime);
    Ok(ParsedFile {
        path: display_path(path),
        rel,
        module_name,
        is_package,
        root_is_package: layout.root_is_package,
        comments: parsed.comment_spans(),
        source,
        semantic,
        ir,
    })
}

/// The project-relative POSIX path of the file at `path`, the module it defines, and whether
/// that module is a package. A file outside a module layout defines the module of its stem.
fn module_identity(path: &Path, root: &Path, layout: &ModuleLayout) -> (String, String, bool) {
    let rel = rel_posix(path, root);
    let (module_name, is_package) = layout.module_name(Path::new(&rel)).unwrap_or_else(|| {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        (stem, false)
    });
    (rel, module_name, is_package)
}

/// The file at `path` that heki could not read or parse, as the module it would define.
pub fn unread_module(path: &Path, root: &Path, layout: &ModuleLayout) -> UnreadModule {
    let (_, module_name, is_package) = module_identity(path, root, layout);
    UnreadModule {
        path: display_path(path),
        module_name,
        is_package,
    }
}

/// Where a run looks for files and configuration.
#[derive(Default)]
pub struct ProjectOptions<'a> {
    pub paths: &'a [PathBuf],
    pub config: Option<&'a Path>,
    /// The directory the run works from, where a project without a configuration file is
    /// rooted ([`find_root`]): the workspace folder of the language server. `None`: the current
    /// directory.
    pub working_dir: Option<&'a Path>,
    pub target_version: Option<&'a str>,
    /// In-memory sources replacing files on disk (keyed like [`ParsedFile::path`]); one that is
    /// not on the disk is a file of the project when discovery would find it there.
    pub overrides: Option<&'a FxHashMap<PathBuf, String>>,
    /// Files the caller knows are gone although the disk may still list them (a deletion the
    /// editor reported; on Windows a deleted file stays listed until its last handle closes):
    /// discovery leaves them out, and the module layout counts them absent. An open document's
    /// path does not belong here: its text is in `overrides`.
    pub removed: Option<&'a FxHashSet<PathBuf>>,
}

/// Configuration and knowledge bases of a project root.
pub struct ProjectEnv {
    pub root: PathBuf,
    pub cfg: HekiConfig,
    pub runtime: RuntimeProfile,
    pub kb: EffectiveEcosystemKb,
    pub soundness_kb: EffectiveSoundnessKb,
    pub layout: ModuleLayout,
}

impl ProjectEnv {
    pub fn load(opts: &ProjectOptions<'_>) -> Result<Self, ProjectError> {
        let first = opts
            .paths
            .first()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("."));
        let working_dir = match opts.working_dir {
            Some(dir) => dir.to_path_buf(),
            None => std::env::current_dir().unwrap_or_else(|_| first.clone()),
        };
        let root = find_root(&first, &working_dir);
        let cfg = match opts.config {
            Some(path) => HekiConfig::load_from_file(path)?,
            None => HekiConfig::load_from_dir(&root)?,
        };
        let runtime = match opts.target_version.or(cfg.target_version.as_deref()) {
            Some(t) => PythonVersion::parse_tag(t)
                .map(RuntimeProfile::from)
                .ok_or_else(|| ProjectError::TargetVersion(t.to_owned()))?,
            None => cfg.runtime_profile(),
        };
        let overrides = cfg
            .ecosystem
            .iter()
            .map(EcosystemPatch::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let kb = EffectiveEcosystemKb::load_builtin(&overrides)?;
        let soundness_kb = EffectiveSoundnessKb::load_builtin(runtime.python_version)?;
        let removed = opts.removed;
        let layout = ModuleLayout::detect_without(&root, &cfg, &|p| {
            removed.is_some_and(|r| r.contains(&display_path(p)))
        });
        Ok(Self {
            root,
            cfg,
            runtime,
            kb,
            soundness_kb,
            layout,
        })
    }
}

/// A file that could not be read or parsed (the CLI exits with 2 after its report).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileError {
    /// Keyed like [`ParsedFile::path`].
    pub path: PathBuf,
    /// The 1-based line and column of a syntax error.
    pub position: Option<(u32, u32)>,
    pub message: String,
}

impl FileError {
    /// A read error of the file at `path`.
    pub fn unreadable(path: &Path, cause: &impl std::fmt::Display) -> Self {
        Self {
            path: display_path(path),
            position: None,
            message: cause.to_string(),
        }
    }

    /// `path:line:col: message` (`path: message` without a position), with the path written as
    /// `path_text` (the caller's choice: `heki check` writes it relative to the working directory).
    pub fn report(&self, path_text: &str) -> String {
        match self.position {
            Some((line, col)) => format!("{path_text}:{line}:{col}: {}", self.message),
            None => format!("{path_text}: {}", self.message),
        }
    }
}

/// With the path as reports write it ([`heki_diagnostics::path_text`]).
impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.report(&heki_diagnostics::path_text(&self.path)))
    }
}

/// A discovered, parsed project.
pub struct Project {
    pub env: ProjectEnv,
    pub files: Vec<ParsedFile>,
    pub errors: Vec<FileError>,
    /// The files of `errors` that heki could not read or parse: modules it knows nothing about.
    pub unread: Vec<UnreadModule>,
}

impl Project {
    pub fn load(opts: &ProjectOptions<'_>) -> Result<Self, ProjectError> {
        let env = ProjectEnv::load(opts)?;
        let exclude = exclude_set(&env.cfg)?;
        let paths = if opts.paths.is_empty() {
            vec![PathBuf::from(".")]
        } else {
            opts.paths.to_vec()
        };
        let mut found = collect_files(&paths, &env.root, &exclude)?;
        if let Some(removed) = opts.removed {
            found.retain(|p| !removed.contains(&display_path(p)));
        }
        // An open document that is not on the disk yet belongs to the project as it will once
        // saved (`discovers`); its text is in `overrides`.
        if let Some(overrides) = opts.overrides {
            let unsaved = overrides.keys().filter(|p| {
                !p.exists() && paths.iter().any(|d| discovers(d, &env.root, &exclude, p))
            });
            found.extend(unsaved.cloned());
            found.sort();
        }
        let results: Vec<Result<ParsedFile, FileError>> = found
            .par_iter()
            .map(|p| {
                let source = match opts.overrides.and_then(|o| o.get(&display_path(p))) {
                    Some(text) => Ok(text.clone()),
                    None => heki_ast::read_source(p).map_err(|e| FileError::unreadable(p, &e)),
                };
                source
                    .and_then(|source| parse_source(p, &env.root, source, &env.layout, env.runtime))
            })
            .collect();
        let mut files = Vec::new();
        let mut errors = Vec::new();
        let mut unread = Vec::new();
        for (r, p) in results.into_iter().zip(&found) {
            match r {
                Ok(parsed) => files.push(parsed),
                Err(e) => {
                    unread.push(unread_module(p, &env.root, &env.layout));
                    errors.push(e);
                }
            }
        }
        Ok(Self {
            env,
            files,
            errors,
            unread,
        })
    }

    pub fn inputs(&self) -> Vec<ModuleInput<'_>> {
        self.files.iter().map(ParsedFile::input).collect()
    }

    pub fn units(&self) -> Vec<ModuleUnit<'_>> {
        self.files.iter().map(ParsedFile::unit).collect()
    }

    /// The index of the project's files with `facts`.
    pub fn index(&self, facts: IndexFacts) -> ProjectIndex {
        let inputs = self.inputs();
        analyze_project_for(
            &inputs,
            &self.unread,
            &self.env.kb,
            facts.contracts,
            facts.typed_writes,
            facts.opt_in,
        )
    }
}

/// Which facts an index holds besides those the selected rules read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Facts {
    /// None: `heki check` and its `--watch` engine.
    Check,
    /// Also what `heki inspect` shows ([`crate::inspect_facts`]): `heki inspect` and the language
    /// server.
    Inspect,
}

/// What an index is built with: the contracts its summaries depend on, whether typed writes
/// propagate, and the opt-in facts it extracts.
pub struct IndexFacts {
    pub contracts: Option<Arc<dyn Contracts>>,
    pub typed_writes: TypedWrites,
    pub opt_in: OptInFacts,
}

impl IndexFacts {
    /// The facts the rules `selection` runs on `config` read, with `facts`.
    pub fn of(
        facts: Facts,
        config: &HekiConfig,
        kb: &EffectiveEcosystemKb,
        selection: &RuleSelection,
    ) -> Result<Self, RuleConfigError> {
        let opt_in = match facts {
            Facts::Check => crate::opt_in_facts(selection),
            Facts::Inspect => crate::inspect_facts(selection),
        };
        Ok(Self {
            contracts: crate::summary_contracts(config, kb)?,
            typed_writes: crate::typed_writes(config, selection),
            opt_in,
        })
    }

    /// Every fact, with no contract: what a summary snapshot records (`MUT103`).
    pub fn snapshot() -> Self {
        Self {
            contracts: None,
            typed_writes: TypedWrites::Propagated,
            opt_in: OptInFacts::ALL,
        }
    }
}

/// A loaded project with the analysis `heki check` runs on it: the rule selection, the saved
/// summary snapshot `MUT103` compares with, and the index built with the configuration's
/// contracts and the selection's typed-write mode.
pub struct Indexed {
    pub project: Project,
    pub selection: RuleSelection,
    pub snapshot: Option<SummarySnapshot>,
    pub index: ProjectIndex,
}

impl Indexed {
    /// Index `project` for the rules `selection` runs, with `facts`.
    pub fn build(
        project: Project,
        selection: RuleSelection,
        snapshot: Option<SummarySnapshot>,
        facts: Facts,
    ) -> Result<Self, RuleConfigError> {
        let env = &project.env;
        let index = project.index(IndexFacts::of(facts, &env.cfg, &env.kb, &selection)?);
        Ok(Self {
            project,
            selection,
            snapshot,
            index,
        })
    }

    /// The diagnostics of every enabled rule in the project's files, after inline suppressions
    /// and without the heuristic reports the selection withholds, and how many those are.
    pub fn diagnose(&self) -> Result<(Vec<Diagnostic>, usize), RuleConfigError> {
        self.diagnose_units(&self.project.units())
    }

    /// [`Self::diagnose`] in `units` (some or all of the project's files): the rules read the
    /// other files through the index only.
    pub fn diagnose_units(
        &self,
        units: &[ModuleUnit<'_>],
    ) -> Result<(Vec<Diagnostic>, usize), RuleConfigError> {
        let env = &self.project.env;
        let decls = self.index.project_decls();
        let sources: FxHashMap<&Path, &str> = self
            .project
            .files
            .iter()
            .map(|f| (f.path.as_path(), f.source.as_str()))
            .collect();
        let text_of = |p: &Path| sources.get(p).map(|t| (*t).to_owned());
        let extras = CheckExtras {
            soundness_kb: &env.soundness_kb,
            snapshot: self.snapshot.as_ref(),
            decls: Some(&decls),
            project_patches_builtins: Some(
                self.project.files.iter().any(ParsedFile::patches_builtins),
            ),
            text_of: Some(&text_of),
        };
        let mut diags = check_project(
            units,
            &env.cfg,
            &env.kb,
            &self.index,
            &self.selection,
            &extras,
        )?;
        retain_unsuppressed(&mut diags, &self.project.files);
        let withheld = self.selection.withhold_heuristic(&mut diags);
        Ok((diags, withheld))
    }
}

/// Drop diagnostics suppressed by `# heki: ignore[...]` / `# noqa` comments of their file.
pub fn retain_unsuppressed<'f>(
    diags: &mut Vec<Diagnostic>,
    files: impl IntoIterator<Item = &'f ParsedFile>,
) {
    let ignores: FxHashMap<&Path, InlineIgnores> = files
        .into_iter()
        .map(|p| {
            (
                p.path.as_path(),
                InlineIgnores::from_comments(&p.source, &p.comments),
            )
        })
        .collect();
    diags.retain(|d| {
        !ignores
            .get(d.path.as_path())
            .is_some_and(|ign| ign.is_suppressed(d.line, &d.rule_code, &d.rule_slug))
    });
}

#[cfg(all(test, windows))]
mod tests {
    use super::without_verbatim_prefix;
    use std::path::PathBuf;

    #[test]
    fn verbatim_drive_and_unc_paths_lose_the_prefix_and_others_stay() {
        for (given, plain) in [
            (r"\\?\C:\Users\me\pkg\a.py", r"C:\Users\me\pkg\a.py"),
            (r"\\?\C:\", r"C:\"),
            (r"\\?\UNC\server\share\pkg\a.py", r"\\server\share\pkg\a.py"),
            (r"C:\Users\me\pkg\a.py", r"C:\Users\me\pkg\a.py"),
            (
                r"\\?\GLOBALROOT\Device\X\a.py",
                r"\\?\GLOBALROOT\Device\X\a.py",
            ),
        ] {
            assert_eq!(
                without_verbatim_prefix(PathBuf::from(given)),
                PathBuf::from(plain),
                "{given}"
            );
        }
    }
}
