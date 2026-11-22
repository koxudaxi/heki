# Changelog

All notable changes to heki are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). heki is experimental: until 1.0, any
release can change rules, configuration keys, diagnostics, and output formats.

## [0.2.2] - 2026-11-22

### Added

- `heki check --generate-prompt [QUESTION]` (experimental) prints a prompt for consulting a
  language model about the run, from the analysis `heki check` runs with the same options: the
  effective configuration (the project root, the configuration file, the target version, the
  source roots, the rule selection, the baseline, and the declared contracts as TOML), the rules
  that apply with their code, name, certainty, activation, and what a report states, the run's
  diagnostics in the `concise` form (up to 16 KiB of lines, with a note counting the rest, and the
  files heki could not analyze), the question, and how to answer (fix at the root cause, do not
  suppress, check heuristic reports, rely on proven fixes, how a contract is written). With
  `--output-format json` it prints the same content as one versioned object, whose JSON Schema
  the new `heki check --output-format-json-schema generate-prompt` prints. It writes no file and
  exits with 0 once the prompt is printed; it cannot be combined with `--fix`, `--diff`,
  `--watch`, `--update-baseline`, `--profile`, or `--include-size-dependent`. A word after the
  option is the question, so paths go before it.
- `heki recommend [PATHS]` (experimental): advice on the configuration of a project, with counts
  from the analysis `heki check` runs on it, and the `[tool.heki]` keys to add. It takes
  `heki check`'s analysis options (`--select`, `--extend-select`, `--ignore`, `--heuristic`,
  `--config`, `--target-version`, `--baseline`) and prints what `heki check` shows with them
  (exact and heuristic reports, the heuristic reports and rules `lint.heuristic` leaves out, and
  with a baseline the allowed reports and the regressions), the rules worth enabling, a note when
  the paths given leave Python files below the project root out, and the keys to add for the
  file the project reads its configuration from. The rules worth enabling are every rule the
  configuration leaves off, and every exact rule whose heuristic reports it withholds, each with
  the reports naming it would add on the project, its consequence, and the published measurement
  of how often its reports are right (or "not measured"). Its block enables a rule in
  `lint.extend-select` when the rule reports there, is not experimental, reports bugs, and no
  published measurement puts its share of real harm below 10%; the other rules are listed with
  the reason they stay out. `--output-format json` prints one object, whose JSON Schema
  `heki recommend --output-format-json-schema` prints and the documentation publishes. It
  writes no file, and exits with 0, or 2 for configuration, I/O, or syntax errors. See
  Configuration Advice in the documentation.
- An Agent Skill for coding agents (experimental), at `skills/heki/` in the repository: it
  teaches an agent to run `heki inspect` before editing a function and `heki check --output-format
  json` after the edit, to follow `related` to the root cause and read `heki rule <CODE>`, to treat
  heuristic reports as possibly wrong, to apply only the fixes heki proves safe, and to use
  `heki check --generate-prompt` when unsure. Claude Code installs it from the repository's
  marketplace (`claude plugin marketplace add koxudaxi/heki`, then
  `claude plugin install heki@heki`), Codex and other agents with
  `gh skill install koxudaxi/heki heki --agent <agent>` or `npx skills add koxudaxi/heki`, or by
  copying the directory; the Coding Agents page documents each way.
- A pre-commit hook in heki's own repository: `repo: https://github.com/koxudaxi/heki` with a
  `rev` and `id: heki` runs `heki check` on the whole project. On its first run, pre-commit builds
  heki from that release's source with cargo (Rust 1.99 or newer; when cargo is missing, the
  build installs Rust into a cache directory first). The `uvx heki check` local hook stays the
  alternative that runs the PyPI wheel and needs no compile.
- `heki rule --output-format json` lists the premises of each exact rule's claim, with what each
  states and where heki decides it (`premises`, empty for a rule without an exact claim). The
  rule pages list the same premises under "What a report states".
- `own-receiver` on `[[tool.heki.write-owners]]`, `[[tool.heki.read-owners]]`, and
  `[[tool.heki.call-owners]]`: `"report"` (default, the behaviour so far) or `"allow"`. With
  `"allow"`, an access through the receiver of the method it is judged in is allowed: the
  function reported (with `attribution = "lexical"`, the function holding the access) is a method
  that is no static method, and the object the member is looked up on may be its first parameter
  (`self._cache[k] = v`, `self._seen`, `cls._kind()`, `super()._label()`, `self._norm(x)` of a
  static method, `Resolver._label(self)`, `setattr(self, name, v)`), also for a member the class
  inherits. Another parameter, an attribute of the receiver, a static method called through its
  class, the parameter of a static method or of a function, and a nested `def`, lambda, or
  comprehension using the enclosing method's `self` stay reported. A layer rule such as "a module
  reaches a private member of another layer only through its own receiver" needs no list of
  methods.
- `allowed-protocols` on `[[tool.heki.write-owners]]`, `[[tool.heki.read-owners]]`, and
  `[[tool.heki.call-owners]]`: project protocols, as class patterns (their subclasses included).
  A read, call, or write through a value typed with a protocol the key names that declares the
  member, itself or through its protocol bases, is allowed wherever it is judged:
  `allowed-protocols = ["*Host"]` allows `parser._flag` with `parser: RenderHost` and
  `class RenderHost(RuntimeFlags, Protocol)` when `RuntimeFlags` declares `_flag`, also a write
  (for the protocol's member and each implementation's attribute), one a private helper makes, and
  a reference `references` counts (`return h._helper`), while the same access through
  `flags: RuntimeFlags`, through the implementation's class, or through a protocol that does not
  declare the member stays reported.
- `unlisted-dict-keys` on `[[tool.heki.write-owners]]`: `"ignore"` (default, the behaviour so far)
  or `"report"`. With `"report"`, `MUT201` also reports a constant key written through the
  namespace of an object of a class a target covers (`field.__dict__[KEY] = v`,
  `field.__dict__.setdefault("_name_cache", v)`, `del vars(field)["k"]`) when no target covering
  that class names the key: "writes `pkg.model.Field._type_cache` (dict_direct), a `__dict__` key
  no target names". A new cache key on a guarded class then needs a target first. A target names
  the keys its attribute pattern matches, and an entry target names its container; the contract's
  allowed writers, exempt methods, own initialization, `fresh-objects`, and `attribution` apply as
  to a target write. A key computed at run time, a plain attribute store, and a write in a helper
  that does not see the object's class are not reported this way.
- `references = "reads"` on `call-owners` and on effect boundaries (`calls`): what
  `"calls-and-bases"` counts, and every other use of a target's value, reported as "`build` reads
  `pkg.core.Keys`": the object of an attribute access and a method's receiver (`Keys.of(names)`,
  `Keys.__annotations__`, `Code.OK`), an argument of a call that only names it (`isinstance(value,
  Keys)`, `cast(Keys, x)`), a subscript or its index (`Pair: TypeAlias = tuple[Keys, int]`), an
  operand (`f is decide`), a tested condition, an iterated, awaited, formatted, or `with` value, a
  dict display's key, the class of a class pattern (`case Keys():`), the types of an `except` clause
  an exception may reach, a decorator, a default of a function or lambda, a keyword of a class
  statement, and the cause of `raise ... from`, also through copies and the displays holding it. An
  annotation, evaluated or not (and a `type` statement's value), an import, a name bound to the
  target by itself (`Alias = Keys`), and a value nothing uses are no read; a load handing a target
  on stays a hand-on, and a read is matched as a construction is. The other values report as
  before.

- `EFF114` (`recursion-error-as-value`, opt-in, heuristic, experimental): an `except` clause that
  takes the `RecursionError` a recursion reached from its `try` body can raise (`Exception`,
  `BaseException`, `RuntimeError`, `RecursionError`, a bare `except`, or a tuple with one), when
  no path through the clause raises, so the function goes on with a value that depends on the
  depth of the stack. The recursion is a call cycle through calls whose bodies run on the
  caller's stack, across modules; the report names the function on the cycle and the calls down
  to it. A clause that re-raises `RecursionError` first, raises on a path, exits, calls a project
  function that raises on every path, or reports the error and returns an exit status (a CLI
  boundary) is not reported.

### Changed

- `MUT201`, `MUT203`, `MUT204`: an access matching several targets of one contract is reported
  once, naming the others. A write on an instance of a subclass matching a target on the class and
  one on the subclass, or a write through a protocol matching the protocol's member and the
  implementation's attribute, was one report per target; it is now one report of the contract,
  under the target configured first, with the others listed in its message (`(also
  `pkg.parser._flags.RuntimeFlags._flag`)`). Accesses of different targets behind one call stay
  one report each. This changes the default output of contracts whose targets overlap: fewer
  reports, each naming the targets it stands for.
- `heki rule --output-format json` prints each rule's published precision: `certification` (the
  release that certified an exact rule, and the reports of its census and holdout run with the
  false claims among them) and `measurement` (the latest round: the sample, its C / P / B / F
  classes, the share right about the code with its 95% interval, and the share of real harm),
  `null` when there is none. The tables of the Rule Precision page are written from the same
  data.
- Rule pages name each premise once: `DATA101`'s "On every iteration" is "Every iteration", as
  on `EFF103`'s page, its loop-carried premise is "Loop-carried on every path", and the `OPT`
  pages' "Builtin list", "Builtin set or dict", "Immutable type", and "Builtin container" are
  "Builtin type by construction".
- The Coding Agents page's typical instruction for an agent runs `heki check --output-format json`
  (with `--heuristic` as an option) instead of always adding `--heuristic`, and ends with
  `heki check --generate-prompt` for unclear cases.
- `OPT120` / `OPT121` fixes reuse an existing `from collections import deque` or
  `import collections` only when nothing else binds or deletes the name: an import that a
  function deletes (`global deque; del deque`) is no longer reused (the fix adds
  `import collections`), and in a module with a star import the rewrite is left to be made by
  hand. Reports are unchanged. Choosing the import no longer walks the module body once per
  block, so checking a module body with many loops is faster.

### Fixed

- Several analyses no longer take time quadratic in the size of their input, and their reports
  are unchanged. The type inference hands a function's closure cells to the generator expressions
  it creates (and to its comprehensions with a scope of their own, before Python 3.12) in time
  linear in the cells' bindings. The iterables those scopes receive from the function creating
  them are sized once per function, for the IR's loop bounds and for the rules that skip loops of
  fixed size (`EFF102`, `EFF103`, `DATA101`, `OPT112`, `OPT113`), and several over the same value
  are sized once. `MUT202`, `MUT114`, and `OPT112` look up the call site of each call, and the
  facts a call runs where the object it returns is used, instead of scanning a module's calls.
  Under an `invalidation` contract, the contracts announced after each project call and write,
  and the next call after it, are looked up instead of scanned. `MUT203` and `MUT204` walk the
  callers of a function once for the accesses they report at the same places (the generator
  expressions of a function share one qualname, so each of their call sites is still reported for
  the accesses of all of them). The constants that argument lists name are looked up from the
  parameters each call binds, instead of comparing every parameter name of every argument list
  with the parameters of every call's callees. The owner contracts decide once per function and
  object whether an access only runs inside allowed functions or a class's own initialization,
  instead of once per access: a public helper making a thousand accesses on its own `self`, run
  by a thousand methods the initializer calls, took about two minutes and now takes a fraction of
  a second.
- A module that hands the dict `globals()` returns to code that may change it no longer keeps its
  constants. A call of a function that the module, an import, a parameter, or a local binds to the
  name of a reading builtin (`def len(ns): ns["KEY"] = "other"`, then `len(globals())`), the dict as
  an index (`obj[globals()]`), as the left operand of `in` (`globals() in obj`), compared with
  another object by `==`, `<`, or another rich comparison (`obj == globals()`), or as an argument
  after the first of a reading builtin (`iter(f, globals())`) now leaves the module's names unfixed:
  a `Final` key of such a module gives `MUT205` in `obj.__dict__[KEY]` instead of a `MUT201` naming
  the attribute of its first value. Reading the dict with the real builtins (`len(globals())`,
  `globals()[name]`, `name in globals()`, `globals() is obj`) still keeps them.
- A module with a star import fixes none of the names the import may bind. A module-level name
  bound before the import holds no constant, also one that starts with an underscore (the imported
  module's `__all__` may list it), and no module-level name counts as bound once for the path
  facts. The module's builtins are no longer taken for the real ones: the import may bind the name
  of any builtin (`from numpy import *` binds `sum`, `min`, `max`, `any`, and `all`), heki does not
  read which names the imported module exports, and a function may run after the import wherever
  it stands. So, in such a module, `time.sleep(_PAUSE)` with `_PAUSE` bound before the import is
  a wait for `EFF101`, and a `__dict__` key in such a name gives `MUT205` instead of a `MUT201`
  naming the attribute; `len(globals())` may hand the dict of module globals to another `len`, so
  the module's names are not fixed (a `Final` key in `obj.__dict__[KEY]` is a `MUT205` write, no
  longer a `MUT201` one); and a size that rests on a builtin (`range(len(rows))`, `range(3)`,
  `min` / `max` bounds, `len`, `sorted`, or `list` of a display) is not known: no exact report
  claims it
  (`EFF103` no longer says the loop runs once for each row), and the loop counts as data-sized for
  the rules that report data-sized loops (`EFF113`, `OPT113`). In modules with a star import, the
  default output can therefore show new `EFF101` reports and `MUT205` reports in place of `MUT201`
  ones, and no longer shows the `EFF103` reports that rested on a builtin's size.
- A name a module imports holds the constant of the imported module only where the module binds
  the name nowhere else. An import the module binds again (in a function declaring it `global`,
  in another module-level store, with `del`), or any import of a module whose star import,
  `exec`, or `globals()` may rebind its names, is no constant there, also for the modules
  re-exporting it. With `from settings import FLAG` and a `def flip(): global FLAG; FLAG = False`,
  `levels(kind=FLAG)` no longer matches an argument list naming `kind=True` (`MUT204`, `EFF201`);
  a catalog argument read from such a name is one the call waits on (`EFF101`: `time.sleep(PAUSE)`
  after `global PAUSE; PAUSE = 1` in a function), a string key or name read from it is unknown
  (`MUT201` reports such a forced write as `MUT205`), and a small display read from it is no
  constant (`OPT113`). A global of the same name another module binds for itself changes nothing.
  Existing code in such modules can get new `EFF101` and `OPT113` reports.
- `reads-environment` effect boundaries (`EFF201`) follow the `__path__` of the packages an
  `importlib.import_module` or `importlib.util.find_spec` call imports through. A name under a
  package (`f"pkg.{name}"`) is the project's only while neither that package nor any package below
  it has its `__path__` changed: `pkg/plugins/__init__.py` assigning `__path__` used to leave
  `import_module(f"pkg.{name}")` unreported, although `name` may be `plugins.thirdparty`, an
  installed module. heki follows what any module of the project does to a package's `__path__`,
  its own or another's through an import. Directories added after the package's own
  (`__path__.append(d)`, `extend`, `+=`, `__path__ + ds`, `pkgutil.extend_path`, also on
  `__import__("pkgutil")`, and `pkg_resources.declare_namespace`) let installed modules answer the
  names the package does not hold, so a name under the package is reported, while its own modules
  (`import_module("pkg.plugins.builtin")`) are still found first. Any other change
  (`__path__ = [d]`, `__path__.insert(0, d)`, a slice store, `del __path__`), handing the list to
  a function other than a builtin that only reads it and `pkgutil.iter_modules`, `walk_packages`,
  and `extend_path`, and a package heki cannot read make every import below the package a read of
  the environment, its own modules included. Reading the path (`pkgutil.iter_modules(__path__)`,
  iterating it, `__path__[0]`) changes nothing, and a regular package of the project, and its
  modules, stay the project's below a package that only adds directories after its own. The
  language server follows a package gaining or losing such a change, and a package with one
  appearing below an imported name. Boundaries that forbid `reads-environment` can report imports
  they passed before.
- A class statement's bases are the values the statement passes where it runs, not the classes
  the module imports under the same names. After `from pkg.core import Rule`, a class whose base
  is a parameter, a local, a local bound on one path, an enclosing function's parameter, or a
  class-body name of that name (`def factory(Rule): class Derived(Rule): ...`), or an attribute of
  such a name (`core.Rule` with a parameter `core`), is no longer a subclass of `pkg.core.Rule`:
  constructing it no longer gives `MUT204` "constructs `pkg.core.Rule`", a method it does not
  define no longer resolves to `Rule`'s (`MUT204` "calls `Rule.check`", `EFF101` and `EFF111`
  through a waiting call of `Rule`'s), its overrides no longer join the calls on a `Rule`
  receiver, and with `references = "calls-and-bases"` it no longer gives `MUT204` or `EFF201`
  "subclasses `pkg.core.Rule`". A base reached through a local alias of the import
  (`Base = Rule`, then `class Derived(Base)`), an import in the function, a class-body alias, or a
  name the class body binds only after the statement (which Python then reads from the module) is
  now the class, and `references = "calls-and-bases"` also counts a base read from a closure cell
  holding it. A module-level base whose name holds the class on one path and another class on
  another (`try: from pkg.core import Base` / `except ImportError: Base = object`) no longer counts
  for `"calls-and-bases"`, as a construction through that name does not. Projects with
  `call-owners` contracts or effect boundaries can get new `MUT204` and `EFF201` reports for such
  aliased bases, and lose the false ones above.
- A base heki names no class for keeps its place in the class's MRO as a base heki knows nothing
  of: a value its class statement's scope binds, an expression (`with_metaclass(Meta)`, `*bases`),
  a module variable (`Base = declarative_base()`), and the bases of a class statement in a function
  too large to model. A method or special method no project class along the MRO defines may come
  from it, so a call of one is unresolved instead of one an instance lacks, `super().__new__(cls)`
  is no plain allocation, and a union holding the class no longer drops it from a call it may
  answer.
- A value annotated `type[C]` or `typing.Type[C]` (a parameter, a local, an attribute, a module
  global, a return or property annotation, the items of a container, also optional or quoted) is
  the class object of `C` or of a subclass, as `cls` in a class method is: calling it constructs
  `C`, a class method called on it runs `C`'s, and an attribute read through it reads `C`'s class
  attribute. `MUT204` missed these constructions and calls (also for a target with an argument
  list, `factory(is_dict=True)` with `factory: type[DataType]`) and now reports them; a class
  object of a protocol, of `Any`, or of one of several classes is of no known class, as before.
  `typing.Type[C]` of such a class is now of no known class too, as `type[C]` was.
- Calling a property's value, `self.data_type(is_dict=True)`, runs the getter as reading the
  property does and calls what the getter's return annotation says it returns, so a property
  returning `type[DataType]` constructs `DataType` there and one returning an instance calls its
  `__call__`. heki took such a call for a call of the getter with the call's arguments, so `MUT204`
  missed it, and a target with an argument list never matched it.
- `MUT203` (`read-owners`) and an effect boundary's `read-targets` see reads through a class
  object, which read the class attribute: `DataType.HOLDS_MEMBERS`, `cls.HOLDS_MEMBERS` in a class
  method, `type(data).HOLDS_MEMBERS`, and `model_type.HOLDS_MEMBERS` with `model_type:
  type[DataType]`, also through `getattr`. Only reads through an instance were reported, so
  contracts on class attributes can get new reports.
- A call target with an argument list (`pkg.f(kind=True)`) reads a keyword as Python binds it: a
  keyword naming a positional-only parameter no longer passes its value to that parameter. With
  `def f(kind=False, /, **kwargs)`, `f(kind=True)` leaves `kind` false and puts `True` in
  `kwargs`, so `MUT204` and `EFF201` no longer report it as a call of `f(kind=True)`, and report it
  as one of `f(kind=False)`; `f(True, kind=False)` passes `True`. A call that raises `TypeError`
  instead of running the function passes nothing: a parameter bound by a position (or the
  receiver) and a keyword, a keyword naming no parameter of a function without `**kwargs`, a
  position past the parameters of a function without `*args`, or a parameter without a default
  left unbound (`guarded(kind=True)` with `def guarded(required, *, kind=True)`), unless `*` or
  `**` unpacking may bind it. The receiver of a method call is its first position: with
  `def check(*args, kind=False)` in a class, `obj.check(kind=True)` passes `kind=True` (the
  receiver lands in `*args`), and a method taking no positional parameter and no `*args` raises.
  A keyword naming no parameter of a function with `**kwargs` still passes its value under its
  name, and a keyword never binds `*args` or `**kwargs` themselves. The rules that follow an
  argument into the project function it is passed to (a parameter the function changes, consumes,
  or tests against a constant) no longer bind such a keyword to a positional-only parameter or to
  `*args` / `**kwargs` either.
- An argument list matches a module's number or boolean constant bound once, as `EFF101` evaluates
  them: with `FLAG = True` or `LIMIT = 0` in a module (also imported from another module, unless a
  module rebinds it), `f(kind=FLAG)` passes `True` and `f(level=LIMIT)` passes `0`. It reads a
  module's constants through every form of the read: a name the function imports itself
  (`from settings import FLAG` inside it) and an attribute of the module (`settings.FLAG`,
  `cfg.LIMIT`) pass their values as a module-level name does, under the same rules on rebinding.
  The language server follows the constant's module changing it. `MUT204` and `EFF201` report the
  calls passing such constants, which no argument list matched before.
- A call target with an argument list (`"pkg.types.DataType(is_dict=True)"`), and an effect
  boundary's `call-targets` entry with one, match a call passing the constant through a `**` dict
  display with string constant keys that nothing changes: `DataType(**{"is_dict": True})`,
  `flags = {"is_dict": True}` then `DataType(**flags)`. The display's entries bind as written
  keywords bind, also into `**kwargs`, a parameter without a default, or a dataclass field. A call
  naming one keyword twice (`DataType(is_dict=True, **{"is_dict": True})`, or in two displays)
  raises `TypeError` and passes nothing; `MUT204` reported such a call. Any other `**` mapping (a
  parameter, `dict(...)`, a display something changes, a display with a computed key) still passes
  nothing it holds.
- A positional argument binds only a positional parameter, as Python binds it: one past them lands
  in `*args` (a new tuple holding it), or the call raises `TypeError`, and binds no parameter of
  the function. With `def log(msg, *args, extra=None)`, `log("x", items)` no longer binds `items`
  to `extra` (a false `MUT110` when `log` changes `extra`), and with `def drain(*its)` iterating
  `its`, `drain(gen)` no longer consumes `gen` (a false `MUT106`). The rules that follow an
  argument into the project function it is passed to all bind positions this way. Whether the
  function keeps such an argument is what it does with `*args`, which holds it: a copy handed to a
  helper's `*objs` in a second position is no longer taken to be under construction when the
  helper keeps the tuple (`MUT201` / `MUT205` missed the write after it, and now report it), and
  one handed beside a keyword-only parameter is judged by `*args`, not by that parameter. A copy
  handed by keyword after `*` unpacking (`show(*extra, label=dup)`) is bound to its parameter
  exactly: a helper that does not keep that parameter no longer makes the copy leave its
  construction (a false `MUT205` / `MUT201`).
- With `references = "calls"` (or `"calls-and-bases"`) on `call-owners` and on the `calls` token of
  effect boundaries, a scope nested in a function hands on what the function binds to a name it
  reads: a class body (`class Config: options = staticmethod(Options)` after `from pkg.core import
  Options` in the function), a nested function, a lambda, a generator expression, and a
  comprehension with a scope of its own (before Python 3.12), also inside a class body. `MUT204`
  and `EFF201` missed these references and now report them at the name the nested scope reads, as
  a module import read there already was.
- A write through an alias of a guarded attribute is followed into the scopes nested in the
  function binding it: `extras = field.extras` and then `def inner(): extras["a"] = 1`,
  `lambda: extras.setdefault("b", 2)`, a comprehension or generator expression with a scope of its
  own, at any depth, is a `MUT201` write of `Field.extras`, as the same write in the function is.
  An alias of an entry of the attribute's container (`entry = field.meta["k"]`) is one too, and,
  with `unresolved = "report"`, an alias of an attribute of an object of unknown class is a
  `MUT205` write. A variable a nested scope rebinds (`nonlocal`), or one the function binds to two
  different attributes, holds no alias. `write-owners` contracts can report these writes, which
  they missed.
- `vars(obj)` (the builtin, not a name shadowing it) is `obj.__dict__`, and `|=` updates either in
  place. `vars(field)[KEY] = 1`, `vars(field).setdefault("required", v)`, `del vars(field)[KEY]`,
  and a local alias of the namespace are `MUT201` writes of the attribute they name;
  `vars(field).update(...)` and `field.__dict__ |= {...}` (also through `vars` or an alias) are
  `MUT205` writes of a name heki cannot tell, as `field.__dict__.update(...)` is. Reads through
  `vars(obj)` are reads (`MUT203`, `MUT206`, `MUT111`, `reads-targets` boundaries), and
  `vars(self).pop("prop", None)` invalidates a cached property for `MUT101`, as
  `self.__dict__.pop("prop", None)` does. An object the function constructs stays under
  construction when it is handed to `vars`, and rebinding an attribute of a shallow copy through
  `vars(copy)` or `copy.__dict__ |= ...` makes it the copy's own, so changing it no longer changes
  the original (`MUT114`, `MUT201` through helpers). Contracts can report these writes and reads,
  which they missed.
- With `unresolved = "report"`, a `write-owners` contract reports (`MUT205`) an attribute name
  computed at run time written on an object of unknown class (`setattr(target, name, v)`,
  `target.__dict__[key] = v`, `vars(target).update(...)` with `target` unannotated), which may
  write any target: "writes an attribute named at run time (set_attr) on an object of unknown
  class". Each half alone was reported already (a computed name on a target class, a target's name
  on an object of unknown class); together they were silent. A parameter that every caller path
  binds to an object of a class no target covers is not reported, nor a helper that only a
  guarded class's initializer runs on its own `self` (`_setup(self, name, value)` in `__init__`).
- `MUT201`, `MUT203`, `MUT204`, `MUT205` with `attribution = "lexical"`: a class's own
  initialization stays allowed through the helpers it passes `self` to, as the configuration
  documentation says. A method or function whose every path of project callers starts in an
  initializer (`__init__`, `__post_init__`, `__new__`, `model_post_init`, `__setstate__`) of a
  target class handing it its own `self` (`self._process_validators()`, `self._add_decorator(self)`,
  `_setup(self)`, a chain of such helpers) is no longer reported for accessing that object. A helper
  also called from elsewhere, or handed another object, is still reported.
- `MUT202` reads what a call runs from the sites of the object that call returns only: a call
  whose generator or coroutine is never iterated or awaited (`kept = gen(shared)`) is no longer
  reported because another call of the same function with the same argument is
  (`for _ in gen(shared):`). The call whose object is iterated, handed to a function iterating
  it, or awaited is still reported.
- `EFF101`, `EFF103`, `DATA101`: looking up a method raises nothing only when no class the value
  may be under R2 runs code on the read. A class that defines `__getattribute__` or has a
  metaclass heki does not know, or a project subclass or one of its other bases that binds the
  name to a property or other code, now puts an exception edge before the call, so a handler
  that goes on after such a lookup leaves the claim open (`EFF111` / `EFF113` / a heuristic
  `DATA101` report instead of a false exact one). `OPT113`: a key a property computes in a mixin
  of a project subclass leaves the exact tier (the report is heuristic), as one the class or a
  subclass computes already did. These were exact reports in 0.2.1 that could be wrong; 0.2.2
  makes them heuristic, so the default output no longer shows them.
- A relative import names the package's own module: `from .typing import TYPE_CHECKING` no
  longer decides a branch as `typing.TYPE_CHECKING` does, and `from . import subprocess` no
  longer counts as the standard library's module for the path facts (looking up its members may
  raise). Exact `EFF101`, `EFF103`, and `DATA101` claims that relied on either are left open:
  these reports are now heuristic, and the default output no longer shows them.
- A branch the module's constants rule out no longer keeps a loop from its path facts. A length
  test that every path to a loop passes, except such a branch (`if DEBUG: ... else: if
  len(rows) > 8: raise`), now caps the loop's count as a test on every path does (`EFF103`,
  `DATA101`, `OPT113`), and a loop that waits on every way back except such a branch
  (`while True: if DEBUG: continue ... time.sleep(1)`) is paced by time, as one that waits on every
  way back is (`EFF102`, `EFF103`, `DATA101`, `OPT112`). A report on such a loop leaves the exact
  tier (or is not made under 16 trips) instead of claiming a count heki does not know to grow,
  and the reports that need a loop sized by data are not made for a paced loop, so the default
  output shows fewer reports on these loops.
- `EFF101` evaluates more catalog arguments read from module constants: a constant computed from
  other constants (`PAUSE = BASE - 2`), one bound after a star import (`from x import *` then
  `PAUSE = 0`), an attribute of the module (`settings.PAUSE`), and a name a function imports
  itself (`from settings import PAUSE` inside it), under the rules on rebinding above.
  `time.sleep(PAUSE)` or `time.sleep(settings.PAUSE)` with such a constant equal to zero is no
  wait, and a database name read as `settings.DB_PATH` names its file. These were exact reports
  in 0.2.1 that could be wrong; 0.2.2 drops them.
- The language server runs its analyses one at a time, in the order of the editor's messages (an
  edit's analysis takes its place when its debounce ends). Each analysis ran on its own thread
  before, in no particular order, so the diagnostics could depend on which one started first: of
  documents opened together, one importing a document opened just before it could be analyzed
  first and keep diagnostics without that document until the next save.
- The language server could keep a deleted module for the rest of a session on Windows: a
  document was keyed by the file's own resolved path, which Windows reports under
  `\$Extend\$Deleted` while a deletion is in progress, so closing the document missed it and the
  following Deleted event was skipped. Documents are now keyed by their resolved directory and
  file name, a Deleted event stays authoritative until the file is created, changed, or saved
  again, and a file counts as gone only when reading it finds no file (not when `exists()` says
  so).

## [0.2.1] - 2026-11-21

### Added

- `heki check` colors its `full` and `concise` output and `--diff` on a terminal, as Ruff does:
  paths in bold, rule codes in red, the severity, the source frame and its caret, and the lines a
  diff removes and adds. `--watch` is colored the same way. The output stays plain when stdout is
  not a terminal, or when `NO_COLOR` is set (or `CLICOLOR=0`); `FORCE_COLOR` or `CLICOLOR_FORCE`
  color it anywhere, also over `NO_COLOR`, and the new `heki check --color auto|always|never`
  decides over every variable. `json`, `github`, and `sarif` output, stderr, and the language server
  are unchanged.
- `heki inspect <SYMBOL> [PATHS]...` (experimental) says what heki knows about one function, from
  the analysis `heki check` runs for the same configuration and selection: the effects it reaches
  with their call chains and guards, the parameters, `self` attributes, other objects, module
  globals, and closure cells it changes (with the chain to the body that changes them), the
  parameters it keeps, what it returns, whether it is pure in heki's sense (`pure`, `impure` with
  the reasons, or `unknown` with the first function whose calls heki cannot resolve, and what the
  verdict assumes), its callers (`--callers --depth N` transitively), the reports `heki check` makes
  in it, and the contracts naming it. The symbol is a dotted name, a dotted suffix, or a glob,
  `path.py:LINE`, or `path.py::NAME`; `--output-format json` writes one versioned object. A symbol
  naming no single function exits with 2 and says why on stderr, listing the candidates when there
  are any; with `--output-format json`, an error object with the same candidates takes the place of
  the report.
- `EFF109` (`lock-reacquired-while-held`, opt-in, experimental) reports a non-reentrant lock
  (`threading.Lock`, `asyncio.Lock`, `asyncio.Condition`, `multiprocessing.Lock`) taken again while
  it is held: by a nested `with` or `acquire()`, or by a function the holder calls on its own thread
  or task, any number of calls deep and across modules (a helper on `self`, an awaited coroutine, a
  property, a generator iterated there), when both sides name the lock the same way (an attribute of
  `self` or of another parameter, a module global, a variable of an enclosing function). It does not
  report an acquisition a constant argument switches off, one behind a test of state the holder
  changes (an attribute of the lock's owner, also through its callees, or a module global it sets,
  tested by the function taking the lock or by one on the way to it), a lock given back before it is
  taken again, a lock whose name may be bound to another object first (by the holder, by a function
  it calls, or on the way to the inner acquisition), or a lock another thread gives back as a
  signal. Its facts are computed only while it is selected. `[[tool.heki.ecosystem]]` takes
  `non-reentrant-lock = true` for other lock classes.
- `EFF110` (`exclusive-object-fanout`, opt-in, experimental) reports an object that allows one
  operation at a time (`asyncio.StreamReader`, an asyncpg `Connection`, a websockets connection, an
  aiohttp WebSocket response) used by tasks a function starts together: two of them, or one started
  for every item, through `asyncio.gather`, `asyncio.wait`, an `asyncio.TaskGroup`, or an anyio task
  group, through helpers and keyword arguments. SQLAlchemy's `AsyncSession` is reported only where
  the fan-out can be its first operation since it was made or committed. It does not report
  different objects (a process's `stdout` and `stderr`), uses under a lock, an object each task gets
  for itself, a task started again only once it is done, or a task that may bind the object's name
  first. Its facts are computed only while it is selected. `[[tool.heki.ecosystem]]` takes
  `one-at-a-time` for other methods.
- `effect-boundaries` scopes can check module bodies, what runs when a module is imported: a scope
  naming `<module>` itself (`pkg.gen:<module>`, `pkg.*:<module>`, `pkg.gen.<module>`, or `<module>`
  for every module) makes `EFF201` check the bodies of the modules it names as it checks a scope
  function, through the calls they make. Their module-level environment reads (`PLATFORM =
  sys.platform`, `if sys.version_info >= (3, 14):`), import probes (`try: import yaml` / `except
  ImportError:`), calls, blocking calls, and writes are reported under the qualname `<module>`, and
  so is what the bodies of their classes take from outside the project (`class Settings: home =
  os.getenv("HOME")`). A scope naming only the module (`pkg.*`, `pkg.gen:*`) still leaves its body
  out, so existing boundaries report as before.
- `attribution` on `[[tool.heki.write-owners]]`, `[[tool.heki.read-owners]]`, and
  `[[tool.heki.call-owners]]`: `"call-graph"` (default, the behaviour so far) or `"lexical"`. With
  `"lexical"`, an access is judged in the function whose code holds it, whoever calls that function:
  a private helper reports its writes, reads, and calls itself instead of handing them to its
  callers, and a function that only runs inside allowed functions is no longer allowed. A lambda,
  comprehension, or generator expression is part of the function it is written in, and a class
  body's code part of the function running the class statement. A class's own initialization,
  `exempt-methods`, `fresh-objects`, and the targets of a call contract calling each other stay
  allowed; for `write-owners`, a call handing a typed object to a helper that changes it without
  knowing its class stays the caller's write. With `unresolved = "report"` (`MUT205`, `MUT206`,
  `MUT207`), the callers binding a parameter then only tell its object's class, allowed callers
  included.
- `references = "calls-and-bases"` on `call-owners` and on effect boundaries (`calls`): what
  `"calls"` counts, and a class statement naming a class target among its bases (`class
  Derived(Base)`, also `core.Base` and `Base[int]`), reported as "`<module>` subclasses
  `pkg.core.Base`" for the function running the class statement. A base is matched as a construction
  is, so with `subclasses = false` only the class itself counts; a keyword (`metaclass=Base`) is no
  base. `"calls"` keeps leaving bases out.
- A `call-owners` target, or an effect boundary's `call-targets` entry, may end in an argument list
  of `parameter=value` entries: `"DataType(is_dict=True)"`, `'container_flags(kind="dict")'`. It
  names only the calls that pass each constant (`True`, `False`, `None`, an integer, or a quoted
  string): heki must establish that the argument bound to the parameter is that constant, a literal
  or a string constant bound once, passed by keyword (also to a dataclass or a pydantic model), by
  position where every function the call may run binds it to the parameter, or left out where every
  such default is the value (`True`, `False`, `None`, `0`, or `""`). A call whose argument heki
  cannot tell is not reported, a reference never matches, and such a target names nothing outside
  the project. Reports name the target with its list (`` `bypass_dict` constructs
  `pkg.types.DataType(is_dict=True)` ``). An argument list heki cannot read is a configuration
  error. `MUT207` matches such a target by its name before the list.

### Changed

- Must-fact chains share their callee's hops, which lowers the peak heap of `heki check` and the
  language server by about 0.5 MiB on the memory gate's project (#90).

### Fixed

- `MUT108` and `MUT110` find the call chain down to the function that changes an argument in time
  linear in the calls they pass: an argument handed down through thousands of functions of one
  module no longer takes time quadratic in them.
- A module that only reads the dict `globals()` returns (`globals().get(name)`, `globals()[name]`,
  `name in globals()`, iterating it, `len(globals())`, also through a local alias) keeps its
  constants: a `Final` key of such a module names its attribute again in `obj.__dict__[KEY]` (a
  `MUT201` write, as in 0.1.2, where 0.2.0 gave `MUT205`). Storing into the dict, deleting from it,
  a method changing it (`update`, `setdefault`, `pop`), or handing it on (an argument of a call that
  does not only read it, a returned or stored value, the builtin `globals` itself) still leaves the
  module's names unfixed.
- `reads-environment` effect boundaries (`EFF201`) no longer report `importlib.import_module` and
  `importlib.util.find_spec` importing a module of the project: every text the name may hold names a
  project module (`"pkg.config"`, a module constant, also an imported one), or starts with a project
  package and a dot (`f"pkg.parser.{name}"`, `"pkg.parser." + name`, `"pkg.%s" % name`) whose
  `__init__.py` heki reads and does not rebind `__path__`. A relative name is anchored at `package`,
  a text or the module's own `__package__` or `__name__`. A name heki cannot tell, a module the
  project does not have, a namespace package, and a call inside a `try` that takes an `ImportError`
  stay reported, and a `call-targets` entry naming `importlib.import_module` still matches every
  call of it. The language server follows a module being created or deleted.
- A variable of an enclosing function read in a nested scope keeps its class: `self` and the other
  variables a method or function binds have the class of what it binds to them in its comprehensions
  and generator expressions with a scope of their own (all generator expressions, and comprehensions
  before 3.12), lambdas, nested functions, and nested classes. Their reads and calls were of unknown
  class, so `unresolved = "report"` contracts reported them under `MUT205`, `MUT206`, and `MUT207`
  (`[name for name in names if name in self._given]` in a method), `MUT201`, `MUT203`, and `MUT204`
  missed the ones on a target class, and the default rules did not follow the calls they make
  (`EFF103` through `any(self._query(k) for _ in ...)`). A comprehension's iteration variable read
  by a lambda or generator expression nested in the comprehension has the class of its items too,
  from 3.12 as before (`[any(n in m.names for n in names) for m in models]`). A variable that a
  nested scope rebinds (`nonlocal`, or an assignment expression in a comprehension before 3.12), or
  that the function binds to a value of unknown class, stays of unknown class.
- `type(obj)` of an instance of a project class is a class object that is the class or a subclass of
  it, as `cls` in a class method is: `type(schema)._first_typed(schema)` calls the `_first_typed` of
  the class and of its subclasses that override it, and `MUT204` reports it where `MUT207` reported
  a call on an object of unknown class. It stays unknown for a union, a protocol, and a class
  outside the project; `obj.__class__` is not followed (a class or a subclass may rebind it).
- `MUT110` and `MUT114` no longer state their exact claim where a project subclass of the builtin
  container overrides the changing method or operator (`reverse`, `setdefault`, `sort`, `|=`,
  `__setitem__`, ...), has a custom metaclass, or defines `__getattribute__` (for a named method
  call; operators and item stores bypass it): under R2 the value may be an instance of that
  subclass, so the line may change nothing. These were exact reports in 0.2.0 that could be wrong;
  0.2.1 makes them heuristic. A cached result the function builds with a display, a comprehension or
  a builtin constructor is exactly the builtin class and stays exact. `OPT113` reads the same fact
  for `__iter__` and the view methods, now for any binding of the name in a project subclass.
- `MUT110` no longer reports a change it reaches only through a helper whose parameter is annotated,
  read-only (that helper is checked on its own) or mutable (a type checker reports the argument).
  Those were exact reports in 0.2.0 that could be wrong; 0.2.1 drops them.
- `EFF101` no longer makes an exact report that a catalog call waits when its argument is a module
  constant, bound once, that ends the wait at once: `time.sleep(PAUSE)` with `PAUSE = 0`, a `data:`
  URL constant for `urlopen`, a `localhost` constant for `getaddrinfo` or `gethostbyname`. Those
  were exact reports in 0.2.0 that could be wrong; 0.2.1 drops them. Imported constants count too,
  unless another module rebinds them.

## [0.2.0] - 2026-11-21

### Breaking changes

- Module names change for projects with a `python/` directory or a directory named after the
  project: they follow the source roots ty detects. Besides `src/`, a `python/` directory (the
  maturin layout) and a directory named after the project that holds one of the same name
  (`acme/acme/`) are source roots when they are not packages, so `python/acme/io.py` is
  `acme.io`, not `python.acme.io`, and the imports of `acme` resolve. The names in messages,
  in contract patterns, and in baseline `target`s change with them, and the default rules can
  report calls through the imports that now resolve. `src = ["src"]` restores the earlier names
  (`src = []` for a project without a `src/` layout).
- The project root of a project without a configuration file changes: it is the current
  directory when that holds the checked paths (the language server: its workspace folder), as
  Ruff and ty root it, instead of the top of the enclosing Git working tree, which stays the
  root when heki runs above it. Outside a Git working tree, `heki check pkg` is rooted in the
  current directory, not in `pkg`. Module names, the `path`s of a `--baseline` file, and the
  place of `.heki-summary-snapshot.json` move with the root; a configuration file keeps the
  root where it is.
- Warnings no longer make `heki check` fail: a run that reports only `warning` diagnostics
  (slower code, by default) prints them and exits 0; an `error` (a bug) still exits 1. The new
  `[tool.heki.lint] fail-on = "warning"` and `heki check --fail-on warning` make warnings fail
  the run again (`--fail-on error` overrides the configuration the other way), in every output
  format and in `--watch`. With a baseline, the same threshold applies to the Ratchet
  regressions; a refused `--update-baseline` and pending `--diff` changes still exit 1.

### Added

- `src` (`[tool.heki]`) lists the source roots, as Ruff's `src` and ty's `environment.root`
  do: directories relative to the project root whose files are named relative to them, so a
  subdirectory application (`src = ["backend"]`, for `backend/config/env.py` imported as
  `config.env`) and a `lib/` layout (`src = ["lib"]`) resolve their imports. It replaces the
  detected source roots.
- The JSON output gives every diagnostic its `certainty`, `"exact"` or `"heuristic"`. SARIF
  gives every rule descriptor `properties.precision` (`very-high` when all its reports are
  exact, `medium` otherwise) and the tag `heuristic` to a heuristic descriptor and to every
  heuristic result, read by GitHub code scanning. The language server sets `data.certainty` on
  every diagnostic and links its code to the rule's page (`codeDescription`). `heki rule --output-format json` gives each
  rule's `certainty`, `activation` (`default`, `opt-in`, or `contract`), `heuristic_note`, and
  `default_enabled` (whether it runs with no configuration).
- `[tool.heki.lint] heuristic`, `heki check --heuristic` / `--no-heuristic`, and
  `heki server --heuristic` / `--no-heuristic`: see the first entry under Changed. A `full` or
  `concise` run that leaves heuristic rules off, or heuristic reports out, says how many, and
  how to show them.
- Every rule page says what a report states: the premises of its claim, and which of them a
  heuristic report may not have established. `docs/precision.md` defines exact and heuristic
  reports, the assumptions R1-R7 exact claims rest on, how a rule is certified, and what each
  rule measured.
- `EFF111` (`async-possibly-blocking-call`) and `EFF113` (`loop-possible-db-query`), heuristic: the
  reports `EFF101` and `EFF103` made before, where the new exact claims are not established. See the
  `EFF101` and `EFF103` entry under Changed.
- `sqlite3.connect` of an in-memory database (`":memory:"`, `file::memory:`, `mode=memory`) does not
  wait, and neither do the statements on that connection. A connection waits only where heki
  establishes that it was opened on a file (a literal or a module constant without an in-memory
  form, or a path built from such parts and `__file__`); any other, one opened from a parameter, an
  f-string, or an environment lookup, or only named by an annotation, may be in memory, so its waits
  are not established (`EFF111`; its statements still count for `EFF103`). Reports name such
  connections' methods by the library's API (`sqlite3.Connection.execute`). A pragma that only reads
  or sets an option of the connection (`PRAGMA busy_timeout = 5000`) does not wait.
- Test code, which `lint.relax-async-in-tests` leaves out of `EFF101`, `EFF111`, `EFF105`, and
  `EFF106`, is what pytest discovers: `test_*` functions and the functions nested in them,
  `test_*.py` and `*_test.py` files at any depth, and files under `test/` or `tests/`.
- The ecosystem catalog says when a call waits. A blocking effect waits for input or output or for
  another thread (`blocking_sync`), which can have done its part already. Every wait entry lists the
  arguments that end it at once: `time.sleep(0)`, `Popen.wait(timeout=0)`, a numeric host or
  `localhost` for `socket.getaddrinfo` and `socket.gethostbyname`, a `data:` URL for `urlopen`; no
  rule reports those calls as waiting. Database entries carry `emits` (`always`,
  `on-identity-map-miss` for `Session.get`, `if-pending` for `Session.flush` / `commit` and a
  driver's `commit()` / `rollback()`, `unless-cached` for Django's `QuerySet.count()` / `exists()` /
  `first()` / `last()`, `unless-connected` for a pool checkout), and context managers
  `exit_suppresses` (`false` for files, locks, HTTP sessions, and database connections; `true` for
  `contextlib.suppress`), and calls whose results the environment sizes `items` (`rows`, `lines`,
  `entries`, `text`). New argument tests: `one_of` and `numeric_address`.
- `MUT206` (`unresolved-guarded-read`) and `MUT207` (`unresolved-guarded-call`): `read-owners`
  and `call-owners` contracts take `unresolved = "report"`, as `write-owners` does, and then
  report the reads of an attribute and the calls of a method named like a target on an object of
  unknown class (an unannotated parameter, a loop variable, an untyped call result, `Any`,
  `object`, or a protocol whose implementations heki cannot list), unless every caller binds an
  instance of a project class outside the targets' classes (for a call target naming no class,
  `reset`, one whose MRO finds no `reset` the target names).
- Contract keys and dynamic names follow module-level string constants bound once in their
  module, and class constants declared `ClassVar` or `Final` and bound once, read on the class,
  an instance, or `cls`; a constant without `Final` stops being one when another module rebinds
  or deletes it (`keys.KEY = "x"`, `Keys.KEY = "x"`), and a class constant when its own module
  stores it on the class or an instance outside the class body, or when a subclass's MRO finds
  another definition. A `Final` field of a dataclass is no constant on an instance.
- `MUT112` (`conditional-ownership-mutation`, opt-in, experimental) reports an in-place change of
  a container that is a new object on one path and borrowed state on another
  (`items = given or []`, `d.get(k, {})`, a callee returning a memo or its argument), made
  directly, through a helper, or in a closure, and a store of such a value in `self.<attr>` that a
  method of the class family changes in place.
- `MUT114` (`cached-result-mutation`) reports an in-place change of an object that a
  `functools.lru_cache` / `functools.cache` function returned, which `MUT202` reported without
  configuration before. It runs by default, and a report is exact when the function changes in place
  the list, dict, or set that the cached function returned, the same object on every path: the
  decorator resolves through the imports to `functools` (a module's own `cache` decorator does not
  count), its `maxsize` is left out, `None`, or a literal number above `0`, the function takes a
  parameter besides `self` / `cls`, and every return a run can reach hands out that kind of
  container, by its return annotation or by construction (`html.escape(text).split("\n")` included).
  Its other reports (through wrappers or helpers, an object below the result, a class instance) are
  heuristic.
- `MUT113` (`iterated-container-mutation`, opt-in, experimental) reports a dict, set, or deque
  of `self` changed in place while a method iterates it: by another method of the class while a
  generator is suspended inside the iteration, or by a function the loop's body calls; the next
  step raises `RuntimeError`.
- SARIF output lists every file heki could not read or parse as an `error` notification of the
  run (`invocations[0].toolExecutionNotifications`, with its location), and then sets
  `executionSuccessful` to `false`; the results of the other files are kept. JSON output is
  unchanged: the errors stay on stderr, and the exit code is still `2`.
- `call-owners` takes `subclasses = false`: a class target then means constructing exactly the
  class, so raising a subclass of a guarded exception class is no `MUT204` report. `Class.method`
  targets keep covering overrides.
- Contract names take a module part, `module:qualname`, which names definitions of one module
  without its submodules: `pkg:*` names what `pkg/__init__.py` defines, where `pkg.*` also covers
  `pkg.sub.user.sub_user`. The module part matches the whole module name, exactly or as a glob
  (`pkg.*:run`), and works in every key that names project functions or classes, the class of
  `Class.attr` targets included (`pkg.model:DataModel.fields`).
- Effect boundaries check what a scope reads of the environment it runs in. `call-targets` name
  functions and classes outside the project by their full dotted name (`os.getenv`, `platform.*`),
  and `read-targets` module attributes (`sys.platform`, `os.environ`, also read through
  `from sys import platform`); a dotted suffix or a leading glob still names project functions only.
  The `forbid` token `reads-environment` forbids the calls and reads the ecosystem catalog tags with
  the new `environment` key (`os.getenv`, `platform.system`, `importlib.util.find_spec`,
  `importlib.metadata.version`, `os.environ`, `sys.version_info`, ...), in one report per scope
  function, and with `import-probes = "report"` the imports inside a `try` whose `except` clause
  names `ImportError` or `ModuleNotFoundError`.
- `call-owners` and effect boundaries (for `calls`) take `references = "calls"`: a reference handing
  a target on to be called later counts as a call of it, reported as "`f` hands on
  `pkg.core.decide`": passed to a call (`partial(decide)`, `field(default_factory=Rule)`), stored,
  returned, yielded, raised, or held by such a display, also through an import or a module alias.
  Uses that only name a class (`isinstance`, `except`, a base class, `cast`, a typing subscript, an
  annotation, a comparison) are no reference.

### Changed

- `DATA101` marks a report exact when every premise is established (the update returns on every
  iteration that completes, every path around the back edge carries it, the loop steps through a
  parameter, the rows of a query, the lines of a file, or directory entries, and its trip count is
  not fixed) and heuristic otherwise; its message names the loop's origin and drops "(quadratic
  copying)".
- `EFF101` and `EFF103` state exact claims, each premise established: `EFF101` that whenever the
  reported call returns, a call that waits for the network, a database, a child process, or a sleep
  has run on the event loop's thread, through functions that each make the next call on every path
  on which they return (exception edges included), with every implementation of each call followed
  (protocol receivers are not); `EFF103` that every iteration of a loop over a parameter, the rows
  of a query, the lines of a file, or the entries of a directory sends a database statement. Their
  messages state the claim and its witnesses only: a call whose implementations reach different
  statements names each with the function that makes it, and relates each where it is, and "N+1" and
  "batch the queries" are gone. Everything else they reported is `EFF111` / `EFF113`. Waits on
  another thread (`Thread.join()`, `Event.wait()`, locks, `Queue.get()`, `put()` on a bounded queue,
  `Future.result()`) are `EFF111` only. Both are certified exact, as is `DATA101`
  (`docs/precision.md` has their census and holdout counts), and run by default.
- Only exact reports are shown by default. A report is *exact* when every premise of its claim was
  established and its rule was certified (a false exact report is a bug), and *heuristic* when a
  premise can be open, so that it can be wrong about the code. A heuristic rule runs when `select`
  or `extend-select` names its code or name, and with the new `[tool.heki.lint] heuristic = true`
  (`--heuristic`) also by default and for every prefix and `ALL`, as Ruff's `preview` does. An exact
  rule's heuristic reports (an `OPT` report whose rewrite is not proven) are shown under the same
  conditions. A contract rule runs when its contract is declared, whatever its certainty. The proven
  tier of the eight `OPT` rewrite rules (`OPT101`-`OPT103`, `OPT110`, `OPT111`, `OPT120`, `OPT121`,
  `OPT130`) is exact: a census of every one of its 234 reports on the Stage 0-A corpus, and a check
  of the 235 of one run on a frozen holdout corpus of 100 packages, found none wrong. `OPT113`'s
  exact tier is certified too (8 reports in the census and 6 in the holdout run, none wrong) and
  runs by default. `EFF101`, `EFF103`, `DATA101`, `MUT110`, and `MUT114` are exact too
  (`docs/precision.md` has their census and holdout counts, and lists their certifications).
  `heki check` with no configuration runs the exact rules (`EFF101`, `EFF103`, `DATA101`, `MUT110`,
  `MUT114`, and the proven tier of the `OPT` rules above) and the contract rules and shows the
  reports whose rewrite is proven safe, and `heki check --fix` applies the proven fixes of `OPT101`
  and `OPT102`. `lint.heuristic = true` restores the rules of 0.1; baselines keep their keys, except
  the reports that move to `MUT114` (below).
- `MUT202` (`readonly-return-mutation`) is a contract rule: it reports the declared
  `readonly-returns` producers, and runs when such a contract is declared. Changes of `@lru_cache` /
  `@cache` results move to `MUT114` (`cached-result-mutation`): regenerate a baseline that holds
  them, or add `MUT114` to `ignore`.
- Every output labels heuristic reports: `[error, heuristic]` in `full`,
  `EFF102 [heuristic] ...` in `concise`, `title=heki EFF102 (heuristic)` in `github`, and
  ` [heuristic]` after the language server's message. `heki rule --all` lists each rule's
  certainty and when it runs (`on`, `off`, `opt-in`, or `contract`) instead of
  `default=true` / `false`.
- A rule's default severity follows what happens in the code it reports, not its family: a bug (a
  wrong result, an exception, a lost update, a deadlock, or a broken contract) is an `error`, slower
  code a `warning`. `EFF101`, `EFF102`, `EFF103`, `EFF105`, `DATA101`, and the `OPT` rules default
  to `warning` instead of `error`; the other rules stay `error`. This changes the output label, the
  SARIF level, the language server's severity, and the exit code (see Breaking changes: a warning no
  longer fails the run). `[tool.heki.lint.severity]` still overrides it. Each rule page shows its
  default severity, and `heki rule --output-format json` gives each rule's `consequence` (`bug`,
  `minor-bug`, or `slowdown`). A bug that is rarely harmful is a `warning` too: `MUT110`
  (`readonly-param-mutation`), a broken read-only annotation.
- `MUT110` names the line where the helper changes the parameter, in the message and as a related
  location: "`median` passes its read-only parameter `values` to `helpers.sort_values`; whenever
  line 2 of `sort_values` runs, it changes the caller's object in place". It no longer says that
  every call changes it. `MUT110` is exact and runs by default when the line changes a builtin
  container (`list`, `dict`, `set`, ... by annotation, construction, or an `isinstance` test); a
  change of a value heki does not know as one (a helper that types nothing), or one heki finds only
  where a generator another function returns is iterated, names no line, and that report is
  heuristic.
- `OPT` messages state their claim and nothing else: what the statement does each time it runs in
  the loop (`` `rows = rows + [...]` copies the whole list each time it runs in the loop ``),
  without "on every iteration", which heki does not establish, and without cost estimates
  ("(quadratic)", "(O(n) per iteration)", "(O(n²))", "(O(n·m))"). `OPT110` names the container it
  scans (a list or a tuple), and `OPT130` says the constructor builds a new container for the
  membership test, not "the same container", which only a proven rewrite establishes. The rule pages
  state the claim the same way: its premises are the container's builtin type, the loop, and, for a
  rule's exact tier, the proven rewrite; no size is claimed. A proven report of a size-dependent
  `OPT` rule in a loop whose trip count the code fixes or caps (`range(50)`, `rows[:100]`, a display
  of 20 items, a length test such as `if len(rows) > 100: raise ...`) is heuristic: true, but no
  evidence either way.
- `OPT113` reports carry a tier. A report is in the exact tier when each key the inner loop loads is
  a plain attribute of an item whose class heki knows (a dataclass, `attrs`, or `NamedTuple` field,
  or what the class stores, with no property or other descriptor of that name in the project) or an
  item of a builtin container, both loops step through the function's inputs (parameters, or
  attributes or items of them), an inner item whose key differs runs nothing but the comparison (no
  statement before the test or after the `if`, no earlier `and` operand, and an inner iterable that
  is a builtin container by its type or construction, not a value of unknown type, a generator,
  `map`, `filter`, or another call), and the inner loop steps through the same collection, with the
  same keys, on every outer iteration; a loop whose trip count the code fixes or caps has no exact
  report. Its message states that claim ("for each item of `orders`, the loop over `customers`
  compares `order.customer_id == customer.id` and does nothing else for an item that does not
  match"); the other reports are heuristic. The help names a `set` or `dict` only for keys known to
  be hashable, and an index built before the outer loop only for an inner collection that stays the
  same; otherwise it states the condition ("if the keys are hashable, ..."). For a mapping the inner
  loop steps through by its own keys, it says to look each outer key up in the mapping.
- `OPT103`'s help for a `tuple` accumulator says to extend a list and convert it once with
  `tuple(...)`, not to join the pieces.
- `EFF102` and `EFF113` also report the function a builtin calls once per item: the function
  handed to `map`, `filter`, `itertools.starmap`, `itertools.filterfalse`, and
  `functools.reduce`, and the `key` of `sorted`, `list.sort`, `min`, `max`, and
  `itertools.groupby` (`sorted(users, key=lambda u: score(conn, u))`), when the items are not a
  fixed number and heki can tell which function it is. Its body runs at the call for every rule,
  so `EFF111` sees its blocking calls too. A `functools.partial` handed to such a builtin is
  followed to the function it calls.
- `MUT107` (`hash-attr-mutation`) is opt-in: it runs only when `select` or `extend-select` names it,
  also with `lint.heuristic = true`. Its message states the claim only: "`Tag.rename` rebinds
  `self.name` outside initialization, and `Tag.__hash__` reads it".

### Fixed

- An f-string's replacement fields are reads of their values: each is converted and formatted
  where it stands, as CPython does (which may run the value's `__str__`, `__repr__`, or
  `__format__`), also in a format specification and in a template string's specifications,
  and the f-string is computed from them. The Soundness lane no longer proves a
  `collections.deque` rewrite safe for a list an f-string shows (`OPT120` / `OPT121`: the
  text of a `deque` differs), and `OPT113` no longer reports a traversal whose inner collection
  is computed from the outer element through an f-string (`index[f"{name}"]`).
- A project module that cannot be read or does not parse still owns its module name: the names
  under it resolve to nothing, never to a module of the same name the catalog lists. A broken
  `requests.py` in the project no longer makes `requests.get` a blocking call. The language
  server and `heki check --watch` keep such a file as the same unknown module while it does not
  parse.
- `heki check --fix` no longer rewrites `s = s | t` to `s |= t` (`OPT102`) where the set's
  iteration order can be seen afterwards. Growing a set in place can order the same elements
  differently from copying it, so a function that iterated, converted, formatted, or returned the
  set could return another result after the fix. The set form is fixed only when the set is just
  tested afterwards (`in`, `len`, truthiness, `==`); its other reports say
  `[not auto-fixable: SetOrderUnobserved(...)]`. The dict forms keep their insertion order and
  are fixed as before.
- `EFF102` and `EFF103` no longer count a `functools.cached_property` getter as running on
  every read: it runs once per instance, like a lazy initialization of `self`, also when
  `cached_property` comes from a module that re-exports it (`kombu.utils.cached_property`). A
  loop reading it on an instance the loop does not construct is not reported.
- `EFF102` and `EFF103` no longer report a client or query created only while a local variable
  the loop carries is unset (`if conn is None: conn = connect()`), only while a first-iteration
  flag holds (`if first: ...; first = False`), or only while a module global is unset
  (`global _session`).
- `EFF102` and `EFF103` no longer report a client or query that every way on from leaves the
  loop (followed by `return`, `break`, or `raise`): it runs once per run of the loop.
- `EFF101` no longer reports `result()` or `exception()` of a `concurrent.futures.Future` known
  to be done: under `if f.done():`, or once `await asyncio.wrap_future(f)` has returned.
- `EFF111` no longer reports `put()` on a `queue.Queue`, `LifoQueue`, or `PriorityQueue` that is not
  known to be bounded: built without a positive `maxsize`, or one heki cannot follow to where it is
  built. A `put` waits only while its queue is full. heki follows the queue into local variables,
  the attributes its class family stores, module globals, and parameters whose callers bind known
  queues; an `effect-boundaries` contract forbidding `blocking-sync` counts a `put` the same way.
- `EFF107` (`unretained-async-task`, opt-in) no longer reports a task whose bound method is
  passed to a call or stored, as in `self.addCleanup(task.cancel)` or
  `loop.call_later(1, task.cancel)`: the bound method holds the task.
- `EFF107` no longer reports a call that makes no new task because its argument is known not
  to be a coroutine: the result of a plain `def` or a generator function, such as a method
  returning a future (`asyncio.ensure_future(event.wait())`), or a future from
  `loop.run_in_executor`, `loop.create_future`, `asyncio.gather`, `asyncio.wrap_future`, or an
  annotation. The `coro=` and `coro_or_future=` keyword forms are checked the same way.
- Calling an `async def` gives a coroutine: a method called on the result without `await` no
  longer resolves on the function's return annotation, so the rules stop following it there.
  `await` and `async with` still give the annotated type, and awaiting a future gives what it
  holds, not the future.
- A value typed with a project `Protocol` and handed to a helper that does not type its
  parameter is an instance of the protocol and of its implementations: the helper's writes are
  `MUT201` writes of those classes, and `MUT205` reports a write to a protocol whose
  implementations heki cannot list. `MUT109` matches such a write against the targets of each
  class once.
- A call target naming no class (`reset`) gives `MUT204` the initialization of the classes
  defining the methods it names: `self.reset()` in their `__init__` is no report.
- `MUT205` uses the class an `isinstance` test around a write narrows its object to.
- A list or set comprehension that collects the results of a call no longer binds each result to
  the call's first parameter: `[load(graph, raw) for raw in items]`, with `load` returning a
  `readonly-returns` value, is no `MUT202` report.
- A `return` no run reaches (after a `while True:` loop that no `break` leaves, or under
  `if False:`) is not what the function returns: a wrapper returning a cached or
  `readonly-returns` value only there is no producer, and changing its result is no `MUT202`
  report.
- `MUT112` no longer counts an attribute of an object the function makes itself
  (`visitor = Visitor()`, then `visitor.errors`) as borrowed state, nor a value that a flag
  parameter copies (`if not in_place: x = x.copy()`).
- `MUT113` no longer counts the changes a function makes to a container it has just bound to the
  attribute (`self.xref = {}`, then `self.xref[k] = v`), nor a store or a `defaultdict` read under
  a key the container holds (past `if self.items.get(k) is None: raise KeyError(k)`, inside
  `if k in self.items:` at any depth).
- `MUT107` reads the `__hash__` of the writer's instances: each `self` method and property the hash
  calls is the one the writer's class finds along its MRO, not an override in another subclass
  (`Expression.__hash__` reading `self.identity` reads `self.lhs` only on the instances of the
  subclass whose `identity` reads it).
- `MUT101` and `MUT107` no longer report a write that is no plain store: to an attribute that a
  class along the MRO binds to a property (`@property`, `@x.setter`, `@x.deleter`,
  `x = property(...)`; a raising deleter changes nothing), on a class whose MRO defines
  `__setattr__` or `__delattr__`, and on a class with a base heki names no class for
  (`class Proxy(with_metaclass(Meta))`, `Base = declarative_base()`), whose metaclass may install
  descriptors.
- `MUT107` (`hash-attr-mutation`) no longer reports a write that cannot change a hash a `set` or
  `dict` has filed: an in-place change (an item store, `append`) of an attribute `__hash__`
  hands whole to `hash()` (`hash(self.items)`: a list or `bytearray` there cannot be hashed, and
  a project value answers for its own hash), a writer whose class defines its own `__hash__`
  (or `__eq__` alone) below the hash's class, and a private helper that only `__init__` and
  other such helpers call (`__init__` -> `self._load()` -> `self._decode()`). A writer in a
  subclass with its own `__hash__` is now reported against that hash.
- `MUT202` (`readonly-return-mutation`) no longer reports a shared object filling its own memo
  (a cache-named attribute changed in place, a store under `if k not in ...`, `setdefault`, a
  lazy initialization), also in a method or helper it is handed to, nor a change of a copy a
  project class's own `copy()` makes (it copies as deeply as its body does).
- `MUT202` reports a change below a shallow copy made with `dict(...)`, `list(...)`,
  `tuple(...)`, `set(...)`, `frozenset(...)`, or a display that only unpacks the shared value
  (`{**shared}`, `[*rows]`), as for `.copy()`; they shared the values below and were taken as new
  objects. A helper changing an attribute's object below its own shallow copy of a parameter
  (`draft = copy.copy(post); draft.tags.append(t)`) now changes that parameter (`MUT202`,
  `MUT108`, `EFF201` `mutates-params`).
- `DATA101` no longer reports a value rebuilt from arithmetic on the previous one, such as a
  recursion that computes `x = np.concatenate((x - np.outer(d, a), [a]))` or a fixed-size
  quaternion product: every entry of the new array changed, so there are no unchanged parts to
  collect and combine once. A list that holds the previous value (`pd.concat([df] + parts)`)
  is still reported.
- A helper taking a path or a buffer is no longer taken to open the file (or fetch the URL) when
  its caller hands it a buffer or `None` (`EFF101`, `EFF102`, `EFF103`, `EFF105`, `EFF106`,
  `DATA101`): `hasattr(buf, "write")` and `isinstance` with the `io` classes are guards too, an
  `io.StringIO` / `io.BytesIO` the caller constructs, a display, or a comprehension decides
  them, and a test of a parameter the helper rebinds on some paths (`if buf is None: buf =
  StringIO()`) is decided per path. `EFF201` still reads no guards.
- Sizes the code states are no longer taken as data-sized (`EFF102`, `EFF103`, `DATA101`, and
  the `OPT` rules): a builtin constructor of a fixed iterable (`list(range(7))`,
  `tuple(".;:")`), a string constant, a comprehension over fixed iterables or a new list filled
  a fixed number of times (`[c for c in LABELS if c in seen]`, `ids.append(...)` for each of a
  few cards), a slice with constant bounds (`cases[:3]`), and an iterable a length test on every
  path to the loop caps (`if not 1 <= len(cards) <= 6: raise ...`, `assert len(xs) <= 3`). A
  module body's loops and comprehensions over imported constants, and generator expressions
  and comprehensions before 3.12 over constants of other modules, get the same sizes as the
  code of a function.
- A module-level tuple or string stays fixed when other code puts it in a list or passes it to
  a function (an `EFF103` report on a loop of `CREATE TABLE` statements).
- `DATA101` no longer reports an accumulation in a loop whose trip count the code fixes
  (`for is_p_wave in [False, True]`, `range(4)`).
- Polls and retries are no longer taken for loops over data (`EFF102`, `EFF103`, and the
  size-dependent rules): a loop that sleeps or waits with a timeout on every path back to its
  test (`while not job_done(): time.sleep(5)`, `for _ in range(30): ... sleep(1)`), a `while`
  loop over a counter the code bounds (`i = 1` ... `while i < 6:` ... `i += 1`), a loop until a
  stop event is set (`while not stop_event.is_set():`), and a retry over a tuple target naming
  the attempt (`for attempt, proxy in enumerate(proxies):`).
- `EFF103` no longer reports DDL built at run time or taken from constants: an f-string, `%`,
  or `.format()` starting with `CREATE` / `ALTER` / `DROP` / `PRAGMA`, SQLAlchemy's `text()` of
  one, and the statements of a module's schema constants (a tuple of statements, the pieces of
  `SCHEMA.split(";")` or `SCHEMA.splitlines()`).
- `EFF101` no longer reports `socket.getaddrinfo` with the `AI_NUMERICHOST` flag, which parses
  a numeric address and never asks a resolver.
- A file that is one definition, class, lambda, or generator expression with nothing after it
  (no final newline) is checked: its function was taken for the module body, which has the same
  span, so it was never analyzed, and `heki check` panicked on such a generator expression
  (`(x async for x in xs)`).
- `heki check` no longer overflows its stack on a loop no path enters (after a `return`,
  `raise`, or `break`) that computes a value from itself (`d = d.x`, `d = d[mask]`): the value
  was defined by itself, and the walks along definitions went around it forever.
- A module-level name bound once to another object by name is that object, in its module and
  imported from it, unless another module rebinds it: `_set = object.__setattr__` makes
  `_set(node, "kind", v)` a `MUT201` write, and `sleep = time.sleep` makes `sleep(1)` in an
  `async def` an `EFF101` report. Any other global a module binds is never the builtin of the same
  name: after `open = functools.partial(...)`, `open(path)` is no blocking `builtins.open`
  (`EFF105`), and a call of it is unresolved unless a `[[tool.heki.ecosystem]]` entry declares it. A
  name heki does not see the module bind (a compiled submodule, a star import of one) keeps what the
  catalog says of it (`mypkg._native.read`).
- A method that no project class of the receiver's MRO defines is no longer taken from a base
  outside the project that is not known to define it (`abc.ABC._copy_model_type`,
  `typing.Generic.result_type`): the call is unresolved, as a call heki cannot follow. Special
  methods, public methods of builtin types, and the members the catalog lists still come from the
  base, when no base heki knows nothing of comes before it.
- `getattr(obj, "attr"[, default])` with a constant name is a load of the attribute for the write
  contracts, also behind a walrus
  (`if (extras := getattr(self, "extras", None)) is not None: extras["k"] = 2` is a `MUT201` write
  of `Node.extras["k"]`).
- A method, property, or function that returns an attribute of an argument on every `return`
  (`return self.items`) hands out that object: `node.members().append(1)` and `node.view.append(1)`
  are `MUT201` writes of `Node.items`. It does so on a class whose reads run it: not through a
  `cached_property` (an assignment can replace what the instance keeps), not on a member of a union,
  or a class, whose own attribute of the name comes first or is stored on the class outside its
  body, and not for a function that stores the attribute itself
  (`items = self.items; self.items = []; return items`) or a keyword-only parameter.
- `object.__new__(Node)`, also through an alias, and `Node.__new__(Node)` where no class of the MRO
  defines `__new__` construct a `Node`: the new object has its class, and with
  `fresh-objects = "allow"` a helper filling it is no `MUT201` report.
- A glob in the last segment of a `call-owners` target or an effect boundary's `call-targets`
  matches within the function, method, or class name, as the attribute of a `Class.attr` target
  does: `pkg.*._*` no longer names every function of a private module (`pkg.parser._concern.typed`),
  nor `pkg.api_*` the functions of `pkg.api_helpers`, so such functions are no targets that may call
  the others, and their calls are reported.
- A report of a glob target names the attribute, function, or class it matched
  (`reads `pkg.types.DataType._flag``, was `reads `pkg.*._*``), and one report standing for several
  members lists the others; the diagnostic target, and so the baseline key, stays the configured
  text.
- `call-owners` contracts (`MUT204`) and the `calls` of effect boundaries look a call's targets up
  from the callee's names instead of trying every target, and `MUT204` indexes the targets of all
  its contracts together: a contract of 1,300 targets on a 200,000-line project took about 30 s and
  now takes about 1 s, with the same reports.
- A read or a call through a value typed with a project `Protocol` that declares the member is
  judged by the protocol, the type the code uses it through: `def f(h: Host): h._m` is no `MUT203`
  read of `_m` of the classes implementing `Host`, `h._helper()` no `MUT204` call of theirs, and
  what an implementation's private method reads or calls inside stays with it instead of being
  handed to the caller (also when other callers call it through the class and take it over). A
  member the protocol does not declare is still one of the implementations', and a write through the
  protocol still writes their attribute.

## [0.1.2] - 2026-11-21

### Fixed

- The source distribution carries the pages, changelog, and documentation generator its tests
  read, so `cargo test --workspace` passes in the unpacked archive (the 0.1.1 release stopped
  there before publishing to PyPI).

## [0.1.1] - 2026-11-21

### Changed

- `MUT111` and `heki check --profile` are experimental: `MUT111` reports the readers of the stale
  writes `MUT101` finds, so it shares `MUT101`'s precision, and `--profile` reads the output of
  CPython 3.15's sampling profiler, whose format can still change. The new
  [Experimental Features](https://heki.koxudaxi.dev/experimental/) page lists every experimental
  feature, and `heki rule` says when a rule is experimental.
- Small displays and loop bounds now go up to 16 items (8 before), so loops whose fixed trip
  count is 9 to 16 and containers of 9 to 16 items no longer get size-dependent reports
  (`OPT103`, `OPT110`, `OPT111`, `OPT112`, `OPT113`, `OPT120`, `OPT121`, `OPT130`, `EFF102`,
  `EFF103`).

### Fixed

- The errors of files that cannot be read or parsed (and of files that changed on disk during
  `--fix`) print the paths diagnostics print: relative to the working directory, without a
  leading `./` (`error: pkg/broken.py:1:7: syntax error: ...`, was `./pkg/broken.py`), also
  in `--watch`, `heki snapshot`, and `heki assumptions`. The hazards `heki assumptions` lists
  print those paths too, also for a directory given as an absolute path.
- On Windows, `json` output, errors, and `heki assumptions` write paths with `/` separators,
  as the other output formats do (`pkg/a.py`, was `pkg\a.py`), and the paths heki resolves
  (the project root and the files under it that messages name, such as a configured baseline,
  and the language server's paths in its log) no longer start with `\\?\`.
- On Windows, a checkout keeps the repository's LF line breaks (`.gitattributes`), so a heki
  built there prints the rule documentation (`heki rule`) as written, without CR characters.
- The language server, when the client names no workspace folder, indexes the working directory
  in the form it compares document paths in; on Windows a document could otherwise be missed
  as part of the project.
- `OPT113` (`nested-loop-equijoin`) no longer reports a join when the code fixes the size of
  either loop, such as a module tuple of operation names or `range(10)`: the cost then grows
  with the other side only. Its help for the grouped index now says to read each outer key with
  `.get(key, [])`, so that a key with no element keeps its empty list.

## [0.1.0] - 2026-11-21

The first public preview.

### Added

- `heki check` checks a project, or the files and directories given, with Ruff-style rule
  selection (`select`, `extend-select`, `ignore`, `per-file-ignores`), suppression comments
  (`# heki: ignore[CODE]`, and `# noqa: CODE` for compatibility), output formats (`full`,
  `concise`, `json`, `github`, `sarif`; paths relative to the working directory, as in Ruff),
  and exit codes `0` / `1` / `2`. The target Python version (`py311` to `py315`) comes from
  `requires-python` or `--target-version`, and `--watch` re-checks on every change.
- 31 rules in four families:
  - Mutation and state: `MUT107`, `MUT109`, `MUT110`, `MUT201`, `MUT202`, `MUT203`, `MUT204`,
    `MUT205`, and the opt-in `MUT103`, `MUT106`, `MUT108`, and `MUT111`, and `MUT101`
    (experimental: its precision outside a few code bases is still low, and it may change).
  - Execution effects: `EFF101`, `EFF102`, `EFF103`, `EFF201`, and the opt-in `EFF105`,
    `EFF106`, and `EFF107`.
  - Data pipelines: `DATA101`.
  - Complexity rewrites: `OPT101`, `OPT102`, `OPT103`, `OPT110`, `OPT111`, `OPT112`, `OPT113`,
    `OPT120`, `OPT121`, and `OPT130`.
- `MUT111` (`cached-property-stale-read`, opt-in) reports each function that reads a
  `@cached_property` that a method can leave stale (a `MUT101` finding), with the writers as
  related locations; under a Ratchet baseline, a new reader of a stale value is one new
  diagnostic.
- Proven-safe rewrites: `heki check --fix` applies the unconditional `OPT101` / `OPT102` fixes,
  `--include-size-dependent` adds the size-dependent ones in every function, `--profile` alone
  applies them in the hot functions of a sampling profile (CPython 3.15's profiler or py-spy),
  and `--diff` previews them. A fix that turns a list into a `collections.deque` also rewrites its `list` annotation
  and adds the import in sorted order to the standard-library imports.
- Contracts in `[tool.heki]`: `write-owners` (`MUT201`), `read-owners` (`MUT203`),
  `call-owners` (`MUT204`), `readonly-returns` (`MUT202`, followed through the functions that
  return a producer's result), `invalidation` (`MUT109`), and `effect-boundaries` (`EFF201`, with
  `reads-targets`, `calls`, and `allow-writes` / `write-universe`); `forbidden-writers` /
  `forbidden-readers` / `forbidden-callers` restrict an owner contract to a layer. Targets can
  name container entries (`Class.attr["key"]`) and use module-level `Final` string constants as
  keys; `fresh-objects = "allow" | "report"` decides whether a write that constructs its object
  counts (allowed by default), and `unresolved = "report"` makes a `write-owners` contract count
  the writes heki cannot tie to a class (`MUT205`).
- A catalog of library behavior (blocking network, database, and file calls, their async
  counterparts, offloading helpers, locks and transactions) for the standard library, requests,
  httpx, urllib3, aiohttp, SQLAlchemy, SQLModel, psycopg, psycopg2, PyMySQL, Django, redis,
  pymongo, boto3, pandas, numpy, the offloading helpers of anyio, trio, Starlette, and FastAPI,
  and asgiref (`sync_to_async`), asyncer (`asyncify`), and Django Channels
  (`database_sync_to_async`), also followed as decorators; extensible with
  `[[tool.heki.ecosystem]]`.
- Generator functions are followed where their generators are iterated (`for`, comprehensions,
  unpacking, `yield from`, consuming calls, lazy wrappers, project functions iterating them),
  generators returned by helpers included; a generator handed to a framework
  (`StreamingResponse(rows())`) is not reported.
- Receiver typing for the call graph: container item types (`list[Model]`), return annotations
  along the MRO, `Self`, copies, `functools.partial`, `typing.cast`, unions of up to three
  classes, `isinstance` narrowing, and the structural implementations of project `Protocol`s.
- Effect guards: a branch the caller's arguments switch off is not followed
  (`load(name, cache=False)`), and a client created by a lazy initialization counts once per
  object, not once per loop iteration.
- Ratchet baselines (`--baseline`, `--update-baseline`), keyed by file, function, rule, and
  target instead of line numbers, that count violation sites (`version = 2`); older files keep
  counting reports until `--update-baseline`.
- Every diagnostic is one sentence saying what happens, and what to do about it is a separate
  help text: a `= help:` line in the `full` output, a `help` field in JSON (`null` when a rule
  has no advice for the report), `properties.help` in SARIF, and the last related-information
  entry in the language server; `concise` and `github` print the message only. A contract's
  `message` is the help of its reports.
- `heki snapshot` saves the effect summaries of public functions for `MUT103`.
- `heki server` (alias `heki lsp`), an incremental language server: call chains as related
  information, quick fixes for the proven-safe rewrites, and suppression quick fixes.
- `heki rule` (alias `heki explain`) explains a rule; `heki assumptions` lists the functions the
  rewrite proofs cannot see through (`eval`, `exec`, `locals()`, frame introspection, and
  functions too large to model).
- A size budget per function bounds heki's memory on any input: a function whose model would
  pass it is left unmodeled (no rewrite in it, its body one call heki cannot resolve), and a
  function whose rewrite proof would be too large gets no `OPT` finding.
- Wheels for Linux (x86_64 and aarch64 with glibc, x86_64 with musl), macOS (arm64 and x86_64),
  and Windows (x86_64), and a source distribution.

[0.2.2]: https://github.com/koxudaxi/heki/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/koxudaxi/heki/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/koxudaxi/heki/compare/v0.1.2...v0.2.0
[0.1.2]: https://github.com/koxudaxi/heki/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/koxudaxi/heki/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/koxudaxi/heki/releases/tag/v0.1.0
