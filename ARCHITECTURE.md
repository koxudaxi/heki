# heki アーキテクチャ設計書（Baseline v4.1）

| 項目 | 内容 |
|---|---|
| ステータス | Architecture Baseline v4.1（正式名称 `heki` 確定・Stage 0-A Wave 1/2 合計 1,900 万行実測・`dcg` 2 万行チェッカー調査・Ruff/`t-linter` 統一 CLI/Config・増分 LSP 設計・`ruff_python_parser 0.17.5` PEP 810 実機確認済み） |
| 日付 | 2026-11-19 |
| 対象言語 | Python（検証対象：CPython 3.11–3.15、PEP 709 / PEP 649 `__annotate__` / PEP 810 `lazy import` 対応） |
| 実装言語 | Rust（単一バイナリ CLI ＋ 増分 LSP、`maturin` 配布。フロントエンドのみ `ruff_python_parser` / `ruff_python_ast` `tag = "0.17.5"` を `heki_ast` へ隔離利用） |
| 関連ドキュメント | [実装計画・引き継ぎ](docs/IMPLEMENTATION_PLAN.md) / [検討経緯 & ADR](docs/DESIGN_HISTORY_AND_ADR.md) / [解析エンジン・IR・KB 詳細仕様](docs/ANALYSIS_AND_KB_SPEC.md) |

---

## 1. プロダクト定義とポジショニング

**heki（壁）**は、Python 静的解析エコシステムにおける **「第 3 の層：Effect（実行・状態変更）＆ Alias（一意性・所有境界）意味論チェッカー」** である。

| 層 | 代表ツール | 単一の統一理論 | 守備範囲と限界 |
|---|---|---|---|
| **第 1 層：構文・局所リンター** | Ruff, Flake8 | AST ＋ 単一ファイルスコープ | スタイル・構文バグ。関数跨ぎ・別名・副作用・状態変更・CFG を扱わない |
| **第 2 層：型チェッカー** | ty, mypy, Pyright, Pyrefly | 型システム（Type Lattice） | 型の整合性。別名・エスケープ・書き込み所有境界・I/O Effect は扱わない |
| **第 3 層：Effect & Alias 意味論チェッカー（本プロジェクト）** | **heki** | **SSA-CFG ＋ コールグラフ上の「Effect（実行・状態変更）＆ Alias（一意性・参照境界）」** | **関数・クラス境界を越える状態破壊・書き込み所有違反・キャッシュ不整合・ブロッキング・$O(n^2)$ 計算量劣化** |

*(モジュール層間 import 依存・循環 import 検査は `tach` / `import-linter` および Python 3.15+ PEP 810 `lazy import` の領域とし、本ツールは Effect & Alias へ純化する。)*

### 1.1 プロダクトの建付け：「意味論チェックが主価値、高速化はその自然な副作用」

- **主価値（成熟プロダクションから Web/AI コードまで効く）**：Ruff（構文）と ty（型）が原理的に見逃す **関数・クラス境界を跨いだ状態破壊・書き込み所有境界違反・共有オブジェクト汚染・キャッシュ不整合・ブロッキング I/O・接続再生成** を、独自デコレータなし（標準 `typing` ＋ Zero-Config 推論 ＋ `pyproject.toml` 宣言境界 ＋ Ratchet ベースライン）で CI 防御する。
- **副産物としての高速化（該当コードには強烈に効く）**：同じ Effect & Alias 解析の自然な帰結として、① イベントループ阻害・ループ内接続再生成・ループ内 DB クエリ・二乗 DataFrame 結合の排除による本番性能改善、② 一意性・派生参照・厳密型が証明できた $O(n^2)$ 計算量劣化の安全自動修正・検証済み提案、が得られる。
- **単一の統一理論・場当たりゼロ**：個別フレームワーク向けの勘ルール（`__getattr__` プロキシ・Pydantic モデル検証・`importtime` 等の場当たりヒューリスティック）は一切混ぜず、単一の Effect & Alias 理論から公理的に導出できる **3 つのルール族** のみで構成する。

### 1.2 単一 IR 上の 2 レーン分離と 3 つのルール族

共通のフロントエンドおよび SSA-CFG IR の上に、**性質の異なる 2 つの解析レーン** をクレート境界で物理分離し、3 つのルール族を載せる。

1. **Advisory / Effect レーン（`heki_effect`）** — 標準 `typing`・`self` 属性・軽量クラス階層を使うコールグラフ解析
   - **族 1：Mutation & State Effect（`MUT`）**：
     - 自動推論系（`MUT1xx`）：`@cached_property` 等のキャッシュ依存属性の無効化なし書き換え、関数間副作用スナップショット CI 検知（＋ 候補：キャッシュ／定数汚染、反復中変更、`Iterator` 二重消費、`__hash__` 依存変更）。
     - 宣言的境界系（`MUT2xx`）：`pyproject.toml` で宣言した **Write Ownership（書き込み境界）** と **Read-Only Return（共有オブジェクト不変性）** の関数跨ぎ・別名経由違反検知。
   - **族 2：Execution Effect（`EFF` / `DATA`）**：`async` イベントループ阻害、ループ内クライアント・接続プール再生成、明示的ループ内 DB クエリ、ループキャリー `pd.concat`/`np.vstack` 二乗累積（＋ 候補：ロック保持中 `await`、非エスケープ `create_task`、リソースリーク）。
2. **Soundness / Alias レーン（`heki_soundness`）** — 型注釈を信用しない保守的抽象解釈（誤変換ゼロ）
   - **族 3：Complexity Rewrite（`OPT`）**：一意性（フレーム性）・派生参照・厳密型・オブジェクト内容不変性に基づく意味保存な計算量改善（$O(n^2) \to O(n)$）。

> **なぜ 2 レーンをクレート境界で分離するか**
> 自動書き換え（Soundness）は型注釈を信用してはならず（サブクラス反例）、不明なら必ず安全側（`Unknown`）へ倒す保守的解析が必須である。一方、関数間 Effect / Mutation 検出（Advisory）では名前解決だけで候補の約 6〜7 割を拾えるものの、残り 3〜4 割（メソッド呼び出し経由の同期 DB セッションや書き込み境界・クラス内状態変更等）を捉えるために PEP 484 型注釈・`self` 属性・クラス階層を使う Best-effort 解決が必要になる。`heki_soundness` が `heki_effect` へ依存できないことを Rust のクレート境界でコンパイル時に強制する。

---

## 2. Goals / Non-Goals

### Goals
- **G1. Zero-Config・コード無汚染**：独自 import・独自デコレータ・独自型注釈不要。標準 `typing` と推論（＋ 任意で `pyproject.toml` の境界宣言）のみで動作し、実行時オーバーヘッド完全ゼロ。
- **G2. 単一の統一理論・場当たりゼロ**：すべての診断を SSA-CFG ＋ コールグラフ上の Effect & Alias 理論から導出し、個別フレームワークの勘ルールを排する。
- **G3. 論理的根拠と誠実な安全性・収益性区分**：書き換えの「意味保存（Safety）」と「性能改善の確実性（Profitability）」を直交管理し、根拠を示せないもの・小サイズで逆効果になり得るものを既定で自動適用しない。
- **G4. 段階導入できる実戦 UX（Ratchet ＆ 機械可読出力）**：行番号非依存の片道縮小ベースライン（`heki check --baseline`）と、成立根拠（`evidence`）・妨害要因（`blocker`）・呼び出し経路（`call_chain`）を含む JSON / SARIF 出力を標準装備し、大規模既存リポジトリへの導入と人間／エージェント双方の CI 修正ループに対応する。
- **G5. 既存ツールとの完全共存**：Ruff・ty・mypy・Pyright・tach と役割を重ねず、`# noqa` 互換で併用可能。

### Non-Goals（明示的に除外）
- ユーザーコードへの独自契約 API（`Owned`, `Borrowed`, `move()`, `@pure` 等）の導入
- Lean 4 等の定理証明支援系の導入
- 定数倍マイクロ最適化の推奨（`re.compile` 巻き上げ等も含む）
- **汎用的な層間 import 境界・import cycle 検査**（`tach` / `import-linter` および Python 3.15+ PEP 810 `lazy import` に委譲し、本ツールは Effect & Alias に集中する）
- **単一式構文リンター・デッドコード／コードクローン検出**（`df.iterrows()`・未使用変数・重複関数等は Ruff / `vulture` / CodeQL に委譲）
- **AOT ネイティブ拡張（mypyc / C / Rust 拡張ブリッジ）およびビルド時コード書き換え**（CLI 起動時の `.so` ロード・互換性オーバーヘッドにより逆効果、Pure Python 維持が大前提）
- **個別フレームワークの場当たりヒューリスティック**（`__getattr__` プロキシ・Pydantic モデル構築・`-X importtime` 最適化等のアーキテクチャ設計領域）
- **プロファイル実行の強制・プロセス介入**（外部プロファイルダンプ読込は任意アドオンにとどめる）
- CPython・バイトコード・VM の変更、実行時フック、Ruff `unsafe fix` 判定器としての従属設計、汎用型チェッカー

---

## 3. 意味論・安全性・収益性の定義

### 3.1 観測モデル（Observational Equivalence）
Soundness レーンにおける書き換え前後で、以下 4 点が一致すれば意味保存とみなす。

1. **関数の結果**：戻り値（等価かつ対応するオブジェクト構造）、または送出される例外の**型**。
2. **外部呼び出しのトレース**：解析範囲外／未知コードへの呼び出し順序と、呼び出し時点で引数・グローバルから到達可能なオブジェクトの状態。
3. **共有状態への変更**：`Shared` オブジェクトに対する破壊的変更の順序と内容。
4. **停止性**：正常終了・例外終了・無限ループの別。

**観測対象外**（言語仕様上も実装依存）：実行時間、メモリ消費量、確保回数、参照カウント、`id()` の数値、トレースバックの行番号、組み込み例外のメッセージ文字列（例外型は保存）。

### 3.2 前提（Assumptions: A1〜A5）

| ID | 名称 | 内容 | ツールの扱い |
|---|---|---|---|
| **A1** | 基本前提 | (a) フレーム覗き見によるローカル変数の外部参照・改変を行わない（`sys._getframe`, `f_locals`, トレース／監視フック, `gc.get_objects`, ジェネレータ `gi_frame` の外部操作）。(b) 組み込み名は解析範囲内で隠されていなければ本物を指す。(c) 観測対象外に依存しない。(d) `MemoryError`・再帰上限・シグナル割り込み位置は保存しない | **Soundness レーンで常に仮定。** 関数内に `locals()`, `eval()`, `exec()`, `sys._getframe` 等があれば、**その関数の Soundness レーンのみ無効化**（Advisory レーンは継続）。プロジェクト全体の違反箇所を `heki assumptions` で列挙 |
| **A2** | モジュール束縛の静的性 | モジュール直下で 1 回定義された関数・クラスは実行時に差し替えられない | `heki_soundness` 内の保守的 `SoundnessSummary` を使う関数間書き換えに必要。既定の自動適用では仮定しない。テストコード等でモック（`mock.patch` / `monkeypatch` 等）の差し替え対象となっているシンボルには **A2 を適用しない** |
| **A3** | ネイティブ KB の正しさ | Layer 1 内蔵 C 拡張・stdlib プリミティブ仕様が実挙動と一致する | 番兵テストで検証。`verified_max_version` 超過時は Soundness レーンで自動 `Unknown` 降格 |
| **A4** | 宣言型注釈の厳密性 | `x: list[int]` 等がサブクラスではなく厳密な組み込み型である | 既定では仮定しない（Advisory レーンの型解決・`MUT` 検査とは無関係） |
| **A5** | 終了処理時期の非依存 | `__del__` や弱参照コールバックの発火時期に依存しない | 要素の寿命を変える規則のみ要求 |

### 3.3 安全性（Safety）と収益性（Profitability）の直交モデル

自動書き換え（族 3：`OPT`）では、**意味保存（Safety Obligation）** と **性能改善の確実性（Profitability：`LoopCarried`, `NotConstSmall`, サイズ依存性等）** を直交する 2 軸で判定する。

- **Safety 区分**：`safe`（全 Safety Obligation が `Established`、前提 A1 のみ）／ `conditional`（A2〜A5 を含む）／ `blocked`（未確定あり、`blocker` 出力）／ `advisory`（族 1・族 2 の意味論警告、自動書換なし）／ `disproved`（反例あり、診断抑止）。
- **Profitability 区分**：
  - `unconditional`：入力サイズ $n=1$（および反復回数 0）から常に改善（**`OPT101`, `OPT102` のみ**）。
  - `size-dependent`：小サイズ入力や 0 回ループでは初期構築・変換コストが勝る（**`OPT103`, `OPT110`, `OPT111`, `OPT120`, `OPT121`, `OPT130`** 等）。
  - `semantic-advisory`：状態破壊・書き込み境界違反・キャッシュ不整合・ブロッキング・ループ内接続等の意味論診断（族 1・族 2）。

#### 自動適用ルール（`heki check --fix`）と現実的期待値
- 既定の `heki check --fix` は **`safety == safe` かつ `profitability == unconditional`（`OPT101`, `OPT102`）のみ** を自動適用する。
- `size-dependent` な `safe` 修正は既定では **意味保存が証明された提案** として提示し、任意で外部プロファイルダンプ（`--profile`）によりホット確認された場合または `--include-size-dependent` 指定時のみ自動適用する。
- 実測上、既定 `heki check --fix` の適用件数は極めて少ない（成熟 1,320 万行で証明可能見込み 20 件、新規 650 万行で 1 件、dcg 19.6 万行で 0 件）。本ツールの日常 CI 価値の中心は **族 1（Mutation & State / Write Ownership）＋ 族 2（Execution Effect）＋ 族 3 の検証済み提案** にある。

---

## 4. 解析理論とエンジン設計

### 4.1 共通基盤：SSA-CFG IR（`heki_ir`）と意味解析（`heki_semantic`）
パーサー（`ruff_python_parser`）から自作のスコープ・束縛・クラス階層解決（`heki_semantic`）を経て、関数ごとに SSA-CFG IR を構築する。

- **最新 Python 構文とスコープ規則の完全対応**：
  - Python 3.12+ PEP 709（内包表記のインライン化）、3.14+ PEP 649（`__annotate__` スコープ）、**3.15+ PEP 810（`lazy import` / `lazy from ... import ...`, `is_lazy=1`：モジュールロード時ではなく初回評価時に実体化）** を `RuntimeProfile` とともに厳密モデル化する。
- **例外エッジ・評価順序明示**：例外送出可能命令からの暗黙制御辺と、左から右の評価順序を明示する。
- **構文展開**：`for`（匿名イテレータを明示 SSA 変数へ束縛する `MakeIter` + `IterNext`。**`for.iter` ヘッダ式はループ本体外として明示分離**）、`while`、`with`/`async with`、`try`/`except`/`else`/`finally`、内包表記、ジェネレータ式（`MakeGenExpr`）、拡張代入、アンパック、`match` を基本命令へ展開。
- **`await` / `yield` の第一級命令化**：非同期中断点とジェネレータ中断点を明示し、`await` 済み呼び出し（非同期メソッド）と同期呼び出しを厳密区別する。
- **クラス・デコレータ・ループ情報**：自然ループ＋`LoopBound`（`ConstSmall(k)` / `RetryPattern` / `DaemonPolling` / `Unbounded`）、ネストクラスのスコープ完全分離、`@cached_property`・`@lru_cache`・CLI/Web デコレータ等のメタデータ。
- **元 AST バイトスパン保持**：修正は IR 逆生成せず最小 `TextEdit` で生成し書式・コメントを維持。

### 4.2 Advisory / Effect レーン（`heki_effect`）— 族 1 & 族 2

#### 軽量コールグラフ型解決（Call-Graph 専用）
型エラー報告は行わず、呼び出し先候補・属性読み書き先・書き込み境界ターゲットの解決のみを行う（優先順：① ローカル生成・直接インポート → ② PEP 484 型注釈 → ③ `__init__` 等での `self.attr` 代入推論 → ④ クラス継承階層 CHA/RTA、候補上限 8 件）。

#### 統一 `EffectSummary` の Tarjan SCC ボトムアップ伝播
プロジェクト内（および必要時の Layer 3 オンデマンド外部モジュール）に対し、コールグラフの Tarjan SCC 逆トポロジカル順で Worklist 不動点計算を行い、**Execution Effect・Mutation & State Effect・Write Ownership** を単一の Summary で同時伝播させる。

```text
EffectSummary(f) = {
    // 族 2: Execution Effect
    direct_exec_effects:      ExecEffectBitset,    // BlockingNet, BlockingDB, BlockingSubprocess,
                                                   // BlockingSync, Sleep, BlockingFS,
                                                   // UnpooledClientAlloc, DBQuery, QuadraticRealloc, ...
    transitive_exec_effects:  Map<ExecEffect, ShortestCallChain>,

    // 族 1: Mutation & State Effect（MUT1xx / MUT2xx 共通）
    mutates_params:           SmallBitSet,         // 引数 i（またはその属性・内部要素）を変更するか
    mutated_param_paths:      Map<ParamIdx, Set<AccessPath>>, // 引数 i 上で書換・コンテナ破壊される属性/添字パス
    escapes_params:           SmallBitSet,         // 引数 i を属性・コンテナ・戻り値へ保持／流出させるか
    reads_self_attrs:         Set<AttrKeyPath>,    // self.<attr>（dict 定数キー含む）の読み取り
    mutates_self_attrs:       Set<AttrKeyPath>,    // self.<attr> の再代入またはインプレース変更
    invalidates_cached:       Set<AttrName>,       // del self.<prop> / self.__dict__.pop("<prop>", ...) 等
    typed_field_writes:       Set<(ClassId, AttrName, WriteKind)>, // 任意の型・属性への直接・間接書込（MUT201）
    mutates_return_of:        Set<CallSite>,       // 共有ファクトリ戻り値の別名経由破壊（MUT202）

    // オフロード・非同期コンテキスト伝播
    offloads_blocking_params: SmallBitSet,         // 引数 i の callable を別スレッドへ逃がすラッパーか
    sync_driven_only:         bool,                // run_sync(coro) 等の同期ドライバ経由でのみ駆動されるか
    unknown_calls:            bool,
}
```

#### 実コード検証（Stage 0-A 339 リポジトリ 1,810 万行 ＋ dcg 19.6 万行）に基づく精度・ノイズ制御
1. **`OffloadsBlocking` のユーザーラッパー伝播**：`asyncio.to_thread` 等に加え、`await in_thread(self.open)` のようなプロジェクト内薄いオフロードラッパーにも `offloads_blocking_params` をボトムアップ伝播（dcg `bodies.py:112` 実測知見）。
2. **Sync-over-Async パターンの緩和**：同期ドライバ経由でのみ駆動される内部 `async def` は `EFF101`/`EFF105` を緩和（dcg `client.py:1399` 実測知見）。
3. **Stage 0-A 目視精査で確定した 6 大誤検出対策**：
   - `for.iter` / `async for.iter` ヘッダはループ外扱い（`EFF102`/`EFF103` の 1 回のみ実行を除外）。
   - `await` 済み呼び出し（例：`await redis.delete(...)`）および非ブロッキング spawn（`subprocess.Popen` 本体。ただし `.wait()`/`.communicate()` はブロッキング）を `EFF101` から厳密除外。
   - `EFF103` では DDL（`CREATE TABLE`/`CREATE INDEX`）・マイグレーション補助・ORM 内部実装を除外。
   - `EFF102` では常駐デーモンのポーリングループ（`while self._running:` 等の再接続ループ）を識別・緩和。
   - `DATA101` は SSA の **`LoopCarried`**（再代入による累積）を必須とし、ループ内での単発結合・パディングを除外。
   - `MUT101` は ① 2 段初期化フック（`__init__`/`__post_init__`/`model_post_init`/`_init_*`/`_configure_*`/`_open` 等）、② メモ辞書（`_cache[k] = v` 等）、③ 互いに素な定数 dict キー（`params["proxy"]` vs `params["outtmpl"]`）、④ ネストクラスの別 `__init__` を厳密除外。
4. **`EFF101` の段階化と重複集約**：既定は `BlockingNet`/`BlockingDB`/`BlockingSubprocess`/`BlockingSync`/`Sleep`、`BlockingFS` はオプトイン `EFF105`、`logging`/`print` 除外。`async` 連鎖は境界のみ報告＋原因別集約。単発 CLI・起動フック・`ConstSmall`/`RetryPattern`・テストコードは既定緩和。

### 4.3 Soundness / Alias レーン（`heki_soundness`）— 族 3

Datalog は採用せず（[ADR-006](docs/DESIGN_HISTORY_AND_ADR.md#adr-006)）、SSA-CFG 上で **Provenance 付き Lattice の RPO Worklist 抽象解釈** と **宣言的 Proof Obligation（3 値：`Established` / `Blocked` / `Disproved`）** を実行する。

- **一意性（Uniqueness）・派生参照（`derived_refs`）・フレーム性**：
  - `for v in x:` の暗黙イテレータ（`MakeIter`）、ジェネレータ式（`MakeGenExpr`）、Layer 1 の `retained_in_return`（`iter`, `enumerate`, `zip`, `map`, `filter`, `reversed`, `dict.keys/values/items`, `memoryview` 等）を元オブジェクトの `derived_refs` として追跡。派生参照のエスケープは元オブジェクトを推移的に `Shared` へ降格させる。
  - 書き換え地点では `x` 以外の全直接別名・全派生参照が Dead であること（`ExclusiveAccessAtWrite`）を必須とする（S23〜S26b 対策）。
  - 異なる `alloc_id` の CFG 合流は `SmallVec<[AllocId; 2]>`（上限 4、超過時 `Shared`）で確定。
- **オブジェクト内容のループ不変性（`ValueLoopInvariant`）**：変数再束縛なし（`BindingLoopInvariant`）に加え、不変組み込み型、または可変組み込み型がループ全体で `Unique` かつループ内に変更操作なしを要求（S27〜S28 対策）。
- **保守的 `SoundnessSummary`（A2・v1.0 用）**：`heki_effect` に一切依存せず、`heki_soundness` 内部でモジュール直下 1 回定義の静的束縛関数（モック対象外）のみを対象に保守的サマリを計算する。

---

## 5. エコシステム知識とバージョン差分管理（3 層モデル）

環境自動解決（`heki_env`：`pyproject.toml` / lockfile / `.venv`）の結果から、起動時に 1 回だけバージョン次元のないフラットなテーブルへ射影（Materialization）し、解析ロジックにバージョン分岐を持たせない。

```text
┌─────────────────────────────────────────────────────────────────────┐
│ Layer 1: ネイティブ Soundness KB (kb/soundness/*.toml)               │
│  builtins, collections, itertools, memoryview 等                     │
│  ・Soundness レーン完全専用（Advisory Effect は一切持たない）        │
│  ・厳密型 / 引数変更・保持 (retained_in_return) / 特殊メソッド / 例外 │
│  ・全件 CI 番兵検証 / verified_max_version 超過時は自動 Unknown 降格  │
├─────────────────────────────────────────────────────────────────────┤
│ Layer 2: エコシステム Effect/Cost カタログ (kb/ecosystem/*.toml)     │
│  標準ライブラリ公開 API (socket, subprocess, time, io, pathlib,       │
│  sqlite3, asyncio [AsyncSafe / OffloadsBlocking], logging [除外])    │
│  ＋ 主要 OSS (requests, httpx, aiohttp, sqlalchemy, django, psycopg, │
│  redis, boto3, pandas, numpy 等)                                     │
│  ・Advisory レーン用の宣言的公開 API モデル（すべての Effect 起点）  │
│  ・バージョン区間オーバーライド対応 / ユーザーカスタム TOML 拡張可能   │
├─────────────────────────────────────────────────────────────────────┤
│ Layer 3: オンデマンド局所ソース解析 (heki_effect + ディスクキャッシュ) │
│  ・ユーザープロジェクト内の全モジュール                               │
│  ・標準ライブラリおよび Layer 2 登録済みパッケージを除く、未登録の    │
│    Pure Python 依存（社内共通パッケージ等）のみをオンデマンド解析    │
└─────────────────────────────────────────────────────────────────────┘
```

- **標準ライブラリ・Layer 2 登録済みパッケージへの潜入禁止**：`asyncio.StreamWriter.write(data)` は 2 ホップでノンブロッキング `socket.send` に届く（A.7 実測）。Layer 1 から Effect を完全排除し、Layer 3 が標準ライブラリおよび Layer 2 登録済みパッケージへ潜ることを禁止することで、正しい非同期コードの誤爆を構造的に根絶する。

---

## 6. ルール体系（単一の統一理論から導出される 3 族）

Stage 0-A 事前実測（339 リポジトリ・1,810 万行・171 サンプル全件目視精査）および `dcg` 2 万行自前チェッカー調査の結果を踏まえ、採用ルール・不採用ルール・次期実測候補を厳格に確定する。

### 族 1：Mutation & State Effect（`MUT` — 成熟プロダクションの堅牢化）

#### コア採用ルール（Stage 0-B / v1.0 実装対象）

| Rule ID | 既定状態 | 検出内容 | 主なノイズ抑制・設定方式 |
|---|---|---|---|
| **MUT101** | 既定有効（厳格版） | **`@cached_property` 依存属性の無効化なし書き換え**：`@cached_property` が読む `self.<attr>` を、初期化フック以外のメソッド（間接呼び出し含む）がキャッシュクリアなしで再代入またはインプレース変更する | 2 段初期化フック（`model_post_init`/`_init_*`/`_configure_*`/`_open` 等）・メモ dict（`_cache[k]=v`）・互いに素な定数 dict キー・ネストクラス `__init__` を除外（Stage 0-A 実測知見） |
| **MUT103** | スナップショット運用時 | **関数間副作用・可変性の CI リグレッション（`heki snapshot summary`）**：スナップショット固定済み関数に新たな引数変更・エスケープ・`Blocking` Effect が混入した PR 差分を検出 | Zero-Config のスナップショット差分モードで動作 |
| **MUT201** | 設定時有効（契約型） | **宣言的 Write Ownership（書き込み境界）**：`[[tool.heki.write-owners]]` で指定した型・属性（ローカル別名・コンテナ操作 `.append()`/`[k]=`・`__dict__`/`setattr`・関数跨ぎ helper 経由含む）への書き込みを、許可されたモジュール／クラス（Store / Service 等）内に限定する | `pyproject.toml` 宣言のみ（デコレータ不要・実行時コスト 0）。行番号非依存 Ratchet ベースライン併用で既存コードから段階移行可能（dcg `check_generation_store_usage.py` 1,665 行＋テスト 1,323 行＋ランタイム witness 13 件を一般化） |
| **MUT202** | 設定時有効（契約型） | **宣言的 Read-Only Return（共有オブジェクト不変性）**：`[[tool.heki.readonly-returns]]` で指定した関数・キャッシュが返す共有オブジェクト（および配下コンテナ）を呼び出し側・下流の関数が mutate することを禁止する | 関数跨ぎ `mutates_params` と alias 追跡で静的検証（dcg `tests/checks/schema_graph.py` のランタイム指紋監視を静的化） |

*(Stage 0-A で実測した **`MUT102`（`MUT102a` 深いミューテーション / `MUT102b` Rep Exposure）** は、ヒット密度が低く大半が意図的イディオムであったため **正式不採用** とする：[ADR-014](docs/DESIGN_HISTORY_AND_ADR.md#adr-014)。)*

#### 次期 v1.x 候補（Stage 0-A Wave 2：339 リポジトリ / 1,902 万行実測＋目視監査済み）
- **採用有力（v1.x 第一弾）**：
  - **`MUT107`**：`__hash__` 依存属性の変更（成熟 OSS **21.2%**、目視精度 ~83%、`MUT101` と同等の lazy-init 除外で ~95% 見込み）。
  - **`OPT112`**：ループ内 `list.index` / `list.remove`（全リポジトリ **52.5%**・成熟 OSS **81.4%**、型付き / `ExactType` `list` 限定）。
  - **`OPT113`**：ネストループ等結合（`for a in A: for b in B: if a.k == b.k:`、成熟 OSS **10.2%**、目視精度 ~90%）。
- **狭く厳格なら採用（v1.x オプトイン）**：
  - **`EFF106`**：型確定した同期 `threading.Lock` / DB `.begin()` 保持中の `await` / HTTP（2.1%、発生時は致命的デッドロック）。
  - **`MUT106`**：SSA で再束縛されていない単一パス `Iterator`/`Generator` の二重消費。
  - **`EFF107`**：ローカル変数へ束縛後に非エスケープで GC 消失する `asyncio.create_task`（裸の呼び出しは Ruff `RUF006` と重複するため除外）。
- **Wave 2 実測により不採用**：`MUT104` のモジュール `ALL_CAPS` 定数書換（100% が意図的レジストリ／キャッシュ；`@lru_cache` 戻り値破壊のみ `MUT202` に統合）、`EFF108`（Ruff `SIM115` 重複＋高ノイズ）、`MUT105`（SSA 下で極低密度）。

### 族 2：Execution Effect（`EFF` / `DATA` — 本番アプリ・AI コードの性能・障害防止）

#### コア採用ルール（Stage 0-A 合格・Stage 0-B / v1.0 実装対象）

| Rule ID | 既定状態 | Stage 0-A 実測密度 | 検出内容・主な精度制御 |
|---|---|---|---|
| **EFF101** | 既定有効 | Web/Async **37.6%**（間接 82%） | `async def` からの直接・間接ブロッキング到達（Net/DB/Subprocess/Sync/Sleep）。`OffloadsBlocking`（ラッパー伝播含む）、Sync-over-Async 緩和、`await` 済み呼び出し除外、`subprocess.Popen` 除外、境界報告＋原因別集約、`logging`/`print` 除外 |
| **EFF105** | オプトイン | — | `async def` からのローカルファイル I/O 到達（`BlockingFS`、`--select EFF105`） |
| **EFF102** | 既定有効 | Web/Async **44.1%** / DB **48.5%**（間接 90〜93%） | ループ内での HTTP クライアント・セッション・接続プールの都度生成（関数跨ぎ含む）。`for.iter` 除外、`ConstSmall`/`RetryPattern`/`DaemonPolling` 緩和 |
| **EFF103** | 既定有効 | DB **47.6%**（間接 74%） | ループ内での明示的 DB クエリ・外部 RPC（N+1、関数跨ぎ含む）。`for.iter`・DDL・マイグレーション・ORM 内部・バルク API 除外。**ORM 暗黙的遅延ロードは対象外（Known Limitation）** |
| **DATA101** | 既定有効 | Data/ML **19.5%**（`LoopCarried` 適用後 ~90% 精度） | ループキャリーな `pandas.concat` / `numpy.vstack` / `numpy.append` 二乗累積。**SSA の `LoopCarried` 判定を必須**とし、単発結合・パディングを除外 |

*(Stage 0-A で実測した **`DATA102`（`df.iterrows()`）** は単一式構文リンターと重複するため **正式不採用** とする。)*

#### 次期コーパス実測候補（v1.x — 事前計測ゲート通過後に採否決定）
- **`EFF106`**：同期ロック（`threading.Lock`）または DB トランザクション保持中の `await` / 外部 HTTP 呼び出し。
- **`EFF107`**：戻り値を変数・コレクションへ保持しない `asyncio.create_task(...)`（GC によるタスク消失、変数束縛後の非エスケープ含む）。
- **`EFF108`**：関数跨ぎ・例外経路でのファイル／接続／カーソル未クローズ。

### 族 3：Complexity Rewrite（`OPT` — Alias 解析の副産物としての $O(n^2)$ 最適化）

| Rule ID | 対象パターン → 変換 | Profitability | 既定 `heki check --fix` | 主な Safety Obligation / Profitability 条件（抜粋） |
|---|---|---|---|---|
| **OPT101** | `x = x + [e]`, `x = [*x, e]`, `x = x + y` → `x.append(e)` / `x.extend(y)` | `unconditional` | **自動適用（`safe` 時）** | Safety: `Unique(x)`, `ExclusiveAccessAtWrite(x)`（全別名・全派生参照 Dead）, `ExactType(x/rhs, list)`, `ExceptionSafe`, `EvalOrder` / Profit: `LoopCarried(x)` |
| **OPT102** | `s = s \| {e}`, `d = {**d, k: v}`, `d = {**d, **other}` → `s.add(e)`, `d[k] = v`, `d.update(other)` | `unconditional` | **自動適用（`safe` 時）** | Safety: 同上（辞書ビュー・反復イテレータ Dead、**累積辞書 `d` が先頭であること：`{**other, **d}` は上書き優先順が逆転するため除外**） / Profit: `LoopCarried` |
| **OPT103** | 不変値二乗累積（`tuple +=`, `bytes +=`, `self.s +=`, 3項連結, 先頭連結, ループ跨ぎ f 文字列）→ バッファ＋最終変換 | `size-dependent` | 提案のみ | Safety: `Unique`, `ExclusiveAccessAtWrite`, `ExactType(lhs/rhs)`, 途中参照なし, `ExceptionSafe` / Profit: `LoopCarried`, `NotConstSmall`（ローカル `s += x` 除外） |
| **OPT110** | ループ内 `e in lst` → ループ外 `set` 化 | `size-dependent` | 提案のみ | Safety: `ValueLoopInvariant(lst)`, `ExactType(lst, list\|tuple)`, ハッシュ整合組み込み型要素, `ZeroTripExceptionSafe` / Profit: `NotConstSmall(lst)` |
| **OPT111** | ループ内 `if v not in seen: seen.append(v)` → 補助 `set` 併用 | `size-dependent` | 提案のみ | Safety: `Unique(seen)`, `ExclusiveAccessAtWrite`, `ExactType(seen, list)`, ハッシュ整合組み込み型要素, 削除なし / Profit: `NotConstSmall` |
| **OPT120** / **OPT121** | `q.pop(0)` / `q.insert(0, v)` / `del q[0]` / `q = q[1:]` → `collections.deque` | `size-dependent` | 提案のみ | Safety: `Unique(q)`, 非エスケープ, `deque` 等価操作のみ / Profit: `NotConstSmall`, **両端（`0`, `-1`）以外の任意添字 `q[i]` なし** |
| **OPT130** | ループ内不変コンテナ再構築（`e in set(keys)` 等）→ ループ外巻き上げ | `size-dependent` | 提案のみ | Safety: 全引数 `ValueLoopInvariant`, 副作用なし, `ZeroTripExceptionSafe`, 変更なし / Profit: `NotConstSmall` |

#### 次期候補（v1.x）
- **`OPT112`**：ループ内 `list.remove`/`index`/`count`（$O(n^2)$ 探索）。
- **`OPT113`**：ネストループ等結合（`for a in A: for b in B: if a.id == b.id:` → dict index 提案）。
- **`OPT131`**：ループ不変な重量計算呼び出し（`json.loads` / `sorted` / `re.compile` 以外の純粋重い変換）の巻き上げ。

---

## 7. 検証・品質保証アーキテクチャ

1. **CPython `symtable` 双方向突合テスト（`heki_semantic` 自己検証）**：`dcg` の `_source_model.py:verify()` に倣い、Rust 実装のスコープツリー・束縛・`global`/`nonlocal`・自由変数・内包表記（3.12+ PEP 709）・`__annotate__`（3.14+）・`lazy import`（3.15+ PEP 810）を実 CPython 3.12〜3.15 の `symtable` と全構文フィクスチャおよび実コーパスで双方向突合する。
2. **敵対的差分テスト＋Obligation 剥奪テスト**：族 3 の全ルールに対し、S1〜S28＋`{**other, **d}` を含む境界ケースと敵対的モックで CPython 3.12〜3.15 実機検証。Safety Obligation を 1 つ外せば必ず意味論反例が出ることを確認。
3. **Layer 1 ネイティブ KB 番兵検証**：弱参照・変更検知ラッパー・カスタム特殊メソッドを持つ番兵オブジェクトで実 CPython 挙動と KB 記述（`retained_in_return` 含む）の一致を CI 検証。
4. **族 1・族 2 の回帰テスト＆ドッグフーディング**：Stage 0-A と dcg 実コードから得られたエッジケース（`in_thread` ラッパー、Sync-over-Async、2 段初期化、メモ dict、互いに素な dict キー、`for.iter`、`await` 済み呼び出し、`subprocess.Popen`、DDL、`DataModel.fields` 書き込み境界＋13 件のランタイム witness、`SchemaGraphBuilder.schema` 共有保護）をフィクスチャ化し、dcg 実リポジトリをドッグフーディング対象として継続検証する。
5. **サイズ別実測ベンチマーク CI**：$n=0, 1, 2, 5, 10, 30, 100, \dots$ で Profitability 分類を継続検証。

---

## 8. システムアーキテクチャ（Rust ワークスペース ＆ 増分 LSP）

```text
heki/
├── Cargo.toml                  # Rust 2024 ワークスペース（unsafe_code = "forbid"）
├── pyproject.toml              # maturin (bindings = "bin") 単一バイナリ PyPI 配布
├── AGENTS.md                   # 他エージェント引き継ぎルール
├── crates/
│   ├── heki_ast/               # ruff_python_parser / ruff_python_ast (tag="0.17.5", PEP 810対応) 完全隔離
│   ├── heki_env/               # pyproject.toml / heki.toml / .hekiignore / RuntimeProfile / 環境ダイジェスト
│   ├── heki_semantic/          # スコープ・束縛・モジュール解決・PEP 709/649/810・軽量クラス階層 (CHA)
│   ├── heki_ir/                # 例外エッジ・評価順序・for.iter 分離・MakeIter・await・LoopBound 付き SSA-CFG IR
│   ├── heki_kb/                # Layer 1 (Soundness KB) + Layer 2 (Ecosystem Catalog) の静的コンパイル & 起動時射影
│   ├── heki_soundness/         # Soundness レーン（heki_effect 非依存をコンパイル時強制）
│   ├── heki_effect/            # Advisory レーン：Call Graph・統一 EffectSummary・増分 ProjectIndex (Early Cutoff)
│   ├── heki_rules/             # 族 1 (MUT1xx/MUT2xx) / 族 2 (EFF/DATA) / 族 3 (OPT: Safety + Profitability + TextEdit)
│   ├── heki_profile/           # 任意アドオン：外部プロファイルダンプ読込（Tachyon gecko/pstats / py-spy collapsed）
│   ├── heki_diagnostics/       # full/concise/json/github/sarif・# heki: ignore[...]・行番号非依存 Ratchet ベースライン
│   ├── heki_lsp/               # 増分 LSP サーバー（tower-lsp、RelatedInformation 呼び出し経路、Code Actions）
│   └── heki/                   # CLI バイナリエントリ（check / server[lsp] / rule[explain] / snapshot / assumptions）
├── kb/
│   ├── soundness/              # Layer 1：Soundness 専用ネイティブ仕様
│   └── ecosystem/              # Layer 2：標準ライブラリ公開 API ＋ 主要 OSS の Effect / AsyncSafe / Offloads カタログ
├── docs/
│   ├── IMPLEMENTATION_PLAN.md  # マイルストーン M1〜M7 と定量ゲート
│   ├── DESIGN_HISTORY_AND_ADR.md
│   └── ANALYSIS_AND_KB_SPEC.md
└── tests/
    ├── symtable_verify/        # heki_semantic vs CPython 3.12-3.15 symtable 双方向突合テスト
    ├── difftest/               # 敵対的差分テストスイート（S1〜S28 ＋ dcg {**other, **d}）
    ├── sentinel/               # Layer 1 KB 動的番兵検証
    └── semantic/               # 族 1 (MUT1xx/MUT2xx) & 族 2 (EFF/DATA) 回帰・ノイズ抑制テスト
```

### クレート依存の不変条件（Architecture Invariant）
- `heki_soundness` は `heki_effect` に **絶対に依存しない**。
- `heki_soundness` が参照できる KB は `EffectiveSoundnessKb`（Layer 1）と `RuntimeProfile` のみ。
- `heki_effect` の Layer 3 オンデマンド解析は、標準ライブラリおよび Layer 2 登録済みパッケージ内部へは絶対に潜らない。
- `ruff_python_*` への直接依存は `heki_ast` 内に完全閉鎖する。

### 増分 LSP 設計（10〜50ms 応答を実現する 2 段階構造）
- **初回起動（バックグラウンド）**：Rust 並列で全ファイルを解析し、IR 本体は破棄して軽量な `FileSummary` ＋ 逆コールグラフ（`ProjectIndex`）のみをメモリ常駐（10 万行規模で約 0.3〜1.5 秒・数十 MB）。
- **編集中（`didChange`, 250ms debounce — 約 5〜20ms）**：編集中 1 ファイルのみ再パース・再解析し、関数内ルールおよび他ファイルキャッシュ済み Summary による関数間ルールを即時診断。
- **差分伝播（`didSave` / Summary 変化時 — 約 10〜50ms）**：編集前後の `EffectSummary` が等しい場合（95% 以上の編集）は **Early Cutoff** で他ファイル再評価を完全スキップし、変化時のみ逆コールグラフ上の呼び出し元だけを増分再評価する。
- **LSP 固有 UX**：`EFF101`/`MUT201` の呼び出し経路を `DiagnosticRelatedInformation` でジャンプ可能にし、`OPT101`/`102` は Quick Fix、`OPT110`〜`130` は `refactor.rewrite.heki` Code Action として提供。VSCode・Claude Code・Codex・Neovim から `heki server`（または `heki lsp`）で即接続可能。

---

## 9. CLI・設定・Ratchet ベースライン・任意ダンプ連携（Ruff / `t-linter` 統一仕様）

### 9.1 CLI サブコマンド（終了コード：`0` 正常 / `1` 診断あり / `2` 実行エラー）
```bash
heki check [PATHS...]                         # 意味論診断（族 1 MUT + 族 2 EFF/DATA + 族 3 OPT）
heki check --fix [PATHS...]                   # safe かつ unconditional (OPT101/OPT102) のみ自動適用
heki check --diff [PATHS...]                  # 修正差分のプレビュー
heki check --select EFF,MUT --ignore MUT101   # ルール選択（コード・スラッグ両対応）
heki check --output-format full|concise|json|github|sarif
heki check --baseline heki-baseline.toml      # Ratchet モード：増加で FAIL、減少時は縮小を要求
heki check --update-baseline                  # 行番号非依存ベースライン生成・縮小更新
heki check --profile dump.collapsed [PATHS]   # 任意：外部採取済みプロファイルダンプ読込
heki check --watch [PATHS...]                 # ファイル監視モード

heki server                                   # 増分 LSP サーバー起動（`heki lsp` エイリアス対応）
heki rule <RULE_ID>                           # 規則・Proof Obligation・反例表示（`ruff rule` 互換）
heki snapshot summary --write [PATHS...]      # 推論済み Summary スナップショット（MUT103）
heki assumptions [PATHS...]                   # プロジェクト内の前提 A1/A2 違反箇所を一覧出力
```

### 9.2 設定ファイル（`pyproject.toml` `[tool.heki]` または `heki.toml` — kebab-case 統一）
```toml
[tool.heki]
target-version = "py312"             # 未指定時は project.requires-python から自動推論
extend-exclude = ["generated"]       # .gitignore および .hekiignore を既定尊重
baseline = "heki-baseline.toml"      # 任意：Ratchet ベースライン

[tool.heki.lint]
select = ["EFF", "DATA", "MUT", "OPT"]
extend-select = ["EFF105"]
ignore = []

[tool.heki.lint.severity]
MUT101 = "warning"

[tool.heki.lint.per-file-ignores]
"tests/**" = ["EFF101", "EFF102", "EFF103"]

[[tool.heki.write-owners]]
name = "generation-store"
targets = ["my_pkg.model.base.DataModel.fields"]
allowed-writers = ["my_pkg.parser.generation.*"]

[[tool.heki.readonly-returns]]
name = "shared-schema"
producers = ["my_pkg.parser._schema_graph.SchemaGraphBuilder.schema"]
deep = true
```

### 9.3 インライン抑止（Ruff `RUF100` 非衝突）
- **推奨（設定不要・Ruff 非衝突）**：`# heki: ignore` / `# heki: ignore[EFF101]` / `# heki: ignore[async-blocking-call]`（行末＋直前単独行の両対応、`t-linter`・`ty` と同一記法）。
- **任意互換**：`# noqa: EFF101` も認識（Ruff 側で `lint.external = ["MUT", "EFF", "OPT", "DATA"]` と併用したいユーザー向け）。
- **外部プロファイルダンプの位置づけ**：実行時にプロファイラを挟む運用はオーバーヘッド・権限制約（macOS での Tachyon / `py-spy` の `sudo` 必須など）があるため必須とせず、**既に採取されたダンプファイル（Tachyon `--gecko` / `--pstats`、`py-spy` `--collapsed`、`cProfile`）を後から読む任意アドオン**にとどめる。

---

## 10. 開発計画と定量的検証ゲート

### Stage 0-A：大規模実測ゲート【完了（2026-11-19）】
- **実績**：成熟 OSS 118 リポジトリ（1,242 万行）＋ 2026 年新規 GitHub 221 リポジトリ（568 万行）＝ **339 リポジトリ・56,629 ファイル・1,810 万行**をスキャンし、全 8 ルール 171 サンプルを目視精査。さらに `dcg`（19.6 万行）の自前チェッカー 2 万行を分析。
- **確定事項**：`EFF101`/`EFF102`/`EFF103`/`DATA101` を主力採用、`MUT101` を厳格ノイズ除外つきで採用、`MUT102`/`DATA102` を実測不採用、dcg 調査から **`MUT201`/`MUT202`・Ratchet ベースライン・`symtable` 突合テスト** を正式組み込み、インポート境界は `tach` ＋ Python 3.15 PEP 810 に委譲。

### Stage 0-B：縦貫プロトタイプゲート（次のフェーズ）
- **実装範囲**：
  - 共通基盤：`heki_ast`（PEP 810 対応確認・固定）、`heki_env`、`heki_semantic`（**`symtable` 双方向突合テスト付き**）、`heki_ir`、`heki_kb`。
  - 解析エンジン：`heki_soundness`（関数内 A1）＋ `heki_effect`（統一 `EffectSummary`・Write Ownership・Alias 追跡）。
  - 代表ルール：**`MUT101`, `MUT201`, `MUT202`, `EFF101`, `EFF102`, `EFF103`, `DATA101`, `OPT101`, `OPT102`, `OPT110`**。
  - 出力・運用：JSON 出力、行番号非依存 **Ratchet ベースライン**、敵対的差分テスト基盤。
  - **実戦ドッグフーディング目標**：`dcg` に対して `MUT201` / `MUT202` / `MUT101` を走らせ、既存 `check_generation_store_usage.py` の検出＋ランタイム witness 13 件の捕捉率と `schema_graph.py` 置換性を定量検証する。
- **v1.0 進行ゲート基準（定量定義）**：
  1. **意味解析器の一致性**：CPython 3.12〜3.15 `symtable` とのスコープ・束縛突合で不一致 **0 件**。
  2. **Soundness 誤変換 0 件**：`safe` 判定の誤変換 **0 件**（各ルール 50 件目視＋差分テスト反例 0 件、S1〜S28＋`{**other, **d}` 全件合格）。
  3. **Advisory 精度・実害密度（族 1・族 2）**：
     - 厳密な偽陽性率（Tool Error）：**5% 未満**。
     - 実務上のノイズ率（非実害含む）：既定ルール＋境界集約適用後の目視精査で **20% 未満**。
     - 実害検出密度：対象プロジェクト群の **30% 以上で 1 件以上の納得感ある指摘**（契約型 `MUT201`/`MUT202` は `dcg` 実機で既存チェッカー相当＋取りこぼし捕捉を実証）。
  4. **解析性能**：10 万行規模のリポジトリを数秒オーダーで解析完了。

### v1.0：プロダクションリリース
- 族 1〜3 全採用ルール、次期候補（`MUT104`–`107`, `EFF106`–`108`, `OPT112`–`113`, `OPT131`）のコーパス事前計測・選抜組込み、`heki_soundness` 内の A2 用 `SoundnessSummary`、`heki snapshot summary` CI ゲート、Layer 1 全番兵テスト CI、Layer 2 主要エコシステムカタログ＋ユーザー拡張、Layer 3 オンデマンド外部解析キャッシュ、SARIF 2.1.0、任意 `--profile` ダンプ読込。
- 品質基準：CPython 3.12〜3.15 各バージョンで全書き換えルール 10 万ケース以上の敵対的差分テスト反例 0 件、主要 OSS（`dcg` 含む）への実適用検証完了。
