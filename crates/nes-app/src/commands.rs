//! UI 執行緒 <-> Emu 執行緒之間傳遞的訊息型別。
//!
//! 兩個方向各自只用一條 `crossbeam-channel`：UI -> Emu 傳 [`EmuCommand`]，
//! Emu -> UI 傳 [`EmuEvent`]。畫面本身不走這條 channel，而是透過
//! `triple_buffer`（見 `emu.rs`），避免每幀畫面資料被 channel 佇列積壓。

use nes_core::{Buttons, RomInfo};

/// UI 執行緒送給 Emu 執行緒的指令。
pub enum EmuCommand {
    LoadRom(Vec<u8>),
    SetInput(u8, Buttons),
    Pause,
    Resume,
    SaveState,
    LoadState,
    Quit,
}

/// Emu 執行緒回報給 UI 執行緒的事件。
pub enum EmuEvent {
    RomLoaded(RomInfo),
    Error(String),
    FpsReport { fps: f64, frame: u64 },
}
