# 解析エンジン・IR・Proof Obligation・KB 詳細仕様（`heki` Baseline v4.1 補足資料）

> **このドキュメントの位置づけ**
> [ARCHITECTURE.md](../ARCHITECTURE.md)（Baseline v4.1：正式名称 `heki`）で定義した **「単一の Effect & Alias 統一理論による 2 レーン・3 族アーキテクチャ ＋ 増分 LSP（Early Cutoff）」** の技術補足仕様である。コード上のクレート名は `heki_*`（`crates/heki_ast`〜`crates/heki_lsp`）、設定キーは `[tool.heki]`（kebab-case）に統一されている。具体的な実装マイルストーンは [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) を参照。

---

## 1. フロントエンド・意味解析・SSA-CFG IR 仕様（`heki_ast` / `heki_semantic` / `heki_ir`）

### 1.1 Python バージョン別スコープ・構文モデル（`heki_semantic`）
`ruff_python_parser` / `ruff_python_ast` は `heki_ast` 内に完全隔離し、スコープ・束縛・モジュール解決は `heki_semantic` で自作する。

- **バージョン依存スコープ規則（`RuntimeProfile` 駆動）**：
  - **3.12+ PEP 709**：`list`/`set`/`dict` 内包表記は外側の関数・モジュールスコープへインライン展開（クラス本体直下など例外ケースを厳密に区別）。
  - **3.14+ PEP 649**：`from __future__ import annotations` なしの型注釈は `__annotate__` 遅延評価スコープとして扱う。
  - **3.15+ PEP 810（実機 CPython 3.15.0 で確認済み）**：`lazy import m` / `lazy from m import x`（`Import(is_lazy=1)` / `ImportFrom(is_lazy=1)`）を認識し、モジュールロード時の即時実行ではなく「当該シンボル名の初回評価時に実体化される束縛」として表現する。
- **CPython `symtable` 双方向突合テスト（QA 基盤）**：
  - `dcg` の `scripts/_source_model.py:verify()` に倣い、スコープ木（種別・行）・束縛名・`global`/`nonlocal`・参照種別（local / enclosing / global / builtin）を実 CPython 3.12〜3.15 の `symtable` と自動突合し、言語仕様との乖離を構造的にゼロに保つ。

### 1.2 `IrFunction` 基本構造
関数（`def` / `async def` / ラムダ / クラス本体 / モジュールトップレベル）ごとに 1 つの `IrFunction` を構築する。ネストクラス内のメソッド（特に `__init__`）は親クラスのメソッド空間と完全に分離する（Stage 0-A `MUT101` 誤検出対策）。

```text
IrFunction {
    id:            FunctionId,
    owner_class:   Option<ClassId>,      // ネストクラスを親クラスと厳密分離
    kind:          Sync | Async | Generator | AsyncGenerator,
    params:        Vec<ParamInfo>,       // 名前・デフォルト有無・PEP 484 型注釈（Advisory レーン専用）
    decorators:    Vec<DecoratorInfo>,   // @cached_property / @lru_cache / @property / CLI / Web ルート / lifespan 等
    blocks:        Vec<BasicBlock>,      // RPO 順に整列可能
    loops:         Vec<NaturalLoop>,     // ヘッダ・ボディ・出口・バックエッジ・LoopBound
    a1_hazards:    Vec<A1Hazard>,        // eval / exec / locals() / sys._getframe 等（スパン付き）
    captured_vars: SmallBitSet,          // ネスト関数・lambda・ジェネレータ式が捕捉する自由変数
}

BasicBlock {
    id:            BlockId,
    phis:          Vec<PhiNode>,
    insts:         Vec<Inst>,            // 各命令は元 AST の TextRange (start..end) を保持
    terminator:    Terminator,
    exc_target:    Option<BlockId>,      // このブロック内で例外発生時の遷移先
}
```

### 1.3 主要命令セット（概要）
複合式はすべて**左から右の評価順序を明示する基本命令列**へフラット化する。

| 命令カテゴリ | 命令例 | 備考 |
|---|---|---|
| 生成・定数 | `Const(Literal)`, `AllocBuiltin(BuiltinKind, args)` | `list`/`dict`/`set`/`tuple`/`bytes`/`str` リテラル・内包表記・隠されていない組み込みコンストラクタ |
| 変数・別名 | `Copy(dst, src)`, `Delete(var)` | `Copy` は直接別名集合への追加、`Delete` は Strong Update による除去 |
| 演算 | `BinOp(op, lhs, rhs)`, `UnaryOp(op, val)`, `AugAssign(op, target, rhs)` | 型が厳密確定するまでは「任意特殊メソッド呼び出し可能」として扱う |
| コンテナ・属性 | `AttrLoad`, `AttrStore`, `AttrDelete`, `SubscriptLoad`, `SubscriptStore`, `SubscriptDelete`, `Contains` | `AttrStore`/`SubscriptStore` は格納値を `Shared` へ降格、`AttrDelete` は `@cached_property` 無効化（`del self.prop`）認識および `MUT201` 書き込み検出に利用 |
| 呼び出し | `Call(callee, args, kwargs)`, `MethodCall(recv, name, args, kwargs)` | `is_awaited: bool` を保持し、`await` 済み呼び出しと同期呼び出しを厳密分離 |
| 反復・ジェネレータ | `MakeIter(dst, obj)`, `IterNext(iter)`, `MakeGenExpr(dst, outer_iter, captured)` | `MakeIter` と `MakeGenExpr` は `obj` / `outer_iter` に対する **派生参照（`derived_refs`）** を明示 SSA 変数 `dst` に束縛 |
| 非同期・中断 | `Await(expr)`, `Yield(expr)`, `YieldFrom(expr)` | `Await` は他タスク干渉あり（`Unique` は不変）。`Yield` は送出値を `Shared` へ降格 |
| コンテキスト | `EnterWith(mgr)`, `ExitWith(mgr, exc)` | `with` / `async with` の例外抑制可能性・ロック／トランザクション区間（`EFF106` 候補）を明示 |
| 制御・例外 | `Branch`, `Jump`, `Return`, `Raise`, `Reraise` | 例外ハンドラ入口は `CatchException(types)` で開始 |

### 1.4 Python 特有構文の展開方針・フォールバック粒度・`LoopBound`
- **`for` / `async for` ループのヘッダ分離**：`for x in expr:` において、**`expr`（`for.iter`）および `it = MakeIter(expr)` はループ入口前ブロック（ループ深度 `d`）に配置**し、ループ本体（深度 `d + 1`）には `IterNext(it)` 以降のみを置く（Stage 0-A で `for.iter` の 1 回のみ実行をループ内誤認した `EFF102`/`EFF103` の 7 件の偽陽性を構造的に根絶）。
- **内包表記・ジェネレータ式**：`RuntimeProfile.pep709_inlined_comprehensions == true`（3.12+）のとき内包表記を外フレーム展開。ジェネレータ式は `MakeGenExpr` で外側イテラブルの派生参照を生成。
- **A1 違反のフォールバック**：`a1_hazards` 非空の関数は **`heki_soundness` のみ対象外**とし、**`heki_effect`（族 1 `MUT`・族 2 `EFF`/`DATA`）は継続**する。
- **`LoopBound`**：`ConstSmall(u32)`（長さ $\le 8$ の定数リテラル / `range(k)`）、`RetryPattern`（定数小回数＋`try`/`except`＋`break`/`return` または backoff `sleep`）、`DaemonPolling`（`while self._running:` / `while True:` の常駐ワーカ／再接続ポーリングループ）、`Unbounded`。Soundness には使わず Profitability および Advisory ノイズ抑制のみに使う。

---

## 2. Advisory / Effect レーン詳細仕様（`heki_effect`）— 族 1 & 族 2

### 2.1 軽量レシーバ型解決（Call-Graph 専用）
型エラー報告は一切行わず、呼び出し先候補・属性対象クラス・書き込み境界ターゲットの解決に特化する。

| 優先度 | 解決ソース | 信頼度 | 例 |
|---|---|---|---|
| 1 | ローカル生成・直接インポート解決 | `Exact` | `requests.get(u)`, `client = httpx.Client(); client.get(u)` |
| 2 | PEP 484 型注釈（引数・変数・戻り値・属性注釈） | `Typed` | `def f(repo: UserRepo, db: Session): db.execute(...)` |
| 3 | クラス `__init__` 等での `self.attr = Cls(...)` 推論 | `Typed` | `self.http = httpx.Client()` → 別メソッドの `self.http.get(...)` |
| 4 | クラス継承階層（CHA / RTA）での同名メソッド解決 | `Heuristic` | 抽象基底クラスからの具象サブクラス解決（候補上限 8 件超は未解決） |

### 2.2 統一 `EffectSummary` の Tarjan SCC ボトムアップ伝播
コールグラフの Tarjan SCC 逆トポロジカル順で Worklist 不動点計算を行い、**Execution Effect（族 2）**、**Mutation & State Effect（`MUT1xx`）**、**Write Ownership / Read-Only Return（`MUT2xx`）** を単一の Summary で同時計算する。

```text
EffectSummary(f) = {
    // 族 2: Execution Effect
    direct_exec_effects:      ExecEffectBitset,
    transitive_exec_effects:  Map<ExecEffect, ShortestCallChain>,

    // 族 1: Mutation & State Effect（MUT1xx / MUT2xx 共通）
    mutates_params:           SmallBitSet,                 // 引数 i 本体または内部要素の変更
    mutated_param_paths:      Map<ParamIdx, Set<AttrPath>>,// 引数 i 経由で変更される属性・コンテナパス（MUT201/MUT202）
    escapes_params:           SmallBitSet,                 // 引数 i の属性・コンテナ格納・グローバル保存・戻り値返却
    reads_self_attrs:         Set<AttrKeyPath>,            // self.<attr> および self.<dict>[const_key] の読み取り
    mutates_self_attrs:       Set<AttrKeyPath>,            // self.<attr> への再代入またはインプレース変更（間接含む）
    invalidates_cached:       Set<AttrName>,               // del self.<prop> / self.__dict__.pop("<prop>", ...) / clear 等
    typed_field_writes:       Set<(ClassId, AttrName, WriteSite)>, // 任意のレシーバ型・属性への直接・間接書込（MUT201）

    // オフロード・同期駆動コンテキスト伝播
    offloads_blocking_params: SmallBitSet,                 // 引数 i の callable を別スレッドへ逃がすラッパーか
    sync_driven_only:         bool,                        // 同期ドライバ経由でのみ呼ばれる内部 async def か
    unknown_calls:            bool,
}
```

### 2.3 族 1（`MUT101` / `MUT103` / `MUT201` / `MUT202`）の判定仕様

#### `MUT101`：`@cached_property` 依存属性の無効化なし書き換え（Stage 0-A 厳格版）
1. クラス `C`（継承階層含む。ネストクラスは別 `ClassId` として厳密分離）の各 `@cached_property` メソッド `p` について、`reads_self_attrs(p)`（`p` が直接および `self.helper()` 経由で読む `self.<attr>` と定数添字 `self.<dict>["k"]` の集合、ただし `p` 自身を除く）を抽出する。
2. `C`（およびサブクラス）の全メソッド `m` のうち、以下を **除外** したものについて `mutates_self_attrs(m)` を抽出する：
   - **初期化・2 段初期化フック**：`__init__`, `__post_init__`, `model_post_init`, `__setstate__`, `__enter__`, `_init_*`, `postinit`, `_configure_*`, `_open`, `setup` 等（かつ `m` が `__init__` から直接呼ばれるプライベート初期化ヘルパーである場合も除外）。
   - **メモ／キャッシュ辞書への追記**：対象属性名が `_cache`/`_memo`/`cache` 系または `setdefault`/`if k not in self._xxx:` 形式の遅延キャッシュ充填である場合を除外。
   - **互いに素な定数 dict キー**：`p` が読むキー集合 `K_read`（例：`self.params["outtmpl"]`）と `m` が書くキー集合 `K_write`（例：`self.params["proxy"]`）がいずれも定数キーかつ `K_read ∩ K_write == ∅` の場合は除外。
3. 残る依存属性 `a ∈ reads_self_attrs(p) ∩ mutates_self_attrs(m)` があり、かつ **`p ∉ invalidates_cached(m)`**（`self.__dict__.clear()` 等の全無効化ヘルパーがない）場合に **`MUT101`** を報告する。

#### `MUT103`：関数間副作用スナップショット（`heki snapshot summary`）
- モジュール公開関数・指定境界関数の `EffectSummary`（`mutates_params`, `escapes_params`, `mutates_self_attrs`, `transitive_exec_effects`）を正規化フォーマットでロックファイルへ保存し、PR 差分で副作用・可変性が単調増加した関数を検出する。

#### `MUT201`：宣言的 Write Ownership（書き込み境界）
`pyproject.toml` に以下のように宣言する（デコレータ不要・実行時オーバーヘッド 0）：

```toml
[[tool.heki.write-owners]]
name = "generation-store"
targets = [
    "datamodel_code_generator.model.base.DataModel.fields",
    "datamodel_code_generator.model.base.DataModel.base_classes",
    "datamodel_code_generator.types.DataType.data_types",
]
allowed_writers = [
    "datamodel_code_generator.parser.generation.*",
    "datamodel_code_generator.model.base.DataModel.__init__",
]
# 任意：指定クラス上での @cached_property や __dict__ 直接操作も禁止可能
forbid_cached_property_on = ["datamodel_code_generator.model.base.DataModel"]
```

- **解析アルゴリズム**（`dcg` の `check_generation_store_usage.py` の弱点＝ローカル別名漏れ・間接 helper 漏れ 13 件を SSA-CFG ＋ Call Graph で解消）：
  1. 対象クラス `C`（およびサブクラス）の属性 `a` に対する以下すべての操作を **Write 起点** とする：
     - 属性再代入・削除・拡張代入（`obj.a = ...`, `del obj.a`, `obj.a += ...`）
     - `setattr(obj, "a", ...)` / `object.__setattr__(obj, "a", ...)` / `obj.__dict__["a"] = ...`
     - **別名経由コンテナ破壊**：`xs = obj.a; xs.append(...)` / `xs[i] = ...` / `xs.clear()` / `xs.sort()` / `xs |= ...`（`heki_effect` 内の関数内 alias 追跡と `mutated_param_paths` により、**別関数 `helper(obj.a)` に渡して先で `.append()` するケースも推移的に捕捉**）。
  2. `EffectSummary.typed_field_writes` および `mutated_param_paths` を Tarjan SCC でボトムアップ伝播し、書き込み起点から到達元（`allowed_writers` に含まれない関数・モジュール）へ違反を報告する。
  3. 既存コードベースの段階移行には **`heki check --baseline`** を併用し、`(path, qualname, "MUT201", "Class.attr")` キーで件数凍結・片道縮小を行う。

#### `MUT202`：宣言的 Read-Only Return（共有オブジェクト不変性）
`pyproject.toml` に以下のように宣言する：

```toml
[[tool.heki.readonly-returns]]
name = "shared-schema-graph"
producers = [
    "datamodel_code_generator.parser._schema_graph.SchemaGraphBuilder.schema",
    "datamodel_code_generator.parser._schema_documents.load_document",
]
deep = true   # 配下の属性・ネストコンテナへの変更も禁止
```

- **解析アルゴリズム**：
  1. `producers` に一致する呼び出しの戻り値 SSA 変数に `ReadOnlyOrigin(producer_name)` タグを付与する。
  2. タグは `Copy`、属性読み取り（`deep = true` 時）、添字読み取り、タプル／リスト展開を通じて関数内に伝播する（`deepcopy(...)` / `.copy()` / `model_copy(deep=True)` 等の Layer 1/2 複製 API で遮断）。
  3. タグ付き値に対して直接の属性書換・コンテナ破壊が行われるか、`mutates_params` に該当する引数位置へ渡された場合に **`MUT202`**（＋最短 `call_chain`）を報告する。

### 2.4 族 2（`EFF101`〜`103`, `EFF105`, `DATA101`）の精度・サニタイザ・重複集約
1. **`OffloadsBlocking` のラッパー伝播（dcg `bodies.py:112` 対策）**：
   - 関数 `w(fn, *args)` が内部で `asyncio.to_thread(fn, ...)`（`offloads_blocking_arg = 0`）等を呼んでいる場合、`w` の `EffectSummary.offloads_blocking_params` に `fn` の引数インデックスを記録し上位へ伝播。`await in_thread(self.open)` 等のブロッキング伝播を遮断する。
2. **Sync-over-Async パターンの緩和（dcg `client.py:1399` 対策）**：
   - 同期ドライバ経由でのみ駆動され通常イベントループから `await` されない内部 `async def` は `sync_driven_only = true` として緩和。
3. **Stage 0-A 確定フィルタ（4 点）**：
   - **`EFF101`**：`is_awaited == true` の呼び出しを除外、`subprocess.Popen(...)` 本体（非ブロッキング spawn）を除外（`.wait()`/`.communicate()` はブロッキング維持）。
   - **`EFF102`**：`for.iter` ヘッダを除外、`DaemonPolling` ループ内のクライアント再接続を緩和、ローカル `dict` 変数名 `requests` 等を型／名前解決で誤認識しない。
   - **`EFF103`**：`for.iter` ヘッダを除外、SQL 文字列リテラルが DDL（`CREATE`/`ALTER`/`DROP`）やマイグレーション関数内のものを除外、ORM ライブラリ自身の内部実装を除外。
   - **`DATA101`**：`df = pd.concat([df, ...])` や `arr = np.vstack([arr, ...])` のように **SSA の `PhiNode` でループバックエッジ経由の自己累積（`LoopCarried`）になっているもののみ** を検出し、ループ内での単発結合・パディングを除外する。
4. **重複集約（Boundary-Only Reporting + Root-Cause Grouping）**：
   - `async A → await async B → sync C → blocking D` では同期世界への最初の越境点（`async B`）のみ報告。同一原因への到達元が閾値（既定 10 件）を超えた場合は代表＋到達元件数へ集約。
5. **コンテキスト緩和**：`click.command` / `typer.command`、`__main__` の `asyncio.run` 直下、FastAPI `lifespan` 起動フック、`LoopBound::ConstSmall` / `RetryPattern`、テストコードは既定で抑止・緩和。

### 2.5 Layer 3 オンデマンド外部解析の境界とキャッシュキー
- **適用境界（厳守）**：**標準ライブラリ全体および Layer 2 登録済みパッケージ内部へは絶対に潜らない**（A.7：`asyncio.StreamWriter.write` → `socket.send` 誤爆防止）。
- **打ち切り上限**：探索深さ最大 3 ホップ、1 モジュール最大 10,000 行、外部解析合計上限 200 モジュール。
- **ディスクキャッシュキー**：`(heki_version, effective_kb_digest, env_packages_digest, package_name, package_version, rel_path, blake3_file_hash)`。

---

## 3. Soundness / Alias レーン詳細仕様（`heki_soundness`）— 族 3

### 3.1 Provenance 付き抽象ドメイン（複数 `alloc_id`・派生参照対応）
```text
const MAX_JOIN_ALLOC_IDS: usize = 4;

enum ExactTypeVal {
    Exact { ty: BuiltinType, origin: Origin },
    Unknown { reason: TypeUnknownReason },
}

enum UniqueState {
    Unique {
        alloc_ids: SmallVec<[AllocId; 2]>,   // CFG 合流で上限 4 超過時は Shared 降格
        origins:   SmallVec<[Origin; 2]>,
    },
    Shared {
        witness:   EscapeWitness,
    },
}

struct FrameState {
    var_type:     Map<VarId, ExactTypeVal>,
    var_unique:   Map<VarId, UniqueState>,
    alias_sets:   Map<AllocId, SmallBitSet<VarId>>,
    derived_refs: Map<AllocId, SmallBitSet<VarId>>,   // MakeIter / MakeGenExpr / retained_in_return
    derives_from: Map<VarId, SmallVec<[AllocId; 2]>>,
}
```

### 3.2 遷移規則・派生参照・合流・`ValueLoopInvariant`・A2 用 `SoundnessSummary`
- **Strong Update**：再束縛・`Delete(x)` は全 `alias_sets` / `derived_refs` から `x` を除去。
- **派生参照の生成と推移的エスケープ**：`MakeIter(r, x)` / `MakeGenExpr(r, x, ...)` / Layer 1 の `retained_in_return` は `r` を `x` の全 `alloc_id` の `derived_refs` へ追加（連鎖も推移的に引き継ぐ）。`r` がエスケープした場合は、元の全 `alloc_id` の全 `alias_sets` も `Shared { witness: DerivedRefEscapedAt(span) }` へ降格する。
- **CFG 合流**：一方が `Shared` なら `Shared`。双方が `Unique` なら `alloc_ids` 和集合（`|Us| <= 4` なら維持、超過なら `Shared { witness: JoinAllocOverflow }`）。
- **`ValueLoopInvariant(v, loop)`**：`BindingLoopInvariant` に加え、不変組み込み型（`tuple` は要素もハッシュ整合組み込み型）、または可変組み込み型がループ全体を通じて `Unique` かつループ内に該当 `alloc_id`（別名・派生参照含む）への変更操作が一切ないこと（S27〜S28 対策）。
- **保守的 `SoundnessSummary`（A2・v1.0 用）**：`heki_effect` に依存せず、`heki_soundness` 内で静的束縛関数（モジュール直下 1 回定義・デコレータなし・モック対象外）のみを対象に計算する。

### 3.3 族 3 ルール別 Proof Obligation マトリクス（Safety × Profitability 分離）

| Rule | Profitability | Safety Proof Obligation（意味保存） | Profitability 条件 | 主な反例 |
|---|---|---|---|---|
| **OPT101** | `unconditional` | `Unique(x)`, `ExclusiveAccessAtWrite(x)`（全別名・全派生参照 Dead）, `ExactType(x/rhs, list)`, `ExceptionStateSafe(x)`, `EvalOrderPreserved` | `LoopCarried(x)` | S1〜S5, S14, S23〜S25 |
| **OPT102** | `unconditional` | `Unique(target)`, `ExclusiveAccessAtWrite(target)`, `ExactType(target/rhs, set\|dict)`, **辞書アンパック時は累積変数 `d` が先頭（`{**d, ...}`）であること**, `ExceptionStateSafe`, `EvalOrderPreserved` | `LoopCarried(target)` | S26, S26b, **dcg `_target_binding.py:465`（`{**other, **found}` 右側累積は上書き優先順が逆転）** |
| **OPT103** | `size-dependent` | `Unique(acc)`, `ExclusiveAccessAtWrite(acc)`, `ExactType(acc/rhs)`, ループ内他参照なし, `ExceptionStateSafe` | `LoopCarried(acc)`, `NotConstSmall`（ローカル `str +=` 除外） | A.8（小反復劣化）, `self.s +=`, 右辺 `__radd__` |
| **OPT110** | `size-dependent` | `ValueLoopInvariant(lst, loop)`, `ExactType(lst, list\|tuple)`, ハッシュ整合組み込み型要素, `ZeroTripExceptionSafe` | `NotConstSmall(lst)` | S9a, S9b, S22, S27 |
| **OPT111** | `size-dependent` | `Unique(seen)`, `ExclusiveAccessAtWrite`, `ExactType(seen, list)`, ハッシュ整合組み込み型要素, 削除・置換なし | `NotConstSmall` | `seen.remove`, 非ハッシュ要素 |
| **OPT120** / **OPT121** | `size-dependent` | `Unique(q)`（非エスケープ・非返却）, `ExactType(q, list)`, `deque` 等価操作集合内 | `NotConstSmall`, **両端（`0`, `-1`）以外の任意添字 `q[i]` なし** | S12, `sort()`, 中間添字 $O(n)$ 劣化 |
| **OPT130** | `size-dependent` | 全引数 `ValueLoopInvariant`, 副作用なし, `ZeroTripExceptionSafe`, 生成コンテナ変更なし | `NotConstSmall`（$m=0$ 回ループで 100 倍劣化: A.8） | S22, S28, A.8 |

---

## 4. 3 層 KB・Ratchet ベースライン・任意ダンプ連携仕様

### 4.1 Layer 1（`kb/soundness/*.toml` — Soundness 完全専用）
Advisory 用の `effects` フィールドは一切持たず、`return_kind` / `param_effects`（`read_only`, `mutated`, `retained_in_return`, `escaped`）/ `dunders_invoked` / `verified_max_python` のみを保持する。`verified_max_python` 超過時は起動時射影で自動 `Unknown` 降格。

### 4.2 Layer 2（`kb/ecosystem/*.toml` — 標準ライブラリ公開 API ＋ 主要 OSS）
すべての Execution Effect 起点、`async_safe = true`（例：`asyncio.StreamWriter.write`）、`offloads_blocking_arg`（例：`asyncio.to_thread`）、`excluded = true`（例：`logging.Logger.*`）、`opaque_to_layer3 = true`、複製サニタイザ（`copy.deepcopy`, `BaseModel.model_copy`：`MUT202` 用）を一元定義する。プロジェクト側からのカスタム追加（`pyproject.toml` `[tool.heki.ecosystem]`）に対応。

### 4.3 行番号非依存 Ratchet ベースライン仕様（`heki_diagnostics`）
- **キー構造**：`BaselineKey = (relative_posix_path, qualname, rule_id, normalized_target)` → `count: u32`。行番号を持たないため、無関係な編集・リフォーマットでベースラインが壊れない。
- **検証ルール（片道縮小）**：
  - 現在の検出件数がベースラインを **上回る** キーがある → **FAIL**（新規違反の混入）。
  - 現在の検出件数がベースラインを **下回る**（またはエントリが消滅した） → **stale baseline 通知**（`--strict-ratchet` 時は FAIL とし `heki check --update-baseline` で縮小更新を要求）。

### 4.4 任意プロファイルダンプ読込仕様（`heki_profile`）
- **オフライン読込専用（実行介入なし）**：`py-spy` の `--collapsed` テキスト、CPython 3.15 Tachyon の `--gecko` JSON / `--pstats` バイナリ、`cProfile` の `pstats` を統一の `(file, line, func, self_pct, cum_pct)` テーブルへ読み込む。
- **実機確認済みの制約**：CPython 3.15 Tachyon は `--opcodes` と `--jsonl` の併用不可（`--gecko` / `--binary` 等が必要）、かつ macOS では外部メモリ読取りに `sudo` が必要。そのためプロファイル連携はコア必須要件にはせず、チューニング時の任意アドオンに限定する。

---

## 5. Stage 0-A 実測結果サマリ（完了実績：2026-11-19）

339 リポジトリ（成熟 OSS 118 リポジトリ / 1,242 万行 ＋ 2026 年新規 GitHub 221 リポジトリ / 568 万行、合計 56,629 ファイル / 18,108,143 行）を 8 並列スキャン（62.8 秒）し、全 8 ルール・計 171 サンプルを目視精査した結果：

| Rule | 対象ドメイン別ヒット率 | 間接呼び出し比率 | 171 件目視監査の結論 | Baseline v4.0 での処置 |
|---|---|---|---|---|
| **EFF101** | Web/Async **37.6%** (35/93) / DB 29.1% | **82.3%** (436/530) | **68% 実害**（`redis-py` `cluster.py:1613` の実 `time.sleep`、4〜5 ホップ到達含む） | **主力採用**（`await` 済み・`Popen` 除外を実装） |
| **EFF102** | Web/Async **44.1%** (41/93) / DB **48.5%** (50/103) | **90〜93%** | `for.iter` 修正後 **~75〜80% 実害**（`NeMo`・`OmniAgent`・`luigi` 等） | **主力採用**（`for.iter`・`DaemonPolling` 対応） |
| **EFF103** | DB **47.6%** (49/103) / Web 31.2% | **74.2%** (630/849) | N+1 多数。DDL・マイグレーション・ORM 内部がノイズ源 | **主力採用**（`for.iter`・DDL・ORM 内部除外） |
| **DATA101** | Data/ML **19.5%** (24/123) | — | 素朴 AST の 60% FP は全件非 `LoopCarried` パディング。`LoopCarried` 適用で **~90% 精度** | **主力採用**（SSA `LoopCarried` 必須） |
| **MUT101** | 成熟 OSS **16.1%** (19/118) / 新規 0.9% | — | `dcg` 0.86.0 実バグ・`mkdocs`・`wagtail`・`textual` 等に的中。素朴版の ~64% ノイズは 3 原因に集約 | **厳格版で採用**（2 段初期化・メモ dict・互いに素な dict キー除外） |
| **MUT102a/b** | 成熟 OSS 4.2% / 新規 0.5〜1.4% | — | 低密度かつ大半が意図的イディオム（ゼロコピー `Sequence`・`frozen` 蓄積 list） | **実測により不採用**（代わりに dcg 由来の **`MUT201`/`MUT202`** を採用） |
| **DATA102** | Data/ML 12.2% / 成熟 OSS 2.5% | — | 単一式構文チェックであり Ruff / pandas-vet と重複 | **不採用** |

*(次期候補ルール `MUT104`–`107`, `EFF106`–`108`, `OPT112`–`113`, `OPT131` も、v1.0 組込み前に同一のコーパス計測・目視精査プロトコルを通して採否を決定する。)*
