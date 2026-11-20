//! `heki_rules` tests: `tests/semantic/fixtures.py` (every `# EXPECT:` found, every
//! `# MUST NOT report` absent, nothing else reported) plus per-rule edge cases.

use super::*;
use heki_ast::parse_module;
use heki_effect::{ModuleInput, analyze_project, module_name_for_path};
use heki_env::{PythonVersion, ReadOnlyReturnRule, RuntimeProfile, WriteOwnerRule};
use heki_ir::{ModuleIr, build_module_ir};
use heki_semantic::{SemanticModel, analyze_module};
use std::collections::BTreeSet;
use std::path::PathBuf;

const FIXTURES: &str = include_str!("../../../tests/semantic/fixtures.py");

struct Built {
    rel: String,
    src: String,
    module_name: String,
    is_package: bool,
    semantic: SemanticModel,
    ir: ModuleIr,
}

fn build(rel: &str, src: &str) -> Built {
    let (module_name, is_package) = module_name_for_path(std::path::Path::new(rel), false).unwrap();
    let parsed = parse_module(src).expect("valid python");
    let profile = RuntimeProfile::from(PythonVersion::PY315);
    let semantic = analyze_module(&parsed, src, profile);
    let ir = build_module_ir(&parsed, src, &semantic, profile);
    Built {
        rel: rel.into(),
        src: src.into(),
        module_name,
        is_package,
        semantic,
        ir,
    }
}

fn check_with(files: &[(&str, &str)], config: &HekiConfig, select: &[&str]) -> Vec<Diagnostic> {
    let built: Vec<Built> = files.iter().map(|(r, s)| build(r, s)).collect();
    let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
    let units: Vec<ModuleUnit<'_>> = built
        .iter()
        .map(|b| ModuleUnit {
            input: ModuleInput {
                path: PathBuf::from(&b.rel),
                module_name: b.module_name.clone(),
                is_package: b.is_package,
                content_hash: String::new(),
                semantic: &b.semantic,
                ir: &b.ir,
            },
            source: &b.src,
            rel_path: b.rel.clone(),
            lines: LineIndex::from_source(&b.src),
        })
        .collect();
    let inputs: Vec<ModuleInput<'_>> = units
        .iter()
        .map(|u| ModuleInput {
            path: u.input.path.clone(),
            module_name: u.input.module_name.clone(),
            is_package: u.input.is_package,
            content_hash: String::new(),
            semantic: u.input.semantic,
            ir: u.input.ir,
        })
        .collect();
    let index = analyze_project(&inputs, &kb);
    let select: Vec<String> = select.iter().map(|s| (*s).to_owned()).collect();
    let selection = RuleSelection::resolve(&config.lint, &select, &[], &[]).unwrap();
    let soundness_kb = EffectiveSoundnessKb::load_builtin(PythonVersion::PY315).unwrap();
    let extras = CheckExtras {
        soundness_kb: &soundness_kb,
        snapshot: None,
        decls: None,
    };
    check_project(&units, config, &kb, &index, &selection, &extras).unwrap()
}

fn check(src: &str, config: &HekiConfig) -> Vec<Diagnostic> {
    check_with(&[("app.py", src)], config, &[])
}

fn codes(diags: &[Diagnostic]) -> Vec<&str> {
    diags.iter().map(|d| d.rule_code.as_str()).collect()
}

fn owners(targets: &[&str], allowed: &[&str]) -> HekiConfig {
    HekiConfig {
        write_owners: vec![WriteOwnerRule {
            name: "store".into(),
            targets: targets.iter().map(|s| (*s).to_owned()).collect(),
            allowed_writers: allowed.iter().map(|s| (*s).to_owned()).collect(),
            ..WriteOwnerRule::default()
        }],
        ..HekiConfig::default()
    }
}

/// One fixture expectation: an `# EXPECT: CODE` / `# MUST NOT report CODE` comment covers the
/// code lines after it, up to the next comment or blank line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Expect {
    code: String,
    lines: std::ops::RangeInclusive<u32>,
}

fn expectations(src: &str) -> (Vec<Expect>, Vec<Expect>) {
    let lines: Vec<&str> = src.lines().collect();
    let mut tp = Vec::new();
    let mut tn = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        let (set, rest) = if let Some(r) = t.strip_prefix("# EXPECT:") {
            (&mut tp, r)
        } else if let Some(r) = t.strip_prefix("# MUST NOT report") {
            (&mut tn, r)
        } else {
            continue;
        };
        let code: String = rest
            .trim()
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        let first = i + 1;
        let last = (first..lines.len())
            .take_while(|j| {
                let l = lines[*j].trim();
                !l.is_empty() && !l.starts_with('#')
            })
            .last()
            .unwrap();
        let n = |j: usize| u32::try_from(j + 1).unwrap();
        set.push(Expect {
            code,
            lines: n(first)..=n(last),
        });
    }
    (tp, tn)
}

#[test]
fn semantic_fixtures_all_tp_detected_and_no_tn() {
    let mut config = owners(&["Model.fields"], &["Store.*"]);
    // The fixtures live under `tests/`: turn the test-code relaxation off for them.
    config.lint.relax_async_in_tests = Some(false);
    let diags = check_with(&[("tests/semantic/fixtures.py", FIXTURES)], &config, &[]);
    let (tp, tn) = expectations(FIXTURES);
    assert_eq!(tp.len(), 6, "{tp:?}");
    assert_eq!(tn.len(), 7, "{tn:?}");
    let in_range = |e: &Expect, d: &Diagnostic| e.code == d.rule_code && e.lines.contains(&d.line);
    for e in &tp {
        assert_eq!(
            diags.iter().filter(|d| in_range(e, d)).count(),
            1,
            "TP {e:?} not reported exactly once:\n{diags:#?}"
        );
    }
    for e in &tn {
        assert!(
            !diags.iter().any(|d| in_range(e, d)),
            "TN {e:?} reported:\n{diags:#?}"
        );
    }
    // Nothing beyond the expectations.
    assert_eq!(diags.len(), tp.len(), "{diags:#?}");
    // The indirect MUT201 carries the helper hop; EFF101 the 2-hop chain.
    let indirect = diags
        .iter()
        .find(|d| d.rule_code == "MUT201" && d.message.contains("_sneaky_helper"))
        .unwrap();
    assert_eq!(indirect.related.len(), 1);
    let eff = diags.iter().find(|d| d.rule_code == "EFF101").unwrap();
    assert_eq!(eff.target, "time.sleep");
    assert_eq!(eff.related.len(), 2);
}

#[test]
fn semantic_fixtures_relax_tests_by_default() {
    let config = owners(&["Model.fields"], &["Store.*"]);
    let diags = check_with(&[("tests/semantic/fixtures.py", FIXTURES)], &config, &[]);
    assert!(!codes(&diags).contains(&"EFF101"));
    assert!(codes(&diags).contains(&"MUT201"));
}

// ============================================================================ MUT101

const MUT101_SRC: &str = r#"
from functools import cached_property

class Conf:
    def __init__(self):
        self.raw = {}
        self.index = {}
        self.registry = {}
        self._load()

    def _load(self):
        self.raw = {"a": 1}

    def _init_more(self):
        self.raw = {}

    @cached_property
    def view(self):
        return (self.raw, self.index, self.registry)

    @cached_property
    def proxy(self):
        return self.raw["proxy"]

    def guarded(self, k, v):
        if k not in self.index:
            self.index[k] = v

    def memo(self, k, v):
        self.registry.setdefault(k, v)

    def reset_del(self):
        self.raw = {}
        del self.view
        del self.proxy

    def reset_dict(self):
        self.raw = {}
        self.__dict__.clear()

    def overwrite(self):
        self.raw = {}

    def via_helper(self):
        self._set()

    def _set(self):
        self.raw["proxy"] = 1

    def refresh(self):
        self._swap()
        del self.view
        del self.proxy

    def _swap(self):
        self.raw = {}

class Child(Conf):
    def child_write(self):
        self.raw = None

class Outer:
    @cached_property
    def p(self):
        return self.x

    class Inner:
        def w(self):
            self.x = 2
"#;

#[test]
fn mut101_filters() {
    let diags = check(MUT101_SRC, &HekiConfig::default());
    let targets: BTreeSet<(String, String)> = diags
        .iter()
        .filter(|d| d.rule_code == "MUT101")
        .map(|d| (d.qualname.clone(), d.target.clone()))
        .collect();
    let expected: BTreeSet<(String, String)> = [
        ("Conf.overwrite", "Conf.raw->view"),
        ("Conf.overwrite", "Conf.raw->proxy"),
        ("Conf._set", "Conf.raw->view"),
        ("Conf._set", "Conf.raw->proxy"),
        ("Child.child_write", "Conf.raw->view"),
        ("Child.child_write", "Conf.raw->proxy"),
    ]
    .iter()
    .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
    .collect();
    // Excluded: __init__, `_load` (called from __init__), `_init_*`, the `not in` guard,
    // `setdefault`, both invalidations, the nested class `Outer.Inner`, `via_helper` (the
    // write is reported once, in `_set`), and `_swap` (its only caller `refresh` invalidates).
    assert_eq!(targets, expected, "{diags:#?}");
    let overwrite = diags
        .iter()
        .find(|d| d.qualname == "Conf.overwrite")
        .unwrap();
    assert_eq!(overwrite.line, 42, "reported at the write");
}

#[test]
fn mut101_disjoint_and_overlapping_const_keys() {
    let src = r#"
from functools import cached_property

class P:
    def __init__(self):
        self.params = {}

    @cached_property
    def tmpl(self):
        return self.params["outtmpl"]

    def set_proxy(self, url):
        self.params["proxy"] = url

    def set_tmpl(self, t):
        self.params["outtmpl"] = t

    def replace(self, p):
        self.params.update(p)
"#;
    let diags = check(src, &HekiConfig::default());
    let methods: BTreeSet<&str> = diags.iter().map(|d| d.qualname.as_str()).collect();
    assert_eq!(methods, BTreeSet::from(["P.set_tmpl", "P.replace"]));
}

// ============================================================================ MUT201

const MUT201_SRC: &str = r#"
from functools import cached_property

class Model:
    def __init__(self):
        self.fields = []

class SubModel(Model):
    pass

class Store:
    def add(self, m: Model, x):
        m.fields.append(x)
        _owned_helper(m, x)

def _owned_helper(m: Model, x):
    m.fields.append(x)

def _shared_helper(m: Model, x):
    m.fields.append(x)

def public_helper(m: Model, x):
    m.fields.append(x)

def bad_direct(m: Model):
    m.fields = []
    del m.fields
    setattr(m, "fields", [])
    m.__dict__["fields"] = []

def bad_sub(s: SubModel):
    s.fields.append(1)

def calls_shared(m: Model):
    _shared_helper(m, 1)

def calls_public(m: Model):
    public_helper(m, 1)

def builds():
    return Model()

class Guarded(Model):
    @cached_property
    def size(self):
        return len(self.fields)
"#;

#[test]
fn mut201_owners_helpers_and_initializers() {
    let mut config = owners(&["Model.fields"], &["Store.*"]);
    config.write_owners[0].forbid_cached_property_on = vec!["Model".into()];
    let diags = check(MUT201_SRC, &config);
    let by_fn: BTreeSet<(String, u32)> = diags
        .iter()
        .filter(|d| d.rule_code == "MUT201")
        .map(|d| (d.qualname.clone(), d.line))
        .collect();
    let expected: BTreeSet<(String, u32)> = [
        ("public_helper", 23),
        ("bad_direct", 26),
        ("bad_direct", 27),
        ("bad_direct", 28),
        ("bad_direct", 29),
        ("bad_sub", 32),
        ("calls_shared", 35),
        ("Guarded.size", 44),
    ]
    .into_iter()
    .map(|(a, b)| (a.to_owned(), b))
    .collect();
    // Not reported: Store.add and `_owned_helper` (only called by the owner), the private
    // `_shared_helper` itself (its non-owner caller is reported), `calls_public` (the public
    // helper is reported instead), and `Model.__init__` / `builds()` (initialization).
    assert_eq!(by_fn, expected, "{diags:#?}");
    let shared = diags.iter().find(|d| d.qualname == "calls_shared").unwrap();
    assert!(
        shared.message.contains("via `app._shared_helper`"),
        "{}",
        shared.message
    );
    assert_eq!(
        shared.related[0].message,
        "`calls_shared` calls `app._shared_helper`"
    );
}

#[test]
fn mut201_glob_writers_and_bad_targets() {
    let diags = check(MUT201_SRC, &owners(&["app.Model.fields"], &["app.*"]));
    assert!(!codes(&diags).contains(&"MUT201"));
    let built = build("app.py", "x = 1\n");
    let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
    let unit = ModuleUnit {
        input: ModuleInput {
            path: PathBuf::from("app.py"),
            module_name: built.module_name.clone(),
            is_package: false,
            content_hash: String::new(),
            semantic: &built.semantic,
            ir: &built.ir,
        },
        source: &built.src,
        rel_path: "app.py".into(),
        lines: LineIndex::from_source(&built.src),
    };
    let index = analyze_project(
        &[ModuleInput {
            path: PathBuf::from("app.py"),
            module_name: built.module_name.clone(),
            is_package: false,
            content_hash: String::new(),
            semantic: &built.semantic,
            ir: &built.ir,
        }],
        &kb,
    );
    let config = owners(&["nodot"], &[]);
    let sel = RuleSelection::resolve(&config.lint, &[], &[], &[]).unwrap();
    assert!(matches!(
        check_project(
            std::slice::from_ref(&unit),
            &config,
            &kb,
            &index,
            &sel,
            &CheckExtras {
                soundness_kb: &EffectiveSoundnessKb::load_builtin(PythonVersion::PY315).unwrap(),
                snapshot: None,
        decls: None,
            }
        ),
        Err(RuleConfigError::InvalidTarget { section: "write-owners", target: t }) if t == "nodot"
    ));
}

// ============================================================================ MUT202

const MUT202_SRC: &str = r#"
import copy
from functools import cache, lru_cache

@lru_cache(maxsize=None)
def schema():
    return {"fields": [1]}

@cache
def pair():
    return ([], [])

def shallow_registry():
    return {"a": []}

def mutate_param(xs):
    xs.append(1)

def tp_nested():
    schema()["fields"].append(2)

def tp_top():
    s = schema()
    s["new"] = 1

def tp_unpack():
    a, b = pair()
    a.append(1)

def tp_helper():
    mutate_param(schema()["fields"])

def tp_iter():
    for v in schema().values():
        v.append(1)

def tn_deepcopy():
    s = copy.deepcopy(schema())
    s["fields"].append(1)

def tn_shallow_top():
    s = schema().copy()
    s["new"] = 1

def tp_shallow_nested():
    s = schema().copy()
    s["fields"].append(1)

def tn_config_shallow():
    r = shallow_registry()
    r["a"].append(1)

def tp_config_shallow_top():
    r = shallow_registry()
    r["b"] = []
"#;

#[test]
fn mut202_producers_propagation_and_sanitizers() {
    let config = HekiConfig {
        readonly_returns: vec![ReadOnlyReturnRule {
            name: "registry".into(),
            producers: vec!["shallow_registry".into()],
            deep: false,
        }],
        ..HekiConfig::default()
    };
    let diags = check(MUT202_SRC, &config);
    let fns: BTreeSet<&str> = diags
        .iter()
        .filter(|d| d.rule_code == "MUT202")
        .map(|d| d.qualname.as_str())
        .collect();
    assert_eq!(
        fns,
        BTreeSet::from([
            "tp_nested",
            "tp_top",
            "tp_unpack",
            "tp_helper",
            "tp_iter",
            "tp_shallow_nested",
            "tp_config_shallow_top",
        ]),
        "{diags:#?}"
    );
    let helper = diags.iter().find(|d| d.qualname == "tp_helper").unwrap();
    assert_eq!(helper.target, "app.schema");
    assert_eq!(helper.related.len(), 1);
}

#[test]
fn mut202_model_copy_deep_flag() {
    let src = r#"
from functools import lru_cache
from pydantic import BaseModel

class M(BaseModel):
    items: list[int]

@lru_cache
def shared() -> M:
    return M(items=[])

def deep_copy_ok():
    m = shared().model_copy(deep=True)
    m.items.append(1)

def shallow_copy_bad():
    m = shared().model_copy()
    m.items.append(1)

def shallow_top_ok():
    m = shared().model_copy(deep=False)
    m.items = []
"#;
    let diags = check(src, &HekiConfig::default());
    let fns: Vec<&str> = diags.iter().map(|d| d.qualname.as_str()).collect();
    assert_eq!(fns, vec!["shallow_copy_bad"], "{diags:#?}");
}

// ============================================================================ EFF101 / EFF105

const EFF101_SRC: &str = r#"
import asyncio
import contextlib
import time
import click

def blocking():
    time.sleep(1)

def read_file(p):
    return open(p).read()

async def bad():
    blocking()

async def bad_fs(p):
    read_file(p)

async def ok_offloaded():
    await asyncio.to_thread(blocking)

@click.command()
async def cli_entry():
    blocking()

@contextlib.asynccontextmanager
async def lifespan(app):
    blocking()
    yield

async def test_something():
    blocking()
"#;

#[test]
fn eff101_relaxations_and_eff105_opt_in() {
    let diags = check(EFF101_SRC, &HekiConfig::default());
    assert_eq!(
        diags
            .iter()
            .map(|d| (d.rule_code.as_str(), d.qualname.as_str()))
            .collect::<Vec<_>>(),
        vec![("EFF101", "bad")],
        "{diags:#?}"
    );
    let diags = check_with(
        &[("app.py", EFF101_SRC)],
        &HekiConfig::default(),
        &["EFF101", "EFF105"],
    );
    let mut got: Vec<(&str, &str)> = diags
        .iter()
        .map(|d| (d.rule_code.as_str(), d.qualname.as_str()))
        .collect();
    got.sort();
    assert_eq!(got, vec![("EFF101", "bad"), ("EFF105", "bad_fs")]);

    let mut config = HekiConfig::default();
    config.lint.relax_async_in_tests = Some(false);
    let names: BTreeSet<String> = check(EFF101_SRC, &config)
        .into_iter()
        .map(|d| d.qualname)
        .collect();
    assert_eq!(
        names,
        BTreeSet::from(["bad".to_owned(), "test_something".to_owned()])
    );
}

#[test]
fn eff101_root_cause_grouping() {
    let mut src = String::from("import time\n\ndef c():\n    time.sleep(1)\n");
    for i in 0..11 {
        src.push_str(&format!("\nasync def h{i:02}():\n    c()\n"));
    }
    let diags = check(&src, &HekiConfig::default());
    assert_eq!(diags.len(), 1);
    assert!(
        diags[0]
            .message
            .ends_with("(+10 more async call sites reaching the same blocking root cause)"),
        "{}",
        diags[0].message
    );
}

// ============================================================================ EFF102 / EFF103

const LOOP_SRC: &str = r#"
import sqlite3
import requests
from sqlalchemy.ext.asyncio import AsyncSession

def make_client():
    return requests.Session()

def tp_direct(urls):
    for u in urls:
        requests.get(u)

def tp_indirect(urls):
    for u in urls:
        make_client()

def tn_const_small():
    for _ in range(3):
        requests.get("u")

def tn_retry(fn):
    for attempt in range(3):
        try:
            return requests.get("u")
        except Exception:
            pass

def tn_iter_header():
    for item in requests.get("u").json():
        print(item)

def tn_local_dict(keys):
    requests = {}
    for k in keys:
        requests.get(k)

def tp_query(cur: sqlite3.Cursor, ids):
    for i in ids:
        cur.execute("SELECT * FROM t WHERE id = ?", (i,))

def tn_ddl(cur: sqlite3.Cursor, tables):
    for t in tables:
        cur.execute("create table x (a int)")

def upgrade(cur: sqlite3.Cursor, ids):
    for i in ids:
        cur.execute("UPDATE t SET a = 1")

async def tp_async_query(db, ids):
    s: AsyncSession = db
    for i in ids:
        await s.execute("q")
"#;

#[test]
fn eff102_eff103_loop_rules() {
    let diags = check(LOOP_SRC, &HekiConfig::default());
    let got: BTreeSet<(&str, &str)> = diags
        .iter()
        .map(|d| (d.rule_code.as_str(), d.qualname.as_str()))
        .collect();
    assert_eq!(
        got,
        BTreeSet::from([
            ("EFF102", "tp_direct"),
            ("EFF102", "tp_indirect"),
            ("EFF103", "tp_query"),
            ("EFF103", "tp_async_query"),
        ]),
        "{diags:#?}"
    );
    let indirect = diags.iter().find(|d| d.qualname == "tp_indirect").unwrap();
    assert_eq!(indirect.target, "requests.Session");
    assert_eq!(indirect.related.len(), 2);
    // Migration files are excluded from EFF103.
    let diags = check_with(
        &[("app/migrations/0001.py", LOOP_SRC)],
        &HekiConfig::default(),
        &["EFF103"],
    );
    assert!(diags.is_empty(), "{diags:#?}");
}

// ============================================================================ DATA101

#[test]
fn data101_requires_self_accumulation() {
    let src = r#"
import numpy as np
import pandas as pd

def bad(chunks):
    df = pd.DataFrame()
    for c in chunks:
        df = pd.concat([df, c])
    return df

def bad_np(rows):
    a = np.zeros(0)
    for r in rows:
        if r:
            a = np.append(a, r)
    return a

def oneshot(pairs):
    out = []
    for a, b in pairs:
        out.append(pd.concat([a, b]))
    return out

def after_loop(chunks):
    parts = []
    for c in chunks:
        parts.append(c)
    return pd.concat(parts)
"#;
    let diags = check(src, &HekiConfig::default());
    let got: Vec<(&str, &str, &str)> = diags
        .iter()
        .map(|d| (d.rule_code.as_str(), d.qualname.as_str(), d.target.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![("DATA101", "bad", "df"), ("DATA101", "bad_np", "a")],
        "{diags:#?}"
    );
}

// ============================================================================ selection

#[test]
fn per_file_ignores_and_severity() {
    let mut config = HekiConfig::default();
    config
        .lint
        .per_file_ignores
        .insert("legacy/**".into(), vec!["EFF".into()]);
    config
        .lint
        .severity
        .insert("async-blocking-call".into(), Severity::Warning);
    let src = "import time\n\nasync def f():\n    time.sleep(1)\n";
    let diags = check_with(&[("legacy/a.py", src), ("new/b.py", src)], &config, &[]);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].path, PathBuf::from("new/b.py"));
    assert_eq!(diags[0].severity, Severity::Warning);
    assert_eq!((diags[0].line, diags[0].column), (4, 5));
}

// ============================================================================ stale indexes

fn unit_of(b: &Built) -> ModuleUnit<'_> {
    ModuleUnit {
        input: ModuleInput {
            path: PathBuf::from(&b.rel),
            module_name: b.module_name.clone(),
            is_package: b.is_package,
            content_hash: String::new(),
            semantic: &b.semantic,
            ir: &b.ir,
        },
        source: &b.src,
        rel_path: b.rel.clone(),
        lines: LineIndex::from_source(&b.src),
    }
}

/// Check `checked` against an index of `indexed` while resolving names with the
/// declarations of `checked` itself: the LSP engine's situation when its index does not have
/// a document in this form yet (declarations are kept up to date separately). Every
/// summary the rules miss must be skipped, never assumed.
fn check_against(checked: &[Built], indexed: &[Built], select: &[&str]) -> Vec<Diagnostic> {
    let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
    let units: Vec<ModuleUnit<'_>> = checked.iter().map(unit_of).collect();
    let index = analyze_project(
        &indexed.iter().map(|b| unit_of(b).input).collect::<Vec<_>>(),
        &kb,
    );
    let decls = analyze_project(
        &units.iter().map(|u| unit_of_input(u)).collect::<Vec<_>>(),
        &kb,
    )
    .project_decls();
    let select: Vec<String> = select.iter().map(|s| (*s).to_owned()).collect();
    let config = HekiConfig::default();
    let selection = RuleSelection::resolve(&config.lint, &select, &[], &[]).unwrap();
    let soundness_kb = EffectiveSoundnessKb::load_builtin(PythonVersion::PY315).unwrap();
    let extras = CheckExtras {
        soundness_kb: &soundness_kb,
        snapshot: None,
        decls: Some(&decls),
    };
    check_project(&units, &config, &kb, &index, &selection, &extras).unwrap()
}

fn unit_of_input<'a>(u: &ModuleUnit<'a>) -> ModuleInput<'a> {
    ModuleInput {
        path: u.input.path.clone(),
        module_name: u.input.module_name.clone(),
        is_package: u.input.is_package,
        content_hash: String::new(),
        semantic: u.input.semantic,
        ir: u.input.ir,
    }
}

#[test]
fn mut101_skips_methods_a_stale_index_does_not_summarize() {
    let edited = "from functools import cached_property\n\nclass C:\n    @cached_property\n    def total(self):\n        return len(self.items)\n\n    def add(self, x):\n        self.items.append(x)\n\n    def extend(self, xs):\n        self.items.extend(xs)\n\n    def drop(self):\n        self.items.clear()\n";
    // The index knows `C.total` from the old `app.py` and `C.add` only from a stub of the
    // same module; `C.extend` and `C.drop` are new.
    let old = "from functools import cached_property\n\nclass C:\n    @cached_property\n    def total(self):\n        return len(self.items)\n";
    let stub = "class C:\n    def add(self, x): ...\n";
    let diags = check_against(
        &[build("app.py", edited)],
        &[build("app.py", old), build("app.pyi", stub)],
        &["MUT101"],
    );
    assert!(diags.is_empty(), "{diags:#?}");
    // Once indexed, the same check reports all three writes.
    let diags = check_against(
        &[build("app.py", edited)],
        &[build("app.py", edited)],
        &["MUT101"],
    );
    assert_eq!(
        diags.iter().map(|d| d.line).collect::<Vec<_>>(),
        vec![9, 12, 15]
    );
}

#[test]
fn mut202_skips_callees_a_stale_index_does_not_summarize() {
    let edited = "from functools import lru_cache\n\n@lru_cache\ndef load():\n    return []\n\ndef mutate(items):\n    items.append(1)\n\ndef use():\n    mutate(load())\n";
    let old = "from functools import lru_cache\n\n@lru_cache\ndef load():\n    return []\n";
    let diags = check_against(
        &[build("app.py", edited)],
        &[build("app.py", old)],
        &["MUT202"],
    );
    assert!(diags.is_empty(), "{diags:#?}");
    let diags = check_against(
        &[build("app.py", edited)],
        &[build("app.py", edited)],
        &["MUT202"],
    );
    assert_eq!(codes(&diags), vec!["MUT202"]);
}

#[test]
fn data101_skips_loop_facts_without_their_loop() {
    // `loop_carried` entries name loops of the same function; one without its loop (an IR
    // inconsistency) is skipped instead of being matched against another loop.
    let src = "import pandas as pd\n\ndef bad(chunks):\n    df = pd.DataFrame()\n    for c in chunks:\n        df = pd.concat([df, c])\n    return df\n";
    let mut b = build("app.py", src);
    let checked = [build("app.py", src)];
    assert_eq!(
        codes(&check_against(&checked, &checked, &["DATA101"])),
        vec!["DATA101"]
    );
    for f in &mut b.ir.functions {
        f.loops.clear();
    }
    let indexed = [build("app.py", src)];
    assert!(check_against(&[b], &indexed, &["DATA101"]).is_empty());
}
