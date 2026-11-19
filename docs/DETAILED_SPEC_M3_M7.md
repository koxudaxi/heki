# `heki` M3〜M7 詳細実装仕様書（Implementation Blueprint）

> **本書の位置づけ**
> - **M1（`heki_semantic`）** および **M2（`heki_ir` + `heki assumptions`）** は実装・検証済みである（CPython 3.11〜3.15 × `dcg` 422 ファイルで `symtable` 不一致 0 件、`heki_ir` で `dcg` 421 ファイル / 15,398〜17,390 関数を 0.18〜0.45 秒・パニック 0 件で構築実証済み）。
> - 本書は、別エージェントが **M3 → M4 → M5 → M6 → M7** を仕様解釈の揺れやアーキテクチャ違反なしに実装するための **確定アルゴリズム・API 契約・エッジケース・受け入れゲート仕様書** である。
> - 絶対に破ってはならない不変条件は [AGENTS.md](../AGENTS.md) および [DESIGN_HISTORY_AND_ADR.md](DESIGN_HISTORY_AND_ADR.md) を正本とする。

---

## 0. 確定済み下位層 API 契約（M1 / M2 実装済み）

後段クレートは以下の確定済み型・関数をそのまま消費する。**下位層の公開型を無断で破壊しないこと**（必要な追加フィールドのみ後方互換で拡張可）。

- `heki_ast`: `parse_module(&str) -> Result<ParsedModule, Vec<ParseError>>`, `Span { start, end }`, `LineIndex::from_source(&str)`。**`ruff_python_*` への直接依存は `heki_ast` のみ。**
- `heki_env`: `PythonVersion` (`PY311`..`PY315`), `RuntimeProfile`, `HekiConfig::load_from_dir(&Path)`, `WriteOwnerRule`, `ReadOnlyReturnRule`, `Severity`.
- `heki_semantic`: `analyze_module(&ParsedModule, &str, RuntimeProfile) -> SemanticModel`（`scopes`, `imports: Vec<ModuleImport>`, `classes: Vec<ClassInfo>` with `parent_class`, `bases`, `methods`）。
- `heki_ir`: `build_module_ir(&ParsedModule, &str, &SemanticModel, RuntimeProfile) -> ModuleIr`
  - `IrFunction` は以下をすでに提供済み：
    - `owner_class: Option<ClassId>`, `owner_class_qualname: Option<String>`（ネストクラス厳密分離済み）
    - `params: Vec<ParamInfo>`（`var: VarId`, `annotation: Option<String>` ※Advisory 専用）
    - `return_annotation: Option<String>`, `local_annotations: Vec<LocalAnnotation>`（※Advisory 専用）
    - `decorators: Vec<String>`, `has_decorator(&[&str])`
    - `captured_vars: Vec<String>`（子関数/lambda/genexpr に捕捉された自由変数）
    - `loops: Vec<NaturalLoop>`（**`for.iter` と `MakeIter` は必ず `preheader` に配置され、`loop.body` に含まれない**）
    - `loop_carried: Vec<LoopCarriedInfo>`（`phi_var`, `var_name`, `init_vars`, `back_vars`, **`self_accumulating: bool`**）
    - `a1_hazards: Vec<A1Hazard>`
    - `rpo_blocks() -> Vec<BlockId>`, `predecessors() -> Vec<Vec<BlockId>>`, `block_loop_depth(BlockId)`, `innermost_loop(BlockId)`, `def_inst(VarId)`, `const_str(VarId)`, `const_int(VarId)`, `const_bool(VarId)`
  - `InstKind` 主要命令：
    - `Const { dst, literal }` / `AllocBuiltin { dst, kind, elems }` / **`DictLiteral { dst, entries: Vec<DictEntry> }`**（`DictEntry::Unpack` の順序保存済み）
    - `BinOp { dst, op, lhs, rhs, in_place }`（`AugAssign` は `in_place: true`）
    - `SubscriptLoad` / `SubscriptStore` / `SubscriptDelete`（`key_const: Option<String>`, `is_slice`, `slice_lower_const`, `slice_upper_const`）
    - `Call` / `MethodCall`（`args`, `kwargs`, **`is_awaited: bool`**）
    - `MakeIter`, `IterNext`, `MakeGenExpr { outer_iter, captured }`, `MakeClosure { captured }`

---

## 1. Milestone M3: `heki_kb`（Layer 1 / Layer 2 ローダー＆番兵検証）

### 1.1 対象ファイル
- `crates/heki_env/src/lib.rs`（`[tool.heki.ecosystem]` 設定型追加）
- `crates/heki_kb/src/lib.rs`（組み込み TOML ローダー・マージ・検証 API）
- `kb/soundness/builtins.toml`（Layer 1 拡充）
- `kb/ecosystem/stdlib.toml`, `kb/ecosystem/web_db_data.toml`（Layer 2 拡充）
- `tests/sentinel/verify_soundness_kb.py`（CPython 3.11〜3.15 実機番兵テスト）

### 1.2 公開 API 契約

#### `heki_env` 追加設定（kebab-case）
```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct EcosystemOverride {
    pub symbol: String,
    pub effects: Vec<String>,
    pub async_safe: bool,
    pub offloads_blocking_arg: Option<u8>,
    pub excluded: bool,
    pub copy_sanitizer: bool,
}
// `HekiConfig` に以下を追加:
// pub ecosystem: Vec<EcosystemOverride>,
```

#### `heki_kb` 公開関数
```rust
#[derive(Debug, Error)]
pub enum KbError {
    #[error("failed to parse embedded KB TOML `{file}`: {source}")]
    Toml { file: &'static str, #[source] source: toml::de::Error },
    #[error("invalid effect kind `{0}` in user ecosystem override")]
    InvalidEffect(String),
}

impl EffectiveSoundnessKb {
    /// Embed `kb/soundness/*.toml` via `include_str!` and project for `target_python`.
    /// Entries with `verified_max_python < target_python` MUST be dropped (downgraded to Unknown).
    pub fn load_builtin(target_python: PythonVersion) -> Result<Self, KbError>;
    pub fn lookup(&self, symbol: &str) -> Option<&SoundnessEntry>;
}

impl EffectiveEcosystemKb {
    /// Embed `kb/ecosystem/*.toml` via `include_str!` and merge user `overrides`.
    /// User overrides take precedence over built-in entries for the same `symbol`.
    pub fn load_builtin(overrides: &[EcosystemEntry]) -> Result<Self, KbError>;
    pub fn lookup(&self, symbol: &str) -> Option<&EcosystemEntry>;
    pub fn is_opaque_to_layer3(&self, top_level_package: &str) -> bool;
}
```

### 1.3 Layer 1（`kb/soundness/builtins.toml`）必須エントリ
**不変条件（ADR-005）**：`kb/soundness/*.toml` に `effects` / `async_safe` 等の Advisory フィールドを絶対に書かない（デシリアライズ時に `#[serde(deny_unknown_fields)]` を付けて物理強制せよ！）。

必須エントリ（すべて `verified_max_python = { major = 3, minor = 15 }`）：
- 派生参照生成（`RetainedInReturn`）：
  `builtins.iter`, `builtins.enumerate`, `builtins.zip`, `builtins.map` (`[ReadOnly, RetainedInReturn]`), `builtins.filter` (`[ReadOnly, RetainedInReturn]`), `builtins.reversed`, `builtins.memoryview`, `builtins.dict.keys`, `builtins.dict.values`, `builtins.dict.items`
- 純粋読取（`ReadOnly`）＋ `return_exact_type`：
  `builtins.len` (`int`), `builtins.bool` (`bool`), `builtins.int` (`int`), `builtins.float` (`float`), `builtins.str` (None — **`__str__` が `StrEnum` 等の `str` サブクラスを返すとそのまま通過する CPython 3.11〜3.15 実測挙動のため意図的に `None`**), `builtins.bytes` (None — 同上), `builtins.range` (None)
- コンテナ新規生成（`return_exact_type` あり、`ReadOnly`）：
  `builtins.list` (`list`, `[ReadOnly]`), `builtins.set` (`set`, `[ReadOnly]`), `builtins.dict` (`dict`, `[ReadOnly]`), `builtins.tuple` (`tuple`, `[ReadOnly]`)
- ミューテータ（引数 0 = `self` は `Mutated`）：
  `builtins.list.append` (`[Mutated, Escaped]`), `builtins.list.extend` (`[Mutated, ReadOnly]`), `builtins.list.insert` (`[Mutated, ReadOnly, Escaped]`), `builtins.list.pop` (`[Mutated, ReadOnly]`), `builtins.list.clear` (`[Mutated]`), `builtins.list.reverse` (`[Mutated]`), `builtins.list.sort` (`[Mutated]`), `builtins.list.remove` (`[Mutated, ReadOnly]`),
  `builtins.set.add` (`[Mutated, Escaped]`), `builtins.set.update` (`[Mutated, ReadOnly]`), `builtins.set.discard` (`[Mutated, ReadOnly]`), `builtins.set.remove` (`[Mutated, ReadOnly]`), `builtins.set.clear` (`[Mutated]`),
  `builtins.dict.update` (`[Mutated, ReadOnly]`), `builtins.dict.pop` (`[Mutated, ReadOnly, ReadOnly]`), `builtins.dict.setdefault` (`[Mutated, Escaped, Escaped]`), `builtins.dict.clear` (`[Mutated]`),
  `builtins.list.copy` (`return_exact_type = "list"`, `[ReadOnly]`), `builtins.set.copy` (`return_exact_type = "set"`, `[ReadOnly]`), `builtins.dict.copy` (`return_exact_type = "dict"`, `[ReadOnly]`).

### 1.4 Layer 2（`kb/ecosystem/*.toml`）必須カタログ
`opaque_packages` には stdlib 主要モジュール＋全登録済み OSS パッケージを必ず列挙する（`asyncio`, `socket`, `ssl`, `subprocess`, `time`, `io`, `os`, `pathlib`, `shutil`, `sqlite3`, `urllib`, `http`, `logging`, `copy`, `functools`, `itertools`, `collections`, `threading`, `multiprocessing`, `concurrent`, `requests`, `httpx`, `aiohttp`, `urllib3`, `sqlalchemy`, `sqlmodel`, `django`, `psycopg`, `psycopg2`, `pymysql`, `redis`, `pymongo`, `boto3`, `botocore`, `grpc`, `pandas`, `numpy`, `pydantic`, `anyio`, `trio`, `starlette`, `fastapi`）。

必須エントリ群：
- **Sleep / Sync / Subprocess / FS**：
  - `time.sleep` (`[Sleep]`)
  - `subprocess.run`, `subprocess.call`, `subprocess.check_call`, `subprocess.check_output`, `subprocess.Popen.wait`, `subprocess.Popen.communicate` (`[BlockingSubprocess]`) — **`subprocess.Popen` 自体は登録しない（非ブロッキング spawn）！**
  - `builtins.open`, `io.open`, `pathlib.Path.read_text`, `pathlib.Path.read_bytes`, `pathlib.Path.write_text`, `pathlib.Path.write_bytes`, `pathlib.Path.open`, `os.listdir`, `os.scandir`, `os.walk`, `shutil.copy`, `shutil.copy2`, `shutil.copytree`, `shutil.rmtree` (`[BlockingFs]` — `EFF105` オプトイン用)
- **Async-Safe & OffloadsBlocking & Excluded**：
  - `asyncio.StreamWriter.write`, `asyncio.sleep`, `asyncio.gather`, `asyncio.wait_for` (`async_safe = true`)
  - `asyncio.to_thread`, `anyio.to_thread.run_sync`, `starlette.concurrency.run_in_threadpool`, `fastapi.concurrency.run_in_threadpool` (`offloads_blocking_arg = 0`, `async_safe = true`)
  - `asyncio.AbstractEventLoop.run_in_executor`, `asyncio.BaseEventLoop.run_in_executor` (`offloads_blocking_arg = 1`, `async_safe = true`)
  - `logging.Logger.debug/info/warning/error/critical/exception/log`, `logging.debug/info/warning/error` (`excluded = true`)
- **Copy Sanitizers（`MUT202` 用）**：
  - `copy.deepcopy`, `copy.copy`, `pydantic.BaseModel.model_copy`, `builtins.dict.copy`, `builtins.list.copy`, `builtins.set.copy` (`copy_sanitizer = true`)
- **HTTP / RPC (`BlockingNet` + `UnpooledClientAlloc`)**：
  - `requests.get/post/put/delete/patch/head/options/request` (`[BlockingNet, UnpooledClientAlloc]`)
  - `requests.Session` (`[UnpooledClientAlloc]`), `requests.Session.get/post/put/delete/patch/request` (`[BlockingNet]`)
  - `httpx.get/post/put/delete/patch/head/options/request` (`[BlockingNet, UnpooledClientAlloc]`)
  - `httpx.Client` (`[UnpooledClientAlloc]`), `httpx.Client.get/post/put/delete/patch/send/request` (`[BlockingNet]`)
  - `httpx.AsyncClient` (`[UnpooledClientAlloc]`), `httpx.AsyncClient.get/post/put/delete/patch/send/request` (`async_safe = true`)
  - `aiohttp.ClientSession` (`[UnpooledClientAlloc]`), `aiohttp.ClientSession.get/post/put/delete/patch/request` (`async_safe = true`)
  - `urllib.request.urlopen` (`[BlockingNet, UnpooledClientAlloc]`), `urllib3.PoolManager` (`[UnpooledClientAlloc]`)
  - `boto3.client`, `boto3.resource`, `boto3.Session` (`[UnpooledClientAlloc]`)
- **DB (`BlockingDb` + `DbQuery` + `UnpooledClientAlloc`)**：
  - `sqlite3.connect`, `psycopg.connect`, `psycopg2.connect`, `pymysql.connect`, `sqlalchemy.create_engine`, `redis.Redis`, `redis.StrictRedis`, `pymongo.MongoClient` (`[BlockingDb, UnpooledClientAlloc]`)
  - `sqlite3.Connection.execute/executemany`, `sqlite3.Cursor.execute/executemany/fetchall/fetchone`, `psycopg.Cursor.execute`, `psycopg2.extensions.cursor.execute`, `sqlalchemy.orm.Session.execute/query/scalars/scalar/get/commit/flush`, `sqlalchemy.engine.Connection.execute`, `sqlmodel.Session.exec/execute/get/commit`, `django.db.models.query.QuerySet.get/filter/all/first/exists/count/create/update/delete` (`[BlockingDb, DbQuery]`)
  - `sqlalchemy.ext.asyncio.AsyncSession.execute/scalars/scalar/get/commit` (`[DbQuery]`, `async_safe = true`)
  - `redis.Redis.get/set/hget/hset/delete/pipeline` (`[BlockingDb, DbQuery]`), `pymongo.collection.Collection.find/find_one/insert_one/update_one/delete_one` (`[BlockingDb, DbQuery]`)
- **Data (`QuadraticRealloc` — `DATA101` 用)**：
  - `pandas.concat`, `numpy.vstack`, `numpy.hstack`, `numpy.concatenate`, `numpy.append` (`[QuadraticRealloc]`)

### 1.5 `tests/sentinel/verify_soundness_kb.py` 仕様
- `kb/soundness/builtins.toml` を Python `tomllib` で読み込み、現在の CPython（3.11〜3.15）で以下を実測検証する：
  1. Advisory 禁止フィールド（`effects`, `async_safe`, `excluded` 等）が 0 件であること。
  2. `retained_in_return` 登録済み API（`iter`, `enumerate`, `zip`, `map`, `filter`, `reversed`, `dict.keys/values/items`, `memoryview`）が、実際に元コンテナの変更を観測する派生参照を返すこと（例：`x = [1, 2]; it = iter(x); x.append(3); assert list(it) == [1, 2, 3]`）。
  3. `return_exact_type` 登録済み API が `type(result) is expected_type` を満たすこと。

### 1.6 M3 完了ゲート
1. `cargo test -p heki_kb --offline` 全合格（`load_builtin` ロード成功、`verified_max_python` 降格、`#[serde(deny_unknown_fields)]` による Advisory フィールド拒否、ユーザーオーバーライド優先、`is_opaque_to_layer3` 検証）。
2. `python3.15 tests/sentinel/verify_soundness_kb.py`（および 3.12〜3.14）が 0 失敗。

---

## 2. Milestone M4: `heki_effect`（コールグラフ・統一 `EffectSummary`・増分インデックス）

### 2.1 拡張するデータ構造（`crates/heki_effect/src/lib.rs`）
既存の `FuncKey`, `AttrKeyPath`, `TypedFieldWrite`, `EffectSummary`, `FileSummary`, `IncrementalUpdate`, `ProjectIndex` を維持しつつ以下を補完する：

```rust
/// One hop in an interprocedural call chain (for Boundary-Only & LSP RelatedLocation).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CallHop {
    pub caller: FuncKey,
    pub callee: String,      // FuncKey or Layer 2 symbol
    pub span: Span,
    pub path: PathBuf,
}

/// Write site detail for MUT201.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TypedFieldWriteSite {
    pub class_qualname: String,
    pub attr: String,
    pub kind: WriteKind,     // DirectAttr | ContainerMut | AliasContainerMut | SetAttr | DictDirect | IndirectHelper
    pub span: Span,
    pub via_chain: Vec<CallHop>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteKind {
    DirectAttr,
    AttrDelete,
    ContainerMut,
    AliasContainerMut,
    SetAttr,
    DictDirect,
    IndirectHelper,
}
```
`EffectSummary` へ追加（既存フィールドはそのまま維持）：
- `pub mutated_param_paths: BTreeMap<u8, BTreeSet<String>>`（引数 `i` 本体へのコンテナ破壊は `""`、引数 `i` の属性 `a` への書込／破壊は `"a"`）
- `pub exec_chains: BTreeMap<ExecEffectKind, Vec<CallHop>>`（最短コールチェーン）
- `pub typed_field_write_sites: BTreeSet<TypedFieldWriteSite>`
- `pub is_async: bool`, `pub is_cached_property: bool`, `pub is_lru_cache: bool`, `pub owner_class_qualname: Option<String>`
- `pub decorators: Vec<String>`

### 2.2 モジュール名・シンボル解決規則
プロジェクトルート `root` からの相対パスを POSIX モジュール名へ変換する：
- `src/pkg/mod.py` または `pkg/mod.py`（`pyproject.toml` または `src/` レイアウトを自動検出：もし `src/<pkg>/__init__.py` があれば `src/` をストリップ）→ `pkg.mod`
- `pkg/__init__.py` → `pkg`
- 相対インポート `from .sub import Foo`（`level = 1`）：現在モジュールがパッケージ `__init__.py` なら `pkg.sub.Foo`、通常モジュール `pkg.a` なら親 `pkg` に結合して `pkg.sub.Foo`。
- 各モジュールで `import_map: FxHashMap<String, String>`（`bound_name -> fully_qualified_symbol`）を構築（`semantic.imports` から生成；`in_type_checking == true` の import も **Advisory の型注釈解決（Priority 2）のみ** には使ってよい！）。

### 2.3 4 段階軽量レシーバ型解決（Advisory レーン専用）
各 `IrFunction` 内で `var_types: FxHashMap<VarId, String>`（`VarId -> resolved_class_or_module_fqn`）を前方 1 パスで推論する（型エラー報告は一切しない）：

1. **Priority 1（Exact — ローカル生成・直接インポート）**：
   - `LoadGlobal { dst, name }` / `Import { dst, .. }`：`import_map` にあればその FQN（例: `requests`, `httpx`, `pandas`）。
   - `AttrLoad { dst, obj, attr }`：`var_types[obj]` がモジュール FQN なら `"{obj_fqn}.{attr}"`（例: `httpx.Client`）。
   - `Call { dst, callee, .. }`：`callee` が解決したクラス FQN（大文字始まり or Layer 2 登録コンストラクタ、例: `httpx.Client`, `requests.Session`, `DataModel`）なら `dst` の型 = そのクラス FQN。
   - `AllocBuiltin { dst, kind, .. }` → `"builtins.list"` / `"builtins.set"` / `"builtins.dict"` 等。
   - `Copy { dst, src }` → `var_types[src]` を引き継ぐ。
2. **Priority 2（Typed — PEP 484 注釈）**：
   - パラメータ `p` の `p.annotation` を正規化（`Optional[T]` / `T | None` → `T`、`list[T]` → `builtins.list`、`Annotated[T, ...]` → `T`、`import_map`＋現在モジュールのクラスで FQN 解決）して `var_types[p.var]` に設定。
   - メソッドの先頭引数 `self` / `cls` → `owner_class_qualname` を現在モジュールで修飾した FQN。
   - `local_annotations`（`x: UserRepo` / `self.http: httpx.Client`）を適用。
   - 既知関数の戻り値注釈 `return_annotation` があれば `Call`/`MethodCall` の `dst` 型に設定。
3. **Priority 3（Typed — クラス `__init__` 等での `self.attr` 推論）**：
   - クラス単位の事前スキャンで、`__init__` / `__post_init__` 内の `AttrStore { obj: self_var, attr, value }` と `local_annotations` (`self.<attr>: T`) から `class_attr_types[(class_fqn, attr)] = type_fqn` を収集。
   - 任意メソッド内の `AttrLoad { dst, obj, attr }` で `var_types[obj]` がクラス `C`（またはその基底クラス）なら `class_attr_types[(C, attr)]` を `var_types[dst]` に設定！
4. **Priority 4（Heuristic — CHA 継承階層解決）**：
   - `MethodCall { recv, method, .. }` で `var_types[recv]` がクラス `C` のとき、`C` 自身と全サブクラス（`ClassInfo.bases` 逆引き）から `method` を持つクラスを列挙。
   - 候補数 `1..=8` なら全候補へコールエッジを張る。**候補数が 8 を超えたら未解決（`unknown_calls = true`）とし、過剰エッジを張らない。**

### 2.4 コンテナ破壊メソッド集合（`MUT101` / `MUT201` / `MUT202` 共通）
以下のメソッド呼び出しはレシーバ `recv` のインプレース変更とみなす：
`append`, `extend`, `insert`, `pop`, `remove`, `clear`, `sort`, `reverse`, `add`, `discard`, `update`, `popitem`, `setdefault`, `difference_update`, `intersection_update`, `symmetric_difference_update`, `__setitem__`, `__delitem__`

### 2.5 ローカル効果抽出（1 関数内）
1. **Direct Exec Effects**：
   - 各 `Call` / `MethodCall` の解決先 FQN を `EffectiveEcosystemKb::lookup` で検索。
   - `entry.excluded == true` なら無視。
   - **`is_awaited == true` かつ `entry.async_safe == true`**、または **`is_awaited == true`（Stage 0-A 確定フィルタ）** の場合は `EFF101` 用ブロッキング効果（`BlockingNet`, `BlockingDb`, `BlockingSubprocess`, `BlockingSync`, `Sleep`, `BlockingFs`）を **付与しない**（ただし `DbQuery` / `UnpooledClientAlloc` はループ内検出 `EFF102`/`EFF103` のため保持する！）。
   - `entry.offloads_blocking_arg == Some(k)` のとき：
     - 呼び出し引数 `args[k]` が関数パラメータ `params[p]` 由来（SSA `Copy` 辿り）なら、この関数の `offloads_blocking_params |= 1u64 << p`！
     - 引数 `args[k]` が既知の関数参照（例: `asyncio.to_thread(sync_helper)`）であっても、**この呼び出し経由のブロッキング効果は `caller` へ伝播させない（オフロード遮断）**！
2. **Self Attr Reads / Mutates / Cache Invalidations（`MUT101` 用）**：
   - `self_var = params[0].var`（メソッドかつ `owner_class.is_some()` の場合）。
   - `AttrLoad { dst, obj: self_var, attr }`：
     - もし `dst` が直後に `SubscriptLoad { obj: dst, key_const: Some(k), .. }` に使われるなら `reads_self_attrs.insert(AttrKeyPath { attr, const_key: Some(k) })`。
     - そうでなければ `reads_self_attrs.insert(AttrKeyPath { attr, const_key: None })`。
   - `AttrStore { obj: self_var, attr, .. }` / `AttrDelete { obj: self_var, attr }`：
     - `AttrDelete` は `invalidates_cached.insert(attr)` にも記録！
     - `AttrStore` は `mutates_self_attrs.insert(AttrKeyPath { attr, const_key: None })`。
   - `self.__dict__.pop("prop", ...)` / `del self.__dict__["prop"]` → `invalidates_cached.insert("prop")`；`self.__dict__.clear()` → `invalidates_cached.insert("*")`（全無効化ワイルドカード）。
   - 関数内ローカル別名マップ `self_attr_alias: FxHashMap<VarId, String>`（`AttrLoad { dst, obj: self_var, attr }` → `dst -> attr`、`Copy` で伝播）：
     - `SubscriptStore { obj, key_const, .. }` で `self_attr_alias[obj] == attr` なら `mutates_self_attrs.insert(AttrKeyPath { attr, const_key: key_const })`（`self.params["proxy"] = url` の定数キーを正確に保持！）。
     - `MethodCall { recv, method, .. }` で `method` がコンテナ破壊かつ `self_attr_alias[recv] == attr` なら `mutates_self_attrs.insert(AttrKeyPath { attr, const_key: None })`。
3. **Param Mutations & Typed Field Writes（`MUT201` / `MUT202` 用）**：
   - `param_alias: FxHashMap<VarId, (u8, Option<String>)>`（`params[i].var -> (i, None)`、`AttrLoad { dst, obj, attr }` で `obj` が `(i, None)` なら `dst -> (i, Some(attr))`、`Copy` で伝播）。
   - `obj_attr_alias: FxHashMap<VarId, (String, String)>`（`AttrLoad { dst, obj, attr }` で `var_types[obj] == class_fqn` なら `dst -> (class_fqn, attr)`、`Copy` で伝播）。
   - 以下の操作を検出して `mutates_params` / `mutated_param_paths` / `typed_field_write_sites` へ記録：
     - `AttrStore` / `AttrDelete`（`WriteKind::DirectAttr` / `AttrDelete`）
     - `MethodCall`（コンテナ破壊メソッド）または `SubscriptStore` / `SubscriptDelete`（直属なら `ContainerMut`、別名変数経由なら `AliasContainerMut`）
     - `Call("setattr", [obj, attr_const, _])` / `Call("object.__setattr__", [obj, attr_const, _])`（`WriteKind::SetAttr`、`const_str(attr_const)` で属性名取得）
     - `SubscriptStore` on `obj.__dict__[attr_const]`（`WriteKind::DictDirect`）

### 2.6 Tarjan SCC 逆トポロジカル順 Worklist 不動点計算
```text
1. 全モジュールの CallGraph を構築（caller -> Vec<CallEdge { callee, args, is_awaited, span }>）。
2. Tarjan SCC で強連結成分を抽出し、トポロジカル順の逆順（葉 → 根）で処理する。
3. 各 SCC 内で、いずれかの関数の EffectSummary が変化しなくなるまで Worklist を回す（単調増加のため必ず有限回で収束；安全上限 16 反復/SCC）：
   For each edge (caller -> callee):
     a. callee.offloads_blocking_params のビット k が立っているとき：
        - edge.args[k] が caller のパラメータ p 由来なら caller.offloads_blocking_params |= 1 << p。
        - edge.args[k] に渡された callable からのブロッキング伝播は遮断！
     b. Exec Effects 伝播：
        - プロジェクト内 `callee` が **同期関数（`!callee.is_async`）** なら `is_awaited` の有無に関係なく実行される（`await sync_helper()` でも本体が先にイベントループ上で同期実行されるため）ので、`direct_exec_effects ∪ transitive_exec_effects` を caller へマージする。
        - プロジェクト内 `callee` が **`async def`** のときは、`edge.is_awaited || edge.drives_coroutine_manually` のときのみ伝播する（await しない素呼び出しはコルーチン生成のみであり、本体は自身の境界で報告される）。
        - Layer 2 API 呼び出しについては 2.5 で `is_awaited` フィルタ済みの `effects` のみ保持する。
        - Boundary-Only Reporting（2.7）では、`async def caller` の呼び出し先が同期プロジェクト関数または Layer 2 ブロッキング API である最初の越境点のみを報告する。
     c. Mutation 伝播：
        - callee が `self.helper()`（同一クラスメソッド）なら `reads_self_attrs`, `mutates_self_attrs`, `invalidates_cached` を caller へマージ。
        - callee.mutated_param_paths の各 (param_idx, path) に対し、caller の `edge.args[param_idx]` が：
          - `caller` のパラメータ `p` 由来なら `caller.mutated_param_paths[p]` へ伝播し `caller.mutates_params |= 1 << p`。
          - `obj_attr_alias` に `(class_fqn, attr)` として載っていれば、`caller.typed_field_write_sites` に `WriteKind::IndirectHelper`（`via_chain` に `callee` を追加）を記録！
        - `callee.typed_field_write_sites` はそのまま `caller` へ推移記録するか、呼び出し元判定（`MUT201` で `allowed_writers` 違反チェック）に使う（注意：`allowed_writers` 内の正規メソッドが呼ぶプライベート helper の場合は、`caller` が許可されており `callee` も許可されていれば違反なし；逆に `caller` が非許可で `callee` が汎用 helper なら `caller` 側を違反報告する！）。
```

### 2.7 `sync_driven_only`・Boundary-Only Reporting・Root-Cause Grouping
- **`sync_driven_only` 判定（dcg `client.py:1399` 対策）**：
  - `async def f` がプライベート名（`_` 始まり）であり、プロジェクト内の全呼び出し元が同期関数内の手動コルーチン駆動（`.send(None)` や `run_sync` 互換ドライバ）であり、通常の `await f()` や Web ルートデコレータを持たない場合、`sync_driven_only = true` として `EFF101`/`EFF105` を緩和する。
- **Boundary-Only Reporting（`EFF101` / `EFF105`）**：
  - `async A -> await async B -> sync C -> time.sleep()` のとき、`async B`（`async` から同期世界へ最初に越境する関数）**のみ** で報告し、`async A` では重複報告しない。
  - 実装ルール：`async def caller` の各コールエッジで、`callee` が **非 async（同期関数または Layer 2 ブロッキング API）** であり、かつ `!edge.is_awaited` かつオフロードされていない場合にのみ `EFF101`/`EFF105` を発火する。
- **Root-Cause Grouping**：
  - 同一の最終シンク（例: `time.sleep` または同一の同期関数 `FuncKey`）に起因する `EFF101` 診断がプロジェクト内で 10 件（既定閾値）を超えた場合、先頭 1 件を代表診断として出力し、メッセージ末尾に `(+N more async call sites reaching the same blocking root cause)` を付記して集約する。

### 2.8 Layer 3 オンデマンド外部 Pure Python 解析（任意拡張・境界厳守）
- プロジェクト外のサードパーティパッケージ `pkg` の関数が呼ばれたとき：
  1. **即座に拒否**：`EffectiveEcosystemKb::is_opaque_to_layer3(top_pkg)` が `true`、または Python 標準ライブラリ名であれば **絶対にファイルを開かず探索終了**（ADR-005）。
  2. それ以外の Pure Python 外部モジュールのみ、`.venv` / `sys.path` 相当から最大 **3 ホップ**・1 ファイル **10,000 行上限**・合計 **200 モジュール上限** で解析し、blake3 キーでキャッシュする。
  3. MVP 段階ではローカル環境で外部パッケージが見つからない場合は安全に `unknown_calls = true` としてスキップしてよい。

### 2.9 M4 完了ゲート
`cargo test -p heki_effect --offline` で以下すべて合格：
1. 2〜5 ホップ間接呼び出しの `BlockingNet` / `Sleep` 伝播と最短 `CallHop` チェーン生成。
2. `in_thread(fn)` ユーザーラッパーの `offloads_blocking_params` 伝播によるブロッキング遮断。
3. Boundary-Only Reporting（`async A -> await async B -> sync C -> sleep` で `B` のみ報告）。
4. `mutated_param_paths` による関数跨ぎコンテナ破壊伝播（`_sneaky_helper(model.fields, "c")`）。
5. `ProjectIndex::update_file` の Early Cutoff（サマリ不変時 `affected_callers` 空、変化時のみ逆コールグラフ通知）。

---

## 3. Milestone M5: 族 1 (`MUT`) & 族 2 (`EFF`/`DATA`) ルール ＋ CLI / Ratchet

### 3.1 対象ファイル
- `crates/heki_rules/src/lib.rs`（＋ `mut_rules.rs`, `eff_rules.rs`, `data_rules.rs`）
- `crates/heki_diagnostics/src/lib.rs`（5 フォーマッタ・インライン抑止行マップ・Ratchet ファイル I/O）
- `crates/heki/src/main.rs`（`heki check` 完全実装）

### 3.2 各ルールの厳密判定仕様

#### `MUT101` (`cached-property-stale-mutation`)
クラス `C` ごとに（**`owner_class: Some(cid)` のメソッドのみ**を対象とし、ネストクラスは別 `ClassId` のまま絶対に混ぜない）：
1. `@cached_property`（`has_decorator(&["cached_property"])`）を持つメソッド `p` を抽出（基底クラスとサブクラスで同名の `@cached_property` をオーバーライドしている場合は `__dict__` の同一スロットを使うためプロパティ名ごとに 1 件へ集約）。`R = reads_self_attrs(p)`（`p` 自身と同名属性は除く、`self.helper()` 経由の読み取りを含む伝播後サマリ）。
2. クラス `C`（および継承サブクラス）の他メソッド `m` について、**`m` 自身のローカル書込サマリ（`local_functions`）** に対して以下の **4 厳格フィルタ＋呼び出し元無効化チェック** を適用し、実際の書込行で 1 回だけ報告する（呼び出しツリー上位の全メソッドでの重複報告を防止）：
   - **Filter 1（初期化・2 段初期化除外）**：`m.name` が `__init__`, `__post_init__`, `model_post_init`, `__setstate__`, `__enter__`, `__new__`, `setup`, `setUp`, `postinit`, `_open`, `_setup` のいずれか、または `_init_` / `_configure_` で始まる、または **`__init__` から直接呼び出されるプライベートメソッド（`_` 始まり）** なら除外。
   - **Filter 2（メモ／キャッシュ辞書除外）**：変更対象属性名 `a.attr` に `cache` または `memo`（大文字小文字無視）を含む場合、または `m` 内の書込が `setdefault` もしくは `if k not in self.<attr>:` ガード直下の添字代入である場合は除外。
   - **Filter 3（互いに素な定数 dict キー除外）**：`p` が `a.attr` をすべて `const_key: Some(kr)` で読んでおり、`m` も `a.attr` をすべて `const_key: Some(kw)` で書いており、かつ `K_read ∩ K_write == ∅` なら除外。
   - **Filter 4（キャッシュ無効化確認）**：`m` の `invalidates_cached` に `p.name` または `"*"`（`self.__dict__.clear()`）が含まれていれば除外。また、`m` がプライベート helper（`_` 始まり）で呼び出し元が存在し、**すべての呼び出し元が `p.name`（または `"*"`）を無効化済み**なら除外。
3. 残った `(p, m, attr)` に対して `MUT101` を書込行に 1 回報告する（`target = "{ClassQualname}.{attr}->{p.name}"`）。

#### `MUT201` (`write-owner-violation`)
`config.write_owners` の各 `WriteOwnerRule` に対し：
1. `targets` の各パターン（例: `"pkg.model.DataModel.fields"` または末尾クラス名 `"Model.fields"`）を `(target_class_pattern, target_attr)` に分解。
2. 関数 `f` の全 `typed_field_write_sites`（直接・別名・コンテナ破壊・`setattr`・`__dict__`・間接 helper 経由）を走査し、`site` が `target` に一致したら：
   - `f` の完全修飾名 `"{module}.{f.qualname}"`（および短縮 `f.qualname`）が `rule.allowed_writers` の glob パターン（`*` ワイルドカード対応、`globset` クレート利用可）のいずれかに一致すれば OK。
   - 一致しなければ `MUT201` を報告（`target = "{Class}.{attr}"`, `related` に `via_chain` を格納）。
3. `rule.forbid_cached_property_on` に一致するクラス上で `@cached_property` メソッドが定義されていたら、その定義位置にも `MUT201` を報告。

#### `MUT202` (`readonly-return-mutation`)
以下のいずれかを **Read-Only Producer** とする：
- `config.readonly_returns` の `producers` に一致する関数／メソッド／プロパティ（`deep = rule.deep`）。
- プロジェクト内で `@lru_cache` / `@functools.lru_cache` / `@cache` / `@functools.cache` デコレータが付いた関数（自動的に `deep = true` 扱い）。

各関数内で前方データフロー（ローカル別名マップ `ro_origin: FxHashMap<VarId, (String, bool /* deep */)>`）を実行：
1. Producer の呼び出し結果 `dst` に `ro_origin[dst] = (producer_name, deep)` を付与。
2. 伝播：`Copy`、`SubscriptLoad`（`deep == true`）、`AttrLoad`（`deep == true`）、タプル展開。
3. **遮断（サニタイザ）**：`EffectiveEcosystemKb` で `copy_sanitizer == true` の呼び出し結果（`copy.deepcopy(x)`, `x.copy()`, `x.model_copy(deep=True)` — `model_copy` は `kwargs` に `deep=True` がある場合のみ `deep` を遮断、`deep=False` なら浅いコピーのみ）は `ro_origin` を付与しない。
4. **違反検出**：`ro_origin` が付いた変数 `v` に対して：
   - `AttrStore` / `AttrDelete` / `SubscriptStore` / `SubscriptDelete` / コンテナ破壊 `MethodCall` を実行 → **`MUT202`**！
   - 既知関数 `callee` の引数 `i` に渡し、`callee.mutates_params` のビット `i` が立っている → **`MUT202`**（`related` に `callee` へのホップを追加）！

#### `EFF101` (`async-blocking-call`) & `EFF105` (`async-blocking-fs`, opt-in)
- 対象：`f.kind` が `Async` または `AsyncGenerator`、かつ `!summary.sync_driven_only`。
- コンテキスト緩和（既定抑止）：`click.command` / `typer.command` デコレータ、FastAPI/Starlette `lifespan`（`@asynccontextmanager` かつ名前に `lifespan`）、テスト関数（`test_*` またはパスが `tests/` 配下 — ただし明示的フィクスチャテスト時は設定で制御可能にすること！）。
- `f` 内の各呼び出しエッジ（`!is_awaited`、オフロード遮断済みでない）について：
  - `callee` が `BlockingNet | BlockingDb | BlockingSubprocess | BlockingSync | Sleep` を持つ → **`EFF101`**（Boundary-Only：同期世界への最初の越境点のみ、`related` に最短 `CallHop` チェーン）。
  - `callee` が `BlockingFs` を持つ → **`EFF105`**（既定 OFF、`--select EFF105` または `--extend-select EFF105` 時のみ有効）。

#### `EFF102` (`loop-unpooled-client`)
- 対象：`f.block_loop_depth(bb.id) > 0`（M2 により `for.iter` はすでに depth 0 で構造的に除外済み！）。
- 内側ループ `lp = f.innermost_loop(bb.id)` の `lp.bound` が `ConstSmall(_)` / `RetryPattern` / `DaemonPolling` なら緩和（除外）。
- 呼び出し先が直接または間接に `UnpooledClientAlloc` を持つ場合に **`EFF102`** を報告。
- 偽陽性防止：`requests.get` 等で `requests` がローカル `dict` 変数なら `var_types` が `"builtins.dict"` となり `requests.get` ではなく `builtins.dict.get` に解決されるため誤爆しない！

#### `EFF103` (`loop-db-query`)
- 対象：`f.block_loop_depth(bb.id) > 0` かつ `lp.bound` が `Unbounded`。
- 呼び出し先が直接または間接に `DbQuery` を持つ場合、以下を除外した上で **`EFF103`** を報告：
  1. 第 1 引数が定数文字列（`f.const_str(args[0])`）で先頭が `CREATE` / `ALTER` / `DROP` / `PRAGMA`（大文字小文字無視）の DDL。
  2. ファイルパスに `migrations/` または `alembic/` を含む、または関数名が `upgrade` / `downgrade`。
  3. 対象リポジトリ自体が ORM 内部実装（モジュール名が `sqlalchemy.*` / `django.db.*` / `sqlmodel.*`）。

#### `DATA101` (`loop-carried-concat`)
- 対象：`f.loop_carried` のうち **`lc.self_accumulating == true`** のエントリのみ！
- `lc.back_vars` からループ内定義を辿った中に、解決先が `QuadraticRealloc`（`pandas.concat`, `numpy.vstack`, `numpy.hstack`, `numpy.concatenate`, `numpy.append`）の呼び出しが含まれる場合に **`DATA101`** を報告。
- これにより、Stage 0-A の 60% 誤検出源だった「ループ内単発結合・パディング」を 100% 除外できる。

### 3.3 `heki_diagnostics` & `heki check` CLI 仕様
1. **インライン抑止（Line-based Comment Scan）**：
   - ソース各行の `#` コメント（文字列リテラル内 `#` を誤認しないよう簡易トークンまたは末尾コメント走査）を `parse_inline_ignores` で解析。
   - 診断行 `line` に `# heki: ignore`（空配列＝全抑止）または `[CODE / SLUG]` が一致すれば抑止。
2. **ルール選択優先順位（Ruff 準拠）**：
   - 既定：`CORE_RULES` の `default_enabled == true` の全ルール。
   - `--select` / `lint.select` 指定時は指定プレフィックス（`"EFF"`, `"MUT"`, `"DATA"`, `"OPT"`）またはコード／スラッグのみ。
   - `--extend-select` / `lint.extend-select` で追加、`--ignore` / `lint.ignore` および `lint.per-file-ignores`（`globset` マッチ）で除外。
3. **出力フォーマット（`--output-format`）**：
   - `full`（既定）：`path:line:col: CODE (slug) [severity] message` + 該当ソース行抜粋 + `related` ホップ一覧。
   - `concise`：`path:line:col: CODE message`（1 行 1 件）。
   - `json`：`serde_json::to_string_pretty(&diagnostics)`。
   - `github`：`::error file=...,line=...,col=...,title=heki CODE::message`（または `::warning`）。
   - `sarif`：SARIF 2.1.0 JSON（`runs[0].tool.driver.name = "heki"`, `results`）。
4. **Ratchet ベースライン（`--baseline <path>`, `--update-baseline`）**：
   - TOML 形式：`[[entries]]` に `path`, `qualname`, `rule_code`, `target`, `count`（ソート順固定で git diff 最小化）。
   - `--update-baseline`：現在の件数でベースラインファイルを生成／縮小更新して終了コード `0`。
   - `--baseline` のみ：`RatchetBaseline::compare` を実行し、`regressions` 非空ならそれらを報告して終了コード **`1`**；`regressions` 空なら終了コード **`0`**（`stale_entries` があれば stderr に縮小可能通知）。
5. **終了コード（厳守）**：
   - `0`: 違反なし（または Ratchet 退行なし）。
   - `1`: 1 件以上の違反（または Ratchet 退行）を検出。
   - `2`: 設定エラー・I/O エラー・不明ルール・構文致命エラー。

### 3.4 M5 完了ゲート（定量チェック必須）
1. `tests/semantic/fixtures.py` を `heki check --output-format json`（`write-owners` に `Model.fields -> Store.*` 設定）で実行し、コメント記載の **全 TP を検出・全 TN を 0 誤検出**。
2. `datamodel-code-generator` (`src/` 421 ファイル / 19.6 万行) で `heki check` を実行：
   - 実行時間 < 5 秒（release ビルドなら < 1 秒目標）。
   - `MUT101` が `jsonschema.py` の実バグを検出。
   - `MUT201`（`DataModel.fields` / `DataModel.base_classes` / `DataType.data_types`）が許可外書込を検出。
   - `EFF101`/`EFF105` で `in_thread` / `run_sync` に対する誤検出 0 件。

---

## 4. Milestone M6: `heki_soundness` & 族 3 (`OPT`) ＋ `--fix` / `MUT103` `snapshot`

### 4.1 絶対不変条件（ADR-004 / ADR-011）
1. **`crates/heki_soundness/Cargo.toml` に `heki_effect` を絶対に追加してはならない。**
2. **`heki_soundness` は `ParamInfo.annotation`, `IrFunction.return_annotation`, `IrFunction.local_annotations` を一切参照してはならない。** 違反すると反例 S9b（`WeirdList(list)` に `list[int]` 注釈）で即誤変換する。
3. **`IrFunction.a1_hazards` が 1 件でも非空なら、当該関数の全 `OPT` 候補を即 `SafetyTier::Blocked("A1 hazard: ...")` とする。**
4. **既定の `heki check --fix` が適用してよいのは `SafetyTier::Safe` かつ `ProfitabilityTier::Unconditional`（`OPT101`, `OPT102` の先頭累積 `{**d, ...}` のみ）に限る。** `SizeDependent` は `--include-size-dependent` または `--profile` ヒット時のみ。

### 4.2 RPO Worklist 抽象解釈（`crates/heki_soundness/src/interpreter.rs`）
入力：`ir_fn: &IrFunction`, `kb: &EffectiveSoundnessKb`。

#### 初期状態（Entry `BlockId(0)`）
- 全 `param` の `var`: `ExactTypeVal::Unknown { reason: "parameter" }`, `UniqueState::Shared { witness: EscapeWitness::ParameterInput }`。
- `ir_fn.captured_vars` に含まれる名前の変数は、生成された瞬間から `Shared` とみなす（クロージャ経由の外部観測・変更を安全側遮断）。

#### 遷移規則（各 `InstKind`）
- `AllocBuiltin { dst, kind, elems }`：
  - 新規 `alloc_id = fresh_alloc()`。
  - `var_type[dst] = Exact { ty: kind, origin: span }`。
  - `var_unique[dst] = Unique { alloc_ids: [alloc_id] }`、`alias_sets[alloc_id] = {dst}`。
  - **全 `e in elems` を `mark_escaped(e, StoredInContainerOrAttr(span))`**！
- `DictLiteral { dst, entries }`：
  - 新規 `alloc_id`、`Exact(Dict)`、`Unique([alloc_id])`。
  - 各 `KeyValue { key, value }` の `key`, `value` を `mark_escaped(..., StoredInContainerOrAttr(span))`；`Unpack { dict }` の `dict` は読取（`dict` が `Exact(Dict)` でないなら不明 `__getitem__` 呼び出し可能として安全側保守）。
- `Const { dst, literal }`：
  - `Int`/`Float`/`Str`/`Bytes`/`Bool` なら対応する `Exact(BuiltinKind)`（`Unique` 不要な不変値）。
- `Copy { dst, src }`：
  - `var_type[dst] = var_type[src]`、`var_unique[dst] = var_unique[src]`。
  - `src` が `Unique { alloc_ids }` なら、各 `aid` の `alias_sets[aid]` に `dst` を追加！
  - `src` が `derives_from[src]` を持つなら `derives_from[dst]` と各 `aid` の `derived_refs[aid]` にも `dst` を追加！
- `Delete { var: Some(v), .. }`（Strong Update）：
  - 全 `alias_sets` / `derived_refs` から `v` を除去。
- `MakeIter { dst, iterable, .. }` / `MakeGenExpr { dst, outer_iter, captured, .. }`：
  - `target = iterable` (or `outer_iter`) の全 `alloc_id`（`var_unique[target]` および `derives_from[target]` の推移閉包）を `dst` の `derives_from[dst]` に登録し、各 `aid` の `derived_refs[aid]` に `dst` を追加！
  - `MakeGenExpr.captured` の全変数を `mark_escaped(c, PassedToUnknownCall(span))`。
- `MakeClosure { captured, .. }`：全 `captured` を `mark_escaped`。
- `AttrStore { value, .. }` / `SubscriptStore { value, .. }` / `StoreGlobal { value, .. }` / `StoreDeref { value, .. }` / `Yield { value: Some(v), .. }` / `YieldFrom { value: v, .. }`：
  - `mark_escaped(value, StoredInContainerOrAttr(span))`。
- `Call` / `MethodCall`：
  - Layer 1 `kb.lookup(symbol)` が存在し、かつ（組み込み名が関数内でシャドーイングされていない）場合：
    - 各引数 `arg_i` に `param_effects[i]` を適用：
      - `ReadOnly`：エスケープなし。
      - `Mutated`：変更記録（`ValueLoopInvariant` 判定用に `mutated_allocs_in_block` へ記録）。
      - `RetainedInReturn`：`dst` を `arg_i` の全 `alloc_id` の `derived_refs` へ推移追加！
      - `Escaped`：`mark_escaped(arg_i, PassedToUnknownCall(span))`。
    - `return_exact_type` があれば新規 `alloc_id` と `Exact` を `dst` に設定。
  - Layer 1 に未登録（未知関数）：全引数・`kwargs`・`recv` を `mark_escaped(..., PassedToUnknownCall(span))`！
- **`mark_escaped(v, witness)` の推移的エスケープ（S23 / S24 / S26 の核心）**：
  1. `v` の `UniqueState` が `Unique { alloc_ids }` なら、各 `aid` について `alias_sets[aid]` の全変数を `Shared { witness }` に降格。
  2. さらに `derives_from[v]` が非空なら（つまり `v` がイテレータ・ジェネレータ・`dict.keys()` ビュー等の派生参照なら）、元の各 `aid` の `alias_sets[aid]` 全変数を **`Shared { witness: DerivedRefEscapedAt(span) }`** に降格！

#### 合流（`join_frame`）と RPO 収束
- `ir_fn.rpo_blocks()` 順で Worklist を回し、各先行ブロック（`predecessors()`、**`exc_target` エッジ含む**）の出力状態を `join_frame` する。
- `ExactTypeVal` の合流：同一 `BuiltinKind` なら維持、異なれば `Unknown`。
- `UniqueState` の合流：既存の `join_unique`（`MAX_JOIN_ALLOC_IDS = 4` 超過で `Shared { JoinAllocOverflow }`）。
- `alias_sets` / `derived_refs` の合流：和集合。

### 4.3 逆方向 Liveness 解析と `ExclusiveAccessAtWrite`（S23 / S24 / S26 撃破）
各ブロック・命令点 `p` で生存している SSA 変数集合 `live_after[p]` を標準の逆方向ビットセット／ハッシュセットデータフローで計算する（**`exc_target` エッジも後続ブロックに含める**こと！例外ハンドラで古い別名が読まれるケース `ExceptionStateSafe` も同時に検出できる！）。

- **`ExclusiveAccessAtWrite(x, write_site)` の厳密定義**：
  - `write_site` 直前の `FrameState` で `var_unique[x] == Unique { alloc_ids }` であること。
  - 各 `aid in alloc_ids` について：
    1. `alias_sets[aid]` に含まれる `x` 以外の変数 `a` が `live_after[write_site]` に **1 つも存在しないこと**。
    2. `derived_refs[aid]` に含まれる変数 `d`（`for v in x:` の `MakeIter` 変数 `iter_var` や `it = iter(x)` や `d.keys()` ビュー等！）が `live_after[write_site]` に **1 つも存在しないこと**！
  - 反例検証：
    - **S23 (`for v in x: x = x + [v]`)**：`MakeIter` で作られた `iter_var` は `derived_refs[alloc(x)]` に入り、ループバックエッジで `header` の `IterNext(iter_var)` に使われるため `write_site` で **LIVE** → `ExclusiveAccessAtWrite` が **`Disproved("live derived iterator in loop")`** になり確実にブロック！
    - **S24 (`it = iter(x); x = x + [3]; list(it)`)**：`it` が `derived_refs[alloc(x)]` に入り後続 `list(it)` で **LIVE** → **`Disproved`**！
    - **S26 (`for k in d.keys(): d = {**d, "b": 2}`)**：`dict.keys` は Layer 1 `RetainedInReturn` → `MakeIter` で推移引き継ぎされループ中 **LIVE** → **`Disproved`**！

### 4.4 `ValueLoopInvariant(v, loop)` の厳密定義（S27 / S28 撃破）
1. `v` の定義が `loop.body` の外にある（`BindingLoopInvariant`）。
2. `v` の `ExactType` が不変組み込み型（`tuple` は生成元 `AllocBuiltin.elems` がすべて不変ハッシュ整合型 `int|str|bytes|bool|float` であること）、**または** 可変組み込み型（`list`）が `loop` 全体を通じて `Unique` であり、かつ `loop.body` 内のどの命令でも `v` の `alloc_id`（別名・派生参照含む）に対する変更（`Mutated` / `SubscriptStore` / `SubscriptDelete`）が 0 件であること。
   - **S27**（ループ内で `lst.append(3)`）：`lst` の `alloc_id` に変更命令あり → **`Disproved`**！

### 4.5 族 3 (`OPT`) 8 ルールの Safety × Profitability 判定と `TextEdit`
各候補に対し `SafetyVerdict`（全 `ProofObligation` の 3 値集計：1 つでも `Disproved` → `Disproved`、1 つでも `Blocked` → `Blocked`、すべて `Established` → `Safe`）と `ProfitabilityTier` を独立に算出する：

| Rule | パターン（IR 上） | 追加必須 Safety 証明義務 | Profitability | 自動 `--fix`（既定） |
|---|---|---|---|---|
| **OPT101** | `LoopCarried(x)` かつ `x = x + [e]` (`BinOp(Add, x, AllocBuiltin(List, [e]), in_place=false)`) | `Unique(x)`, `ExclusiveAccessAtWrite(x)`, `ExactType(x, List)`, `ExactType(rhs, List)`, `EvalOrderPreserved`, `ExceptionStateSafe` | **`Unconditional`** | **YES** (`x.append(e)` または複数要素なら `x.extend([...])`) |
| **OPT102** | `LoopCarried(d)` かつ (a) `d = {**d, ...}`（**`entries[0]` が `DictEntry::Unpack { dict }` かつ `dict == d_phi` かつ以降に `d` の再アンパックなし**）または (b) `s = s \| rhs` | `Unique(target)`, `ExclusiveAccessAtWrite(target)`, `ExactType(target, Dict\|Set)`, **辞書アンパック先頭位置（dcg 反例 `{**other, **d}` を `Disproved`！）**, `ExceptionStateSafe` | **`Unconditional`** | **YES**（`{**d, k: v}` → `d[k] = v` / `d.update(...)`、`s = s \| rhs` → `s \|= rhs`） |
| **OPT103** | `LoopCarried(acc)` かつ `acc = acc + rhs` (`Bytes` / `Tuple` / `self.s +=`) | `Unique(acc)`, `ExclusiveAccessAtWrite(acc)`, `ExactType(acc/rhs)`, ローカル `str +=`（`in_place=true`）は除外 | `SizeDependent` | NO（診断＋提案のみ） |
| **OPT110** | ループ内 `Compare(In/NotIn, e, lst)` | `ValueLoopInvariant(lst, loop)`, `ExactType(lst, List\|Tuple)`, ハッシュ整合組み込み要素（S22 防止：`ZeroTripExceptionSafe` — 全要素が `ExactType` で `int\|str\|bytes\|bool\|float` 確定時のみ `Safe`、それ以外は `Blocked`！） | `SizeDependent`（`NotConstSmall`） | NO |
| **OPT111** | ループ内 `if v not in seen: seen.append(v)` | `Unique(seen)`, `ExclusiveAccessAtWrite`, `ExactType(seen, List)`, ハッシュ整合要素, ループ内削除・置換なし | `SizeDependent` | NO |
| **OPT120** | ループ内 `q.pop(0)` / `del q[0]` / `q = q[1:]`（`const_int == Some(0)` or `is_slice && slice_lower_const == Some(1) && slice_upper_const == None`） | `Unique(q)`（非返却・非エスケープ）, `ExactType(q, List)`, `deque` 等価操作のみ, **任意中間添字 `q[i]`（`0`, `-1` 以外）なし** | `SizeDependent` | NO |
| **OPT121** | ループ内 `q.insert(0, v)` | `OPT120` と同一 | `SizeDependent` | NO |
| **OPT130** | ループ内 `e in set(keys)` 等のコンテナ再構築 | 全引数 `ValueLoopInvariant`, 副作用なし, `ZeroTripExceptionSafe` | `SizeDependent` | NO |

### 4.6 `--fix` / `--diff` 適用仕様（`crates/heki/`）
- 1 ファイル内の複数 `TextEdit` は **span 重複チェック** を行い、後ろから前へ降順適用する（重複があれば後者をスキップし安全優先）。
- `--diff` 時は統一 diff を stdout に出力しファイルには書き込まない。
- `--fix` 時は、既定で `safe && unconditional`（`OPT101`/`OPT102`）のみ適用。`--include-size-dependent` または `--profile` のホット関数一致時のみ `size-dependent` の `safe` fix も許可。

### 4.7 `MUT103` & `heki snapshot` 仕様
- `heki snapshot [PATHS...] --write`：各公開関数（`_` 始まり以外）の `(module, qualname)` ごとに正規化サマリ（`mutates_params`, `escapes_params`, `mutates_self_attrs`, `transitive_exec_effects`）を `.heki-summary-snapshot.json` へ保存。
- `heki check --extend-select MUT103`：保存済みスナップショットと比較し、既存関数の副作用集合が真に増加した箇所に **`MUT103`** を報告。

### 4.8 M6 完了ゲート（定量チェック必須）
1. **`tests/difftest/counterexamples.py` 全 7 反例（S23, S24, S26, dcg `{**other, **d}`, S27, S9b, S22）に対し、`heki_soundness` が全件 `Blocked` または `Disproved`（`Safe` 0 件）を返す自動テスト合格。**
2. **義務剥奪テスト（Obligation-Stripping Test）**：`heki_soundness` のユニットテスト内で `ExclusiveAccessAtWrite` / `ValueLoopInvariant` / `DictUnpackLeading` / `ZeroTripExceptionSafe` を意図的に無効化するフラグを使い、反例が素通りすること（＝各義務が実際に反例を止めていること）を証明。
3. **`OPT101` / `OPT102` 正例**（`x = []; for i in items: x = x + [i]`、`d = {}; for k, v in items: d = {**d, k: v}`）が `Safe` + `Unconditional` と判定され、`--fix` 後の Python コードが CPython 3.15 で元コードと完全同一出力を返す差分テスト合格。
4. `cargo tree -p heki_soundness` に `heki_effect` が **一切出現しない** ことを確認。

---

## 5. Milestone M7: `heki_lsp`（増分 LSP サーバー `heki server` / `heki lsp`）

### 5.1 依存追加（`Cargo.toml` / `crates/heki_lsp/Cargo.toml`）
- `tower-lsp = "0.20"`
- `tokio = { version = "1", features = ["rt-multi-thread", "macros", "io-std", "time", "sync"] }`
- （※オフライン環境の場合は先に `cargo fetch` を 1 回実行すること）

### 5.2 アーキテクチャ（ADR-017 厳守）
- **`ProjectIndex` に `IrFunction` 本体を常駐させない。** メモリ常駐は軽量 `FileSummary` + `reverse_call_graph` + 開いているドキュメントのテキストバッファのみ。
- **初期化（`initialized`）**：バックグラウンドタスクでワークスペースの `.py` ファイルを `rayon` 並列解析し `ProjectIndex` を構築。完了後に全開中ドキュメントの診断を `publish_diagnostics` する。
- **編集時（`didChange`）**：250ms debounce 後に **編集中の単一ファイルのみ** 再パース → IR → 診断計算（既存 `ProjectIndex` の他ファイルサマリを参照）。目標応答 5〜20ms。
- **保存時（`didSave`）またはサマリ確定時**：`IncrementalEngine::apply_file_summary`（`ProjectIndex::update_file`）を実行。
  - **`summary_changed == false`（Early Cutoff）**：他ファイルの再解析を完全スキップ！（※ただし `FileSummary.decls` のクラス属性型・基底クラス等の宣言が変化した場合は、依存先ファイルも再抽出・再伝播する。）
  - **`summary_changed == true`**：`affected_callers` を含むファイルのみ再解析して `publish_diagnostics`。
- **診断・Code Action 変換**：
  - `Diagnostic.related` → LSP `DiagnosticRelatedInformation`（`EFF101`/`MUT201` の呼び出しチェーン全ホップへエディタから 1 クリックジャンプ）。
  - `Diagnostic.fix`（`OPT101`/`OPT102` の `safe` + `unconditional`）→ `CodeActionKind::QUICKFIX`。
  - `size-dependent` `OPT` 提案 → `CodeActionKind::new("refactor.rewrite.heki")`。
  - 全診断共通 → `# heki: ignore[CODE]` 行末挿入の `CodeActionKind::QUICKFIX`。

### 5.3 M7 完了ゲート
1. `heki server --stdio` が LSP `initialize` → `initialized` → `textDocument/didOpen` → `publishDiagnostics` → `textDocument/codeAction` → `shutdown` の一連の JSON-RPC やり取りを自動統合テストで完走。
2. `didChange` でコメントのみ変更した際に `ProjectIndex::update_file` が `summary_changed == false`（Early Cutoff）を返し、他ファイル再解析回数 0 であることをテスト検証。
