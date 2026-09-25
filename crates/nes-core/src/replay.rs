//! Replay：「開機狀態 + 輸入序列 + 行為指紋檢查點」。離線版的 netplay。
//!
//! 只處理位元組（編碼／解碼）與逐幀的錄製／播放邏輯，**不做任何檔案 I/O**（呼叫端負責）。
//! 位元組佈局是明確定義的（全部 little-endian，不依賴 Rust 的記憶體佈局），完整規格與語意見
//! `docs/architecture.md` §18.3。
//!
//! ```text
//! offset  size  欄位
//! 0       4     magic "NESR"
//! 4       2     replay 格式版本（REPLAY_FORMAT_VERSION）
//! 6       2     CORE_BEHAVIOR_VERSION（錄製時）
//! 8       16    rom_id（xxh3-128 的 canonical 位元組；整個 ROM 檔案）
//! 24      4     總幀數
//! 28      2     檢查點間隔（幀，預設 60，≥ 1）
//! 30      4     輸入段數 N
//! 34      7×N   輸入段：p1 (u8)、p2 (u8)、旗標 (u8，bit0 = reset，其餘必須為 0)、重複次數 (u32)
//! ..      4     檢查點數 M
//! ..      12×M  檢查點：幀號 (u32)、行為指紋 (u64)
//! ```
//!
//! **replay 不含存檔**（`docs/architecture.md` §15.6）：起點永遠是 `Nes::from_rom` 的開機狀態，
//! 所以存檔格式的改變不會使 replay 失效；只有 `CORE_BEHAVIOR_VERSION` 改變才會。
//!
//! # 幀號的約定
//!
//! 「第 `n` 幀」＝ 第 `n` 次 `run_frame`（從 1 起算）；「檢查點在第 `n` 幀」＝ 第 `n` 次 `run_frame`
//! 之後的指紋（`n = 0` 是開機狀態）。第 `n` 幀使用的輸入是輸入串流的第 `n − 1` 筆（0 起算）。

use std::fmt;
use std::ops::RangeInclusive;

use thiserror::Error;

use crate::Nes;
use crate::error::RomError;
use crate::input::FrameInput;
use crate::joypad::Buttons;
use crate::rom_id::RomId;
use crate::state::CORE_BEHAVIOR_VERSION;

pub const REPLAY_MAGIC: [u8; 4] = *b"NESR";
/// replay 位元組佈局的版本。
pub const REPLAY_FORMAT_VERSION: u16 = 1;
/// 預設的檢查點間隔（幀）：約每秒一個。
pub const DEFAULT_CHECKPOINT_INTERVAL: u16 = 60;

const HEADER_LEN: usize = 4 + 2 + 2 + 16 + 4 + 2 + 4;
const RUN_LEN: usize = 7;
const CHECKPOINT_LEN: usize = 12;
/// 輸入段旗標：這一幀開始前 reset。
const FLAG_RESET: u8 = 0x01;

/// 一段連續、相同的輸入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRun {
    pub input: FrameInput,
    /// 重複幾幀（≥ 1）。
    pub count: u32,
}

/// 第 `frame` 幀結束時的行為指紋。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    pub frame: u32,
    pub fingerprint: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    /// 錄製時的 `CORE_BEHAVIOR_VERSION`。
    pub core_behavior_version: u16,
    pub rom_id: RomId,
    pub total_frames: u32,
    pub checkpoint_interval: u16,
    pub runs: Vec<InputRun>,
    pub checkpoints: Vec<Checkpoint>,
}

/// 第一個對不上的檢查點，與分歧可能開始的範圍。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayMismatch {
    /// 第一個不符的檢查點的幀號。
    pub frame: u32,
    /// 上一個相符的檢查點的幀號（`None`＝連開機狀態的檢查點都不符或還沒有相符的）。
    pub last_good_frame: Option<u32>,
    pub expected: u64,
    pub actual: u64,
}

impl ReplayMismatch {
    /// 分歧可能開始的幀範圍（含頭尾，第 n 幀＝第 n 次 `run_frame`）：上一個相符的檢查點
    /// 之後，到不符的檢查點為止。`frame == 0`（開機狀態就不符）時是 `0..=0`。
    pub fn suspect_frames(&self) -> RangeInclusive<u32> {
        self.last_good_frame.map_or(0, |p| p + 1)..=self.frame
    }
}

impl fmt::Display for ReplayMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let range = self.suspect_frames();
        write!(
            f,
            "第 {} 幀的檢查點不符（預期指紋 {:#018x}，實際 {:#018x}）；",
            self.frame, self.expected, self.actual
        )?;
        if self.frame == 0 {
            write!(f, "開機狀態就不同（核心的初始狀態變了？）")
        } else {
            write!(
                f,
                "分歧發生在第 {}–{} 幀的執行之間（上一個相符的檢查點：{}）",
                range.start(),
                range.end(),
                self.last_good_frame
                    .map_or("無".to_string(), |p| format!("第 {p} 幀"))
            )
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReplayError {
    #[error("不是 replay 檔案（開頭不是 \"NESR\"）")]
    BadMagic,
    #[error("replay 檔案被截斷：{what}不完整")]
    Truncated { what: &'static str },
    #[error("不支援的 replay 格式版本 {found}（本程式支援 {supported}）")]
    UnsupportedFormat { found: u16, supported: u16 },
    #[error("檢查點間隔不可為 0")]
    BadInterval,
    #[error("輸入串流第 {index} 段的旗標 {flags:#04x} 含有未定義的位元")]
    BadFlags { index: u32, flags: u8 },
    #[error("輸入串流第 {index} 段的重複次數為 0")]
    ZeroRun { index: u32 },
    #[error("輸入串流的幀數總和 {sum} 與標頭的總幀數 {total} 不符")]
    FrameCountMismatch { sum: u64, total: u32 },
    #[error("第 {index} 個檢查點的幀號 {frame} 超過總幀數或沒有嚴格遞增")]
    BadCheckpoint { index: u32, frame: u32 },
    #[error("檔案尾端多出 {extra} 個位元組")]
    TrailingBytes { extra: usize },

    #[error(
        "replay 是用核心行為版本 {replay} 錄製的，目前的核心是版本 {current}：\
         同樣的輸入不保證得到同樣的結果，拒絕播放"
    )]
    CoreVersionMismatch { replay: u16, current: u16 },
    #[error(
        "replay 屬於另一份 ROM（replay 的 rom_id {}，目前載入的 {}）",
        replay.short(),
        current.short()
    )]
    RomMismatch { replay: RomId, current: RomId },
    #[error("replay 只能從開機狀態開始錄製與播放（目前的 Nes 已經執行過了）")]
    NotPowerOn,
    #[error("{0}")]
    Mismatch(ReplayMismatch),
    #[error("錄製已達幀數上限（{} 幀）", u32::MAX)]
    TooLong,
    #[error(
        "錄製與模擬不同步：已記錄 {recorded} 幀，但 Nes 已跑過 {nes_frames} 幀（有幀沒有記錄？）"
    )]
    OutOfSync { recorded: u32, nes_frames: u64 },
    #[error("無法載入 ROM：{0}")]
    Rom(#[from] RomError),
}

impl Replay {
    /// 展開成逐幀的輸入（第 `n` 幀的輸入是 `[n − 1]`）。
    pub fn inputs(&self) -> impl Iterator<Item = FrameInput> + '_ {
        self.runs
            .iter()
            .flat_map(|run| std::iter::repeat_n(run.input, run.count as usize))
    }

    /// 把逐幀的輸入壓縮成 RLE 輸入段（相鄰且相同的合併，單段最多 `u32::MAX` 幀）。
    pub fn compress(inputs: impl IntoIterator<Item = FrameInput>) -> Vec<InputRun> {
        let mut runs: Vec<InputRun> = Vec::new();
        for input in inputs {
            match runs.last_mut() {
                Some(last) if last.input == input && last.count < u32::MAX => last.count += 1,
                _ => runs.push(InputRun { input, count: 1 }),
            }
        }
        runs
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            HEADER_LEN + self.runs.len() * RUN_LEN + 4 + self.checkpoints.len() * CHECKPOINT_LEN,
        );
        out.extend_from_slice(&REPLAY_MAGIC);
        out.extend_from_slice(&REPLAY_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.core_behavior_version.to_le_bytes());
        out.extend_from_slice(self.rom_id.as_bytes());
        out.extend_from_slice(&self.total_frames.to_le_bytes());
        out.extend_from_slice(&self.checkpoint_interval.to_le_bytes());
        out.extend_from_slice(&(self.runs.len() as u32).to_le_bytes());
        for run in &self.runs {
            out.push(run.input.p1.bits());
            out.push(run.input.p2.bits());
            out.push(if run.input.reset { FLAG_RESET } else { 0 });
            out.extend_from_slice(&run.count.to_le_bytes());
        }
        out.extend_from_slice(&(self.checkpoints.len() as u32).to_le_bytes());
        for cp in &self.checkpoints {
            out.extend_from_slice(&cp.frame.to_le_bytes());
            out.extend_from_slice(&cp.fingerprint.to_le_bytes());
        }
        out
    }

    /// 解碼。對**任意**位元組都不會 panic：長度先檢查、再配置；不合法的內容回傳
    /// [`ReplayError`]。接受的位元組一定是規範形式（`decode` 後再 `encode` 得到同樣的位元組）。
    ///
    /// 只檢查結構與自我一致性；核心版本與 `rom_id` 是否與目前相符，由 [`ReplayPlayer::new`]
    /// 判斷（這樣 `nes-test replay info` 仍能顯示版本不同的 replay）。
    pub fn decode(bytes: &[u8]) -> Result<Replay, ReplayError> {
        let mut r = Reader { bytes, pos: 0 };
        if r.take(4, "標頭")? != REPLAY_MAGIC {
            return Err(ReplayError::BadMagic);
        }
        let format = r.u16("標頭")?;
        if format != REPLAY_FORMAT_VERSION {
            return Err(ReplayError::UnsupportedFormat {
                found: format,
                supported: REPLAY_FORMAT_VERSION,
            });
        }
        let core_behavior_version = r.u16("標頭")?;
        let mut id = [0u8; 16];
        id.copy_from_slice(r.take(16, "標頭")?);
        let total_frames = r.u32("標頭")?;
        let checkpoint_interval = r.u16("標頭")?;
        if checkpoint_interval == 0 {
            return Err(ReplayError::BadInterval);
        }
        let run_count = r.u32("標頭")?;

        // 先確認剩下的位元組夠放，再配置：避免惡意的「億個輸入段」造成巨量配置。
        let runs_bytes = u64::from(run_count) * RUN_LEN as u64;
        if runs_bytes + 4 > r.remaining() as u64 {
            return Err(ReplayError::Truncated {
                what: "輸入串流"
            });
        }
        let mut runs = Vec::with_capacity(run_count as usize);
        let mut sum = 0u64;
        for index in 0..run_count {
            let p1 = r.u8("輸入串流")?;
            let p2 = r.u8("輸入串流")?;
            let flags = r.u8("輸入串流")?;
            let count = r.u32("輸入串流")?;
            if flags & !FLAG_RESET != 0 {
                return Err(ReplayError::BadFlags { index, flags });
            }
            if count == 0 {
                return Err(ReplayError::ZeroRun { index });
            }
            sum += u64::from(count);
            runs.push(InputRun {
                input: FrameInput {
                    p1: Buttons::from_bits_retain(p1),
                    p2: Buttons::from_bits_retain(p2),
                    reset: flags & FLAG_RESET != 0,
                },
                count,
            });
        }
        if sum != u64::from(total_frames) {
            return Err(ReplayError::FrameCountMismatch {
                sum,
                total: total_frames,
            });
        }

        let checkpoint_count = r.u32("檢查點")?;
        if u64::from(checkpoint_count) * CHECKPOINT_LEN as u64 > r.remaining() as u64 {
            return Err(ReplayError::Truncated { what: "檢查點" });
        }
        let mut checkpoints: Vec<Checkpoint> = Vec::with_capacity(checkpoint_count as usize);
        for index in 0..checkpoint_count {
            let frame = r.u32("檢查點")?;
            let fingerprint = r.u64("檢查點")?;
            let ordered = checkpoints.last().is_none_or(|prev| frame > prev.frame);
            if frame > total_frames || !ordered {
                return Err(ReplayError::BadCheckpoint { index, frame });
            }
            checkpoints.push(Checkpoint { frame, fingerprint });
        }
        if r.remaining() != 0 {
            return Err(ReplayError::TrailingBytes {
                extra: r.remaining(),
            });
        }
        Ok(Replay {
            core_behavior_version,
            rom_id: RomId::from_bytes(id),
            total_frames,
            checkpoint_interval,
            runs,
            checkpoints,
        })
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], ReplayError> {
        if n > self.remaining() {
            return Err(ReplayError::Truncated { what });
        }
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, ReplayError> {
        Ok(self.take(1, what)?[0])
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, ReplayError> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, ReplayError> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self, what: &'static str) -> Result<u64, ReplayError> {
        let b = self.take(8, what)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }
}

/// 逐幀錄製：每跑完一幀就把那一幀的輸入交給它；每 `checkpoint_interval` 幀（以及第 0 幀與最後
/// 一幀）記錄一次行為指紋。
///
/// 只能從開機狀態開始（[`Nes::is_power_on_state`]）；錄製期間不得 `load_state`、不得
/// `step_instruction`（那會破壞「從開機狀態依輸入序列執行」的前提），[`record_frame`]
/// 會以幀數對不上偵測到沒有記錄的幀。
///
/// [`record_frame`]: ReplayRecorder::record_frame
#[derive(Debug, Clone)]
pub struct ReplayRecorder {
    rom_id: RomId,
    interval: u16,
    runs: Vec<InputRun>,
    frames: u32,
    checkpoints: Vec<Checkpoint>,
}

impl ReplayRecorder {
    pub fn new(nes: &Nes, checkpoint_interval: u16) -> Result<Self, ReplayError> {
        if checkpoint_interval == 0 {
            return Err(ReplayError::BadInterval);
        }
        if !nes.is_power_on_state() {
            return Err(ReplayError::NotPowerOn);
        }
        Ok(Self {
            rom_id: nes.rom_id(),
            interval: checkpoint_interval,
            runs: Vec::new(),
            frames: 0,
            checkpoints: vec![Checkpoint {
                frame: 0,
                fingerprint: nes.behavior_fingerprint(),
            }],
        })
    }

    /// 已記錄的幀數。
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// 記錄「剛剛跑完的那一幀」：`input` 是傳給 `run_frame` 的輸入，`nes` 是跑完之後的狀態。
    pub fn record_frame(&mut self, input: FrameInput, nes: &Nes) -> Result<(), ReplayError> {
        if self.frames == u32::MAX {
            return Err(ReplayError::TooLong);
        }
        if nes.frame_count() != u64::from(self.frames) + 1 {
            return Err(ReplayError::OutOfSync {
                recorded: self.frames,
                nes_frames: nes.frame_count(),
            });
        }
        match self.runs.last_mut() {
            Some(last) if last.input == input && last.count < u32::MAX => last.count += 1,
            _ => self.runs.push(InputRun { input, count: 1 }),
        }
        self.frames += 1;
        if self.frames.is_multiple_of(u32::from(self.interval)) {
            self.checkpoints.push(Checkpoint {
                frame: self.frames,
                fingerprint: nes.behavior_fingerprint(),
            });
        }
        Ok(())
    }

    /// 結束錄製：補上最後一幀的檢查點（若還沒有）。`nes` 必須正好是最後一次 `record_frame` 之後的狀態。
    pub fn finish(mut self, nes: &Nes) -> Result<Replay, ReplayError> {
        if nes.frame_count() != u64::from(self.frames) {
            return Err(ReplayError::OutOfSync {
                recorded: self.frames,
                nes_frames: nes.frame_count(),
            });
        }
        if self
            .checkpoints
            .last()
            .is_none_or(|cp| cp.frame != self.frames)
        {
            self.checkpoints.push(Checkpoint {
                frame: self.frames,
                fingerprint: nes.behavior_fingerprint(),
            });
        }
        Ok(Replay {
            core_behavior_version: CORE_BEHAVIOR_VERSION,
            rom_id: self.rom_id,
            total_frames: self.frames,
            checkpoint_interval: self.interval,
            runs: self.runs,
            checkpoints: self.checkpoints,
        })
    }
}

/// 逐幀播放並驗證檢查點。
///
/// 用法：`ReplayPlayer::new` 之後反覆呼叫 [`ReplayPlayer::step`]（它呼叫 `run_frame`、再驗證
/// 該幀的檢查點）；輸出開關由呼叫端決定（驗證模式關閉輸出以加速）。
#[derive(Debug, Clone)]
pub struct ReplayPlayer {
    replay: Replay,
    run: usize,
    used: u32,
    frame: u32,
    next_checkpoint: usize,
    last_good: Option<u32>,
    verified: u32,
}

impl ReplayPlayer {
    /// 版本或 `rom_id` 不符、或 `nes` 不是開機狀態時拒絕，並回傳可讀的錯誤。通過之後會先驗證
    /// 第 0 幀（開機狀態）的檢查點。
    pub fn new(replay: Replay, nes: &Nes) -> Result<Self, ReplayError> {
        if replay.core_behavior_version != CORE_BEHAVIOR_VERSION {
            return Err(ReplayError::CoreVersionMismatch {
                replay: replay.core_behavior_version,
                current: CORE_BEHAVIOR_VERSION,
            });
        }
        if replay.rom_id != nes.rom_id() {
            return Err(ReplayError::RomMismatch {
                replay: replay.rom_id,
                current: nes.rom_id(),
            });
        }
        if !nes.is_power_on_state() {
            return Err(ReplayError::NotPowerOn);
        }
        let mut player = Self {
            replay,
            run: 0,
            used: 0,
            frame: 0,
            next_checkpoint: 0,
            last_good: None,
            verified: 0,
        };
        player
            .check_checkpoints(nes)
            .map_err(ReplayError::Mismatch)?;
        Ok(player)
    }

    pub fn replay(&self) -> &Replay {
        &self.replay
    }

    /// 已經播放的幀數。
    pub fn frame(&self) -> u32 {
        self.frame
    }

    pub fn total_frames(&self) -> u32 {
        self.replay.total_frames
    }

    pub fn is_finished(&self) -> bool {
        self.frame >= self.replay.total_frames
    }

    /// 已驗證通過的檢查點數（含第 0 幀）。
    pub fn verified_checkpoints(&self) -> u32 {
        self.verified
    }

    pub fn checkpoint_count(&self) -> u32 {
        self.replay.checkpoints.len() as u32
    }

    /// 下一幀的輸入（並前進）；播完了回傳 `None`。
    fn next_input(&mut self) -> Option<FrameInput> {
        if self.is_finished() {
            return None;
        }
        loop {
            let run = self.replay.runs.get(self.run)?;
            if self.used < run.count {
                self.used += 1;
                return Some(run.input);
            }
            self.run += 1;
            self.used = 0;
        }
    }

    fn check_checkpoints(&mut self, nes: &Nes) -> Result<(), ReplayMismatch> {
        while let Some(cp) = self.replay.checkpoints.get(self.next_checkpoint) {
            if cp.frame > self.frame {
                break;
            }
            let actual = nes.behavior_fingerprint();
            if actual != cp.fingerprint {
                return Err(ReplayMismatch {
                    frame: cp.frame,
                    last_good_frame: self.last_good,
                    expected: cp.fingerprint,
                    actual,
                });
            }
            self.last_good = Some(cp.frame);
            self.verified += 1;
            self.next_checkpoint += 1;
        }
        Ok(())
    }

    /// 播放一幀：用 replay 的輸入呼叫 `nes.run_frame`，再驗證這一幀的檢查點（若有）。
    /// 回傳 `Ok(true)` 表示前進了一幀，`Ok(false)` 表示已經播完（什麼都沒做）；檢查點不符時
    /// 回傳 [`ReplayMismatch`]（`nes` 已經跑了那一幀）。
    pub fn step(&mut self, nes: &mut Nes) -> Result<bool, ReplayMismatch> {
        let Some(input) = self.next_input() else {
            return Ok(false);
        };
        nes.run_frame(input);
        self.frame += 1;
        self.check_checkpoints(nes)?;
        Ok(true)
    }
}

/// [`verify`] 的結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyReport {
    pub frames: u32,
    pub checkpoints_verified: u32,
}

/// 驗證模式：從 `rom` 開機、**關閉輸出**（不畫畫面、不混音，以加速；輸出不影響行為指紋）、
/// 依 replay 的輸入跑完全部幀，驗證每個檢查點。第一個不符的檢查點以
/// [`ReplayError::Mismatch`] 回報；版本或 ROM 不符則在開始前就拒絕。
pub fn verify(rom: &[u8], replay: &Replay) -> Result<VerifyReport, ReplayError> {
    let mut nes = Nes::from_rom(rom)?;
    nes.set_output_enabled(false);
    let mut player = ReplayPlayer::new(replay.clone(), &nes)?;
    while player.step(&mut nes).map_err(ReplayError::Mismatch)? {}
    Ok(VerifyReport {
        frames: player.frame(),
        checkpoints_verified: player.verified_checkpoints(),
    })
}
