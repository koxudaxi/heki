//! What a command says when it cannot do what it was asked (a configuration it cannot read, a
//! rule it does not know, a path that does not exist): the message, where the cause is, what to
//! do about it, the page of the documentation that explains it, and the names it might have
//! meant. The command line prints it in the layout of a report, `--output-format json` as an
//! object, and the language server in its log, so all of them say the same thing.

use crate::{ConfigLayout, HekiConfig};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Serialize, Serializer};
use std::borrow::Cow;
use std::ops::Range;
use std::path::{Path, PathBuf};

/// The documentation site (`site_url` in `zensical.toml`).
pub const SITE: &str = "https://heki.koxudaxi.dev/";

/// What each release changed, on the default branch (the binary also prints its own release's
/// section with `heki version --changelog`).
pub const CHANGELOG: &str = "https://github.com/koxudaxi/heki/blob/main/CHANGELOG.md";

/// Where heki's bugs are reported.
pub const ISSUES: &str = "https://github.com/koxudaxi/heki/issues";

macro_rules! docs_pages {
    ($($variant:ident => $path:literal,)*) => {
        /// A page of the documentation, or a section of one, that heki's output points to.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Docs {
            $($variant,)*
        }

        impl Docs {
            /// Every page and section the output can point to.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)*];

            /// The page below the site root, with the anchor of its section.
            pub const fn path(self) -> &'static str {
                match self {
                    $(Self::$variant => $path,)*
                }
            }
        }
    };
}

docs_pages! {
    Home => "",
    GettingStarted => "getting-started/",
    Configuration => "configuration/",
    CodingAgents => "coding-agents/",
    AgentSkill => "coding-agents/#agent-skill-experimental",
    InstallSkill => "coding-agents/#install-the-skill",
    LlmsTxt => "llms.txt",
    ConfigFiles => "configuration/#where-heki-finds-the-configuration",
    ConfigTables => "configuration/#which-tables-hold-the-settings",
    SourceRoots => "configuration/#module-names-follow-the-source-roots",
    TopLevel => "configuration/#top-level-settings",
    TargetVersion => "configuration/#the-default-target-version",
    RequiredVersion => "configuration/#requiring-a-heki-version",
    CheckedFiles => "configuration/#which-files-are-checked",
    Lint => "configuration/#rule-selection-toolhekilint",
    Contracts => "configuration/#contracts",
    WriteOwners => "configuration/#write-owners",
    ReadOwners => "configuration/#read-owners",
    CallOwners => "configuration/#call-owners",
    EffectBoundaries => "configuration/#effect-boundaries",
    ReadonlyReturns => "configuration/#readonly-returns",
    Invalidation => "configuration/#invalidation",
    ImportBoundaries => "configuration/#import-boundaries",
    Groups => "configuration/#named-groups",
    Include => "configuration/#several-configuration-files",
    DefinedAt => "configuration/#where-a-contract-is-defined",
    Imports => "configuration/#imports-toolhekiimports",
    Ecosystem => "configuration/#ecosystem-knowledge-toolhekiecosystem",
    Rules => "rules/",
    Baselines => "adoption/#ratchet-baselines",
    Snapshots => "adoption/#effect-snapshots-mut103",
    Fixes => "fixes/#how-fix-applies-fixes",
    CliCheck => "cli-reference/#heki-check",
    CliServer => "cli-reference/#heki-server",
    CliRule => "cli-reference/#heki-rule",
    CliSnapshot => "cli-reference/#heki-snapshot",
    CliAssumptions => "cli-reference/#heki-assumptions",
    CliInspect => "cli-reference/#heki-inspect",
    CliRecommend => "cli-reference/#heki-recommend",
    CliSkillInstall => "cli-reference/#heki-skill-install",
    CliVersion => "cli-reference/#heki-version",
    Inspect => "coding-agents/#what-a-function-does-heki-inspect-experimental",
}

impl Docs {
    pub fn url(self) -> String {
        format!("{SITE}{}", self.path())
    }

    /// The section of the configuration page that describes the table `table` (`lint`,
    /// `write-owners`; empty for the top level).
    pub fn of_table(table: &str) -> Self {
        let table = table.split('.').next().unwrap_or_default();
        // A defaults table is described with its contract.
        match table.strip_suffix(crate::defaults::SUFFIX).unwrap_or(table) {
            "lint" => Self::Lint,
            "write-owners" => Self::WriteOwners,
            "read-owners" => Self::ReadOwners,
            "call-owners" => Self::CallOwners,
            "effect-boundaries" => Self::EffectBoundaries,
            "readonly-returns" => Self::ReadonlyReturns,
            "invalidation" => Self::Invalidation,
            "import-boundaries" => Self::ImportBoundaries,
            "imports" => Self::Imports,
            "ecosystem" => Self::Ecosystem,
            "groups" => Self::Groups,
            "include" => Self::Include,
            _ => Self::TopLevel,
        }
    }

    /// The section that describes the key `key` of the table `table`: a top-level key with a
    /// section of its own has it.
    pub fn of_key(table: &str, key: &str) -> Self {
        match (table, key) {
            ("", "target-version") => Self::TargetVersion,
            ("", "exclude" | "extend-exclude") => Self::CheckedFiles,
            ("", "required-version") => Self::RequiredVersion,
            ("", "src") => Self::SourceRoots,
            ("", "baseline") => Self::Baselines,
            ("", table) => Self::of_table(table),
            (table, _) => Self::of_table(table),
        }
    }
}

impl Serialize for Docs {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.url())
    }
}

/// The URL of one of the pages and sections of [`Docs::ALL`].
impl JsonSchema for Docs {
    fn schema_name() -> Cow<'static, str> {
        "Docs".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        let urls: Vec<String> = Self::ALL.iter().map(|d| d.url()).collect();
        json_schema!({
            "description": "The page of the documentation, or the section of one, that explains it.",
            "type": "string",
            "enum": urls,
        })
    }
}

/// The page of the rule named `slug`.
pub fn rule_page(slug: &str) -> String {
    format!("{SITE}rules/{slug}/")
}

/// A file, and a position in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    /// The 1-based line and byte column, when the cause is at one.
    pub position: Option<(u32, u32)>,
}

/// `path`, and `line` and `column` when there is a position.
impl JsonSchema for Location {
    fn schema_name() -> Cow<'static, str> {
        "Location".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "title": Self::schema_name(),
            "description": "A file, and a position in it.",
            "type": "object",
            "properties": {
                "path": {
                    "description": "The file, as `heki check` prints paths.",
                    "type": "string",
                },
                "line": {
                    "description": "The 1-based line, when the cause is at a position.",
                    "type": "integer",
                    "minimum": 0,
                    "format": "uint32",
                },
                "column": {
                    "description": "The 1-based byte column; written with `line`.",
                    "type": "integer",
                    "minimum": 0,
                    "format": "uint32",
                },
            },
            "required": ["path"],
            "additionalProperties": false,
        })
    }
}

impl Serialize for Location {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("path", &self.path.to_string_lossy())?;
        if let Some((line, column)) = self.position {
            map.serialize_entry("line", &line)?;
            map.serialize_entry("column", &column)?;
        }
        map.end()
    }
}

/// A value of the configuration, found again in the configuration file to show where it is:
/// the first `value` (an entry of an array, or the whole value) of the key `key` in the table
/// `table` (`lint`, `write-owners`; empty for the top level), in the entry named `entry` of an
/// array of tables. `None` matches any key or value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Setting {
    pub table: String,
    pub entry: Option<String>,
    pub key: Option<String>,
    pub value: Option<String>,
}

impl Setting {
    /// A `table.key` context, as contract errors name the key they read.
    pub fn of_context(context: &str, value: Option<&str>) -> Self {
        let (table, key) = context.split_once('.').unwrap_or((context, ""));
        Self {
            table: table.to_owned(),
            entry: None,
            key: (!key.is_empty()).then(|| key.to_owned()),
            value: value.map(str::to_owned),
        }
    }
}

/// What a command could not do, and how to fix it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema, thiserror::Error)]
#[error("{message}")]
#[schemars(deny_unknown_fields)]
pub struct Problem {
    /// What went wrong.
    pub message: String,
    /// Where the cause is, when it is in a file: `path`, and `line` and `column` when it is at
    /// a position.
    #[serde(flatten)]
    pub location: Option<Location>,
    /// What to do about it, one step per line.
    pub help: Vec<String>,
    /// The page of the documentation that explains it.
    pub docs: Option<Docs>,
    /// The names it might have meant, the closest first.
    pub candidates: Vec<String>,
    /// The value of the configuration it is about, until [`Problem::locate`] finds it.
    #[serde(skip)]
    #[schemars(skip)]
    pub setting: Option<Setting>,
}

impl Problem {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::default()
        }
    }

    pub fn help(mut self, help: impl Into<String>) -> Self {
        self.help.push(help.into());
        self
    }

    pub fn docs(mut self, docs: Docs) -> Self {
        self.docs = Some(docs);
        self
    }

    /// The candidates, with a help line offering the first ones.
    pub fn did_you_mean(mut self, candidates: Vec<String>) -> Self {
        if let Some(line) = did_you_mean(&candidates) {
            self.help.insert(0, line);
        }
        self.candidates = candidates;
        self
    }

    /// `word`, which is none of `names`: the names that look like it, else all of them.
    pub fn one_of<S: AsRef<str>>(self, word: &str, names: &[S]) -> Self {
        let near = nearest(word, names.iter().map(AsRef::as_ref));
        if near.is_empty() {
            let listed: Vec<String> = names.iter().map(|n| format!("`{}`", n.as_ref())).collect();
            let mut problem = self.help(format!("expected one of {}", listed.join(", ")));
            problem.candidates = names.iter().map(|n| n.as_ref().to_owned()).collect();
            problem
        } else {
            self.did_you_mean(near.into_iter().map(str::to_owned).collect())
        }
    }

    pub fn at(mut self, path: &Path, line: u32, column: u32) -> Self {
        self.location = Some(Location {
            path: path.to_path_buf(),
            position: Some((line, column)),
        });
        self
    }

    /// The cause is the file at `path` as a whole.
    pub fn at_file(mut self, path: &Path) -> Self {
        self.location = Some(Location {
            path: path.to_path_buf(),
            position: None,
        });
        self
    }

    pub fn setting(mut self, setting: Setting) -> Self {
        self.setting = Some(setting);
        self
    }

    /// The location of its configuration value: where `config_file`, the file the run read its
    /// configuration from, or a file its `include` lists writes it, else the group a list took
    /// it from.
    pub fn locate(self, config_file: Option<&Path>) -> Self {
        let wanted = self.setting.as_ref().filter(|_| self.location.is_none());
        let found = wanted.zip(config_file).and_then(|(setting, main)| {
            let included = crate::included_files(main);
            (std::iter::once((main.to_path_buf(), ConfigLayout::of(main))))
                .chain(included.into_iter().map(|p| (p, ConfigLayout::TopLevel)))
                .find_map(|(path, layout)| {
                    let text = std::fs::read_to_string(&path).ok()?;
                    let at = locate_setting(&text, layout, setting)?;
                    Some((path, line_column(&text, at.start)))
                })
                .or_else(|| crate::fold::group_origin(main, setting))
        });
        match found {
            Some((path, (line, column))) => self.at(&path, line, column),
            None => self,
        }
    }
}

/// An error that says what to do about it.
pub trait Explain {
    fn explain(&self) -> Problem;
}

impl Explain for Problem {
    fn explain(&self) -> Problem {
        self.clone()
    }
}

/// "did you mean `a` or `b`?" for the first two of `candidates`.
fn did_you_mean(candidates: &[String]) -> Option<String> {
    let quoted: Vec<String> = candidates
        .iter()
        .take(2)
        .map(|c| format!("`{c}`"))
        .collect();
    (!quoted.is_empty()).then(|| format!("did you mean {}?", quoted.join(" or ")))
}

/// The candidates that look like `word`, the closest first: at most three, each as similar as
/// `clap` requires for its own suggestions (a Jaro similarity above 0.7, ignoring case).
pub fn nearest<'c>(word: &str, candidates: impl IntoIterator<Item = &'c str>) -> Vec<&'c str> {
    let word = word.to_lowercase();
    let mut scored: Vec<(f64, &str)> = candidates
        .into_iter()
        .map(|c| (strsim::jaro(&word, &c.to_lowercase()), c))
        .filter(|(score, _)| *score > 0.7)
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.into_iter().take(3).map(|(_, c)| c).collect()
}

/// The 1-based line and byte column of byte `offset` of `text`.
pub fn line_column(text: &str, offset: usize) -> (u32, u32) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.len() - before.rfind('\n').map_or(0, |i| i + 1) + 1;
    (to_u32(line), to_u32(column))
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The span of `setting` in the configuration `text`: in its own table, else, for a key an
/// entry of a contract leaves to its defaults, in the contract's defaults table.
fn locate_setting(text: &str, layout: ConfigLayout, setting: &Setting) -> Option<Range<usize>> {
    let doc = toml_edit::Document::parse(text).ok()?;
    let root = settings_root(doc.as_table(), layout)?;
    locate_in(root, &setting.table, setting.entry.as_deref(), setting).or_else(|| {
        let defaults = format!("{}{}", setting.table, crate::defaults::SUFFIX);
        // Only a key can be defaulted: a setting without one is not found in the defaults.
        setting.key.as_ref()?;
        locate_in(root, &defaults, None, setting)
    })
}

/// The span of the key and value of `setting` in the tables of `table`, those of the entry named
/// `entry` only when there is one.
fn locate_in(
    root: &dyn toml_edit::TableLike,
    table: &str,
    entry: Option<&str>,
    setting: &Setting,
) -> Option<Range<usize>> {
    let mut tables = vec![root];
    for part in table.split('.').filter(|p| !p.is_empty()) {
        tables = tables
            .into_iter()
            .filter_map(|t| t.get(part))
            .flat_map(table_items)
            .collect();
    }
    tables
        .into_iter()
        .filter(|t| {
            entry.is_none_or(|name| t.get("name").and_then(toml_edit::Item::as_str) == Some(name))
        })
        .find_map(|t| {
            let keys: Vec<&str> = t.iter().map(|(k, _)| k).collect();
            keys.into_iter()
                .filter(|k| setting.key.as_deref().is_none_or(|wanted| wanted == *k))
                .find_map(|k| {
                    let (key, item) = t.get_key_value(k)?;
                    match &setting.value {
                        None => key.span(),
                        Some(value) => value_span(item, value),
                    }
                })
        })
}

/// The table that holds the settings in a file of `layout`.
pub(crate) fn settings_root(
    doc: &toml_edit::Table,
    layout: ConfigLayout,
) -> Option<&dyn toml_edit::TableLike> {
    match layout.table_prefix() {
        Some(_) => doc.get("tool")?.get("heki")?.as_table_like(),
        None => Some(doc),
    }
}

/// The tables an item holds: itself, or each table of an array of tables.
pub(crate) fn table_items(item: &toml_edit::Item) -> Vec<&dyn toml_edit::TableLike> {
    (tables_at(item).into_iter()).map(|(t, _)| t).collect()
}

/// [`table_items`], each with the offset where it starts: its header, or its `{` when inline.
pub(crate) fn tables_at(item: &toml_edit::Item) -> Vec<(&dyn toml_edit::TableLike, Option<usize>)> {
    let start = |span: Option<Range<usize>>| span.map(|s| s.start);
    if let Some(array) = item.as_array_of_tables() {
        return array
            .iter()
            .map(|t| (t as &dyn toml_edit::TableLike, start(t.span())))
            .collect();
    }
    if let Some(array) = item.as_array() {
        return array
            .iter()
            .filter_map(|v| v.as_inline_table())
            .map(|t| (t as &dyn toml_edit::TableLike, start(t.span())))
            .collect();
    }
    let at = |t: &toml_edit::Table| start(t.span());
    (item.as_table_like().into_iter())
        .map(|t| (t, item.as_table().and_then(at)))
        .collect()
}

/// The span of `value` in `item`: the item itself, or an entry of its array.
fn value_span(item: &toml_edit::Item, value: &str) -> Option<Range<usize>> {
    let item = item.as_value()?;
    if item.as_str() == Some(value) {
        return item.span();
    }
    item.as_array()?
        .iter()
        .find(|v| v.as_str() == Some(value))
        .and_then(toml_edit::Value::span)
}

/// The name a configuration file writes `table` with: `[tool.heki.lint]` in a `pyproject.toml`,
/// `[lint]` in a `heki.toml`; `[tool.heki]` or "the top level" for the top level.
pub fn table_name(layout: ConfigLayout, table: &str) -> String {
    let array = HekiConfig::CONTRACT_KEYS.contains(&table) || table == "ecosystem";
    let (open, close) = if array { ("[[", "]]") } else { ("[", "]") };
    match (layout.table_prefix(), table) {
        (Some(prefix), "") => format!("[{prefix}]"),
        (None, "") => "the top level".to_owned(),
        (Some(prefix), table) => format!("{open}{prefix}.{table}{close}"),
        (None, table) => format!("{open}{table}{close}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_names_look_like_the_word() {
        let codes = ["MUT101", "MUT119", "EFF101", "OPT101"];
        assert_eq!(nearest("MUT999", codes), ["MUT119"]);
        assert_eq!(nearest("mut101", codes)[0], "MUT101");
        assert!(nearest("zzz", codes).is_empty());
        let keys = ["select", "extend-select", "ignore"];
        assert_eq!(nearest("selct", keys)[0], "select");
    }

    #[test]
    fn did_you_mean_offers_two() {
        assert_eq!(did_you_mean(&[]), None);
        let three = ["a", "b", "c"].map(str::to_owned);
        assert_eq!(
            did_you_mean(&three).as_deref(),
            Some("did you mean `a` or `b`?")
        );
    }

    #[test]
    fn line_columns_are_one_based() {
        assert_eq!(line_column("ab\ncd", 0), (1, 1));
        assert_eq!(line_column("ab\ncd", 4), (2, 2));
        assert_eq!(line_column("ab", 9), (1, 3));
    }

    #[test]
    fn settings_are_found_in_their_tables() {
        let text = "[tool.heki.lint]\nselect = [\"EFF\", \"MUT999\"]\n\n[[tool.heki.write-owners]]\nname = \"a\"\ntargets = [\"x\"]\n\n[[tool.heki.write-owners]]\nname = \"b\"\ntargets = [\"x\"]\n";
        let at = |table: &str, entry: Option<&str>, key: Option<&str>, value: Option<&str>| {
            let setting = Setting {
                table: table.to_owned(),
                entry: entry.map(str::to_owned),
                key: key.map(str::to_owned),
                value: value.map(str::to_owned),
            };
            locate_setting(text, ConfigLayout::PyProject, &setting)
                .map(|span| line_column(text, span.start))
        };
        assert_eq!(
            at("lint", None, Some("select"), Some("MUT999")),
            Some((2, 18))
        );
        assert_eq!(at("lint", None, Some("select"), None), Some((2, 1)));
        assert_eq!(
            at("write-owners", Some("b"), Some("targets"), Some("x")),
            Some((10, 12))
        );
        assert_eq!(at("write-owners", None, None, Some("x")), Some((6, 12)));
        assert_eq!(at("lint", None, Some("ignore"), None), None);
        assert_eq!(at("missing", None, None, None), None);
        let inline = "lint = { select = \"EFF\" }\necosystem = [{ symbol = \"m.f\" }]\n";
        let found = |setting: &Setting| {
            locate_setting(inline, ConfigLayout::TopLevel, setting)
                .map(|span| line_column(inline, span.start))
        };
        assert_eq!(
            found(&Setting::of_context("lint.select", Some("EFF"))),
            Some((1, 19))
        );
        assert_eq!(
            found(&Setting::of_context("ecosystem.symbol", Some("m.f"))),
            Some((2, 25))
        );
        assert_eq!(
            found(&Setting::of_context("ecosystem", None)),
            Some((2, 16))
        );
        assert_eq!(
            locate_setting("[", ConfigLayout::TopLevel, &Setting::default()),
            None
        );
        assert_eq!(
            locate_setting("x = 1\n", ConfigLayout::PyProject, &Setting::default()),
            None
        );
    }

    #[test]
    fn tables_are_named_as_the_file_writes_them() {
        assert_eq!(table_name(ConfigLayout::PyProject, ""), "[tool.heki]");
        assert_eq!(table_name(ConfigLayout::TopLevel, ""), "the top level");
        assert_eq!(
            table_name(ConfigLayout::PyProject, "lint"),
            "[tool.heki.lint]"
        );
        assert_eq!(
            table_name(ConfigLayout::TopLevel, "write-owners"),
            "[[write-owners]]"
        );
    }

    #[test]
    fn docs_name_the_section_of_each_key() {
        for (table, key, docs) in [
            ("", "target-version", Docs::TargetVersion),
            ("", "extend-exclude", Docs::CheckedFiles),
            ("", "baseline", Docs::Baselines),
            ("", "write-owners", Docs::WriteOwners),
            ("read-owners", "targets", Docs::ReadOwners),
            ("call-owners", "targets", Docs::CallOwners),
            ("effect-boundaries", "forbid", Docs::EffectBoundaries),
            ("readonly-returns", "deep", Docs::ReadonlyReturns),
            ("invalidation", "invalidators", Docs::Invalidation),
            ("import-boundaries", "reach", Docs::ImportBoundaries),
            ("imports", "cycle-edges", Docs::Imports),
            ("lint.severity", "MUT101", Docs::Lint),
        ] {
            assert_eq!(Docs::of_key(table, key), docs, "{table}.{key}");
        }
        assert_eq!(Docs::of_key("", "src"), Docs::SourceRoots);
        assert_eq!(Docs::of_key("", "lint"), Docs::Lint);
        assert_eq!(Docs::of_key("", "nope"), Docs::TopLevel);
        assert_eq!(
            Docs::of_key("ecosystem.writes-in-order", "arg"),
            Docs::Ecosystem
        );
        assert_eq!(
            serde_json::to_value(Docs::Lint).unwrap(),
            "https://heki.koxudaxi.dev/configuration/#rule-selection-toolhekilint"
        );
        let problem = Problem::new("m").at(Path::new("a/b.toml"), 1, 2);
        let json = serde_json::to_value(&problem).unwrap();
        assert_eq!(
            (&json["path"], &json["line"]),
            (&"a/b.toml".into(), &1.into())
        );
        let file = serde_json::to_value(Problem::new("m").at_file(Path::new("b"))).unwrap();
        assert_eq!((&file["path"], file.get("line")), (&"b".into(), None));
    }
}
