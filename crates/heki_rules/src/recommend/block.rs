//! The configuration block `heki recommend` prints: the keys its advice adds, written from the
//! types the configuration loader reads, for the file the project reads its configuration from.
//!
//! The block only adds. An array the configuration already sets (`lint.extend-select`) is
//! written whole, since TOML cannot append to it, with the additions named in a comment.

use heki_env::{ConfigLayout, EffectBoundaryRule, HekiConfig, ReadOwnerRule, WriteOwnerRule};
use serde::Serialize;
use std::fmt::Write as _;

/// What the advice adds to the configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Additions {
    /// The source roots to set, when the detected ones should be written out with others.
    pub src: Option<Vec<String>>,
    /// Rules to add to `lint.extend-select`.
    pub extend_select: Vec<String>,
    pub write_owners: Vec<WriteOwnerRule>,
    pub read_owners: Vec<ReadOwnerRule>,
    pub effect_boundaries: Vec<EffectBoundaryRule>,
}

/// The block as TOML, and as the keys and values it sets.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub toml: String,
    pub config: serde_json::Map<String, serde_json::Value>,
}

/// The lines of the keys of `entry` whose values differ from the type's default, those a user
/// writes, in the order of the type's fields.
fn set_lines<T: Serialize + Default>(entry: &T) -> String {
    let lines = |value: &T| toml::to_string(value).expect("a configuration entry is a table");
    let default = lines(&T::default());
    lines(entry)
        .lines()
        .filter(|line| !default.lines().any(|d| d == *line))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// The keys of `entry` whose values differ from the type's default, as JSON.
pub(super) fn set_keys<T: Serialize + Default>(
    entry: &T,
) -> serde_json::Map<String, serde_json::Value> {
    let object = |value: &T| -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_value(json(value)).expect("a configuration entry is a table")
    };
    let default = object(&T::default());
    object(entry)
        .into_iter()
        .filter(|(key, value)| default.get(key) != Some(value))
        .collect()
}

/// The block that adds `additions` to `current`, the configuration read from a file of `layout`.
pub fn render(additions: &Additions, current: &HekiConfig, layout: ConfigLayout) -> Block {
    let prefix = layout.table_prefix();
    let header = |name: &str| match (prefix, name) {
        (Some(prefix), "") => format!("[{prefix}]"),
        (Some(prefix), name) => format!("[{prefix}.{name}]"),
        (None, name) => format!("[{name}]"),
    };
    let mut toml = String::new();
    let mut config = serde_json::Map::new();
    if let Some(src) = &additions.src {
        if prefix.is_some() {
            let _ = writeln!(toml, "{}", header(""));
        }
        let _ = writeln!(toml, "src = {}", array(src));
        config.insert("src".to_owned(), json(src));
    }
    if !additions.extend_select.is_empty() {
        let mut whole = current.lint.extend_select.clone();
        let added: Vec<&String> = additions
            .extend_select
            .iter()
            .filter(|code| !whole.contains(code))
            .collect();
        whole.extend(added.iter().map(|code| (*code).clone()));
        let names: Vec<&str> = added.iter().map(|code| code.as_str()).collect();
        separate(&mut toml);
        let _ = writeln!(
            toml,
            "{}\nextend-select = {}  # adds {}",
            header("lint"),
            array(&whole),
            names.join(", ")
        );
        let mut lint = serde_json::Map::new();
        lint.insert("extend-select".to_owned(), json(&whole));
        config.insert("lint".to_owned(), lint.into());
    }
    let mut entries = |name: &str, entries: Vec<(String, serde_json::Value)>| {
        if entries.is_empty() {
            return;
        }
        for (lines, _) in &entries {
            separate(&mut toml);
            let _ = write!(toml, "[{}]\n{lines}", header(name));
        }
        let values = entries.into_iter().map(|(_, value)| value).collect();
        config.insert(name.to_owned(), serde_json::Value::Array(values));
    };
    entries("write-owners", written(&additions.write_owners));
    entries("read-owners", written(&additions.read_owners));
    entries("effect-boundaries", written(&additions.effect_boundaries));
    Block { toml, config }
}

/// Each entry of a list of tables as its TOML lines and its JSON object.
fn written<T: Serialize + Default>(entries: &[T]) -> Vec<(String, serde_json::Value)> {
    entries
        .iter()
        .map(|e| (set_lines(e), serde_json::Value::Object(set_keys(e))))
        .collect()
}

/// An array of strings as TOML writes it inline.
fn array(values: &[String]) -> toml::Value {
    toml::Value::try_from(values).expect("strings are TOML values")
}

/// `value` as JSON.
fn json<T: Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).expect("configuration values are JSON values")
}

/// A blank line before the next table.
fn separate(toml: &mut String) {
    if !toml.is_empty() {
        toml.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn additions() -> Additions {
        Additions {
            src: Some(vec!["backend".into(), String::new()]),
            extend_select: vec!["EFF105".into(), "EFF106".into()],
            write_owners: vec![WriteOwnerRule {
                name: "Order.items writers".into(),
                targets: vec!["app.models:Order.items".into()],
                allowed_writers: vec!["app.store:*".into()],
                unresolved: Some("report".into()),
                ..WriteOwnerRule::default()
            }],
            read_owners: vec![ReadOwnerRule {
                name: "Cache._entries readers".into(),
                targets: vec!["app.cache:Cache._entries".into()],
                allowed_readers: vec!["app.cache:*".into()],
                ..ReadOwnerRule::default()
            }],
            effect_boundaries: vec![EffectBoundaryRule {
                name: "app.domain without I/O".into(),
                scopes: vec!["app.domain:*".into(), "app.domain.*:*".into()],
                forbid: vec!["blocking".into()],
                ..EffectBoundaryRule::default()
            }],
        }
    }

    /// The configuration in `text`, read as the file `name` of a directory without other files.
    fn parse(name: &str, text: &str) -> HekiConfig {
        let path = std::env::temp_dir()
            .join("heki-block-parse-back")
            .join(name);
        ConfigLayout::of(&path).parse(&path, text).unwrap()
    }

    /// `text` with `block` merged in as a user would: keys before the first table go first, a
    /// key of a table the file has replaces its line there, and the other tables are appended.
    fn merged(text: &str, block: &str) -> String {
        let mut out = text.to_owned();
        for part in block.trim_end().split("\n\n") {
            let (header, keys) = part.split_once('\n').unwrap_or((part, ""));
            if !header.starts_with('[') {
                out = format!("{part}\n{out}");
            } else if header.starts_with("[[") || !out.contains(&format!("{header}\n")) {
                out.push_str(&format!("\n{part}\n"));
            } else {
                for line in keys.lines() {
                    let key = format!("{} = ", line.split(" = ").next().unwrap());
                    let old = out
                        .lines()
                        .find(|l| l.starts_with(&key))
                        .unwrap()
                        .to_owned();
                    out = out.replace(&old, line);
                }
            }
        }
        out
    }

    #[test]
    fn the_block_parses_back_to_the_additions() {
        for (file, layout, existing, expected_select) in [
            (
                "pyproject.toml",
                ConfigLayout::PyProject,
                "",
                vec!["EFF105", "EFF106"],
            ),
            (
                "heki.toml",
                ConfigLayout::TopLevel,
                "",
                vec!["EFF105", "EFF106"],
            ),
            (
                "pyproject.toml",
                ConfigLayout::PyProject,
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"EFF105\"]\nignore = [\"MUT101\"]\n\n[[tool.heki.write-owners]]\nname = \"old\"\ntargets = [\"a:B.c\"]\n",
                vec!["EFF105", "EFF106"],
            ),
            (
                "heki.toml",
                ConfigLayout::TopLevel,
                "[lint]\nextend-select = [\"MUT108\"]\n",
                vec!["MUT108", "EFF105", "EFF106"],
            ),
        ] {
            let current = parse(file, existing);
            let additions = additions();
            let block = render(&additions, &current, layout);
            let parsed = parse(file, &merged(existing, &block.toml));
            assert_eq!(
                parsed.lint.extend_select, expected_select,
                "{file}:\n{}",
                block.toml
            );
            assert_eq!(parsed.lint.ignore, current.lint.ignore);
            assert_eq!(parsed.src, additions.src);
            assert_eq!(
                parsed.write_owners[current.write_owners.len()..],
                additions.write_owners
            );
            assert_eq!(parsed.read_owners, additions.read_owners);
            assert_eq!(parsed.effect_boundaries, additions.effect_boundaries);
            // The JSON form holds the same keys and values.
            let config = serde_json::Value::Object(block.config.clone());
            assert_eq!(
                config["lint"]["extend-select"],
                serde_json::json!(expected_select)
            );
            assert_eq!(config["src"], serde_json::json!(["backend", ""]));
            assert_eq!(
                config["write-owners"][0],
                serde_json::json!({
                    "name": "Order.items writers",
                    "targets": ["app.models:Order.items"],
                    "allowed-writers": ["app.store:*"],
                    "unresolved": "report",
                })
            );
            assert_eq!(
                config["effect-boundaries"][0]["forbid"],
                serde_json::json!(["blocking"])
            );
        }
    }

    #[test]
    fn the_block_is_written_for_the_file_and_names_what_it_adds() {
        let current = parse(
            "pyproject.toml",
            "[tool.heki.lint]\nextend-select = [\"EFF105\"]\n",
        );
        let only_rules = Additions {
            extend_select: vec!["EFF106".into()],
            ..Additions::default()
        };
        assert_eq!(
            render(&only_rules, &current, ConfigLayout::PyProject).toml,
            "[tool.heki.lint]\nextend-select = [\"EFF105\", \"EFF106\"]  # adds EFF106\n"
        );
        let block = render(&additions(), &HekiConfig::default(), ConfigLayout::TopLevel).toml;
        assert!(
            block.starts_with("src = [\"backend\", \"\"]\n\n[lint]\n")
                && block.contains("\n\n[[write-owners]]\nname = \"Order.items writers\"\n")
                && block.ends_with("forbid = [\"blocking\"]\n"),
            "{block}"
        );
        let nothing = render(&Additions::default(), &current, ConfigLayout::PyProject);
        assert_eq!((nothing.toml.as_str(), nothing.config.len()), ("", 0));
    }
}
