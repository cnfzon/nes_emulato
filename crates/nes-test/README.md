# nes-test

`nes-core` 的命令列測試 / 除錯工具。

```
cargo run -p nes-test -- info <rom.nes>
cargo run -p nes-test -- nestest <rom.nes> <log.txt> [--strict]
cargo run -p nes-test -- blargg <rom.nes>               # 尚未實作
```

`nestest` 會從 nestest.nes 的 automation entry point（`$C000`）開始，逐指令
跟 `log.txt` 比對 PC、指令 bytes、A/X/Y/P/SP、CYC、PPU dot；預設不比對反組譯
文字（含 `= xx` 記憶體值標註），只顯示差異警告，加 `--strict` 才會把它也
納入比對結果。遇到第一個不一致就停止，印出行號、期望值、實際值與前 5 行
上下文。全部跑完後會檢查 `$02`/`$03` 是否為 0（nestest 官方指令測試的
錯誤碼）。

## 取得測試 ROM（不進 repo！）

`nestest`、blargg、SingleStepTests 都是社群製作的自由發布測試資料，用來
驗證 CPU/PPU/APU 的正確性，但它們仍然是二進位/大型資料檔，**不會被加進
這個 public repo**（見根目錄 `.gitignore` 的 `*.nes` / `roms/` 規則）。要在
本機測試時，請自行下載並放在被 `.gitignore` 排除的資料夾：

- **nestest**（`roms/nestest/`）：`nestest.nes` 與 `nestest.log` 下載自
  <https://www.qmtpro.com/~nes/misc/>（NESdev wiki nestest 頁面引用的來源）。
  ```
  curl -o roms/nestest/nestest.nes https://www.qmtpro.com/~nes/misc/nestest.nes
  curl -o roms/nestest/nestest.log https://www.qmtpro.com/~nes/misc/nestest.log
  cargo run -p nes-test -- nestest roms/nestest/nestest.nes roms/nestest/nestest.log
  ```
- **SingleStepTests**（`roms/singlestep/v1/`，選用）：
  <https://github.com/SingleStepTests/65x02> 的 `nes6502/v1` 子集，256 個
  opcode 各一個 JSON 檔（`00.json`..`ff.json`），每個 10,000 筆單指令測試，
  總共約 1GB。這份資料是 `nes-core` 內部（`#[cfg(test)]`）的
  `cpu::singlestep::singlestep_tests_all_opcodes` 測試在用，不是透過
  `nes-test` 這個 CLI 執行；下載到位後：
  ```
  cargo test --release -p nes-core --lib cpu::singlestep -- --ignored --nocapture
  ```
  資料量大、預設用 `#[ignore]` 排除在一般 `cargo test` 之外；沒有這份資料
  時該測試會直接印訊息跳過，不算失敗。
- **blargg 測試套件**（`cpu_dummy_reads`、`instr_test-v5`、
  `ppu_vbl_nmi`……等）：搜尋作者 "blargg" 在 NESdev wiki 上發布的測試 ROM
  合集。用法：`cargo run -p nes-test -- blargg roms/instr_test-v5/official_only.nes`

`blargg` 子命令目前只會印出 `not implemented yet` 並以非零狀態碼結束；
排進之後做 PPU（Phase 2）之後的階段。
