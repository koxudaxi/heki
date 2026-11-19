# AGENTS.md — `heki` 開発・引き継ぎルール

このリポジトリは Python 静的解析の第 3 層 **「`heki`（壁）：Effect & Alias 意味論チェッカー」**（Rust 製・単一バイナリ CLI ＋ 増分 LSP）です。
実装・改修を行うすべてのエージェントは、以下の不変条件と手順を厳守してください。

## 1. 必読ドキュメント（この順に読むこと）
1. `ARCHITECTURE.md`（Baseline v4.1：全体設計・ルール体系・CLI/Config/LSP 仕様）
2. `docs/IMPLEMENTATION_PLAN.md`（マイルストーン M1〜M7 と完了ゲート）
3. `docs/DETAILED_SPEC_M3_M7.md`（**M3〜M7 実装ブループリント**：API 契約・アルゴリズム・エッジケース・受け入れゲート）
4. `docs/ANALYSIS_AND_KB_SPEC.md`（IR・`EffectSummary`・`FrameState`・Proof Obligation・KB・実測データ）
5. `docs/DESIGN_HISTORY_AND_ADR.md`（ADR-001〜ADR-017：なぜ棄却したかの履歴）
6. `docs/HANDOFF_PROMPTS.md`（マイルストーン別実装プロンプト集・最終レビューチェックリスト）

## 2. 絶対に破ってはならない Architecture Invariants
1. **2 レーンの物理分離（ADR-004）**：
   - `crates/heki_soundness` は `crates/heki_effect` に **絶対に依存させない**。
   - `heki_soundness` は PEP 484 型注釈を信用せず、`EffectiveSoundnessKb`（Layer 1）と `RuntimeProfile` のみを使う。
2. **パーサーの完全隔離（ADR-009 / ADR-016）**：
   - `ruff_python_parser` / `ruff_python_ast` / `ruff_text_size` への直接依存は **`crates/heki_ast` のみ** に閉じ込める（バージョン `tag = "0.17.5"` 固定、Python 3.15 PEP 810 `lazy import` 対応維持）。
3. **Layer 1 純化 ＆ Layer 3 潜入禁止（ADR-005）**：
   - `kb/soundness/` に Advisory Effect を書かない。
   - `heki_effect` の Layer 3 オンデマンド外部解析は、標準ライブラリおよび Layer 2 登録済みパッケージ（`opaque_packages`）内部へ絶対に潜らない。
4. **Safety × Profitability 直交性（ADR-007 / ADR-011）**：
   - 既定の `heki check --fix` が自動適用してよいのは **`safe` かつ `unconditional`（`OPT101`, `OPT102` 先頭累積 `{**d, ...}` 限定）のみ**。
   - 独自デコレータ・独自契約 API・Lean 4・ネイティブ拡張・汎用インポート循環チェッカー（`tach` 領域）・個別フレームワーク勘ルールは **Non-Goal** であり追加しない。
5. **LSP 増分実行の前提（ADR-017）**：
   - `heki_effect::ProjectIndex` に IR 本体を常駐させず、軽量 `FileSummary` ＋ 逆コールグラフ ＋ Early Cutoff を維持する。
6. **Ruff / `t-linter` 統一 UX（ADR-017）**：
   - 設定キーは `[tool.heki]` kebab-case、抑止コメントは `# heki: ignore[...]`（主）＋ `# noqa:`（互換）、終了コードは `0 / 1 / 2`。

## 3. 必須検証コマンド
```bash
# Rust ワークスペース全体のビルド・テスト
cargo check --workspace
cargo test --workspace

# CLI 動作確認
cargo run -p heki -- rule --all

# CPython 3.12 / 3.14 / 3.15 symtable 双方向突合（M1 完了条件：不一致 0 件）
python3.15 tests/symtable_verify/verify_symtable.py --heki target/debug/heki tests/symtable_verify/fixtures/

# 敵対的 Soundness 反例スイート（M6 完了条件：全件 Blocked / Disproved）
python3.15 tests/difftest/counterexamples.py
```

## 4. ルール追加・変更時の鉄則
- 新ルール・フィルタ変更は必ず `tests/semantic/fixtures.py` または `tests/difftest/` に TP/TN 回帰テストを追加する。
- Stage 0-A で実測不採用となったルール（`MUT102a/b`, `DATA102`, `EFF104`, `MUT104` の `ALL_CAPS` 定数, `EFF108`）を根拠なく復活させない。
