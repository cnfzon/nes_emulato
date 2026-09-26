//! 一場對戰的紀錄：雙方合併後的每幀輸入，加上每幀結束時的行為指紋。
//!
//! 可以在任意長度切出一份標準的 replay（`nes-core` 的 `Replay`，格式與 Phase 4a 相同）：
//! `replay verify` 通過，就等於「離線重播這份合併輸入得到同樣的結果」。兩端各自記錄，
//! 若 lockstep 正確，兩份 replay 的位元組完全相同（前提是取相同的長度，見
//! [`crate::session::Event::Disconnected`] 的 `peer_frames`）。
//!
//! 為什麼每幀都存指紋（8 位元組／幀）而不是只在檢查點存：對戰結束時要能切成「雙方共同完成的
//! 幀數」，檢查點必須落在那一幀上，而那一幀要到結束才知道。一小時約 216000 幀 × 8 位元組 ≈ 1.7 MB。

use nes_core::replay::{Checkpoint, Replay};
use nes_core::{CORE_BEHAVIOR_VERSION, FrameInput, Nes, RomId};

#[derive(Debug, Clone)]
pub struct MatchLog {
    rom_id: RomId,
    inputs: Vec<FrameInput>,
    /// `fingerprints[n]`＝已完成 `n` 幀之後的行為指紋（`[0]` 是開機狀態）。
    fingerprints: Vec<u64>,
}

impl MatchLog {
    /// `nes` 必須是剛開機的狀態（第 0 幀）。
    pub fn new(nes: &Nes) -> Self {
        Self {
            rom_id: nes.rom_id(),
            inputs: Vec::new(),
            fingerprints: vec![nes.behavior_fingerprint()],
        }
    }

    /// 記錄「剛剛跑完的那一幀」：`input` 是傳給 `run_frame` 的輸入，`nes` 是跑完之後的狀態。
    /// 幀數對不上（有幀沒有記錄）時不記錄並回傳 `false`。
    pub fn record(&mut self, input: FrameInput, nes: &Nes) -> bool {
        if nes.frame_count() != self.inputs.len() as u64 + 1 {
            return false;
        }
        self.inputs.push(input);
        self.fingerprints.push(nes.behavior_fingerprint());
        true
    }

    pub fn frames(&self) -> u32 {
        self.inputs.len() as u32
    }

    pub fn inputs(&self) -> &[FrameInput] {
        &self.inputs
    }

    /// `[n]`＝已完成 `n` 幀之後的指紋，長度是 `frames() + 1`。
    pub fn fingerprints(&self) -> &[u64] {
        &self.fingerprints
    }

    /// 切出前 `frames` 幀的 replay（超過已記錄的長度就取全部），檢查點的規則與 `ReplayRecorder`
    /// 相同：第 0 幀、每 `interval` 幀、最後一幀。
    pub fn to_replay(&self, frames: u32, interval: u16) -> Replay {
        let interval = interval.max(1);
        let frames = frames.min(self.frames());
        let mut checkpoints = vec![Checkpoint {
            frame: 0,
            fingerprint: self.fingerprints[0],
        }];
        for n in (u32::from(interval)..=frames).step_by(usize::from(interval)) {
            checkpoints.push(Checkpoint {
                frame: n,
                fingerprint: self.fingerprints[n as usize],
            });
        }
        if checkpoints.last().is_some_and(|cp| cp.frame != frames) {
            checkpoints.push(Checkpoint {
                frame: frames,
                fingerprint: self.fingerprints[frames as usize],
            });
        }
        Replay {
            core_behavior_version: CORE_BEHAVIOR_VERSION,
            rom_id: self.rom_id,
            total_frames: frames,
            checkpoint_interval: interval,
            runs: Replay::compress(self.inputs[..frames as usize].iter().copied()),
            checkpoints,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nes_core::Buttons;
    use nes_core::replay::{ReplayRecorder, verify};
    use nes_core::test_support::input_probe_rom;

    fn script(n: u32) -> FrameInput {
        FrameInput::new(
            Buttons::from_bits_truncate((n * 7) as u8),
            Buttons::from_bits_truncate((n * 13 + 1) as u8),
        )
    }

    /// `MatchLog` 切出來的 replay，位元組與官方的 `ReplayRecorder` 完全相同（任意長度、任意間隔）。
    #[test]
    fn to_replay_is_byte_identical_to_the_replay_recorder() {
        let rom = input_probe_rom();
        for interval in [1u16, 7, 60] {
            for frames in [0u32, 1, 59, 60, 61, 130] {
                let mut nes = Nes::from_rom(&rom).unwrap();
                nes.set_output_enabled(false);
                let mut log = MatchLog::new(&nes);
                let mut rec = ReplayRecorder::new(&nes, interval).unwrap();
                for n in 0..frames {
                    nes.run_frame(script(n));
                    assert!(log.record(script(n), &nes));
                    rec.record_frame(script(n), &nes).unwrap();
                }
                let expected = rec.finish(&nes).unwrap();
                assert_eq!(
                    log.to_replay(frames, interval).encode(),
                    expected.encode(),
                    "interval {interval}, frames {frames}"
                );
            }
        }
    }

    #[test]
    fn a_truncated_replay_verifies_and_matches_the_prefix() {
        let rom = input_probe_rom();
        let mut nes = Nes::from_rom(&rom).unwrap();
        nes.set_output_enabled(false);
        let mut log = MatchLog::new(&nes);
        for n in 0..200 {
            nes.run_frame(script(n));
            assert!(log.record(script(n), &nes));
        }
        let short = log.to_replay(137, 60);
        assert_eq!(short.total_frames, 137);
        assert!(verify(&rom, &short).is_ok());
        assert!(
            verify(&rom, &log.to_replay(u32::MAX, 60)).is_ok(),
            "超過長度取全部"
        );
    }

    #[test]
    fn record_refuses_a_frame_that_does_not_line_up() {
        let rom = input_probe_rom();
        let mut nes = Nes::from_rom(&rom).unwrap();
        let mut log = MatchLog::new(&nes);
        nes.run_frame(FrameInput::NONE);
        nes.run_frame(FrameInput::NONE); // 有一幀沒記錄
        assert!(!log.record(FrameInput::NONE, &nes));
        assert_eq!(log.frames(), 0);
    }
}
