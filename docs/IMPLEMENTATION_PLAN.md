# `heki` 実装計画・マイルストーン（他エージェント引き継ぎガイド）

> **前提ドキュメント**
> すべての設計根拠・不変条件・実測データは [ARCHITECTURE.md](../ARCHITECTURE.md)（Baseline v4.1）、[ANALYSIS_AND_KB_SPEC.md](ANALYSIS_AND_KB_SPEC.md)、[DESIGN_HISTORY_AND_ADR.md](DESIGN_HISTORY_AND_ADR.md) に確定済みである。M3〜M7 の具体的な API 契約・アルゴリズム擬似コード・完了ゲートは **[DETAILED_SPEC_M3_M7.md](DETAILED_SPEC_M3_M7.md)**、マイルストーン別の引き継ぎプロンプトは **[HANDOFF_PROMPTS.md](HANDOFF_PROMPTS.md)** を参照し、仕様を再議論せず上から順に緑にしていく。

---

## 0. 完了済み資産（引き継ぎ時点のベースライン）

- **12 クレート Rust ワークスペース骨格**（`Cargo.toml` / `pyproject.toml`）：
  - `heki_ast`：`ruff_python_parser` / `ruff_python_ast` (`tag = "0.17.5"`) を完全隔離し、Python 3.15 **PEP 810 (`lazy import`, `is_lazy`)** のパースをユニットテスト実証済み。
  - `heki_env`：Ruff / `t-linter` 互換 `pyproject.toml` (`[tool.heki]`, kebab-case) ＋ `RuntimeProfile`（PEP 709/649/810）。
  - `heki_kb`：Layer 1 (`EffectiveSoundnessKb`, `verified_max_python` 自動降格) と Layer 2 (`EffectiveEcosystemKb`, `opaque_to_layer3`) の型分離＋スターター TOML (`kb/soundness/builtins.toml`, `kb/ecosystem/stdlib.toml`)。
  - `heki_soundness`：**`heki_effect` 非依存を `Cargo.toml` で物理強制**、`MAX_JOIN_ALLOC_IDS = 4`・`FrameState`・`ProofObligation`・`SafetyTier` × `ProfitabilityTier` 定義済み。
  - `heki_effect`：LSP 増分実行前提の `ProjectIndex`・`FileSummary`・**Early Cutoff**（テスト実証済み）。
  - `heki_diagnostics`：`Diagnostic`・`RelatedLocation`・`# heki: ignore[...]` パーサ・行番号非依存 `RatchetBaseline`（テスト実証済み）。
  - `heki_rules` / `heki_profile` / `heki_lsp` / `heki` CLI（`check`, `server`/`lsp`, `rule`, `snapshot`, `assumptions`, `dump-semantic`）。
- **先行検証ハーネス（`tests/`）**：
  - `tests/symtable_verify/verify_symtable.py`：CPython 3.12 / 3.14 / 3.15 実機 `symtable` 双方向突合ツール＋フィクスチャ。
  - `tests/difftest/counterexamples.py`：S9b・S22・S23・S24・S26・S27・`dcg` `{**other, **d}` の実行可能反例スイート。
  - `tests/semantic/fixtures.py`：Stage 0-A ＋ `dcg` 由来の `MUT101`/`MUT201`/`MUT202`/`EFF101`/`EFF102` TP/TN 回帰フィクスチャ。

---

## 1. マイルストーン一覧（推奨実装順）

### M1: `heki_semantic` — スコープ・束縛・モジュール解決・CHA【完了・0 不一致実証済み】
- **実装対象**：`crates/heki_semantic/`
  - Python 3.11〜3.15 全対応（3.11 非インライン内包表記、3.12+ PEP 709 内包表記インライン化＆レキシカル隔離・クラス本体例外、3.13、3.14+ PEP 649 `__annotate__` スコープ、3.15+ PEP 810 `lazy import` 束縛、`global`/`nonlocal`、クロージャ自由変数、walrus `:=`、`match` パターン束縛、PEP 695 型パラメータ、軽量 CHA、`TYPE_CHECKING`）。
- **完了条件（Acceptance Gate）**：
  1. `cargo test -p heki_semantic` 全合格。
  2. CPython **3.11 / 3.12 / 3.13 / 3.14 / 3.15** 全 5 バージョンで `tests/symtable_verify/fixtures/` および `datamodel-code-generator` (`src/` 422 ファイル / 19.6 万行) に対して **不一致 0 件**。

### M2: `heki_ir` — SSA-CFG IR ビルダー【完了・`dcg` 421 ファイル全件ゼロパニック実証済み】
- **実装対象**：`crates/heki_ir/`（`lib.rs` + `builder.rs`）＋ `heki assumptions` / `heki dump-ir` CLI
  - Braun et al. (2013) SSA-CFG 構築（左から右の評価順序、明示的 `exc_target` 例外エッジ、`PhiNode` 挿入・自明 Phi 除去、自然ループ・`LoopBound`・`LoopCarriedInfo`、`rpo_blocks()` / `predecessors()`）。
  - **実装・検証済み不変条件**：
    - `for` / `async for` の `for.iter` と `MakeIter` はループ前ブロック（深度 `d`）、`IterNext` と本体は深度 `d + 1` に厳密分離。
    - `DictLiteral` の `DictEntry::Unpack` 順序保持（`{**d, ...}` vs `{**other, **d}`）。
    - ネストクラスのメソッドは独立 `ClassId` を持ち、親クラスへ漏らさない。
    - `is_awaited: bool`、`AugAssign` の `in_place: bool`、クロージャ `captured_vars`、`MakeGenExpr` 外側イテラブル、`A1Hazard` 収集、`heki assumptions` 並列走査コマンド動作化。
- **達成実績**：
  1. `cargo test -p heki_ir` 9/9 全合格（警告 0 件）。
  2. `datamodel-code-generator` (`src/` 421 ファイル / 19.6 万行) に対して `heki assumptions` を Python 3.15（15,398 関数 / **0.45s**）および Python 3.11（17,390 関数 / **0.18s**、debug ビルド）で実行し、パニック 0 件・構文エラー 0 件・実 A1 ハザード 15 件検出を実証。

### M3: `heki_kb` — Layer 1 / Layer 2 TOML ローダー＆番兵検証【完了・CPython 3.11〜3.15 全版 0 失敗実証済み】
- **実装対象**：`crates/heki_kb/`, `crates/heki_env/` (`[tool.heki.ecosystem]`), `kb/soundness/builtins.toml` (41 件), `kb/ecosystem/stdlib.toml` (55 件) + `web_db_data.toml` (105 件), `tests/sentinel/verify_soundness_kb.py`
  - 組み込み TOML を `include_str!` で内包＋`#[serde(deny_unknown_fields)]` で Layer 1/2 の相互混入を物理拒否。
  - 実機敵対テストにより `builtins.str` / `builtins.bytes` が `__str__`/`__bytes__` の返すサブクラスをそのまま返すことを実証し、`return_exact_type` を意図的に外して番兵テストで根拠を固定化。
- **達成実績**：`cargo test -p heki_kb` 12/12・`heki_env` 3/3 全合格 ＋ `verify_soundness_kb.py` CPython 3.11〜3.15 全 5 版 0 失敗（壊した KB での 10 件違反検出も確認済み）。

### M4: `heki_effect` — コールグラフ・統一 `EffectSummary`・増分インデックス【完了・19 テスト＋5 変異テスト全合格】
- **実装対象**：`crates/heki_effect/`（`lib.rs`, `resolver.rs`, `summary.rs`, `scc.rs`, `callgraph.rs`, `report.rs`）
  - 4 段階軽量レシーバ型解決（ローカル生成・直接/相対 import・`src/` レイアウト・re-export → PEP 484 → `__init__` `self.attr` → CHA 上限 8 件・`super()`）。
  - 反復版 Tarjan SCC 逆トポロジカル順 Worklist による統一 `EffectSummary` 不動点計算（深さ 20 万非再帰検証済み、ファイル跨ぎ再帰 SCC の効果減少追随、Span 無視 Early Cutoff）。
  - `OffloadsBlocking` ユーザーラッパー伝播（`in_thread` パターン）、`sync_driven_only` 緩和、同期関数呼び出しの即時実行伝播（`await sync_helper()` 捕捉＋コルーチン生成のみ非伝播）、Boundary-Only Reporting ＋ Root-Cause Grouping（閾値 10 件）。
- **達成実績**：`cargo test -p heki_effect` 19/19 全合格（2.9 全 5 ゲート＋5 規則の変異テスト検証済み）、`cargo test --workspace` 50/50 全合格・警告 0 件。

### M5: 族 1（`MUT101`, `MUT201`, `MUT202`）＆ 族 2（`EFF101`–`103`, `EFF105`, `DATA101`）＋ CLI / Ratchet【完了・`dcg` 19.6 万行 1.72 秒実戦検証済み】
- **実装対象**：`crates/heki_rules/`（`selection.rs`, `mut_rules.rs`, `eff_rules.rs`, `data_rules.rs`）、`crates/heki_diagnostics/`、`crates/heki/src/check.rs`、`crates/heki_ast/`（tokenizer ベース `comment_spans()`）
  - 8 ルール実装：`MUT101`（4 厳格フィルタ＋ローカル書込 1 回報告＋全呼び出し元無効化済み private helper 除外＋オーバーライド同名集約）、`MUT201`（5 書込経路＋最外側 1 回報告＋自己 `__init__` 除外）、`MUT202`（`readonly-returns`＋`@lru_cache`/`@cache`＋`copy_sanitizer`/`model_copy(deep=True)`）、`EFF101`/`EFF105`/`EFF102`/`EFF103`/`DATA101`。
  - CLI `heki check`：`rayon` 並列、`ignore` 走査、5 フォーマッタ（`full`/`concise`/`json`/`github`/`sarif`）、行単位 `# heki: ignore[...]` / `# noqa:` 抑止、`--baseline`/`--update-baseline` Ratchet、終了コード `0 / 1 / 2`（レビュー時に `--config` 外置き時の `root` 決定も `find_root(&first)` へ修正済み）。
- **達成実績**：
  1. `cargo test --workspace` **78/78 全合格・警告 0 件**（`tests/semantic/fixtures.py` TP 6/6 全検出・TN 7/7 誤検出 0 件の統合＋CLI E2E テスト含む）。
  2. 実プロダクションドッグフーディング（`datamodel-code-generator` `src/` 421 ファイル / 19.6 万行、release ビルド）：**1.72 秒**で完走、`MUT101` 16 件（`self.raw_obj` 等の実再代入 8 箇所＋`_empty_enum_locations`/`_unread_patterns`）、`MUT201` で `DataModel.fields`/`base_classes`/`DataType.data_types` への直接・コンテナ・関数跨ぎ（`_copy_data_type`, `_link_parts`, ネスト関数）書込を捕捉、`EFF101`/`EFF105` で `await in_thread(self.open)` 誤検出 0 件（`bodies.py:112:85` の `else self.open()` のみ的確に検出）を実証。

### M6: `heki_soundness` ＆ 族 3（`OPT101`–`103`, `OPT110`–`130`）＋ `--fix` / `snapshot`【完了・反例 24 件全ブロック＆差分テスト 0 誤変換実証済み】
- **実装対象**：`crates/heki_soundness/`（`interpreter.rs`, `liveness.rs`, `obligations.rs`, `opt_rules.rs`, `edits.rs`）, `crates/heki_rules/` (`snapshot.rs`, `OPT`), `crates/heki/` (`--fix`, `--diff`, `--profile`, `snapshot`)
  - RPO Worklist 抽象解釈（`ExactTypeVal`, `UniqueState` 上限 4, `points_to`, `alloc_elems`, `derived_refs` 推移的エスケープ, `ExclusiveAccessAtWrite`, `ValueLoopInvariant`, `HashConsistentProbe`, 組み込みシャドーイング保護）。
  - 宣言的 3 値 Proof Obligation（`Established` / `Blocked` / `Disproved`）＋ Safety × Profitability 直交判定。既定 `--fix` は **`safe` かつ `unconditional`（`OPT101`, `OPT102` 先頭累積 `{**d, ...}` 限定）のみ**。
  - `MUT103`（`heki snapshot [summary] --write`、`direct ∪ transitive` 実行効果対応）と `--profile`（py-spy / Tachyon `--collapsed` / `--gecko`）。
- **達成実績**：
  1. `tests/difftest/counterexamples.py` (7 件)・`opt_rewrites.py` (7 件)・`soundness_e2e_counterexamples.py` (17 件) 全件 `Blocked` / `Disproved`、義務剥奪テスト全合格、CPython 3.15 実機差分テスト誤変換 0 件。
  2. `cargo tree -p heki_soundness` に `heki_effect` 0 件（ADR-004 物理分離維持）。
  3. `dcg` (`src/` 421 ファイル / 19.6 万行) で既定 `--diff` 0 誤変換（`_target_binding.py:465` `{**site, **found}` ブロック）、`--include-size-dependent` で `_openapi_wire_plan.py:231` の `deque` 最適化を正確に提案。

### M7: `heki_lsp` — 増分 LSP サーバー（`heki server` / `heki lsp`）＋ `--watch`【完了・ADR-017 ガード＆増分実測済み】
- **実装対象**：`crates/heki_lsp/`（`engine.rs`, `server.rs`, `convert.rs`；`tower-lsp` + `tokio`）、`crates/heki_rules/src/project.rs`、`crates/heki/` (`--watch`)
  - 起動時バックグラウンド `ProjectIndex` 構築（IR・SemanticModel 非保持をソースレベルガードテストで強制）。
  - `didChange`（250ms debounce）で単一ファイル再解析、`didSave` 時に `ProjectIndex::update_file` Early Cutoff ＋ `decls` 変化時の依存先再抽出。
  - `DiagnosticRelatedInformation`（UTF-16 レンジ）、`quickfix`（`OPT101`/`OPT102`）、`refactor.rewrite.heki`（proven-safe size-dependent）、`# heki: ignore[CODE]` Code Action。
- **達成実績**：JSON-RPC E2E 統合テスト 8 本＋`--watch` E2E 8 本全合格、`dcg` 大規模ワークスペース p90 ファイル編集応答 ~45ms。

### M8: E2E メイン 100% カバレッジテスト配備（`t-linter` / `dcg` 流）【完了・到達可能コード 100%（行 99.98% / 関数 100%）達成】
- **実装対象**：`crates/heki/tests/`（`e2e_frontend.rs`, `e2e_advisory_cli.rs`, `e2e_advisory_effects.rs`, `e2e_advisory_mut.rs`, `e2e_soundness.rs`, `cli_check.rs`, `cli_fix.rs`, `cli_watch.rs`, `lsp_server.rs`）、`tests/corpus/frontend_tour*.py`、`tests/difftest/soundness_e2e_counterexamples.py`
  - 実バイナリ（`CARGO_BIN_EXE_heki`）＋実 LSP プロセス E2E を主軸とし、モック・ユニットテストは異常系・上限・不変条件ガード・義務剥奪のみに限定。到達不能なデッドコードを整理・削除。
  - 未カバー 3 行は `crates/heki_ir/src/builder.rs` の代入・拡張代入・`del` ターゲット match の `_ => {}` 分岐のみ（`ruff_python_parser` が構文エラーとして弾くため到達不能）。CI で `--fail-uncovered-lines 3` ラチェットを強制。
- **達成実績**：`cargo llvm-cov --workspace --fail-uncovered-lines 3` 合格（行 99.98% / 関数 100.00%）、32 テストバイナリ・全テスト合格、`verify_symtable.py` (`fixtures` + `corpus`) CPython 3.11〜3.15 全版 0 不一致。

### M9: CI/CD ＆ ユーザー向けドキュメント整備（`t-linter` / `dcg` 参考）【完了・actionlint/zizmor 0 指摘】
- **実装対象**：
  1. **`.github/workflows/`**（全 Action コミット SHA 固定、Cloudflare 設定なし）：`ci.yml`（rustfmt / clippy `-D warnings` / ADR-004・009 チェック / Linux & macOS テスト / `llvm-cov --fail-uncovered-lines 3` / CPython 3.11〜3.15 マトリクス）、`release.yml`（maturin 6 ターゲット wheel + sdist + wheel smoke test + PyPI trusted publishing + 任意 `PUBLISH_CRATES`）、`docs.yml`（`build_docs.py --check` / `build_llms_txt.py --check` / Zensical build）。
  2. **ユーザー向けドキュメント**：`README.md`, `zensical.toml`, `docs/`（Getting Started, Configuration, 17 ルールページ, Fixes, Adoption, LSP, Coding Agents, CLI Reference, Development, `llms.txt`, `llms-full.txt`）、`crates/heki_rules/rule-docs/<CODE>.md` ＆ `heki rule --output-format json` 単一ソース化。
- **達成実績**：`build_docs.py --check` / `build_llms_txt.py --check` / `actionlint` / `zizmor --offline` すべて 0 指摘。

---

## 2. v1.x 次期候補ルール（Stage 0-A Wave 2 実測に基づく優先順）

339 リポジトリ（1,902 万行）実測＋目視監査済みの優先度：
1. **高優先（採用確定・v1.x 第一弾）**：
   - **`MUT107`**（`__hash__` 依存属性の書換）：成熟 OSS **21.2%**、目視精度 ~83%（`MUT101` と同じ lazy-init / メモ除外で ~95% 見込み）。
   - **`OPT112`**（ループ内 `list.index` / `list.remove`）：全リポジトリ **52.5%**・成熟 OSS **81.4%**。`ExactType` / 型付き `list` 限定で高信号。
   - **`OPT113`**（ネストループ等結合 `for a in A: for b in B: if a.k == b.k:`）：成熟 OSS **10.2%**、精度 ~90%。
2. **狭く厳格なら採用（v1.x オプトイン）**：
   - **`EFF106`**（型確定した同期 `threading.Lock` / DB `.begin()` 保持中の `await` / HTTP）：2.1% だが発生時は致命的デッドロック。文字列一致禁止・型解決必須。
   - **`MUT106`**（SSA で再束縛されていない単一パス `Iterator`/`Generator` の二重消費）。
   - **`EFF107`**（Ruff `RUF006` と重複する裸の `asyncio.create_task()` は除外し、ローカル変数束縛後の非エスケープ消失のみ）。
3. **不採用（Wave 2 実測により却下）**：
   - `MUT104` のモジュール `ALL_CAPS` 定数書換（100% が意図的レジストリ／キャッシュだったため不採用；`@lru_cache` 戻り値破壊のみ `MUT202` に統合）。
   - `EFF108`（Ruff `SIM115` と重複＋`__enter__`/`Popen` ノイズ過多）。
   - `MUT105`（反復中別名変更は SSA 下では極めて稀かつ `heki_soundness` の `derived_refs` がカバー）。
