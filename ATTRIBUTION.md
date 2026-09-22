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

## 授權

TODO：本專案的授權條款尚未決定，由專案負責人（課程學生）在繳交前選定並補上
根目錄的 `LICENSE` 檔案。在授權確認之前，請勿將本 repository 的程式碼用於
教學/課程作業以外的用途。

## 第三方依賴

所有第三方 crate 依賴列在各 `Cargo.toml` 中，版本與授權條款以
[crates.io](https://crates.io) 上各自套件頁面為準。
