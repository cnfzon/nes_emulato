# nes-netplay

一個用 Rust 寫的 NES 模擬器，目標是支援「rollback 連線雙人對戰」。這是大學
「應用軟體設計」課程的期末專案，涵蓋作業系統（多執行緒／timing）、視窗環境
（egui GUI）、網路環境（UDP rollback netplay）、以及整合設計四大主題。

> **目前狀態：Phase 3.5（APU 與音訊輸出；模擬核心正式凍結）。** CPU 通過 nestest（8991
> 行逐指令比對，含 `--strict`）與 SingleStepTests 回歸閘門；PPU 以 scanline 為單位渲染
> （含 loopy 捲動、精靈、8×16、sprite 0 hit、NMI）；**APU 五個聲道、frame counter、frame／DMC IRQ、
> DMC 抓取樣本暫停 CPU 都已實作**（blargg 的 `apu_test`、`blargg_apu_2005.07.30`、`apu_reset` 全數通過），
> 音訊經 cpal 輸出（lock-free 環形緩衝區 + ±0.5% 動態速率控制，目標延遲約 50 ms）。支援 mapper 0
> （NROM）、1（MMC1）、2（UxROM）、3（CNROM）。CPU 與 PPU 的時序模型（instruction-level + 分段
> catch-up）已定案，存檔開頭有 magic 與兩個版本號（格式、模擬行為）——之後任何會改變模擬結果的修改都必須
> 遞增 `CORE_BEHAVIOR_VERSION`，規則見 [`docs/architecture.md`](docs/architecture.md) §13–§17。
> **尚未以真實遊戲驗證**（尤其是聲音要用耳朵確認），手動測試清單見
> [`docs/manual-test-phase2.md`](docs/manual-test-phase2.md)、
> [`docs/manual-test-phase3.md`](docs/manual-test-phase3.md)、
> [`docs/manual-test-phase3_5.md`](docs/manual-test-phase3_5.md)。
>
> **Phase 4a（決定性基礎設施）**：`run_frame` 改收 `FrameInput { p1, p2, reset }`（reset 是輸入的一部分）；
> 與存檔格式無關的行為指紋 `Nes::behavior_fingerprint`；ROM 識別碼 `rom_id`（整個檔案的 xxh3-128）；
> replay（開機狀態 + 每幀輸入 + 指紋檢查點）可在 GUI 錄製／播放／驗證、用 `nes-test replay verify` 驗證；
> 雙實例逐幀比對工具與 `diff-state` 供 4b／4c 的 desync 除錯使用。模擬行為**沒有改變**
> （`CORE_BEHAVIOR_VERSION` 仍是 3；只有存檔格式升到 3）。規格見 `docs/architecture.md` §18，
> 需要 GUI 確認的項目見 [`docs/manual-test-phase4a.md`](docs/manual-test-phase4a.md)。

模擬核心的實作順序參考了 bugzmanov 的教學《Writing NES Emulator in Rust》
（<https://bugzmanov.github.io/nes_ebook/>），但沒有複製其程式碼；細節見
[`ATTRIBUTION.md`](ATTRIBUTION.md)。

## 目錄結構

```
nes-netplay/
├── crates/
│   ├── nes-core/   決定性的模擬核心（CPU/PPU/APU/Cartridge），無 I/O 依賴
│   ├── nes-net/    Rollback netplay：協定、UDP 傳輸層、會話排程
│   ├── nes-app/    eframe GUI 前端（UI 執行緒 + Emu 執行緒）
│   └── nes-test/   命令列測試／除錯工具（iNES header 解析、nestest/blargg）
└── docs/
    └── architecture.md   架構文件（課程主題對應表、執行緒模型、rollback 流程）
```

## 建置方式

需要 Rust stable（見 `rust-toolchain.toml`，edition 2024）。Linux 另需 ALSA 開發標頭
（`libasound2-dev`，cpal 的後端；見 `.github/workflows/ci.yml`）。

```bash
# 建置整個 workspace
cargo build --workspace

# 跑所有測試
cargo test --workspace

# 靜態檢查
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# 啟動 GUI
cargo run -p nes-app

# 解析一份 iNES ROM 的 header
cargo run -p nes-test -- info path/to/rom.nes

# 跑 blargg test ROM（$6000 結果協定；舊版 ROM 會判讀畫面文字）
cargo run --release -p nes-test -- blargg roms/nes-test-roms/instr_test-v5/rom_singles/01-basics.nes

# 跑 N 幀後存成 PNG／印出畫面雜湊（視覺除錯、黃金畫面）
cargo run --release -p nes-test -- screenshot path/to/rom.nes out.png --frames 120
cargo run --release -p nes-test -- golden path/to/rom.nes --frames 120

# replay：檢視、驗證全部檢查點（結束碼 0 = 通過）、產生偽隨機輸入的 replay
cargo run --release -p nes-test -- replay info recording.replay
cargo run --release -p nes-test -- replay verify path/to/rom.nes recording.replay
cargo run --release -p nes-test -- replay generate path/to/rom.nes demo.replay --frames 3600 --reset-at 1800

# 逐欄位比對兩份存檔（GUI：File → Save State to File...；CLI：save-state）
cargo run --release -p nes-test -- diff-state a.state b.state

# run_frame 效能（CPU + PPU + APU 合計；輸出開啟與關閉各量一次；也量行為指紋的耗時）
cargo run --release -p nes-core --features testing --example bench_run_frame

# 音訊診斷：不開視窗、音量 0（不出聲），在真實音訊裝置上跑 12 秒並印出緩衝區／underrun 統計
cargo run --release -p nes-app -- --audio-selftest
```

遊戲操作（`nes-app`）：

| | 玩家 1 | 玩家 2 |
|---|---|---|
| 方向 | 方向鍵 | W A S D |
| B / A | Z / X | F / G |
| Select / Start | 右 Shift / Enter | R / T |

F5 存檔、F9 讀檔（存在記憶體，不寫磁碟）。選單 **Emulation → Reset**（soft reset）；選單 **Replay**：
錄製（會先重新開機）／停止並儲存／播放（1x、2x、最快，播放時鍵盤輸入被忽略、逐幀驗證檢查點）。**錄製與播放
replay 期間停用**讀取存檔（F9）、單步指令、Trace 與載入別的 ROM（它們會破壞「從開機狀態依輸入序列執行」的
前提）。選單 **Audio**：靜音與主音量；Debugger 的 **APU** 分頁：
各聲道獨立靜音、暫存器與計數器、音訊緩衝區填充量與累計 underrun。

## 交付與效能量測

交付或量測效能時，請用：

```bash
cargo build --release -p nes-app
```

而不是 `cargo build --release --workspace`。原因是 Cargo 的 feature 統一機制
（feature unification）：`nes-test` 依賴 `nes-core` 時啟用了 `testing`
feature，如果在同一次 cargo 呼叫中一起建置整個 workspace，`nes-core` 只會被
編譯一次、而且是「所有人要求的 feature 的聯集」，`nes-app` 用到的 `nes-core`
就會被帶進 `testing` feature（例如 `Nes::override_pc` 這類純測試用旁路 API），
交付的執行檔因此跟只建置 `nes-app` 的結果不同。只指定 `-p nes-app` 時
`nes-test` 不在這次建置內，`testing` 就不會被啟用。

## 注意事項

- **不要把 `.nes` ROM 檔案放進這個 repository**：這是 public repo，ROM 檔案
  涉及著作權問題。`.gitignore` 已經排除 `*.nes` 與 `roms/`。如果要在本機測試
  （例如 `nes-test nestest`/`blargg`），請把 ROM 放在被排除的資料夾裡，見
  [`crates/nes-test/README.md`](crates/nes-test/README.md)。
- 授權條款尚未決定，見 [`ATTRIBUTION.md`](ATTRIBUTION.md) 的 TODO。
