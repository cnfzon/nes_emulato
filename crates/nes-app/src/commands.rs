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
    /// 開關 Debugger 的 PPU 影像（pattern table / nametable）產生：`Some(p)`
    /// 表示要（`p` 是 0–7 的 pattern table 調色盤選擇），`None` 表示不要。
    /// 這些影像要畫 6 張圖，只在對應分頁可見時才開，且執行中每隔幾幀才更新一次。
    SetDebugViews(Option<u8>),
    /// 單步執行一條 CPU 指令。只在暫停狀態下有效（見
    /// `Nes::step_instruction` 的說明）；未暫停時 emu 執行緒回報
    /// [`EmuEvent::Error`]。
    StepInstruction,
    /// 單步執行一整幀（使用目前的輸入）。只在暫停狀態下有效。
    StepFrame,
    /// 設定聽得到的 APU 聲道（`nes_core::apu::CHANNEL_*` 位元，1 = 聽得到）。只影響混音，
    /// 不影響模擬狀態；換 ROM 之後 emu 執行緒會重新套用。
    SetAudioChannelMask(u8),
    /// 從目前位置起執行接下來 `count` 條指令，並把每條指令執行前的 trace
    /// 行寫到 `path`。只在暫停狀態下有效；模擬狀態會真的前進 `count` 條指令。
    /// 檔案 I/O 由 nes-app 負責，`nes-core` 只提供 trace 字串。
    TraceToFile {
        count: u32,
        path: PathBuf,
    },
    Quit,
    /// 只給測試用的屏障：emu 執行緒處理到這個指令、且本輪之前的所有指令的結果（快照、事件、
    /// 音訊）都已發布之後，才對 `ack` 送出一個訊號。命令是依序處理的，所以收到 ack 就代表
    /// 先前送出的指令都完成了——測試不必再靠 `sleep` 猜 emu 執行緒處理完了沒有。
    #[cfg(test)]
    Barrier(crossbeam_channel::Sender<()>),
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
