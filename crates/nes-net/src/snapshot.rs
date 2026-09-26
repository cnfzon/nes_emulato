//! 快照環形緩衝與請求執行器：把 [`RollbackPlanner`](crate::rollback::RollbackPlanner) 的請求清單套用在一台 `Nes` 上。
//!
//! `nes-app` 的 emu 執行緒、`nes-test netsim` 與測試共用這一份實作，所以「快照怎麼存、還原怎麼做」只有一個版本。
//!
//! # 快照策略（量測與理由見 `docs/architecture.md` §20.1）
//!
//! - 快照是**直接複製模擬狀態**，不經過序列化（[`Nes::copy_state_from`]）。
//! - 環形緩衝有 `K + 1` 個槽（預測視窗 K：需要 `S_C ..= S_cur` 共 `K + 1` 個狀態），第 `n` 幀的狀態放在槽 `n % (K + 1)`。
//!   每個槽是開機時 `clone` 出來的一整台 `Nes`，之後只用 `copy_state_from` 覆寫，**沒有任何重新配置**。
//! - **framebuffer 與音訊輸出管線不進快照**：它們是輸出，還原時不需要還原（`copy_state_from` 不動它們）；
//!   每次存快照因此不必複製約 240 KB。代價是每個槽帶著一份用不到的 framebuffer 與 ROM 資料（`K + 1` 份，約 3 MB 量級）。

use nes_core::{FrameInput, Nes};

use crate::rollback::{MAX_WINDOW, Request};

#[derive(Debug)]
struct Slot {
    /// 這個槽目前存的是 `S_frame`；`None` ＝ 還沒用過。
    frame: Option<u32>,
    nes: Nes,
}

/// 保存最近 `K + 1` 個狀態的環形緩衝。
#[derive(Debug)]
pub struct SnapshotRing {
    slots: Vec<Slot>,
}

impl SnapshotRing {
    /// `window` ＝ 預測視窗 K；`template` 只用來取得同一份 ROM 的實例（clone 出各個槽）。
    pub fn new(window: u32, template: &Nes) -> Self {
        let count = window.clamp(1, MAX_WINDOW) as usize + 1;
        Self {
            slots: (0..count)
                .map(|_| Slot {
                    frame: None,
                    nes: template.clone(),
                })
                .collect(),
        }
    }

    /// 槽的數量（`K + 1`）。
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    fn slot_index(&self, frame: u32) -> usize {
        frame as usize % self.slots.len()
    }

    /// 把 `nes` 目前的狀態存成 `S_frame`（覆寫同一槽裡更舊的狀態）。
    pub fn save(&mut self, frame: u32, nes: &Nes) {
        let i = self.slot_index(frame);
        let slot = &mut self.slots[i];
        if slot.nes.copy_state_from(nes) {
            slot.frame = Some(frame);
        } else {
            // 不同的 ROM：不可能發生（槽是同一份 ROM clone 的）。標記為無效，讓還原時明確失敗。
            slot.frame = None;
        }
    }

    /// 把 `S_frame` 還原進 `nes`。槽裡不是這一幀（被覆寫了或從沒存過）→ `false`，`nes` 不變。
    pub fn load(&self, frame: u32, nes: &mut Nes) -> bool {
        let slot = &self.slots[self.slot_index(frame)];
        slot.frame == Some(frame) && nes.copy_state_from(&slot.nes)
    }

    /// 槽裡存的是不是 `S_frame`（測試用）。
    pub fn has(&self, frame: u32) -> bool {
        self.slots[self.slot_index(frame)].frame == Some(frame)
    }
}

/// 執行一份請求清單的結果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecReport {
    /// 執行了幾次 `AdvanceFrame`。
    pub advanced: u32,
    /// 其中輸出關閉的（重跑的）幀數。
    pub resimulated: u32,
    /// `LoadState` 的次數（0 或 1）。
    pub loads: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExecError {
    #[error("找不到第 {0} 幀的快照（環形緩衝裡沒有這一幀：規劃器要求還原到已經被丟棄的幀）")]
    MissingSnapshot(u32),
}

/// 依序在 `nes` 上執行 `requests`。
///
/// - `SaveState { frame }`：存進 `ring`，並呼叫 `on_saved(frame, nes.behavior_fingerprint())`
///   （呼叫端把指紋交給規劃器：[`RollbackPlanner::state_saved`](crate::rollback::RollbackPlanner::state_saved)）；
/// - `LoadState { frame }`：從 `ring` 還原（不含輸出）；
/// - `AdvanceFrame`：依 `output_enabled` 開關輸出，`run_frame`。
///
/// 全部執行完時輸出開關留在最後一個 `AdvanceFrame` 的設定；沒有任何 `AdvanceFrame` 時不動。
/// 錯誤（快照不存在）時中止並回傳 `Err`，已執行的請求不會被復原。
pub fn execute(
    requests: &[Request],
    nes: &mut Nes,
    ring: &mut SnapshotRing,
    mut on_saved: impl FnMut(u32, u64),
) -> Result<ExecReport, ExecError> {
    let mut report = ExecReport::default();
    for request in requests {
        match *request {
            Request::SaveState { frame } => {
                ring.save(frame, nes);
                on_saved(frame, nes.behavior_fingerprint());
            }
            Request::LoadState { frame } => {
                if !ring.load(frame, nes) {
                    return Err(ExecError::MissingSnapshot(frame));
                }
                report.loads += 1;
            }
            Request::AdvanceFrame {
                input,
                output_enabled,
                ..
            } => {
                nes.set_output_enabled(output_enabled);
                run(nes, input);
                report.advanced += 1;
                if !output_enabled {
                    report.resimulated += 1;
                }
            }
        }
    }
    Ok(report)
}

fn run(nes: &mut Nes, input: FrameInput) {
    nes.run_frame(input);
}

#[cfg(test)]
mod tests {
    use super::*;
    use nes_core::Buttons;
    use nes_core::test_support::input_probe_rom;

    fn input(i: u32) -> FrameInput {
        FrameInput::new(
            Buttons::from_bits_truncate((i * 5) as u8),
            Buttons::from_bits_truncate((i * 9 + 1) as u8),
        )
    }

    #[test]
    fn ring_keeps_exactly_k_plus_one_states() {
        let rom = input_probe_rom();
        let mut nes = Nes::from_rom(&rom).unwrap();
        let mut ring = SnapshotRing::new(4, &nes);
        assert_eq!(ring.capacity(), 5);
        for f in 0..12u32 {
            ring.save(f, &nes);
            nes.run_frame(input(f));
        }
        // 最近 5 個（7..=11）在，更舊的被覆寫。
        for f in 7..12 {
            assert!(ring.has(f), "第 {f} 幀應該還在");
        }
        for f in 0..7 {
            assert!(!ring.has(f), "第 {f} 幀應該已被覆寫");
        }
        let mut other = Nes::from_rom(&rom).unwrap();
        assert!(!ring.load(3, &mut other), "已被覆寫的幀不能還原");
    }

    #[test]
    fn execute_saves_loads_and_advances_and_reports_fingerprints() {
        let rom = input_probe_rom();
        let mut nes = Nes::from_rom(&rom).unwrap();
        let mut ring = SnapshotRing::new(8, &nes);
        let mut fps = Vec::new();
        let plan = [
            Request::SaveState { frame: 0 },
            Request::AdvanceFrame {
                frame: 0,
                input: input(0),
                output_enabled: true,
            },
            Request::SaveState { frame: 1 },
            Request::AdvanceFrame {
                frame: 1,
                input: input(1),
                output_enabled: true,
            },
            Request::SaveState { frame: 2 },
            // 還原到 S_1，用不同的輸入重跑（輸出關閉）。
            Request::LoadState { frame: 1 },
            Request::AdvanceFrame {
                frame: 1,
                input: input(7),
                output_enabled: false,
            },
            Request::SaveState { frame: 2 },
        ];
        let report = execute(&plan, &mut nes, &mut ring, |f, fp| fps.push((f, fp))).unwrap();
        assert_eq!(
            report,
            ExecReport {
                advanced: 3,
                resimulated: 1,
                loads: 1
            }
        );
        assert_eq!(fps.len(), 4);
        assert_eq!(fps[0].0, 0);

        // 與直接跑 [input(0), input(7)] 的結果相同。
        let mut direct = Nes::from_rom(&rom).unwrap();
        direct.run_frame(input(0));
        direct.run_frame(input(7));
        assert_eq!(nes.behavior_fingerprint(), direct.behavior_fingerprint());
        assert_eq!(fps[3], (2, direct.behavior_fingerprint()));
        assert_ne!(fps[2].1, fps[3].1, "同一幀重跑之後指紋不同（輸入不同）");
    }

    #[test]
    fn loading_a_missing_snapshot_is_an_error_not_a_panic() {
        let rom = input_probe_rom();
        let mut nes = Nes::from_rom(&rom).unwrap();
        let mut ring = SnapshotRing::new(2, &nes);
        let before = nes.behavior_fingerprint();
        let err = execute(
            &[Request::LoadState { frame: 5 }],
            &mut nes,
            &mut ring,
            |_, _| {},
        )
        .unwrap_err();
        assert_eq!(err, ExecError::MissingSnapshot(5));
        assert_eq!(nes.behavior_fingerprint(), before, "失敗時 `Nes` 不變");
    }
}
