//! Django model managers and model bases through the ecosystem catalog: `Model.objects` and the
//! managers a model's body declares are `Manager`s whose query methods run what the catalog's
//! `QuerySet` methods run (`[[class_attributes]]`), and `Model` keeps what its subclasses' code
//! stores in the instance (`plain_attributes_except`).

mod common;

use common::Project;
use serde_json::Value;

/// `(line, rule with its tier, qualname, target)` of every diagnostic of `heki check` with
/// `rules` selected on the one-file project `rel` holding `source`.
fn check(name: &str, rel: &str, source: &str, rules: &str) -> Vec<(u64, String, String, String)> {
    let p = Project::new(name);
    p.write(rel, source);
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "--select", rules]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                match d["certainty"].as_str() {
                    Some("heuristic") => {
                        format!("{} [heuristic]", d["rule_code"].as_str().unwrap())
                    }
                    _ => d["rule_code"].as_str().unwrap().to_owned(),
                },
                d["qualname"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn d(line: u64, code: &str, qualname: &str, target: &str) -> (u64, String, String, String) {
    (line, code.into(), qualname.into(), target.into())
}

const MANAGERS: &str = r#"from django.db import models


class StreamManager(models.Manager):
    def live(self):
        return self.filter(live=True)


class Stream(models.Model):
    name = models.CharField(max_length=60)


class Custom(models.Model):
    objects = StreamManager()


class Extra(models.Model):
    published = StreamManager()


def tp_default_manager_get(ids: list[int]) -> None:
    for i in ids:
        Stream.objects.get(id=i)


def tp_declared_manager_get(ids: list[int]) -> None:
    for i in ids:
        Custom.objects.get_or_create(id=i)


def tp_class_method_manager(cls: type[Stream], ids: list[int]) -> None:
    for i in ids:
        cls.objects.count()


def tn_lazy_filter(ids: list[int]) -> None:
    for i in ids:
        Stream.objects.filter(id=i)
        Custom.objects.live()


def tn_other_manager_declared(ids: list[int]) -> None:
    for i in ids:
        Extra.objects.get(id=i)


def tn_once(i: int) -> None:
    Stream.objects.get(id=i)
"#;

/// A query through a model's default manager, or through one its body declares, per item is an
/// exact N+1; a lazy method or a project method building a lazy `QuerySet` sends nothing, and a
/// model declaring another manager gets no `objects`.
#[test]
fn manager_queries_per_item() {
    assert_eq!(
        check("django_managers", "app/models.py", MANAGERS, "EFF103"),
        vec![
            d(
                23,
                "EFF103",
                "tp_default_manager_get",
                "django.db.models.Manager.get"
            ),
            d(
                28,
                "EFF103",
                "tp_declared_manager_get",
                "django.db.models.Manager.get_or_create"
            ),
            d(
                33,
                "EFF103",
                "tp_class_method_manager",
                "django.db.models.Manager.count"
            ),
        ]
    );
}

const UNKNOWN_MANAGERS: &str = r#"from django.db import models
from vendor.managers import VendorManager


class Books(models.QuerySet):
    pass


class Vendored(models.Model):
    objects = VendorManager()


class FromQuerySet(models.Model):
    objects = Books.as_manager()


class Built(models.Model):
    custom = Books.as_manager()


class Related(models.Model):
    tags = models.ManyToManyField(Vendored)


def tn_unknown_managers(ids: list[int], related: list[Related]) -> None:
    for i in ids:
        Vendored.objects.get(id=i)
        FromQuerySet.objects.get(id=i)
        Built.objects.get(id=i)
    for r in related:
        r.tags.count()
"#;

/// A manager of a class heki does not know, one `as_manager()` builds, a model whose body may
/// declare one, and a related manager leave the calls unknown.
#[test]
fn unknown_managers_stay_unknown() {
    assert_eq!(
        check(
            "django_unknown_managers",
            "app/models.py",
            UNKNOWN_MANAGERS,
            "EFF103"
        ),
        vec![]
    );
}

const MODEL_WRITES: &str = r#"from functools import cached_property

from django.db import models


class Session(models.Model):
    def __init__(self, *args, **kwargs) -> None:
        super().__init__(*args, **kwargs)
        self.expires = None

    @cached_property
    def is_expired(self) -> bool:
        return self.expires is not None

    def expire(self, now) -> None:
        self.expires = now


class Field(models.Model):
    expires = models.DateTimeField(null=True)

    @cached_property
    def is_expired(self) -> bool:
        return self.expires is not None

    def expire(self, now) -> None:
        self.expires = now


class Keyed(models.Model):
    @cached_property
    def label(self) -> str:
        return str(self.pk)

    def reset(self) -> None:
        self.pk = None
"#;

/// `Model` defines no attribute hooks: a write of an attribute a model's code sets stores in
/// the instance, so the stale cached property is exact. A class-body field (Django replaces it
/// with a descriptor) and `pk` (a property of `Model`) keep the report heuristic.
#[test]
fn model_base_writes_are_plain() {
    assert_eq!(
        check(
            "django_model_writes",
            "app/models.py",
            MODEL_WRITES,
            "MUT101"
        ),
        vec![
            d(
                16,
                "MUT101",
                "Session.expire",
                "Session.expires->is_expired"
            ),
            d(
                27,
                "MUT101 [heuristic]",
                "Field.expire",
                "Field.expires->is_expired"
            ),
            d(36, "MUT101 [heuristic]", "Keyed.reset", "Keyed.pk->label"),
        ]
    );
}
