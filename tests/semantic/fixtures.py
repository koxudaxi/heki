"""Semantic regression fixtures derived from Stage 0-A (18.1M LOC) and `dcg` (196k LOC).

Each fixture documents expected `heki` diagnostics (TP) and mandatory non-diagnostics (TN)
for Milestones M4-M6.
"""
from __future__ import annotations

import asyncio
from functools import cached_property, lru_cache
import subprocess
import time


# ==============================================================================
# MUT101: @cached_property dependent attribute mutated without invalidation
# ==============================================================================
class StaleCacheTP:
    def __init__(self, raw: dict[str, int]) -> None:
        self.raw_obj = raw

    @cached_property
    def keys_snapshot(self) -> tuple[str, ...]:
        return tuple(self.raw_obj.keys())

    def replace_raw(self, new_raw: dict[str, int]) -> None:
        # EXPECT: MUT101 (no `del self.keys_snapshot` or `self.__dict__.pop(...)`)
        self.raw_obj = new_raw


class DisjointKeysAndTwoPhaseInitTN:
    def __init__(self) -> None:
        self.params: dict[str, str] = {}
        self._cache: dict[str, int] = {}

    def _configure_defaults(self) -> None:
        # MUST NOT report MUT101: 2-phase init helper
        self.params["outtmpl"] = "default"

    @cached_property
    def output_template(self) -> str:
        return self.params["outtmpl"]

    def set_proxy(self, url: str) -> None:
        # MUST NOT report MUT101: disjoint constant dict key (`proxy` != `outtmpl`)
        self.params["proxy"] = url

    def memoize(self, k: str, v: int) -> None:
        # MUST NOT report MUT101: memo dict write
        self._cache[k] = v


# ==============================================================================
# MUT201: Declarative Write Ownership (generalized from dcg GenerationStore)
# Targets: Model.fields | Allowed writers: Store.*
# ==============================================================================
class Model:
    def __init__(self) -> None:
        self.fields: list[str] = []


class Store:
    def add_field(self, model: Model, name: str) -> None:
        # MUST NOT report MUT201: inside allowed_writers
        model.fields.append(name)


def _sneaky_helper(items: list[str], name: str) -> None:
    items.append(name)


def parser_bad_direct_alias_and_indirect(model: Model) -> None:
    # EXPECT: MUT201 (direct container mutation outside Store)
    model.fields.append("a")
    # EXPECT: MUT201 (local alias mutation)
    alias = model.fields
    alias.append("b")
    # EXPECT: MUT201 (interprocedural helper mutation)
    _sneaky_helper(model.fields, "c")


# ==============================================================================
# MUT114: @lru_cache result shared mutation
# ==============================================================================
@lru_cache(maxsize=32)
def get_shared_schema(name: str) -> dict[str, list[str]]:
    return {"fields": [name]}


def caller_corrupts_shared_cache() -> None:
    schema = get_shared_schema("user")
    # EXPECT: MUT114 (mutates shared cached return value)
    schema["fields"].append("corrupted")


# ==============================================================================
# EFF101: async blocking + OffloadsBlocking wrapper propagation + Popen TN
# ==============================================================================
async def in_thread(fn, *args):
    return await asyncio.to_thread(fn, *args)


def sync_helper_blocks() -> None:
    time.sleep(0.1)


async def handler_indirect_block_tp() -> None:
    # EXPECT: EFF101 (2-hop blocking reach to time.sleep)
    sync_helper_blocks()


async def handler_offloaded_via_wrapper_tn() -> None:
    # MUST NOT report EFF101: `in_thread` propagates `offloads_blocking_params`
    await in_thread(sync_helper_blocks)


async def handler_popen_spawn_tn() -> subprocess.Popen:
    # MUST NOT report EFF101: Popen spawns without blocking (only .wait()/.communicate() block)
    return subprocess.Popen(["true"])


# ==============================================================================
# EFF102: a client allocated in the loop header runs once; in the body, per item
# ==============================================================================
def _connect_and_list(names: list[str]) -> list[str]:
    import sqlite3

    sqlite3.connect(":memory:").close()
    return names


def connect_once_then_iterate_tn(names: list[str]) -> list[str]:
    out: list[str] = []
    # MUST NOT report EFF102: `for.iter` runs once before the loop body
    for item in _connect_and_list(names):
        out.append(str(item))
    return out


def connect_per_item_tp(names: list[str]) -> list[str]:
    out: list[str] = []
    for item in names:
        # EXPECT: EFF102 (the helper allocates a client on every iteration)
        out.extend(_connect_and_list([item]))
    return out
