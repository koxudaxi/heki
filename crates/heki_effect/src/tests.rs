//! `heki_effect` tests. The five gates of the effect engine (milestone M4) are
//! marked `gate_1` .. `gate_5`; the rest pin the resolver and local-extraction rules.

use super::*;
use ExecEffectKind::*;
use heki_ast::parse_module;
use heki_env::{PythonVersion, RuntimeProfile};
use heki_ir::build_module_ir;
use heki_kb::EffectiveEcosystemKb;
use heki_semantic::analyze_module;
use std::path::Path;

struct Built {
    path: PathBuf,
    module_name: String,
    is_package: bool,
    semantic: SemanticModel,
    ir: ModuleIr,
    hash: String,
}

fn build(rel: &str, src: &str) -> Built {
    let (module_name, is_package) = module_name_for_path(Path::new(rel)).unwrap();
    let parsed = parse_module(src).expect("valid python");
    let profile = RuntimeProfile::from(PythonVersion::PY315);
    let semantic = analyze_module(&parsed, src, profile);
    let ir = build_module_ir(&parsed, src, &semantic, profile);
    Built {
        path: PathBuf::from(rel),
        module_name,
        is_package,
        semantic,
        ir,
        hash: format!("{}", src.len()),
    }
}

fn input(b: &Built) -> ModuleInput<'_> {
    ModuleInput {
        path: b.path.clone(),
        module_name: b.module_name.clone(),
        is_package: b.is_package,
        root_is_package: false,
        content_hash: b.hash.clone(),
        semantic: &b.semantic,
        ir: &b.ir,
    }
}

fn kb() -> EffectiveEcosystemKb {
    EffectiveEcosystemKb::load_builtin(&[]).unwrap()
}

fn project(files: &[(&str, &str)]) -> ProjectIndex {
    let built: Vec<Built> = files.iter().map(|(rel, src)| build(rel, src)).collect();
    let inputs: Vec<ModuleInput<'_>> = built.iter().map(input).collect();
    analyze_project(&inputs, &kb())
}

fn key(module: &str, qualname: &str) -> FuncKey {
    FuncKey {
        module: module.into(),
        qualname: qualname.into(),
    }
}

fn summary<'a>(index: &'a ProjectIndex, module: &str, qualname: &str) -> &'a EffectSummary {
    index
        .summary(&key(module, qualname))
        .unwrap_or_else(|| panic!("no summary for {module}.{qualname}"))
}

fn chain_callees(s: &EffectSummary, e: ExecEffectKind) -> Vec<String> {
    s.exec_chains
        .get(&e)
        .unwrap_or_else(|| panic!("no chain for {e:?}: {:?}", s.exec_chains))
        .iter()
        .map(|h| h.callee.clone())
        .collect()
}

fn set<T: Ord + Clone>(items: &[T]) -> BTreeSet<T> {
    items.iter().cloned().collect()
}

// =================================================================== gate 1: 2-5 hop chains

const NET_PY: &str = r#"
import time
import requests

def fetch(url):
    return requests.get(url)

def nap():
    time.sleep(1)

def ping(n):
    if n:
        return pong(n - 1)

def pong(n):
    time.sleep(0.1)
    return ping(n)
"#;

const SVC_PY: &str = r#"
from pkg.net import fetch, nap
from . import net

def h1(u):
    return fetch(u)

def h2(u):
    return h1(u)

def h3(u):
    return h2(u)

def h4(u):
    nap()
    return h3(u)

def h5(u):
    return h4(u)

def both(u):
    h3(u)
    return net.fetch(u)
"#;

#[test]
fn gate_1_indirect_blocking_net_and_sleep_with_shortest_chains() {
    let index = project(&[
        ("pkg/__init__.py", ""),
        ("pkg/net.py", NET_PY),
        ("pkg/svc.py", SVC_PY),
    ]);

    let fetch = summary(&index, "pkg.net", "fetch");
    assert_eq!(
        fetch.direct_exec_effects,
        set(&[BlockingNet, UnpooledClientAlloc])
    );
    assert_eq!(chain_callees(fetch, BlockingNet), vec!["requests.get"]);

    // 2 hops (h1) .. 5 project hops (h5), across modules.
    for (name, hops) in [("h1", 1), ("h2", 2), ("h3", 3), ("h4", 4), ("h5", 5)] {
        let s = summary(&index, "pkg.svc", name);
        assert!(s.direct_exec_effects.is_empty(), "{name}");
        assert!(s.transitive_exec_effects.contains(&BlockingNet), "{name}");
        assert_eq!(chain_callees(s, BlockingNet).len(), hops + 1, "{name}");
    }
    let h5 = summary(&index, "pkg.svc", "h5");
    assert_eq!(
        chain_callees(h5, BlockingNet),
        vec![
            "pkg.svc.h4",
            "pkg.svc.h3",
            "pkg.svc.h2",
            "pkg.svc.h1",
            "pkg.net.fetch",
            "requests.get"
        ]
    );
    assert_eq!(
        chain_callees(h5, Sleep),
        vec!["pkg.svc.h4", "pkg.net.nap", "time.sleep"]
    );
    let chain = &h5.exec_chains[&Sleep];
    assert_eq!(chain[0].caller, key("pkg.svc", "h5"));
    assert_eq!(chain[0].path, PathBuf::from("pkg/svc.py"));
    assert_eq!(chain[2].path, PathBuf::from("pkg/net.py"));

    // The shortest of two chains wins (`net.fetch` via `from . import net`, not via h3).
    let both = summary(&index, "pkg.svc", "both");
    assert_eq!(
        chain_callees(both, BlockingNet),
        vec!["pkg.net.fetch", "requests.get"]
    );

    // Recursion (one SCC) converges and both members see the effect.
    let ping = summary(&index, "pkg.net", "ping");
    assert_eq!(
        chain_callees(ping, Sleep),
        vec!["pkg.net.pong", "time.sleep"]
    );
    assert!(
        summary(&index, "pkg.net", "pong")
            .direct_exec_effects
            .contains(&Sleep)
    );
}

// =================================================================== gate 2: offload wrappers

const OFFLOAD_PY: &str = r#"
import asyncio
import time

def blocking():
    time.sleep(1)

def in_thread(fn, *args):
    return asyncio.to_thread(fn, *args)

def in_thread_twice(func):
    return in_thread(func)

def submit(loop: asyncio.AbstractEventLoop, fn):
    return loop.run_in_executor(None, fn)

async def handler():
    await in_thread(blocking)
    await in_thread_twice(blocking)
    await asyncio.to_thread(blocking)

async def bad():
    blocking()
"#;

#[test]
fn gate_2_in_thread_wrapper_offload_blocks_propagation() {
    let index = project(&[("app.py", OFFLOAD_PY)]);
    assert_eq!(
        summary(&index, "app", "in_thread").offloads_blocking_params,
        0b1
    );
    // Propagated through the user wrapper's wrapper.
    assert_eq!(
        summary(&index, "app", "in_thread_twice").offloads_blocking_params,
        0b1
    );
    // `loop: asyncio.AbstractEventLoop` (Priority 2) -> `run_in_executor` offloads arg 1 = `fn`.
    assert_eq!(
        summary(&index, "app", "submit").offloads_blocking_params,
        0b10
    );

    let handler = summary(&index, "app", "handler");
    assert!(!handler.transitive_exec_effects.contains(&Sleep));
    assert!(handler.direct_exec_effects.is_empty());

    let reports = boundary_reports(&index);
    let funcs: Vec<&str> = reports
        .iter()
        .map(|r| r.function.qualname.as_str())
        .collect();
    assert_eq!(funcs, vec!["bad"]);
    assert_eq!(reports[0].root_cause, "time.sleep");
}

// =================================================================== gate 3: boundary-only

const BOUNDARY_PY: &str = r#"
import time

def c():
    time.sleep(1)

async def b():
    c()

async def a():
    await b()

async def d():
    b()
"#;

#[test]
fn gate_3_boundary_only_reports_first_sync_crossing() {
    let index = project(&[("flow.py", BOUNDARY_PY)]);
    let reports = boundary_reports(&index);
    assert_eq!(reports.len(), 1, "{reports:#?}");
    let r = &reports[0];
    assert_eq!(r.function, key("flow", "b"));
    assert_eq!(r.effect, Sleep);
    assert_eq!(r.root_cause, "time.sleep");
    let callees: Vec<&str> = r.chain.iter().map(|h| h.callee.as_str()).collect();
    assert_eq!(callees, vec!["flow.c", "time.sleep"]);

    // `a` does block transitively (it awaits `b`) but is not reported.
    assert!(
        summary(&index, "flow", "a")
            .transitive_exec_effects
            .contains(&Sleep)
    );
    // `d` only creates the coroutine: nothing runs there.
    assert!(
        !summary(&index, "flow", "d")
            .transitive_exec_effects
            .contains(&Sleep)
    );
}

const ASYNC_FILTERS_PY: &str = r#"
import asyncio
import time
import requests
from sqlalchemy.ext.asyncio import AsyncSession

async def awaited_sync_api(s: requests.Session):
    await s.get("u")

async def awaited_async_db(db: AsyncSession):
    await db.execute("select 1")

async def plain_blocking():
    requests.get("u")

async def fine():
    await asyncio.sleep(1)

async def _drive_me():
    time.sleep(1)

def runner():
    coro = _drive_me()
    try:
        coro.send(None)
    except StopIteration:
        pass

async def drive_public():
    time.sleep(1)

def public_runner():
    coro = drive_public()
    coro.send(None)
"#;

#[test]
fn awaited_filter_keeps_loop_effects_and_sync_driven_only_relaxes() {
    let index = project(&[("af.py", ASYNC_FILTERS_PY)]);
    // Stage 0-A filter: awaited calls drop blocking effects ...
    assert!(
        summary(&index, "af", "awaited_sync_api")
            .direct_exec_effects
            .is_empty()
    );
    // ... but keep loop-sensitive ones.
    assert_eq!(
        summary(&index, "af", "awaited_async_db").direct_exec_effects,
        set(&[DbQuery])
    );
    assert!(summary(&index, "af", "fine").direct_exec_effects.is_empty());

    let drive = summary(&index, "af", "_drive_me");
    assert!(drive.sync_driven_only);
    assert!(!summary(&index, "af", "drive_public").sync_driven_only);
    // The manual driver really runs the coroutine body.
    assert!(
        summary(&index, "af", "runner")
            .transitive_exec_effects
            .contains(&Sleep)
    );

    let funcs: BTreeSet<String> = boundary_reports(&index)
        .into_iter()
        .map(|r| r.function.qualname)
        .collect();
    assert_eq!(
        funcs,
        set(&["drive_public".to_owned(), "plain_blocking".to_owned()])
    );
}

// =================================================================== gate 4: param paths

const MODEL_PY: &str = r#"
class DataModel:
    def __init__(self):
        self.fields: list[str] = []

    def add(self, name):
        _sneaky_helper(self.fields, name)

def _sneaky_helper(items, name):
    items.append(name)

def _attr_helper(model, name):
    model.fields.append(name)

def forward(xs, name):
    _sneaky_helper(xs, name)
"#;

const USE_PY: &str = r#"
from pkg.model import DataModel, _attr_helper, _sneaky_helper

def outer(model: DataModel):
    _sneaky_helper(model.fields, "c")

def outer2(model: DataModel):
    _attr_helper(model, "c")

def outer3(holder):
    _sneaky_helper(holder.fields, "c")

def outer4(model: DataModel):
    _sneaky_helper(items=model.fields, name="c")
"#;

#[test]
fn gate_4_mutated_param_paths_cross_function_container_mutation() {
    let index = project(&[
        ("pkg/__init__.py", ""),
        ("pkg/model.py", MODEL_PY),
        ("pkg/use.py", USE_PY),
    ]);
    let helper = summary(&index, "pkg.model", "_sneaky_helper");
    assert_eq!(
        helper.mutated_param_paths,
        BTreeMap::from([(0, set(&[String::new()]))])
    );
    assert_eq!(helper.mutates_params, 0b1);
    assert_eq!(helper.escapes_params, 0b10);

    let forward = summary(&index, "pkg.model", "forward");
    assert_eq!(
        forward.mutated_param_paths,
        BTreeMap::from([(0, set(&[String::new()]))])
    );
    assert_eq!(forward.mutates_params, 0b1);
    assert_eq!(forward.escapes_params, 0b10);

    let attr_helper = summary(&index, "pkg.model", "_attr_helper");
    assert_eq!(
        attr_helper.mutated_param_paths,
        BTreeMap::from([(0, set(&["fields".to_owned()]))])
    );

    let indirect = |s: &EffectSummary| -> Vec<(String, String, Vec<String>)> {
        s.typed_field_write_sites
            .iter()
            .filter(|t| t.kind == WriteKind::IndirectHelper)
            .map(|t| {
                (
                    t.class_qualname.clone(),
                    t.attr.clone(),
                    t.via_chain.iter().map(|h| h.callee.clone()).collect(),
                )
            })
            .collect()
    };
    let expected = |helper: &str| {
        vec![(
            "pkg.model.DataModel".to_owned(),
            "fields".to_owned(),
            vec![format!("pkg.model.{helper}")],
        )]
    };
    let outer = summary(&index, "pkg.use", "outer");
    assert_eq!(indirect(outer), expected("_sneaky_helper"));
    assert_eq!(
        outer.mutated_param_paths,
        BTreeMap::from([(0, set(&["fields".to_owned()]))])
    );
    assert_eq!(
        indirect(summary(&index, "pkg.use", "outer2")),
        expected("_attr_helper")
    );
    // Keyword arguments bind by parameter name.
    assert_eq!(
        indirect(summary(&index, "pkg.use", "outer4")),
        expected("_sneaky_helper")
    );

    let outer3 = summary(&index, "pkg.use", "outer3");
    assert_eq!(
        outer3.mutated_param_paths,
        BTreeMap::from([(0, set(&["fields".to_owned()]))])
    );
    assert!(outer3.typed_field_write_sites.is_empty());

    let add = summary(&index, "pkg.model", "DataModel.add");
    assert!(add.mutates_self_attrs.contains(&AttrKeyPath {
        attr: "fields".into(),
        const_key: None
    }));
}

const HANDS_PY: &str = r#"
class Model:
    def __init__(self):
        self.fields: list[str] = []

def _private_writer(model: Model):
    model.fields = []

def public_writer(model: Model):
    model.fields = []

def top(model: Model):
    _private_writer(model)
    public_writer(model)

def untyped(model):
    model.fields.append("x")

def hands_over(model: Model):
    untyped(model)

def above(model: Model):
    hands_over(model)
"#;

#[test]
fn private_callees_hand_write_sites_to_callers_public_ones_keep_them() {
    let index = project(&[("hands.py", HANDS_PY)]);
    let sites = |q: &str| -> Vec<(WriteKind, Vec<String>)> {
        summary(&index, "hands", q)
            .typed_field_write_sites
            .iter()
            .map(|t| {
                (
                    t.kind,
                    t.via_chain.iter().map(|h| h.callee.clone()).collect(),
                )
            })
            .collect()
    };
    // The private helper's write reaches `top`; the public function reports its own.
    assert_eq!(
        sites("top"),
        vec![(WriteKind::DirectAttr, vec!["hands._private_writer".into()])]
    );
    assert_eq!(
        sites("public_writer"),
        vec![(WriteKind::DirectAttr, vec![])]
    );
    // The facts of both writes reach `top` all the same.
    let top = summary(&index, "hands", "top");
    assert_eq!(top.typed_field_writes.len(), 1);
    assert!(top.typed_field_writes.iter().all(|t| t.attr == "fields"));
    // An untyped helper's change is the caller's `IndirectHelper` write; a caller above it
    // knows the field is written (no second `IndirectHelper`) but gets no site from a public
    // callee.
    assert_eq!(
        sites("hands_over"),
        vec![(WriteKind::IndirectHelper, vec!["hands.untyped".into()])]
    );
    assert!(sites("above").is_empty());
    assert!(
        summary(&index, "hands", "above")
            .typed_field_writes
            .iter()
            .any(|t| t.kind == WriteKind::IndirectHelper)
    );
}

const CYCLE_PY: &str = r#"
class Model:
    def __init__(self):
        self.fields: list[str] = []


def _ping(m: Model, n):
    m.fields = []
    if n:
        _pong(m, n - 1)


def _pong(m: Model, n):
    _ping(m, n)


def entry(m: Model):
    _ping(m, 1)
"#;

#[test]
fn a_write_handed_around_a_cycle_of_private_helpers_stays_with_its_writer() {
    let index = project(&[("cycle.py", CYCLE_PY)]);
    let chains = |q: &str| -> Vec<Vec<String>> {
        summary(&index, "cycle", q)
            .typed_field_write_sites
            .iter()
            .map(|t| t.via_chain.iter().map(|h| h.callee.clone()).collect())
            .collect()
    };
    // `_ping` keeps its own write once; `_pong` and `entry` get it through the cycle.
    assert_eq!(chains("_ping"), vec![Vec::<String>::new()]);
    assert_eq!(chains("_pong"), vec![vec!["cycle._ping".to_owned()]]);
    assert_eq!(chains("entry"), vec![vec!["cycle._ping".to_owned()]]);
}

// =================================================================== gate 5: Early Cutoff

const B_PY: &str = r#"
from pkg.a import helper

def caller():
    return helper()

def top():
    return caller()
"#;

fn reanalyze(index: &mut ProjectIndex, rel: &str, src: &str) -> IncrementalUpdate {
    let b = build(rel, src);
    index.reanalyze_module(&input(&b), &kb())
}

#[test]
fn gate_5_update_file_early_cutoff() {
    let mut index = project(&[
        ("pkg/__init__.py", ""),
        ("pkg/a.py", "def helper():\n    return 1\n"),
        ("pkg/b.py", B_PY),
    ]);
    assert!(
        summary(&index, "pkg.b", "top")
            .transitive_exec_effects
            .is_empty()
    );

    // Edit that moves code but keeps every summary: Early Cutoff.
    let res = reanalyze(
        &mut index,
        "pkg/a.py",
        "# a comment shifting every offset\n\n\ndef helper():\n    return 1\n",
    );
    assert!(!res.summary_changed);
    assert!(res.affected_callers.is_empty());

    // Summary change: only then are reverse-call-graph callers notified and re-propagated.
    let res = reanalyze(
        &mut index,
        "pkg/a.py",
        "import time\n\ndef helper():\n    time.sleep(1)\n    return 1\n",
    );
    assert!(res.summary_changed);
    assert_eq!(
        res.affected_callers,
        vec![key("pkg.b", "caller"), key("pkg.b", "top")]
    );
    assert_eq!(
        chain_callees(summary(&index, "pkg.b", "top"), Sleep),
        vec!["pkg.b.caller", "pkg.a.helper", "time.sleep"]
    );

    // Removing the effect again also reaches the callers (summaries may shrink).
    let res = reanalyze(&mut index, "pkg/a.py", "def helper():\n    return 2\n");
    assert!(res.summary_changed);
    assert_eq!(
        res.affected_callers,
        vec![key("pkg.b", "caller"), key("pkg.b", "top")]
    );
    assert!(
        summary(&index, "pkg.b", "top")
            .transitive_exec_effects
            .is_empty()
    );
}

/// `sync_driven_only` is decided by a function's callers: re-propagating a function after an
/// edit of a callee or of its own module keeps what they decided, and an edit that decides it
/// again unchanged is no change.
#[test]
fn gate_5_edits_keep_what_callers_decided_about_a_coroutine() {
    let leaf = "import time\n\ndef block():\n    time.sleep(1)\n";
    let worker = "from pkg.leaf import block\n\nasync def _work():\n    block()\n";
    let mut index = project(&[
        ("pkg/__init__.py", ""),
        ("pkg/leaf.py", leaf),
        ("pkg/worker.py", worker),
        (
            "pkg/driver.py",
            "from pkg.worker import _work\n\ndef drive():\n    _work().send(None)\n",
        ),
    ]);
    assert!(summary(&index, "pkg.worker", "_work").sync_driven_only);
    // The callee's blocking call moves: the coroutine copies the new position.
    let res = reanalyze(
        &mut index,
        "pkg/leaf.py",
        &leaf.replace("    time.sleep", "\n    time.sleep"),
    );
    assert!(!res.summary_changed);
    assert!(summary(&index, "pkg.worker", "_work").sync_driven_only);
    let res = reanalyze(&mut index, "pkg/worker.py", &format!("# workers\n{worker}"));
    assert!(!res.summary_changed);
    assert!(res.changed.is_empty());
    assert!(summary(&index, "pkg.worker", "_work").sync_driven_only);
}

#[test]
fn gate_5_cross_file_cycle_does_not_keep_stale_effects() {
    let mut index = project(&[
        ("pkg/__init__.py", ""),
        (
            "pkg/a.py",
            "import time\nfrom pkg import b\n\ndef helper(n):\n    time.sleep(1)\n    return b.caller(n)\n",
        ),
        (
            "pkg/b.py",
            "from pkg import a\n\ndef caller(n):\n    return a.helper(n)\n",
        ),
    ]);
    assert!(
        summary(&index, "pkg.b", "caller")
            .transitive_exec_effects
            .contains(&Sleep)
    );
    let res = reanalyze(
        &mut index,
        "pkg/a.py",
        "from pkg import b\n\ndef helper(n):\n    return b.caller(n)\n",
    );
    assert!(res.summary_changed);
    assert!(
        summary(&index, "pkg.a", "helper")
            .transitive_exec_effects
            .is_empty()
    );
    assert!(
        summary(&index, "pkg.b", "caller")
            .transitive_exec_effects
            .is_empty()
    );
}

#[test]
fn gate_5_handbuilt_file_summaries() {
    let f_helper = key("a", "helper");
    let f_caller = key("b", "caller");
    let mut file_a = FileSummary {
        path: PathBuf::from("a.py"),
        module_name: "a".into(),
        content_hash: "h1".into(),
        ..Default::default()
    };
    file_a
        .local_functions
        .insert(f_helper.clone(), EffectSummary::default().into());
    let mut index = ProjectIndex::build(vec![file_a.clone()]);

    let mut file_b = FileSummary {
        path: PathBuf::from("b.py"),
        module_name: "b".into(),
        content_hash: "h2".into(),
        ..Default::default()
    };
    file_b
        .local_functions
        .insert(f_caller.clone(), EffectSummary::default().into());
    let span = Span { start: 0, end: 1 };
    file_b.call_sites.push(CallSite {
        caller: f_caller.clone(),
        target: CallTarget::Project(vec![BoundCallee {
            callee: f_helper.clone(),
            params: Vec::new(),
        }]),
        is_awaited: false,
        span,
        loop_depth: 0,
        recv_is_self: false,
        drives_coroutine_manually: false,
        constructs: None,
        role: crate::SiteRole::Call,
        returned: None,
        flow: None,
        guards: Vec::new(),
        once_per: None,
        once_loops: 0,
        runner: crate::Runner::Caller,
        binds_recv: false,
        handed_callable: None,
        per_item: None,
        dispatch: None,
        result_discarded: false,
        queues: None,
        result_held: false,
    });
    index.update_file(file_b);

    // Re-edit file_a without changing helper's EffectSummary -> Early Cutoff!
    file_a.content_hash = "h1_edited".into();
    let res = index.update_file(file_a.clone());
    assert!(!res.summary_changed);
    assert!(res.affected_callers.is_empty());

    // Now add a BlockingNet effect to helper -> caller is affected!
    let mut sum = EffectSummary::default();
    sum.direct_exec_effects.insert(BlockingNet);
    file_a.local_functions.insert(f_helper, sum.into());
    let res2 = index.update_file(file_a);
    assert!(res2.summary_changed);
    assert_eq!(res2.affected_callers, vec![f_caller.clone()]);
    assert!(
        index
            .summary(&f_caller)
            .unwrap()
            .transitive_exec_effects
            .contains(&BlockingNet)
    );
}

// =================================================================== local extraction (2.5)

const STATE_PY: &str = r#"
import functools

class Config:
    def __init__(self):
        self.params = {}
        self.cache = {}

    @functools.cached_property
    def proxy(self):
        return self.params["proxy"]

    def set_proxy(self, url):
        self.params["proxy"] = url

    def reset(self):
        del self.proxy

    def reset_dict(self):
        self.__dict__.pop("proxy", None)

    def reset_all(self):
        self.__dict__.clear()

    def bump(self):
        alias = self.cache
        alias.update(a=1)

    def via_helper(self):
        self.set_proxy("x")

class Model:
    def __init__(self):
        self.fields = []

def writes(m: Model, other: Model):
    m.fields = []
    del m.fields
    m.fields.append(1)
    alias = m.fields
    alias.append(2)
    setattr(m, "fields", [])
    object.__setattr__(other, "fields", [])
    m.__dict__["fields"] = []
"#;

#[test]
fn self_attr_reads_mutations_and_cache_invalidations() {
    let index = project(&[("state.py", STATE_PY)]);
    let path = |attr: &str, k: Option<&str>| AttrKeyPath {
        attr: attr.into(),
        const_key: k.map(str::to_owned),
    };
    let proxy = summary(&index, "state", "Config.proxy");
    assert!(proxy.is_cached_property);
    assert_eq!(
        proxy.reads_self_attrs,
        set(&[path("params", Some("proxy"))])
    );
    assert_eq!(
        summary(&index, "state", "Config.set_proxy").mutates_self_attrs,
        set(&[path("params", Some("proxy"))])
    );
    assert_eq!(
        summary(&index, "state", "Config.reset").invalidates_cached,
        set(&["proxy".to_owned()])
    );
    assert_eq!(
        summary(&index, "state", "Config.reset_dict").invalidates_cached,
        set(&["proxy".to_owned()])
    );
    assert_eq!(
        summary(&index, "state", "Config.reset_all").invalidates_cached,
        set(&["*".to_owned()])
    );
    assert_eq!(
        summary(&index, "state", "Config.bump").mutates_self_attrs,
        set(&[path("cache", None)])
    );
    // `self.helper()` merges the helper's self-attribute effects.
    assert!(
        summary(&index, "state", "Config.via_helper")
            .mutates_self_attrs
            .contains(&path("params", Some("proxy")))
    );
}

#[test]
fn typed_field_write_kinds() {
    let index = project(&[("state.py", STATE_PY)]);
    let writes = summary(&index, "state", "writes");
    let kinds: BTreeSet<WriteKind> = writes
        .typed_field_write_sites
        .iter()
        .inspect(|t| {
            assert_eq!(t.class_qualname, "state.Model");
            assert_eq!(t.attr, "fields");
        })
        .map(|t| t.kind)
        .collect();
    assert_eq!(
        kinds,
        set(&[
            WriteKind::DirectAttr,
            WriteKind::AttrDelete,
            WriteKind::ContainerMut,
            WriteKind::AliasContainerMut,
            WriteKind::SetAttr,
            WriteKind::DictDirect,
        ])
    );
    assert_eq!(writes.mutated_param_paths[&0], set(&["fields".to_owned()]));
    assert_eq!(writes.mutated_param_paths[&1], set(&["fields".to_owned()]));
    assert!(
        writes
            .typed_field_writes
            .iter()
            .any(|t| t.kind == WriteKind::SetAttr && t.class_qualname == "state.Model")
    );
}

// =================================================================== receiver resolution (2.3)

const TYPES_PY: &str = r#"
from typing import TYPE_CHECKING, Optional
import httpx
import requests

if TYPE_CHECKING:
    import pymongo

CLIENT = httpx.Client()

def make():
    return requests.Session()

class Repo:
    def __init__(self):
        self.http = httpx.Client()
        self.s: requests.Session = make()

    def load(self):
        return self.http.get("u")

    def save(self):
        self.s.post("u")

def optional_param(c: Optional[httpx.Client]):
    c.get("u")

def union_param(c: "httpx.Client | None"):
    c.get("u")

def tc_only(m: "pymongo.MongoClient"):
    m.close()

def module_global():
    CLIENT.get("u")

def local_ann():
    c: httpx.Client = make()
    c.get("u")

def with_block():
    with httpx.Client() as c:
        c.get("u")

def returns_repo() -> Repo:
    return Repo()

def via_return():
    returns_repo().load()
"#;

#[test]
fn receiver_types_priorities_1_to_3() {
    let index = project(&[("types_mod.py", TYPES_PY)]);
    let net = |q: &str| {
        let s = summary(&index, "types_mod", q);
        s.direct_exec_effects
            .iter()
            .chain(&s.transitive_exec_effects)
            .any(|e| *e == BlockingNet)
    };
    for q in [
        "Repo.load",
        "Repo.save",
        "optional_param",
        "union_param",
        "module_global",
        "local_ann",
        "with_block",
        "via_return",
    ] {
        assert!(net(q), "{q}");
    }
    let init = summary(&index, "types_mod", "Repo.__init__");
    assert!(init.direct_exec_effects.contains(&UnpooledClientAlloc));
    // `pymongo` imported only under TYPE_CHECKING still resolves the annotation;
    // `MongoClient.close` is not catalogued, so no effect and no unknown call.
    assert!(!summary(&index, "types_mod", "tc_only").unknown_calls);
}

fn cha_source(subclasses: usize) -> String {
    let mut src = String::from(
        "import time\n\nclass Base:\n    def run(self):\n        return 0\n\n    def __init__(self):\n        self.x = 1\n",
    );
    for i in 0..subclasses {
        let body = if i == 0 { "time.sleep(1)" } else { "return 1" };
        src.push_str(&format!(
            "\nclass Sub{i}(Base):\n    def run(self):\n        {body}\n"
        ));
    }
    src.push_str(
        "\ndef call(b: Base):\n    b.run()\n\nclass Child(Base):\n    def __init__(self):\n        super().__init__()\n",
    );
    src
}

#[test]
fn cha_priority_4_limit() {
    // Base + 7 overriding subclasses = 8 candidates: all get edges.
    let index = project(&[("cha.py", &cha_source(7))]);
    let call = summary(&index, "cha", "call");
    assert!(!call.unknown_calls);
    assert!(call.transitive_exec_effects.contains(&Sleep));
    let file = &index.files[&PathBuf::from("cha.py")];
    let edges = file
        .call_edges()
        .filter(|(caller, _, _)| caller.qualname == "call")
        .count();
    assert_eq!(edges, 8);
    // `super().__init__()` binds to `Base.__init__` with the caller's `self`.
    let child = summary(&index, "cha", "Child.__init__");
    assert!(child.mutates_self_attrs.iter().any(|p| p.attr == "x"));

    // 9 candidates exceed the limit: unresolved, no edges.
    let index = project(&[("cha.py", &cha_source(8))]);
    let call = summary(&index, "cha", "call");
    assert!(call.unknown_calls);
    assert!(!call.transitive_exec_effects.contains(&Sleep));
}

// =================================================================== module names (2.2)

#[test]
fn module_names_and_relative_imports() {
    let name = |p: &str, roots: &[&str]| {
        let layout = ModuleLayout {
            source_roots: roots.iter().map(|r| (*r).to_owned()).collect(),
            root_is_package: false,
        };
        layout.module_name(Path::new(p))
    };
    assert_eq!(name("pkg/mod.py", &[]), Some(("pkg.mod".into(), false)));
    assert_eq!(name("pkg/__init__.py", &[]), Some(("pkg".into(), true)));
    assert_eq!(
        name("src/pkg/mod.py", &["src"]),
        Some(("pkg.mod".into(), false))
    );
    assert_eq!(
        name("src/pkg/mod.py", &[]),
        Some(("src.pkg.mod".into(), false))
    );
    // The deepest source root holding the file names it; a file below none keeps its path.
    assert_eq!(
        name("src/plugins/x/mod.py", &["src/plugins", "src"]),
        Some(("x.mod".into(), false))
    );
    assert_eq!(
        name("srcs/pkg/mod.py", &["src"]),
        Some(("srcs.pkg.mod".into(), false))
    );
    assert_eq!(name("pkg/data.txt", &[]), None);
    // `heki_rules::project::rel_posix` keeps normal components only; others are refused.
    assert_eq!(name("../escape.py", &[]), None);
    // A root `__init__.py` names no module.
    assert_eq!(name("__init__.py", &[]), None);

    let rel = |m: &str, pkg, level, module| resolve_relative_module(m, pkg, level, module, false);
    assert_eq!(rel("pkg.a", false, 1, Some("sub")), Some("pkg.sub".into()));
    assert_eq!(rel("pkg", true, 1, Some("sub")), Some("pkg.sub".into()));
    assert_eq!(rel("pkg", true, 1, None), Some("pkg".into()));
    assert_eq!(rel("pkg.sub.mod", false, 2, None), Some("pkg".into()));
    assert_eq!(rel("pkg.a", false, 3, Some("x")), None);
    // Past the top-level package (`from ..time import sleep` in `pkg/mod.py`), or with no
    // package at all: `ImportError`.
    assert_eq!(rel("pkg.mod", false, 2, Some("time")), None);
    assert_eq!(rel("pkg", true, 2, None), None);
    assert_eq!(rel("mod", false, 1, Some("x")), None);
    assert_eq!(rel("pkg.a", false, 0, Some("os")), Some("os".into()));
    // A project root that is a package: one more level reaches it (its modules are top-level).
    let in_root =
        |m: &str, pkg, level, module| resolve_relative_module(m, pkg, level, module, true);
    assert_eq!(in_root("api", false, 1, Some("util")), Some("util".into()));
    assert_eq!(in_root("api", false, 1, None), Some(String::new()));
    assert_eq!(
        in_root("pkg.mod", false, 2, Some("time")),
        Some("time".into())
    );
    assert_eq!(in_root("pkg.mod", false, 3, Some("time")), None);
}

#[test]
fn source_root_detection() {
    let root = std::env::temp_dir().join(format!("heki_effect_src_layout_{}", std::process::id()));
    let detected = |name: Option<&str>| detect_source_roots(&root, name);
    assert!(detected(None).is_empty());
    // A namespace package (no `__init__.py`) under `src/` is laid out like a regular one.
    std::fs::create_dir_all(root.join("src/pkg")).unwrap();
    assert_eq!(detected(None), ["src"]);
    std::fs::write(root.join("src/pkg/__init__.py"), "").unwrap();
    assert_eq!(detected(None), ["src"]);
    // `src` itself a package: imported as `src.pkg`.
    std::fs::write(root.join("src/__init__.py"), "").unwrap();
    assert!(detected(None).is_empty());
    // `python/` (maturin), and `<project-name>/` holding `<project-name>/`, in ty's order.
    std::fs::create_dir_all(root.join("python/acme")).unwrap();
    std::fs::create_dir_all(root.join("acme/acme")).unwrap();
    assert_eq!(detected(None), ["python"]);
    assert_eq!(detected(Some("acme")), ["acme", "python"]);
    std::fs::write(root.join("acme/__init__.py"), "").unwrap();
    assert_eq!(detected(Some("acme")), ["python"]);
    // Without `project.name`, the project is named after its root.
    let own = root.file_name().unwrap().to_str().unwrap();
    std::fs::create_dir_all(root.join(own).join(own)).unwrap();
    assert_eq!(detected(None), [own, "python"]);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn relative_imports_resolve_in_src_layout_projects() {
    let index = project(&[
        ("pkg/__init__.py", "from .net import fetch\n"),
        ("pkg/net.py", NET_PY),
        (
            "pkg/app.py",
            "from . import fetch\n\ndef go(u):\n    return fetch(u)\n",
        ),
    ]);
    // `from . import fetch` -> `pkg.fetch` -> re-export -> `pkg.net.fetch`.
    assert_eq!(
        chain_callees(summary(&index, "pkg.app", "go"), BlockingNet),
        vec!["pkg.net.fetch", "requests.get"]
    );
}

#[test]
fn annotation_normalization() {
    let decls = ModuleDecls {
        module_name: "m".into(),
        imports: BTreeMap::from([
            ("httpx".into(), "httpx".into()),
            ("Client".into(), "httpx.Client".into()),
            ("t".into(), "typing".into()),
            ("List".into(), "typing.List".into()),
        ]),
        type_checking_imports: BTreeMap::from([("Repo".into(), "pkg.repo.Repo".into())]),
        classes: BTreeMap::from([("Local".into(), Box::default())]),
        ..ModuleDecls::default()
    };
    let r = |a: &str| decls.resolve_annotation(a);
    assert_eq!(r("httpx.Client").as_deref(), Some("httpx.Client"));
    assert_eq!(r("Optional[Client]").as_deref(), Some("httpx.Client"));
    assert_eq!(r("t.Optional[Client]").as_deref(), Some("httpx.Client"));
    assert_eq!(r("Client | None").as_deref(), Some("httpx.Client"));
    assert_eq!(r("None | Client").as_deref(), Some("httpx.Client"));
    assert_eq!(r("'Client'").as_deref(), Some("httpx.Client"));
    assert_eq!(
        r("Annotated[Client, 'meta']").as_deref(),
        Some("httpx.Client")
    );
    assert_eq!(r("list[int]").as_deref(), Some("builtins.list"));
    assert_eq!(r("List[int]").as_deref(), Some("builtins.list"));
    assert_eq!(r("Local").as_deref(), Some("m.Local"));
    assert_eq!(r("Repo").as_deref(), Some("pkg.repo.Repo"));
    assert_eq!(r("Client | int"), None);
    assert_eq!(r("Unknown"), None);
}

/// A resolver mapping the first dotted component like a module's imports (`MUT108` parsing).
fn mut108_resolve(name: &str) -> Option<String> {
    let (first, rest) = name
        .split_once('.')
        .map_or((name, None), |(f, r)| (f, Some(r)));
    let base = match first {
        "Final"
        | "ClassVar"
        | "Annotated"
        | "Optional"
        | "Union"
        | "Required"
        | "NotRequired"
        | "MutableSequence"
        | "Mapping"
        | "dataclass_transform" => format!("typing.{first}"),
        "te" => "typing_extensions".into(),
        "other" => "pkg.other".into(),
        "ReadOnly" => "typing_extensions.ReadOnly".into(),
        "InitVar" | "KW_ONLY" | "dataclass" => format!("dataclasses.{first}"),
        "Sequence" => "collections.abc.Sequence".into(),
        "deque" => "collections.deque".into(),
        "list" | "dict" | "int" => format!("builtins.{first}"),
        "t" => "typing".into(),
        "dc" => "dataclasses".into(),
        "attrs" => "attrs".into(),
        "Box" => "pkg.Box".into(),
        _ => return None,
    };
    Some(rest.map_or(base.clone(), |r| format!("{base}.{r}")))
}

#[test]
fn mut108_attribute_qualifiers() {
    use crate::resolver::{Qualifiers, qualifiers};
    let q = |a: &'static str| qualifiers(a, &mut108_resolve);
    let none = Qualifiers::default();
    let fin = Qualifiers {
        is_final: true,
        ..none
    };
    let pseudo = Qualifiers {
        pseudo_field: true,
        ..none
    };
    let ro = Qualifiers {
        read_only: true,
        ..none
    };
    let class_var = Qualifiers {
        class_var: true,
        ..none
    };
    assert_eq!(q("int"), (none, Some("int")));
    assert_eq!(q("Final"), (fin, None));
    assert_eq!(q("Final[list[int]]"), (fin, Some("list[int]")));
    assert_eq!(q("t.Final[int]"), (fin, Some("int")));
    assert_eq!(
        q("ClassVar[Final[list[int]]]"),
        (
            Qualifiers {
                class_var: true,
                ..fin
            },
            Some("list[int]")
        )
    );
    assert_eq!(
        q("'ReadOnly[NotRequired[list[str]]]'"),
        (ro, Some("list[str]"))
    );
    assert_eq!(q("Required[ReadOnly[list[str]]]"), (ro, Some("list[str]")));
    assert_eq!(
        q("Annotated[Final[dict[str, int]], 'doc']"),
        (fin, Some("dict[str, int]"))
    );
    assert_eq!(q("InitVar[list[int]]"), (pseudo, None));
    assert_eq!(q("KW_ONLY"), (pseudo, None));
    // `typing` names that are no qualifiers, and names of other modules.
    assert_eq!(q("t.List[int]"), (none, Some("t.List[int]")));
    assert_eq!(q("Box"), (none, Some("Box")));
    // Unresolved names: only the ones that take protection away count.
    assert_eq!(q("x.Final[list[int]]"), (none, Some("x.Final[list[int]]")));
    assert_eq!(q("x.ClassVar[list[int]]"), (class_var, Some("list[int]")));
}

#[test]
fn mut108_parameter_access() {
    use crate::resolver::{ParamAccess, param_access};
    let a = |t: &str| param_access(t, &mut108_resolve);
    let ro = Some(ParamAccess::ReadOnly);
    let mutable = Some(ParamAccess::Mutable);
    assert_eq!(a("Sequence[int]"), ro);
    assert_eq!(a("Sequence"), ro);
    assert_eq!(a("Mapping[str, int] | None"), ro);
    assert_eq!(a("None | Mapping[str, int]"), ro);
    assert_eq!(a("Optional[Sequence[int]]"), ro);
    assert_eq!(a("Union[Sequence[int], None]"), ro);
    assert_eq!(a("Annotated[Mapping[str, int], 'doc']"), ro);
    assert_eq!(a("'ReadOnly[list[int]]'"), ro);
    assert_eq!(a("t.Sequence[int]"), ro);
    assert_eq!(a("list[int]"), mutable);
    assert_eq!(a("dict[str, int] | None"), mutable);
    assert_eq!(a("MutableSequence[int]"), mutable);
    assert_eq!(a("deque[int]"), mutable);
    // Unresolved names: only the mutable ones count.
    assert_eq!(a("List[int]"), mutable);
    assert_eq!(a("Iterable[int]"), None);
    assert_eq!(a("Box"), None);
    assert_eq!(a("int"), None);
    assert_eq!(a("Optional"), None);
    // Unions of several types promise nothing.
    assert_eq!(a("Union[Sequence[int], str]"), None);
    assert_eq!(a("Sequence[int] | list[int]"), None);
}

#[test]
fn mut108_dataclass_decorators() {
    use crate::resolver::dataclass_decorator;
    let f = |d: &str| dataclass_decorator(d, &mut108_resolve);
    assert_eq!(f("dataclass"), Some(false));
    assert_eq!(f("dataclass()"), Some(false));
    assert_eq!(f("dataclass(frozen=True)"), Some(true));
    assert_eq!(f("dataclass(slots=True, frozen = True)"), Some(true));
    assert_eq!(f("dataclass(\n    frozen=True,\n)"), Some(true));
    assert_eq!(f("dc.dataclass(frozen=True)"), Some(true));
    assert_eq!(f("dataclass(frozen=False)"), Some(false));
    assert_eq!(f("dataclass(frozen=FROZEN)"), Some(false));
    assert_eq!(f("attrs.frozen"), None);
    assert_eq!(f("unknown(frozen=True)"), None);
}

#[test]
fn mut108_dataclass_transform_decorators() {
    use crate::resolver::transform_decorator;
    let f = |d: &str| transform_decorator(d, &mut108_resolve);
    assert_eq!(
        f("dataclass_transform(frozen_default=True, field_specifiers=(field,))"),
        Some(true)
    );
    assert_eq!(
        f("te.dataclass_transform(frozen_default = True)"),
        Some(true)
    );
    assert_eq!(f("dataclass_transform(frozen_default=False)"), Some(false));
    assert_eq!(
        f("dataclass_transform(field_specifiers=(field,))"),
        Some(false)
    );
    assert_eq!(f("dataclass_transform"), Some(false));
    assert_eq!(f("other.dataclass_transform(frozen_default=True)"), None);
    assert_eq!(f("dataclass(frozen=True)"), None);
    assert_eq!(f("unknown.dataclass_transform(frozen_default=True)"), None);
}

// =================================================================== limits and invariant guards

/// A cycle of `n` methods, each writing its own `self` attribute and calling the next.
fn method_cycle(n: usize) -> String {
    let mut src = String::from("class C:\n");
    for i in 0..n {
        src.push_str(&format!(
            "    def m{i}(self):\n        self.a{i} = 1\n        self.m{}()\n\n",
            (i + 1) % n
        ));
    }
    src
}

#[test]
fn non_converged_sccs_are_marked_unknown() {
    // Even a long simple cycle converges in two rounds because Tarjan pops components callee-first.
    let mut index = project(&[("big.py", &method_cycle(24))]);
    for i in 0..24 {
        let s = summary(&index, "big", &format!("C.m{i}"));
        assert!(!s.unknown_calls, "C.m{i}");
        assert_eq!(s.mutates_self_attrs.len(), 24, "C.m{i}");
    }
    // When an SCC still exceeds its iteration budget (abnormal guard), propagation stops and
    // marks every member as not fully known.
    let all: FxHashSet<FuncKey> = index.all_functions().collect();
    crate::callgraph::reset_to_local(&mut index, &all);
    crate::callgraph::propagate_with_budget(&mut index, &all, 1);
    for i in 0..24 {
        assert!(
            summary(&index, "big", &format!("C.m{i}")).unknown_calls,
            "C.m{i}"
        );
    }
}

#[test]
fn callers_left_without_a_defining_file_are_skipped() {
    // Two `.py` files share one module name: `b_alt.py` defines `b.caller`, and the call edges
    // of `b.py` (hand-built: a summarized file defines its callers) name it as a caller of
    // `a.helper`. When `b_alt.py` drops `caller`, no file defines it any more (a file that
    // still did would take it over).
    let helper = key("a", "_helper");
    let caller = key("b", "caller");
    let span = Span { start: 0, end: 1 };
    let file = |path: &str, module: &str, funcs: &[&FuncKey]| {
        let mut f = FileSummary {
            path: PathBuf::from(path),
            module_name: module.into(),
            ..Default::default()
        };
        for k in funcs {
            f.local_functions
                .insert((*k).clone(), EffectSummary::default().into());
        }
        f
    };
    let file_a = file("a.py", "a", &[&helper]);
    let mut file_b = file("b.py", "b", &[]);
    file_b.call_sites.push(CallSite {
        caller: caller.clone(),
        target: CallTarget::Project(vec![BoundCallee {
            callee: helper.clone(),
            params: Vec::new(),
        }]),
        is_awaited: false,
        span,
        loop_depth: 0,
        recv_is_self: false,
        drives_coroutine_manually: false,
        constructs: None,
        role: crate::SiteRole::Call,
        returned: None,
        flow: None,
        guards: Vec::new(),
        once_per: None,
        once_loops: 0,
        runner: crate::Runner::Caller,
        binds_recv: false,
        handed_callable: None,
        per_item: None,
        dispatch: None,
        result_discarded: false,
        queues: None,
        result_held: false,
    });
    let live = key("c", "live");
    let mut file_c = file("c.py", "c", &[&live]);
    file_c.call_sites.push(CallSite {
        caller: live.clone(),
        target: CallTarget::Project(vec![BoundCallee {
            callee: helper.clone(),
            params: Vec::new(),
        }]),
        is_awaited: false,
        span,
        loop_depth: 0,
        recv_is_self: false,
        drives_coroutine_manually: true,
        constructs: None,
        role: crate::SiteRole::Call,
        returned: None,
        flow: None,
        guards: Vec::new(),
        once_per: None,
        once_loops: 0,
        runner: crate::Runner::Caller,
        binds_recv: false,
        handed_callable: None,
        per_item: None,
        dispatch: None,
        result_discarded: false,
        queues: None,
        result_held: false,
    });
    file_c.call_sites.push(CallSite {
        caller: live.clone(),
        target: CallTarget::Project(vec![BoundCallee {
            callee: live.clone(),
            params: Vec::new(),
        }]),
        is_awaited: false,
        span,
        loop_depth: 0,
        recv_is_self: false,
        drives_coroutine_manually: false,
        constructs: None,
        role: crate::SiteRole::Call,
        returned: None,
        flow: None,
        guards: Vec::new(),
        once_per: None,
        once_loops: 0,
        runner: crate::Runner::Caller,
        binds_recv: false,
        handed_callable: None,
        per_item: None,
        dispatch: None,
        result_discarded: false,
        queues: None,
        result_held: false,
    });
    let dup = file("b_alt.py", "b", &[&caller]);
    let mut index = ProjectIndex::build(vec![file_a.clone(), file_b, file_c, dup]);
    assert_eq!(index.file_of(&caller), Some(Path::new("b_alt.py")));
    index.update_file(file("b_alt.py", "b", &[]));
    assert_eq!(index.file_of(&caller), None);

    // `helper` starts blocking (and async): the stale caller is reported as affected, but
    // propagation and the `sync_driven_only` pass skip it (it has no summary to update).
    let mut blocking = file_a;
    let mut s = EffectSummary {
        is_async: true,
        ..EffectSummary::default()
    };
    s.direct_exec_effects.insert(BlockingNet);
    blocking.local_functions.insert(helper.clone(), s.into());
    let res = index.update_file(blocking);
    assert!(res.summary_changed);
    assert_eq!(res.affected_callers, vec![caller.clone(), live]);
    assert!(index.summary(&caller).is_none());
    assert!(
        index
            .summary(&helper)
            .unwrap()
            .direct_exec_effects
            .contains(&BlockingNet)
    );
}

#[test]
fn py_implementation_takes_precedence_over_sibling_pyi_stub() {
    let mut index = project(&[
        (
            "pkg/util.py",
            "import time\n\ndef helper():\n    time.sleep(1)\n",
        ),
        ("pkg/util.pyi", "def helper() -> None: ...\n"),
        (
            "pkg/api.py",
            "from pkg.util import helper\n\nasync def handler():\n    helper()\n",
        ),
        ("pkg/api.pyi", "async def handler() -> None: ...\n"),
    ]);
    assert_eq!(
        index.file_of(&key("pkg.util", "helper")),
        Some(Path::new("pkg/util.py"))
    );
    assert!(
        index
            .module_decls("pkg.util")
            .unwrap()
            .imports
            .contains_key("time")
    );
    assert!(
        summary(&index, "pkg.api", "handler")
            .transitive_exec_effects
            .contains(&Sleep)
    );
    // Editing the sibling `.pyi` stub leaves the `.py` implementation's decls and effects intact.
    let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
    let mut decls = index.decl_passes();
    let stub_src = "def helper() -> None: ...\ndef extra() -> None: ...\n";
    let stub_parsed = heki_ast::parse_module(stub_src).unwrap();
    let profile = RuntimeProfile::from(PythonVersion::PY315);
    let stub_sem = heki_semantic::analyze_module(&stub_parsed, stub_src, profile);
    let stub_ir = heki_ir::build_module_ir(&stub_parsed, stub_src, &stub_sem, profile);
    let stub_input = ModuleInput {
        path: PathBuf::from("pkg/util.pyi"),
        module_name: "pkg.util".into(),
        is_package: false,
        root_is_package: false,
        content_hash: String::new(),
        semantic: &stub_sem,
        ir: &stub_ir,
    };
    let (_upd, decls_changed) = index.reanalyze_module_cached(&stub_input, &kb, &mut decls);
    assert!(decls_changed.is_empty());
    assert_eq!(
        index.file_of(&key("pkg.util", "helper")),
        Some(Path::new("pkg/util.py"))
    );
    assert!(
        summary(&index, "pkg.util", "helper")
            .direct_exec_effects
            .contains(&Sleep)
    );
    // The stub keeps its own declarations (`extra`, no `time` import).
    let stub_decls = index.files[Path::new("pkg/util.pyi")].decls.clone();
    assert!(stub_decls.functions.contains_key("extra"));
    assert!(!stub_decls.imports.contains_key("time"));
    // Removing the `.py` file falls back to the remaining stub: its declarations, and its
    // functions, which no longer block (the caller's effect goes away).
    let update = index.remove_file(Path::new("pkg/util.py"));
    assert_eq!(update.affected_callers, vec![key("pkg.api", "handler")]);
    assert_eq!(index.module_decls("pkg.util"), Some(stub_decls));
    assert_eq!(
        index.file_of(&key("pkg.util", "helper")),
        Some(Path::new("pkg/util.pyi"))
    );
    assert!(
        summary(&index, "pkg.api", "handler")
            .transitive_exec_effects
            .is_empty()
    );
    index.remove_file(Path::new("pkg/util.pyi"));
    assert!(index.module_decls("pkg.util").is_none());
}

#[test]
fn classes_missing_from_the_semantic_model_keep_their_methods_summarized() {
    // The semantic model and the IR come from one AST, so every IR class body has a
    // semantic class. Without one, the class gets no declaration (no attribute types), but
    // its methods are still summarized.
    let mut b = build(
        "m.py",
        "import time\n\n\nclass Late:\n    field: int\n\n    def __init__(self):\n        self.x = 1\n\n    def run(self):\n        time.sleep(1)\n",
    );
    b.semantic.classes.clear();
    let index = analyze_project(&[input(&b)], &kb());
    assert!(index.files[Path::new("m.py")].decls.classes.is_empty());
    assert!(
        summary(&index, "m", "Late.run")
            .direct_exec_effects
            .contains(&Sleep)
    );
}
