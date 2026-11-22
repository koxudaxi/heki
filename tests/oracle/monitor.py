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
  names_before, names_after]`: `names_*` map the captured names to the identity of what they
  hold when the line starts and ends;
- `["ret", ret, act, seq, value_id, type_name, base, self_id]`: `base` is `list`, `dict`, or
  `set` for an instance of one (a subclass too), else the type's name; `self_id` is the identity
  of the frame's first argument (`None` without one), the instance a getter computed for;
- `["load", act, seq, object_id, attr]`: an attribute load `<first argument>.<attr>` of a frame ran,
  for the attributes a kept-value claim names (`MUT101`);
- `["kept_ret", ret, act, seq, self_id, kept_id]`: the writer of a kept-value claim returned
  normally; `kept_id` is the identity of what its first argument's `__dict__` holds under the
  property's name then (`None`: nothing). A line of such a writer carries the same facts at its
  start and end as a last element of its `line` event (`kept_facts`);
- `["fake", seq, api, kind, waits, detail, stack, input_ids, output_id]`.
"""

from __future__ import annotations

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
        self.watched = False
        self._instructions = instructions

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
        "kept",
        "kept_self",
    )

    def __init__(self, plan, start):
        self.plan = plan
        self.start = start
        self.names = {}
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
        if isinstance(callable_, (types.FunctionType, types.MethodType)):
            # A method a program class defines (an override of `append`) is not the builtin one.
            return None
        name = getattr(callable_, "__name__", None)
        receiver = getattr(callable_, "__self__", None)
        if receiver is None or isinstance(receiver, types.ModuleType):
            receiver = arg0
        if name is not None and mutating_method(receiver, name):
            self.keep.append(receiver)
            fs.open_line.muts.append([id(receiver), name, type(receiver).__name__])
        return None

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
        merged = {"id": plans[0]["id"], "capture": [], "types": [], "chain": False}
        for plan in plans:
            merged["capture"].extend(plan.get("capture", ()))
            merged["types"].extend(plan.get("types", ()))
            merged["chain"] = merged["chain"] or plan.get("chain", False)
        ex = LineExec(plans, seq)
        local = frame.f_locals
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

    def _end_line(self, fs, seq, outcome):
        ex = fs.open_line
        fs.open_line = None
        after = [[id(v), type(v).__name__, snapshot(v)] for v in ex.watched]
        local = fs.frame.f_locals if ex.names else {}
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
                ]
            )

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
    if isinstance(value, _PLAIN):
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
    """Evaluate a key expression (`a`, `a.k`, `a.x.y`, `a["k"]`, `a[0]`) without running code."""
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
    if isinstance(node, ast.Subscript) and isinstance(node.slice, ast.Constant):
        base = _static_eval(node.value, local)
        if type(base) in (dict, list, tuple):
            try:
                return base[node.slice.value]
            except (KeyError, IndexError, TypeError):
                return UNKNOWN
        return UNKNOWN
    if isinstance(node, ast.Constant):
        return node.value
    return UNKNOWN
