//! The fold that makes one configuration of the configuration files: the files `include` lists
//! join the main file, each `@name` entry is replaced by the entries of the group `name`, and
//! each `<kind>-defaults` table is written into the tables of its kind that leave its keys out.
//! The result is the text of one file holding every table written out, so every reader (a check,
//! `heki inspect`, `heki recommend`, the language server) sees the contracts, in their order, that
//! the one file would give, and a baseline keys its reports as that file would.

use crate::defaults::SUFFIX;
use crate::problem::{LineStarts, line_column, settings_root, tables_at};
use crate::{ConfigError, ConfigLayout, Docs, HekiConfig, Problem, Setting};
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;
use toml_edit::TableLike;

/// The key of the main configuration file that lists the files it includes.
pub(crate) const INCLUDE: &str = "include";

/// The table of the named groups.
pub(crate) const GROUPS: &str = "groups";

/// A place of the configuration: where a file writes a contract table or an entry of one (in a
/// table, or in a group), or the place the table's `source` or `entry-sources` names instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct EntryPlace {
    /// The file, relative to the project root when it is below it.
    pub file: String,
    /// The line (from 1).
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

/// Where a contract table or an entry of one is defined ([`EntryPlace`]), with the file as the
/// configuration was read (a path `source` or `entry-sources` names joined to the project root)
/// and the byte column (1 for a named place), as reports point at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinedAt {
    pub path: PathBuf,
    pub column: u32,
    pub place: EntryPlace,
}

/// Where a contract table and the entries of its lists are defined.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContractPlaces {
    table: DefinedAt,
    /// By key, each entry with its place, in list order.
    entries: Vec<(String, Vec<(String, DefinedAt)>)>,
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
    /// By contract kind and position of the table among those of its kind: where it and its
    /// entries are defined.
    contracts: BTreeMap<(String, usize), ContractPlaces>,
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

    /// Where the contract at `table` (in configuration order) of the kind `kind` is defined.
    pub fn contract(&self, kind: &str, table: usize) -> Option<&DefinedAt> {
        Some(&self.contracts.get(&(kind.to_owned(), table))?.table)
    }

    /// The key of that contract whose list holds the entry `entry`, and where the entry is
    /// defined: its first place, in the keys' order.
    pub fn entry_named(&self, kind: &str, table: usize, entry: &str) -> Option<(&str, &DefinedAt)> {
        let contract = self.contracts.get(&(kind.to_owned(), table))?;
        (contract.entries.iter()).find_map(|(key, entries)| {
            let (_, at) = entries.iter().find(|(text, _)| text == entry)?;
            Some((key.as_str(), at))
        })
    }

    /// Where the entry at `position` of the key `key` of that contract is defined.
    pub fn entry_at(
        &self,
        kind: &str,
        table: usize,
        key: &str,
        position: usize,
    ) -> Option<&DefinedAt> {
        let contract = self.contracts.get(&(kind.to_owned(), table))?;
        let (_, entries) = contract.entries.iter().find(|(k, _)| k == key)?;
        entries.get(position).map(|(_, at)| at)
    }
}

/// Why the files or groups of a configuration do not fold into one.
#[derive(Debug, Error)]
pub enum FoldProblem {
    #[error("{}", unknown_group(chain))]
    UnknownGroup { chain: Vec<String> },
    #[error("a cycle between groups: {}", chain.iter().map(|g| format!("`@{g}`")).collect::<Vec<_>>().join(" -> "))]
    GroupCycle { chain: Vec<String> },
    #[error("the group `{0}` is empty")]
    EmptyGroup(String),
    #[error("`@{group}` writes more than {MAX_GROUP_ENTRIES} entries from groups into one list")]
    TooManyEntries { group: String },
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
    #[error("`include` entry `{0}` is not a relative path")]
    Rooted(String),
    #[error("`{0}` in an included file")]
    MainSetting(String),
    #[error("`{key}` value `{value}` is not `path:line`")]
    NamedPlace { key: &'static str, value: String },
    #[error("`entry-sources` names `{0}`, which no list of the contract holds")]
    UnknownEntrySource(String),
}

impl FoldProblem {
    pub(crate) fn explain(&self, problem: Problem) -> Problem {
        let (help, docs) = match self {
            Self::UnknownGroup { .. } => (
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
            Self::TooManyEntries { .. } => (
                "a group that names another group twice doubles it: list the entries once, or split the contract".to_owned(),
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
            Self::Rooted(_) => (
                "write the path from the directory of the file that holds `include`, so the configuration reads the same in every checkout".to_owned(),
                Docs::Include,
            ),
            Self::MainSetting(key) => (
                format!(
                    "an included file holds contract tables, their `<kind>-defaults` tables, and `[groups]`: move `{key}` into the main configuration file"
                ),
                Docs::Include,
            ),
            Self::NamedPlace { .. } => (
                "write the file relative to the project root and the line from 1: `source = \"scripts/tables.py:120\"`".to_owned(),
                Docs::DefinedAt,
            ),
            Self::UnknownEntrySource(_) => (
                "name an entry as the contract's lists write it out, or remove it from `entry-sources`".to_owned(),
                Docs::DefinedAt,
            ),
        };
        problem.help(help).docs(docs)
    }
}

/// How many entries the groups of one list may write into it: a few lines of groups that each
/// name the one before twice would otherwise write out more entries than memory holds.
const MAX_GROUP_ENTRIES: u64 = 10_000;

/// The message of an unknown group: the name, and the groups that lead to it.
fn unknown_group(chain: &[String]) -> String {
    let missing = chain.last().map_or("", String::as_str);
    if chain.len() < 2 {
        return format!("`@{missing}` names no group");
    }
    let names: Vec<String> = chain.iter().map(|g| format!("`@{g}`")).collect();
    format!("`@{missing}` names no group: {}", names.join(" -> "))
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
/// An entry is a plain relative path: a rooted or drive-prefixed one names a place on one
/// machine, whatever the platform the configuration is read on.
fn included_path(main: &Path, entry: &str) -> Result<PathBuf, FoldProblem> {
    if entry.contains(['*', '?', '[', ']', '{', '}']) {
        return Err(FoldProblem::Glob(entry.to_owned()));
    }
    if rooted(entry) {
        return Err(FoldProblem::Rooted(entry.to_owned()));
    }
    let joined = main.parent().unwrap_or(Path::new("")).join(entry);
    Ok(joined
        .components()
        .filter(|c| *c != Component::CurDir)
        .collect())
}

/// Whether the path `entry` is rooted or drive-prefixed, on any platform.
fn rooted(entry: &str) -> bool {
    let drive = entry
        .as_bytes()
        .get(..2)
        .is_some_and(|b| b[0].is_ascii_alphabetic() && b[1] == b':');
    drive || entry.starts_with(['/', '\\'])
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
/// resolves them, leaving out the entries it rejects; none when the file cannot be read or is
/// not valid TOML. The language server watches them, also while the configuration does not
/// load.
pub fn included_files(path: &Path) -> Vec<PathBuf> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    (include_entries(&text, ConfigLayout::of(path)).into_iter())
        .filter_map(|(entry, _)| included_path(path, &entry).ok())
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
        let file = included_path(path, &entry).map_err(|problem| main.error(at, problem))?;
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
    /// A group is named by a whole entry that starts with `@`; an `@` anywhere else is part of
    /// the entry, as in the item key of `'Model.fields["user@example.com"]'`.
    fn meaning(&self) -> Meaning<'_> {
        match self.text.strip_prefix('@') {
            Some(name) => Meaning::Group(name),
            None => Meaning::Text,
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

/// Where a contract table is written, and the places it names for itself and its entries.
#[derive(Debug, Clone, Default)]
struct Origin {
    file: usize,
    /// Where the table starts.
    offset: Option<usize>,
    /// `source`, and the offset of its value.
    source: Option<(String, Option<usize>)>,
    /// `entry-sources`: each entry, the place it names, and the offset of the entry.
    entry_sources: Vec<(String, String, Option<usize>)>,
}

/// The key of a contract table naming where the contract is defined.
const SOURCE: &str = "source";

/// The key of a contract table naming where its entries are defined.
const ENTRY_SOURCES: &str = "entry-sources";

impl Origin {
    fn of(file: usize, (table, offset): (&dyn TableLike, Option<usize>)) -> Self {
        let source = table.get(SOURCE).and_then(|item| {
            let value = item.as_value()?;
            Some((value.as_str()?.to_owned(), value.span().map(|s| s.start)))
        });
        let named = table
            .get(ENTRY_SOURCES)
            .and_then(toml_edit::Item::as_table_like);
        let entry_sources = (named.into_iter().flat_map(|t| t.iter()))
            .filter_map(|(entry, item)| {
                let at = named?.get_key_value(entry)?.0.span().map(|s| s.start);
                Some((entry.to_owned(), item.as_str()?.to_owned(), at))
            })
            .collect();
        Self {
            file,
            offset,
            source,
            entry_sources,
        }
    }
}

/// The contract tables, defaults tables, and groups of one file.
struct Settings {
    /// By kind, in [`HekiConfig::CONTRACT_KEYS`] order.
    tables: Vec<Vec<Table>>,
    /// Where each table of `tables` is written.
    origins: Vec<Vec<Origin>>,
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
    let (tables, origins) = kinds
        .iter()
        .map(|kind| {
            let values = value.and_then(|v| v.get(*kind)?.as_array());
            let edits = edit
                .and_then(|e| e.get(kind))
                .map(tables_at)
                .unwrap_or_default();
            (values.into_iter().flatten())
                .filter_map(toml::Value::as_table)
                .enumerate()
                .map(|(i, t)| {
                    let edit = edits.get(i).copied();
                    let table = table_of(index, t, edit.map(|(e, _)| e));
                    (
                        table,
                        edit.map(|e| Origin::of(index, e)).unwrap_or_default(),
                    )
                })
                .unzip()
        })
        .unzip();
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
        origins,
        defaults,
        groups,
    }
}

/// The groups of every file, checked over the whole graph; their entries are written out only
/// into the lists that use them.
struct Groups<'s> {
    defs: Vec<&'s Group>,
    by_name: HashMap<&'s str, usize>,
    /// By group: how many entries it writes out, saturated at `u64::MAX`.
    sizes: Vec<u64>,
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
            sizes: Vec::new(),
            defs,
            by_name,
        };
        let mut sizes = vec![None; groups.defs.len()];
        for g in 0..groups.defs.len() {
            groups.size_of(files, g, &mut Vec::new(), &mut sizes)?;
        }
        groups.sizes = (sizes.into_iter())
            .map(|size| size.expect("every group is sized"))
            .collect();
        Ok(groups)
    }

    /// How many entries the group `g` writes out, each of its references checked on the way;
    /// `chain` holds the groups being sized, the outermost first.
    fn size_of(
        &self,
        files: &[File],
        g: usize,
        chain: &mut Vec<usize>,
        sizes: &mut [Option<u64>],
    ) -> Result<u64, ConfigError> {
        if let Some(size) = sizes[g] {
            return Ok(size);
        }
        chain.push(g);
        let mut size = 0u64;
        for entry in &self.defs[g].entries {
            let at = |problem| files[entry.file].error(entry.offset, problem);
            let Meaning::Group(name) = entry.meaning() else {
                size = size.saturating_add(1);
                continue;
            };
            let names = |groups: &[usize]| -> Vec<String> {
                (groups.iter())
                    .map(|c| self.defs[*c].name.clone())
                    .collect()
            };
            let Some(&inner) = self.by_name.get(name) else {
                let mut chain = names(chain);
                chain.push(name.to_owned());
                return Err(at(FoldProblem::UnknownGroup { chain }));
            };
            if let Some(start) = chain.iter().position(|c| *c == inner) {
                let chain = names(&[&chain[start..], &[inner]].concat());
                return Err(at(FoldProblem::GroupCycle { chain }));
            }
            size = size.saturating_add(self.size_of(files, inner, chain, sizes)?);
        }
        chain.pop();
        sizes[g] = Some(size);
        Ok(size)
    }

    /// `field` with its list written out; the groups it uses join `used`.
    fn expand_field(
        &self,
        files: &[File],
        field: &Field,
        used: &mut BTreeSet<usize>,
    ) -> Result<Field, ConfigError> {
        let Field::List(entries) = field else {
            return Ok(field.clone());
        };
        // Sized before anything is written out, so a list past the limit costs nothing.
        let mut from_groups = 0u64;
        for entry in entries {
            if let Meaning::Group(name) = entry.meaning() {
                let at = |problem| files[entry.file].error(entry.offset, problem);
                let Some(&g) = self.by_name.get(name) else {
                    let chain = vec![name.to_owned()];
                    return Err(at(FoldProblem::UnknownGroup { chain }));
                };
                from_groups = from_groups.saturating_add(self.sizes[g]);
                if from_groups > MAX_GROUP_ENTRIES {
                    let group = name.to_owned();
                    return Err(at(FoldProblem::TooManyEntries { group }));
                }
            }
        }
        let mut out = Vec::new();
        for entry in entries {
            self.write_out(entry, &mut out, used);
        }
        Ok(Field::List(out))
    }

    /// `entry`, or the entries of the group it names, onto `out`; the groups it reaches join
    /// `used`. Every reference is known and acyclic ([`Groups::new`]).
    fn write_out(&self, entry: &Entry, out: &mut Vec<Entry>, used: &mut BTreeSet<usize>) {
        match entry.meaning() {
            Meaning::Text => out.push(entry.clone()),
            Meaning::Group(name) => {
                let g = self.by_name[name];
                used.insert(g);
                for inner in &self.defs[g].entries {
                    self.write_out(inner, out, used);
                }
            }
        }
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
    let Merged {
        tables: merged,
        origins,
        main_defaults,
        used,
    } = merge(files, &settings, &groups)?;
    let kinds = HekiConfig::CONTRACT_KEYS;
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
        contracts: BTreeMap::new(),
    };
    let defined = Defined {
        files,
        starts: files.iter().map(|f| LineStarts::of(&f.text)).collect(),
        shown: &shown,
        root,
    };
    for (k, tables) in merged.iter().enumerate() {
        for (i, (table, origin)) in tables.iter().zip(&origins[k]).enumerate() {
            let places = defined.contract(table, origin)?;
            sources.contracts.insert((kinds[k].to_owned(), i), places);
            for (key, field) in table {
                let Field::List(entries) = field else {
                    continue;
                };
                let places: Vec<Option<EntryPlace>> = entries
                    .iter()
                    .map(|e| {
                        (e.file != 0 || e.group.is_some()).then(|| EntryPlace {
                            file: shown[e.file].clone(),
                            line: e.offset.map_or(1, |o| {
                                defined.starts[e.file].line_column(&files[e.file].text, o).0
                            }),
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
    let text = folds.then(|| written_out(&files[0], &merged, &main_defaults));
    Ok(Folded { text, sources })
}

/// The contract tables of every file with their lists written out.
struct Merged {
    /// By kind, the main file's tables, then each included file's.
    tables: Vec<Vec<Table>>,
    /// Where each table of `tables` is written.
    origins: Vec<Vec<Origin>>,
    /// By kind, the main file's defaults table with its lists written out.
    main_defaults: Vec<Option<Defaults>>,
    /// The groups a table uses.
    used: BTreeSet<usize>,
}

fn merge(files: &[File], settings: &[Settings], groups: &Groups) -> Result<Merged, ConfigError> {
    let kinds = HekiConfig::CONTRACT_KEYS;
    let mut used: BTreeSet<usize> = BTreeSet::new();
    // By file and kind: the defaults table with its lists written out, each key with the groups
    // it uses, which a table that takes the key uses.
    let mut defaults: Vec<Vec<Option<Defaults>>> = Vec::new();
    for s in settings {
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
    let mut origins: Vec<Vec<Origin>> = vec![Vec::new(); kinds.len()];
    for (f, s) in settings.iter().enumerate() {
        for (k, tables) in s.tables.iter().enumerate() {
            origins[k].extend(s.origins[k].iter().cloned());
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
                merged[k].push(expanded);
            }
        }
    }
    let main_defaults = defaults.swap_remove(0);
    Ok(Merged {
        tables: merged,
        origins,
        main_defaults,
        used,
    })
}

/// What the places of a fold's contracts are read against: its files, as [`EntryPlace::file`]
/// names them (`shown`), and the project root a named place is relative to.
struct Defined<'f> {
    files: &'f [File],
    /// The line starts of each of `files`.
    starts: Vec<LineStarts>,
    shown: &'f [String],
    root: &'f Path,
}

impl Defined<'_> {
    /// Where `table`, written as `origin` says, and the entries of its lists are defined.
    fn contract(&self, table: &Table, origin: &Origin) -> Result<ContractPlaces, ConfigError> {
        let file = &self.files[origin.file];
        let table_at = match &origin.source {
            Some((value, at)) => self
                .named(SOURCE, value)
                .map_err(|problem| file.error(*at, problem))?,
            None => self.written(origin.file, origin.offset, None),
        };
        let mut named: HashMap<&str, DefinedAt> = HashMap::new();
        let held: HashSet<&str> = (table.values())
            .filter_map(|field| match field {
                Field::List(entries) => Some(entries),
                Field::Value(_) => None,
            })
            .flatten()
            .map(|e| e.text.as_str())
            .collect();
        for (entry, value, at) in &origin.entry_sources {
            if !held.contains(entry.as_str()) {
                return Err(file.error(*at, FoldProblem::UnknownEntrySource(entry.clone())));
            }
            let place = (self.named(ENTRY_SOURCES, value)).map_err(|p| file.error(*at, p))?;
            named.insert(entry.as_str(), place);
        }
        let entries = (table.iter())
            .filter_map(|(key, field)| match field {
                Field::List(entries) => Some((key, entries)),
                Field::Value(_) => None,
            })
            .map(|(key, entries)| {
                let places = (entries.iter())
                    .map(|e| {
                        let at = named
                            .get(e.text.as_str())
                            .cloned()
                            .unwrap_or_else(|| self.written(e.file, e.offset, e.group.clone()));
                        (e.text.clone(), at)
                    })
                    .collect();
                (key.clone(), places)
            })
            .collect();
        Ok(ContractPlaces {
            table: table_at,
            entries,
        })
    }

    /// The place at `offset` of the file at `file`, in the group `group`.
    fn written(&self, file: usize, offset: Option<usize>, group: Option<String>) -> DefinedAt {
        let (line, column) = offset.map_or((1, 1), |o| {
            self.starts[file].line_column(&self.files[file].text, o)
        });
        DefinedAt {
            path: self.files[file].path.clone(),
            column,
            place: EntryPlace {
                file: self.shown[file].clone(),
                line,
                group,
            },
        }
    }

    /// The place `value` of `key` names: `path:line`, the path relative to the project root.
    fn named(&self, key: &'static str, value: &str) -> Result<DefinedAt, FoldProblem> {
        let bad = || FoldProblem::NamedPlace {
            key,
            value: value.to_owned(),
        };
        let (file, line) = value.rsplit_once(':').ok_or_else(bad)?;
        let line: u32 = line.parse().ok().filter(|l| *l > 0).ok_or_else(bad)?;
        if file.is_empty() || rooted(file) {
            return Err(bad());
        }
        Ok(DefinedAt {
            path: self.root.join(file),
            column: 1,
            place: EntryPlace {
                file: file.to_owned(),
                line,
                group: None,
            },
        })
    }
}

/// Where the entry `setting` names is written in a group, when a list of its contract took it
/// from one. The fold records where each entry it writes out comes from, so a group that holds
/// the same text but that no list of the contract uses is never named.
pub(crate) fn group_origin(main: &Path, setting: &Setting) -> Option<(PathBuf, (u32, u32))> {
    let kind = (HekiConfig::CONTRACT_KEYS.iter()).position(|k| *k == setting.table)?;
    let value = setting.value.as_deref()?;
    let text = std::fs::read_to_string(main).ok()?;
    let layout = ConfigLayout::of(main);
    // The project root only gives metadata that the places of entries do not depend on.
    let root = main.parent().unwrap_or(Path::new(""));
    layout.read(main, &text, root).ok()?;
    let files = read_files(layout, main, &text, root).ok()?;
    let settings: Vec<Settings> = (files.iter().enumerate())
        .map(|(i, f)| settings_of(i, f))
        .collect();
    let shown: Vec<String> = files.iter().map(|f| shown_path(&f.path, root)).collect();
    let groups = Groups::new(&files, &shown, &settings).ok()?;
    let merged = merge(&files, &settings, &groups).ok()?;
    let named = |table: &&Table| {
        setting.entry.as_deref().is_none_or(|name| {
            matches!(table.get("name"), Some(Field::Value(toml::Value::String(n))) if n == name)
        })
    };
    let entry = (merged.tables[kind].iter().filter(named))
        .flat_map(|table| table.iter())
        .filter(|(key, _)| setting.key.as_deref().is_none_or(|k| k == key.as_str()))
        .filter_map(|(_, field)| match field {
            Field::List(entries) => Some(entries),
            Field::Value(_) => None,
        })
        .flatten()
        .find(|e| e.group.is_some() && e.text == value)?;
    let file = &files[entry.file];
    Some((file.path.clone(), line_column(&file.text, entry.offset?)))
}

/// The main file with its contract tables replaced by `merged`, its groups removed, and its
/// defaults tables (`main_defaults`, by kind) written with their groups expanded, so a tool that
/// adds a table later takes the same values the tables did. Its settings table is there: a group, a defaults table, or an `include`
/// is in it, or in a file its `include` lists.
fn written_out(main: &File, merged: &[Vec<Table>], main_defaults: &[Option<Defaults>]) -> String {
    let mut doc: toml::Table = toml::from_str(&main.text).expect("a configuration that was read");
    let settings = settings_mut(main.layout, &mut doc).expect("a configuration with settings");
    settings.remove(GROUPS);
    for ((kind, tables), defaults) in HekiConfig::CONTRACT_KEYS
        .iter()
        .zip(merged)
        .zip(main_defaults)
    {
        let defaults_key = format!("{kind}{SUFFIX}");
        settings.remove(&defaults_key);
        if let Some(defaults) = defaults {
            let table: Table = defaults
                .iter()
                .map(|(k, (f, _))| (k.clone(), f.clone()))
                .collect();
            settings.insert(defaults_key, toml::Value::Table(value_of(&table)));
        }
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
