//! `MUT118`: a `copy.deepcopy` call that copies an instance holding an `object()` sentinel in an
//! attribute that a function tests against that sentinel by identity. The copy holds a new
//! object in the attribute, which the test does not match.
//!
//! The deep copy is in a checked file; the sentinel, the class holding it, and the tests may be
//! anywhere in the project. They come from the declarations ([`ProjectDecls::held_sentinels`],
//! [`ProjectDecls::copy_of`]) and the summaries' local facts (`sentinel_tests`,
//! `deep_copies`), never another file's IR.

use crate::{CheckContext, ModuleUnit};
use heki_ast::Span;
use heki_diagnostics::{Certainty, Diagnostic, TextEdit};
use heki_effect::resolver::object_sentinel_calls;
use heki_effect::{
    CopyMade, DeepCopySite, FuncKey, HeldSentinel, ProjectDecls, ProjectIndex, SentinelTest,
};
use heki_env::RuntimeProfile;
use heki_ir::walk::{Answer, Budget, Closure, Join};
use heki_kb::CopyDepth;
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The most tests one report lists as related locations: a sentinel may be tested in thousands
/// of functions, and each deep copy reports once.
const MAX_RELATED_TESTS: usize = 5;

/// The owner of the walk below the copied objects' attributes.
const OWNER: &str = "heki_rules::mut_rules::sentinel_copies";

/// A comparison of an attribute with a sentinel by identity, with where it is.
struct TestAt<'a> {
    path: &'a Path,
    func: &'a FuncKey,
    test: &'a SentinelTest,
}

/// Every sentinel test of the project, by attribute and sentinel, in the order of their files,
/// functions, and places.
type Tests<'a> = BTreeMap<(String, String), Vec<TestAt<'a>>>;

fn tests_of(index: &ProjectIndex) -> Tests<'_> {
    let mut out = Tests::new();
    for (path, file) in &index.files {
        let defined = file
            .local_functions
            .iter()
            .filter(|(func, _)| index.file_of(func) == Some(path.as_path()));
        for (func, summary) in defined {
            for test in summary.sentinel_test_sites() {
                out.entry((test.attr.clone(), test.sentinel.clone()))
                    .or_default()
                    .push(TestAt { path, func, test });
            }
        }
    }
    for tests in out.values_mut() {
        tests.sort_by(|a, b| (a.path, a.test.span).cmp(&(b.path, b.test.span)));
    }
    out
}

/// The tests of one sentinel in one attribute of one class: those whose object may be an
/// instance of the class (at most [`MAX_RELATED_TESTS`], the known ones first), and whether one
/// is known to read the attribute on its instances.
struct Tested<'t> {
    tests: Vec<&'t TestAt<'t>>,
    exact: bool,
}

/// A claim one copied class gives, before the deep copy's own premises: the class holding the
/// sentinel and whether the copied object is surely one (its class is that class or a subclass)
/// or possibly one (that class is a subclass of its class).
struct Candidate {
    class: String,
    surely: bool,
    held: HeldSentinel,
    /// The copied object's attribute that may hold the instance, itself or in what it holds
    /// ([`Facts::below`]); `None` when the copied object is the instance.
    below: Option<String>,
    /// Every premise but the deep copy's own (one argument of one class) is established.
    exact: bool,
}

/// What the claims read of the project, each answer once per run: a sentinel may be held by a
/// class of thousands of subclasses, tested and deep-copied in thousands of functions.
struct Facts<'c, 't> {
    cx: &'c CheckContext<'c>,
    tests: &'t Tests<'t>,
    candidates: FxHashMap<String, Vec<Candidate>>,
    tested: FxHashMap<(String, String, String), Tested<'t>>,
    ancestors: FxHashMap<String, Vec<String>>,
    plain: FxHashMap<(String, String), bool>,
    in_attributes: FxHashMap<String, Vec<(String, String)>>,
    held: FxHashMap<String, Vec<HeldSentinel>>,
    subclasses: FxHashMap<String, Vec<String>>,
    /// The classes the walk below the attributes has met, by number (`Closure` keys).
    ids: FxHashMap<String, usize>,
    names: Vec<String>,
    nearest: Closure<usize, Nearest>,
    /// The walk over the declarations expands each class once per run: a budget spent in
    /// another order by `heki check` and by the language server would make them disagree.
    budget: Budget,
}

/// A class holding a tested sentinel, the first by name and sentinel of those a class reaches
/// through its attributes ([`Facts::below`]).
#[derive(Debug, Clone, Default, PartialEq)]
struct Nearest(Option<(String, HeldSentinel)>);

impl Join for Nearest {
    fn join(&mut self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (_, None) => false,
            (Some(mine), Some(theirs)) if mine <= theirs => false,
            _ => {
                self.0.clone_from(&other.0);
                true
            }
        }
    }
}

impl<'c, 't> Facts<'c, 't> {
    fn decls(&self) -> &'c ProjectDecls {
        self.cx.decls
    }

    fn ancestors(&mut self, class: &str) -> &[String] {
        let decls = self.cx.decls;
        self.ancestors
            .entry(class.to_owned())
            .or_insert_with(|| decls.ancestors_of(class))
    }

    fn held(&mut self, class: &str) -> Vec<HeldSentinel> {
        let (decls, kb) = (self.cx.decls, self.cx.kb);
        self.held
            .entry(class.to_owned())
            .or_insert_with(|| decls.held_sentinels(class, kb))
            .clone()
    }

    fn subclasses(&mut self, class: &str) -> Vec<String> {
        let decls = self.cx.decls;
        self.subclasses
            .entry(class.to_owned())
            .or_insert_with(|| decls.subclasses_of(class))
            .clone()
    }

    fn plain(&mut self, class: &str, attr: &str) -> bool {
        let (decls, kb) = (self.cx.decls, self.cx.kb);
        *self
            .plain
            .entry((class.to_owned(), attr.to_owned()))
            .or_insert_with(|| decls.plain_sentinel_attr(class, attr, kb))
    }

    /// The claims a deep copy of an object of `copied` gives (none when a copy hook of its
    /// own decides the copy).
    fn candidates(&mut self, copied: &str) -> &[Candidate] {
        if !self.candidates.contains_key(copied) {
            let found = self.find_candidates(copied);
            self.candidates.insert(copied.to_owned(), found);
        }
        &self.candidates[copied]
    }

    fn find_candidates(&mut self, copied: &str) -> Vec<Candidate> {
        let decls = self.decls();
        let deep = decls.copy_of(copied, CopyDepth::Deep, self.cx.kb);
        if deep == CopyMade::Hooked {
            return Vec::new();
        }
        // The object is an instance of the classes along its class's MRO, and may be one of a
        // subclass.
        let surely = std::iter::once(copied.to_owned())
            .chain(self.ancestors(copied).iter().cloned())
            .map(|c| (c, true))
            .collect::<Vec<_>>();
        let maybe = self.subclasses(copied).into_iter().map(|c| (c, false));
        let mut out = Vec::new();
        for (class, surely) in surely.into_iter().chain(maybe) {
            for held in self.held(&class) {
                let Some(tested) = self.tested(&class, &held.attr, &held.sentinel) else {
                    continue;
                };
                let exact = surely
                    && held.stored
                    && deep == CopyMade::Attributes
                    && tested.exact
                    && self.plain(copied, &held.attr);
                out.push(Candidate {
                    class: class.clone(),
                    surely,
                    held,
                    below: None,
                    exact,
                });
            }
        }
        // An instance held below the copied object's attributes, by their types: what they hold
        // at run time may be another object, so these claims are heuristic.
        if let Some((attr, (class, held))) = self.below(copied) {
            out.push(Candidate {
                class,
                surely: false,
                held,
                below: Some(attr),
                exact: false,
            });
        }
        out.sort_by(|a, b| {
            let rank = |c: &Candidate| (!c.exact, c.below.is_some(), !c.surely);
            (rank(a), &a.class, &a.held).cmp(&(rank(b), &b.class, &b.held))
        });
        out
    }

    /// The class holding a tested sentinel that an object of `copied` may hold below its
    /// attributes, with the attribute of `copied` it is in or below: the first by name and
    /// sentinel ([`Nearest`]) over the attributes, each joining what the classes it may hold reach
    /// through theirs ([`ProjectDecls::classes_in_attributes`]), once per class and run.
    fn below(&mut self, copied: &str) -> Option<(String, (String, HeldSentinel))> {
        let children = self.in_attributes(copied);
        let nearest = std::mem::take(&mut self.nearest);
        let budget = std::mem::replace(&mut self.budget, Budget::unmetered(OWNER));
        let mut found: Option<(String, (String, HeldSentinel))> = None;
        {
            let facts = RefCell::new(&mut *self);
            for (child, attr) in children {
                let start = facts.borrow_mut().id(&child);
                let fact = nearest.ask(
                    &budget,
                    start,
                    |k| facts.borrow_mut().holder(k),
                    |k, out| facts.borrow_mut().held_ids(k, out),
                );
                if let Answer::Known(fact) = fact
                    && let Some(holder) = &fact.0
                    && found.as_ref().is_none_or(|(_, best)| holder < best)
                {
                    found = Some((attr, holder.clone()));
                }
            }
        }
        self.nearest = nearest;
        self.budget = budget;
        found
    }

    fn in_attributes(&mut self, class: &str) -> Vec<(String, String)> {
        let decls = self.cx.decls;
        self.in_attributes
            .entry(class.to_owned())
            .or_insert_with(|| decls.classes_in_attributes(class))
            .clone()
    }

    fn id(&mut self, class: &str) -> usize {
        if let Some(&id) = self.ids.get(class) {
            return id;
        }
        self.names.push(class.to_owned());
        self.ids.insert(class.to_owned(), self.names.len() - 1);
        self.names.len() - 1
    }

    /// The classes an object of the class numbered `k` may hold in its attributes.
    fn held_ids(&mut self, k: usize, out: &mut Vec<usize>) {
        let class = self.names[k].clone();
        for (held, _) in self.in_attributes(&class) {
            out.push(self.id(&held));
        }
    }

    /// The first class by name and sentinel that holds a tested sentinel among those an object
    /// typed by the class numbered `k` is an instance of or may be (its MRO, its subclasses).
    fn holder(&mut self, k: usize) -> Nearest {
        let class = self.names[k].clone();
        let mut family = vec![class.clone()];
        family.extend(self.ancestors(&class).iter().cloned());
        family.extend(self.subclasses(&class));
        let mut out = Nearest::default();
        for c in family {
            for held in self.held(&c) {
                if self.tested(&c, &held.attr, &held.sentinel).is_some() {
                    out.join(&Nearest(Some((c.clone(), held))));
                }
            }
        }
        out
    }

    /// The tests of `sentinel` in `attr` whose object may be an instance of `class`: an
    /// instance of `class` or of a subclass (one class, under R2) reading the attribute without
    /// running code is known to; one of a base class of `class` may be.
    fn tested(&mut self, class: &str, attr: &str, sentinel: &str) -> Option<&Tested<'t>> {
        let key = (class.to_owned(), attr.to_owned(), sentinel.to_owned());
        if !self.tested.contains_key(&key) {
            let tests = self.tests;
            let mut found: Vec<(bool, &'t TestAt<'t>)> = Vec::new();
            for t in tests
                .get(&(key.1.clone(), key.2.clone()))
                .into_iter()
                .flatten()
            {
                let on = t.test.class_qualname.as_str();
                let below = on == class || self.ancestors(on).iter().any(|c| c == class);
                let above = !below && self.ancestors(class).iter().any(|c| c == on);
                if below || above {
                    let exact = below && t.test.only && self.plain(on, attr);
                    found.push((exact, t));
                }
            }
            // The known tests first, in their order otherwise (a stable sort).
            found.sort_by_key(|(exact, _)| !exact);
            let tested = Tested {
                exact: found.first().is_some_and(|(exact, _)| *exact),
                tests: found
                    .into_iter()
                    .take(MAX_RELATED_TESTS)
                    .map(|(_, t)| t)
                    .collect(),
            };
            self.tested.insert(key.clone(), tested);
        }
        self.tested.get(&key).filter(|t| !t.tests.is_empty())
    }
}

/// `MUT118` at the deep copies of the checked files.
pub(crate) fn mut118(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    let sites: Vec<(&ModuleUnit<'_>, &FuncKey, &DeepCopySite)> = cx
        .units
        .iter()
        .filter(|unit| cx.enabled("MUT118", unit))
        .flat_map(|unit| {
            let path = unit.input.path.as_path();
            cx.index
                .files
                .get(path)
                .into_iter()
                .flat_map(|file| &file.local_functions)
                .filter(move |(func, _)| cx.index.file_of(func) == Some(path))
                .flat_map(move |(func, s)| s.deep_copy_sites().map(move |site| (unit, func, site)))
        })
        .collect();
    if sites.is_empty() {
        return;
    }
    let tests = tests_of(cx.index);
    if tests.is_empty() {
        return;
    }
    let mut facts = Facts {
        cx,
        tests: &tests,
        candidates: FxHashMap::default(),
        tested: FxHashMap::default(),
        ancestors: FxHashMap::default(),
        plain: FxHashMap::default(),
        in_attributes: FxHashMap::default(),
        held: FxHashMap::default(),
        subclasses: FxHashMap::default(),
        ids: FxHashMap::default(),
        names: Vec::new(),
        nearest: Closure::new(),
        budget: Budget::unmetered(OWNER),
    };
    let mut sentinel_calls = SentinelCalls::default();
    for (unit, func, site) in sites {
        // One argument of one class: the deep copy's own premises.
        let one = site.only && site.classes.len() == 1;
        let mut best: Option<(&str, bool)> = None;
        for copied in &site.classes {
            if let Some(c) = facts.candidates(copied).first() {
                let exact = c.exact && one;
                if best.is_none_or(|(_, e)| exact && !e) {
                    best = Some((copied.as_str(), exact));
                }
            }
        }
        let Some((copied, exact)) = best else {
            continue;
        };
        let candidate = &facts.candidates[copied][0];
        let tested = &facts.tested[&(
            candidate.class.clone(),
            candidate.held.attr.clone(),
            candidate.held.sentinel.clone(),
        )];
        let fix = sentinel_fix(
            &mut sentinel_calls,
            unit,
            &candidate.held.sentinel,
            cx.runtime,
        );
        report(
            cx,
            unit,
            func,
            site,
            candidate,
            &tested.tests,
            exact,
            fix,
            out,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn report(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    func: &FuncKey,
    site: &DeepCopySite,
    claim: &Candidate,
    tests: &[&TestAt<'_>],
    exact: bool,
    fix: Option<Vec<TextEdit>>,
    out: &mut Vec<Diagnostic>,
) {
    let class = cx
        .decls
        .class_qualname(&claim.class)
        .unwrap_or(&claim.class)
        .to_owned();
    let attr = &claim.held.attr;
    let sentinel = claim
        .held
        .sentinel
        .rsplit('.')
        .next()
        .unwrap_or(&claim.held.sentinel);
    let tester = &tests[0].func.qualname;
    let copied = match (&claim.below, claim.surely) {
        (Some(below), _) => {
            format!("may copy an instance of `{class}` held in `{below}` or in what it holds")
        }
        (None, true) => format!("receives an instance of `{class}`"),
        (None, false) => format!("may receive an instance of `{class}`"),
    };
    let related = tests
        .iter()
        .map(|t| {
            cx.related_at(
                t.path,
                t.test.span,
                format!(
                    "`{}` tests `{attr}` against `{sentinel}` by identity",
                    t.func.qualname
                ),
            )
        })
        .collect();
    let span: Span = site.span;
    let d = cx
        .emit(
        out,
        "MUT118",
        unit,
        span,
        &func.qualname,
        format!("{class}.{attr}"),
        format!(
            "`copy.deepcopy` {copied}, whose `{attr}` can hold the `object()` sentinel `{sentinel}` that `{tester}` tests by identity: the copy holds a new object there, which the test does not match"
        ),
        Some(sentinel_help(sentinel, cx.runtime)),
        &[],
    )
        .expect("the deep copies are those of the units the rule runs on");
    d.related = related;
    d.fix = fix;
    let tier = if exact {
        Certainty::Exact
    } else {
        Certainty::Heuristic
    };
    d.certainty = tier.at_most(d.certainty);
}

/// The help of a report on the sentinel `name`: a PEP 661 sentinel of that name, which `copy`
/// and `copy.deepcopy` return as it is, the builtin from Python 3.15 and the `typing_extensions`
/// function (from its 4.16.0, which first spells it so) below it.
fn sentinel_help(name: &str, runtime: RuntimeProfile) -> String {
    let call = if runtime.pep661_builtin_sentinel {
        format!("`sentinel(\"{name}\")`")
    } else {
        format!("`typing_extensions.sentinel(\"{name}\")` (typing_extensions 4.16.0 or later)")
    };
    format!("bind `{name}` to {call} instead of `object()`: a copy keeps it as the same object")
}

/// The `object()` calls of the checked files that a call of the builtin `sentinel` can replace
/// ([`object_sentinel_calls`]), found once per file.
type SentinelCalls<'u> = FxHashMap<&'u Path, FxHashMap<String, Span>>;

/// The unsafe fix of a report on the sentinel `fqn` when the target has the builtin `sentinel`
/// and the report's own file binds the sentinel (a fix edits that file only): its `object()`
/// call becomes `sentinel("NAME")` of the name it is bound to, which `pickle` also finds the
/// object by. The object's type and repr change, and so does what `pickle` makes of it.
fn sentinel_fix<'u>(
    calls: &mut SentinelCalls<'u>,
    unit: &'u ModuleUnit<'_>,
    fqn: &str,
    runtime: RuntimeProfile,
) -> Option<Vec<TextEdit>> {
    if !runtime.pep661_builtin_sentinel {
        return None;
    }
    let name = fqn
        .strip_prefix(unit.input.module_name.as_str())?
        .strip_prefix('.')?;
    let span = *calls
        .entry(unit.input.path.as_path())
        .or_insert_with(|| object_sentinel_calls(unit.input.ir))
        .get(name)?;
    Some(vec![TextEdit {
        span,
        replacement: format!("sentinel(\"{name}\")"),
    }])
}

/// What a sentinel test gives `MUT118` apart from where it is: the testing function (its reports
/// name it), the class, the attribute, the sentinel, and whether the object is of that one class.
pub type SentinelTestKey = (String, String, String, String, bool);

/// The sentinel tests of the functions defined in `path`, without their places. The language
/// server re-checks the files with deep copies ([`deep_copy_files`]) when they change.
pub fn sentinel_tests_in(index: &ProjectIndex, path: &Path) -> BTreeSet<SentinelTestKey> {
    index
        .files
        .get(path)
        .into_iter()
        .flat_map(|file| &file.local_functions)
        .filter(|(func, _)| index.file_of(func) == Some(path))
        .flat_map(|(func, s)| s.sentinel_test_sites().map(move |t| (func, t)))
        .map(|(func, t)| {
            (
                func.qualname.clone(),
                t.class_qualname.clone(),
                t.attr.clone(),
                t.sentinel.clone(),
                t.only,
            )
        })
        .collect()
}

/// The files with a function calling `copy.deepcopy` on an object of a project class: those a
/// `MUT118` report can be at.
pub fn deep_copy_files(index: &ProjectIndex) -> Vec<PathBuf> {
    index
        .files
        .iter()
        .filter(|(path, file)| {
            file.local_functions.iter().any(|(func, s)| {
                index.file_of(func) == Some(path.as_path()) && s.deep_copy_sites().next().is_some()
            })
        })
        .map(|(path, _)| path.clone())
        .collect()
}
