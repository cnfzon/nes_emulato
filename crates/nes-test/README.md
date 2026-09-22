# nes-test

`nes-core` 的命令列測試 / 除錯工具。

```
cargo run -p nes-test -- info <rom.nes>
cargo run -p nes-test -- nestest <rom.nes> <log.txt>   # 尚未實作
cargo run -p nes-test -- blargg <rom.nes>               # 尚未實作
```

## 取得測試 ROM（不進 repo！）

`nestest` 與 blargg 測試套件都是社群製作的自由發布測試 ROM，用來驗證
CPU/PPU/APU 的正確性，但它們仍然是二進位遊戲/測試檔，**不會被加進這個
public repo**（見根目錄 `.gitignore` 的 `*.nes` / `roms/` 規則）。要在本機
測試時，請自行下載並放在被 `.gitignore` 排除的資料夾（例如 `roms/`）：

- **nestest**：搜尋 "nestest.nes" 與其對照 log（`nestest.log`），常見於
  NESdev wiki 的 nestest 頁面。用法：
  `cargo run -p nes-test -- nestest roms/nestest.nes roms/nestest.log`
- **blargg 測試套件**（`cpu_dummy_reads`、`instr_test-v5`、
  `ppu_vbl_nmi`……等）：搜尋作者 "blargg" 在 NESdev wiki 上發布的測試 ROM
  合集。用法：`cargo run -p nes-test -- blargg roms/instr_test-v5/official_only.nes`

這兩個子命令目前（Phase 0）只會印出 `not implemented yet` 並以非零狀態碼
結束；實際比對邏輯排進 Phase 1（照 bugzmanov 教材實作 CPU 之後）。
