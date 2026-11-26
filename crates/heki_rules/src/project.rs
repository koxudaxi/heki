//! Project discovery and parsing shared by `heki check`, `heki snapshot`, `--watch`, and the
//! LSP server: configuration lookup, file discovery (`.gitignore` / `.hekiignore` / `exclude`),
//! parallel parse → semantic model → IR, inline-suppression filtering, and the index the
//! commands that report from `heki check`'s analysis build.

use crate::{
    CheckExtras, ModuleUnit, ProjectLayout, RuleConfigError, RuleSelection, SummarySnapshot,
    check_project, find_rule,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use heki_ast::{LineIndex, Span};
use heki_diagnostics::{
    BaselineKey, Certainty, Diagnostic, InlineIgnores, RatchetBaseline, Suppression,
    SuppressionForm,
};
use heki_effect::{
    Contracts, ModuleInput, ModuleLayout, OptInFacts, ProjectIndex, TypedWrites, UnreadModule,
    analyze_project_for,
};
use heki_env::{
    ConfigError, Docs, Explain, HekiConfig, LintConfig, Problem, PythonVersion, RuntimeProfile,
    Setting, Severity,
};
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

impl Explain for ProjectError {
    fn explain(&self) -> Problem {
        let problem = Problem::new(self.to_string());
        match self {
            Self::Config(e) => e.explain(),
            Self::Kb(e) => e.explain(),
            Self::TargetVersion(version) => {
                let versions: Vec<String> =
                    PythonVersion::SUPPORTED.iter().map(|v| v.tag()).collect();
                problem
                    .one_of(version, &versions)
                    .help("heki checks Python 3.11 to 3.15: `target-version = \"py312\"`, or `--target-version py312`")
                    .docs(Docs::TargetVersion)
                    .setting(Setting {
                        key: Some("target-version".to_owned()),
                        value: Some(version.clone()),
                        ..Setting::default()
                    })
            }
            Self::Glob { pattern, .. } => problem
                .help("an entry of `exclude` and `extend-exclude` is a glob of project-relative paths, as `\"generated\"` or `\"tests/fixtures/*\"`")
                .docs(Docs::CheckedFiles)
                .setting(Setting {
                    value: Some(pattern.clone()),
                    ..Setting::default()
                }),
            Self::MissingPath(_) => problem
                .help("pass files or directories that exist, relative to the working directory: `heki check src tests`")
                .docs(Docs::CliCheck),
            Self::Walk { .. } => problem
                .help("make the directory readable, or exclude it with `extend-exclude`")
                .docs(Docs::CheckedFiles),
        }
    }
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
    locate_root(start, working_dir).dir
}

/// A project root [`find_root`] found.
struct FoundRoot {
    dir: PathBuf,
    /// A configuration file or the top of a Git working tree is at or above `start`: the root
    /// belongs to a project, not only to the place the run was started from.
    marked: bool,
}

fn locate_root(start: &Path, working_dir: &Path) -> FoundRoot {
    let start = canonical_or_given(start);
    let dir = if start.is_dir() {
        start.clone()
    } else {
        start.parent().map(Path::to_path_buf).unwrap_or_default()
    };
    let mut git_top = None;
    for d in dir.ancestors() {
        if CONFIG_MARKERS.iter().any(|m| d.join(m).exists()) {
            return FoundRoot {
                dir: d.to_path_buf(),
                marked: true,
            };
        }
        if d.join(".git").exists() {
            git_top = Some(d);
            break;
        }
    }
    let working_dir = canonical_or_given(working_dir);
    let holds_start = dir.starts_with(&working_dir);
    let marked = git_top.is_some();
    let dir = match git_top {
        Some(top) if !(holds_start && working_dir.starts_with(top)) => top.to_path_buf(),
        _ if holds_start => working_dir,
        _ => dir,
    };
    FoundRoot { dir, marked }
}

/// What the paths of a command line name: the paths it analyzes, and the files whose reports
/// `heki check` shows.
///
/// A file is checked in its project. Whether a report in it holds depends on its callers,
/// subclasses, and rebindings anywhere in the project, so a file given alone, or with other
/// files, names the project root [`find_root`] finds for the first of them: the run analyzes
/// that root and reports on the files. With directories, the directories are analyzed (a chosen
/// set of files, such as `src` without `tests`) and the files select what is shown. A file
/// outside any project, with no configuration file and no Git working tree above it, is analyzed
/// alone, as the whole program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPaths {
    pub analyzed: Vec<PathBuf>,
    /// Empty when the run shows the reports of everything it analyzes.
    pub files: Vec<PathBuf>,
}

impl CommandPaths {
    /// The meaning of `paths` (the working directory when empty) for a run from the current
    /// directory. A path that does not exist stays among the analyzed ones, where loading the
    /// project reports it.
    pub fn of(paths: &[PathBuf]) -> Self {
        let (files, dirs): (Vec<PathBuf>, Vec<PathBuf>) =
            paths.iter().cloned().partition(|p| p.is_file());
        if files.is_empty() || !dirs.is_empty() {
            let analyzed = if dirs.is_empty() {
                vec![PathBuf::from(".")]
            } else {
                dirs
            };
            return Self { analyzed, files };
        }
        let working_dir = std::env::current_dir().unwrap_or_default();
        let root = locate_root(&files[0], &working_dir);
        if !root.marked {
            return Self {
                analyzed: files,
                files: Vec::new(),
            };
        }
        // The working directory is written `.`, so that the paths the run prints are those of a
        // run from the root.
        let analyzed = if canonical_or_given(&working_dir) == root.dir {
            PathBuf::from(".")
        } else {
            root.dir
        };
        Self {
            analyzed: vec![analyzed],
            files,
        }
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

/// [`walk`] of `dir` that never enters a directory `exclude` leaves out (it may not even be
/// readable).
fn walk_project(dir: &Path, root: &Path, exclude: &GlobSet) -> ignore::WalkBuilder {
    let mut builder = walk(dir);
    let (dir_root, dir_exclude) = (root.to_path_buf(), exclude.clone());
    builder.filter_entry(move |e| {
        !e.file_type().is_some_and(|t| t.is_dir())
            || !is_excluded(&rel_posix(e.path(), &dir_root), &dir_exclude)
    });
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
        for entry in walk_project(p, root, exclude).build() {
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

/// The Python files below `root` that [`collect_files`] over it finds and `analyzed` does not
/// hold (by project-relative path), in the order of a walk sorted by name: at most `limit` of
/// them, among the first `max_entries` entries of the walk. Returns them with whether the walk
/// saw every entry: an entry it cannot read leaves them a lower bound.
pub fn files_left_out(
    root: &Path,
    exclude: &GlobSet,
    analyzed: &dyn Fn(&str) -> bool,
    limit: usize,
    max_entries: usize,
) -> (Vec<PathBuf>, bool) {
    let mut builder = walk_project(root, root, exclude);
    builder.sort_by_file_name(|a, b| a.cmp(b));
    let mut found = Vec::new();
    let mut complete = true;
    for (seen, entry) in builder.build().enumerate() {
        if seen == max_entries || found.len() == limit {
            return (found, false);
        }
        let Ok(entry) = entry else {
            complete = false;
            continue;
        };
        let path = entry.path();
        if path.is_file() && walked_python(path, root, exclude) && !analyzed(&rel_posix(path, root))
        {
            found.push(path.to_path_buf());
        }
    }
    (found, complete)
}

/// The Python files that discovery from the root of `project` finds (with the same ignore files
/// and `exclude`) and the paths it was loaded from leave out ([`files_left_out`]: at most
/// `limit`, among the first `max_entries` entries of the walk, with whether it saw every entry).
/// Paths that hold the root leave nothing out.
pub fn paths_leave_out(
    project: &Project,
    limit: usize,
    max_entries: usize,
) -> Result<(Vec<PathBuf>, bool), ProjectError> {
    let env = &project.env;
    if project.covered()("") {
        return Ok((Vec::new(), true));
    }
    let analyzed: FxHashSet<String> = project
        .files
        .iter()
        .map(|f| f.rel.clone())
        .chain(project.errors.iter().map(|e| rel_posix(&e.path, &env.root)))
        .collect();
    let exclude = exclude_set(&env.cfg)?;
    let analyzed = |rel: &str| analyzed.contains(rel);
    Ok(files_left_out(
        &env.root,
        &exclude,
        &analyzed,
        limit,
        max_entries,
    ))
}

/// A project module that an analyzed file imports and the paths of the run leave out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeftOutImport {
    pub module: String,
    /// Keyed like [`ParsedFile::path`].
    pub path: PathBuf,
}

/// The project modules that the analyzed files of `project` import (an import outside `if
/// TYPE_CHECKING:`, wherever it runs) and its paths leave out ([`paths_leave_out`], among the
/// first `max_entries` entries of the walk), sorted by name: code the run's own files run and
/// the run did not analyze. A run over a chosen set of files (`src` without `tests`) imports
/// none of the others.
pub fn imports_left_out(
    project: &Project,
    max_entries: usize,
) -> Result<Vec<LeftOutImport>, ProjectError> {
    let (left_out, _) = paths_leave_out(project, usize::MAX, max_entries)?;
    if left_out.is_empty() {
        return Ok(Vec::new());
    }
    let env = &project.env;
    let left_out: FxHashMap<String, PathBuf> = left_out
        .into_iter()
        .map(|path| {
            let (module, _) = module_of(&rel_posix(&path, &env.root), &path, &env.layout);
            (module, display_path(&path))
        })
        .collect();
    let analyzed: FxHashSet<&str> = project
        .files
        .iter()
        .map(|f| f.module_name.as_str())
        .collect();
    let mut found = std::collections::BTreeMap::new();
    for file in &project.files {
        for candidates in imported_modules(file) {
            // The deepest name that is a module of the project: `pkg.sub` of `from pkg import
            // sub` when it is a module, else `pkg`.
            let Some(module) = candidates
                .into_iter()
                .flatten()
                .find(|m| analyzed.contains(m.as_str()) || left_out.contains_key(m))
            else {
                continue;
            };
            if let Some((module, path)) = left_out.get_key_value(&module) {
                found.entry(module).or_insert(path);
            }
        }
    }
    Ok(found
        .into_iter()
        .map(|(module, path)| LeftOutImport {
            module: module.clone(),
            path: path.clone(),
        })
        .collect())
}

/// For each import of `file` that runs (not under `if TYPE_CHECKING:`), the modules it may
/// load, deepest first: `a.b` of `import a.b`, `m.name` then `m` of `from m import name`, with
/// relative imports resolved. `None` for a relative import climbing past the top-level package.
fn imported_modules(file: &ParsedFile) -> impl Iterator<Item = [Option<String>; 2]> + '_ {
    let base = |level: u32, module: Option<&str>| {
        heki_effect::resolver::resolve_relative_module(
            &file.module_name,
            file.is_package,
            level,
            module,
            file.root_is_package,
        )
    };
    let semantic = &file.semantic;
    let imports = semantic
        .imports
        .iter()
        .filter(|i| !i.in_type_checking)
        .map(move |i| {
            if !i.is_from {
                return [Some(i.imported_name.clone()), None];
            }
            let from = base(i.level, i.module.as_deref());
            // `from . import x` in a root that is a package names `x`.
            let named = (from.as_ref()).map(|m| {
                format!("{m}.{}", i.imported_name)
                    .trim_start_matches('.')
                    .to_owned()
            });
            [named, from]
        });
    let stars = semantic
        .star_imports
        .iter()
        .filter(|s| !s.in_type_checking)
        .map(move |s| [base(s.level, s.module.as_deref()), None]);
    imports.chain(stars)
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
    let parsed =
        heki_ast::parse_module(&source).map_err(|errs| FileError::syntax(path, &source, &errs))?;
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
/// that module is a package.
fn module_identity(path: &Path, root: &Path, layout: &ModuleLayout) -> (String, String, bool) {
    let rel = rel_posix(path, root);
    let (module_name, is_package) = module_of(&rel, path, layout);
    (rel, module_name, is_package)
}

/// The module the file at `path`, `rel` below the project root, defines under `layout`, and
/// whether it is a package. A file outside a module layout defines the module of its stem.
pub fn module_of(rel: &str, path: &Path, layout: &ModuleLayout) -> (String, bool) {
    layout.module_name(Path::new(rel)).unwrap_or_else(|| {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        (stem, false)
    })
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
    /// The files and directories to analyze, at least one: the commands default to the working
    /// directory (`.`), and the language server names its workspace folder.
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

impl ProjectOptions<'_> {
    /// The file the configuration is read from: `--config`, else the first configuration file
    /// at the project root.
    pub fn config_file(&self) -> Option<PathBuf> {
        self.config_file_at(&self.root())
    }

    /// The project root ([`find_root`]) of the first path.
    fn root(&self) -> PathBuf {
        let first = self
            .paths
            .first()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("."));
        let working_dir = match self.working_dir {
            Some(dir) => dir.to_path_buf(),
            None => std::env::current_dir().unwrap_or_else(|_| first.clone()),
        };
        find_root(&first, &working_dir)
    }

    fn config_file_at(&self, root: &Path) -> Option<PathBuf> {
        self.config
            .map(Path::to_path_buf)
            .or_else(|| HekiConfig::file_in(root))
    }
}

/// Configuration and knowledge bases of a project root.
pub struct ProjectEnv {
    pub root: PathBuf,
    /// The file `cfg` was read from (`--config`, or the first configuration file at the root).
    pub config_file: Option<PathBuf>,
    pub cfg: HekiConfig,
    pub runtime: RuntimeProfile,
    pub kb: EffectiveEcosystemKb,
    pub soundness_kb: EffectiveSoundnessKb,
    pub layout: ModuleLayout,
}

impl ProjectEnv {
    pub fn load(opts: &ProjectOptions<'_>) -> Result<Self, ProjectError> {
        let root = opts.root();
        let config_file = opts.config_file_at(&root);
        let cfg = match &config_file {
            Some(path) => HekiConfig::load_from_file(path, &root)?,
            None => HekiConfig::default(),
        };
        let runtime = match opts.target_version.or(cfg.target_version.as_deref()) {
            Some(t) => PythonVersion::parse_tag(t)
                .map(RuntimeProfile::from)
                .ok_or_else(|| ProjectError::TargetVersion(t.to_owned()))?,
            None => cfg.runtime_profile(),
        };
        let kb = Self::catalog(&cfg)?;
        let soundness_kb = EffectiveSoundnessKb::load_builtin(runtime.python_version)?;
        let removed = opts.removed;
        let layout = ModuleLayout::detect_without(&root, &cfg, &|p| {
            removed.is_some_and(|r| r.contains(&display_path(p)))
        });
        Ok(Self {
            root,
            config_file,
            cfg,
            runtime,
            kb,
            soundness_kb,
            layout,
        })
    }

    /// The ecosystem catalog of `cfg`: heki's own, with the configuration's
    /// `[[ecosystem]]` entries over it.
    pub fn catalog(cfg: &HekiConfig) -> Result<EffectiveEcosystemKb, ProjectError> {
        let overrides = cfg
            .ecosystem
            .iter()
            .map(EcosystemPatch::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(EffectiveEcosystemKb::load_builtin(&overrides)?)
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
    pub kind: FileErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileErrorKind {
    Syntax,
    Unreadable,
    /// The file changed on disk while `--fix` was fixing it: its fixes were not written.
    ChangedWhileFixing,
}

impl FileError {
    /// A read error of the file at `path`.
    pub fn unreadable(path: &Path, cause: &impl std::fmt::Display) -> Self {
        Self {
            path: display_path(path),
            position: None,
            message: cause.to_string(),
            kind: FileErrorKind::Unreadable,
        }
    }

    /// The first of the syntax errors `errs` of `source`, the text of the file at `path`.
    pub fn syntax(path: &Path, source: &str, errs: &[heki_ast::ParseError]) -> Self {
        let first = &errs[0];
        Self {
            path: display_path(path),
            position: Some(LineIndex::from_source(source).line_col(first.span.start)),
            message: format!("syntax error: {} ({} error(s))", first.message, errs.len()),
            kind: FileErrorKind::Syntax,
        }
    }

    /// The file at `path` changed on disk while `--fix` was fixing it.
    pub fn changed_while_fixing(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            position: None,
            message: "changed on disk while heki was fixing it; its fixes were not written"
                .to_owned(),
            kind: FileErrorKind::ChangedWhileFixing,
        }
    }

    /// `path:line:col: message` (`path: message` without a position), with the path written as
    /// `path_text` (the caller's choice: `heki check` writes it relative to the working directory).
    pub fn report(&self, path_text: &str) -> String {
        let at = self
            .position
            .map_or_else(String::new, |(line, col)| format!(":{line}:{col}"));
        format!("{path_text}{at}: {}", self.message)
    }
}

impl Explain for FileError {
    fn explain(&self) -> Problem {
        let message = match self.kind {
            FileErrorKind::Unreadable => format!("cannot read the file: {}", self.message),
            _ => self.message.clone(),
        };
        let problem = Problem::new(message);
        let problem = match self.position {
            Some((line, column)) => problem.at(&self.path, line, column),
            None => problem.at_file(&self.path),
        };
        match self.kind {
            FileErrorKind::Syntax => problem
                .help("fix the syntax error, or exclude the file with `extend-exclude`")
                .docs(Docs::CheckedFiles),
            FileErrorKind::Unreadable => problem
                .help("make the file readable UTF-8 text, or exclude it with `extend-exclude`")
                .docs(Docs::CheckedFiles),
            FileErrorKind::ChangedWhileFixing => problem
                .help("run `heki check --fix` again")
                .docs(Docs::Fixes),
        }
    }
}

impl std::error::Error for FileError {}

/// With the path as reports write it ([`heki_diagnostics::path_text`]).
impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.report(&heki_diagnostics::path_text(&self.path)))
    }
}

/// A discovered, parsed project.
pub struct Project {
    pub env: ProjectEnv,
    /// The paths discovery walked ([`ProjectOptions::paths`]).
    pub paths: Vec<PathBuf>,
    pub files: Vec<ParsedFile>,
    pub errors: Vec<FileError>,
    /// The files of `errors` that heki could not read or parse: modules it knows nothing about.
    pub unread: Vec<UnreadModule>,
}

impl Project {
    pub fn load(opts: &ProjectOptions<'_>) -> Result<Self, ProjectError> {
        let env = ProjectEnv::load(opts)?;
        let exclude = exclude_set(&env.cfg)?;
        let paths = opts.paths.to_vec();
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
            paths,
            files,
            errors,
            unread,
        })
    }

    /// Whether the file at a project-relative path (one that exists or not) is in what the
    /// paths of the run cover: below one of its directories, or one of its files. A run over
    /// part of the project says nothing about the other files.
    pub fn covered(&self) -> impl Fn(&str) -> bool + '_ {
        let walked: Vec<PathBuf> = self
            .paths
            .iter()
            .filter_map(|p| canonical(p).ok())
            .collect();
        move |rel| {
            let file = self.env.root.join(rel);
            walked.iter().any(|p| file.starts_with(p))
        }
    }

    /// The analyzed files a baseline entry or report is about: the file of a key, and for a
    /// group key, which is of no one file, the files of the modules its target lists. Also
    /// whether every module of the group is one of them: one that is not is deleted, or left
    /// out of a run over part of the project.
    pub fn entry_files<'a>(&'a self, key: &'a BaselineKey) -> (Vec<&'a str>, bool) {
        if !key.is_group() {
            return (vec![key.path.as_str()], true);
        }
        let members = key.members();
        let listed: Vec<&ParsedFile> = (self.files.iter())
            .filter(|f| members.contains(f.module_name.as_str()))
            .collect();
        let found: FxHashSet<&str> = listed.iter().map(|f| f.module_name.as_str()).collect();
        let files = listed.into_iter().map(|f| f.rel.as_str()).collect();
        (files, found.len() == members.len())
    }

    /// Whether the run covers every file of the entry `key` ([`Self::entry_files`]): it alone
    /// decides the entry. A group with a module that is not an analyzed file is decided by a
    /// run over the whole project.
    pub fn covers_entry(&self) -> impl Fn(&BaselineKey) -> bool + '_ {
        let covered = self.covered();
        move |key| {
            let (files, all) = self.entry_files(key);
            (all || covered("")) && files.into_iter().all(&covered)
        }
    }

    pub fn inputs(&self) -> Vec<ModuleInput<'_>> {
        self.files.iter().map(ParsedFile::input).collect()
    }

    pub fn units(&self) -> Vec<ModuleUnit<'_>> {
        self.files.iter().map(ParsedFile::unit).collect()
    }

    /// Analyze the files with `cfg` from now on, as `heki check` would with that configuration
    /// in the same file: its catalog is rebuilt from its `[[ecosystem]]` entries. Returns the
    /// configuration it replaces. `src` and `target-version` must stay as they were: the module
    /// names and the parse follow them.
    pub fn set_config(&mut self, cfg: HekiConfig) -> Result<HekiConfig, ProjectError> {
        debug_assert!(
            cfg.src == self.env.cfg.src && cfg.target_version == self.env.cfg.target_version,
            "the module names and the parse follow `src` and `target-version`"
        );
        self.env.kb = ProjectEnv::catalog(&cfg)?;
        Ok(std::mem::replace(&mut self.env.cfg, cfg))
    }

    /// The index of the project's files with `facts`.
    pub fn index(&self, facts: IndexFacts) -> ProjectIndex {
        let inputs = self.inputs();
        analyze_project_for(
            &inputs,
            &self.unread,
            &self.env.kb,
            Some(&self.env.soundness_kb),
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
    /// Also what `heki recommend` reads ([`crate::recommend_facts`]).
    Recommend,
    /// Also the import facts ([`crate::import_facts`]): `heki inspect --imports`.
    Imports,
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
        let mut opt_in = match facts {
            Facts::Check => crate::opt_in_facts(selection),
            Facts::Inspect => crate::inspect_facts(selection),
            Facts::Recommend => crate::recommend_facts(selection),
            Facts::Imports => crate::import_facts(selection),
        };
        // The calls an effect boundary reporting what it cannot judge names (`EFF201`).
        opt_in.blind_spots |= selection.is_enabled("EFF201")
            && config
                .effect_boundaries
                .iter()
                .any(|b| b.unresolved.as_deref() == Some("report"));
        // `IMP201` and `IMP202` read the import facts of every file once a contract is declared.
        opt_in.imports |= crate::judges_imports(config, selection);
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

    /// What the checks of this index run.
    pub fn rules_run(&self) -> RulesRun<'_> {
        RulesRun {
            selection: &self.selection,
            snapshot: self.snapshot.is_some(),
            lint: &self.project.env.cfg.lint,
        }
    }

    /// The diagnostics of every enabled rule in the project's files, after inline suppressions
    /// and with the reports on suppression comments the configuration asks for, without the
    /// heuristic reports the selection withholds, and those.
    pub fn diagnose(&self) -> Result<(Vec<Diagnostic>, Vec<Diagnostic>), RuleConfigError> {
        self.diagnose_units(&self.project.units(), false)
    }

    /// [`Self::diagnose`], and the reports of the selected rules that inline suppressions hid
    /// (not the heuristic reports the selection withholds), each with the suppression that hid
    /// it, from the same analysis.
    pub fn diagnose_recording_hidden(&self) -> Result<Diagnosed, RuleConfigError> {
        let units = self.project.units();
        let mut diags = self.reports(&units, false)?;
        let mut hidden = Vec::new();
        self.retain_unsuppressed_in(&units, &mut diags, Some(&mut hidden));
        hidden.retain(|h| !self.selection.withholds(&h.report));
        let withheld = self.selection.take_heuristic(&mut diags);
        Ok(Diagnosed {
            diags,
            withheld,
            hidden,
        })
    }

    /// [`Self::diagnose`] in `units` (some or all of the project's files): the rules read the
    /// other files through the index only. `open_premises`: the heuristic reports of exact
    /// claims record the premises they left open ([`CheckExtras::open_premises`]).
    pub fn diagnose_units(
        &self,
        units: &[ModuleUnit<'_>],
        open_premises: bool,
    ) -> Result<(Vec<Diagnostic>, Vec<Diagnostic>), RuleConfigError> {
        let mut diags = self.reports(units, open_premises)?;
        self.retain_unsuppressed_in(units, &mut diags, None);
        let withheld = self.selection.take_heuristic(&mut diags);
        Ok((diags, withheld))
    }

    /// [`Self::diagnose`], with the reports inline suppressions drop apart, as
    /// `(kept, suppressed)`.
    pub fn diagnose_with_suppressed(
        &self,
    ) -> Result<(Vec<Diagnostic>, Vec<Diagnostic>), RuleConfigError> {
        let units = self.project.units();
        let mut diags = self.reports(&units, false)?;
        let mut kept = diags.clone();
        self.retain_unsuppressed_in(&units, &mut kept, None);
        // Both are in the same order, so one pass pairs them up; the reports on suppression
        // comments are not among `diags`.
        let mut next = kept
            .iter()
            .filter(|k| !is_suppression_report(&k.rule_code))
            .peekable();
        diags.retain(|d| next.next_if(|k| *k == d).is_none());
        let mut suppressed = diags;
        self.selection.take_heuristic(&mut kept);
        self.selection.take_heuristic(&mut suppressed);
        Ok((kept, suppressed))
    }

    /// Drop the reports on `units` that inline suppressions suppress, and add the reports on
    /// suppression comments the configuration asks for.
    fn retain_unsuppressed_in(
        &self,
        units: &[ModuleUnit<'_>],
        diags: &mut Vec<Diagnostic>,
        hidden: Option<&mut Vec<Hidden>>,
    ) {
        // The reports of a unit are on its own file.
        let checked: FxHashSet<&Path> = units.iter().map(|u| u.input.path.as_path()).collect();
        let files = self.project.files.iter();
        retain_unsuppressed_recording(
            diags,
            files.filter(|f| checked.contains(f.path.as_path())),
            &self.rules_run(),
            hidden,
        );
    }

    /// Every report of the selected rules in `units`, before inline suppressions and the
    /// heuristic reports the selection withholds.
    fn reports(
        &self,
        units: &[ModuleUnit<'_>],
        open_premises: bool,
    ) -> Result<Vec<Diagnostic>, RuleConfigError> {
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
            open_premises,
            layout: ProjectLayout {
                root: &env.root,
                modules: &env.layout,
            },
            runtime: env.runtime,
        };
        check_project(
            units,
            &env.cfg,
            &env.kb,
            &self.index,
            &self.selection,
            &extras,
        )
    }
}

/// The path a baseline entry names for a file of `files`: relative to the project root.
pub fn baseline_key(files: &[ParsedFile]) -> impl Fn(&Path) -> String + '_ {
    let rel: FxHashMap<&Path, &str> = files
        .iter()
        .map(|f| (f.path.as_path(), f.rel.as_str()))
        .collect();
    move |p: &Path| {
        rel.get(p)
            .map_or_else(|| heki_diagnostics::path_text(p), |r| (*r).to_owned())
    }
}

/// For each of `diags` (reports on `files`), whether `baseline` counts it among the regressions,
/// the reports `heki check` prints with the baseline.
pub fn regressed(
    baseline: &RatchetBaseline,
    diags: &[Diagnostic],
    files: &[ParsedFile],
) -> Vec<bool> {
    let mut regressions = diags.to_vec();
    baseline.keep_regressions(&mut regressions, &baseline_key(files));
    // `keep_regressions` keeps the regressions in their order, so one pass pairs them up.
    let mut next = regressions.iter().peekable();
    diags
        .iter()
        .map(|d| next.next_if(|r| *r == d).is_some())
        .collect()
}

/// The code of a report that a suppression comment named a rule that ran and suppressed none of
/// its reports (`lint.report-unused-suppressions`).
pub const UNUSED_SUPPRESSION: &str = "unused-suppression";
/// The code of a report that a `# heki: ignore` comment names no rule
/// (`lint.require-suppression-codes`).
pub const SUPPRESSION_WITHOUT_CODE: &str = "suppression-without-code";

/// Whether `code` is the code of a report on a suppression comment, which no rule makes.
pub fn is_suppression_report(code: &str) -> bool {
    code == UNUSED_SUPPRESSION || code == SUPPRESSION_WITHOUT_CODE
}

/// What a check ran: what suppressions and baseline entries are judged by.
#[derive(Clone, Copy)]
pub struct RulesRun<'a> {
    pub selection: &'a RuleSelection,
    /// Whether the check compared with a summary snapshot.
    pub snapshot: bool,
    pub lint: &'a LintConfig,
}

impl RulesRun<'_> {
    fn unused_reported(&self) -> bool {
        self.lint.report_unused_suppressions.unwrap_or(false)
    }

    fn codes_required(&self) -> bool {
        self.lint.require_suppression_codes.unwrap_or(false)
    }

    /// Whether the check ran what reports `code` on the file at the project-relative
    /// `rel_path`: the rule ([`crate::runs_on`]), or the setting of a suppression report. A
    /// rule that did not run (a heuristic rule left off, a rule `per-file-ignores` turns off
    /// there) reported nothing that a suppression or a baseline entry could match.
    pub fn reports(&self, code: &str, rel_path: &str) -> bool {
        match code {
            UNUSED_SUPPRESSION => self.unused_reported(),
            SUPPRESSION_WITHOUT_CODE => self.codes_required(),
            _ => find_rule(code)
                .is_some_and(|r| crate::runs_on(self.selection, self.snapshot, r.code, rel_path)),
        }
    }

    /// Whether the baseline entry `key`, which `found` reports of this check matched (the
    /// heuristic reports the selection withholds among them), is reported for matching none:
    /// with `lint.report-unused-suppressions`, when its file was checked (`checked` holds the
    /// project-relative paths) and its rule ran there. `files` are the files of the entry
    /// ([`Project::entry_files`]): every one must be checked and run the rule.
    pub fn unused_entry(
        &self,
        key: &BaselineKey,
        found: u32,
        files: &[&str],
        checked: &FxHashSet<&str>,
    ) -> bool {
        self.unused_reported()
            && found == 0
            && files
                .iter()
                .all(|f| checked.contains(f) && self.reports(&key.rule_code, f))
    }
}

/// The result of [`Indexed::diagnose_recording_hidden`].
pub struct Diagnosed {
    /// The reports after inline suppressions ([`Indexed::diagnose`]).
    pub diags: Vec<Diagnostic>,
    /// The heuristic reports the selection withholds.
    pub withheld: Vec<Diagnostic>,
    /// The reports inline suppressions hid, in the order of the reports.
    pub hidden: Vec<Hidden>,
}

/// A report that an inline suppression hid.
pub struct Hidden {
    pub report: Diagnostic,
    /// The suppression that hid it, in the report's file: the first of them in the source when
    /// several do.
    pub by: Suppression,
}

/// Drop diagnostics suppressed by `# heki: ignore[...]` / `# noqa` comments of their file
/// (`files` holds every file the diagnostics are on), then add the reports on those comments
/// that `run`'s settings ask for.
pub fn retain_unsuppressed<'f>(
    diags: &mut Vec<Diagnostic>,
    files: impl IntoIterator<Item = &'f ParsedFile>,
    run: &RulesRun<'_>,
) {
    retain_unsuppressed_recording(diags, files, run, None);
}

/// [`retain_unsuppressed`], pushing each report it drops onto `hidden` with the suppression
/// that dropped it.
fn retain_unsuppressed_recording<'f>(
    diags: &mut Vec<Diagnostic>,
    files: impl IntoIterator<Item = &'f ParsedFile>,
    run: &RulesRun<'_>,
    mut hidden: Option<&mut Vec<Hidden>>,
) {
    // For each file, its suppressions and the rules each one suppressed reports of.
    let mut ignores: FxHashMap<&Path, (&ParsedFile, InlineIgnores, Vec<FxHashSet<String>>)> = files
        .into_iter()
        .map(|p| {
            let ignores = InlineIgnores::from_comments(&p.source, &p.comments);
            let suppressed = vec![FxHashSet::default(); ignores.suppressions().len()];
            (p.path.as_path(), (p, ignores, suppressed))
        })
        .collect();
    let judged = run.unused_reported() || run.codes_required();
    diags.retain(|d| {
        let mut kept = true;
        // A diagnostic on a path without a checked source has no suppressions: no iteration.
        for (_, ign, suppressed) in ignores.get_mut(d.path.as_path()).into_iter() {
            for i in ign.suppressing(d.line, &d.rule_code, &d.rule_slug) {
                if judged {
                    suppressed[i].insert(d.rule_code.clone());
                }
                if kept && let Some(hidden) = hidden.as_deref_mut() {
                    hidden.push(Hidden {
                        report: d.clone(),
                        by: ign.suppressions()[i].clone(),
                    });
                }
                kept = false;
            }
        }
        kept
    });
    if !judged {
        return;
    }
    for (file, ign, suppressed) in ignores.values() {
        let lines = LineIndex::from_source(&file.source);
        for (s, suppressed) in ign.suppressions().iter().zip(suppressed) {
            if let Some(d) = suppression_report(file, &lines, s, suppressed, run) {
                diags.push(d);
            }
        }
    }
    heki_diagnostics::sort_diagnostics(diags);
}

/// The report on suppression `s` of `file`, which suppressed reports of the rules `suppressed`,
/// if `run`'s settings ask for one: the codes naming a rule that ran and suppressed none of its
/// reports, or a `# heki: ignore` naming no rule. A bare `# noqa` belongs to Ruff as much as to
/// heki, and a code that names no heki rule (`F401` in `# noqa: F401, EFF101`) is another
/// tool's; neither is reported.
fn suppression_report(
    file: &ParsedFile,
    lines: &LineIndex,
    s: &Suppression,
    suppressed: &FxHashSet<String>,
    run: &RulesRun<'_>,
) -> Option<Diagnostic> {
    let (code, setting, target, message, help, fix) = if s.codes.is_empty() {
        if s.form != SuppressionForm::Heki || !run.codes_required() {
            return None;
        }
        let mut names: Vec<&str> = suppressed.iter().map(String::as_str).collect();
        names.sort_unstable();
        let help = if names.is_empty() {
            "name the rules it is for: `# heki: ignore[CODE]`".to_owned()
        } else {
            format!(
                "name the rules it suppresses here: `# heki: ignore[{}]`",
                names.join(", ")
            )
        };
        (
            SUPPRESSION_WITHOUT_CODE,
            "require-suppression-codes",
            "# heki: ignore".to_owned(),
            "`# heki: ignore` names no rule: it suppresses every rule on this line".to_owned(),
            help,
            None,
        )
    } else {
        if !run.unused_reported() {
            return None;
        }
        let unused: Vec<usize> = (0..s.codes.len())
            .filter(|&i| {
                find_rule(&s.codes[i])
                    .is_some_and(|r| run.reports(r.code, &file.rel) && !suppressed.contains(r.code))
            })
            .collect();
        let names: Vec<&str> = unused.iter().map(|&i| s.codes[i].as_str()).collect();
        let listed: Vec<String> = names.iter().map(|c| format!("`{c}`")).collect();
        let (last, others) = listed.split_last()?;
        let either = if others.is_empty() {
            last.clone()
        } else {
            format!("{} or {last}", others.join(", "))
        };
        let help = if unused.len() == s.codes.len() {
            "remove the suppression".to_owned()
        } else {
            format!("remove {} from the suppression", listed.join(", "))
        };
        (
            UNUSED_SUPPRESSION,
            "report-unused-suppressions",
            names.join(", "),
            format!("No {either} report on this line to suppress"),
            help,
            Some(vec![s.without(&file.source, &unused)]),
        )
    };
    let (line, column) = lines.line_col(s.span.start);
    Some(Diagnostic {
        path: file.path.clone(),
        line,
        column,
        span: s.span,
        qualname: enclosing_function(file, lines, line).to_owned(),
        rule_code: code.to_owned(),
        rule_slug: setting.to_owned(),
        target,
        severity: Severity::Error,
        certainty: Certainty::Exact,
        message,
        help: Some(help),
        evidence: None,
        blocker: None,
        related: Vec::new(),
        fix,
        sites: 1,
        fold_key: None,
        open_records: Vec::new(),
        contracts: Vec::new(),
        allowed_examples: Vec::new(),
        group: false,
        key_target: None,
        key_module: None,
    })
}

/// The qualified name of the innermost function of `file` whose lines hold `line`.
fn enclosing_function<'f>(file: &'f ParsedFile, lines: &LineIndex, line: u32) -> &'f str {
    file.ir
        .functions
        .iter()
        .filter(|f| (lines.line(f.span.start)..=lines.line(f.span.end)).contains(&line))
        .min_by_key(|f| f.span.end - f.span.start)
        .map_or("<module>", heki_effect::func_qualname)
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
