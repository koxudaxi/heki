//! What a project's `pyproject.toml` declares about the distribution it builds (ADR-022): its
//! dependencies, extras, and development groups, its entry points, and the import packages its
//! build backend ships. These are facts about the project, read next to `requires-python`, not
//! heki settings; a malformed entry only loses what heki reads from it.

use std::collections::{BTreeMap, BTreeSet};
use toml::{Table, Value};

/// The declarations of one `pyproject.toml` ([`Declared::of`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Declared {
    /// What installing the distribution installs: `[project] dependencies`, and the
    /// `[tool.poetry.dependencies]` that are not `optional` (`python` is none). Distribution
    /// names are normalized ([`normalize_distribution`]).
    pub dependencies: BTreeSet<String>,
    /// Per extra, what installing it adds: `[project.optional-dependencies]` and
    /// `[tool.poetry.extras]`.
    pub extras: BTreeMap<String, BTreeSet<String>>,
    /// Per development group, what only a development environment installs: PEP 735
    /// `[dependency-groups]` (an `include-group` takes in the group it names),
    /// `[tool.poetry.group.<name>.dependencies]`, and the older lists `[tool.uv]
    /// dev-dependencies`, `[tool.poetry.dev-dependencies]`, and `[tool.pdm.dev-dependencies]`
    /// (each in the group `dev`, or the one PDM names).
    pub groups: BTreeMap<String, BTreeSet<String>>,
    /// The `[project] dynamic` fields: a build backend computes them, so heki does not know
    /// them (`dependencies`, `optional-dependencies`, `scripts`, `entry-points`).
    pub dynamic: BTreeSet<String>,
    /// `[project.scripts]`, `[project.gui-scripts]`, `[project.entry-points.<group>]`, and
    /// `[tool.poetry.scripts]`.
    pub entry_points: Vec<EntryPoint>,
    /// The import packages the build backend's settings name: `[tool.setuptools] packages`,
    /// `[tool.hatch.build.targets.wheel] packages` (directories: the last component), and
    /// `[tool.uv.build-backend] module-name`.
    pub packages: BTreeSet<String>,
}

/// One entry point the distribution installs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntryPoint {
    /// `scripts`, `gui-scripts`, or the group of an `entry-points` table.
    pub group: String,
    pub name: String,
    /// The module it imports (`pkg.cli` of `pkg.cli:main`).
    pub module: String,
}

impl Declared {
    /// The declarations of the `pyproject.toml` parsed as `table`: `None` when it has neither a
    /// `[project]` nor a `[tool.poetry]` table, where the dependencies live elsewhere
    /// (`setup.py`, `setup.cfg`) or nowhere.
    pub fn of(table: &Table) -> Option<Self> {
        let project = table.get("project").and_then(Value::as_table);
        let tool = table.get("tool").and_then(Value::as_table);
        let poetry = tool.and_then(|t| t.get("poetry")).and_then(Value::as_table);
        if project.is_none() && poetry.is_none() {
            return None;
        }
        let mut declared = Self::default();
        if let Some(project) = project {
            declared.read_project(project);
        }
        if let Some(poetry) = poetry {
            declared.read_poetry(poetry);
        }
        if let Some(groups) = table.get("dependency-groups").and_then(Value::as_table) {
            for name in groups.keys() {
                let mut members = BTreeSet::new();
                dependency_group(groups, name, &mut Vec::new(), &mut members);
                declared.add_group(name, members);
            }
        }
        let tool_table = |name: &str| tool.and_then(|t| t.get(name)).and_then(Value::as_table);
        if let Some(uv) = tool_table("uv") {
            declared.add_group("dev", requirements(uv.get("dev-dependencies")));
            let backend = uv.get("build-backend").and_then(|b| b.get("module-name"));
            declared
                .packages
                .extend(strings(backend).map(str::to_owned));
        }
        if let Some(pdm) = tool_table("pdm").and_then(|p| p.get("dev-dependencies")) {
            for (group, list) in pdm.as_table().into_iter().flatten() {
                declared.add_group(group, requirements(Some(list)));
            }
        }
        let setuptools = tool_table("setuptools").and_then(|s| s.get("packages"));
        declared
            .packages
            .extend(strings(setuptools).map(str::to_owned));
        let wheel = tool_table("hatch").and_then(|h| {
            h.get("build")?
                .get("targets")?
                .get("wheel")?
                .get("packages")
        });
        declared.packages.extend(strings(wheel).map(|dir| {
            dir.trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(dir)
                .to_owned()
        }));
        Some(declared)
    }

    fn read_project(&mut self, project: &Table) {
        self.dependencies
            .extend(requirements(project.get("dependencies")));
        for (extra, list) in table_entries(project.get("optional-dependencies")) {
            (self
                .extras
                .entry(normalize_distribution(extra))
                .or_default())
            .extend(requirements(Some(list)));
        }
        self.dynamic
            .extend(strings(project.get("dynamic")).map(str::to_owned));
        for group in ["scripts", "gui-scripts"] {
            self.add_entry_points(group, project.get(group));
        }
        for (group, points) in table_entries(project.get("entry-points")) {
            self.add_entry_points(group, Some(points));
        }
    }

    fn read_poetry(&mut self, poetry: &Table) {
        for (name, spec) in table_entries(poetry.get("dependencies")) {
            let optional = spec.get("optional").and_then(Value::as_bool) == Some(true);
            if name != "python" && !optional {
                self.dependencies.insert(normalize_distribution(name));
            }
        }
        for (extra, names) in table_entries(poetry.get("extras")) {
            (self
                .extras
                .entry(normalize_distribution(extra))
                .or_default())
            .extend(strings(Some(names)).map(normalize_distribution));
        }
        let names = |deps: Option<&Value>| -> BTreeSet<String> {
            table_entries(deps)
                .map(|(name, _)| normalize_distribution(name))
                .collect()
        };
        self.add_group("dev", names(poetry.get("dev-dependencies")));
        for (group, table) in table_entries(poetry.get("group")) {
            self.add_group(group, names(table.get("dependencies")));
        }
        for (name, target) in table_entries(poetry.get("scripts")) {
            let reference = target.as_str().or_else(|| target.get("callable")?.as_str());
            if let Some(module) = reference.and_then(entry_point_module) {
                self.entry_points.push(EntryPoint {
                    group: "scripts".to_owned(),
                    name: name.clone(),
                    module,
                });
            }
        }
    }

    fn add_group(&mut self, name: &str, members: BTreeSet<String>) {
        if !members.is_empty() {
            (self.groups.entry(normalize_distribution(name)).or_default()).extend(members);
        }
    }

    fn add_entry_points(&mut self, group: &str, points: Option<&Value>) {
        for (name, reference) in table_entries(points) {
            if let Some(module) = reference.as_str().and_then(entry_point_module) {
                self.entry_points.push(EntryPoint {
                    group: group.to_owned(),
                    name: name.clone(),
                    module,
                });
            }
        }
    }
}

/// A distribution name as PEP 503 normalizes it: lowercase, each run of `-`, `_`, and `.`
/// one `-` (`Typing_Extensions` and `typing.extensions` are `typing-extensions`).
pub fn normalize_distribution(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for part in name.split(['-', '_', '.']).filter(|part| !part.is_empty()) {
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&part.to_ascii_lowercase());
    }
    out
}

/// The distribution a PEP 508 requirement names, normalized: `requests` of
/// `requests[socks]>=2; python_version < "3.12"` and of `requests @ https://...`. `None` when
/// it starts with no name.
pub fn requirement_name(requirement: &str) -> Option<String> {
    let requirement = requirement.trim_start();
    let end = requirement
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(requirement.len());
    Some(normalize_distribution(&requirement[..end])).filter(|name| !name.is_empty())
}

/// The module an entry point reference imports: `pkg.cli` of `pkg.cli:main` and of `pkg.cli`
/// (the extras of `pkg.cli:main [extra]` aside).
fn entry_point_module(reference: &str) -> Option<String> {
    let module = reference.split([':', '[']).next()?.trim();
    (!module.is_empty()).then(|| module.to_owned())
}

/// The members of the PEP 735 group `name` of `groups`, the groups it includes taken in
/// (`stack`: the groups being read, where an include cycle stops).
fn dependency_group(
    groups: &Table,
    name: &str,
    stack: &mut Vec<String>,
    out: &mut BTreeSet<String>,
) {
    let key = normalize_distribution(name);
    if stack.contains(&key) {
        return;
    }
    stack.push(key.clone());
    let entries = groups
        .iter()
        .filter(|(group, _)| normalize_distribution(group) == key)
        .flat_map(|(_, list)| list.as_array().into_iter().flatten());
    for entry in entries {
        match entry {
            Value::String(requirement) => out.extend(requirement_name(requirement)),
            other => {
                if let Some(included) = other.get("include-group").and_then(Value::as_str) {
                    dependency_group(groups, included, stack, out);
                }
            }
        }
    }
    stack.pop();
}

/// The distributions an array of PEP 508 requirements names.
fn requirements(list: Option<&Value>) -> BTreeSet<String> {
    strings(list).filter_map(requirement_name).collect()
}

/// The strings of an array value, or the string itself.
fn strings(value: Option<&Value>) -> impl Iterator<Item = &str> {
    let (one, many) = match value {
        Some(Value::String(s)) => (Some(s.as_str()), None),
        Some(Value::Array(items)) => (None, Some(items.iter().filter_map(Value::as_str))),
        _ => (None, None),
    };
    one.into_iter().chain(many.into_iter().flatten())
}

/// The entries of a table value.
fn table_entries(value: Option<&Value>) -> impl Iterator<Item = (&String, &Value)> {
    value.and_then(Value::as_table).into_iter().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distribution_names_and_requirements() {
        for (name, normalized) in [
            ("requests", "requests"),
            ("Typing_Extensions", "typing-extensions"),
            ("zope.interface", "zope-interface"),
            ("a__b--c", "a-b-c"),
        ] {
            assert_eq!(normalize_distribution(name), normalized);
        }
        for (requirement, name) in [
            ("requests", Some("requests")),
            (
                "requests[socks] >= 2; python_version < '3.12'",
                Some("requests"),
            ),
            (" PyYAML>=6", Some("pyyaml")),
            ("pkg @ https://example.com/pkg.whl", Some("pkg")),
            ("@ nothing", None),
            ("", None),
        ] {
            assert_eq!(
                requirement_name(requirement).as_deref(),
                name,
                "{requirement}"
            );
        }
        for (reference, module) in [
            ("pkg.cli:main", Some("pkg.cli")),
            ("pkg.cli", Some("pkg.cli")),
            ("pkg.cli:main [extra]", Some("pkg.cli")),
            (":main", None),
        ] {
            assert_eq!(
                entry_point_module(reference).as_deref(),
                module,
                "{reference}"
            );
        }
    }

    #[test]
    fn declarations_of_pyproject_tables() {
        let set = |names: &[&str]| {
            names
                .iter()
                .map(|n| (*n).to_owned())
                .collect::<BTreeSet<_>>()
        };
        let parse = |text: &str| Declared::of(&toml::from_str::<Table>(text).unwrap());
        assert_eq!(parse("[tool.ruff]\nline-length = 1\n"), None);
        let declared = parse(
            r#"
[project]
name = "acme"
dependencies = ["requests>=2", "PyYAML; sys_platform != 'win32'"]
dynamic = ["version"]
[project.optional-dependencies]
Fast = ["orjson"]
[project.scripts]
acme = "acme.cli:main"
[project.gui-scripts]
acme-gui = "acme.gui"
[project.entry-points."acme.plugins"]
csv = "acme.plugins.csv:Plugin"
broken = 1
[dependency-groups]
test = ["pytest", {include-group = "lint"}, {include-group = "test"}, 1]
lint = ["ruff"]
[tool.uv]
dev-dependencies = ["ipython"]
build-backend = { module-name = "acme" }
[tool.pdm.dev-dependencies]
docs = ["mkdocs"]
[tool.setuptools]
packages = ["acme_tools", "acme_extra"]
[tool.hatch.build.targets.wheel]
packages = ["src/acme_hatch/", "plain"]
"#,
        )
        .unwrap();
        assert_eq!(declared.dependencies, set(&["requests", "pyyaml"]));
        assert_eq!(
            declared.extras,
            BTreeMap::from([("fast".to_owned(), set(&["orjson"]))])
        );
        assert_eq!(
            declared.groups,
            BTreeMap::from([
                ("dev".to_owned(), set(&["ipython"])),
                ("docs".to_owned(), set(&["mkdocs"])),
                ("lint".to_owned(), set(&["ruff"])),
                ("test".to_owned(), set(&["pytest", "ruff"])),
            ])
        );
        assert_eq!(declared.dynamic, set(&["version"]));
        let points: Vec<(&str, &str, &str)> = (declared.entry_points.iter())
            .map(|p| (p.group.as_str(), p.name.as_str(), p.module.as_str()))
            .collect();
        assert_eq!(
            points,
            vec![
                ("scripts", "acme", "acme.cli"),
                ("gui-scripts", "acme-gui", "acme.gui"),
                ("acme.plugins", "csv", "acme.plugins.csv"),
            ]
        );
        assert_eq!(
            declared.packages,
            set(&["acme", "acme_extra", "acme_hatch", "acme_tools", "plain"])
        );
        let poetry = parse(
            r#"
[tool.poetry]
name = "acme"
packages = [{include = "acme"}]
[tool.poetry.dependencies]
python = "^3.11"
Requests = "^2"
uvloop = {version = "*", optional = true}
[tool.poetry.extras]
speed = ["uvloop"]
[tool.poetry.dev-dependencies]
pytest = "*"
[tool.poetry.group.lint.dependencies]
ruff = "*"
[tool.poetry.scripts]
acme = "acme.cli:main"
other = {callable = "acme.other:run"}
"#,
        )
        .unwrap();
        assert_eq!(poetry.dependencies, set(&["requests"]));
        assert_eq!(
            poetry.extras,
            BTreeMap::from([("speed".to_owned(), set(&["uvloop"]))])
        );
        assert_eq!(
            poetry.groups,
            BTreeMap::from([
                ("dev".to_owned(), set(&["pytest"])),
                ("lint".to_owned(), set(&["ruff"])),
            ])
        );
        let modules: Vec<&str> = poetry
            .entry_points
            .iter()
            .map(|p| p.module.as_str())
            .collect();
        assert_eq!(modules, vec!["acme.cli", "acme.other"]);
    }
}
