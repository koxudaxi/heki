#!/usr/bin/env python3
"""Layer 2 sentinel: verify the catalog rows of each `[[distributions]]` entry of
`kb/ecosystem/*.toml` against releases of the distribution.

For each distribution, the releases checked are the oldest and the newest its `verified` range
admits and the first and the last of each major release in between (final releases only, not
yanked). Each is installed in a throwaway virtual environment of its own, with its dependencies
as they were a day after its upload (`uv pip install --exclude-newer`), and checked in a child
interpreter there (docs/ANALYSIS_AND_KB_SPEC.md section 4.2):
  1. Every `[[attributes]]` row under the distribution's packages: reading the attribute on an
     instance of the row's class gives an instance of exactly the row's `type`. The instances
     come from the constructors below (`CONSTRUCTORS`, which do no I/O) and from the rows
     themselves, followed from them; a row no instance reaches fails.
  2. Every `[[entries]]` row resolves. A method listed `async_safe` is a coroutine function,
     unless it is a builder (3); one with a blocking effect is not, nor an async generator
     function.
  3. A method with `return_type = "Self"` returns an instance of the receiver's class, also on
     an instance of a subclass. A receiver the constructors cannot build comes from a chain of
     calls on a constructed root (`CHAINS`), and a subclass instance is that receiver re-classed.
The constructors and the builders run with every network connection refused: they do no network
I/O.

Usage:
  python3 tests/sentinel/verify_ecosystem_kb.py [--only DIST] [--version DIST==VERSION ...]
(exit status 0 = no failure; needs `uv` and access to PyPI.)
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
import tomllib
import urllib.request
from datetime import datetime, timedelta
from pathlib import Path

KB_DIR = Path(__file__).resolve().parents[2] / "kb" / "ecosystem"
# The interpreter of the environments: every release checked has wheels for it.
PYTHON = "3.12"
BLOCKING = {
    "blocking_net",
    "blocking_db",
    "blocking_subprocess",
    "blocking_sync",
    "sleep",
    "blocking_fs",
}

# How the sentinel builds an instance of a client class without I/O: keyword arguments of its
# constructor (credentials that are never sent, an endpoint that is never contacted).
CONSTRUCTORS: dict[str, dict[str, str]] = {
    "openai.OpenAI": {"api_key": "sk-sentinel"},
    "openai.AsyncOpenAI": {"api_key": "sk-sentinel"},
    "openai.AzureOpenAI": {
        "api_key": "sentinel",
        "api_version": "2024-10-21",
        "azure_endpoint": "https://sentinel.invalid",
    },
    "openai.AsyncAzureOpenAI": {
        "api_key": "sentinel",
        "api_version": "2024-10-21",
        "azure_endpoint": "https://sentinel.invalid",
    },
    "anthropic.Anthropic": {"api_key": "sentinel"},
    "anthropic.AsyncAnthropic": {"api_key": "sentinel"},
    "google.genai.Client": {"api_key": "sentinel"},
    "postgrest.SyncPostgrestClient": {"base_url": "https://sentinel.invalid"},
}

# A builder class that keyword arguments cannot build comes from a short chain of calls on a
# constructed root: `[root, steps]`, where a step is `[method, arguments]` called on the object so
# far, or `["wrap", class]`, which builds `class(obj.request)`. The calls only assemble a request.
CHAINS: dict[str, list] = {
    "postgrest._sync.request_builder.SyncSelectRequestBuilder": [
        "postgrest.SyncPostgrestClient",
        [["from_", ["sentinel"]], ["select", ["*"]]],
    ],
    "postgrest._sync.request_builder.SyncFilterRequestBuilder": [
        "postgrest.SyncPostgrestClient",
        [
            ["from_", ["sentinel"]],
            ["select", ["*"]],
            ["wrap", "postgrest._sync.request_builder.SyncFilterRequestBuilder"],
        ],
    ],
    "postgrest._sync.request_builder.SyncRPCFilterRequestBuilder": [
        "postgrest.SyncPostgrestClient",
        [["rpc", ["sentinel", {}]]],
    ],
}

# The arguments of a builder method, by parameter name: every parameter without a default takes
# the value here.
ARGUMENTS: dict[str, object] = {
    "column": "c",
    "operator": "eq",
    "criteria": "1",
    "value": "1",
    "values": ["1"],
    "pattern": "p",
    "filters": "c.eq.1",
    "query": {"c": "1"},
    "range": ["1", "2"],
    "size": 1,
    "start": 1,
    "end": 2,
}

# Runs inside the environment of one release: reads the rows as JSON on stdin, prints a JSON list
# of failures.
CHILD = r"""
import importlib, inspect, json, socket, sys

rows = json.load(sys.stdin)
failures = []


# The constructors and the builders run below do no network I/O: any attempt raises.
def no_network(*args, **kwargs):
    raise OSError("the sentinel allows no network I/O")


socket.socket.connect = no_network
socket.socket.connect_ex = no_network
socket.getaddrinfo = no_network
socket.create_connection = no_network


def resolve(path):
    parts = path.split(".")
    for i in range(len(parts), 0, -1):
        try:
            obj = importlib.import_module(".".join(parts[:i]))
        except ImportError:
            continue
        for name in parts[i:]:
            obj = getattr(obj, name)
        return obj
    raise ImportError(path)


instances = {}
for cls, kwargs in rows["constructors"].items():
    try:
        instances[cls] = resolve(cls)(**kwargs)
    except Exception as e:
        failures.append(f"{cls}: cannot construct: {e!r}")

def build(owner, cls):
    # An instance of `cls`, a class of `owner`, built by its constructor or by its chain.
    if owner in rows["constructors"]:
        return cls(**rows["constructors"][owner])
    root, steps = rows["chains"][owner]
    obj = resolve(root)(**rows["constructors"][root])
    for step in steps:
        if step[0] == "wrap":
            obj = resolve(step[1])(obj.request)
        else:
            obj = getattr(obj, step[0])(*step[1])
    if type(obj) is not resolve(owner):
        raise TypeError(f"the chain of {owner} gives {type(obj).__qualname__}")
    obj.__class__ = cls
    return obj


for owner in rows["chains"]:
    try:
        instances[owner] = build(owner, resolve(owner))
    except Exception as e:
        failures.append(f"{owner}: cannot build from its chain: {e!r}")

pending = dict(rows["attributes"])
while True:
    progressed = False
    for symbol, type_path in list(pending.items()):
        owner, _, attr = symbol.rpartition(".")
        if owner not in instances:
            continue
        del pending[symbol]
        progressed = True
        try:
            value = getattr(instances[owner], attr)
            expected = resolve(type_path)
        except Exception as e:
            failures.append(f"{symbol}: {e!r}")
            continue
        if type(value) is not expected:
            failures.append(f"{symbol}: {type(value).__module__}.{type(value).__qualname__}, not {type_path}")
            continue
        instances.setdefault(type_path, value)
    if not progressed:
        break
for symbol in pending:
    failures.append(f"{symbol}: no instance of its class is reached")

for symbol, entry in rows["entries"].items():
    try:
        obj = resolve(symbol)
    except Exception as e:
        failures.append(f"{symbol}: does not resolve: {e!r}")
        continue
    if inspect.isclass(obj):
        continue
    # A decorator validating the arguments (`openai._utils.required_args`) wraps the definition.
    body = inspect.unwrap(obj)
    is_async = inspect.iscoroutinefunction(body) or inspect.isasyncgenfunction(body)
    builder = entry.get("return_type") == "Self"
    if entry.get("async_safe") and not builder and not inspect.iscoroutinefunction(body):
        failures.append(f"{symbol}: async_safe, but no coroutine function")
    if set(entry.get("effects", [])) & set(rows["blocking"]) and is_async:
        failures.append(f"{symbol}: blocking, but a coroutine or async generator function")
    if builder:
        owner, _, method = symbol.rpartition(".")
        if owner not in rows["constructors"] and owner not in rows["chains"]:
            failures.append(f"{symbol}: no constructor or chain for {owner}")
            continue
        base = resolve(owner)
        sub = type("SentinelSubclass", (base,), {})
        for cls in (base, sub):
            try:
                receiver = build(owner, cls)
                if isinstance(inspect.getattr_static(base, method), property):
                    result = getattr(receiver, method)
                else:
                    function = getattr(receiver, method)
                    args = []
                    for name, parameter in inspect.signature(function).parameters.items():
                        if parameter.default is not parameter.empty or parameter.kind in (
                            parameter.VAR_POSITIONAL,
                            parameter.VAR_KEYWORD,
                        ):
                            continue
                        args.append(rows["arguments"][name])
                    result = function(*args)
            except Exception as e:
                failures.append(f"{symbol}: on {cls.__qualname__}: {e!r}")
                continue
            if type(result) is not cls:
                failures.append(f"{symbol}: on {cls.__qualname__} returns {type(result).__qualname__}")

print(json.dumps(failures))
"""


def load_catalog() -> tuple[list[dict], dict[str, dict], dict[str, str]]:
    distributions: list[dict] = []
    entries: dict[str, dict] = {}
    attributes: dict[str, str] = {}
    for path in sorted(KB_DIR.glob("*.toml")):
        data = tomllib.loads(path.read_text())
        distributions += data.get("distributions", [])
        entries.update({e["symbol"]: e for e in data.get("entries", [])})
        attributes.update({a["symbol"]: a["type"] for a in data.get("attributes", [])})
    return distributions, entries, attributes


def under(symbol: str, packages: list[str]) -> bool:
    return any(symbol == p or symbol.startswith(p + ".") for p in packages)


def releases(name: str) -> dict[str, datetime]:
    """Final, not yanked releases of `name` on PyPI, with their first upload time."""
    request = urllib.request.Request(
        f"https://pypi.org/pypi/{name}/json", headers={"User-Agent": "heki-sentinel"}
    )
    with urllib.request.urlopen(request) as response:
        data = json.load(response)
    out = {}
    for version, files in data["releases"].items():
        if files and not any(f.get("yanked") for f in files):
            out[version] = min(datetime.fromisoformat(f["upload_time"]) for f in files)
    return out


def checked_versions(name: str, verified: str) -> dict[str, datetime]:
    """The oldest and newest releases `verified` admits, and the first and last of each major."""
    from packaging.specifiers import SpecifierSet
    from packaging.version import Version

    spec = SpecifierSet(verified)
    admitted = sorted(
        ((Version(v), t) for v, t in releases(name).items() if not Version(v).is_prerelease),
        key=lambda vt: vt[0],
    )
    admitted = [(v, t) for v, t in admitted if v in spec]
    by_major: dict[int, list] = {}
    for v, t in admitted:
        by_major.setdefault(v.major, []).append((v, t))
    chosen = {}
    for group in by_major.values():
        for v, t in (group[0], group[-1]):
            chosen[str(v)] = t
    return chosen


def check_release(name: str, version: str, uploaded: datetime, rows: dict) -> list[str]:
    with tempfile.TemporaryDirectory(prefix="heki-sentinel-") as tmp:
        venv = Path(tmp) / "venv"
        subprocess.run(["uv", "venv", "-q", "--python", PYTHON, str(venv)], check=True)
        python = venv / ("Scripts/python.exe" if sys.platform == "win32" else "bin/python")
        cutoff = (uploaded + timedelta(days=1)).strftime("%Y-%m-%dT%H:%M:%SZ")
        subprocess.run(
            [
                "uv",
                "pip",
                "install",
                "-q",
                "--python",
                str(python),
                "--exclude-newer",
                cutoff,
                f"{name}=={version}",
            ],
            check=True,
        )
        # `-I`: nothing from the working directory is imported.
        child = subprocess.run(
            [str(python), "-I", "-c", CHILD],
            input=json.dumps(rows),
            capture_output=True,
            text=True,
            check=False,
        )
        if child.returncode != 0:
            return [f"the check failed: {child.stderr.strip()}"]
        return json.loads(child.stdout)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--only", help="check this distribution only")
    parser.add_argument(
        "--version", action="append", default=[], help="DIST==VERSION: check this release instead"
    )
    args = parser.parse_args()
    pinned: dict[str, list[str]] = {}
    for spec in args.version:
        dist, _, version = spec.partition("==")
        pinned.setdefault(dist, []).append(version)
    distributions, entries, attributes = load_catalog()
    failed = 0
    for dist in distributions:
        name = dist["name"]
        if args.only and name != args.only:
            continue
        packages = dist["packages"]
        rows = {
            "constructors": {c: k for c, k in CONSTRUCTORS.items() if under(c, packages)},
            "chains": {c: k for c, k in CHAINS.items() if under(c, packages)},
            "arguments": ARGUMENTS,
            "attributes": {s: t for s, t in attributes.items() if under(s, packages)},
            "entries": {s: e for s, e in entries.items() if under(s, packages)},
            "blocking": sorted(BLOCKING),
        }
        if name in pinned:
            known = releases(name)
            versions = {v: known[v] for v in pinned[name]}
        else:
            versions = checked_versions(name, dist["verified"])
        if not versions:
            # A range that selects no published release would leave every row of the
            # distribution unchecked while the sentinel passes.
            print(f"{name}: `verified = {dist['verified']!r}` selects no published release")
            failed += 1
            continue
        for version, uploaded in versions.items():
            failures = check_release(name, version, uploaded, rows)
            status = "ok" if not failures else f"{len(failures)} failure(s)"
            print(
                f"{name} {version}: {len(rows['attributes'])} attribute rows, "
                f"{len(rows['entries'])} entries: {status}"
            )
            for f in failures:
                print(f"  {f}")
            failed += len(failures)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
