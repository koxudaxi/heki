"""What a generated program runs, recorded with `sys.monitoring`.

The recorder watches the places a plan names (built by `plan.py` from heki's reports):

- a call site: each execution of the call is an instance, from the call instruction until control
  is back in the calling frame, either at the next instruction (it returned) or through an
  exception;
- a loop: each iteration starts after the loop's `FOR_ITER` has produced an item, and completes
  when control reaches that `FOR_ITER` again; an iteration left by `break`, `return`, or an
  exception does not complete;
- a line: each execution of the line in a frame, with snapshots of the objects it may change,
  and the in-place changes the builtin container methods called on it made;
- a function's returns: the identity of each value it returned.

The fakes add their calls as events. Every event gets a sequence number, so a checker decides
whether an event happened inside a call instance or an iteration by comparing numbers. Python
function frames get an activation number when they start (`PY_START`), which a generator or a
coroutine keeps across its suspensions.

Events (lists, so that the trace is compact JSON):

- `["start", seq, act, qualname, path, caller_act, caller_line]`: a project function started;
- `["call", site, act, seq_start, seq_end, outcome, captures]`: a call instance ended
  (`outcome` is `ok` or `exc`);
- `["iter", loop, act, entry, seq_start, seq_end, completed, start_caps, body_caps, end_caps,
  lines, seq_fetch]`: `seq_fetch` is the `FOR_ITER` that produced the item, so the window from
  it to `seq_end` also holds the code that advanced the iterator;
- `["resume", seq, act, qualname, path]`: a project generator or coroutine resumed;
- `["exhaust", loop, act, entry, seq]`: the loop's iterator was exhausted;
- `["line", line, act, seq_start, seq_end, outcome, chain, before, after, muts, types,
  names_before, names_after, evals_before, evals_after, attrs_before, attrs_after]`: `names_*`
  map the captured names to the identity of what they hold when the line starts and ends,
  `evals_*` the plan's expressions (`a.k`, `a["k"]`) to `[id, type, snapshot]` of what they load
  without running code of the program (`None` when that cannot be told), and `attrs_*` map
  `name.attr` to `[object id, classes along its MRO as module.qualname, id of what its __dict__
  holds under attr or None, that value's type name or None]` for the pairs the plan names;
- `["ret", ret, act, seq, value_id, type_name, base, self_id]`: `base` is `list`, `dict`, or
  `set` for an instance of one (a subclass too), else the type's name; `self_id` is the identity
  of the frame's first argument (`None` without one), the instance a getter computed for;
- `["load", act, seq, object_id, attr]`: an attribute load `<first argument>.<attr>` of a frame ran,
  for the attributes a kept-value claim names (`MUT101`);
- `["kept_ret", ret, act, seq, self_id, kept_id]`: the writer of a kept-value claim returned
  normally; `kept_id` is the identity of what its first argument's `__dict__` holds under the
  property's name then (`None`: nothing). A line of such a writer carries the same facts at its
  start and end as a last element of its `line` event (`kept_facts`, after `attrs_after`);
- `["lost", line, act, seq_start, seq_end, outcome, facts]`: for a line plan's change of an
  object the code never names (`MUT119`), when the line ran without an exception: `made` (what
  made the object at its expression: `["call", qualname, builtin maker]`, `["slice", qualname
  of the sliced value's class, builtin slice]`, or `["return", qualname, False]`), `method` (at
  a method's call: `[name, a changing method of the receiver's builtin type, qualname of the
  program's method or None]`), `store` (the qualname of a program's `__setitem__` or
  `__delitem__` the change ran), and, when the line saw the object, `held` (how many references
  the program keeps to it when the line ends), `holders`, `named` (the names of the line's frame
  the line bound to it), and `type`; `results` maps each assignment target to the type's name of
  what it holds then (`None` when loading it would run code);
- `["fake", seq, api, kind, waits, detail, stack, input_ids, output_id]`.
"""

from __future__ import annotations

import bisect
import collections
import dis
import functools
import os
import re
import sys
import types

MON = sys.monitoring
TOOL_ID = 4
EV = MON.events
DISABLE = MON.DISABLE
MISSING = object()
# The program's frame, seen from a callback: the callback, its guard, then the program.
_PROGRAM_FRAME = 2

_MUTATORS = {
    list: frozenset(
        "append extend insert remove pop clear sort reverse __setitem__ __delitem__ __iadd__ __imul__".split()
    ),
    dict: frozenset("update pop popitem clear setdefault __setitem__ __delitem__ __ior__".split()),
    set: frozenset(
        "add discard remove pop clear update intersection_update difference_update "
        "symmetric_difference_update __ior__ __iand__ __isub__ __ixor__".split()
    ),
    bytearray: frozenset("append extend insert remove pop clear reverse __setitem__".split()),
    collections.deque: frozenset(
        "append appendleft extend extendleft pop popleft remove rotate clear __setitem__ __delitem__".split()
    ),
}
_HEX = re.compile(r"0x[0-9a-fA-F]+")


def mutating_method(receiver, name):
    """Whether calling method `name` of a builtin container changes the container in place."""
    for kind, names in _MUTATORS.items():
        if isinstance(receiver, kind):
            return name in names
    return False


def snapshot(obj, depth=5):
    """A JSON value standing for what `obj` holds, deep enough to see an in-place change."""
    if obj is None or isinstance(obj, (bool, int, str)):
        return obj
    if isinstance(obj, float):
        return {"float": repr(obj)}
    if depth <= 0:
        return {"deep": type(obj).__name__}
    if isinstance(obj, (bytes, bytearray)):
        return {type(obj).__name__: obj.hex()}
    if isinstance(obj, list):
        return {"list": [snapshot(v, depth - 1) for v in obj]}
    if isinstance(obj, tuple):
        return {"tuple": [snapshot(v, depth - 1) for v in obj]}
    if isinstance(obj, collections.deque):
        return {"deque": [snapshot(v, depth - 1) for v in obj]}
    if isinstance(obj, dict):
        return {"dict": [[snapshot(k, depth - 1), snapshot(v, depth - 1)] for k, v in obj.items()]}
    if isinstance(obj, (set, frozenset)):
        return {type(obj).__name__: [snapshot(v, depth - 1) for v in obj]}
    data = getattr(type(obj), "__slots__", None)
    if type(obj).__name__ in ("FakeArray", "FakeFrame"):
        field = "data" if type(obj).__name__ == "FakeArray" else "rows"
        return {type(obj).__name__: [snapshot(v, depth - 1) for v in getattr(obj, field)]}
    if isinstance(obj, BaseException):
        return {"exception": type(obj).__name__, "msg": _HEX.sub("0x?", str(obj))}
    if isinstance(obj, (types.FunctionType, types.BuiltinFunctionType, type)):
        return {"callable": getattr(obj, "__qualname__", repr(obj))}
    attrs = getattr(obj, "__dict__", None)
    if isinstance(attrs, dict) and data is None:
        return {
            "object": type(obj).__qualname__,
            "attrs": [[k, snapshot(v, depth - 1)] for k, v in attrs.items()],
        }
    return {"opaque": type(obj).__qualname__}


class CodeInfo:
    """The instructions of one code object and the plan entries that live in it."""

    __slots__ = (
        "code",
        "path",
        "line_of",
        "next_off",
        "pos_of",
        "sites",
        "for_iters",
        "get_iters",
        "lines",
        "returns",
        "watched",
        "body_offs",
        "load_offs",
        "kept_rets",
        "offsets",
        "_instructions",
    )

    def __init__(self, code, path):
        self.code = code
        self.path = path
        self.line_of = {}
        self.next_off = {}
        self.pos_of = {}
        self.sites = {}
        self.for_iters = {}
        self.get_iters = {}
        self.lines = {}
        self.returns = []
        self.body_offs = {}
        self.load_offs = {}
        self.kept_rets = []
        instructions = list(dis.get_instructions(code))
        for i, ins in enumerate(instructions):
            pos = ins.positions
            self.line_of[ins.offset] = pos.lineno if pos is not None else None
            if pos is not None and pos.lineno is not None:
                self.pos_of[ins.offset] = (pos.lineno, pos.col_offset, pos.end_lineno, pos.end_col_offset)
            if i + 1 < len(instructions):
                self.next_off[ins.offset] = instructions[i + 1].offset
        self.offsets = [ins.offset for ins in instructions]
        self.watched = False
        self._instructions = instructions

    def position(self, lasti):
        """The position of the instruction a frame runs: on CPython 3.12 a frame that called out
        reports the offset of one of its instruction's inline cache entries."""
        i = bisect.bisect_right(self.offsets, lasti) - 1
        return self.pos_of.get(self.offsets[i]) if i >= 0 else None

    def attach(self, plan_sites, plan_loops, plan_lines, plan_returns, kept_attrs=frozenset()):
        """Index the plan entries in this code object; returns whether any is here."""
        instructions = self._instructions
        if kept_attrs and self.code.co_argcount:
            self.load_offs = first_argument_loads(instructions, self.code.co_varnames[0], kept_attrs)
        by_pos = collections.defaultdict(list)
        for ins in instructions:
            pos = self.pos_of.get(ins.offset)
            if pos is not None:
                by_pos[pos].append(ins)
        for site in plan_sites.get(self.path, ()):
            candidates = by_pos.get(tuple(site["pos"]), [])
            chosen = _choose_site_instruction(candidates)
            if chosen is not None:
                self.sites.setdefault(chosen.offset, []).append(site)
        for loop in plan_loops.get(self.path, ()):
            found = match_loop(instructions, self.pos_of, loop)
            if found is not None:
                get_iter, for_iter, body = found
                self.for_iters.setdefault(for_iter, []).append(loop)
                if get_iter is not None:
                    self.get_iters.setdefault(get_iter, []).append(loop)
                if body is not None:
                    self.body_offs[loop["id"]] = body
        lines_here = {self.line_of[ins.offset] for ins in instructions}
        for line in plan_lines.get(self.path, ()):
            if line["line"] in lines_here:
                self.lines.setdefault(line["line"], []).append(line)
        for ret in plan_returns.get(self.path, ()):
            if ret["qualname"] == self.code.co_qualname:
                if "kept" in ret:
                    self.kept_rets.append(ret)
                else:
                    self.returns.append(ret)
        self.watched = bool(self.sites or self.for_iters or self.lines or self.load_offs)
        return self.watched


# The instructions that load a local onto the stack, the last name pushed last.
_LOAD_LOCAL = frozenset("LOAD_FAST LOAD_FAST_BORROW LOAD_FAST_CHECK".split())
_LOAD_TWO_LOCALS = frozenset("LOAD_FAST_LOAD_FAST LOAD_FAST_BORROW_LOAD_FAST_BORROW".split())


def first_argument_loads(instructions, first, attrs):
    """The `LOAD_ATTR` instructions of a code object whose receiver is its first argument `first`
    (the local it pushed just before), for the attributes `attrs`: `{offset: attr}`."""
    out = {}
    for prev, ins in zip(instructions, instructions[1:]):
        if ins.opname != "LOAD_ATTR" or ins.argval not in attrs:
            continue
        pushed = None
        if prev.opname in _LOAD_LOCAL:
            pushed = prev.argval
        elif prev.opname in _LOAD_TWO_LOCALS and isinstance(prev.argval, tuple):
            pushed = prev.argval[-1]
        if pushed == first:
            out[ins.offset] = ins.argval
    return out


def instance_dict(obj):
    """`obj.__dict__` read without running code of the program (`None` without one)."""
    try:
        attrs = object.__getattribute__(obj, "__dict__")
    except (AttributeError, TypeError):
        return None
    return attrs if isinstance(attrs, dict) else None


def kept_class_facts(obj, prop, attr, getter):
    """What the class of `obj` makes of a kept-value claim: `has`, a class along its MRO defines
    `prop`; `cached`, that first definition is a `functools.cached_property` of the getter
    `getter` (a qualname);
    `plain`, storing or deleting `attr` on it changes its own `__dict__` (no `__setattr__`,
    `__delattr__`, or data descriptor of the name)."""
    klass = type(obj)
    cached = False
    has = False
    for k in klass.__mro__:
        if prop in k.__dict__:
            has = True
            desc = k.__dict__[prop]
            func = getattr(desc, "func", None)
            cached = isinstance(desc, functools.cached_property) and getattr(func, "__qualname__", None) == getter
            break
    plain = klass.__setattr__ is object.__setattr__ and klass.__delattr__ is object.__delattr__
    for k in klass.__mro__:
        if attr in k.__dict__:
            kind = type(k.__dict__[attr])
            plain = plain and not (hasattr(kind, "__set__") or hasattr(kind, "__delete__"))
            break
    return {
        "class": klass.__qualname__,
        "mro": [k.__qualname__ for k in klass.__mro__],
        "has": has,
        "cached": cached,
        "plain": plain,
    }


_TARGET_STORES = frozenset(
    "STORE_FAST STORE_FAST_LOAD_FAST STORE_FAST_STORE_FAST STORE_NAME STORE_DEREF STORE_GLOBAL "
    "UNPACK_SEQUENCE UNPACK_EX".split()
)


def match_loop(instructions, pos_of, loop):
    """The `GET_ITER`, `FOR_ITER`, and first body instruction of the plan's loop in a code object:
    `(get_iter, for_iter, body)` offsets, or `None` when the code object has no such loop.

    CPython gives the loop's instructions the position of its iterable (3.12.4 and later, 3.13,
    3.14, 3.15) or of the whole `for` statement or comprehension (earlier 3.12 releases): each is
    tried in turn. The body starts at the first instruction after the stores that bind the loop's
    target, so that a capture there reads this iteration's item, also when the target and the
    body share one line (a comprehension).
    """
    for pos in [loop["pos"], *loop.get("alt_pos", ())]:
        fors = [
            i
            for i, ins in enumerate(instructions)
            if ins.opname.startswith("FOR_ITER") and pos_of.get(ins.offset) == tuple(pos)
        ]
        if fors:
            break
    else:
        return None
    index = fors[0]
    get_iter = None
    for ins in reversed(instructions[:index]):
        if ins.opname in ("GET_ITER", "GET_AITER"):
            get_iter = ins.offset
            break
        if ins.opname not in ("LOAD_FAST_AND_CLEAR", "SWAP", "NOP", "CACHE", "EXTENDED_ARG"):
            break
    body = None
    for ins in instructions[index + 1 :]:
        if ins.opname not in _TARGET_STORES:
            body = ins.offset
            break
    return get_iter, instructions[index].offset, body


def _choose_site_instruction(candidates):
    calls = [ins for ins in candidates if ins.opname.startswith("CALL") and "INTRINSIC" not in ins.opname]
    if calls:
        return calls[0]
    for opnames in (("LOAD_ATTR",), ("BINARY_SUBSCR", "BINARY_OP")):
        chosen = [ins for ins in candidates if ins.opname in opnames]
        if chosen:
            return chosen[-1]
    return None


class FrameState:
    __slots__ = (
        "frame",
        "act",
        "info",
        "open_call",
        "open_line",
        "cur_line",
        "loops",
        "pending_for",
        "entries",
    )

    def __init__(self, frame, act, info):
        self.frame = frame
        self.act = act
        self.info = info
        self.open_call = []
        self.open_line = None
        self.cur_line = None
        self.loops = {}
        self.pending_for = []
        self.entries = collections.Counter()


class CallInstance:
    __slots__ = ("site", "act", "start", "next_off", "captures", "objects")

    def __init__(self, site, act, start, next_off, captures, objects):
        self.site = site
        self.act = act
        self.start = start
        self.next_off = next_off
        self.captures = captures
        self.objects = objects


class Iteration:
    __slots__ = ("loop", "entry", "start", "fetch", "start_caps", "body_caps", "lines")

    def __init__(self, loop, entry, start, fetch, start_caps):
        self.loop = loop
        self.entry = entry
        self.start = start
        self.fetch = fetch
        self.start_caps = start_caps
        self.body_caps = None
        self.lines = set()


class LostChange:
    """A line plan's change of an object the code never names (`MUT119`): the object the change
    ran on, once the line has seen it, and how the line made it and changed it."""

    __slots__ = ("id", "change", "base", "results", "sliced", "held", "made", "method", "store")

    def __init__(self, plan_id, spec):
        self.id = plan_id
        self.change = tuple(spec["change"])
        self.base = tuple(spec["base"])
        self.results = spec.get("results", ())
        # The value a slice at the object's expression slices (`xs` of `xs[:]`), when loading it
        # runs no code.
        self.sliced = spec.get("sliced")
        # The recorder's one reference to the object besides `Recorder.keep`.
        self.held = []
        self.made = None
        self.method = None
        self.store = None


# What calling one of these makes: a new builtin container that nothing else holds.
_MAKERS = (list, dict, set, bytearray, collections.deque, sorted)


def builtin_maker(callable_, receiver):
    """Whether a call of `callable_` (on `receiver`, for a method) makes a new builtin container
    that nothing else holds: a builtin container type or `sorted` itself, or the builtin `copy`
    of a builtin container."""
    if any(callable_ is maker for maker in _MAKERS):
        return True
    return (
        isinstance(callable_, (types.MethodDescriptorType, types.BuiltinMethodType))
        and callable_.__name__ == "copy"
        and type(receiver) in _MUTATORS
    )


def builtin_slice(value):
    """What slicing `value` makes, when it is new and nothing else holds it: the slice of a list
    or a bytearray whose class keeps the builtin `__getitem__`."""
    for kind in (list, bytearray):
        if isinstance(value, kind):
            return ["slice", type(value).__qualname__, type(value).__getitem__ is kind.__getitem__]
    return None


def _receiver(callable_, arg0):
    """The object a method call runs on: a bound method's `__self__`, else the first argument."""
    receiver = getattr(callable_, "__self__", None)
    if receiver is None or isinstance(receiver, types.ModuleType):
        return arg0
    return receiver


class LineExec:
    __slots__ = (
        "plan",
        "start",
        "chain",
        "watched",
        "before",
        "muts",
        "types",
        "raised",
        "names",
        "evals",
        "evals_before",
        "attrs",
        "attrs_before",
        "kept",
        "kept_self",
        "lost",
        "names_at_start",
    )

    def __init__(self, plan, start):
        self.plan = plan
        self.start = start
        self.lost = [LostChange(entry["id"], entry["lost"]) for entry in plan if "lost" in entry]
        # With a lost change: the identity of what each local holds when the line starts.
        self.names_at_start = {}
        self.names = {}
        self.evals = []
        self.evals_before = {}
        self.attrs = []
        self.attrs_before = {}
        self.chain = None
        self.watched = []
        self.before = []
        self.muts = []
        self.types = {}
        self.raised = False
        self.kept = None
        self.kept_self = MISSING


class Recorder:
    """Collects the events of one program; `begin_run()` starts a new run's event list."""

    def __init__(self, root, plan):
        self.root = os.path.realpath(root) + os.sep
        self.seq = 0
        self.events = []
        self.keep = []
        self.frames = {}
        self.codes = {}
        self.project = {}
        self.next_act = 0
        self.open_calls = []
        self.plan_sites = _by_path(plan.get("sites", ()))
        self.plan_loops = _by_path(plan.get("loops", ()))
        self.plan_lines = _by_path(plan.get("lines", ()))
        self.plan_returns = _by_path(plan.get("returns", ()))
        self.kept_attrs = frozenset(
            entry["kept"]["attr"] for entry in plan.get("lines", ()) if "kept" in entry
        )
        self.active = False
        self.errors = []

    # -- lifecycle ---------------------------------------------------------------------------
    def install(self):
        MON.use_tool_id(TOOL_ID, "heki-oracle")
        register = MON.register_callback
        register(TOOL_ID, EV.PY_START, self._guard(self._on_start))
        register(TOOL_ID, EV.PY_RETURN, self._guard(self._on_return))
        register(TOOL_ID, EV.PY_YIELD, self._guard(self._on_yield))
        register(TOOL_ID, EV.PY_RESUME, self._guard(self._on_resume))
        register(TOOL_ID, EV.PY_UNWIND, self._guard(self._on_unwind))
        register(TOOL_ID, EV.RAISE, self._guard(self._on_raise))
        register(TOOL_ID, EV.INSTRUCTION, self._guard(self._on_instruction))
        register(TOOL_ID, EV.CALL, self._guard(self._on_call))
        MON.set_events(
            TOOL_ID,
            EV.PY_START | EV.PY_RESUME | EV.PY_RETURN | EV.PY_YIELD | EV.PY_UNWIND | EV.RAISE,
        )

    def _guard(self, callback):
        """A failure of the recorder must not become an exception of the program: it is kept,
        and the run that saw it is discarded."""

        def guarded(*args):
            try:
                return callback(*args)
            except Exception as exc:  # noqa: BLE001
                if len(self.errors) < 5:
                    import traceback

                    self.errors.append("".join(traceback.format_exception(exc)))
                return None

        return guarded

    def uninstall(self):
        MON.set_events(TOOL_ID, 0)
        for code, info in self.codes.items():
            if info is not None and info.watched:
                MON.set_local_events(TOOL_ID, code, 0)
        for event in (
            EV.PY_START,
            EV.PY_RESUME,
            EV.PY_RETURN,
            EV.PY_YIELD,
            EV.PY_UNWIND,
            EV.RAISE,
            EV.INSTRUCTION,
            EV.CALL,
        ):
            MON.register_callback(TOOL_ID, event, None)
        MON.free_tool_id(TOOL_ID)

    def begin_run(self):
        """Start a fresh event list; frames of earlier runs are forgotten."""
        self.events = []
        self.frames = {}
        self.open_calls = []
        self.errors = []
        self.active = True

    def end_run(self):
        self.active = False
        events = self.events
        self.events = []
        return events

    # -- helpers ---------------------------------------------------------------------------
    def _is_project(self, code):
        known = self.project.get(code)
        if known is None:
            known = code.co_filename.startswith(self.root)
            self.project[code] = known
        return known

    def _code_info(self, code):
        if code in self.codes:
            return self.codes[code]
        rel = os.path.relpath(code.co_filename, self.root).replace(os.sep, "/")
        info = CodeInfo(code, rel)
        if info.attach(self.plan_sites, self.plan_loops, self.plan_lines, self.plan_returns, self.kept_attrs):
            MON.set_local_events(TOOL_ID, code, EV.INSTRUCTION | EV.CALL)
        self.codes[code] = info
        return info

    def _state(self, frame, code):
        fs = self.frames.get(id(frame))
        if fs is None or fs.frame is not frame:
            info = self._code_info(code)
            fs = FrameState(frame, self.next_act, info)
            self.next_act += 1
            self.frames[id(frame)] = fs
        return fs

    def _tick(self):
        self.seq += 1
        return self.seq

    def _capture(self, frame, names):
        if not names:
            return None
        local = frame.f_locals
        out = {}
        for name in names:
            value = local.get(name, MISSING)
            if value is MISSING:
                out[name] = None
                continue
            self.keep.append(value)
            out[name] = [id(value), type(value).__name__, snapshot(value)]
        return out

    def _attr_facts(self, local, pairs):
        """What each `[name, attr]` of `pairs` names in `local`: the object, its classes, and
        what its `__dict__` holds under the attribute (`snapshot` cannot tell two `object()`s
        apart)."""
        out = {}
        for name, attr in pairs:
            value = local.get(name, MISSING)
            if value is MISSING:
                out[f"{name}.{attr}"] = None
                continue
            held = getattr(value, "__dict__", {}).get(attr, MISSING)
            self.keep.append(value)
            self.keep.append(held)
            classes = [f"{c.__module__}.{c.__qualname__}" for c in type(value).__mro__]
            out[f"{name}.{attr}"] = [
                id(value),
                classes,
                None if held is MISSING else id(held),
                None if held is MISSING else type(held).__name__,
            ]
        return out

    def _eval_keys(self, frame, keys):
        if not keys:
            return None
        local = frame.f_locals
        out = {}
        for key in keys:
            value = static_eval(key, local)
            if value is not UNKNOWN:
                self.keep.append(value)
            # Apart from the captured names: a key may be an item itself (`a == b`).
            out["key:" + key] = UNKNOWN_TEXT if value is UNKNOWN else [snapshot(value), _is_plain_key(value)]
        return out

    def _stack(self):
        stack = []
        frame = sys._getframe(2)
        while frame is not None:
            code = frame.f_code
            if self._is_project(code):
                fs = self.frames.get(id(frame))
                info = self.codes.get(code) or self._code_info(code)
                pos = info.pos_of.get(frame.f_lasti) if info is not None else None
                stack.append(
                    [
                        info.path if info is not None else code.co_filename,
                        code.co_qualname,
                        pos[0] if pos else frame.f_lineno,
                        pos[1] if pos else None,
                        fs.act if fs is not None else -1,
                    ]
                )
            frame = frame.f_back
        return stack

    # -- fakes ------------------------------------------------------------------------------
    def fake(self, api, kind, waits, detail, inputs, output):
        if not self.active:
            return
        seq = self._tick()
        input_ids = []
        for value in inputs:
            self.keep.append(value)
            input_ids.append(id(value))
        output_id = None
        if output is not None:
            self.keep.append(output)
            output_id = id(output)
        self.events.append(["fake", seq, api, kind, waits, detail, self._stack(), input_ids, output_id])

    # -- monitoring callbacks ---------------------------------------------------------------
    def _on_start(self, code, offset):
        if not self._is_project(code):
            return DISABLE
        if not self.active:
            return None
        frame = sys._getframe(_PROGRAM_FRAME)
        info = self._code_info(code)
        seq = self._tick()
        fs = FrameState(frame, self.next_act, info)
        self.next_act += 1
        self.frames[id(frame)] = fs
        caller = frame.f_back
        caller_act, caller_line = -1, None
        if caller is not None and self._is_project(caller.f_code):
            cfs = self.frames.get(id(caller))
            caller_act = cfs.act if cfs is not None and cfs.frame is caller else -1
            caller_line = caller.f_lineno
        self.events.append(["start", seq, fs.act, code.co_qualname, info.path, caller_act, caller_line])
        self._lost_store(frame, code)
        return None

    def _on_resume(self, code, offset):
        if not self._is_project(code):
            return DISABLE
        if not self.active:
            return None
        frame = sys._getframe(_PROGRAM_FRAME)
        fs = self._state(frame, code)
        self.events.append(["resume", self._tick(), fs.act, code.co_qualname, fs.info.path])
        return None

    def _on_yield(self, code, offset, value):
        if not self._is_project(code):
            return DISABLE
        if not self.active:
            return None
        fs = self.frames.get(id(sys._getframe(_PROGRAM_FRAME)))
        if fs is not None and fs.open_line is not None:
            self._end_line(fs, self._tick(), "ok")
            fs.cur_line = None
        return None

    def _on_return(self, code, offset, value):
        if not self._is_project(code):
            return DISABLE
        if not self.active:
            return None
        frame = sys._getframe(_PROGRAM_FRAME)
        self._lost_return(frame, code, value)
        fs = self.frames.pop(id(frame), None)
        seq = self._tick()
        if fs is not None and fs.frame is frame:
            for ret in fs.info.kept_rets:
                obj = frame.f_locals.get(code.co_varnames[0], MISSING) if code.co_argcount else MISSING
                if obj is MISSING:
                    continue
                attrs = instance_dict(obj)
                kept = attrs.get(ret["kept"]["prop"], MISSING) if attrs is not None else MISSING
                self.keep.append(obj)
                self.events.append(
                    ["kept_ret", ret["id"], fs.act, seq, id(obj), None if kept is MISSING else id(kept)]
                )
            if fs.info.returns:
                self.keep.append(value)
                base = next(
                    (k.__name__ for k in (list, dict, set) if isinstance(value, k)), type(value).__name__
                )
                first = frame.f_locals.get(code.co_varnames[0], MISSING) if code.co_argcount else MISSING
                self_id = None if first is MISSING else id(first)
                for ret in fs.info.returns:
                    self.events.append(
                        ["ret", ret["id"], fs.act, seq, id(value), type(value).__name__, base, self_id]
                    )
            self._close_frame(fs, seq, "ok")
        return None

    def _on_unwind(self, code, offset, exc):
        if not self._is_project(code) or not self.active:
            return None
        frame = sys._getframe(_PROGRAM_FRAME)
        fs = self.frames.pop(id(frame), None)
        if fs is not None and fs.frame is frame:
            self._close_frame(fs, self._tick(), "exc")
        return None

    def _on_raise(self, code, offset, exc):
        if not self.active or not self._is_project(code):
            return None
        frame = sys._getframe(_PROGRAM_FRAME)
        fs = self.frames.get(id(frame))
        if fs is None or fs.frame is not frame:
            return None
        seq = self._tick()
        calls, fs.open_call = fs.open_call, []
        for call in calls:
            self._end_call(call, seq, "exc")
        if fs.open_line is not None:
            fs.open_line.raised = True
        return None

    def _on_call(self, code, offset, callable_, arg0):
        if not self.active:
            return None
        frame = sys._getframe(_PROGRAM_FRAME)
        fs = self.frames.get(id(frame))
        if fs is None or fs.open_line is None:
            return None
        if fs.open_line.lost:
            self._lost_call(fs, offset, callable_, arg0)
        if isinstance(callable_, (types.FunctionType, types.MethodType)):
            # A method a program class defines (an override of `append`) is not the builtin one.
            return None
        name = getattr(callable_, "__name__", None)
        receiver = _receiver(callable_, arg0)
        if name is not None and mutating_method(receiver, name):
            self.keep.append(receiver)
            fs.open_line.muts.append([id(receiver), name, type(receiver).__name__])
        return None

    # -- changes of unnamed objects (`MUT119`) ---------------------------------------------------
    def _lost_call(self, fs, offset, callable_, arg0):
        """A call on a line with a lost change: at the change, the method and its receiver; at the
        changed object's expression, what makes the object."""
        pos = fs.info.pos_of.get(offset)
        python = isinstance(callable_, (types.FunctionType, types.MethodType))
        name = getattr(callable_, "__name__", None)
        for watch in fs.open_line.lost:
            if pos == watch.change:
                watch.held[:] = [_receiver(callable_, arg0)]
                builtin = not python and name is not None and mutating_method(watch.held[0], name)
                watch.method = [name, builtin, getattr(callable_, "__qualname__", None) if python else None]
            elif pos == watch.base:
                qualname = getattr(callable_, "__qualname__", type(callable_).__name__)
                watch.made = ["call", qualname, builtin_maker(callable_, _receiver(callable_, arg0))]

    def _lost_watches(self, frame):
        """The caller of `frame` when it runs a line with a lost change, with the position of the
        instruction that is running there."""
        caller = frame.f_back
        fs = self.frames.get(id(caller)) if caller is not None else None
        if fs is None or fs.frame is not caller or fs.open_line is None or not fs.open_line.lost:
            return None, None
        return fs.open_line.lost, fs.info.position(caller.f_lasti)

    def _lost_return(self, frame, code, value):
        """Project code returning to a line with a lost change, from the changed object's
        expression (a function, a property's getter, a `__getitem__`): the value is the object.
        A class's `__init__` returns `None` to the call that makes the instance. A builtin
        constructor that runs project code (`list(Items())` calls `Items.__iter__`) is that call's
        result, not what the callback returns at the same position."""
        watches, pos = self._lost_watches(frame)
        for watch in watches or ():
            made_by_builtin = watch.made is not None and watch.made[0] == "call" and watch.made[2]
            if pos == watch.base and code.co_name != "__init__" and not made_by_builtin:
                watch.held[:] = [value]
                watch.made = ["return", code.co_qualname, False]

    def _lost_store(self, frame, code):
        """Project code an item assignment or deletion with a lost change runs: the object's own
        `__setitem__` or `__delitem__`, which is not its builtin type's."""
        watches, pos = self._lost_watches(frame)
        for watch in watches or ():
            if pos == watch.change and code.co_name in ("__setitem__", "__delitem__"):
                watch.store = code.co_qualname

    def _program_refs(self, ex, watch):
        """How many references to the object `watch` holds the program keeps: its reference count
        less the recorder's own and less the count of a new object held and passed the same way.
        It counts what `gc` cannot see: the locals of running frames."""
        own = sum(1 for v in self.keep if v is watch.held[0])
        own += sum(1 for v in ex.watched if v is watch.held[0])
        own += sum(1 for call in self.open_calls for v in call.objects if v is watch.held[0])
        own += sum(1 for w in ex.lost if w is not watch and w.held and w.held[0] is watch.held[0])
        own += ex.kept_self is watch.held[0]
        probe = [object()]
        return sys.getrefcount(watch.held[0]) - sys.getrefcount(probe[0]) - own

    def _holders(self, ex, watch, frame):
        """What holds the object: the types of the containers `gc` finds, then the names of the
        running frames' locals (read last: on CPython 3.12 reading them stores a copy); and the
        names of the line's frame that the line bound to it."""
        import gc

        recorder = [self.keep, ex.watched, *(w.held for w in ex.lost), *(c.objects for c in self.open_calls)]
        found = sorted(
            {
                type(r).__name__
                for r in gc.get_referrers(watch.held[0])
                if not any(r is mine for mine in recorder) and not isinstance(r, types.FrameType)
            }
        )
        named = []
        line_frame = frame
        while frame is not None:
            if self._is_project(frame.f_code):
                for name, value in frame.f_locals.items():
                    if value is watch.held[0]:
                        found.append(f"{frame.f_code.co_qualname}:{name}")
                        if frame is line_frame and ex.names_at_start.get(name) != id(value):
                            named.append(name)
            frame = frame.f_back
        return found, named

    def _lost_facts(self, fs, ex):
        """What each lost change of the ending line ran on and whether anything holds it now;
        counted before the line's locals are read."""
        out = []
        for watch in ex.lost:
            facts = {"made": watch.made, "method": watch.method, "store": watch.store, "held": None}
            if watch.held:
                held = self._program_refs(ex, watch)
                if held > 0:
                    # An object only a garbage cycle holds is unreachable.
                    import gc

                    gc.collect()
                    held = self._program_refs(ex, watch)
                facts["held"] = held
                facts["holders"], facts["named"] = self._holders(ex, watch, fs.frame) if held > 0 else ([], [])
                kind = type(watch.held[0])
                facts["type"] = [kind.__module__, kind.__qualname__]
            out.append((watch, facts))
        return out

    def _lost_results(self, fs, lost):
        """What each assignment target of a lost change holds when the line ends: `None`, the
        type's name, or `None` in place of the dict when loading it would run code."""
        for watch, facts in lost:
            local = fs.frame.f_locals if watch.results else {}
            loaded = {}
            for expr in watch.results:
                value = static_eval(expr, local)
                loaded[expr] = None if value is UNKNOWN else [value is None, type(value).__name__]
            facts["results"] = loaded

    def _on_instruction(self, code, offset):
        if not self.active:
            return None
        frame = sys._getframe(_PROGRAM_FRAME)
        fs = self._state(frame, code)
        info = fs.info
        seq = self._tick()
        attr = info.load_offs.get(offset)
        if attr is not None:
            obj = frame.f_locals.get(code.co_varnames[0], MISSING)
            if obj is not MISSING:
                self.keep.append(obj)
                self.events.append(["load", fs.act, seq, id(obj), attr])
        if fs.open_call:
            calls, fs.open_call = fs.open_call, []
            for call in calls:
                self._end_call(call, seq, "ok" if offset == call.next_off else "exc")
        if fs.pending_for:
            pending, fs.pending_for = fs.pending_for, []
            for loop, next_off, fetch in pending:
                if offset == next_off:
                    self._start_iter(fs, loop, seq, fetch, frame)
                else:
                    self.events.append(["exhaust", loop["id"], fs.act, fs.entries[loop["id"]], seq])
        line = info.line_of.get(offset)
        if line is not None:
            if line != fs.cur_line:
                if fs.open_line is not None:
                    self._end_line(fs, seq, "exc" if fs.open_line.raised else "ok")
                fs.cur_line = line
                plans = info.lines.get(line)
                if plans:
                    self._start_line(fs, plans, seq, frame)
            for loop_id, it in list(fs.loops.items()):
                if it is None:
                    continue
                low, high = it.loop["line_range"]
                if not low <= line <= high:
                    self._end_iter(fs, it, seq, False, frame)
                    fs.loops[loop_id] = None
                    continue
                if it.loop.get("track_lines"):
                    it.lines.add(line)
        for loop_id, it in fs.loops.items():
            if it is not None and it.body_caps is None and offset == info.body_offs.get(loop_id):
                it.body_caps = self._eval_keys(frame, it.loop.get("keys")) or {}
                caps = self._capture(frame, it.loop.get("capture_body"))
                if caps:
                    it.body_caps.update(caps)
        for loop in info.get_iters.get(offset, ()):
            it = fs.loops.get(loop["id"])
            if it is not None:
                self._end_iter(fs, it, seq, False, frame)
            fs.entries[loop["id"]] += 1
            fs.loops[loop["id"]] = None
        for loop in info.for_iters.get(offset, ()):
            it = fs.loops.get(loop["id"])
            if it is not None:
                self._end_iter(fs, it, seq, True, frame)
            fs.loops[loop["id"]] = None
            fs.pending_for.append((loop, info.next_off.get(offset), seq))
        for site in info.sites.get(offset, ()):
            names = site.get("capture")
            objects = []
            if names:
                local = frame.f_locals
                for name in names:
                    value = local.get(name, MISSING)
                    if value is not MISSING:
                        objects.append(value)
            call = CallInstance(
                site, fs.act, seq, info.next_off.get(offset), self._capture(frame, names), objects
            )
            fs.open_call.append(call)
            self.open_calls.append(call)
        return None

    # -- instances -------------------------------------------------------------------------
    def _end_call(self, call, seq, outcome):
        try:
            self.open_calls.remove(call)
        except ValueError:
            pass
        self.events.append(["call", call.site["id"], call.act, call.start, seq, outcome, call.captures])

    def _start_iter(self, fs, loop, seq, fetch, frame):
        fs.loops[loop["id"]] = Iteration(
            loop, fs.entries[loop["id"]], seq, fetch, self._capture(frame, loop.get("capture"))
        )

    def _end_iter(self, fs, it, seq, completed, frame):
        end_caps = self._capture(frame, it.loop.get("capture")) if frame is not None else None
        self.events.append(
            [
                "iter",
                it.loop["id"],
                fs.act,
                it.entry,
                it.start,
                seq,
                completed,
                it.start_caps,
                it.body_caps,
                end_caps,
                sorted(it.lines) if it.loop.get("track_lines") else None,
                it.fetch,
            ]
        )

    def _start_line(self, fs, plans, seq, frame):
        merged = {"id": plans[0]["id"], "capture": [], "types": [], "chain": False, "evals": []}
        ex = LineExec(plans, seq)
        for plan in plans:
            ex.attrs.extend(plan.get("attrs", ()))
            merged["capture"].extend(plan.get("capture", ()))
            merged["types"].extend(plan.get("types", ()))
            merged["evals"].extend(plan.get("evals", ()))
            merged["chain"] = merged["chain"] or plan.get("chain", False)
        local = frame.f_locals
        ex.evals = sorted(set(merged["evals"]))
        ex.evals_before = self._eval_objects(local, ex.evals)
        ex.attrs_before = self._attr_facts(local, ex.attrs)
        if ex.lost:
            ex.names_at_start = {name: id(value) for name, value in local.items()}
            for watch in ex.lost:
                if watch.sliced:
                    watch.made = builtin_slice(static_eval(watch.sliced, local))
        watched = []
        for name in merged["capture"]:
            value = local.get(name, MISSING)
            if value is not MISSING:
                watched.append(value)
                ex.names[name] = id(value)
        for call in self.open_calls:
            watched.extend(call.objects)
        seen = set()
        for value in watched:
            if id(value) not in seen:
                seen.add(id(value))
                self.keep.append(value)
                ex.watched.append(value)
                ex.before.append([id(value), type(value).__name__, snapshot(value)])
        for name in merged["types"]:
            value = local.get(name, MISSING)
            ex.types[name] = (
                None
                if value is MISSING
                else type(value).__name__
                + ("" if type(value).__module__ == "builtins" else "@" + type(value).__module__)
            )
        if merged["chain"]:
            ex.chain = self._chain(frame)
        kept = next((plan["kept"] for plan in plans if "kept" in plan), None)
        code = frame.f_code
        if kept is not None and (kept.get("receiver") or code.co_argcount):
            obj = local.get(kept.get("receiver") or code.co_varnames[0], MISSING)
            if obj is not MISSING:
                self.keep.append(obj)
                ex.kept_self = obj
                ex.kept = self._kept_state(obj, kept)
                ex.kept.update(kept_class_facts(obj, kept["prop"], kept["attr"], kept["getter"]))
        fs.open_line = ex

    def _kept_state(self, obj, kept):
        """What a kept-value claim reads of `obj`: the identity of the value its `__dict__` keeps
        under the property's name, and the identity and contents of its written attribute."""
        attrs = instance_dict(obj)
        if attrs is None:
            return {"self": id(obj), "kept": None, "attr": None}
        value = attrs.get(kept["prop"], MISSING)
        held = attrs.get(kept["attr"], MISSING)
        for v in (value, held):
            if v is not MISSING:
                self.keep.append(v)
        return {
            "self": id(obj),
            "kept": None if value is MISSING else id(value),
            "attr": None if held is MISSING else [id(held), snapshot(held)],
        }

    def _chain(self, frame):
        chain = []
        while frame is not None:
            code = frame.f_code
            if not self._is_project(code):
                frame = frame.f_back
                continue
            info = self.codes.get(code) or self._code_info(code)
            fs = self.frames.get(id(frame))
            pos = info.pos_of.get(frame.f_lasti)
            chain.append(
                [
                    info.path,
                    code.co_qualname,
                    pos[0] if pos else frame.f_lineno,
                    pos[1] if pos else None,
                    fs.act if fs is not None and fs.frame is frame else -1,
                ]
            )
            frame = frame.f_back
        return chain

    def _eval_objects(self, local, exprs):
        out = {}
        for expr in exprs:
            value = static_eval(expr, local)
            if value is UNKNOWN:
                out[expr] = None
                continue
            self.keep.append(value)
            out[expr] = [id(value), type(value).__name__, snapshot(value)]
        return out

    def _end_line(self, fs, seq, outcome):
        ex = fs.open_line
        fs.open_line = None
        lost = self._lost_facts(fs, ex) if ex.lost and outcome == "ok" else ()
        after = [[id(v), type(v).__name__, snapshot(v)] for v in ex.watched]
        evals_after = self._eval_objects(fs.frame.f_locals, ex.evals) if ex.evals else {}
        local = fs.frame.f_locals if ex.names or ex.attrs else {}
        attrs_after = self._attr_facts(local, ex.attrs)
        after_names = {}
        for name in ex.names:
            value = local.get(name, MISSING)
            if value is not MISSING:
                self.keep.append(value)
                after_names[name] = id(value)
        kept_facts = None
        if ex.kept is not None:
            kept = next(plan["kept"] for plan in ex.plan if "kept" in plan)
            kept_facts = {"before": ex.kept, "after": self._kept_state(ex.kept_self, kept)}
        self.events.append(
            [
                "line",
                ex.plan[0]["id"],
                fs.act,
                ex.start,
                seq,
                outcome,
                ex.chain,
                ex.before,
                after,
                ex.muts,
                ex.types,
                ex.names,
                after_names,
                ex.evals_before,
                evals_after,
                ex.attrs_before,
                attrs_after,
                kept_facts,
            ]
        )
        for extra in ex.plan[1:]:
            self.events.append(
                [
                    "line",
                    extra["id"],
                    fs.act,
                    ex.start,
                    seq,
                    outcome,
                    ex.chain,
                    ex.before,
                    after,
                    ex.muts,
                    ex.types,
                    ex.names,
                    after_names,
                    ex.evals_before,
                    evals_after,
                    ex.attrs_before,
                    attrs_after,
                ]
            )
        self._lost_results(fs, lost)
        for watch, facts in lost:
            self.events.append(["lost", watch.id, fs.act, ex.start, seq, outcome, facts])
        for watch in ex.lost:
            watch.held.clear()

    def _close_frame(self, fs, seq, outcome):
        calls, fs.open_call = fs.open_call, []
        for call in calls:
            self._end_call(call, seq, "exc")
        if fs.open_line is not None:
            self._end_line(fs, seq, "exc" if outcome == "exc" or fs.open_line.raised else "ok")
        for it in fs.loops.values():
            if it is not None:
                self._end_iter(fs, it, seq, False, None)
        fs.loops.clear()


def _by_path(entries):
    out = collections.defaultdict(list)
    for entry in entries:
        out[entry["path"]].append(entry)
    return out


# ---------------------------------------------------------------------------------------------
# Static evaluation of a join key: loads that run no code of the program
# ---------------------------------------------------------------------------------------------
UNKNOWN = object()
UNKNOWN_TEXT = "?"
_PLAIN = (int, str, bytes, float, bool, type(None))


def _is_plain_key(value):
    """Whether hashing and comparing `value` runs no program code: an exact builtin type (a
    subclass may define `__hash__`, `__eq__`, or `__index__`), tuples of them included."""
    if type(value) in _PLAIN:
        return True
    if type(value) is tuple:
        return all(_is_plain_key(v) for v in value)
    return False


def static_getattr(obj, name):
    """`obj.name` when loading it runs no Python code of the program, else `UNKNOWN`."""
    for klass in type(obj).__mro__:
        attr = klass.__dict__.get(name, MISSING)
        if attr is MISSING:
            continue
        if isinstance(attr, types.MemberDescriptorType):
            try:
                return attr.__get__(obj, type(obj))
            except AttributeError:
                return UNKNOWN
        if hasattr(type(attr), "__get__") and (
            hasattr(type(attr), "__set__") or hasattr(type(attr), "__delete__")
        ):
            return UNKNOWN
        break
    attrs = getattr(obj, "__dict__", None)
    if isinstance(attrs, dict) and name in attrs:
        return attrs[name]
    for klass in type(obj).__mro__:
        attr = klass.__dict__.get(name, MISSING)
        if attr is not MISSING:
            if hasattr(type(attr), "__get__"):
                return UNKNOWN
            return attr
    return UNKNOWN


def static_eval(expr, local):
    """Evaluate a key expression (`a`, `a.k`, `a.x.y`, `a["k"]`, `a[0]`, `a[k]` of a name holding a
    plain key) without running code."""
    import ast

    try:
        node = ast.parse(expr, mode="eval").body
    except SyntaxError:
        return UNKNOWN
    return _static_eval(node, local)


def _static_eval(node, local):
    import ast

    if isinstance(node, ast.Name):
        return local.get(node.id, UNKNOWN)
    if isinstance(node, ast.Attribute):
        base = _static_eval(node.value, local)
        if base is UNKNOWN:
            return UNKNOWN
        return static_getattr(base, node.attr)
    if isinstance(node, ast.Subscript) and isinstance(node.slice, (ast.Constant, ast.Name)):
        base = _static_eval(node.value, local)
        key = _static_eval(node.slice, local)
        if type(base) in (dict, list, tuple) and key is not UNKNOWN and _is_plain_key(key):
            if type(base) is dict and not all(_is_plain_key(k) for k in base):
                # A stored key of the program's class may run its `__eq__` on a hash collision.
                return UNKNOWN
            try:
                return base[key]
            except (KeyError, IndexError, TypeError):
                return UNKNOWN
        return UNKNOWN
    if isinstance(node, ast.Constant):
        return node.value
    return UNKNOWN
