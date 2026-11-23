use super::*;
use crate::{BlockId, InstKind, ModuleIr, NaturalLoop, Terminator, VarId, build_module_ir};
use heki_ast::parse_module;
use heki_env::{PythonVersion, RuntimeProfile};
use heki_semantic::analyze_module;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

fn build(src: &str) -> ModuleIr {
    let parsed = parse_module(src).expect("valid python");
    let profile = RuntimeProfile::from(PythonVersion::PY315);
    let semantic = analyze_module(&parsed, src, profile);
    build_module_ir(&parsed, src, &semantic, profile)
}

/// A budget of exactly `steps` steps.
fn budget(steps: usize) -> Budget {
    let ir = build("pass\n");
    Budget::per_source_byte("test", &ir.functions[0], 0, steps, steps)
}

/// The nodes one edge from `n` in a graph given as adjacency lists.
fn edges<'g>(graph: &'g [&'g [u32]]) -> impl Fn(u32, &mut Vec<u32>) + 'g {
    move |n, out| out.extend_from_slice(graph[n as usize])
}

// ---------------------------------------------------------------------------------- answers

#[test]
fn an_open_answer_means_what_its_consumer_states() {
    assert_eq!(Answer::Known(3).open_means(0), 3);
    assert_eq!(Answer::<u32>::Open.open_means(0), 0);
    assert!(Answer::<u32>::Open.is_open());
    assert!(!Answer::Known(1).is_open());
}

#[test]
fn combined_answers_are_open_when_one_part_is() {
    let known = Answer::Known(2);
    let open = Answer::<u32>::Open;
    assert_eq!(known.as_ref(), Answer::Known(&2));
    assert_eq!(open.as_ref(), Answer::Open);
    assert_eq!(known.map(|x| x * 2), Answer::Known(4));
    assert_eq!(open.map(|x| x * 2), Answer::Open);
    assert_eq!(known.and_then(|x| Answer::Known(x + 1)), Answer::Known(3));
    assert_eq!(known.and_then(|_| Answer::<u32>::Open), Answer::Open);
    assert_eq!(open.and_then(|x| Answer::Known(x + 1)), Answer::Open);
    assert_eq!(known.zip(Answer::Known("a")), Answer::Known((2, "a")));
    assert_eq!(known.zip(Answer::<&str>::Open), Answer::Open);
    assert_eq!(open.zip(Answer::Known("a")), Answer::Open);
    assert_eq!(
        known.join_with(Answer::Known(5), u32::max),
        Answer::Known(5)
    );
    assert_eq!(known.join_with(open, u32::max), Answer::Open);
}

#[test]
fn collected_answers_are_established_only_when_each_is() {
    let all: Answer<Vec<u32>> = [Answer::Known(1), Answer::Known(2)].into_iter().collect();
    assert_eq!(all, Answer::Known(vec![1, 2]));
    let one_open: Answer<Vec<u32>> = [Answer::Known(1), Answer::Open, Answer::Known(3)]
        .into_iter()
        .collect();
    assert_eq!(one_open, Answer::Open);
    let none: Answer<Vec<u32>> = std::iter::empty().collect();
    assert_eq!(none, Answer::Known(Vec::new()));
}

#[test]
fn answers_serialize_by_their_state() {
    let json = |a: &Answer<u32>| serde_json::to_string(a).unwrap();
    assert_eq!(json(&Answer::Known(4)), r#"{"known":4}"#);
    assert_eq!(json(&Answer::Open), r#""open""#);
    let back: Answer<u32> = serde_json::from_str(r#"{"known":4}"#).unwrap();
    assert_eq!(back, Answer::Known(4));
}

// ----------------------------------------------------------------------------------- budget

#[test]
fn a_function_budget_counts_its_units_with_a_floor() {
    let small = build("def f():\n    return 1\n");
    let f = small.functions.iter().find(|f| f.name == "f").unwrap();
    let b = Budget::of_function("test", f);
    assert!(b.take(Budget::MIN_STEPS));
    assert!(!b.take(1));

    let body: String = (0..200).map(|i| format!("    x{i} = g(x)\n")).collect();
    let large = build(&format!("def f(x):\n{body}    return x\n"));
    let f = large.functions.iter().find(|f| f.name == "f").unwrap();
    let units: usize = f
        .blocks
        .iter()
        .map(|b| 1 + b.phis.len() + b.insts.len())
        .sum();
    assert!(units * Budget::STEPS_PER_UNIT > Budget::MIN_STEPS);
    let b = Budget::of_function("test", f);
    assert!(b.take(units * Budget::STEPS_PER_UNIT));
    assert!(!b.take(1));
}

#[test]
fn a_source_budget_is_clamped() {
    let ir = build("def f():\n    return 1\n");
    let f = ir.functions.iter().find(|f| f.name == "f").unwrap();
    let bytes = (f.span.end - f.span.start) as usize;
    let b = Budget::per_source_byte("test", f, 2, 0, usize::MAX);
    assert!(b.take(2 * bytes));
    assert!(!b.take(1));
    let b = Budget::per_source_byte("test", f, 2, 0, 3);
    assert!(b.take(3));
    assert!(!b.take(1));
}

#[test]
fn a_spent_budget_stays_spent() {
    let b = budget(2);
    assert!(b.take(2));
    assert!(!b.spent());
    assert!(!b.take(1));
    assert!(b.spent());
    // Even a take of nothing: every walk of the owner is now open.
    assert!(!b.take(0));
    assert_eq!(b.taken(), 3);

    let b = budget(5);
    b.spend();
    assert!(b.spent());
    assert!(!b.take(1));
}

#[test]
fn an_unmetered_budget_counts_but_never_runs_out() {
    let b = Budget::unmetered("test");
    assert!(b.take(usize::MAX));
    assert!(b.take(usize::MAX));
    assert_eq!(b.taken(), usize::MAX);
    assert!(!b.spent());
    b.spend();
    assert!(!b.take(1));
}

#[test]
fn recorded_budgets_keep_their_steps() {
    record_steps();
    let ir = build("def recorded_function():\n    return 1\n");
    let f = ir
        .functions
        .iter()
        .find(|f| f.name == "recorded_function")
        .unwrap();
    {
        let b = Budget::of_function("recorded-test", f);
        assert!(b.take(7));
        let unmetered = Budget::unmetered("recorded-test");
        assert!(unmetered.take(2));
    }
    let records: Vec<StepRecord> = recorded_steps()
        .into_iter()
        .filter(|r| r.owner == "recorded-test")
        .collect();
    assert_eq!(
        records,
        [
            StepRecord {
                owner: "recorded-test",
                function: None,
                start: None,
                limit: None,
                taken: 2,
                spent: false,
            },
            StepRecord {
                owner: "recorded-test",
                function: Some("recorded_function".to_owned()),
                start: Some(f.span.start),
                limit: Some(Budget::MIN_STEPS),
                taken: 7,
                spent: false,
            },
        ]
    );
}

// ------------------------------------------------------------------------------------- memo

/// The number `n` asks of the memo itself, nested one inside the other down to 0.
fn chain(memo: &Memo<u32, u32>, budget: &Budget, n: u32, decided: &Cell<u32>) -> Answer<u32> {
    memo.ask(budget, n, || {
        decided.set(decided.get() + 1);
        match n {
            0 => 0,
            _ => chain(memo, budget, n - 1, decided).open_means(u32::MAX),
        }
    })
}

#[test]
fn a_memo_decides_each_key_once() {
    let memo = Memo::new();
    let b = budget(100);
    let decided = Cell::new(0);
    assert_eq!(chain(&memo, &b, 10, &decided), Answer::Known(0));
    assert_eq!(chain(&memo, &b, 10, &decided), Answer::Known(0));
    assert_eq!(chain(&memo, &b, 12, &decided), Answer::Known(0));
    assert_eq!(decided.get(), 13);
    assert_eq!(b.taken(), 13);
}

#[test]
fn a_key_asked_while_deciding_is_open() {
    let memo: Memo<u32, bool> = Memo::new();
    let b = budget(100);
    // 0 asks 1, which asks 0 again: the cycle answers open, which 1 takes as `false`.
    let answer = memo.ask(&b, 0, || {
        memo.ask(&b, 1, || memo.ask(&b, 0, || true).open_means(false))
            .open_means(true)
    });
    assert_eq!(answer, Answer::Known(false));
    assert_eq!(memo.ask(&b, 1, || true), Answer::Known(false));
}

#[test]
fn a_memo_past_its_budget_is_open_and_keeps_nothing() {
    let memo = Memo::new();
    let decided = Cell::new(0);
    let b = budget(3);
    // Keys 5, 4, and 3 take the three steps; 2 has none, and the decisions resting on it are
    // open too.
    assert_eq!(chain(&memo, &b, 5, &decided), Answer::Open);
    assert!(b.spent());
    assert_eq!(decided.get(), 3);
    // From then on every question is open, asked before or not.
    assert_eq!(memo.ask(&b, 9, || 1), Answer::Open);
}

#[test]
fn asks_nested_past_the_guard_are_open_and_spend_the_budget() {
    let memo = Memo::new();
    let decided = Cell::new(0);
    let b = budget(10_000);
    let deepest = MAX_NESTED_ASKS as u32 - 1;
    assert_eq!(chain(&memo, &b, deepest, &decided), Answer::Known(0));
    assert!(!b.spent());

    let memo = Memo::new();
    let b = budget(10_000);
    assert_eq!(chain(&memo, &b, deepest + 1, &decided), Answer::Open);
    assert!(b.spent());
}

// ---------------------------------------------------------------------------------- closure

fn singleton(n: u32) -> KeySet<u32> {
    std::iter::once(n).collect()
}

fn keys(keys: impl IntoIterator<Item = u32>) -> Answer<Rc<KeySet<u32>>> {
    Answer::Known(Rc::new(keys.into_iter().collect()))
}

#[test]
fn a_closure_joins_what_each_node_reaches_and_expands_each_once() {
    let graph: &[&[u32]] = &[&[1, 2], &[2], &[], &[0]];
    let closure = Closure::new();
    let b = budget(100);
    let expanded = Cell::new(0);
    let own = |n: u32| {
        expanded.set(expanded.get() + 1);
        singleton(n)
    };
    assert_eq!(closure.ask(&b, 0, own, edges(graph)), keys([0, 1, 2]));
    assert_eq!(closure.ask(&b, 1, own, edges(graph)), keys([1, 2]));
    // 3 reaches the component of 0, already complete: only 3 is expanded.
    assert_eq!(closure.ask(&b, 3, own, edges(graph)), keys([0, 1, 2, 3]));
    assert_eq!(expanded.get(), 4);
}

#[test]
fn the_members_of_a_cycle_share_one_fact() {
    // 0 -> 1 -> 2 -> 0, and 2 -> 3; 4 -> 1 asked after.
    let graph: &[&[u32]] = &[&[1], &[2], &[0, 3], &[], &[1]];
    let closure = Closure::new();
    let b = budget(100);
    for start in [1, 0, 2] {
        assert_eq!(
            closure.ask(&b, start, singleton, edges(graph)),
            keys([0, 1, 2, 3])
        );
    }
    assert_eq!(closure.ask(&b, 3, singleton, edges(graph)), keys([3]));
    assert_eq!(
        closure.ask(&b, 4, singleton, edges(graph)),
        keys([0, 1, 2, 3, 4])
    );
}

#[test]
fn a_large_cycle_costs_steps_linear_in_its_facts() {
    // n nodes in one cycle, each with a fact of its own: the members' facts are joined into
    // the component's once each, never into a growing copy.
    let n: u32 = 100_000;
    let next = |k: u32, out: &mut Vec<u32>| out.push((k + 1) % n);
    let closure = Closure::new();
    let b = budget(usize::MAX);
    let fact = match closure.ask(&b, 0, singleton, next) {
        Answer::Known(fact) => fact,
        Answer::Open => panic!("the budget is not spent"),
    };
    assert_eq!(fact.len(), n as usize);
    // One step and the own fact's weight per node; the component reaches no other.
    assert_eq!(b.taken(), 2 * n as usize);
    // Every member shares the fact.
    for k in [1, n / 2, n - 1] {
        let Answer::Known(shared) = closure.ask(&b, k, singleton, next) else {
            panic!("the fact is kept");
        };
        assert!(Rc::ptr_eq(&shared, &fact));
    }
}

#[test]
fn each_component_takes_the_weight_of_the_facts_it_joins() {
    // 1 and 2 each reach 0, whose own fact holds five keys.
    let graph: &[&[u32]] = &[&[], &[0], &[0, 0]];
    let own = |k: u32| match k {
        0 => (10..15).collect(),
        _ => singleton(k),
    };
    let closure = Closure::new();
    let b = budget(100);
    assert_eq!(closure.ask(&b, 0, own, edges(graph)), keys(10..15));
    assert_eq!(b.taken(), 1 + 5);
    assert_eq!(
        closure.ask(&b, 1, own, edges(graph)),
        keys([1, 10, 11, 12, 13, 14])
    );
    // Node 2 lists 0 twice and joins its fact once.
    assert_eq!(
        closure.ask(&b, 2, own, edges(graph)),
        keys([2, 10, 11, 12, 13, 14])
    );
    assert_eq!(b.taken(), (1 + 5) + 2 * ((1 + 1) + 5));
}

#[test]
fn a_closure_past_its_budget_is_open() {
    let graph: &[&[u32]] = &[&[1], &[2], &[]];
    let spent = budget(1);
    spent.spend();
    let closure: Closure<u32, KeySet<u32>> = Closure::new();
    assert_eq!(
        closure.ask(&spent, 0, singleton, edges(graph)),
        Answer::Open
    );
    // No steps for the root.
    assert_eq!(
        closure.ask(&budget(1), 0, singleton, edges(graph)),
        Answer::Open
    );
    // Steps for the root only.
    assert_eq!(
        closure.ask(&budget(2), 0, singleton, edges(graph)),
        Answer::Open
    );
    // Steps for every node, not for joining the fact of 2 into the component of 1.
    assert_eq!(
        closure.ask(&budget(6), 0, singleton, edges(graph)),
        Answer::Open
    );
}

// ------------------------------------------------------------------------------------ reach

#[test]
fn reach_visits_each_node_once_in_order() {
    // 0 -> 1, 2; 1 -> 3; 2 -> 3; 3 -> 0.
    let graph: &[&[u32]] = &[&[1, 2], &[3], &[3], &[0]];
    let b = budget(100);
    let mut order = Vec::new();
    let found = reach(&b, [0], edges(graph), |n| {
        order.push(n);
        Visit::<()>::Expand
    });
    assert_eq!(found, Answer::Known(None));
    assert_eq!(order, [0, 1, 3, 2]);
    assert_eq!(b.taken(), 4);

    let found = reach(&budget(100), [0], edges(graph), |n| match n {
        1 => Visit::Prune,
        3 => Visit::Stop(n),
        _ => Visit::Expand,
    });
    assert_eq!(found, Answer::Known(Some(3)));

    let found = reach(&budget(100), [2, 1], edges(graph), |n| match n {
        0 => Visit::Prune,
        _ => Visit::Stop(n),
    });
    assert_eq!(found, Answer::Known(Some(2)));

    // 2 is pushed from 0 and again from 1 before it is visited: visited once.
    let diamond: &[&[u32]] = &[&[1, 2], &[2], &[]];
    let b = budget(100);
    let mut order = Vec::new();
    let found = reach(&b, [0], edges(diamond), |n| {
        order.push(n);
        Visit::<()>::Expand
    });
    assert_eq!(found, Answer::Known(None));
    assert_eq!(order, [0, 1, 2]);
    assert_eq!(b.taken(), 3);
}

#[test]
fn reach_past_its_budget_is_open() {
    let graph: &[&[u32]] = &[&[1], &[]];
    let spent = budget(1);
    spent.spend();
    let expand = |_| Visit::<()>::Expand;
    assert_eq!(reach(&spent, [0], edges(graph), expand), Answer::Open);
    assert_eq!(reach(&budget(1), [0], edges(graph), expand), Answer::Open);
    assert_eq!(
        reach(&budget(2), [0], edges(graph), expand),
        Answer::Known(None)
    );
}

#[test]
fn reach_breadth_first_visits_nearer_nodes_first() {
    // 0 -> 1, 2; 1 -> 3; 2 -> 3; 3 -> 0.
    let graph: &[&[u32]] = &[&[1, 2], &[3], &[3], &[0]];
    let b = budget(100);
    let mut order = Vec::new();
    let found = reach_breadth_first(&b, [0], edges(graph), |n| {
        order.push(n);
        Visit::<()>::Expand
    });
    assert_eq!(found, Answer::Known(None));
    assert_eq!(order, [0, 1, 2, 3]);
    assert_eq!(b.taken(), 4);

    // A chain 0 -> 1 -> 2 -> 3 and a shortcut 0 -> 3: 3 is visited at distance 1.
    let shortcut: &[&[u32]] = &[&[1, 3], &[2], &[3], &[]];
    let mut order = Vec::new();
    let found = reach_breadth_first(&budget(100), [0], edges(shortcut), |n| {
        order.push(n);
        Visit::<()>::Expand
    });
    assert_eq!(found, Answer::Known(None));
    assert_eq!(order, [0, 1, 3, 2]);

    let found = reach_breadth_first(&budget(100), [0], edges(graph), |n| match n {
        1 => Visit::Prune,
        3 => Visit::Stop(n),
        _ => Visit::Expand,
    });
    assert_eq!(found, Answer::Known(Some(3)));

    // Starts listed twice are visited once, in their order.
    let mut order = Vec::new();
    let found = reach_breadth_first(&budget(100), [2, 1, 2], edges(graph), |n| {
        order.push(n);
        Visit::<()>::Prune
    });
    assert_eq!(found, Answer::Known(None));
    assert_eq!(order, [2, 1]);
}

#[test]
fn reach_breadth_first_past_its_budget_is_open() {
    let graph: &[&[u32]] = &[&[1], &[]];
    let spent = budget(1);
    spent.spend();
    let expand = |_| Visit::<()>::Expand;
    assert_eq!(
        reach_breadth_first(&spent, [0], edges(graph), expand),
        Answer::Open
    );
    assert_eq!(
        reach_breadth_first(&budget(1), [0], edges(graph), expand),
        Answer::Open
    );
    assert_eq!(
        reach_breadth_first(&budget(2), [0], edges(graph), expand),
        Answer::Known(None)
    );
}

// --------------------------------------------------------------------------------- fixpoint

/// Node `i` is read by node `i + 1`, and node 3 by node 0: a loop of four nodes.
fn ring(i: usize, out: &mut Vec<usize>) {
    out.push((i + 1) % 4);
}

#[test]
fn a_fixpoint_joins_facts_and_revisits_only_readers_of_growth() {
    let b = budget(100);
    let visits = Cell::new(0);
    let facts = fixpoint(
        &b,
        Sparse {
            nodes: 4,
            readers: &ring,
            max_visits: 4,
        },
        |i, facts: &Facts<Or>| {
            visits.set(visits.get() + 1);
            let before = facts.get((i + 3) % 4).copied().unwrap_or_default();
            Some(Or(before.0 | 1 << i))
        },
    );
    let facts = match facts {
        Answer::Known(facts) => facts,
        Answer::Open => panic!("the fixpoint ends"),
    };
    assert_eq!(facts.get(3), Some(&Or(0b1111)));
    assert_eq!(facts.get(9), None);
    assert_eq!(
        facts.clone().into_vec(),
        [
            Some(Or(0b1111)),
            Some(Or(0b1111)),
            Some(Or(0b1111)),
            Some(Or(0b1111))
        ]
    );
    // One visit each, then each again from node 0 (node 3 grew) to node 3, which does not grow.
    assert_eq!(visits.get(), 8);
}

#[test]
fn a_fixpoint_with_no_fact_keeps_none() {
    let facts = fixpoint(
        &budget(100),
        Sparse {
            nodes: 2,
            readers: &|_, _| {},
            max_visits: 1,
        },
        |_, _: &Facts<Or>| None,
    );
    assert_eq!(facts, Answer::Known(Facts(vec![None, None])));
}

#[test]
fn a_fixpoint_past_its_limits_is_open() {
    let grows = |i: usize, facts: &Facts<KeySet<usize>>| {
        let mut fact = facts.get(i).cloned().unwrap_or_default();
        fact.insert(fact.len());
        Some(fact)
    };
    let self_reader = |i: usize, out: &mut Vec<usize>| out.push(i);
    let spec = |max_visits| Sparse {
        nodes: 1,
        readers: &self_reader,
        max_visits,
    };
    // A transfer that never stops growing passes the visit limit.
    assert_eq!(fixpoint(&budget(100), spec(3), grows), Answer::Open);
    let spent = budget(1);
    spent.spend();
    assert_eq!(fixpoint(&spent, spec(3), grows), Answer::Open);
    // A step for the first visit, none for the fact it joins.
    assert_eq!(fixpoint(&budget(1), spec(3), grows), Answer::Open);
    // Steps for the first visit and its fact, none for the second visit.
    assert_eq!(fixpoint(&budget(2), spec(3), grows), Answer::Open);
    // Each visit takes one step and the weight of the fact it joins (1 + 1, 1 + 2, 1 + 3); the
    // fourth passes the visit limit before taking any.
    let b = budget(100);
    assert_eq!(fixpoint(&b, spec(3), grows), Answer::Open);
    assert_eq!(b.taken(), 2 + 3 + 4);
}

// ------------------------------------------------------------------------------ shared memo

/// A recording that reads `u32` keys: what an analysis read so far, and what it replayed.
#[derive(Default)]
struct Log {
    on: Cell<bool>,
    read: RefCell<Vec<u32>>,
    replayed: RefCell<Vec<Arc<Vec<u32>>>>,
}

impl ReadLog for Log {
    type Reads = Vec<u32>;

    fn recording(&self) -> bool {
        self.on.get()
    }

    fn isolated<A>(&self, decide: impl FnOnce() -> A) -> (A, Arc<Vec<u32>>) {
        let before = self.read.take();
        let answer = decide();
        (answer, Arc::new(self.read.replace(before)))
    }

    fn replay(&self, reads: Arc<Vec<u32>>) {
        self.replayed.borrow_mut().push(reads);
    }
}

#[test]
fn a_shared_memo_replays_what_deciding_read() {
    let mut memo: SharedMemo<u32, u32, Vec<u32>> = SharedMemo::new();
    let log = Log::default();
    let decided = Cell::new(0);
    let decide = |log: &Log, key: u32| {
        decided.set(decided.get() + 1);
        log.read.borrow_mut().push(key + 100);
        key * 2
    };

    // Not recording: decided once, nothing to replay.
    assert_eq!(memo.ask(&log, 1, || decide(&log, 1)), 2);
    assert_eq!(memo.ask(&log, 1, || decide(&log, 1)), 2);
    assert_eq!(decided.get(), 1);
    log.read.borrow_mut().clear();

    // Recording: an answer kept without its reads is decided again, to record them.
    log.on.set(true);
    log.read.borrow_mut().push(7);
    assert_eq!(memo.ask(&log, 1, || decide(&log, 1)), 2);
    assert_eq!(decided.get(), 2);
    assert_eq!(*log.read.borrow(), [7], "what was read before is kept");
    assert_eq!(*log.replayed.borrow(), [Arc::new(vec![101])]);

    // Kept with its reads: replayed, not decided.
    assert_eq!(memo.ask(&log, 1, || decide(&log, 1)), 2);
    assert_eq!(decided.get(), 2);
    assert_eq!(log.replayed.borrow().len(), 2);

    // Read while not recording: the answer, without a replay.
    log.on.set(false);
    assert_eq!(memo.ask(&log, 1, || decide(&log, 1)), 2);
    assert_eq!(log.replayed.borrow().len(), 2);

    memo.clear();
    assert_eq!(memo.ask(&log, 1, || decide(&log, 1)), 2);
    assert_eq!(decided.get(), 3);
}

// ------------------------------------------------------------------------------------ joins

#[test]
fn joins_report_growth() {
    let mut b = false;
    assert!(!b.join(&false));
    assert!(b.join(&true));
    assert!(!b.join(&true));
    assert_eq!(b.weight(), 1);

    let mut pair = (false, Or(1));
    assert!(pair.join(&(false, Or(2))));
    assert!(pair.join(&(true, Or(0))));
    assert!(!pair.join(&(true, Or(3))));
    assert_eq!(pair.weight(), 2);

    let mut min = Min(5);
    assert!(min.join(&Min(3)));
    assert!(!min.join(&Min(4)));
    assert_eq!(min, Min(3));

    let mut or = Or::default();
    assert!(or.join(&Or(0b101)));
    assert!(!or.join(&Or(0b001)));

    let mut and = And(0b111);
    assert!(and.join(&And(0b101)));
    assert!(!and.join(&And(0b111)));
    assert_eq!(and, And(0b101));
}

#[test]
fn a_key_set_is_sorted_without_duplicates() {
    let mut set: KeySet<u32> = [3, 1, 3, 2].into_iter().collect();
    assert_eq!(set.iter().copied().collect::<Vec<_>>(), [1, 2, 3]);
    assert!(!set.insert(2));
    assert!(set.insert(0));
    assert!(set.contains(&0));
    assert!(!set.contains(&9));
    assert_eq!(set.len(), 4);
    assert!(!set.is_empty());
    assert_eq!(set.iter().copied().collect::<Vec<_>>(), [0, 1, 2, 3]);
    assert!(set.join(&[2, 5, 6].into_iter().collect()));
    assert!(!set.join(&[0, 6].into_iter().collect()));
    assert!(set.join(&[4].into_iter().collect()));
    assert_eq!(
        set.iter().copied().collect::<Vec<_>>(),
        [0, 1, 2, 3, 4, 5, 6]
    );
    assert_eq!(set.weight(), 7);
    let empty = KeySet::<u32>::new();
    assert!(empty.is_empty());
    assert_eq!(empty.weight(), 1);
}

#[test]
fn a_join_map_keeps_one_entry_per_key() {
    let mut map = JoinMap::new();
    assert!(map.is_empty());
    assert!(map.insert(2, Min(9)));
    assert!(map.insert(2, Min(4)));
    assert!(!map.insert(2, Min(6)));
    assert!(map.insert(1, Min(1)));
    assert_eq!(map.get(&2), Some(&Min(4)));
    assert_eq!(map.len(), 2);
    assert_eq!(
        map.iter().map(|(k, v)| (*k, v.0)).collect::<Vec<_>>(),
        [(1, 1), (2, 4)]
    );
    let mut other = JoinMap::new();
    other.insert(2, Min(8));
    assert!(!map.join(&other));
    other.insert(3, Min(0));
    assert!(map.join(&other));
    assert_eq!(map.weight(), 3);
    assert_eq!(JoinMap::<u32, Min<u32>>::default().weight(), 1);
}

#[test]
fn a_bit_set_grows_by_words() {
    let mut set = BitSet::new();
    assert!(set.is_empty());
    assert!(set.insert(130));
    assert!(!set.insert(130));
    assert!(set.insert(3));
    assert!(!set.is_empty());
    assert!(set.contains(130));
    assert!(!set.contains(4));
    assert!(!set.contains(1_000));
    assert_eq!(set.iter().collect::<Vec<_>>(), [3, 130]);
    assert_eq!(set.weight(), 3);
    let mut small = BitSet::new();
    small.insert(1);
    assert!(small.join(&set));
    assert!(!small.join(&set));
    assert_eq!(small.iter().collect::<Vec<_>>(), [1, 3, 130]);
    assert_eq!(BitSet::default().weight(), 1);
}

#[test]
fn a_failure_absorbs_every_fact() {
    let mut f: Fallible<Or> = Fallible::default();
    assert_eq!(f, Fallible(Some(Or(0))));
    assert!(f.join(&Fallible(Some(Or(1)))));
    assert_eq!(f.weight(), 1);
    assert!(f.join(&Fallible::failed()));
    assert!(!f.join(&Fallible(Some(Or(2)))));
    assert_eq!(f, Fallible::failed());
    assert_eq!(f.weight(), 1);
}

// ---------------------------------------------------------------------------------- FnShape

/// Functions with phis, nested loops, exception edges, starred operands, constants, and a loop
/// no path enters whose copies close a cycle.
const SHAPES: &str = r#"
import os

def phis(x, flag):
    if flag:
        y = x + 1
    else:
        y = "text"
    z = y
    return z

def loops(items, n):
    total = 0
    for a in items:
        for b in range(n):
            if b:
                continue
            total = total + a * b
        while total > 10:
            total = total - 1
    return total

def handlers(path):
    try:
        data = os.read(path)
    except OSError:
        data = None
    finally:
        os.close(path)
    return data

def starred(xs, ys):
    f(*xs, 1, *ys)
    return [0, *xs]

def constants():
    return (1, "a", True, 2.5)

def dead(x):
    return x
    while True:
        y = x
        x = y
"#;

#[test]
fn shape_tables_answer_as_the_linear_helpers() {
    let ir = build(SHAPES);
    for f in &ir.functions {
        let shape = FnShape::new(f);
        let vars = (0..f.var_names.len() as u32 + 2).map(VarId);
        for v in vars {
            assert_eq!(shape.defining(v), f.def_inst(v), "{} {v:?}", f.qualname);
            assert_eq!(shape.inst(v), f.def_inst(v).map(|(_, i)| i));
            assert_eq!(shape.str_const(v), f.const_str(v));
            assert_eq!(shape.int_const(v), f.const_int(v));
            assert_eq!(shape.bool_const(v), f.const_bool(v));
            match shape.def(v) {
                Def::Inst(b, i) => assert_eq!(f.block(b).insts[i as usize].kind.dst(), Some(v)),
                Def::Phi(b, i) => assert_eq!(f.block(b).phis[i as usize].dst, v),
                Def::Param => assert!(f.params.iter().any(|p| p.var == v)),
                Def::None => {}
            }
            assert_eq!(
                shape.readers(v),
                readers_by_scan(f, v),
                "{} {v:?}",
                f.qualname
            );
            let starred: Vec<u32> = f
                .starred
                .iter()
                .filter(|(d, _)| *d == v)
                .map(|(_, i)| *i)
                .collect();
            assert_eq!(shape.first_starred(v), starred.iter().copied().min());
            for i in 0..4 {
                assert_eq!(shape.is_starred(v, i), f.is_starred(v, i));
            }
        }
        assert!(!shape.is_starred(VarId(0), usize::MAX));
        let preds = f.predecessors();
        assert_eq!(shape.rpo(), f.rpo_blocks());
        for bb in &f.blocks {
            let b = bb.id;
            assert_eq!(shape.preds(b), preds[b.0 as usize]);
            let mut succs = bb.terminator.successors();
            succs.extend(bb.exc_target);
            assert_eq!(shape.succs(b).collect::<Vec<_>>(), succs);
            assert_eq!(
                shape.rpo_index(b),
                f.rpo_blocks()
                    .iter()
                    .position(|x| *x == b)
                    .map(|i| i as u32)
            );
            assert_eq!(shape.reachable(b), shape.rpo_index(b).is_some());
            assert_eq!(shape.loop_depth(b), f.block_loop_depth(b));
            assert_eq!(shape.loop_depth(b) > 0, f.is_in_loop(b));
            assert_eq!(shape.innermost(b), f.innermost_loop(b));
            for (li, lp) in f.loops.iter().enumerate() {
                assert_eq!(shape.in_loop(li, b), lp.body.contains(&b));
                assert_eq!(shape.in_body(lp, b), lp.body.contains(&b));
            }
            for d in &f.blocks {
                assert_eq!(
                    shape.dominators().dominates(d.id, b),
                    dominates_by_walk(f, d.id, b),
                    "{} {:?} {b:?}",
                    f.qualname,
                    d.id
                );
            }
        }
        let outside = BlockId(f.blocks.len() as u32);
        assert!(!shape.in_loop(f.loops.len(), BlockId(0)));
        // A loop no block of the function heads holds no block.
        for lp in &f.loops {
            for header in [BlockId(0), outside] {
                let other = NaturalLoop {
                    header,
                    ..lp.clone()
                };
                assert!(f.blocks.iter().all(|bb| !shape.in_body(&other, bb.id)));
            }
        }
        assert_eq!(shape.innermost_index(outside), None);
        assert_eq!(shape.loop_depth(outside), 0);
        assert_eq!(shape.rpo_index(outside), None);
        assert!(shape.preds(outside).is_empty());
    }
    let nested = ir.functions.iter().find(|f| f.name == "loops").unwrap();
    assert!(nested.loops.iter().any(|lp| lp.depth == 2));
}

const CHAINS: &str = "def chains(x):\n    a = x\n    b = a\n    c = b\n    d = c\n    return d\n";

#[test]
fn copy_chains_end_at_their_root() {
    let ir = build(&format!("{SHAPES}\n{CHAINS}"));
    for f in &ir.functions {
        let shape = FnShape::new(f);
        for v in (0..f.var_names.len() as u32).map(VarId) {
            let mut cur = v;
            while let Some(InstKind::Copy { src, .. }) = f.def_inst(cur).map(|(_, i)| &i.kind) {
                cur = *src;
            }
            assert_eq!(shape.copy_root(v), cur, "{} {v:?}", f.qualname);
        }
        assert_eq!(shape.copy_root(VarId(u32::MAX)), VarId(u32::MAX));
    }
}

#[test]
fn a_cycle_of_copies_ends_at_its_lowest_value() {
    let ir = build(CHAINS);
    let mut f = ir
        .functions
        .iter()
        .find(|f| f.name == "chains")
        .unwrap()
        .clone();
    let named = |f: &crate::IrFunction, name: &str| {
        let v = f.var_names.iter().rposition(|n| n.as_deref() == Some(name));
        VarId(v.unwrap() as u32)
    };
    let (x, a, c, d) = (
        named(&f, "x"),
        named(&f, "a"),
        named(&f, "c"),
        named(&f, "d"),
    );
    // `a = c` in place of `a = x`: a, b, and c copy each other, and d copies c.
    for inst in f.blocks.iter_mut().flat_map(|b| &mut b.insts) {
        if let InstKind::Copy { dst, src, .. } = &mut inst.kind
            && *dst == a
        {
            *src = c;
        }
    }
    let shape = FnShape::new(&f);
    assert_eq!(shape.copy_root(d), a);
    assert_eq!(shape.copy_root(c), a);
    assert_eq!(shape.copy_root(a), a);
    assert_eq!(shape.copy_root(x), x);
}

#[test]
fn a_block_the_entry_does_not_reach_dominates_nothing() {
    let ir = build(SHAPES);
    let f = ir.functions.iter().find(|f| f.name == "dead").unwrap();
    let shape = FnShape::new(f);
    let dead = f
        .blocks
        .iter()
        .map(|b| b.id)
        .find(|b| !shape.reachable(*b))
        .unwrap();
    assert!(!shape.dominators().dominates(dead, dead));
    assert!(!shape.dominators().dominates(BlockId(0), dead));
    assert!(!shape.dominators().dominates(dead, BlockId(0)));
}

/// The phis, instructions, and terminators reading `v`, by a scan of every block.
fn readers_by_scan(f: &crate::IrFunction, v: VarId) -> Vec<Use> {
    let mut out = Vec::new();
    for bb in &f.blocks {
        for (i, phi) in bb.phis.iter().enumerate() {
            if phi.incoming.iter().any(|(_, x)| *x == v) {
                out.push(Use::Phi(bb.id, i as u32));
            }
        }
        for (i, inst) in bb.insts.iter().enumerate() {
            if inst.kind.operands().contains(&v) {
                out.push(Use::Inst(bb.id, i as u32));
            }
        }
        if let Terminator::Branch { cond: x, .. }
        | Terminator::Return(Some(x))
        | Terminator::Raise(Some(x)) = bb.terminator
            && x == v
        {
            out.push(Use::Terminator(bb.id));
        }
    }
    out
}

/// Every path from the entry to `b` runs through `d`, both reached from the entry: a walk that
/// avoids `d` does not find `b`.
fn dominates_by_walk(f: &crate::IrFunction, d: BlockId, b: BlockId) -> bool {
    let reached = |avoid: Option<BlockId>| {
        let mut seen = vec![false; f.blocks.len()];
        let mut stack = vec![BlockId(0)];
        while let Some(cur) = stack.pop() {
            if Some(cur) == avoid || std::mem::replace(&mut seen[cur.0 as usize], true) {
                continue;
            }
            let block = f.block(cur);
            stack.extend(block.terminator.successors());
            stack.extend(block.exc_target);
        }
        seen
    };
    let all = reached(None);
    all[d.0 as usize] && all[b.0 as usize] && (d == b || !reached(Some(d))[b.0 as usize])
}
