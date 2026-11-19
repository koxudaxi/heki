# 検討経緯・設計改訂ログ & ADR（Architecture Decision Records）

> **このドキュメントの位置づけ**
> [ARCHITECTURE.md](../ARCHITECTURE.md)（Baseline v4.1：正式名称 `heki`）に至るまでの検討経緯、大規模実測（1,970 万行＋Stage 0-A Wave 1/2 合計 1,902 万行）・反例実験・`datamodel-code-generator`（19.6 万行＋自前チェッカー 2 万行）調査・CPython 3.15 PEP 810 & `ruff_python_parser 0.17.5` 実機検証の根拠、v1.0 → v4.1 の全改訂理由、およびすべての主要意思決定（ADR-001〜ADR-017）を記録する補足資料である。

---

## 1. プロジェクトの出発点とオーナーの基本姿勢

### 1.1 当初の構想（Origin）
当初の構想は、Rust の Ownership / Borrowing 概念を Python の静的解析に導入し、① 無注釈推論（Stage 1）、② 明示契約 API（Stage 2：`Owned`/`Borrowed`/`move()`/`@pure`）、③ Lean 4 形式検証、を組み合わせたパフォーマンス・オプティマイザーを作るというものだった。

### 1.2 議論と実測を通じて確定したゴールと建付け
- Rust 所有権や Lean 4 は**目的ではなく手段**に過ぎない。
- **「場当たり対応」や「先送り」を排除**し、単一の統一理論に基づく完結したアーキテクチャで、Ruff・ty・tach と被らずに CI 常設できる業界標準級のツールを目指す。
- 大規模コーパス実測（合計 3,700 万行超）・`datamodel-code-generator`（`dcg` 19.6 万行＋自前防御インフラ 2 万行）の深掘り検証を経て、最終的に **「Python 本体の Effect（実行・状態変更）＆ Alias（一意性・所有境界）意味論チェッカー（Ruff・ty に続く第 3 の静的解析層）であり、高速化はその自然な副作用として得られる」** という誠実かつ強固な建付けへ収斂した（[ADR-012](#adr-012)〜[ADR-016](#adr-016)）。

---

## 2. 実測・一次情報・実プロジェクト検証から得た決定的事実

### 2.1 大規模コーパス（1,970 万行）＆ 反例実験（S1〜S28）＆ CPython 3.15 実測
1. **Pure Python コンテナ書き換え単体では密度が低く、既定自動適用対象はさらに少ない**
   - 無注釈で証明できそうな組み込みコンテナ最適化は 100 万行あたり約 130 件（約 7,600 行に 1 件）。
   - 小サイズ実測（A.8）により、$n=1$ から常に改善（`unconditional`）するのは **`OPT101` と `OPT102` のみ**（成熟 1,320 万行に証明可能見込み 20 件、新規 650 万行に 1 件）。`OPT103` は小反復で 0.23〜0.84 倍に劣化、`OPT130` は 0 回ループで 0.01 倍（約 100 倍劣化）する。
2. **一意性（フレーム性）は成立するが、派生参照とオブジェクト内容不変性の追跡が必須（S1〜S28）**
   - `for v in x: x = x + [v]` の無限ループ化（S23）、`iter`/`enumerate`/`zip`/`map`/ジェネレータ式/`d.keys()` 生存中の書き換え（S24〜S26b）、ループ中での外部／別名変更（S27〜S28）が実験確認された。
3. **定数倍マイクロ最適化は逆効果が多く、底辺プリミティブからの Effect 伝播は `asyncio` を誤爆する**
   - 組み込み・束縛メソッドのローカルキャッシュは CPython 3.15 で 2〜12% 悪化、`re.compile` も内部キャッシュにより定数倍。
   - `sys.setprofile` 実測（A.7）により、正しい非同期コード `asyncio.StreamWriter.write(data)` が 2 ホップでノンブロッキング `socket.send` に届くことが実証された。

### 2.2 Stage 0-A 事前実測（339 リポジトリ / 56,629 ファイル / 18,108,143 行 ＋ 171 サンプル全件目視精査）
- **対象**：成熟 OSS 118 リポジトリ（1,242 万行）＋ 2026 年新規 GitHub 221 リポジトリ（568 万行）を 8 並列スキャン（62.8 秒）し、8 ルール 171 サンプルを目視精査。
- **主力採用の確定**：
  - `EFF101`：Web/Async **37.6%**（間接 82.3%）、**68% 実害**（`redis-py` `redis/asyncio/cluster.py:1613` の実 `time.sleep`、4〜5 ホップ到達含む）。`await` 済み呼び出しと `subprocess.Popen` の除外要件を特定。
  - `EFF102`：Web/Async **44.1%**・DB **48.5%**（間接 90〜93%）、`for.iter` 除外後 ~75〜80% 実害（`NVIDIA-NeMo`、`OmniAgent`、`luigi` 等）。
  - `EFF103`：DB **47.6%**（間接 74.2%）。`for.iter`・DDL・マイグレーション・ORM 内部除外要件を特定。
  - `DATA101`：Data/ML **19.5%**。素朴 AST 偽陽性 60% は全件非 `LoopCarried` パディングであり、SSA `LoopCarried` 判定により ~90% 精度となることを確認。
- **厳格化・不採用の確定**：
  - `MUT101`：成熟 OSS **16.1%**（`dcg`・`mkdocs`・`wagtail`・`textual` 等に実ヒット）。素朴版のノイズ 3 原因（2 段初期化・メモ dict・互いに素な dict キー）およびネストクラス `__init__` 分離要件を特定し、厳格版で採用。
  - `MUT102a`/`MUT102b`：成熟 OSS 4.2%・新規 0.5〜1.4% と低密度かつ大半が意図的イディオムのため **実測により不採用**。
  - `DATA102`（`df.iterrows()`）：単一式構文リンターと重複するため **不採用**。

### 2.3 実プロダクションプロジェクト検証：`datamodel-code-generator`（19.6 万行 ＋ 自前チェッカー 2 万行）
1. **性能・CLI 起動・ネイティブ化の解剖**：
   - 純粋コア（JSON 6 スキーマ、`Formatter.BUILTIN`：1,771 ms）はスケーリング試験すべて完全線形 $O(N)$。中規模 CLI（269 ms）の 45% が起動・395 モジュール import 時間であり、一部ネイティブ拡張（`.so`）化は CLI として逆効果（オーナー検証済＋数値確認）。
   - 既定 `OPT101`/`OPT102` 自動適用は 0 件（`_target_binding.py:465` で `{**other, **found}` 右側累積の新反例を発見）。`_runtime/client/` から `await in_thread(self.open)` ラッパー伝播と Sync-over-Async（`run_sync`）緩和の 2 知見を獲得。
   - `@cached_property` スキャンで **dcg 0.86.0 実バグ（`jsonschema.py:1460`）に一発的中**。
2. **dcg の自前防御インフラ（約 20,272 行）の解剖から得た決定的知見**：
   - dcg は静的チェッカー `scripts/` **9,762 行** ＋ ランタイム監視 `tests/checks/` **4,685 行** ＋ チェッカー単体テスト **5,825 行** を自前保守している。
   - 基盤の `scripts/_source_model.py`（2,433 行）自身が *「value flow・container aliases・attribute types・call graphs は AST では答えられない」* と明記しており、その結果 `check_generation_store_usage.py`（1,665 行）はローカル alias 追跡を手書きしてなお **13 件を取りこぼしランタイム `__setattr__` 監視（`witnesses`）に依存** し、`tests/checks/schema_graph.py`（236 行）は共有 `JsonSchemaObject` の不変性検証にディープ指紋比較を強いられている。
   - これらはすべて `pyopt_effect` の **State Effect + Alias + Call Graph** で一般化できる（**`MUT201` Write Ownership** / **`MUT202` Read-Only Return**）。また、行番号非依存の片道縮小ベースライン（`check_ratchets.py` + `generation_store_allowlist.toml`）と CPython `symtable` 双方向突合テスト（`_source_model.py:verify()`）はツール設計・QA の中核手法として取り込む価値が高い。
3. **CPython 3.15 実機検証（Tachyon ＆ PEP 810 `lazy import`）**：
   - Tachyon（`profiling.sampling`）は `--opcodes` と `--jsonl` 非併用、かつ macOS では `sudo` 必須。プロファイル連携は任意オフラインダンプ読込に限定。
   - **PEP 810（`lazy import json` / `lazy from pathlib import Path` → `is_lazy=1`）が CPython 3.15.0 に入っていることを実機確認**。モジュール import 遅延・循環回避は `tach` ＋ Python 3.15+ PEP 810 に委譲し、`pyopt` は Effect & Alias へ純化する判断を確定。

---

## 3. 設計の進化ログ（v1.0 → v2.0 → v2.1 → v3.0 → v4.0）

### 3.1 v1.0 → v3.0 の要約
- **v1.0 → v2.0**：2 レーン物理分離（`pyopt_soundness` vs `pyopt_effect`）、3 層知識モデル、Datalog 不採用、Safety × Profitability 直交モデル、`LoopBound` ゲート、Stage 0-A 先行。
- **v2.0 → v2.1**：派生参照（`derived_refs`）、`ValueLoopInvariant`、`pyopt_soundness` 内 `SoundnessSummary` 独立化、`OPT103`/`OPT130` の `size-dependent` 化、Layer 1 純化・Layer 3 標準ライブラリ潜入禁止、`EFF101` 段階化・境界集約、`EFF104` 不採用。
- **v2.1 → v3.0**：dcg 19.6 万行実測を受け、「Effect & Alias 意味論チェッカー（高速化はその自然な副作用）」へ再定義、ネイティブ拡張・場当たり個別ルールを Non-Goal 確定、Tachyon 任意ダンプ化、`OffloadsBlocking` ラッパー伝播・Sync-over-Async 緩和、`OPT102` 先頭累積限定。

### 3.2 Baseline v3.0 → Baseline v4.0 の改訂（Stage 0-A 完了・dcg 自前チェッカー調査・PEP 810 検証）

| 論点 | Baseline v3.0 | 実測・調査で見えた事実 | Baseline v4.0 の確定設計 | 関連 ADR |
|---|---|---|---|---|
| **Stage 0-A ルール採否** | `MUT101`/`102`, `EFF101`–`103`, `DATA101`/`102` を計測予定 | 339 リポジトリ・171 サンプル精査で `EFF101`–`103`/`DATA101` が高密度・高実害、`MUT102` は低密度・高ノイズ、`DATA102` は Ruff 重複 | `EFF101`–`103`/`DATA101`(SSA `LoopCarried`必須)/`MUT101`(厳格版) を採用、**`MUT102`/`DATA102` を正式不採用**。次期拡張候補（`MUT104`–`107`, `EFF106`–`108`, `OPT112`–`113`, `OPT131`）を整理 | [ADR-014](#adr-014) |
| **成熟 OSS の状態保護（宣言的境界）** | 自動推論（`MUT101`/`102`/`103`）のみ | dcg が 2 万行の自前チェッカー＋ランタイム監視で「書き込み境界」「共有オブジェクト不変性」を守りつつ AST の壁（13 件取りこぼし）に直面 | **`MUT201`（Write Ownership）**・**`MUT202`（Read-Only Return）**・**行番号非依存 Ratchet ベースライン**・**`symtable` 突合テスト** を正式組み込み | [ADR-015](#adr-015) |
| **インポート境界・Python 3.15 PEP 810** | 未規定 | `tach`/`import-linter` が既存であり、さらに CPython 3.15.0 で **PEP 810 (`lazy import`, `is_lazy=1`)** が正式導入済みであることを実機確認 | 汎用的な層間 import / 循環 import 検査は **Non-Goal として委譲**し Effect & Alias に純化。`pyopt_ast`/`pyopt_semantic` は PEP 810 構文と遅延評価セマンティクスを正式サポート | [ADR-016](#adr-016) |

---

## 4. Architecture Decision Records（ADR）

<a id="adr-001"></a>
### ADR-001: プロダクトの主軸 — 独立 Middle-end の採用、Ruff `unsafe fix` 判定器の不採用
- **決定**：Ruff `unsafe fix` 判定器（`pyopt judge`）は不採用とし、独立した Middle-end 解析器として構築する（[ADR-012](#adr-012) で「Effect & Alias 意味論チェッカー」へ統合）。
- **理由**：Ruff `unsafe fix` の過半は型情報で解け `ty` に吸収されるリスクが高く、上流追従コストも過大。一方、関数跨ぎの Effect（間接到達は全体の 74〜93%）と Alias/Mutation 解析は Ruff の構造的守備範囲外である。

<a id="adr-002"></a>
### ADR-002: 独自契約 API（Stage 2：`Owned`/`Borrowed`/`move()`/`@pure`）の完全棄却
- **決定**：ユーザーコードへの独自契約 API は Non-Goal として完全に棄却する。代わりに **標準 `typing` ＋ Zero-Config 推論 ＋ `pyproject.toml` 境界宣言（`MUT201`/`MUT202`）＋ `pyopt snapshot summary`** を採用する。
- **理由**：関数色付け・パッケージ依存・実行時オーバーヘッド・普及障壁を避けるため。

<a id="adr-003"></a>
### ADR-003: Lean 4 導入の完全棄却と、公理的解析＋敵対的差分テストによる厳密性担保
- **決定**：Lean 4 は Non-Goal として完全に棄却し、保守的抽象解釈（Lattice）＋宣言的 Proof Obligation（3 値評価）＋敵対的差分テスト（Obligation 剥奪テスト含む）＋Layer 1 KB 番兵検証を採用する。

<a id="adr-004"></a>
### ADR-004: 単一 IR 上の 2 レーン物理分離（`pyopt_soundness` vs `pyopt_effect`）と各サマリの独立定義
- **決定**：SSA-CFG IR の下流を `pyopt_soundness` と `pyopt_effect` にクレート分離し、`pyopt_soundness` の `pyopt_effect` への依存をコンパイル時禁止する。
  - `pyopt_soundness`：型注釈を信用せず `ExactType`・`Unique`・`derived_refs`・`ValueLoopInvariant` のみを使用。A2（v1.0）用には独立した保守的 `SoundnessSummary` を持つ。
  - `pyopt_effect`：軽量レシーバ型解決（名前解決＋PEP 484＋`self` 属性＋CHA/RTA）を用い、統一 `EffectSummary`（Execution Effect ＋ Mutation & State Effect ＋ Write Ownership）を計算。A1 違反関数でも継続する。

<a id="adr-005"></a>
### ADR-005: 3 層知識モデル（Layer 1 純化 / Layer 2 一本化 / Layer 3 潜入禁止と依存完全キャッシュ）
- **決定**：Layer 1（Soundness 専用、Effect なし、番兵検証、`verified_max_python` 降格）、Layer 2（標準ライブラリ公開 API＋主要 OSS の全 Effect 起点・`AsyncSafe`・`OffloadsBlocking`・`logging` 除外・複製サニタイザ）、Layer 3（標準ライブラリおよび Layer 2 登録済みパッケージを除く未登録 Pure Python 依存のみオンデマンド解析、キャッシュキーに `effective_kb_digest` と `env_packages_digest` を含む）の 3 層＋起動時射影を採用する。

<a id="adr-006"></a>
### ADR-006: 埋め込み Datalog（`ascent`）の不採用と専用解析エンジンへの置換
- **決定**：埋め込み Datalog は採用せず、Provenance 付き Lattice の RPO Worklist 抽象解釈 ＋ 宣言的 Proof Obligation（3 値評価）＋ Tarjan SCC Worklist 不動点計算を採用する。

<a id="adr-007"></a>
### ADR-007: Safety × Profitability 直交モデル・`LoopBound` ゲート・現実的自動修正件数の明示
- **決定**：Safety（意味保存）と Profitability（収益性：`LoopCarried`, `NotConstSmall`, 入力サイズ依存）を分離。`unconditional`（既定 `pyopt fix`）は `OPT101`・`OPT102` のみとし、`OPT103`・`OPT110`〜`OPT130` は `size-dependent`、`OPT120`/`OPT121` は両端以外の任意添字 `q[i]` 禁止とする。

<a id="adr-008"></a>
### ADR-008: Advisory 精度設計（段階化・境界集約・ラッパーオフロード伝播・Sync-over-Async 緩和・`EFF104` 不採用）
- **決定**：`EFF101` は高影響ブロッキングに絞り `BlockingFS` はオプトイン `EFF105` 分離・`logging`/`print` 除外。境界報告＋原因別集約、`OffloadsBlocking` ラッパー伝播、Sync-over-Async 緩和、`EFF104` 不採用、`EFF103` ORM 遅延ロード対象外明記。

<a id="adr-009"></a>
### ADR-009: Ruff crate の最小隔離利用とスコープ・CFG・SSA IR の自作
- **決定**：`ruff_python_parser` / `ruff_python_ast` のみバージョン完全固定（PEP 810 対応版）で `pyopt_ast` へ隔離し、スコープ解決・CFG・SSA IR は自作する。

<a id="adr-010"></a>
### ADR-010: Stage 0-A 事前実測先行と定量ゲート基準の厳格定義
- **決定**：すべての主力ルール・次期候補ルールはコーパス実測と目視精査でゲート判定してから正式採用する。Stage 0-B ゲートは `symtable` 不一致 0 件、Soundness 誤変換 0 件、Advisory 厳密偽陽性 < 5%、実務ノイズ < 20%、対象プロジェクト 30% 以上で納得指摘（契約型ルールは `dcg` 実機実証）、10 万行を数秒オーダーとする。

<a id="adr-011"></a>
### ADR-011: 派生参照・オブジェクト内容不変性・`OPT102` 辞書アンパック優先順の厳密化
- **決定**：`FrameState` に `derived_refs` と推移的エスケープ、`ExclusiveAccessAtWrite`、上限 4 の `alloc_ids` 合流、`ValueLoopInvariant` を導入する（S23〜S28 対策）。`OPT102` の辞書アンパックは累積辞書が先頭（`{**d, ...}`）の形のみに限定する（dcg `_target_binding.py:465` 対策）。

<a id="adr-012"></a>
### ADR-012: プロダクト再定義 — 「Effect & Alias 意味論チェッカー（高速化はその副作用）」への統合
- **決定**：プロダクトを **「Python 本体の Effect & Alias 意味論チェッカー（Ruff・ty に続く第 3 の静的解析層）」** と再定義し、**「高速化はその自然な副作用・応用として得られる」** 建付けとする。単一の SSA-CFG ＋ コールグラフ解析基盤から、族 1（`MUT`）・族 2（`EFF`/`DATA`）・族 3（`OPT`）の 3 族を統一導出する。

<a id="adr-013"></a>
### ADR-013: ネイティブ拡張（AOT/mypyc）・場当たりフレームワーク最適化の除外と外部ダンプの任意アドオン化
- **決定**：AOT ネイティブ拡張・ビルド時書換・個別フレームワーク勘ルールを Non-Goal とし、プロファイル連携は実行非介入のオフラインダンプ読込（任意アドオン）に限定する。

<a id="adr-014"></a>
### ADR-014: Stage 0-A 実測に基づくルール採否確定（`MUT102`/`DATA102` 不採用、`DATA101` SSA 必須化、`MUT101` 厳格化）
- **背景**：339 リポジトリ（1,810 万行）・171 サンプル全件目視精査を実施した。
- **決定**：
  1. `EFF101`/`EFF102`/`EFF103`/`DATA101` を主力採用し、IR 上で `for.iter` ループ外分離・`is_awaited` 識別・`subprocess.Popen` 除外・DDL 除外・`DaemonPolling` 緩和・`DATA101` の SSA `LoopCarried` 必須化を行う。
  2. `MUT101` は 2 段初期化フック・メモ dict・互いに素な定数 dict キー・ネストクラス分離の 4 フィルタを必須として採用する。
  3. **`MUT102`（深いミューテーション・Rep Exposure）と `DATA102`（`df.iterrows()`）は実測結果に基づき正式不採用**とする。
  4. 追加候補（`MUT104`–`107`, `EFF106`–`108`, `OPT112`–`113`, `OPT131`）は v1.x 候補として保持し、実装前に同一の事前実測ゲートにかける。

<a id="adr-015"></a>
### ADR-015: 宣言的 Write Ownership（`MUT201`）・Read-Only Return（`MUT202`）・Ratchet ベースライン・`symtable` 突合の採用
- **背景**：`dcg` の自前防御インフラ（20,272 行）を調査した結果、成熟プロダクションが Ruff でも `ty` でも守れず自前 AST＋ランタイム監視に追い込まれている最大の痛みは「指定ストア外からのモデル状態書換（別名・コンテナ操作・間接呼び出し含む）」と「共有キャッシュオブジェクトの破壊」であり、同時に大規模導入には行番号非依存の片道縮小ベースラインが、意味解析器の信頼性には CPython `symtable` 突合が不可欠であることが実証された。
- **決定**：
  1. `pyproject.toml` で宣言する **`MUT201`（Write Ownership）** と **`MUT202`（Read-Only Return）** を族 1 の正式な柱に組み込む（既存の State Effect + Alias エンジンに無改造で乗る）。
  2. **`pyopt baseline --ratchet`**（行番号非依存キー `(path, qualname, rule, target) = count` の片道縮小管理）を標準機能化する。
  3. `pyopt_semantic` の品質保証に **CPython 3.12〜3.15 `symtable` 双方向突合テスト** を義務づける。

<a id="adr-016"></a>
### ADR-016: インポート境界・循環検査の非採用（`tach` 委譲）と Python 3.15 PEP 810 (`lazy import`) の意味論サポート
- **背景**：`dcg` は `_import_cycles.py`（117 行）や `check_architecture_boundaries.py`（1,977 行）、`redundant-local-import` を持つ一方、エコシステムには既に `tach` / `import-linter` が存在し、さらに実機 CPython 3.15.0 で **PEP 810（`lazy import` / `lazy from ... import ...`, `is_lazy=1`）** が標準導入されたことを確認した。
- **決定**：
  1. 汎用的な層間 import 境界・import cycle 検査は `pyopt` のコアに含めず、`tach` / `import-linter` および PEP 810 に委譲して **「Effect & Alias」へ純化**する。
  2. 一方、`pyopt_ast` および `pyopt_semantic` は **Python 3.15 PEP 810（`is_lazy=1`）の構文と初回評価時実体化セマンティクスを正式サポート**する（`ruff_python_parser` の固定バージョン選定要件に含める）。

<a id="adr-017"></a>
### ADR-017: 正式名称 `heki` 確定、Ruff/`t-linter` 統一インターフェース、増分 LSP 設計、Stage 0-A Wave 2 採否確定
- **背景**：実装着手にあたり、① 行末 `# <name>: ignore[EFF101]` に耐える短さ・日英発音（`v`/`th` なし）・SEO・PyPI/crates.io 空きを満たす正式名称の確定、② Ruff・`t-linter` と違和感なく併用できる CLI/Config インターフェース、③ LSP 実行時のレイテンシ現実性、④ v1.x 次期候補ルール 9 本の事前実測が必要だった。
- **決定**：
  1. **作業名称を `heki`（壁・鉄壁／仮称）に設定**：4 文字（`ruff` と同長）・`v`/`th` なし・宗教／スラング／IT 企業 SEO 競合なし・PyPI（HTTP 404）および crates.io（HTTP 404）完全空きを実機確認（※当初候補 `kabe` はトルコ語 `Kâbe`＝カアバ神殿との衝突が判明し回避。最終公開名称は並行検討中）。Write Ownership・共有不変性・`async` 越境・Alias 境界という全ルールを「障壁・鉄壁（Boundary）」で象徴する。
  2. **Ruff ＋ `t-linter` 統一インターフェース**：`maturin` 単一 Rust バイナリ配布、CLI は `heki check` / `heki check --fix` / `heki server`（`lsp` エイリアス）/ `heki rule`（`explain` エイリアス）/ `heki snapshot` / `heki assumptions`、終了コード `0/1/2`、設定は `pyproject.toml` `[tool.heki]`（kebab-case）および `heki.toml` / `.hekiignore`、インライン抑止は Ruff `RUF100` 非衝突の `# heki: ignore[...]` を主とし `# noqa:` も互換受理する。
  3. **増分 LSP 設計（Early Cutoff）**：`heki_effect::ProjectIndex` は IR 本体を保持せず軽量 `FileSummary` ＋ 逆コールグラフのみ常駐させ、`didChange`（250ms debounce）は単一ファイル再評価、`didSave` / Summary 変化時は Early Cutoff で影響呼び出し元のみ再評価する構造を初日から採用する。
  4. **Stage 0-A Wave 2（339 リポジトリ・1,902 万行・30.5 秒実測＋目視監査）**：
     - **v1.x 第一弾採用**：`MUT107`（`__hash__` 依存書換：成熟 OSS 21.2%、精度 ~83%）、`OPT112`（ループ内 `list.index`/`remove`：成熟 OSS 81.4%、型限定）、`OPT113`（ネストループ等結合：成熟 OSS 10.2%、精度 ~90%）。
     - **狭く厳格なら採用（v1.x オプトイン）**：`EFF106`（型確定した同期ロック／DB `.begin()` 内 `await`/HTTP）、`MUT106`（SSA 非再束縛の単一パス `Iterator` 二重消費）、`EFF107`（Ruff `RUF006` 重複の裸呼び出しを除外し、ローカル束縛後の非エスケープ消失のみ）。
     - **Wave 2 実測により不採用**：`MUT104` のモジュール `ALL_CAPS` 定数書換（100% が意図的レジストリ／キャッシュ；`@lru_cache` 戻り値破壊のみ `MUT202` に統合）、`EFF108`（Ruff `SIM115` 重複＋高ノイズ）、`MUT105`（SSA 下で極低密度）。

---

## 5. 未決定事項・今後の検討項目

- 主要設計・名称・ライセンス（MIT）・インターフェース・Stage 0-A 事前実測（Wave 1 & Wave 2）はすべて完了。以降の実装手順と定量ゲートは [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md)（マイルストーン M1〜M7）および [AGENTS.md](../AGENTS.md) を参照。
