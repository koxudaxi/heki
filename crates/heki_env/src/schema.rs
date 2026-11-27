//! The tables of the configuration with the keys each accepts, derived from the types that read
//! them, and the checks that find a misplaced or misspelled key, and the key of a value of the
//! wrong type, so that the error can name the table the key belongs in and show a value it
//! takes.

use crate::defaults::{NOT_DEFAULTED, SUFFIX};
use crate::problem::{settings_root, table_items, table_name};
use crate::{
    CallOwnerRule, ConfigError, ConfigLayout, EcosystemOverride, EffectBoundaryRule, HekiConfig,
    ImportBoundaryRule, ImportsConfig, InvalidationRule, LintConfig, OrderedOutputSetting,
    ReadOnlyReturnRule, ReadOwnerRule, WriteOwnerRule,
};
use crate::{Docs, Problem};
use serde::Serialize;
use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;
use toml_edit::TableLike;

/// A table of the configuration: `lint`, a contract, or an entry of one (`ecosystem`); the
/// empty path is the top level.
struct Table {
    path: &'static str,
    /// `None` for a table whose keys are the user's: rule codes, globs.
    keys: Option<Vec<String>>,
}

fn keys_of<T: Default + Serialize>() -> Option<Vec<String>> {
    let value = serde_json::to_value(T::default()).expect("a configuration type serializes");
    Some(
        value
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, _)| k.clone())
            .collect(),
    )
}

fn tables() -> &'static [Table] {
    static TABLES: OnceLock<Vec<Table>> = OnceLock::new();
    TABLES.get_or_init(|| {
        let table = |path, keys| Table { path, keys };
        // The keys of a contract's defaults table: all but the ones that name one contract.
        let defaults = |path, keys: Option<Vec<String>>| {
            let keys = keys.map(|keys| {
                (keys.into_iter())
                    .filter(|k| !NOT_DEFAULTED.contains(&k.as_str()))
                    .collect()
            });
            Table { path, keys }
        };
        vec![
            table("", keys_of::<HekiConfig>()),
            table("lint", keys_of::<LintConfig>()),
            table("lint.severity", None),
            table("lint.per-file-ignores", None),
            table("write-owners", keys_of::<WriteOwnerRule>()),
            table("read-owners", keys_of::<ReadOwnerRule>()),
            table("call-owners", keys_of::<CallOwnerRule>()),
            table("effect-boundaries", keys_of::<EffectBoundaryRule>()),
            table("readonly-returns", keys_of::<ReadOnlyReturnRule>()),
            table("invalidation", keys_of::<InvalidationRule>()),
            table("import-boundaries", keys_of::<ImportBoundaryRule>()),
            defaults("write-owners-defaults", keys_of::<WriteOwnerRule>()),
            defaults("read-owners-defaults", keys_of::<ReadOwnerRule>()),
            defaults("call-owners-defaults", keys_of::<CallOwnerRule>()),
            defaults(
                "effect-boundaries-defaults",
                keys_of::<EffectBoundaryRule>(),
            ),
            defaults("readonly-returns-defaults", keys_of::<ReadOnlyReturnRule>()),
            defaults("invalidation-defaults", keys_of::<InvalidationRule>()),
            defaults(
                "import-boundaries-defaults",
                keys_of::<ImportBoundaryRule>(),
            ),
            table("groups", None),
            table("imports", keys_of::<ImportsConfig>()),
            table("imports.module-distributions", None),
            table("ecosystem", keys_of::<EcosystemOverride>()),
            table(
                "ecosystem.writes-in-order",
                keys_of::<OrderedOutputSetting>(),
            ),
        ]
    })
}

fn table(path: &str) -> Option<&'static Table> {
    tables().iter().find(|t| t.path == path)
}

fn child(table: &str, key: &str) -> String {
    if table.is_empty() {
        key.to_owned()
    } else {
        format!("{table}.{key}")
    }
}

/// A value each key takes, by `table.key`, else by the key alone (keys every contract has); in a
/// table whose keys are the user's (`lint.severity`), by `table.*`. The tables (`lint`, the
/// contracts, `ecosystem`) are shown by their headers instead.
const EXAMPLES: &[(&str, &str)] = &[
    ("target-version", r#""py312""#),
    ("required-version", r#"">=0.2""#),
    ("exclude", r#"[".venv", "build"]"#),
    ("extend-exclude", r#"["generated"]"#),
    ("src", r#"["src"]"#),
    ("baseline", r#""heki-baseline.toml""#),
    ("include", r#"["contracts/store.toml"]"#),
    ("groups.*", r#"["pkg.parser.*", "pkg.cli:parse"]"#),
    ("lint.select", r#"["EFF", "MUT101"]"#),
    ("lint.extend-select", r#"["EFF105"]"#),
    ("lint.ignore", r#"["OPT110"]"#),
    ("lint.severity", r#"{ MUT110 = "error" }"#),
    ("lint.per-file-ignores", r#"{ "tests/**" = ["EFF"] }"#),
    ("lint.severity.*", r#""error""#),
    ("lint.per-file-ignores.*", r#"["EFF"]"#),
    ("lint.relax-async-in-tests", "false"),
    ("lint.heuristic", "true"),
    ("lint.fail-on", r#""warning""#),
    ("lint.fail", r#"["IMP101"]"#),
    ("lint.report-unused-suppressions", "true"),
    ("lint.require-suppression-codes", "true"),
    ("lint.unmatched-contract-entries", r#""error""#),
    ("name", r#""generation-store""#),
    ("targets", r#"["pkg.model.DataModel.fields"]"#),
    ("message", r#""use GenerationStore.set_fields() instead""#),
    ("fail", "true"),
    ("source", r#""scripts/tables.py:120""#),
    (
        "entry-sources",
        r#"{ "pkg.model.DataModel.fields" = "scripts/tables.py:131" }"#,
    ),
    ("exempt-methods", r#"["__deepcopy__"]"#),
    ("fresh-objects", r#""allow""#),
    ("unresolved", r#""report""#),
    ("attribution", r#""lexical""#),
    ("own-receiver", r#""allow""#),
    ("allowed-protocols", r#"["pkg.protocols.Writable"]"#),
    ("protocol-subclasses", "false"),
    ("references", r#""calls""#),
    ("include-overrides", "true"),
    (
        "write-owners.allowed-writers",
        r#"["pkg.parser.generation.*"]"#,
    ),
    ("write-owners.forbidden-writers", r#"["pkg.views.*"]"#),
    ("write-owners.forbid-cached-property-on", r#"["DataModel"]"#),
    ("write-owners.unlisted-dict-keys", r#""report""#),
    ("read-owners.allowed-readers", r#"["*GenerationStore*"]"#),
    ("read-owners.forbidden-readers", r#"["pkg.views.*"]"#),
    ("call-owners.targets", r#"["pkg.db.Session.commit"]"#),
    ("call-owners.allowed-callers", r#"["pkg.db.*"]"#),
    ("call-owners.forbidden-callers", r#"["pkg.views.*"]"#),
    ("call-owners.subclasses", "false"),
    ("call-owners.targets-call-each-other", r#""report""#),
    ("effect-boundaries.scopes", r#"["*.render"]"#),
    (
        "effect-boundaries.forbid",
        r#"["writes-targets", "blocking"]"#,
    ),
    ("effect-boundaries.write-targets", r#"["DataModel.*"]"#),
    (
        "effect-boundaries.read-targets",
        r#"["DataType.is_optional"]"#,
    ),
    ("effect-boundaries.call-targets", r#"["GenerationStore.*"]"#),
    (
        "effect-boundaries.allow-writes",
        r#"["DataModel.base_classes"]"#,
    ),
    ("effect-boundaries.write-universe", r#"["DataModel.*"]"#),
    ("effect-boundaries.allowed-callees", r#"["pkg.logging.*"]"#),
    ("effect-boundaries.import-probes", r#""report""#),
    (
        "readonly-returns.producers",
        r#"["pkg.parser.SchemaGraphBuilder.schema"]"#,
    ),
    ("readonly-returns.deep", "true"),
    (
        "invalidation.invalidators",
        r#"["GenerationStore.refresh_now"]"#,
    ),
    (
        "invalidation.exempt-writers",
        r#"["pkg.parser.generation.*"]"#,
    ),
    ("import-boundaries.importers", r#"["pkg.model.*"]"#),
    (
        "import-boundaries.exempt-importers",
        r#"["pkg.model.testing"]"#,
    ),
    (
        "import-boundaries.allowed-imports",
        r#"["<stdlib>", "pkg.model.*"]"#,
    ),
    (
        "import-boundaries.forbidden-imports",
        r#"["pkg.backends.*"]"#,
    ),
    (
        "import-boundaries.relative-imports",
        r#"["pkg._runtime.*"]"#,
    ),
    (
        "import-boundaries.ignored-imports",
        r#"["pkg.model.legacy -> pkg.backends.sql"]"#,
    ),
    ("import-boundaries.type-checking", r#""report""#),
    ("import-boundaries.reach", r#""import-time""#),
    ("imports.cycle-edges", r#"["body", "lazy"]"#),
    ("imports.module-distributions", r#"{ yaml = ["PyYAML"] }"#),
    ("imports.module-distributions.*", r#"["PyYAML"]"#),
    ("ecosystem.symbol", r#""mypkg.db.run_query""#),
    ("ecosystem.effects", r#"["blocking_db", "db_query"]"#),
    ("ecosystem.async-safe", "true"),
    ("ecosystem.offloads-blocking-arg", "0"),
    ("ecosystem.excluded", "true"),
    ("ecosystem.copy-sanitizer", r#""deep""#),
    ("ecosystem.return-type", r#""sqlite3.Connection""#),
    ("ecosystem.sync-lock-or-tx", "true"),
    ("ecosystem.non-reentrant-lock", "true"),
    ("ecosystem.one-at-a-time", r#""always""#),
    (
        "ecosystem.writes-in-order",
        r#"{ arg = 0, param = "data" }"#,
    ),
    ("ecosystem.attribute-type", r#""mypkg.client.Chat""#),
    ("ecosystem.writes-in-order.arg", "0"),
    ("ecosystem.writes-in-order.param", r#""data""#),
    ("ecosystem.writes-in-order.sorted-by", r#""sort_keys""#),
    ("ecosystem.writes-in-order.sorted-by-default", "true"),
];

/// A line that sets the key `key` of `table` to a value it takes, and what that value is (`a
/// string`): `select = ["EFF", "MUT101"]`, or the header of a table key (`[tool.heki.lint]`).
fn example(layout: ConfigLayout, table: &str, key: &str) -> Option<(String, &'static str)> {
    // A defaults table takes the values of its contract's keys.
    let table = table.strip_suffix(SUFFIX).unwrap_or(table);
    let path = child(table, key);
    if table.is_empty() && self::table(&path).is_some() {
        let kind = if HekiConfig::CONTRACT_KEYS.contains(&key) || key == "ecosystem" {
            "an array of tables"
        } else {
            "a table"
        };
        return Some((table_name(layout, key), kind));
    }
    let value = EXAMPLES
        .iter()
        .find(|(k, _)| *k == path)
        .or_else(|| EXAMPLES.iter().find(|(k, _)| *k == key))
        .or_else(|| EXAMPLES.iter().find(|(k, _)| *k == format!("{table}.*")))
        .map(|(_, v)| *v)?;
    let parsed: toml::Table = toml::from_str(&format!("v = {value}")).ok()?;
    let kind = match parsed.get("v")? {
        toml::Value::String(_) => "a string",
        toml::Value::Integer(_) => "an integer",
        toml::Value::Boolean(_) => "`true` or `false`",
        toml::Value::Array(_) => "an array",
        _ => "a table",
    };
    Some((format!("{key} = {value}"), kind))
}

/// The settings of the configuration `doc` (the text of `path`) in tables heki knows: no
/// `[tool.heki]` outside a `pyproject.toml`, no setting at the top level of one, and no key a
/// table does not have.
pub(crate) fn check_keys(
    layout: ConfigLayout,
    path: &Path,
    text: &str,
    doc: &toml_edit::Table,
) -> Result<(), ConfigError> {
    let at = |span: Option<Range<usize>>| span.map(|s| crate::problem::line_column(text, s.start));
    match layout {
        ConfigLayout::TopLevel => {
            if let Some((key, _)) = doc.get_key_value("tool") {
                return Err(ConfigError::ToolTable {
                    path: path.to_path_buf(),
                    position: at(key.span()),
                });
            }
        }
        ConfigLayout::PyProject => {
            if let Some((key, _)) = doc
                .iter()
                .find(|(k, _)| HekiConfig::KEYS.contains(k))
                .and_then(|(k, _)| doc.get_key_value(k))
            {
                return Err(ConfigError::TopLevelSetting {
                    path: path.to_path_buf(),
                    key: key.get().to_owned(),
                    position: at(key.span()),
                });
            }
        }
    }
    let Some(root) = settings_root(doc, layout) else {
        return Ok(());
    };
    check_required_version(layout, path, text, root)?;
    match unknown_key("", root) {
        Some(unknown) => Err(ConfigError::UnknownKey {
            path: path.to_path_buf(),
            layout,
            position: at(unknown.span),
            table: unknown.table,
            key: unknown.key,
            keys_inside: unknown.keys_inside,
        }),
        None => Ok(()),
    }
}

/// The release `required-version` asks for, checked before any other key: an older heki cannot
/// know the keys a newer one reads, so what it says about them would be a guess.
fn check_required_version(
    layout: ConfigLayout,
    path: &Path,
    text: &str,
    root: &dyn TableLike,
) -> Result<(), ConfigError> {
    let Some((key, item)) = root.get_key_value("required-version") else {
        return Ok(());
    };
    let Some(required) = item.as_str() else {
        return Ok(());
    };
    let position = item
        .span()
        .map(|s| crate::problem::line_column(text, s.start));
    match crate::version_satisfies(required, crate::VERSION) {
        Some(true) => Ok(()),
        Some(false) => Err(ConfigError::RequiredVersion {
            path: path.to_path_buf(),
            position,
            required: required.to_owned(),
            running: crate::VERSION,
        }),
        None => Err(ConfigError::Value {
            path: path.to_path_buf(),
            layout,
            position,
            key: Some((String::new(), key.get().to_owned())),
            message: format!(
                "`{required}` is not accepted: each comma-separated clause is an operator (`==`, `!=`, `>=`, `>`, `<=`, `<`, `~=`, `===`) and a release of digits and dots, such as `>=0.2.4, <0.3`; pre-, post-, and development-release suffixes are not accepted"
            ),
        }),
    }
}

struct UnknownKey {
    table: String,
    key: String,
    span: Option<Range<usize>>,
    /// The keys of the unknown key's own table, when it is one.
    keys_inside: Vec<String>,
}

/// The first key of `t` (the table at `path`) or of a table below it that its table does not
/// have.
fn unknown_key(path: &str, t: &dyn TableLike) -> Option<UnknownKey> {
    let schema = table(path)?;
    let keys: Vec<&str> = t.iter().map(|(k, _)| k).collect();
    for k in keys {
        let (key, item) = t.get_key_value(k)?;
        if schema
            .keys
            .as_ref()
            .is_some_and(|known| !known.iter().any(|known| known == k))
        {
            return Some(UnknownKey {
                table: path.to_owned(),
                key: k.to_owned(),
                span: key.span(),
                keys_inside: table_items(item)
                    .iter()
                    .flat_map(|inner| inner.iter().map(|(k, _)| k.to_owned()))
                    .collect(),
            });
        }
        let below = child(path, k);
        if let Some(found) = table_items(item)
            .into_iter()
            .find_map(|inner| unknown_key(&below, inner))
        {
            return Some(found);
        }
    }
    None
}

/// The table and key whose value holds byte `offset` of the configuration, the innermost one.
pub(crate) fn key_at(path: &str, t: &dyn TableLike, offset: usize) -> Option<(String, String)> {
    let keys: Vec<&str> = t.iter().map(|(k, _)| k).collect();
    for k in keys {
        let item = t.get(k)?;
        let below = child(path, k);
        if let Some(found) = table_items(item)
            .into_iter()
            .find_map(|inner| key_at(&below, inner, offset))
        {
            return Some(found);
        }
        if item.span().is_some_and(|s| s.contains(&offset)) {
            return Some((path.to_owned(), k.to_owned()));
        }
    }
    None
}

/// The tables a release renamed, by their old path: the old name is unknown, with no alias,
/// and the error names the new one.
const RENAMED_TABLES: &[(&str, &str)] = &[("import-owners", "import-boundaries")];

/// `problem` about the key `key` that `table` does not have, with what to do: the new name of a
/// renamed table, the table the key belongs in (and the keys its own table takes), the tables of
/// a table it should not have opened, or the keys its table has.
pub(crate) fn explain_unknown_key(
    problem: Problem,
    layout: ConfigLayout,
    table: &str,
    key: &str,
    keys_inside: &[String],
) -> Problem {
    let path = child(table, key);
    if let Some((old, new)) = RENAMED_TABLES.iter().find(|(old, _)| *old == path) {
        let header = table_name(layout, new);
        return problem
            .help(format!(
                "`{}` is now `{header}`: rename the table, its keys are the same",
                header.replacen(new, old, 1)
            ))
            .docs(Docs::of_table(new));
    }
    if let Some(kind) = table.strip_suffix(SUFFIX)
        && NOT_DEFAULTED.contains(&key)
    {
        return problem
            .help(format!(
                "`{key}` names one contract and cannot be defaulted: set it in each table of {}",
                table_name(layout, kind)
            ))
            .docs(Docs::of_table(kind));
    }
    let known = self::table(table)
        .and_then(|t| t.keys.as_deref())
        .unwrap_or_default();
    if let Some(inside) = keys_inside.iter().find(|k| known.contains(k)) {
        let header = table_name(layout, &child(table, inside));
        return problem
            .help(format!(
                "heki has no `{key}` table: its keys belong in {} itself (`{header}`)",
                table_name(layout, table)
            ))
            .docs(Docs::of_key(table, inside));
    }
    let homes: Vec<&Table> = tables()
        .iter()
        .filter(|t| t.path != table && !t.path.ends_with(SUFFIX))
        .filter(|t| {
            t.keys
                .as_ref()
                .is_some_and(|keys| keys.iter().any(|k| k == key))
        })
        .collect();
    if let Some(home) = homes.first() {
        let names: Vec<String> = homes.iter().map(|t| table_name(layout, t.path)).collect();
        let mut problem = problem.help(format!(
            "`{key}` is a key of {}: move it there",
            names.join(", ")
        ));
        if let Some((line, _)) = example(layout, home.path, key) {
            problem = problem.help(format!("for example: `{line}`"));
        }
        if !table.is_empty() && !known.is_empty() {
            let keys: Vec<String> = known.iter().map(|k| format!("`{k}`")).collect();
            problem = problem.help(format!(
                "{} takes {}",
                table_name(layout, table),
                keys.join(", ")
            ));
        }
        return problem.docs(Docs::of_key(home.path, key));
    }
    let mut problem = problem.one_of(key, known).docs(Docs::of_table(table));
    if crate::problem::nearest(key, known.iter().map(String::as_str)).is_empty() {
        problem = problem.help(format!(
            "`{key}` may be a key of a newer heki than {}: `required-version` makes an older heki refuse to run, and the changelog lists what each release adds ({})",
            crate::VERSION,
            crate::problem::CHANGELOG
        ));
    }
    problem
}

/// What the value of `key` in `table` takes, with an example.
pub(crate) fn value_help(layout: ConfigLayout, table: &str, key: &str) -> Option<String> {
    let (line, kind) = example(layout, table, key)?;
    Some(format!("`{key}` takes {kind}, for example: `{line}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key of every table has an example, and each example is a value the key takes.
    #[test]
    fn every_key_has_an_example_its_table_reads() {
        for t in tables() {
            for key in t.keys.iter().flatten() {
                let (line, _) = example(ConfigLayout::TopLevel, t.path, key)
                    .unwrap_or_else(|| panic!("no example of `{key}` in `{}`", t.path));
                let text = match t.path {
                    "" => format!("{line}\n"),
                    "ecosystem.writes-in-order" => {
                        format!("[[ecosystem]]\n[ecosystem.writes-in-order]\n{line}\n")
                    }
                    path if path == "lint" || path == "imports" || path.ends_with(SUFFIX) => {
                        format!("[{path}]\n{line}\n")
                    }
                    contract => format!("[[{contract}]]\n{line}\n"),
                };
                // Read, not folded: the example of `include` names a file that is not there.
                let parsed =
                    ConfigLayout::TopLevel.read(Path::new("heki.toml"), &text, Path::new(""));
                assert!(parsed.is_ok(), "{text}: {parsed:?}");
            }
        }
    }
}
