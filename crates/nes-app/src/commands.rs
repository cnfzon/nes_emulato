//! UI 執行緒 <-> Emu 執行緒之間傳遞的訊息型別。
//!
//! 兩個方向各自只用一條 `crossbeam-channel`：UI -> Emu 傳 [`EmuCommand`]，
//! Emu -> UI 傳 [`EmuEvent`]。畫面本身不走這條 channel，而是透過
//! `triple_buffer`（見 `emu.rs`），避免每幀畫面資料被 channel 佇列積壓。

use std::path::PathBuf;

use nes_core::{Buttons, ReplayMismatch, RomId, RomInfo};

/// replay 播放速度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackSpeed {
    /// 正常速度（60.0988 Hz，有聲音）。
    X1,
    /// 兩倍速（靜音）。
    X2,
    /// 盡可能快（靜音；畫面只更新每個時間片的第一幀）。
    Max,
}

/// UI 執行緒送給 Emu 執行緒的指令。
pub enum EmuCommand {
    LoadRom(Vec<u8>),
    SetInput(u8, Buttons),
    Pause,
    Resume,
    SaveState,
    /// 讀取記憶體中的存檔。錄製或播放 replay 時會被拒絕（會破壞「從開機狀態依輸入序列執行」）。
    LoadState,
    /// soft reset（Emulation 選單的 Reset）。**不是**直接改動 `Nes`：它變成下一幀 `FrameInput`
    /// 的 `reset` 旗標，所以錄製時會被記進 replay。播放 replay 時忽略。
    Reset,
    /// 重新開機（power-on）並開始錄製 replay。
    StartRecording,
    /// 結束錄製，回報 [`EmuEvent::RecordingFinished`]（replay 的位元組）。
    StopRecording,
    /// 從 replay 的位元組開始播放：重新開機、依 replay 的輸入逐幀執行並驗證檢查點。
    StartReplay(Vec<u8>),
    StopReplay,
    SetPlaybackSpeed(PlaybackSpeed),
    /// 把目前狀態的存檔位元組交給 UI（存成檔案，供 `nes-test diff-state` 使用）。
    ExportState,
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

/// 錄製／播放的狀態（狀態列用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Idle,
    Recording {
        frames: u32,
    },
    Playing {
        frame: u32,
        total: u32,
        /// 已驗證通過的檢查點數。
        verified: u32,
        checkpoints: u32,
    },
    /// 播放完成，全部檢查點相符。
    Finished {
        total: u32,
        checkpoints: u32,
    },
    /// 檢查點不符：播放已停止並暫停。
    Mismatch(ReplayMismatch),
}

/// Emu 執行緒回報給 UI 執行緒的事件。
#[derive(Debug)]
pub enum EmuEvent {
    RomLoaded(RomInfo, RomId),
    /// 錄製／播放的狀態改變（錄製與播放中每幀都會送）。
    Session(SessionStatus),
    /// emu 執行緒自己改變了暫停狀態（播放完成或檢查點不符時會自動暫停）。
    Paused(bool),
    /// 錄製結束：完整的 replay 位元組，由 UI 存成檔案（emu 執行緒不做檔案 I/O）。
    RecordingFinished {
        bytes: Vec<u8>,
        frames: u32,
    },
    /// `ExportState` 的結果。
    StateExported(Vec<u8>),
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
