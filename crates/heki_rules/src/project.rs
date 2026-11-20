//! Project discovery and parsing shared by `heki check`, `heki snapshot`, `--watch`, and the
//! LSP server: configuration lookup, file discovery (`.gitignore` / `.hekiignore` / `exclude`),
//! parallel parse → semantic model → IR, and inline-suppression filtering.

use crate::ModuleUnit;
use globset::{Glob, GlobSet, GlobSetBuilder};
use heki_ast::{LineIndex, Span};
use heki_diagnostics::{Diagnostic, InlineIgnores};
use heki_effect::{ModuleInput, detect_src_layout, module_name_for_path};
use heki_env::{ConfigError, HekiConfig, PythonVersion, RuntimeProfile};
use heki_ir::ModuleIr;
use heki_kb::{EcosystemEntry, EffectiveEcosystemKb, EffectiveSoundnessKb, KbError};
use heki_semantic::SemanticModel;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::{Component, Path, PathBuf};
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

const ROOT_MARKERS: &[&str] = &["heki.toml", ".heki.toml", "pyproject.toml", ".git"];

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

/// The canonical form of `path`, or `path` itself when it cannot be resolved (it does not exist,
/// e.g. an unsaved document, or no longer exists).
fn canonical_or_given(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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

/// Nearest ancestor of `start` holding a root marker (`heki.toml`, `pyproject.toml`, `.git`).
pub fn find_root(start: &Path) -> PathBuf {
    let start = canonical_or_given(start);
    let dir = if start.is_dir() {
        start.clone()
    } else {
        start.parent().map(Path::to_path_buf).unwrap_or_default()
    };
    dir.ancestors()
        .find(|d| ROOT_MARKERS.iter().any(|m| d.join(m).exists()))
        .unwrap_or(&dir)
        .to_path_buf()
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
        let mut builder = ignore::WalkBuilder::new(p);
        builder.add_custom_ignore_filename(".hekiignore");
        for entry in builder.build() {
            let entry = entry.map_err(|cause| ProjectError::Walk {
                path: p.clone(),
                cause,
            })?;
            let ep = entry.path();
            if ep.is_file()
                && ep.extension().is_some_and(|e| e == "py" || e == "pyi")
                && !is_excluded(&rel_posix(ep, root), exclude)
            {
                files.push(ep.to_path_buf());
            }
        }
    }
    let mut seen = FxHashSet::default();
    files.retain(|f| seen.insert(canonical_or_given(f)));
    files.sort();
    Ok(files)
}

/// Parse `source` as the module at `path` (`Err`: a `path:line:col: syntax error` message).
pub fn parse_source(
    path: &Path,
    root: &Path,
    source: String,
    src_layout: bool,
    runtime: RuntimeProfile,
) -> Result<ParsedFile, String> {
    // A typing stub is never run: it may spell annotations with newer syntax than the target.
    let parsed = if heki_effect::is_stub(path) {
        heki_ast::parse_module(&source)
    } else {
        heki_ast::parse_module_for(&source, runtime.python_version)
    };
    let parsed = parsed.map_err(|errs| {
        let index = LineIndex::from_source(&source);
        let first = &errs[0];
        let (line, col) = index.line_col(first.span.start);
        format!(
            "{}:{line}:{col}: syntax error: {} ({} error(s))",
            path.display(),
            first.message,
            errs.len()
        )
    })?;
    let rel = rel_posix(path, root);
    let (module_name, is_package) = module_name_for_path(Path::new(&rel), src_layout)
        .unwrap_or_else(|| {
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            (stem, false)
        });
    let semantic = heki_semantic::analyze_module(&parsed, &source, runtime);
    let ir = heki_ir::build_module_ir(&parsed, &source, &semantic, runtime);
    Ok(ParsedFile {
        path: display_path(path),
        rel,
        module_name,
        is_package,
        comments: parsed.comment_spans(),
        source,
        semantic,
        ir,
    })
}

/// Where a run looks for files and configuration.
#[derive(Default)]
pub struct ProjectOptions<'a> {
    pub paths: &'a [PathBuf],
    pub config: Option<&'a Path>,
    pub target_version: Option<&'a str>,
    /// In-memory sources replacing files on disk (keyed like [`ParsedFile::path`]).
    pub overrides: Option<&'a FxHashMap<PathBuf, String>>,
}

/// Configuration and knowledge bases of a project root.
pub struct ProjectEnv {
    pub root: PathBuf,
    pub cfg: HekiConfig,
    pub runtime: RuntimeProfile,
    pub kb: EffectiveEcosystemKb,
    pub soundness_kb: EffectiveSoundnessKb,
    pub src_layout: bool,
}

impl ProjectEnv {
    pub fn load(opts: &ProjectOptions<'_>) -> Result<Self, ProjectError> {
        let first = opts
            .paths
            .first()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("."));
        let root = find_root(&first);
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
            .map(EcosystemEntry::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let kb = EffectiveEcosystemKb::load_builtin(&overrides)?;
        let soundness_kb = EffectiveSoundnessKb::load_builtin(runtime.python_version)?;
        let src_layout = detect_src_layout(&root);
        Ok(Self {
            root,
            cfg,
            runtime,
            kb,
            soundness_kb,
            src_layout,
        })
    }
}

/// A discovered, parsed project.
pub struct Project {
    pub env: ProjectEnv,
    pub files: Vec<ParsedFile>,
    /// Read / syntax errors (the CLI exits with 2 after its report).
    pub errors: Vec<String>,
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
        let found = collect_files(&paths, &env.root, &exclude)?;
        let results: Vec<Result<ParsedFile, String>> = found
            .par_iter()
            .map(|p| {
                let source = match opts.overrides.and_then(|o| o.get(&display_path(p))) {
                    Some(text) => text.clone(),
                    None => {
                        heki_ast::read_source(p).map_err(|e| format!("{}: {e}", p.display()))?
                    }
                };
                parse_source(p, &env.root, source, env.src_layout, env.runtime)
            })
            .collect();
        let mut files = Vec::new();
        let mut errors = Vec::new();
        for r in results {
            match r {
                Ok(p) => files.push(p),
                Err(e) => errors.push(e),
            }
        }
        Ok(Self { env, files, errors })
    }

    pub fn inputs(&self) -> Vec<ModuleInput<'_>> {
        self.files.iter().map(ParsedFile::input).collect()
    }

    pub fn units(&self) -> Vec<ModuleUnit<'_>> {
        self.files.iter().map(ParsedFile::unit).collect()
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
