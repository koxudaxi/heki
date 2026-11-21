//! What a function takes from outside the project that effect boundaries forbid
//! (`FileSummary::external_accesses`): the calls of external functions and the reads of
//! external module attributes a boundary names, and the imports it probes. Only the names some
//! boundary forbids are kept ([`crate::Contracts::external_call`],
//! [`crate::Contracts::external_read`], [`crate::Contracts::import_probes`]), so a project
//! without such a boundary keeps none.

use crate::resolver::resolve_relative_module;
use heki_ast::Span;
use heki_ir::{BlockId, InstKind, IrFunction, Terminator};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

/// What an [`ExternalAccess`] does with its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessKind {
    /// Calls the external function (`os.getenv`).
    Call,
    /// Reads the attribute of a module outside the project (`sys.platform`), also through a
    /// name imported from it (`from sys import platform`).
    Read,
    /// Imports the module inside a `try` an `except` clause of which names `ImportError` or
    /// `ModuleNotFoundError`: the import finds out whether the module is installed.
    ImportProbe,
}

impl AccessKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Call => "calls",
            Self::Read => "module_attr",
            Self::ImportProbe => "import_probe",
        }
    }
}

/// One access of a function outside the project, in its own body (a local fact).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ExternalAccess {
    pub kind: AccessKind,
    /// The function called, the attribute read (`sys.platform`), or the module imported.
    pub name: String,
    pub span: Span,
    /// The call imports or finds a module of the project (a function the catalog tags
    /// `environment = "import"`: `importlib.import_module("pkg.config")`), which reads nothing
    /// of the environment.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub imports_project_module: bool,
}

/// The absolute name of the module that `name`, passed to a function the catalog tags
/// `environment = "import"` with `package`, names, or with `head`, the text that name starts
/// with (`pkg.parser.` of `f"pkg.parser.{name}"`). A name starting with a dot is anchored at
/// `package`, one package up for each further dot, as `importlib` resolves it
/// (`importlib.import_module("..base", "pkg.parser")` imports `pkg.base`). `None` when a
/// relative name has no package or climbs past its top-level package, where `importlib`
/// raises, and for a head of dots only: more dots may follow, climbing further.
pub(crate) fn absolute_module_name(
    name: &str,
    package: Option<&str>,
    head: bool,
) -> Option<String> {
    let rest = name.trim_start_matches('.');
    let level = name.len() - rest.len();
    if level == 0 {
        return Some(name.to_owned());
    }
    if head && rest.is_empty() {
        return None;
    }
    let package = package.filter(|p| !p.is_empty())?;
    resolve_relative_module(
        package,
        true,
        u32::try_from(level).ok()?,
        Some(rest).filter(|r| !r.is_empty()),
        false,
    )
}

/// The `Import` instructions of `f` that probe their module (`AccessKind::ImportProbe`): their
/// spans, and the `from` module and the name each imports. A lazy import runs where its name is
/// first used, not inside the `try`.
pub(crate) fn import_probes(
    f: &IrFunction,
) -> impl Iterator<Item = (Span, Option<&str>, &str)> + '_ {
    // The blocks of one `try` body share their handler: each handler is walked once.
    let mut catches: FxHashMap<BlockId, bool> = FxHashMap::default();
    f.blocks.iter().flat_map(move |bb| {
        let mut imports = bb
            .insts
            .iter()
            .filter_map(|i| match &i.kind {
                InstKind::Import {
                    module,
                    name,
                    is_lazy: false,
                    ..
                } => Some((i.span, module.as_deref(), name.as_str())),
                _ => None,
            })
            .peekable();
        let probing = imports.peek().is_some()
            && bb.exc_target.is_some_and(|handler| {
                *catches
                    .entry(handler)
                    .or_insert_with(|| catches_import_error(f, handler))
            });
        imports.filter(move |_| probing)
    })
}

/// Whether a call in block `bb` of `f` runs inside a `try` that takes an `ImportError`
/// ([`catches_import_error`]): the code expects what it imports to be missing.
pub(crate) fn guarded_against_import_errors(f: &IrFunction, bb: BlockId) -> bool {
    f.block(bb)
        .exc_target
        .is_some_and(|handler| catches_import_error(f, handler))
}

/// Whether the exception handler `handler` takes an `ImportError`: one of the `except` clauses
/// it tests in turn names it, or, when none does and it raises the exception again, an
/// enclosing handler takes it (also after a `finally` body). Any other shape of handler (the
/// exit of a `with` block, which may suppress the exception) is not followed.
fn catches_import_error(f: &IrFunction, handler: BlockId) -> bool {
    let mut seen: FxHashSet<BlockId> = FxHashSet::default();
    let mut handler = Some(handler);
    while let Some(start) = handler.filter(|b| seen.insert(*b)) {
        let mut clause = f.block(start);
        handler = loop {
            match &clause.terminator {
                // Each clause tests its types and branches to its body, which starts by
                // binding the exception.
                Terminator::Branch {
                    then_bb, else_bb, ..
                } => {
                    let Some(InstKind::CatchException { exc_types, .. }) =
                        f.block(*then_bb).insts.first().map(|i| &i.kind)
                    else {
                        return false;
                    };
                    if exc_types.iter().any(|t| names_import_error(t)) {
                        return true;
                    }
                    clause = f.block(*else_bb);
                }
                // No clause took it: it is raised to the enclosing handler.
                Terminator::Raise(None) => break clause.exc_target,
                _ => return false,
            }
        };
    }
    false
}

/// Whether the source text of an `except` clause's types names `ImportError` or
/// `ModuleNotFoundError` (`ImportError`, `(ImportError, OSError)`, `builtins.ImportError`).
fn names_import_error(types: &str) -> bool {
    types
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .split(',')
        .map(|t| t.trim().rsplit('.').next().unwrap_or(t))
        .any(|name| name == "ImportError" || name == "ModuleNotFoundError")
}

#[cfg(test)]
mod tests {
    use super::{absolute_module_name, names_import_error};

    #[test]
    fn module_names_passed_to_importlib() {
        for (name, package, head, absolute) in [
            ("pkg.config", None, false, Some("pkg.config")),
            ("pkg.config", Some("other"), false, Some("pkg.config")),
            ("pkg.parser.", None, true, Some("pkg.parser.")),
            ("", None, true, Some("")),
            (".config", Some("pkg"), false, Some("pkg.config")),
            (".", Some("pkg.parser"), false, Some("pkg.parser")),
            ("..base", Some("pkg.parser"), false, Some("pkg.base")),
            ("..", Some("pkg.parser"), false, Some("pkg")),
            ("...base", Some("pkg.parser"), false, None),
            (".config", None, false, None),
            (".config", Some(""), false, None),
            (".parser.", Some("pkg"), true, Some("pkg.parser.")),
            (".p", Some("pkg"), true, Some("pkg.p")),
            (".", Some("pkg.parser"), true, None),
            ("..", Some("pkg.parser"), true, None),
        ] {
            assert_eq!(
                absolute_module_name(name, package, head).as_deref(),
                absolute,
                "{name:?} {package:?} {head}"
            );
        }
    }

    #[test]
    fn except_clauses_naming_import_errors() {
        for (types, named) in [
            ("ImportError", true),
            ("ModuleNotFoundError", true),
            ("builtins.ImportError", true),
            ("(ImportError, OSError)", true),
            ("(\n    OSError,\n    ModuleNotFoundError,\n)", true),
            ("(ImportError,)", true),
            ("OSError", false),
            ("Exception", false),
            ("MyImportError", false),
            ("ImportErrors", false),
            ("", false),
        ] {
            assert_eq!(names_import_error(types), named, "{types}");
        }
    }
}
