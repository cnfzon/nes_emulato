//! 雙實例測試工具（`testing` feature）：同時驅動兩個 `Nes`，逐幀比對行為指紋，找出第一個
//! 分歧的幀。
//!
//! 為 Phase 4b／4c 設計：`nes-net` 的測試要比對「連線的兩端」與「離線重播」是否得到逐幀相同
//! 的結果。三種用法：
//!
//! - [`run_both`]：給 ROM 與輸入序列，兩個實例吃同樣的輸入（驗證核心本身的決定性）。
//! - [`DualRunner`]：兩個實例各吃**各自**的輸入（例如 lockstep 兩端各自收到的輸入串流，或故意
//!   餵不同輸入來驗證分歧偵測）；也能接手已經存在的實例（`from_instances`，例如剛 `load_state`
//!   過的），並各自開關輸出（rollback 重跑時會關閉輸出，指紋必須不受影響）。
//! - [`fingerprint_trace`] + [`first_divergence`]：兩端各自獨立跑（甚至在不同執行緒／行程），
//!   各自收集每幀指紋，最後再比對；離線重播用 `Replay::inputs()` 餵同一個函式。
//!
//! 「幀號」一律是「已完成的 `run_frame` 次數」（第 1 幀＝第一次 `run_frame` 之後）。

use crate::Nes;
use crate::error::RomError;
use crate::input::FrameInput;

/// 第一個指紋不同的幀。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Divergence {
    /// 已完成的幀數（第 1 幀＝第一次 `run_frame` 之後）。
    pub frame: u64,
    pub a: u64,
    pub b: u64,
}

/// 同時驅動兩個 `Nes` 並逐幀比對行為指紋。
#[derive(Debug, Clone)]
pub struct DualRunner {
    a: Nes,
    b: Nes,
    frames: u64,
    first: Option<Divergence>,
}

impl DualRunner {
    /// 兩個都是剛開機的實例。
    pub fn new(rom: &[u8]) -> Result<Self, RomError> {
        Ok(Self::from_instances(
            Nes::from_rom(rom)?,
            Nes::from_rom(rom)?,
        ))
    }

    /// 接手已經存在的兩個實例。幀號從 0 起算（相對於接手的時間點）；若接手時兩者的指紋
    /// 已經不同，第一次 `step` 就會回報分歧。
    pub fn from_instances(a: Nes, b: Nes) -> Self {
        Self {
            a,
            b,
            frames: 0,
            first: None,
        }
    }

    pub fn a(&self) -> &Nes {
        &self.a
    }

    pub fn b(&self) -> &Nes {
        &self.b
    }

    pub fn a_mut(&mut self) -> &mut Nes {
        &mut self.a
    }

    pub fn b_mut(&mut self) -> &mut Nes {
        &mut self.b
    }

    /// 分別設定兩個實例的輸出開關。
    pub fn set_output(&mut self, a: bool, b: bool) {
        self.a.set_output_enabled(a);
        self.b.set_output_enabled(b);
    }

    /// 這個 runner 已經一起跑了幾幀。
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// 兩個實例各跑一幀（各自的輸入），比對指紋。回傳**第一個**分歧（一旦出現就固定，
    /// 之後的幀仍會繼續跑，但回傳值不變）。
    pub fn step(&mut self, a: FrameInput, b: FrameInput) -> Option<Divergence> {
        self.a.run_frame(a);
        self.b.run_frame(b);
        self.frames += 1;
        let (fa, fb) = (self.a.behavior_fingerprint(), self.b.behavior_fingerprint());
        if self.first.is_none() && fa != fb {
            self.first = Some(Divergence {
                frame: self.frames,
                a: fa,
                b: fb,
            });
        }
        self.first
    }

    pub fn first_divergence(&self) -> Option<Divergence> {
        self.first
    }
}

/// 兩個剛開機的實例吃**同樣的**輸入序列，回傳第一個指紋不同的幀（`None` ＝ 全程一致）。
/// 出現分歧就立刻停止。
pub fn run_both(rom: &[u8], inputs: &[FrameInput]) -> Result<Option<Divergence>, RomError> {
    let mut runner = DualRunner::new(rom)?;
    for &input in inputs {
        if let Some(divergence) = runner.step(input, input) {
            return Ok(Some(divergence));
        }
    }
    Ok(None)
}

/// 用 `nes` 跑完 `inputs`，回傳每一幀結束時的行為指紋（長度 ＝ 輸入的幀數）。
pub fn fingerprint_trace(nes: &mut Nes, inputs: impl IntoIterator<Item = FrameInput>) -> Vec<u64> {
    inputs
        .into_iter()
        .map(|input| {
            nes.run_frame(input);
            nes.behavior_fingerprint()
        })
        .collect()
}

/// 比對兩份逐幀指紋，回傳第一個不同的幀。只比對共同的部分（長度不同但前綴一致回傳 `None`）；
/// 呼叫端需要的話自己檢查長度。
pub fn first_divergence(a: &[u64], b: &[u64]) -> Option<Divergence> {
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .map(|i| Divergence {
            frame: i as u64 + 1,
            a: a[i],
            b: b[i],
        })
}
