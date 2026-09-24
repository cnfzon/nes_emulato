# CLAUDE.md — 本專案的長期規則

## 專案簡介

大學「應用軟體設計」課程期末專案：用 Rust 寫的 NES 模擬器，目標是支援
rollback 連線雙人對戰。課程四大主題與對應模組（精簡版；細節、執行緒模型、
決定性規則、rollback 流程見 [`docs/architecture.md`](docs/architecture.md)）：

1. 作業系統與應用程式的關係（多執行緒、檔案 I/O、timing）→ `nes-app` 的 emu 執行緒與 channel / triple buffer
2. 視窗環境 → `nes-app`（eframe/egui）
3. 網路環境 → `nes-net`（UDP + 手刻協定 + rollback 排程）
4. 整合設計 → `Nes::run_frame` 作為 core / net / app 的交會點

Workspace：`nes-core`（模擬核心）、`nes-net`、`nes-app`（GUI）、`nes-test`（CLI 測試工具）。

## 工作流程規則

- **不得 `git commit` / `git push` / 建立 branch。** 所有變更留在 working tree，由使用者 review 後自行提交。
- **新增任何依賴（crate）之前必須先問使用者**，說明用途、授權與替代方案。
- **不得下載任何商業遊戲 ROM。** 測試 ROM（nestest、blargg 等自由發布者）只放在已被 gitignore 的 `roms/`，絕不進 repo。
  公開 test ROM 的來源與取得方式見 `ATTRIBUTION.md`（`roms/nes-test-roms/`）。
- **參考教學（例如 bugzmanov 的 "Writing NES Emulator in Rust"）的檔案，必須在檔頭 doc comment 標註來源，並同步更新 `ATTRIBUTION.md`。**

## nes-core 的硬性限制

- `#![forbid(unsafe_code)]`。
- 不得有 I/O、讀取時間、亂數、執行緒，也不得依賴 GUI／網路 crate。
- 完全決定性：相同輸入序列必須得到相同狀態；不得在影響狀態的邏輯中迭代 `HashMap`。
- **`run_frame` 路徑上不得 panic**（含 `unwrap`／`expect`／越界索引／算術溢位）；異常情況要用確定的方式處理。
- `Nes::step_instruction` 會打破以幀為單位的決定性，只能在暫停時使用，netplay 進行中不得呼叫。

## 測試基準與階段驗收

- **目前基準（Phase 2 結束）：167 通過 + 1 忽略**：nes-app 7、nes-core（lib）131 + 1 ignored、
  nes-core `golden_frames` 1、nes-net 9、`transport_roundtrip` 1、nes-test 18。
- 每個階段結束時，以 `cargo test --workspace` 的**實際輸出**逐一列出每個執行檔的測試數量。**數量只能增加**；若有測試被移除或被 cfg 排除，必須說明理由。
- 每個階段的驗收指令（全部要跑並回報結果）：
  1. `cargo build --workspace`
  2. `cargo test --workspace`
  3. `cargo clippy --workspace --all-targets -- -D warnings`
  4. `cargo fmt --all -- --check`
  5. nestest：`cargo run -p nes-test -- nestest roms/nestest/nestest.nes roms/nestest/nestest.log`
  6. SingleStepTests 閘門：`cargo test --release -p nes-core --lib cpu::singlestep -- --ignored --nocapture`
     （官方、非官方穩定、JAM 暫存器/RAM 必須 100%；分類見 `docs/architecture.md` §10）
  7. blargg 測試結果表：對 `roms/nes-test-roms/` 底下的 test ROM 跑
     `cargo run --release -p nes-test -- blargg <rom>`（`instr_test-v5/rom_singles`、
     `ppu_vbl_nmi/rom_singles`、`oam_read`、`sprite_hit_tests_2005.10.05`、
     `blargg_ppu_tests_2005.09.15b`），與 `docs/architecture.md` §14 比對，不得退步；
     預期失敗的項目要個別說明原因，**不要硬湊到通過**。
  8. 黃金畫面：`cargo test --workspace` 已包含 `golden_frames`（需要 `roms/nes-test-roms/`，
     缺檔時略過）與 `Nes` 的 `golden_frame_hash_of_rendering_rom`。畫面雜湊改變時，先確認變動是
     預期的，再用 `nes-test golden <rom> --frames N` 重新產生並更新雜湊。

## 建置與交付

- 交付與效能量測一律使用 `cargo build --release -p nes-app`，不要用 `--workspace`
  （Cargo feature 統一會讓 `nes-core` 帶入 `testing` feature；見 README）。

## 回報規範

- 一律使用**繁體中文**回報。
- 明確區分兩類結論：**「實際執行驗證過」**（附指令與實際輸出數字）與**「從程式碼推論」**（未執行驗證）。不得把推論寫成已驗證。
- **需要 GUI 才能驗證的項目**（面板顯示、按鈕行為、字型渲染等），必須列出手動測試步驟與預期結果，交給使用者確認，不得宣稱已驗證。
