//! The fold that makes one configuration of the configuration files: the files `include` lists
//! join the main file, each `@name` entry is replaced by the entries of the group `name`, and
//! each `<kind>-defaults` table is written into the tables of its kind that leave its keys out.
//! The result is the text of one file holding every table written out, so every reader (a check,
//! `heki inspect`, `heki recommend`, the language server) sees the contracts, in their order, that
//! the one file would give, and a baseline keys its reports as that file would.

use crate::defaults::SUFFIX;
use crate::problem::{line_column, settings_root, table_items};
use crate::{ConfigError, ConfigLayout, Docs, HekiConfig, Problem};
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;
use toml_edit::TableLike;

/// The key of the main configuration file that lists the files it includes.
pub(crate) const INCLUDE: &str = "include";

/// The table of the named groups.
pub(crate) const GROUPS: &str = "groups";

/// Where a contract entry is written when no contract table of the main configuration file holds
/// its text: in a table of an included file, or in a group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct EntryPlace {
    /// The file, relative to the project root when it is below it.
    pub file: String,
    /// The line of the entry (from 1).
    pub line: u32,
    /// The group that holds the entry, when it comes from one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

impl fmt::Display for EntryPlace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(group) = &self.group {
            write!(f, "group `{group}` at ")?;
        }
        write!(f, "{}:{}", self.file, self.line)
    }
}

/// A group of `[groups]` that no contract uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnusedGroup {
    pub name: String,
    /// Where it is defined, as [`EntryPlace::file`] and [`EntryPlace::line`] name a place.
    pub file: String,
    pub line: u32,
}

/// What the fold read besides the main configuration file, and where the entries it wrote into
/// the contracts come from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigSources {
    /// The files `include` lists, in their order.
    pub included: Vec<PathBuf>,
    /// The groups no contract uses, in the order they are defined.
    pub unused_groups: Vec<UnusedGroup>,
    /// By contract kind, position of the table among those of its kind, and key: the place of
    /// each entry, `None` for an entry a contract table of the main file holds.
    places: BTreeMap<(String, usize, String), Vec<Option<EntryPlace>>>,
}

impl ConfigSources {
    /// Where the entry at `position` of the key `key` of the contract at `table` (in
    /// configuration order) of the kind `kind` is written, when it is not in a contract table
    /// of the main file.
    pub fn place(
        &self,
        kind: &str,
        table: usize,
        key: &str,
        position: usize,
    ) -> Option<&EntryPlace> {
        self.places
            .get(&(kind.to_owned(), table, key.to_owned()))?
            .get(position)?
            .as_ref()
    }
}

/// Why the files or groups of a configuration do not fold into one.
#[derive(Debug, Error)]
pub enum FoldProblem {
    #[error("`@{0}` names no group")]
    UnknownGroup(String),
    #[error("a cycle between groups: {}", chain.iter().map(|g| format!("`@{g}`")).collect::<Vec<_>>().join(" -> "))]
    GroupCycle { chain: Vec<String> },
    #[error("the group `{0}` is empty")]
    EmptyGroup(String),
    #[error("the entry `{0}` has `@` after its start")]
    GroupInsideEntry(String),
    #[error("the group `{name}` is defined twice; it is first defined at {first}")]
    GroupTwice { name: String, first: String },
    #[error("`include` lists `{0}`, the main configuration file")]
    IncludesMain(String),
    #[error("`include` in an included file")]
    NestedInclude,
    #[error("cannot read the included file `{entry}`: {cause}")]
    Unreadable { entry: String, cause: String },
    #[error("`include` lists `{0}` twice")]
    IncludedTwice(String),
    #[error("`include` entry `{0}` is a glob")]
    Glob(String),
    #[error("`{0}` in an included file")]
    MainSetting(String),
}

impl FoldProblem {
    pub(crate) fn explain(&self, problem: Problem) -> Problem {
        let (help, docs) = match self {
            Self::UnknownGroup(_) => (
                "define the group under `[groups]` in the main configuration file or an included one, or remove the entry".to_owned(),
                Docs::Groups,
            ),
            Self::GroupCycle { .. } => (
                "a group stands for its entries written out, so it cannot hold itself: remove one `@` entry of the cycle".to_owned(),
                Docs::Groups,
            ),
            Self::EmptyGroup(_) => (
                "an empty group would empty every key that uses it, and an empty `forbidden-*` key means every function: list its entries, or remove the group and its uses".to_owned(),
                Docs::Groups,
            ),
            Self::GroupInsideEntry(_) => (
                "a group is named by a whole entry, `\"@name\"`: list it as an entry of its own".to_owned(),
                Docs::Groups,
            ),
            Self::GroupTwice { .. } => (
                "group names are shared by the main configuration file and the files it includes: rename one of the two".to_owned(),
                Docs::Groups,
            ),
            Self::IncludesMain(_) | Self::IncludedTwice(_) => (
                "list each further file once, and not the file that holds `include`".to_owned(),
                Docs::Include,
            ),
            Self::NestedInclude => (
                "an included file includes nothing: list the file in the `include` of the main configuration file".to_owned(),
                Docs::Include,
            ),
            Self::Unreadable { .. } => (
                "a path in `include` is relative to the file that holds it: fix the path, or add the file".to_owned(),
                Docs::Include,
            ),
            Self::Glob(_) => (
                "globs are not accepted: list the files, so that adding one is an edit of this file".to_owned(),
                Docs::Include,
            ),
            Self::MainSetting(key) => (
                format!(
                    "an included file holds contract tables, their `<kind>-defaults` tables, and `[groups]`: move `{key}` into the main configuration file"
                ),
                Docs::Include,
            ),
        };
        problem.help(help).docs(docs)
    }
}

/// A configuration file of the fold: the main file, then the files its `include` lists.
pub(crate) struct File {
    /// The path as it was read, for errors.
    pub(crate) path: PathBuf,
    pub(crate) text: String,
    pub(crate) layout: ConfigLayout,
}

impl File {
    fn error(&self, offset: Option<usize>, problem: FoldProblem) -> ConfigError {
        ConfigError::Fold {
            path: self.path.clone(),
            position: offset.map(|o| line_column(&self.text, o)),
            problem,
        }
    }
}

/// The file `entry` of `include` names, from the directory of `main`, the file that holds it.
fn included_path(main: &Path, entry: &str) -> PathBuf {
    let joined = main.parent().unwrap_or(Path::new("")).join(entry);
    joined
        .components()
        .filter(|c| *c != Component::CurDir)
        .collect()
}

/// The entries of `include` in the configuration `text` of `layout`, with their offsets; none
/// when the text is not valid TOML.
fn include_entries(text: &str, layout: ConfigLayout) -> Vec<(String, Option<usize>)> {
    let doc = toml_edit::Document::parse(text).ok();
    let root = doc
        .as_ref()
        .and_then(|d| settings_root(d.as_table(), layout));
    let listed = root.and_then(|r| entries(0, r.get(INCLUDE)?));
    (listed.into_iter().flatten())
        .map(|e| (e.text, e.offset))
        .collect()
}

/// The files the `include` of the configuration file `path` lists, resolved as the loader
/// resolves them; none when the file cannot be read or is not valid TOML. The language server
/// watches them, also while the configuration does not load.
pub fn included_files(path: &Path) -> Vec<PathBuf> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    (include_entries(&text, ConfigLayout::of(path)).into_iter())
        .map(|(entry, _)| included_path(path, &entry))
        .collect()
}

/// The main configuration file at `path`, of `layout` and with `text`, and the files its
/// `include` lists, each checked to hold only what an included file may hold.
pub(crate) fn read_files(
    layout: ConfigLayout,
    path: &Path,
    text: &str,
    root: &Path,
) -> Result<Vec<File>, ConfigError> {
    let main = File {
        path: path.to_path_buf(),
        text: text.to_owned(),
        layout,
    };
    let main_identity = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut included = Vec::new();
    for (entry, at) in include_entries(text, layout) {
        if entry.contains(['*', '?', '[', ']', '{', '}']) {
            return Err(main.error(at, FoldProblem::Glob(entry)));
        }
        let file = included_path(path, &entry);
        let read = std::fs::canonicalize(&file)
            .and_then(|identity| Ok((identity, std::fs::read_to_string(&file)?)));
        let (identity, text) = read.map_err(|cause| {
            main.error(
                at,
                FoldProblem::Unreadable {
                    entry: entry.clone(),
                    cause: cause.to_string(),
                },
            )
        })?;
        if identity == main_identity {
            return Err(main.error(at, FoldProblem::IncludesMain(entry)));
        }
        if seen.contains(&identity) {
            return Err(main.error(at, FoldProblem::IncludedTwice(entry)));
        }
        seen.push(identity);
        let file = File {
            path: file,
            text,
            layout: ConfigLayout::TopLevel,
        };
        check_included(&file, root)?;
        included.push(file);
    }
    Ok(std::iter::once(main).chain(included).collect())
}

/// Whether `key` is one an included file may hold: a contract kind, its defaults table, or
/// `groups`.
fn included_file_key(key: &str) -> bool {
    let kind = key.strip_suffix(SUFFIX).unwrap_or(key);
    key == GROUPS || HekiConfig::CONTRACT_KEYS.contains(&kind)
}

/// An included file holds contract tables, their defaults tables, and groups, each of which
/// reads as in the main file; every other setting belongs to the main file.
fn check_included(file: &File, root: &Path) -> Result<(), ConfigError> {
    if let Ok(doc) = toml_edit::Document::parse(file.text.as_str()) {
        let setting = (doc.as_table().iter())
            .map(|(key, _)| key)
            .find(|key| HekiConfig::KEYS.contains(key) && !included_file_key(key));
        if let Some((key, _)) = setting.and_then(|key| doc.as_table().get_key_value(key)) {
            let at = key.span().map(|s| s.start);
            let problem = match key.get() {
                INCLUDE => FoldProblem::NestedInclude,
                key => FoldProblem::MainSetting(key.to_owned()),
            };
            return Err(file.error(at, problem));
        }
    }
    ConfigLayout::TopLevel.read(&file.path, &file.text, root)?;
    Ok(())
}

/// A list entry of a contract, a defaults table, or a group, and where it is written.
#[derive(Debug, Clone)]
struct Entry {
    text: String,
    /// The file ([`File`], by its position) and the byte offset of the entry in it.
    file: usize,
    offset: Option<usize>,
    /// The group whose list holds the entry.
    group: Option<String>,
}

/// What an entry stands for.
enum Meaning<'e> {
    /// Itself.
    Text,
    /// The entries of the group of this name.
    Group(&'e str),
}

impl Entry {
    fn meaning(&self) -> Result<Meaning<'_>, FoldProblem> {
        match self.text.find('@') {
            None => Ok(Meaning::Text),
            Some(0) if !self.text[1..].contains('@') => Ok(Meaning::Group(&self.text[1..])),
            Some(_) => Err(FoldProblem::GroupInsideEntry(self.text.clone())),
        }
    }
}

/// A key of a contract or defaults table: a list of strings, or any other value as written.
#[derive(Debug, Clone)]
enum Field {
    List(Vec<Entry>),
    Value(toml::Value),
}

type Table = BTreeMap<String, Field>;

/// A defaults table with its lists written out, each key with the groups it uses.
type Defaults = BTreeMap<String, (Field, BTreeSet<usize>)>;

/// A group of `[groups]`.
struct Group {
    name: String,
    file: usize,
    offset: Option<usize>,
    entries: Vec<Entry>,
}

/// The contract tables, defaults tables, and groups of one file.
struct Settings {
    /// By kind, in [`HekiConfig::CONTRACT_KEYS`] order.
    tables: Vec<Vec<Table>>,
    defaults: Vec<Option<Table>>,
    groups: Vec<Group>,
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

/// The entries of `item`, of the file at `file`, with their places, when it is an array of
/// strings.
fn entries(file: usize, item: &toml_edit::Item) -> Option<Vec<Entry>> {
    (item.as_array()?.iter())
        .map(|v| {
            Some(Entry {
                text: v.as_str()?.to_owned(),
                file,
                offset: v.span().map(|s| s.start),
                group: None,
            })
        })
        .collect()
}

/// The keys of the contract or defaults table `value`, of the file at `file`, with the places
/// of their entries in `edit`, the same table as the file writes it.
fn table_of(file: usize, value: &toml::Table, edit: Option<&dyn TableLike>) -> Table {
    value
        .iter()
        .map(|(key, v)| {
            let listed = edit.and_then(|t| entries(file, t.get(key)?));
            let field = listed.map_or_else(|| Field::Value(v.clone()), Field::List);
            (key.clone(), field)
        })
        .collect()
}

fn settings_of(index: usize, file: &File) -> Settings {
    let mut value: toml::Table = toml::from_str(&file.text).expect("a configuration that was read");
    let doc =
        toml_edit::Document::parse(file.text.as_str()).expect("a configuration that was read");
    let edit = settings_root(doc.as_table(), file.layout);
    let value = settings_mut(file.layout, &mut value).map(|v| &*v);
    let kinds = HekiConfig::CONTRACT_KEYS;
    let tables = kinds
        .iter()
        .map(|kind| {
            let values = value.and_then(|v| v.get(*kind)?.as_array());
            let edits = edit
                .and_then(|e| e.get(kind))
                .map(table_items)
                .unwrap_or_default();
            (values.into_iter().flatten())
                .filter_map(toml::Value::as_table)
                .enumerate()
                .map(|(i, t)| table_of(index, t, edits.get(i).copied()))
                .collect()
        })
        .collect();
    let defaults = kinds
        .iter()
        .map(|kind| {
            let key = format!("{kind}{SUFFIX}");
            let t = value?.get(&key)?.as_table()?;
            Some(table_of(
                index,
                t,
                edit.and_then(|e| e.get(&key)?.as_table_like()),
            ))
        })
        .collect();
    let groups = edit.and_then(|e| e.get(GROUPS)?.as_table_like());
    let groups = (groups.into_iter().flat_map(|t| {
        t.iter().filter_map(|(name, item)| {
            let offset = t.get_key_value(name)?.0.span().map(|s| s.start);
            Some((name, offset, entries(index, item)?))
        })
    }))
    .map(|(name, offset, listed)| Group {
        name: name.to_owned(),
        file: index,
        offset,
        entries: (listed.into_iter())
            .map(|e| Entry {
                group: Some(name.to_owned()),
                ..e
            })
            .collect(),
    })
    .collect();
    Settings {
        tables,
        defaults,
        groups,
    }
}

/// The groups of every file, each with its entries written out.
struct Groups<'s> {
    defs: Vec<&'s Group>,
    by_name: HashMap<&'s str, usize>,
    /// By group: its entries written out, and the groups it reaches (itself included).
    expanded: Vec<Option<(Vec<Entry>, BTreeSet<usize>)>>,
}

impl<'s> Groups<'s> {
    /// The groups of `settings`, of `files`, each named as `shown` names its file.
    fn new(
        files: &[File],
        shown: &[String],
        settings: &'s [Settings],
    ) -> Result<Self, ConfigError> {
        let mut defs: Vec<&Group> = Vec::new();
        let mut by_name = HashMap::new();
        for group in settings.iter().flat_map(|s| &s.groups) {
            let file = &files[group.file];
            if let Some(&first) = by_name.get(group.name.as_str()) {
                let first: &Group = defs[first];
                let line = first
                    .offset
                    .map_or(1, |o| line_column(&files[first.file].text, o).0);
                let first = format!("{}:{line}", shown[first.file]);
                let problem = FoldProblem::GroupTwice {
                    name: group.name.clone(),
                    first,
                };
                return Err(file.error(group.offset, problem));
            }
            if group.entries.is_empty() {
                return Err(file.error(group.offset, FoldProblem::EmptyGroup(group.name.clone())));
            }
            by_name.insert(group.name.as_str(), defs.len());
            defs.push(group);
        }
        let mut groups = Self {
            expanded: vec![None; defs.len()],
            defs,
            by_name,
        };
        for g in 0..groups.defs.len() {
            groups.resolve(files, g, &mut Vec::new())?;
        }
        Ok(groups)
    }

    /// Write out the entries of the group `g`; `chain` holds the groups whose entries are being
    /// written out, the outermost first.
    fn resolve(
        &mut self,
        files: &[File],
        g: usize,
        chain: &mut Vec<usize>,
    ) -> Result<(), ConfigError> {
        if self.expanded[g].is_some() {
            return Ok(());
        }
        chain.push(g);
        let mut out = Vec::new();
        let mut reached = BTreeSet::from([g]);
        for entry in &self.defs[g].entries {
            let at = |problem| files[entry.file].error(entry.offset, problem);
            match entry.meaning().map_err(at)? {
                Meaning::Text => out.push(entry.clone()),
                Meaning::Group(name) => {
                    let inner = self.index(name).map_err(at)?;
                    if let Some(start) = chain.iter().position(|c| *c == inner) {
                        let chain = (chain[start..].iter().chain([&inner]))
                            .map(|c| self.defs[*c].name.clone())
                            .collect();
                        return Err(at(FoldProblem::GroupCycle { chain }));
                    }
                    self.resolve(files, inner, chain)?;
                    let (entries, reaches) = self.expanded[inner].as_ref().expect("resolved above");
                    out.extend(entries.iter().cloned());
                    reached.extend(reaches.iter().copied());
                }
            }
        }
        chain.pop();
        self.expanded[g] = Some((out, reached));
        Ok(())
    }

    fn index(&self, name: &str) -> Result<usize, FoldProblem> {
        (self.by_name.get(name).copied()).ok_or_else(|| FoldProblem::UnknownGroup(name.to_owned()))
    }

    /// `entries` with each `@name` written out, and the groups that took part.
    fn expand(
        &self,
        files: &[File],
        entries: &[Entry],
    ) -> Result<(Vec<Entry>, BTreeSet<usize>), ConfigError> {
        let mut out = Vec::new();
        let mut used = BTreeSet::new();
        for entry in entries {
            let at = |problem| files[entry.file].error(entry.offset, problem);
            match entry.meaning().map_err(at)? {
                Meaning::Text => out.push(entry.clone()),
                Meaning::Group(name) => {
                    let g = self.index(name).map_err(at)?;
                    let (entries, reaches) =
                        self.expanded[g].as_ref().expect("every group is resolved");
                    out.extend(entries.iter().cloned());
                    used.extend(reaches.iter().copied());
                }
            }
        }
        Ok((out, used))
    }

    /// `field` with its list written out; the groups it uses join `used`.
    fn expand_field(
        &self,
        files: &[File],
        field: &Field,
        used: &mut BTreeSet<usize>,
    ) -> Result<Field, ConfigError> {
        Ok(match field {
            Field::List(entries) => {
                let (entries, groups) = self.expand(files, entries)?;
                used.extend(groups);
                Field::List(entries)
            }
            Field::Value(v) => Field::Value(v.clone()),
        })
    }
}

/// The configuration the files give, as the text of one file.
pub(crate) struct Folded {
    /// `None` when the main file is that text already: no file has a group, a defaults table,
    /// or an `include`.
    pub(crate) text: Option<String>,
    pub(crate) sources: ConfigSources,
}

/// The files of a configuration (the main file first, [`read_files`]) folded into one; `root`
/// is the project root that [`EntryPlace::file`] names files relative to.
pub(crate) fn fold(files: &[File], root: &Path) -> Result<Folded, ConfigError> {
    let settings: Vec<Settings> = files
        .iter()
        .enumerate()
        .map(|(i, f)| settings_of(i, f))
        .collect();
    let shown: Vec<String> = files.iter().map(|f| shown_path(&f.path, root)).collect();
    let groups = Groups::new(files, &shown, &settings)?;
    let kinds = HekiConfig::CONTRACT_KEYS;
    let mut used: BTreeSet<usize> = BTreeSet::new();
    // By file and kind: the defaults table with its lists written out, each key with the groups
    // it uses, which a table that takes the key uses.
    let mut defaults: Vec<Vec<Option<Defaults>>> = Vec::new();
    for s in &settings {
        let of_file = (s.defaults.iter().map(|table| {
            let Some(table) = table else { return Ok(None) };
            let keys = table.iter().map(|(key, field)| {
                let mut used = BTreeSet::new();
                let field = groups.expand_field(files, field, &mut used)?;
                Ok((key.clone(), (field, used)))
            });
            keys.collect::<Result<Defaults, ConfigError>>().map(Some)
        }))
        .collect::<Result<_, _>>()?;
        defaults.push(of_file);
    }
    let mut merged: Vec<Vec<Table>> = vec![Vec::new(); kinds.len()];
    for (f, s) in settings.iter().enumerate() {
        for (k, tables) in s.tables.iter().enumerate() {
            for table in tables {
                let mut expanded = Table::new();
                for (key, field) in table {
                    expanded.insert(key.clone(), groups.expand_field(files, field, &mut used)?);
                }
                // A table's own key, else its file's default, else the main file's.
                for keys in [&defaults[f][k], &defaults[0][k]].into_iter().flatten() {
                    for (key, (field, groups_of_key)) in keys {
                        if !expanded.contains_key(key) {
                            expanded.insert(key.clone(), field.clone());
                            used.extend(groups_of_key.iter().copied());
                        }
                    }
                }
                let table = expanded;
                merged[k].push(table);
            }
        }
    }
    let mut sources = ConfigSources {
        included: files[1..].iter().map(|f| f.path.clone()).collect(),
        unused_groups: (groups.defs.iter().enumerate())
            .filter(|(g, _)| !used.contains(g))
            .map(|(_, group)| UnusedGroup {
                name: group.name.clone(),
                file: shown[group.file].clone(),
                line: group
                    .offset
                    .map_or(1, |o| line_column(&files[group.file].text, o).0),
            })
            .collect(),
        places: BTreeMap::new(),
    };
    for (k, tables) in merged.iter().enumerate() {
        for (i, table) in tables.iter().enumerate() {
            for (key, field) in table {
                let Field::List(entries) = field else {
                    continue;
                };
                let places: Vec<Option<EntryPlace>> = entries
                    .iter()
                    .map(|e| {
                        (e.file != 0 || e.group.is_some()).then(|| EntryPlace {
                            file: shown[e.file].clone(),
                            line: e
                                .offset
                                .map_or(1, |o| line_column(&files[e.file].text, o).0),
                            group: e.group.clone(),
                        })
                    })
                    .collect();
                if places.iter().any(Option::is_some) {
                    sources
                        .places
                        .insert((kinds[k].to_owned(), i, key.clone()), places);
                }
            }
        }
    }
    let folds = files.len() > 1
        || settings
            .iter()
            .any(|s| !s.groups.is_empty() || s.defaults.iter().any(Option::is_some));
    let text = folds.then(|| written_out(&files[0], &merged));
    Ok(Folded { text, sources })
}

/// The main file with its contract tables replaced by `merged`, and its groups and defaults
/// tables removed. Its settings table is there: a group, a defaults table, or an `include`
/// is in it, or in a file its `include` lists.
fn written_out(main: &File, merged: &[Vec<Table>]) -> String {
    let mut doc: toml::Table = toml::from_str(&main.text).expect("a configuration that was read");
    let settings = settings_mut(main.layout, &mut doc).expect("a configuration with settings");
    settings.remove(GROUPS);
    for (kind, tables) in HekiConfig::CONTRACT_KEYS.iter().zip(merged) {
        settings.remove(&format!("{kind}{SUFFIX}"));
        settings.remove(*kind);
        if tables.is_empty() {
            continue;
        }
        let tables = tables
            .iter()
            .map(|t| toml::Value::Table(value_of(t)))
            .collect();
        settings.insert((*kind).to_owned(), toml::Value::Array(tables));
    }
    toml::to_string(&doc).expect("a configuration table serializes")
}

fn value_of(table: &Table) -> toml::Table {
    table
        .iter()
        .map(|(key, field)| {
            let value = match field {
                Field::List(entries) => toml::Value::Array(
                    entries
                        .iter()
                        .map(|e| toml::Value::String(e.text.clone()))
                        .collect(),
                ),
                Field::Value(v) => v.clone(),
            };
            (key.clone(), value)
        })
        .collect()
}

/// `path` relative to `root` in POSIX form, as warnings name a file; as given when it is not
/// below the root.
fn shown_path(path: &Path, root: &Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let rel = resolved.strip_prefix(&root).unwrap_or(path);
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.join("/")
}
