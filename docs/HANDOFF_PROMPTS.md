# 他エージェント実装用プロンプト集（M3〜M7）＆最終レビュー手順

このドキュメントは、**Opus 5.5 等の別エージェントに M3〜M7 を 1 マイルストーンずつ高品質に実装させるためのコピペ用プロンプト集**である。
各マイルストーン完了後にコミット（または差分確認）を挟んで次へ進み、全完了後にアーキテクト側エージェントで最終レビュー・敵対的反例検証・実コードドッグフーディングを行う。

---

## 共通ルール（全マイルストーンで厳守）

- 既存ドキュメントの正本：[AGENTS.md](../AGENTS.md)、[ARCHITECTURE.md](../ARCHITECTURE.md)、[DETAILED_SPEC_M3_M7.md](DETAILED_SPEC_M3_M7.md)、[DESIGN_HISTORY_AND_ADR.md](DESIGN_HISTORY_AND_ADR.md)
- `DESIGN.md` は過去の参考資料であり**編集しない**。
- `crates/heki_ast`・`crates/heki_semantic`・`crates/heki_ir` は M1/M2 で検証済み。後方互換を壊す変更は行わない。
- `unsafe_code = "forbid"` を維持する。

---

## Prompt 1: Milestone M3（`heki_kb` ローダー・KB 拡充・番兵検証）

```markdown
リポジトリ `/Users/koudai/work/pyopt` の Milestone M3（`heki_kb`）を実装してください。

### 必読ファイル
1. `AGENTS.md`
2. `docs/DETAILED_SPEC_M3_M7.md` のセクション 0 と セクション 1（M3）
3. `crates/heki_kb/src/lib.rs`, `crates/heki_env/src/lib.rs`, `kb/soundness/builtins.toml`, `kb/ecosystem/stdlib.toml`

### 実装タスク
1. `crates/heki_env/src/lib.rs` に `[tool.heki.ecosystem]`（`EcosystemOverride`）設定を追加し、TOML パースのユニットテストを追加する。
2. `kb/soundness/builtins.toml`（Layer 1）をセクション 1.3 の全必須エントリで拡充する。**Advisory 用フィールドは 1 つも入れず、`#[serde(deny_unknown_fields)]` で物理強制すること。**
3. `kb/ecosystem/stdlib.toml`（および必要なら `kb/ecosystem/web_db_data.toml`）をセクション 1.4 の全必須カタログと `opaque_packages` で拡充する（`subprocess.Popen` 本体はブロッキング登録しないこと）。
4. `crates/heki_kb/src/lib.rs` に `EffectiveSoundnessKb::load_builtin(PythonVersion)` / `lookup` と `EffectiveEcosystemKb::load_builtin(&[EcosystemEntry])` / `lookup` を `include_str!` で実装する。
5. `tests/sentinel/verify_soundness_kb.py` を作成し、CPython 3.12〜3.15 で `retained_in_return` と `return_exact_type`、および禁止フィールド不在を動的検証する。

### 完了条件（すべて実行して結果を確認すること）
- `cargo test --workspace`（全テスト合格、警告 0 件）
- `python3.15 tests/sentinel/verify_soundness_kb.py`（0 失敗）
```

---

## Prompt 2: Milestone M4（`heki_effect` コールグラフ・統一 `EffectSummary`・増分インデックス）

```markdown
リポジトリ `/Users/koudai/work/pyopt` の Milestone M4（`heki_effect`）を実装してください。

### 必読ファイル
1. `AGENTS.md`
2. `docs/DETAILED_SPEC_M3_M7.md` のセクション 0 と セクション 2（M4）
3. `crates/heki_effect/src/lib.rs`, `crates/heki_ir/src/lib.rs`, `crates/heki_kb/src/lib.rs`

### 実装タスク
1. `crates/heki_effect/` に以下を実装する（モジュール分割推奨：`resolver.rs`, `summary.rs`, `callgraph.rs`, `scc.rs`）：
   - プロジェクト内モジュール名・相対 import 解決（`src/` レイアウト対応）
   - 4 段階軽量レシーバ型解決（Priority 1 ローカル生成/import → Priority 2 PEP 484 注釈 → Priority 3 `__init__` `self.attr` → Priority 4 CHA 上限 8 件）
   - 1 関数ローカル効果抽出（Direct Exec Effects、`is_awaited` 除外、`offloads_blocking_params`、Self Attr Reads/Mutates/Invalidations、Param Mutations、`mutated_param_paths`、`typed_field_write_sites`）
   - Tarjan SCC 逆トポロジカル順 Worklist による統一 `EffectSummary` 不動点伝播（`CallHop` 最短チェーン保持、`in_thread` 型オフロードラッパー伝播、関数跨ぎ `mutated_param_paths` 伝播、`sync_driven_only` 判定）
   - Boundary-Only Reporting ヘルパーと Root-Cause Grouping（閾値 10 件）
   - `ProjectIndex` との接続（IR 本体を保持せず `FileSummary` のみ保持＋Early Cutoff 維持）
2. 禁止事項：Layer 3 探索を実装する場合、標準ライブラリと `EffectiveEcosystemKb::is_opaque_to_layer3` 登録パッケージ内部へ絶対に潜らないこと。

### 完了条件（すべて実行して結果を確認すること）
- `cargo test -p heki_effect` でセクション 2.9 の 5 項目すべてをカバーするユニットテストが全合格
- `cargo test --workspace` 全合格・警告 0 件
```

---

## Prompt 3: Milestone M5（族 1 `MUT` & 族 2 `EFF`/`DATA` ルール ＋ `heki check` CLI / Ratchet）

```markdown
リポジトリ `/Users/koudai/work/pyopt` の Milestone M5（族 1・族 2 ルール ＋ CLI / Ratchet）を実装してください。

### 必読ファイル
1. `AGENTS.md`
2. `docs/DETAILED_SPEC_M3_M7.md` のセクション 3（M5）
3. `tests/semantic/fixtures.py`
4. `crates/heki_rules/src/lib.rs`, `crates/heki_diagnostics/src/lib.rs`, `crates/heki/src/main.rs`

### 実装タスク
1. `crates/heki_rules/` に 8 ルールを実装する：
   - `MUT101`（2 段初期化除外・メモ辞書除外・互いに素な定数 dict キー除外・ネストクラス `owner_class` 分離の 4 フィルタ必須）
   - `MUT201`（`write-owners`：直接属性・別名コンテナ破壊・`setattr`/`__dict__`・`del`・間接 helper 経由 ＋ `forbid_cached_property_on`）
   - `MUT202`（`readonly-returns` ＋ `@lru_cache`/`@cache` 自動推論 ＋ `copy_sanitizer` 遮断）
   - `EFF101`, `EFF105`（opt-in）, `EFF102`, `EFF103`, `DATA101`（`IrFunction.loop_carried` の `self_accumulating == true` 必須）
2. `crates/heki_diagnostics/` に 5 フォーマッタ（`full`, `concise`, `json`, `github`, `sarif`）、行単位 `# heki: ignore[...]` / `# noqa:` 抑止、Ratchet ベースライン TOML 読込／書出を実装する。
3. `crates/heki/src/main.rs` の `heki check` を完全配線する（`rayon` 並列解析、`ignore` クレート、ルール選択、`--baseline` / `--update-baseline`、終了コード `0 / 1 / 2`）。
4. `tests/semantic/fixtures.py` の全 TP を検出し全 TN を非検出とする統合テストを追加する。

### 完了条件（すべて実行して結果を確認すること）
- `cargo test --workspace` 全合格・警告 0 件
- `tests/semantic/fixtures.py` 統合テスト 100% 合格（TP 全検出・TN 0 誤検出）
- 可能であれば実 Python コードベースに対して `heki check` を実行し、パニック 0 件・速度・出力形式を確認
```

---

## Prompt 4: Milestone M6（`heki_soundness` & 族 3 `OPT` ＋ `--fix`/`--diff` / `MUT103` `snapshot`）

```markdown
リポジトリ `/Users/koudai/work/pyopt` の Milestone M6（`heki_soundness` & `OPT` & `--fix` & `MUT103`）を実装してください。

### 必読ファイル
1. `AGENTS.md`（特に Architecture Invariants 1・3・4 を厳守）
2. `docs/DETAILED_SPEC_M3_M7.md` のセクション 4（M6）
3. `tests/difftest/counterexamples.py`
4. `crates/heki_soundness/Cargo.toml`, `crates/heki_soundness/src/lib.rs`, `crates/heki_profile/src/lib.rs`

### 絶対に破ってはならない禁止事項
- `crates/heki_soundness/Cargo.toml` に `heki_effect` を **絶対に追加しないこと**。
- `heki_soundness` は `ParamInfo.annotation`, `return_annotation`, `local_annotations` を **一切参照しないこと**（S9b 反例）。
- `IrFunction.a1_hazards` 非空の関数は全 `OPT` 候補を即 `SafetyTier::Blocked` とすること。
- 既定の `heki check --fix` は **`SafetyTier::Safe` かつ `ProfitabilityTier::Unconditional`（`OPT101`, `OPT102` 先頭累積 `{**d, ...}` のみ）** に限定すること。

### 実装タスク
1. `crates/heki_soundness/` に RPO Worklist 抽象解釈（`IrFunction::rpo_blocks()`）、逆方向 Liveness 解析（`exc_target` 込み）、`ExclusiveAccessAtWrite`、`ValueLoopInvariant`、宣言的 3 値 `ProofObligation` 評価器を実装する。
2. `OPT101`, `OPT102`, `OPT103`, `OPT110`, `OPT111`, `OPT120`, `OPT121`, `OPT130` の Safety × Profitability 判定と最小 byte-span `TextEdit` 生成を実装する。
3. `heki check --fix` / `--diff` / `--include-size-dependent` / `--profile`、および `heki snapshot --write` + `MUT103` 差分検出を配線する。
4. 自動テストを 3 種類追加する：
   - `tests/difftest/counterexamples.py` の全 7 反例（S23, S24, S26, `{**other, **d}`, S27, S9b, S22）が全件 `Blocked` または `Disproved` になるテスト。
   - 義務剥奪テスト（Obligation-Stripping Test）：各 Proof Obligation を無効化すると対応反例が通過する証明。
   - `OPT101` / `OPT102` 正例を `--fix` したコードが CPython 3.15 で元コードと完全同一挙動を示す差分テスト。

### 完了条件（すべて実行して結果を確認すること）
- `cargo tree -p heki_soundness` に `heki_effect` が一切含まれないこと
- `cargo test --workspace` 全合格・警告 0 件
- `python3.15 tests/difftest/counterexamples.py` 全合格
```

---

## Prompt 5: Milestone M7（`heki_lsp` 増分 LSP サーバー `heki server`）

```markdown
リポジトリ `/Users/koudai/work/pyopt` の Milestone M7（`heki_lsp`）を実装してください。

### 必読ファイル
1. `AGENTS.md`（特に Architecture Invariant 5: IR 非保持・Early Cutoff）
2. `docs/DETAILED_SPEC_M3_M7.md` のセクション 5（M7）
3. `crates/heki_lsp/Cargo.toml`, `crates/heki_lsp/src/lib.rs`, `crates/heki/src/main.rs`

### 実装タスク
1. `tower-lsp` + `tokio` を導入し、`crates/heki_lsp/` に stdio LSP サーバーを実装して `heki server` / `heki lsp` に配線する。
2. 起動時バックグラウンド `ProjectIndex` 構築（IR 本体は保持しない）、`didChange`（250ms debounce 単一ファイル再診断）、`didSave` 時の `ProjectIndex::update_file` による Early Cutoff 増分伝播を実装する。
3. `DiagnosticRelatedInformation`（呼び出しチェーン各ホップ）、`OPT` Quick Fix / `refactor.rewrite.heki`、および `# heki: ignore[CODE]` Code Action を実装する。
4. JSON-RPC 統合テスト（`initialize` → `didOpen` → `publishDiagnostics` → `codeAction` → `didChange` Early Cutoff → `shutdown`）を追加する。

### 完了条件
- `cargo test --workspace` 全合格・警告 0 件
```

---

## 最終レビュー手順（実装完了後にアーキテクト側エージェントへ渡すチェックリスト）

M3〜M7 実装完了後、以下の観点でレビューを行う：

1. **Architecture Invariants 監査**：
   - `cargo tree -p heki_soundness` に `heki_effect` なし
   - `cargo tree` で `ruff_python_*` 直接依存が `heki_ast` のみ
   - `heki_soundness` コードベース内に `.annotation` / `return_annotation` / `local_annotations` 参照が 0 件
   - `kb/soundness/` に Advisory フィールドが 0 件
   - `heki_effect` の Layer 3 が stdlib / `opaque_packages` に潜っていないこと
   - 既定 `--fix` が `safe && unconditional`（`OPT101` / `OPT102` 先頭累積）以外に一切適用されないこと
   - `heki_lsp` / `ProjectIndex` が `IrFunction` を常駐させていないこと
2. **全自動ゲート実行**：
   - `cargo test --workspace`
   - `python3.15 tests/symtable_verify/verify_symtable.py --heki target/debug/heki tests/symtable_verify/fixtures/`
   - `python3.15 tests/sentinel/verify_soundness_kb.py`
   - `python3.15 tests/difftest/counterexamples.py`
3. **`datamodel-code-generator` (`src/` 421 ファイル / 19.6 万行) 実戦ドッグフーディング**：
   - 実行速度計測（release ビルド目標 < 1〜2 秒）
   - `MUT101`（`jsonschema.py` 実バグ的中・他ノイズ目視精査）
   - `MUT201`（`GenerationStore` 境界違反＋別名/間接ケース捕捉）
   - `EFF101`/`EFF105`（`in_thread` / `run_sync` 誤検出 0 件確認）
   - `OPT101`/`OPT102`（`_target_binding.py:465` `{**other, **found}` が確実にブロックされることを実ファイルで確認！）

---

## Prompt 6: Milestone M8（E2E メイン 100% カバレッジテスト配備）

```markdown
リポジトリ `/Users/koudai/work/pyopt` の Milestone M8（E2E メイン 100% カバレッジ配備）を実装してください。

### 必読ファイル
1. `AGENTS.md`
2. `docs/IMPLEMENTATION_PLAN.md` の M8

### テスト設計原則（`t-linter` / `dcg` 流・厳守）
- **正常系・エッジケースはすべて実バイナリ（`CARGO_BIN_EXE_heki`）または実 LSP プロセス（`heki server --stdio`）に対する E2E テストでカバーすること。**
- **モック・直接ユニットテストは異常系（I/O エラー・権限エラー・破損設定/TOML/プロファイル・SCC 非収束上限・`MAX_JOIN_ALLOC_IDS` 超過・義務剥奪テスト・不変条件ガード）のみに限定すること。**
- 到達不能な死んだブランチを見つけた場合は、無理にモックで踏まずにコードを簡潔化・削除すること。
- `DESIGN.md` は編集しないこと。

### 実装タスク
1. `cargo llvm-cov` でワークスペース全体の未カバー行を計測する（Homebrew LLVM 利用時は `LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata` を指定）。
2. 全コマンド（`check` 全 17 ルール・5 出力形式・`--fix`/`--diff`/`--include-size-dependent`/`--profile`/`--baseline`/`--update-baseline`、`rule`、`assumptions`、`snapshot`、`server` JSON-RPC E2E、`dump-semantic`、`dump-ir`）を網羅する E2E テストを配備し、異常系テストで残余分岐をカバーする。
3. ワークスペース全体でラインカバレッジ **100%** を達成する。

### 完了条件（すべて実行して出力を確認すること）
- `cargo llvm-cov --workspace --show-missing-lines --fail-under-lines 100` 合格
- `cargo clippy --workspace --all-targets -- -D warnings` 0 警告
- `python3.15 tests/symtable_verify/verify_symtable.py --heki target/debug/heki tests/symtable_verify/fixtures/` 0 不一致
- `python3.15 tests/sentinel/verify_soundness_kb.py` 0 失敗
- `python3.15 tests/difftest/counterexamples.py` 全合格
```

---

## Prompt 7: Milestone M9（CI/CD ＆ ユーザー向けドキュメント整備 — `t-linter` / `dcg` 参考）

```markdown
リポジトリ `/Users/koudai/work/pyopt` の Milestone M9（CI/CD ＆ ユーザー向けドキュメント整備）を実装してください。

### 必読・参照対象
1. `AGENTS.md`, `ARCHITECTURE.md`, `docs/IMPLEMENTATION_PLAN.md`（M9）
2. 参考リポジトリ構成：`/Volumes/WD_Black_SN8100/work/t-linter`（Rust + `maturin` CLI/LSP・CI・ドキュメント構成）および `datamodel-code-generator`（GitHub Actions・生成ドキュメント同期・`llms.txt`・ルール/CLI リファレンス）
3. **Cloudflare Pages/Workers 等のデプロイ設定は今回作成しないこと（後からユーザー側で設定する）。**

### 実装タスク
1. `.github/workflows/` に CI / Release / Docs ワークフローを整備する：
   - `ci.yml`：`fmt`, `clippy -D warnings`, `test`, `cargo-llvm-cov` 100% ゲート、CPython 3.11〜3.15 マトリクス（`symtable` / `sentinel` / `counterexamples`）、ADR-004 依存検証（`cargo tree -p heki_soundness`）。
   - `release.yml`：`maturin` (`bindings = "bin"`) マルチプラットフォーム wheel ビルド＋PyPI / crates.io 公開フロー。
   - `docs.yml`：ドキュメントビルド検証＋生成ドキュメント（ルール一覧・CLI ヘルプ・`llms.txt`）同期チェック。
2. `README.md`, `LICENSE`（MIT）, エンドユーザー向けドキュメント（Getting Started, Installation, CLI Reference, Configuration, Rules Reference 17 ルール, LSP / Coding Agent Integration, `llms.txt`）を整備する。既存の設計文書（`docs/DETAILED_SPEC_M3_M7.md` 等）は開発者設計ドキュメントとして保持し、干渉しない構成にする。

### 完了条件（すべて実行して確認すること）
- CI ワークフローの YAML 構文チェックおよびローカルで実行可能な全チェックコマンドが通過すること
- ドキュメントビルドおよび生成ドキュメント同期チェックが通過すること
```
