//! The `<contract>-defaults` tables: keys a contract kind's tables leave out take the value of
//! the defaults table, so a configuration of many tables that repeat one value writes it once.
//!
//! A table's own key wins, and a list is the table's whole or the default's whole, never both.
//! `name` and `targets` identify one contract and cannot be defaulted. The defaults are folded
//! into the text before the tables are read, so each reader (a check, `heki inspect`) sees the
//! effective values; a tool that adds a table to the configuration (`heki recommend` trying a
//! candidate) takes them through [`with_defaults`].

use crate::{ConfigLayout, HekiConfig};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The keys that name one contract of a kind.
pub(crate) const NOT_DEFAULTED: [&str; 2] = ["name", "targets"];

/// The suffix of a contract key's defaults table: `call-owners-defaults`.
pub(crate) const SUFFIX: &str = "-defaults";

/// `text`, a configuration that reads as TOML, with each `<contract>-defaults` table removed
/// and its keys written into the tables of its contract that do not set them.
pub(crate) fn expand(layout: ConfigLayout, text: &str) -> String {
    let mut table: toml::Table = toml::from_str(text).expect("a configuration that was read");
    let settings = settings_mut(layout, &mut table).expect("a configuration with defaults tables");
    for kind in HekiConfig::CONTRACT_KEYS {
        fold(settings, kind);
    }
    toml::to_string(&table).expect("a configuration table serializes")
}

/// The table that holds the settings of a file of `layout`.
fn settings_mut(layout: ConfigLayout, table: &mut toml::Table) -> Option<&mut toml::Table> {
    let Some(prefix) = layout.table_prefix() else {
        return Some(table);
    };
    prefix
        .split('.')
        .try_fold(table, |t, part| t.get_mut(part)?.as_table_mut())
}

fn fold(settings: &mut toml::Table, kind: &str) {
    let Some(toml::Value::Table(defaults)) = settings.remove(&format!("{kind}{SUFFIX}")) else {
        return;
    };
    let Some(toml::Value::Array(entries)) = settings.get_mut(kind) else {
        return;
    };
    for entry in entries.iter_mut().filter_map(toml::Value::as_table_mut) {
        for (key, value) in &defaults {
            entry.entry(key).or_insert_with(|| value.clone());
        }
    }
}

/// `entry`, a contract table written after the configuration was read, with the keys it leaves
/// unset (no value, an empty string or list) taken from `defaults`, as the same table written in
/// the file would take them.
pub fn with_defaults<T: Serialize + DeserializeOwned>(entry: &T, defaults: Option<&T>) -> T {
    let mut table = serde_json::to_value(entry).expect("a contract serializes");
    let defaults = serde_json::to_value(defaults).expect("a contract serializes");
    for (key, default) in defaults.as_object().into_iter().flatten() {
        if !NOT_DEFAULTED.contains(&key.as_str()) && is_unset(&table[key]) {
            table[key] = default.clone();
        }
    }
    serde_json::from_value(table).expect("a contract reads back what it wrote")
}

fn is_unset(value: &serde_json::Value) -> bool {
    use serde_json::Value;
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WriteOwnerRule;

    fn rule(f: impl FnOnce(&mut WriteOwnerRule)) -> WriteOwnerRule {
        let mut rule = WriteOwnerRule::default();
        f(&mut rule);
        rule
    }

    #[test]
    fn a_table_added_later_takes_the_keys_it_leaves_unset() {
        let defaults = rule(|r| {
            r.name = "defaults".into();
            r.targets = vec!["Model.fields".into()];
            r.allowed_writers = vec!["a".into()];
            r.message = Some("help".into());
            r.protocol_subclasses = Some(true);
            r.attribution = Some("lexical".into());
        });
        let table = rule(|r| {
            r.name = "own".into();
            r.targets = vec!["Model.items".into()];
            r.allowed_writers = vec!["b".into()];
            r.attribution = Some("call-graph".into());
            r.protocol_subclasses = Some(false);
        });
        // Its own keys stay, a list is not merged, and `name` and `targets` are never taken.
        assert_eq!(
            with_defaults(&table, Some(&defaults)),
            rule(|r| {
                r.name = "own".into();
                r.targets = vec!["Model.items".into()];
                r.allowed_writers = vec!["b".into()];
                r.message = Some("help".into());
                r.attribution = Some("call-graph".into());
                r.protocol_subclasses = Some(false);
            })
        );
        assert_eq!(
            with_defaults(&WriteOwnerRule::default(), Some(&defaults)),
            rule(|r| {
                r.allowed_writers = vec!["a".into()];
                r.message = Some("help".into());
                r.attribution = Some("lexical".into());
                r.protocol_subclasses = Some(true);
            })
        );
        assert_eq!(with_defaults(&table, None), table);
    }
}
