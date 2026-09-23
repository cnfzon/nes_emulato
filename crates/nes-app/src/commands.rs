//! UI 執行緒 <-> Emu 執行緒之間傳遞的訊息型別。
//!
//! 兩個方向各自只用一條 `crossbeam-channel`：UI -> Emu 傳 [`EmuCommand`]，
//! Emu -> UI 傳 [`EmuEvent`]。畫面本身不走這條 channel，而是透過
//! `triple_buffer`（見 `emu.rs`），避免每幀畫面資料被 channel 佇列積壓。

use std::path::PathBuf;

use nes_core::{Buttons, RomInfo};

/// UI 執行緒送給 Emu 執行緒的指令。
pub enum EmuCommand {
    LoadRom(Vec<u8>),
    SetInput(u8, Buttons),
    Pause,
    Resume,
    SaveState,
    LoadState,
    /// 開關 Debugger 面板要看的 `DebugSnapshot` 產生（見 `emu.rs` 的
    /// `debug_input`）。只在面板真的打開時才產生快照，避免面板關閉時
    /// 白白浪費每幀一次的複製成本。
    SetDebugEnabled(bool),
    /// 單步執行一條 CPU 指令。只在暫停狀態下有效（見
    /// `Nes::step_instruction` 的說明）；未暫停時 emu 執行緒回報
    /// [`EmuEvent::Error`]。
    StepInstruction,
    /// 單步執行一整幀（使用目前的輸入）。只在暫停狀態下有效。
    StepFrame,
    /// 從目前位置起執行接下來 `count` 條指令，並把每條指令執行前的 trace
    /// 行寫到 `path`。只在暫停狀態下有效；模擬狀態會真的前進 `count` 條指令。
    /// 檔案 I/O 由 nes-app 負責，`nes-core` 只提供 trace 字串。
    TraceToFile {
        count: u32,
        path: PathBuf,
    },
    Quit,
}

/// Emu 執行緒回報給 UI 執行緒的事件。
#[derive(Debug)]
pub enum EmuEvent {
    RomLoaded(RomInfo),
    Error(String),
    /// 每秒一次的 FPS 統計（只有 FPS；幀數請用 [`EmuEvent::FrameAdvanced`]，
    /// 否則幀數只會每秒更新一次，跟 Debugger 快照對不上）。
    FpsReport {
        fps: f64,
    },
    /// 模擬狀態的幀數改變了（每跑完一幀、單步一幀、讀檔、載入 ROM 都會送）。
    FrameAdvanced(u64),
    /// `TraceToFile` 完成。
    TraceWritten {
        path: PathBuf,
        lines: u32,
    },
}
