//! Which declared distribution provides a module outside the project, and how the project
//! declares it (ADR-022): the premise `IMP301` and `IMP302` rest on. The map from a module to the
//! distributions that may provide it is the user's (`[tool.heki.imports] module-distributions`)
//! when it names the module, else the catalog's `provides` rows together with the distributions
//! named like the module or a package holding it (PEP 503 names: `ruamel.yaml` is
//! `ruamel-yaml`). Nothing else may decide it, and an answer it cannot give is no report.

use heki_env::{Declared, HekiConfig, normalize_distribution};
use heki_kb::EffectiveEcosystemKb;
use rustc_hash::FxHashSet;
use std::collections::{BTreeMap, BTreeSet};

/// Where the project declares every distribution that may provide a module, when it declares
/// them all in one kind of place other than its dependencies ([`Distributions::declared_only`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Only {
    /// In development groups (`[dependency-groups]` and the older tool tables) alone.
    Development,
    /// In extras (`[project.optional-dependencies]`, `[tool.poetry.extras]`), maybe also in
    /// development groups.
    Extras,
}

/// The distributions that may provide a module, all declared [`Only`] in one kind of place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredOnly {
    pub only: Only,
    /// Each distribution (normalized), with the groups or extras declaring it, ascending.
    pub distributions: Vec<(String, Vec<String>)>,
}

/// The project's declarations, the catalog, and the user's map: what [`Self::declared_only`]
/// asks.
pub struct Distributions<'d> {
    declared: &'d Declared,
    kb: &'d EffectiveEcosystemKb,
    user: &'d BTreeMap<String, Vec<String>>,
    /// Every distribution the project declares anywhere, normalized, but the project's own: a
    /// requirement naming the project (`all = ["acme[redis,yaml]"]`) adds its extras, not a
    /// distribution.
    names: BTreeSet<&'d str>,
    /// The top-level names of the project's own copies of modules.
    copies: FxHashSet<&'d str>,
}

impl<'d> Distributions<'d> {
    /// The declarations of the project `cfg` describes, with the top-level names of its own
    /// copies of modules (`copies`, [`super::ImportGraph::loose_names`]); `None` when they do not
    /// say what installing the package installs: no `pyproject.toml` declaring the
    /// distribution, or dependencies or extras a build backend computes (`[project] dynamic`).
    pub fn of(
        cfg: &'d HekiConfig,
        kb: &'d EffectiveEcosystemKb,
        copies: FxHashSet<&'d str>,
    ) -> Option<Self> {
        let declared = cfg.declared.as_ref()?;
        let computed = ["dependencies", "optional-dependencies"];
        if computed
            .iter()
            .any(|field| declared.dynamic.contains(*field))
        {
            return None;
        }
        let own = cfg.project_name.as_deref().map(normalize_distribution);
        let names = (declared.dependencies.iter())
            .chain(declared.extras.values().flatten())
            .chain(declared.groups.values().flatten())
            .filter(|d| own.as_ref() != Some(*d))
            .map(String::as_str)
            .collect();
        Some(Self {
            declared,
            kb,
            user: &cfg.imports.module_distributions,
            names,
            copies,
        })
    }

    /// The declared distributions that may provide the module `module`, outside the project,
    /// normalized and ascending: the user's map for the longest of `module` and the packages
    /// holding it that it names; without one, the catalog's distributions for it and the
    /// distributions named like it or a package holding it. A distribution the project does not
    /// declare is left out. Empty for a module of the standard library, and, without the user's
    /// map, for one the project may hold a copy of: a module or package of its top-level name in
    /// a directory of the project that is no regular package, which a `sys.path` entry may name
    /// (`setuptools/_vendor/packaging/`, which `setuptools` puts on `sys.path`).
    pub fn providing(&self, module: &str) -> Vec<&'d str> {
        if heki_env::is_stdlib_module(module) {
            return Vec::new();
        }
        let packages = || {
            (module.match_indices('.').map(|(i, _)| &module[..i])).chain(std::iter::once(module))
        };
        let top = packages().next().unwrap_or(module);
        let candidates: BTreeSet<String> = match packages().rev().find_map(|p| self.user.get(p)) {
            Some(listed) => listed.iter().map(|d| normalize_distribution(d)).collect(),
            None if self.copies.contains(top) => BTreeSet::new(),
            None => (self.kb.distributions_providing(module))
                .chain(packages())
                .map(normalize_distribution)
                .collect(),
        };
        (candidates.iter())
            .filter_map(|d| self.names.get(d.as_str()).copied())
            .collect()
    }

    /// Where the project declares the distributions that may provide `module`, when it declares
    /// at least one and each of them [`Only`] in one kind of place: in none of its dependencies,
    /// and all in development groups alone, or all in extras.
    pub fn declared_only(&self, module: &str) -> Option<DeclaredOnly> {
        let providing = self.providing(module);
        let declared = self.declared;
        let in_tables = |tables: &BTreeMap<String, BTreeSet<String>>, d: &str| -> Vec<String> {
            (tables.iter())
                .filter(|(_, members)| members.contains(d))
                .map(|(name, _)| name.clone())
                .collect()
        };
        let mut kinds: Vec<(Only, String, Vec<String>)> = Vec::new();
        for d in providing {
            if declared.dependencies.contains(d) {
                return None;
            }
            let extras = in_tables(&declared.extras, d);
            let only = if extras.is_empty() {
                (Only::Development, in_tables(&declared.groups, d))
            } else {
                (Only::Extras, extras)
            };
            kinds.push((only.0, d.to_owned(), only.1));
        }
        let only = kinds.first()?.0;
        (kinds.iter().all(|(k, _, _)| *k == only)).then(|| DeclaredOnly {
            only,
            distributions: (kinds.into_iter())
                .map(|(_, d, places)| (d, places))
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distributions_declared_only_for_development_or_in_extras() {
        let set = |names: &[&str]| -> BTreeSet<String> {
            names.iter().map(|n| normalize_distribution(n)).collect()
        };
        let tables = |entries: &[(&str, &[&str])]| -> BTreeMap<String, BTreeSet<String>> {
            (entries.iter())
                .map(|(name, members)| ((*name).to_owned(), set(members)))
                .collect()
        };
        let declared = Declared {
            dependencies: set(&["requests", "psycopg2-binary", "zope.interface"]),
            extras: tables(&[
                ("redis", &["redis", "hiredis", "acme"]),
                ("yaml", &["PyYAML"]),
                ("both", &["orjson"]),
            ]),
            groups: tables(&[
                ("test", &["pytest", "orjson", "zope", "opencv-python"]),
                ("lint", &["ruff", "hiredis"]),
                ("docs", &["mkdocs", "ruamel.yaml", "attr"]),
            ]),
            ..Declared::default()
        };
        let mut cfg = HekiConfig {
            project_name: Some("Acme".to_owned()),
            declared: Some(declared),
            ..HekiConfig::default()
        };
        let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        let dev = |names: &[(&str, &[&str])]| {
            Some(DeclaredOnly {
                only: Only::Development,
                distributions: (names.iter())
                    .map(|(d, g)| ((*d).to_owned(), g.iter().map(|g| (*g).to_owned()).collect()))
                    .collect(),
            })
        };
        let extras = |names: &[(&str, &[&str])]| {
            dev(names).map(|d| DeclaredOnly {
                only: Only::Extras,
                ..d
            })
        };
        let distributions = Distributions::of(&cfg, &kb, FxHashSet::default()).unwrap();
        for (module, found) in [
            ("pytest", dev(&[("pytest", &["test"])])),
            // The catalog: `pytest` installs `_pytest`.
            ("_pytest.fixtures", dev(&[("pytest", &["test"])])),
            ("ruff", dev(&[("ruff", &["lint"])])),
            // A namespace package's portion is named like the package holding the module.
            ("ruamel.yaml.main", dev(&[("ruamel-yaml", &["docs"])])),
            // `zope.interface` may come from `zope` (development) or `zope-interface`
            // (installed): no report.
            ("zope.interface", None),
            ("zope", dev(&[("zope", &["test"])])),
            ("redis", extras(&[("redis", &["redis"])])),
            ("redis.asyncio", extras(&[("redis", &["redis"])])),
            ("yaml", extras(&[("pyyaml", &["yaml"])])),
            // In an extra and a development group: an extra.
            ("hiredis", extras(&[("hiredis", &["redis"])])),
            ("orjson", extras(&[("orjson", &["both"])])),
            ("requests", None),
            // The catalog maps `psycopg2` to `psycopg2-binary`, a dependency.
            ("psycopg2", None),
            // Both catalog distributions of `cv2` that the project declares: one.
            ("cv2", dev(&[("opencv-python", &["test"])])),
            // `attrs` installs `attr`; the project declares the unrelated `attr` only.
            ("attr", dev(&[("attr", &["docs"])])),
            ("numpy", None),
            ("json", None),
            ("os.path", None),
            // The project's own name in an extra adds that extra, no distribution.
            ("acme.plugins", None),
        ] {
            assert_eq!(distributions.declared_only(module), found, "{module}");
        }
        // A copy of `ruamel` the project holds outside its packages may be the one imported.
        let copies = Distributions::of(&cfg, &kb, ["ruamel", "packaging"].into_iter().collect());
        let copies = copies.unwrap();
        assert_eq!(copies.declared_only("ruamel.yaml.main"), None);
        assert_eq!(
            copies.declared_only("pytest"),
            dev(&[("pytest", &["test"])])
        );
        // The user's map decides for the module and the modules below it.
        cfg.imports.module_distributions = BTreeMap::from([
            ("zope.interface".to_owned(), vec!["Zope".to_owned()]),
            ("cv2".to_owned(), vec!["opencv-contrib-python".to_owned()]),
            (
                "yaml".to_owned(),
                vec!["PyYAML".to_owned(), "requests".to_owned()],
            ),
        ]);
        let distributions = Distributions::of(&cfg, &kb, FxHashSet::default()).unwrap();
        for (module, found) in [
            ("zope.interface.verify", dev(&[("zope", &["test"])])),
            ("cv2", None),
            ("yaml", None),
            ("pytest", dev(&[("pytest", &["test"])])),
        ] {
            assert_eq!(distributions.declared_only(module), found, "{module}");
        }
        // Dependencies a build backend computes: nothing is known.
        cfg.declared = Some(Declared {
            dynamic: set(&["dependencies"]),
            ..Declared::default()
        });
        assert!(Distributions::of(&cfg, &kb, FxHashSet::default()).is_none());
        cfg.declared = None;
        assert!(Distributions::of(&cfg, &kb, FxHashSet::default()).is_none());
    }
}
