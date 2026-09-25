//! 每一幀的輸入：兩個搖桿的按鍵，加上「這一幀開始前按下 reset 鍵」。
//!
//! `reset` 是輸入的一部分，所以 replay 與 netplay 只要交換 `FrameInput` 序列，就能重現
//! 包含 reset 在內的所有操作。它在 `Nes::run_frame` 內、該幀開始前執行 soft reset
//! （CPU／PPU／APU 的 reset 訊號，RAM 與卡帶內容維持不變），不是重新開機。

use crate::joypad::Buttons;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct FrameInput {
    pub p1: Buttons,
    pub p2: Buttons,
    /// 這一幀開始前按下 reset 鍵（soft reset）。
    pub reset: bool,
}

impl FrameInput {
    /// 沒有按任何鍵、沒有 reset。
    pub const NONE: FrameInput = FrameInput {
        p1: Buttons::empty(),
        p2: Buttons::empty(),
        reset: false,
    };

    pub const fn new(p1: Buttons, p2: Buttons) -> Self {
        Self {
            p1,
            p2,
            reset: false,
        }
    }

    /// 同樣的按鍵，但這一幀開始前先 reset。
    pub const fn with_reset(mut self) -> Self {
        self.reset = true;
        self
    }
}

/// 舊的呼叫方式（`[p1, p2]`）等同於「沒有 reset」。
impl From<[Buttons; 2]> for FrameInput {
    fn from(pads: [Buttons; 2]) -> Self {
        FrameInput::new(pads[0], pads[1])
    }
}
