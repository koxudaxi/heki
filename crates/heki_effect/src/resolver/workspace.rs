//! The members of the project's workspaces ([`WorkspaceMembers`]) in the module layout: each
//! member's own layout, the way installing the member puts its packages on the import path.

use super::{is_source_dir, source_roots};
use globset::{GlobBuilder, GlobMatcher};
use heki_env::WorkspaceMembers;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// What the workspace members add to the module layout (`ModuleLayout`).
#[derive(Debug, Default)]
pub(super) struct MemberLayout {
    /// The source roots of the members that have them (`packages/a/src`).
    pub source_roots: Vec<String>,
    /// The packages of the members without one (`packages/b/beta`), each held by one member.
    pub packages: Vec<String>,
}

/// The layout of the workspace members below `root`. A member is a directory a `members` glob
/// names, no `exclude` glob of the same table matches, and that holds a `pyproject.toml` (uv and
/// PDM require one). Its source roots are the ones the project root would have
/// ([`source_roots`]) with the member's directory name as the project name; without any, its
/// top-level packages are named below it.
pub(super) fn member_layout(
    root: &Path,
    workspaces: &[WorkspaceMembers],
    is_file: &dyn Fn(&Path) -> bool,
) -> MemberLayout {
    let mut members: BTreeSet<String> = BTreeSet::new();
    for workspace in workspaces {
        let exclude: Vec<GlobMatcher> = workspace
            .exclude
            .iter()
            .filter_map(|pattern| matcher(pattern, false))
            .collect();
        for pattern in &workspace.members {
            members.extend(expand(root, pattern).into_iter().filter(|member| {
                !member.is_empty()
                    && is_file(&root.join(member).join("pyproject.toml"))
                    && !exclude.iter().any(|g| g.is_match(member))
            }));
        }
    }
    let mut layout = MemberLayout::default();
    let mut holders: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for member in &members {
        let dir = root.join(member);
        let own = source_roots(&dir, None, is_file);
        if own.is_empty() {
            for package in packages_in(&dir, is_file) {
                holders.entry(package).or_default().push(member);
            }
        } else {
            layout
                .source_roots
                .extend(own.into_iter().map(|r| format!("{member}/{r}")));
        }
    }
    // A package several members hold (`tests`) names no single module: its files keep their
    // names below the project root, which differ.
    layout.packages = holders
        .into_iter()
        .filter_map(|(package, members)| match members[..] {
            [member] => Some(format!("{member}/{package}")),
            _ => None,
        })
        .collect();
    layout
}

/// The regular packages directly in `dir` (`beta/__init__.py`), by name.
fn packages_in(dir: &Path, is_file: &dyn Fn(&Path) -> bool) -> Vec<String> {
    subdirectories(dir)
        .filter(|(path, _)| !is_source_dir(path, is_file))
        .map(|(_, name)| name)
        .collect()
}

/// The directories in `dir` with their names (a name that is not UTF-8 names no module).
fn subdirectories(dir: &Path) -> impl Iterator<Item = (std::path::PathBuf, String)> {
    dir.read_dir()
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_owned();
            Some((path, name))
        })
}

/// The project-relative directories below `root` that the `members` glob `pattern` names. A
/// `*` stays within one path segment, and `**` spans several, as uv's globs do.
fn expand(root: &Path, pattern: &str) -> Vec<String> {
    let Some(segments) = segments(pattern) else {
        return Vec::new();
    };
    let literal = segments.iter().take_while(|s| !is_glob(s)).count();
    let base = segments[..literal].join("/");
    if literal == segments.len() {
        return if root.join(&base).is_dir() {
            vec![base]
        } else {
            Vec::new()
        };
    }
    let Some(glob) = matcher(&segments.join("/"), true) else {
        return Vec::new();
    };
    let rest = &segments[literal..];
    let depth = if rest.iter().any(|s| s.contains("**")) {
        None
    } else {
        Some(rest.len())
    };
    let mut out = Vec::new();
    walk_dirs(root, &base, depth, &mut |rel| {
        if glob.is_match(rel) {
            out.push(rel.to_owned());
        }
    });
    out
}

/// Every directory below the project-relative `dir`, at most `depth` levels down (`None`: all of
/// them, without following symbolic links, which may loop).
fn walk_dirs(root: &Path, dir: &str, depth: Option<usize>, found: &mut dyn FnMut(&str)) {
    let deeper = match depth {
        Some(0) => return,
        Some(d) => Some(d - 1),
        None => None,
    };
    for (path, name) in subdirectories(&root.join(dir)) {
        let rel = [dir, name.as_str()]
            .iter()
            .filter(|part| !part.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("/");
        found(&rel);
        if depth.is_some() || !path.is_symlink() {
            walk_dirs(root, &rel, deeper, found);
        }
    }
}

/// A workspace glob as a matcher of project-relative paths; `literal_separator` keeps `*`
/// within one segment (an `exclude` glob's `*` spans segments, as uv matches it).
fn matcher(pattern: &str, literal_separator: bool) -> Option<GlobMatcher> {
    let pattern = segments(pattern)?.join("/");
    let glob = GlobBuilder::new(&pattern)
        .literal_separator(literal_separator)
        .build()
        .ok()?;
    Some(glob.compile_matcher())
}

/// The segments of a workspace path (`./packages/*/` -> `packages`, `*`); `None` for a path
/// that leaves the project root, which names no directory below it.
fn segments(pattern: &str) -> Option<Vec<&str>> {
    if pattern.starts_with('/') {
        return None;
    }
    let mut out = Vec::new();
    for segment in pattern.split('/') {
        match segment {
            "" | "." => {}
            ".." => return None,
            _ => out.push(segment),
        }
    }
    Some(out)
}

fn is_glob(segment: &str) -> bool {
    segment.contains(['*', '?', '[', '{'])
}
