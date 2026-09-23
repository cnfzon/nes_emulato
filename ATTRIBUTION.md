# 引用與致謝

本專案是大學「應用軟體設計」課程的期末專案：一個支援 rollback 連線雙人對戰
的 NES 模擬器（Rust）。

## 參考教學

模擬核心（`crates/nes-core`）的架構與實作順序參考了 bugzmanov 撰寫的線上教學
**"Writing NES Emulator in Rust"**：

- 教學網站：<https://bugzmanov.github.io/nes_ebook/>
- 對應的 GitHub repository：<https://github.com/bugzmanov/nes_ebook>

本專案**沒有**複製該教學或其 repository 的任何程式碼；只是把它當作學習
6502 CPU / PPU 實作順序的參考資料。專案的所有權結構（`Nes -> Cpu -> Bus ->
{Ppu, Apu, Cartridge, Joypad}`）、外部驅動每幀 API、rollback netplay 層都是
本專案自行設計，教材本身並不涵蓋 netplay。

### 參考了教學的檔案清單

以下檔案在檔頭 doc comment 註明了參考來源（只參考「先做定址模式表、再做
opcode 表、再做 `step()`」這種實作順序/敘事結構，實際的型別設計、opcode
表內容、每條指令的執行邏輯、trace 格式都是依 nestest.log 與公開的 6502
opcode 參考資料自行重新設計/實作）：

- [`crates/nes-core/src/cpu/mod.rs`](crates/nes-core/src/cpu/mod.rs) ——
  參考 bugzmanov 教學第 3 章
  （<https://bugzmanov.github.io/nes_ebook/chapter_3.html>）的實作順序。
  非官方 opcode 的行為對照 NESdev wiki 的
  "[CPU unofficial opcodes](https://www.nesdev.org/wiki/CPU_unofficial_opcodes)"
  頁面整理而成。

## 測試資料來源

- **nestest**（`roms/nestest/`，不進 repo）：`nestest.nes` 與 `nestest.log`
  下載自 <https://www.qmtpro.com/~nes/misc/>，這是 NESdev wiki nestest 頁面
  引用的來源。
- **SingleStepTests**（`roms/singlestep/`，不進 repo）：
  <https://github.com/SingleStepTests/65x02> 的 `nes6502/v1` 子集（256 個
  opcode，每個 10,000 筆單指令測試）。

## 授權

TODO：本專案的授權條款尚未決定，由專案負責人（課程學生）在繳交前選定並補上
根目錄的 `LICENSE` 檔案。在授權確認之前，請勿將本 repository 的程式碼用於
教學/課程作業以外的用途。

## 第三方依賴

所有第三方 crate 依賴列在各 `Cargo.toml` 中，版本與授權條款以
[crates.io](https://crates.io) 上各自套件頁面為準。

## 內嵌字型

`nes-app` 的 egui GUI（選單、Debugger 面板）需要顯示繁體中文，但 egui 內建
預設字型不含 CJK 字符，因此內嵌了一套開源 CJK 字型作為 fallback：

- **字型**：Noto Sans CJK TC（思源黑體 繁體中文，Regular 字重）
- **來源**：<https://github.com/notofonts/noto-cjk>，檔案取自
  `Sans/OTF/TraditionalChinese/NotoSansCJKtc-Regular.otf`
  （由 Google 與 Adobe 共同開發；與 Google Fonts 上的 "Noto Sans TC" 屬同一
  字族）
- **授權**：SIL Open Font License 1.1（OFL-1.1），全文見
  [`crates/nes-app/assets/fonts/OFL.txt`](crates/nes-app/assets/fonts/OFL.txt)
  （取自該 repo 的 `Sans/LICENSE`）。OFL-1.1 允許免費使用、修改、subset、
  嵌入軟體並重新散布（含商業用途），唯不可單獨販售字型本身。
- **存放位置**：
  [`crates/nes-app/assets/fonts/NotoSansCJKtc-Regular.otf`](crates/nes-app/assets/fonts/NotoSansCJKtc-Regular.otf)，
  透過 `include_bytes!` 內嵌進執行檔（見
  [`crates/nes-app/src/main.rs`](crates/nes-app/src/main.rs) 的
  `install_cjk_fonts`），在 `eframe::run_native` 啟動時以
  `egui::Context::set_fonts` 加入 proportional/monospace family 的
  fallback 清單尾端 —— 拉丁字母/數字仍優先用 egui 預設字型，只有中文等
  字符才會落到這套字型。
- 目前內嵌的是完整字重（未做字符 subset），檔案約 16MB。
