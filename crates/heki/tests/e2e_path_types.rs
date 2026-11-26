//! A value made from a `pathlib.Path` by a method, an operator, or a property is a `Path` too
//! (`return_type = "Self"` and `[[attributes]]` rows of the catalog), so a file system call on
//! it is followed: `blocking-fs` boundaries over functions that open or test a path they derive
//! from another. A value that is not known to be a path, and a method a path does not give a
//! path from, stay untyped.

mod common;

use common::Project;
use serde_json::Value;

const CONFIG: &str = r#"[project]
name = "t"
requires-python = ">=3.11"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "fs"
scopes = ["pkg.probe:*"]
forbid = ["blocking-fs"]
"#;

/// `iterdir`, `glob`, and `rglob` made not blocking, so that a call on an item they give is
/// the one the boundary reports.
const LISTING_NOT_BLOCKING: &str = r#"
[[tool.heki.ecosystem]]
symbol = "pathlib.Path.iterdir"
effects = []

[[tool.heki.ecosystem]]
symbol = "pathlib.Path.glob().__iter__"
effects = []

[[tool.heki.ecosystem]]
symbol = "pathlib.Path.rglob().__iter__"
effects = []
"#;

/// The functions of `pkg/probe.py` the `fs` boundary reports, with the call it reaches.
fn reported(config: &str, source: &str) -> Vec<(String, String)> {
    let p = Project::new("path-types");
    p.write("pyproject.toml", config);
    p.write("pkg/__init__.py", "");
    p.write("pkg/probe.py", source);
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(String, String)> = diags
        .iter()
        .map(|d| {
            let message = d["message"].as_str().unwrap();
            let call = message.split('`').nth(3).unwrap_or(message);
            (d["qualname"].as_str().unwrap().to_owned(), call.to_owned())
        })
        .collect();
    out.sort();
    out
}

fn names(reports: &[(String, String)]) -> Vec<&str> {
    reports.iter().map(|(name, _)| name.as_str()).collect()
}

/// What a method, an operator, and a property of a path give is a path: the call on it reaches
/// the file system.
#[test]
fn what_a_path_derives_is_a_path() {
    let reports = reported(
        CONFIG,
        r#"from pathlib import Path


def control_open(path: Path) -> None:
    with path.open("wt") as file:
        file.write("x")


def joinpath_open(output: Path) -> None:
    with output.joinpath("a.py").open("wt") as file:
        file.write("x")


def walrus_joinpath_open(output: Path, paths: list[Path]) -> None:
    paths.append(path := output.joinpath("a.py"))
    with path.open("wt") as file:
        file.write("x")


def div_open(output: Path) -> None:
    with (output / "a.py").open("wt") as file:
        file.write("x")


def reflected_div(output: Path) -> bool:
    return ("a" / output).exists()


def augmented_div(output: Path) -> bool:
    output /= "a"
    return output.is_file()


def with_suffix_exists(path: Path) -> bool:
    return path.with_suffix(".py").exists()


def with_name_exists(path: Path) -> bool:
    return path.with_name("b").exists()


def with_stem_exists(path: Path) -> bool:
    return path.with_stem("b").exists()


def relative_exists(path: Path) -> bool:
    return path.relative_to("/").exists()


def parent_exists(name: str) -> bool:
    return Path(name).parent.exists()


def parent_of_parent(path: Path) -> bool:
    return path.parent.parent.exists()


def absolute_exists(path: Path) -> bool:
    return path.absolute().exists()


def expanduser_exists(path: Path) -> bool:
    return path.expanduser().exists()


def cwd_exists() -> bool:
    return Path.cwd().exists()


def home_exists() -> bool:
    return Path.home().exists()


def chained(path: Path) -> bool:
    return (path.parent / "a").with_suffix(".py").exists()
"#,
    );
    assert_eq!(
        names(&reports),
        [
            "absolute_exists",
            "augmented_div",
            "chained",
            "control_open",
            "cwd_exists",
            "div_open",
            "expanduser_exists",
            "home_exists",
            "joinpath_open",
            "parent_exists",
            "parent_of_parent",
            "reflected_div",
            "relative_exists",
            "walrus_joinpath_open",
            "with_name_exists",
            "with_stem_exists",
            "with_suffix_exists",
        ],
        "{reports:?}"
    );
}

/// The items of `iterdir`, `glob`, `rglob`, and `parents` are paths.
#[test]
fn the_items_of_a_listing_are_paths() {
    let reports = reported(
        &format!("{CONFIG}{LISTING_NOT_BLOCKING}"),
        r#"from pathlib import Path


def iterdir_items(folder: Path) -> None:
    for entry in folder.iterdir():
        entry.exists()


def glob_items(folder: Path) -> None:
    for entry in folder.glob("*"):
        entry.exists()


def rglob_items(folder: Path) -> None:
    for entry in folder.rglob("*"):
        entry.exists()


def parents_items(folder: Path) -> None:
    for entry in folder.parents:
        entry.exists()


def parents_index(folder: Path) -> bool:
    return folder.parents[1].exists()


def listed(folder: Path) -> bool:
    return list(folder.iterdir())[0].exists()
"#,
    );
    assert_eq!(
        names(&reports),
        [
            "glob_items",
            "iterdir_items",
            "listed",
            "parents_index",
            "parents_items",
            "rglob_items",
        ],
        "{reports:?}"
    );
}

/// A value that is not known to be a path stays untyped through the operators and members that
/// give a path from one, and a project class that defines the operator or the name is not a path.
#[test]
fn a_value_not_known_to_be_a_path_is_not_one() {
    let reports = reported(
        CONFIG,
        r#"from pathlib import Path


def unknown_left(other, folder: Path) -> bool:
    return (other / folder).exists()


def numbers(a: int, b: str) -> bool:
    return (a / 2).exists() or (b / 3).exists()


class Scope:
    parent = None

    def __truediv__(self, other):
        return None

    def joinpath(self, other):
        return None


def project_class(scope: Scope) -> bool:
    return (scope / "a").exists() or scope.joinpath("a").exists() or scope.parent.exists()


def untyped_list(items: list) -> bool:
    return items[0].exists()


def name_of(path: Path) -> bool:
    return path.name.exists() or path.stem.exists() or path.suffix.exists()
"#,
    );
    assert_eq!(names(&reports), Vec::<&str>::new(), "{reports:?}");
}
