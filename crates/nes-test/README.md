# nes-test

`nes-core` 的命令列測試 / 除錯工具。

```
cargo run -p nes-test -- info <rom.nes>
cargo run -p nes-test -- nestest <rom.nes> <log.txt> [--strict]
cargo run -p nes-test -- blargg <rom.nes> [--max-frames N] [--screen]
cargo run -p nes-test -- screenshot <rom.nes> <out.png> [--frames N] [--scale S]
cargo run -p nes-test -- golden <rom.nes> [--frames N]
cargo run -p nes-test -- replay info <replay>
cargo run -p nes-test -- replay verify <rom.nes> <replay>
cargo run -p nes-test -- replay generate <rom.nes> <out.replay> [--frames N] [--seed S] [--interval K] [--reset-at F]...
cargo run -p nes-test -- diff-state <a.state> <b.state>
cargo run -p nes-test -- save-state <rom.nes> <out.state> [--replay <replay>] [--frames N]
```

Phase 4a 的 replay 與除錯工具（規格見 [`docs/architecture.md`](../../docs/architecture.md) §18）：

- `info` 也會印出 ROM 的 `rom_id`（整個檔案的 xxh3-128；前 16 字元是 UI／CLI 的顯示用）。
- `replay info`：header、總幀數、reset 次數與位置、檢查點數量。
- `replay verify`：從開機依 replay 的輸入重播（關閉輸出以加速），驗證全部檢查點。通過 → 結束碼 0；不符、
  版本或 ROM 不符、檔案損毀 → 非 0，並回報第一個不符的檢查點與「分歧可能開始的幀範圍」。
- `replay generate`：依偽隨機的雙人輸入腳本錄一份 replay（測試與效能量測用；真正的 replay 由 GUI 錄製）。
- `diff-state`：逐欄位比對兩份存檔，輸出 `路徑: 左 vs 右`（例如 `ppu.v: 0x2104 vs 0x2105`）；大型陣列只列出
  不同的索引範圍。結束碼：相同 0、有差異 1、無法讀取／解碼 2。
- `save-state`：跑到指定幀數（可依 replay 的輸入）後寫出存檔，用來產生 `diff-state` 的比對對象。

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
  合集。目前實際使用的是 GitHub 上的整理版（見下方與 `ATTRIBUTION.md`）；`official_only.nes`
  這類多合一版本需要 MMC1，尚不支援，請用 `rom_singles/` 裡的單一 ROM。

## blargg 子命令

實作 blargg 測試 ROM 的 `$6000` 結果協定：`$6001-$6003` 是簽章 `DE B0 61`，
`$6000` 為 `$80` 表示執行中、`$81` 表示需要 reset（等 ≥100ms 後 reset）、其他是
結果碼（0 = 通過），`$6004` 起是以 `\0` 結尾的結果文字。結束碼：通過 0、失敗/逾時 1。

2005 年的舊版 ROM（`blargg_ppu_tests_2005.09.15b`、`sprite_hit_tests_2005.10.05`）
沒有這個協定，只把結果印在畫面上；此時 `blargg` 會讀 nametable 0 的文字（tile
編號當 ASCII）判讀 `$01`（通過）或 `PASSED`。

Phase 3.5 起也用它跑 APU 與中斷測試（`apu_test/rom_singles/*`、`apu_test/apu_test.nes`、
`blargg_apu_2005.07.30/*`（2005 舊版，判讀畫面上的 `$01`）、`apu_reset/*`（會要求 reset）、
`cpu_interrupts_v2/rom_singles/*`）；結果與預期失敗的原因見 `docs/architecture.md` §14.7。

用到的公開 test ROM 與取得方式見根目錄 `ATTRIBUTION.md`（放在被 gitignore 的
`roms/nes-test-roms/`）。單一 ROM 的例子：

```
cargo run --release -p nes-test -- blargg roms/nes-test-roms/instr_test-v5/rom_singles/01-basics.nes
cargo run --release -p nes-test -- blargg roms/nes-test-roms/ppu_vbl_nmi/rom_singles/01-vbl_basics.nes
```

結果表與失敗原因：`docs/architecture.md` §14。

## screenshot / golden 子命令

- `screenshot`：跑 N 幀（不按任何鍵）後把畫面存成 PNG（不依賴任何額外套件，
  用未壓縮的 zlib stored block 自己編碼）。視覺除錯用。
- `golden`：跑 N 幀後印出最後一幀畫面的 xxh3-64 雜湊，用來產生
  `crates/nes-core/tests/golden_frames.rs` 的黃金畫面表（只存雜湊，不存圖片）。
