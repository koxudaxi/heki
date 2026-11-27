//! Project code behind Django's catalogued query methods: a project `QuerySet` subclass that
//! binds a name the methods look up on the queryset (`delegates_to`) runs its own code under
//! them, and a model's `save` runs under `QuerySet.create` (`effect_through`), unless it calls
//! `super().save(...)` on every path. `models.QuerySet` names the catalog's `QuerySet`.

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

const QS: &str = "django.db.models.query.QuerySet";

const REPLACED_METHOD: &str = r#"from django.db import models


class LookupQuerySet(models.QuerySet):
    def get(self, *args, **kwargs):
        return None


class Item(models.Model):
    name = models.CharField(max_length=10)


def tp_lookup(groups: list[int]) -> None:
    for g in groups:
        Item.objects.filter(group=g).get(id=1)


def tp_param(qs: LookupQuerySet, groups: list[int]) -> None:
    for g in groups:
        qs.update(group=g)


async def tp_async_lookup(groups: list[int]) -> None:
    for g in groups:
        await Item.objects.filter(group=g).aget(id=1)


def tn_update(groups: list[int]) -> None:
    for g in groups:
        Item.objects.filter(group=g).update(name="x")


def tn_manager(groups: list[int]) -> None:
    for g in groups:
        Item.objects.get(id=g)
"#;

/// A `models.QuerySet` subclass replacing `get`: a queryset of the catalog's class may be one,
/// so `get` and `aget` (which calls `self.get`) on it are heuristic, while `update`, which looks
/// up no `get`, and the default manager's `get` stay exact; the subclass's own instances run
/// the catalog's `update`.
#[test]
fn a_queryset_subclass_replacing_a_method_opens_it() {
    assert_eq!(
        check(
            "django_qs_replaced",
            "app/models.py",
            REPLACED_METHOD,
            "EFF103"
        ),
        vec![
            d(15, "EFF103 [heuristic]", "tp_lookup", &format!("{QS}.get")),
            d(20, "EFF103", "tp_param", &format!("{QS}.update")),
            d(
                25,
                "EFF103 [heuristic]",
                "tp_async_lookup",
                &format!("{QS}.aget")
            ),
            d(30, "EFF103", "tn_update", &format!("{QS}.update")),
            d(35, "EFF103", "tn_manager", "django.db.models.Manager.get"),
        ]
    );
}

const REPLACED_DELEGATE: &str = r#"from django.db import models
from django.db.models.query import QuerySet


class CachedQuerySet(QuerySet):
    def _fetch_all(self):
        if self._result_cache is None:
            self._result_cache = []


class Item(models.Model):
    name = models.CharField(max_length=10)


def tp_lookup(groups: list[int]) -> None:
    for g in groups:
        Item.objects.filter(group=g).get(id=1)
        Item.objects.filter(group=g).update(name="x")


def tn_manager(groups: list[int]) -> None:
    for g in groups:
        Item.objects.get(id=g)
"#;

/// A subclass binding `_fetch_all`, which `get` reaches, and storing `_result_cache`, which
/// `update` reads: both on a queryset of the catalog's class are heuristic; the manager's stay
/// exact.
#[test]
fn a_queryset_subclass_binding_a_delegate_opens_the_query_methods() {
    assert_eq!(
        check(
            "django_qs_delegate",
            "app/models.py",
            REPLACED_DELEGATE,
            "EFF103"
        ),
        vec![
            d(17, "EFF103 [heuristic]", "tp_lookup", &format!("{QS}.get")),
            d(
                18,
                "EFF103 [heuristic]",
                "tp_lookup",
                &format!("{QS}.update")
            ),
            d(23, "EFF103", "tn_manager", "django.db.models.Manager.get"),
        ]
    );
}

const ADDED_METHODS: &str = r#"from django.db import models


class PublishedQuerySet(models.QuerySet):
    def live(self):
        return self.filter(live=True)


class Item(models.Model):
    name = models.CharField(max_length=10)


def tp_lookup(groups: list[int]) -> None:
    for g in groups:
        Item.objects.filter(group=g).get(id=1)


def tp_subclass(qs: PublishedQuerySet, groups: list[int]) -> None:
    for g in groups:
        qs.live().get(id=g)
        qs.get(id=g)
"#;

/// A subclass that only adds methods keeps the catalog's rows, on its own instances too.
#[test]
fn a_queryset_subclass_adding_methods_keeps_the_rows() {
    assert_eq!(
        check("django_qs_added", "app/models.py", ADDED_METHODS, "EFF103"),
        vec![
            d(15, "EFF103", "tp_lookup", &format!("{QS}.get")),
            d(20, "EFF103", "tp_subclass", &format!("{QS}.get")),
            d(21, "EFF103", "tp_subclass", &format!("{QS}.get")),
        ]
    );
}

const CHAINED_SAVES: &str = r#"from django.db import models


class Stamped(models.Model):
    def save(self, *args, **kwargs):
        if not self.pk:
            self.created = True
        super().save(*args, **kwargs)


class OldStyle(models.Model):
    def save(self, *args, **kwargs):
        self.touched = True
        return super(OldStyle, self).save(*args, **kwargs)


def tp_create(groups: list[int]) -> None:
    for g in groups:
        Stamped.objects.create(group=g)


def tp_queryset_create(groups: list[int]) -> None:
    for g in groups:
        OldStyle.objects.filter(group=g).create(group=g)
"#;

/// A model's `save` that calls `super().save(...)` (or `super(Model, self).save(...)`) on every
/// path still inserts the row: `create` stays exact.
#[test]
fn saves_calling_super_on_every_path_keep_create() {
    assert_eq!(
        check(
            "django_saves_chained",
            "app/models.py",
            CHAINED_SAVES,
            "EFF103"
        ),
        vec![
            d(19, "EFF103", "tp_create", "django.db.models.Manager.create"),
            d(24, "EFF103", "tp_queryset_create", &format!("{QS}.create")),
        ]
    );
}

const REPLACED_SAVE: &str = r#"from django.db import models


class Draft(models.Model):
    def save(self, *args, **kwargs):
        if self.draft:
            return
        super().save(*args, **kwargs)

    def delete(self, *args, **kwargs):
        self.deleted = True


def tp_create(groups: list[int]) -> None:
    for g in groups:
        Draft.objects.create(group=g)


async def tp_async_create(groups: list[int]) -> None:
    for g in groups:
        await Draft.objects.acreate(group=g)


def tn_get_or_create(groups: list[int]) -> None:
    for g in groups:
        Draft.objects.get_or_create(group=g)


def tn_queryset_delete(groups: list[int]) -> None:
    for g in groups:
        Draft.objects.filter(group=g).delete()
"#;

/// A `save` that may return without calling `super().save()` may insert nothing: `create` and
/// `acreate` are heuristic. `get_or_create` queries by `get` first on every call, and
/// `QuerySet.delete` never calls the model's `delete`, so both stay exact.
#[test]
fn a_save_returning_early_opens_create() {
    assert_eq!(
        check(
            "django_save_replaced",
            "app/models.py",
            REPLACED_SAVE,
            "EFF103"
        ),
        vec![
            d(
                16,
                "EFF103 [heuristic]",
                "tp_create",
                "django.db.models.Manager.create"
            ),
            d(
                21,
                "EFF103 [heuristic]",
                "tp_async_create",
                "django.db.models.Manager.acreate"
            ),
            d(
                26,
                "EFF103",
                "tn_get_or_create",
                "django.db.models.Manager.get_or_create"
            ),
            d(31, "EFF103", "tn_queryset_delete", &format!("{QS}.delete")),
        ]
    );
}

const MANAGER_ALIAS: &str = r#"from django.db import models
from django.db.models.manager import Manager


class LiveManager(Manager):
    def get_queryset(self):
        return super().get_queryset().none()


def tp_param(manager: models.Manager, groups: list[int]) -> None:
    for g in groups:
        manager.get(id=g)
"#;

/// `django.db.models.manager.Manager` is the catalog's `models.Manager`: a manager deriving
/// from it that binds `get_queryset` opens a manager's query methods.
#[test]
fn a_manager_imported_from_its_module_is_the_catalog_manager() {
    assert_eq!(
        check(
            "django_manager_alias",
            "app/models.py",
            MANAGER_ALIAS,
            "EFF103"
        ),
        vec![d(
            12,
            "EFF103 [heuristic]",
            "tp_param",
            "django.db.models.Manager.get"
        )]
    );
}

const LIBRARY_MIXIN: &str = r#"from django.db import models
from stamps import StampMixin


class Stamped(StampMixin, models.Model):
    name = models.CharField(max_length=10)


class Item(models.Model):
    name = models.CharField(max_length=10)


def tp_create(groups: list[int]) -> None:
    for g in groups:
        Item.objects.create(group=g)
"#;

/// A library class before `models.Model` along a model's MRO may define `save`: `create` on
/// any manager, which may be that model's, is heuristic.
#[test]
fn a_library_mixin_before_the_model_base_opens_create() {
    assert_eq!(
        check(
            "django_save_mixin",
            "app/models.py",
            LIBRARY_MIXIN,
            "EFF103"
        ),
        vec![d(
            15,
            "EFF103 [heuristic]",
            "tp_create",
            "django.db.models.Manager.create"
        )]
    );
}

const STAR_IMPORTED_SUPER: &str = r#"from django.db import models
from helpers import *


class Stamped(models.Model):
    def save(self, *args, **kwargs):
        super().save(*args, **kwargs)


def tp_create(groups: list[int]) -> None:
    for g in groups:
        Stamped.objects.create(group=g)
"#;

/// A star import may bind `super`: the call in `save` is not known to be the next `save`.
#[test]
fn a_star_import_may_bind_super() {
    assert_eq!(
        check(
            "django_save_star",
            "app/models.py",
            STAR_IMPORTED_SUPER,
            "EFF103"
        ),
        vec![d(
            12,
            "EFF103 [heuristic]",
            "tp_create",
            "django.db.models.Manager.create"
        )]
    );
}
