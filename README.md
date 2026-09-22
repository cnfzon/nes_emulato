# nes-netplay

一個用 Rust 寫的 NES 模擬器，目標是支援「rollback 連線雙人對戰」。這是大學
「應用軟體設計」課程的期末專案，涵蓋作業系統（多執行緒／timing）、視窗環境
（egui GUI）、網路環境（UDP rollback netplay）、以及整合設計四大主題。

> **目前狀態：Phase 0（專案骨架）。** CPU / PPU / APU 尚未實作，`nes-app`
> 顯示的是一張依幀數捲動的測試畫面，不是真正的遊戲畫面。詳見
> [`docs/architecture.md`](docs/architecture.md) 與各 crate 原始碼中的
> `TODO Phase 1` 註解。

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

需要 Rust stable（見 `rust-toolchain.toml`，edition 2024）。

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
```

## 注意事項

- **不要把 `.nes` ROM 檔案放進這個 repository**：這是 public repo，ROM 檔案
  涉及著作權問題。`.gitignore` 已經排除 `*.nes` 與 `roms/`。如果要在本機測試
  （例如 `nes-test nestest`/`blargg`），請把 ROM 放在被排除的資料夾裡，見
  [`crates/nes-test/README.md`](crates/nes-test/README.md)。
- 授權條款尚未決定，見 [`ATTRIBUTION.md`](ATTRIBUTION.md) 的 TODO。
