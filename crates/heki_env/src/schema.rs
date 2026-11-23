//! The tables of the configuration with the keys each accepts, derived from the types that read
//! them, and the checks that find a misplaced or misspelled key, and the key of a value of the
//! wrong type, so that the error can name the table the key belongs in and show a value it
//! takes.

use crate::problem::{settings_root, table_items, table_name};
use crate::{
    CallOwnerRule, ConfigError, ConfigLayout, EcosystemOverride, EffectBoundaryRule, HekiConfig,
    ImportOwnerRule, InvalidationRule, LintConfig, OrderedOutputSetting, ReadOnlyReturnRule,
    ReadOwnerRule, WriteOwnerRule,
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
            table("import-owners", keys_of::<ImportOwnerRule>()),
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
    ("exclude", r#"[".venv", "build"]"#),
    ("extend-exclude", r#"["generated"]"#),
    ("src", r#"["src"]"#),
    ("baseline", r#""heki-baseline.toml""#),
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
    ("lint.report-unused-suppressions", "true"),
    ("lint.require-suppression-codes", "true"),
    ("name", r#""generation-store""#),
    ("targets", r#"["pkg.model.DataModel.fields"]"#),
    ("message", r#""use GenerationStore.set_fields() instead""#),
    ("exempt-methods", r#"["__deepcopy__"]"#),
    ("fresh-objects", r#""allow""#),
    ("unresolved", r#""report""#),
    ("attribution", r#""lexical""#),
    ("own-receiver", r#""allow""#),
    ("allowed-protocols", r#"["pkg.protocols.Writable"]"#),
    ("protocol-subclasses", "false"),
    ("references", r#""calls""#),
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
    ("import-owners.importers", r#"["pkg.model.*"]"#),
    ("import-owners.exempt-importers", r#"["pkg.model.testing"]"#),
    (
        "import-owners.allowed-imports",
        r#"["<stdlib>", "pkg.model.*"]"#,
    ),
    ("import-owners.forbidden-imports", r#"["pkg.backends.*"]"#),
    (
        "import-owners.ignored-imports",
        r#"["pkg.model.legacy -> pkg.backends.sql"]"#,
    ),
    ("import-owners.type-checking", r#""report""#),
    ("import-owners.reach", r#""import-time""#),
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
    let path = child(table, key);
    if table.is_empty() && self::table(&path).is_some() {
        let kind = if key == "lint" {
            "a table"
        } else {
            "an array of tables"
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

/// `problem` about the key `key` that `table` does not have, with what to do: the table the key
/// belongs in, the tables of a table it should not have opened, or the keys its table has.
pub(crate) fn explain_unknown_key(
    problem: Problem,
    layout: ConfigLayout,
    table: &str,
    key: &str,
    keys_inside: &[String],
) -> Problem {
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
        .filter(|t| t.path != table)
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
        return problem.docs(Docs::of_key(home.path, key));
    }
    problem.one_of(key, known).docs(Docs::of_table(table))
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
                    "lint" => format!("[lint]\n{line}\n"),
                    contract => format!("[[{contract}]]\n{line}\n"),
                };
                let parsed = ConfigLayout::TopLevel.parse(Path::new("heki.toml"), &text);
                assert!(parsed.is_ok(), "{text}: {parsed:?}");
            }
        }
    }
}
