//! Attribute types of library classes end to end (`[[attributes]]` rows, here declared with
//! `attribute-type` in `[[tool.heki.ecosystem]]`): a read of a catalogued attribute on an
//! instance of the library class has the row's class, so a chain of reads reaches a catalogued
//! call. The read stays untyped, and neither tier reports the call, wherever the row is not
//! established: a project class the value may be binds the name or defines
//! `__getattribute__`, a base heki knows nothing of comes first, the receiver is a union, an
//! untyped value, a class object, or the result of an unknown factory. The libraries are not
//! installed: heki resolves imports by name.

mod common;

use common::Project;
use serde_json::Value;

/// A synchronous and an asynchronous client of one library, whose `chat.completions.create`
/// waits on the network on the synchronous side only, and a fluent builder on each.
const CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.ecosystem]]
symbol = "acme.Completions.create"
effects = ["blocking_net"]

[[tool.heki.ecosystem]]
symbol = "acme.AsyncCompletions.create"
async-safe = true

[[tool.heki.ecosystem]]
symbol = "acme.Client.chat"
attribute-type = "acme.Chat"

[[tool.heki.ecosystem]]
symbol = "acme.AsyncClient.chat"
attribute-type = "acme.AsyncChat"

[[tool.heki.ecosystem]]
symbol = "acme.Chat.completions"
attribute-type = "acme.Completions"

[[tool.heki.ecosystem]]
symbol = "acme.AsyncChat.completions"
attribute-type = "acme.AsyncCompletions"

[[tool.heki.ecosystem]]
symbol = "acme.Client.with_options"
return-type = "Self"

[[tool.heki.ecosystem]]
symbol = "acme.AsyncClient.with_options"
return-type = "Self"
"#;

/// `(path, line, rule, qualname)` of every `EFF101` report of `heki check` on the project holding
/// `files`, the rule with its tier ([`common::tiered_code`]).
fn check(name: &str, files: &[(&str, &str)]) -> Vec<(String, u64, String, String)> {
    let p = Project::new(name);
    p.write("pyproject.toml", CONFIG);
    for (rel, text) in files {
        p.write(rel, text);
    }
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "--select", "EFF101"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                common::tiered_code(d).to_owned(),
                d["qualname"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn d(path: &str, line: u64, rule: &str, qualname: &str) -> (String, u64, String, String) {
    (path.into(), line, rule.into(), qualname.into())
}

const SERVICE: &str = r#"import random
from typing import Any

from acme import AsyncClient, Client, make_client

sync_client = Client()
async_client = AsyncClient()


async def module_client() -> None:
    sync_client.chat.completions.create(model="m")


async def built() -> None:
    sync_client.with_options(timeout=1).chat.completions.create(model="m")


async def alias() -> None:
    chat = sync_client.chat
    chat.completions.create(model="m")


async def annotated(c: Client) -> None:
    c.chat.completions.create(model="m")


class Service:
    def __init__(self) -> None:
        self.client = Client()

    async def handle(self) -> None:
        self.client.chat.completions.create(model="m")


async def asynchronous() -> None:
    await async_client.chat.completions.create(model="m")
    async_client.chat.completions.create(model="m")
    async_client.with_options(timeout=1).chat.completions.create(model="m")


async def union(c: Client | AsyncClient) -> None:
    c.chat.completions.create(model="m")


async def joined() -> None:
    c = Client() if random.random() else AsyncClient()
    c.chat.completions.create(model="m")


async def untyped(c: Any) -> None:
    c.chat.completions.create(model="m")


async def unannotated(c) -> None:
    c.chat.completions.create(model="m")


async def factory() -> None:
    make_client().chat.completions.create(model="m")


async def class_object() -> None:
    Client.chat.completions.create(model="m")


async def local_overwrite() -> None:
    sync_client = AsyncClient()
    sync_client.chat.completions.create(model="m")


def sync_caller() -> None:
    sync_client.chat.completions.create(model="m")
"#;

/// The synchronous client's chain blocks the event loop wherever its receiver is an established
/// instance (constructed, annotated, stored on `self`, through a local alias or a fluent
/// builder); the asynchronous client's never does; a union, a join of both clients, an untyped
/// value, an unknown factory, and a class object type nothing.
#[test]
fn catalogued_attributes_type_reads_on_established_instances() {
    assert_eq!(
        check("attribute-types-chains", &[("svc.py", SERVICE)]),
        [
            d("svc.py", 11, "EFF101", "module_client"),
            d("svc.py", 15, "EFF101", "built"),
            d("svc.py", 20, "EFF101", "alias"),
            d("svc.py", 24, "EFF101", "annotated"),
            d("svc.py", 32, "EFF101", "Service.handle"),
        ]
    );
}

const HANDLER: &str = r#"from acme import Client

client = Client()


async def handler() -> None:
    client.chat.completions.create(model="m")
"#;

/// A project subclass of the library class takes the row along its MRO, past a builtin type
/// without one (`object`, which a project base deriving from it by name puts before the library
/// class), and a value annotated with it is typed too; a project class the value may be that
/// binds the name (in its body, on `self`, or on the class object), defines `__getattribute__`,
/// has a metaclass heki does not know, or puts a library class without the row or a base heki
/// knows nothing of first leaves every read of the attribute on the library class untyped, and
/// a base heki knows nothing of leaves the subclass's own reads untyped too.
#[test]
fn project_subclasses_keep_or_withdraw_the_row() {
    let plain = r#"from acme import Client


class Mine(Client):
    pass


async def read(m: Mine) -> None:
    m.chat.completions.create(model="m")
    super(Mine, m).chat.completions.create(model="m")


class Logged(object):
    pass


class Mixed(Logged, Client):
    pass


async def mixed(m: Mixed) -> None:
    m.chat.completions.create(model="m")
"#;
    assert_eq!(
        check(
            "attribute-types-plain-subclass",
            &[("svc.py", HANDLER), ("sub.py", plain)]
        ),
        [
            d("sub.py", 9, "EFF101", "read"),
            d("sub.py", 22, "EFF101", "mixed"),
            d("svc.py", 7, "EFF101", "handler"),
        ]
    );
    let withdrawing = [
        (
            "class-attribute",
            "from acme import Client\n\n\nclass Mine(Client):\n    chat = None\n",
        ),
        (
            "self-store",
            "from acme import Client\n\n\nclass Mine(Client):\n    def __init__(self) -> None:\n        super().__init__()\n        self.chat = object()\n",
        ),
        (
            "rebound",
            "from acme import Client\n\n\nclass Mine(Client):\n    pass\n\n\nMine.chat = None\n",
        ),
        (
            "getattribute",
            "from acme import Client\n\n\nclass Mine(Client):\n    def __getattribute__(self, name):\n        return None\n",
        ),
        (
            "metaclass",
            "from acme import Client\n\n\nclass Meta(type):\n    pass\n\n\nclass Mine(Client, metaclass=Meta):\n    pass\n",
        ),
        (
            "library-base",
            "from acme import Client\nfrom other import Base\n\n\nclass Mine(Base, Client):\n    pass\n",
        ),
        (
            "expression-base",
            "from acme import Client\nfrom mixins import mixin\n\n\nclass Mine(mixin(), Client):\n    pass\n\n\nasync def read(m: Mine) -> None:\n    m.chat.completions.create(model=\"m\")\n",
        ),
        (
            "local-base",
            "from acme import Client\n\n\ndef make(base):\n    class Mine(base, Client):\n        pass\n\n    return Mine\n",
        ),
    ];
    for (shape, sub) in withdrawing {
        assert_eq!(
            check(
                &format!("attribute-types-{shape}"),
                &[("svc.py", HANDLER), ("sub.py", sub)]
            ),
            [],
            "{shape}"
        );
    }
    // A project subclass declaring the attribute's type types its own reads, not the library
    // class's.
    let typed = r#"from acme import AsyncChat, Client


class Mine(Client):
    chat: AsyncChat


async def read(m: Mine) -> None:
    m.chat.completions.create(model="m")
"#;
    assert_eq!(
        check(
            "attribute-types-typed-subclass",
            &[("svc.py", HANDLER), ("sub.py", typed)]
        ),
        []
    );
}

/// A row names an attribute of a library class: one naming a project class types nothing, and
/// the project's own declarations type its reads.
#[test]
fn rows_never_type_project_classes() {
    let p = Project::new("attribute-types-project-class");
    p.write(
        "pyproject.toml",
        &format!(
            "{CONFIG}\n[[tool.heki.ecosystem]]\nsymbol = \"svc.Local.chat\"\nattribute-type = \"acme.Chat\"\n"
        ),
    );
    p.write(
        "svc.py",
        r#"class Local:
    pass


async def handler(local: Local) -> None:
    local.chat.completions.create(model="m")
"#,
    );
    let (code, stdout, stderr) = p.check(&["--select", "EFF101"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
}

/// `attribute-type` describes a read: an override that also sets a key of a call is refused.
#[test]
fn attribute_type_takes_no_call_key() {
    let p = Project::new("attribute-types-mixed-keys");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.ecosystem]]\nsymbol = \"acme.Client.chat\"\nattribute-type = \"acme.Chat\"\neffects = [\"blocking_net\"]\n",
    );
    p.write("svc.py", "x = 1\n");
    let (code, stdout, stderr) = p.check(&[]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(
        stderr.contains("`attribute-type` in the user ecosystem override of `acme.Client.chat`"),
        "{stderr}"
    );
}
