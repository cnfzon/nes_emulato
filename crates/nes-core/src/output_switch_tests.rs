//! 輸出開關（rollback 重跑幀關閉輸出）不得改變行為——**不依賴任何外部 ROM，CI 上必定執行**。
//!
//! 對應 Phase 4c.1：輸出關閉時 PPU 只畫 sprite 0、而且沒有 sprite 0 的不透明像素時略過背景。
//! `tests/output_off_equivalence.rs` 用真實 test ROM 驗證同一件事，但那些 ROM 在被 gitignore 的 `roms/`，
//! CI 上沒有就會略過；這裡用合成的 SMB 式畫面分割 ROM（[`sprite0_split_rom`]）補上必定執行的版本。

use crate::ppu::render::BG_RENDERS;
use crate::test_support::{rendering_rom, sprite0_split_rom};
use crate::{Buttons, FrameInput, Nes};

fn script(i: u64) -> FrameInput {
    FrameInput::new(
        Buttons::from_bits_truncate((i / 3).wrapping_mul(37) as u8),
        Buttons::from_bits_truncate((i / 5).wrapping_mul(91) as u8),
    )
}

/// 三個實例：輸出開、輸出關、逐幀切換。逐幀指紋與最後的存檔位元組都必須相同。
fn assert_output_switch_is_invisible(rom: &[u8], frames: u64) -> Nes {
    let mut on = Nes::from_rom(rom).unwrap();
    let mut off = Nes::from_rom(rom).unwrap();
    off.set_output_enabled(false);
    let mut toggling = Nes::from_rom(rom).unwrap();
    for i in 0..frames {
        toggling.set_output_enabled(i % 3 == 0);
        on.run_frame(script(i));
        off.run_frame(script(i));
        toggling.run_frame(script(i));
        let fp = on.behavior_fingerprint();
        assert_eq!(
            off.behavior_fingerprint(),
            fp,
            "輸出關閉在第 {} 幀分歧",
            i + 1
        );
        assert_eq!(
            toggling.behavior_fingerprint(),
            fp,
            "逐幀切換在第 {} 幀分歧",
            i + 1
        );
    }
    assert_eq!(off.save_state(), on.save_state());
    on
}

#[test]
fn output_off_matches_output_on_on_a_sprite0_split_screen() {
    let nes = assert_output_switch_is_invisible(&sprite0_split_rom(), 240);
    // 測試不是空轉：sprite 0 hit 真的每幀發生、等待迴圈真的有計時、overflow 旗標真的被讀到。
    let ram = |a: u16| nes.peek(a);
    assert!(ram(0x0001) >= 200, "命中次數 {}", ram(0x0001));
    assert_ne!(ram(0x0002), 0, "等待命中的迴圈計數");
    assert_ne!(
        ram(0x0003) & 0x20,
        0,
        "NMI 讀到的 $2002 應含 sprite overflow"
    );
    assert_ne!(ram(0x0003) & 0x40, 0, "NMI 讀到的 $2002 應含 sprite 0 hit");
}

#[test]
fn output_off_matches_output_on_on_the_rendering_rom() {
    assert_output_switch_is_invisible(&rendering_rom(), 200);
}

/// 輸出關閉、mapper 不觀察 CHR 讀取：只在「這條線有 sprite 0 的不透明像素」時才算背景（次數遠少於 240 × 幀數）；
/// mapper 宣告會觀察 CHR 讀取（測試用強制開關）時走完整路徑，每條有背景的可見掃描線都算，
/// 而且行為與前者完全相同。
#[test]
fn a_mapper_that_observes_chr_reads_takes_the_full_render_path() {
    use crate::cartridge::mapper::OBSERVE_OVERRIDE;
    let rom = sprite0_split_rom();
    let count = |force: bool| {
        OBSERVE_OVERRIDE.with(|c| c.set(force));
        BG_RENDERS.with(|c| c.set(0));
        let mut nes = Nes::from_rom(&rom).unwrap();
        nes.set_output_enabled(false);
        for i in 0..60 {
            nes.run_frame(script(i));
        }
        OBSERVE_OVERRIDE.with(|c| c.set(false));
        (
            BG_RENDERS.with(std::cell::Cell::get),
            nes.behavior_fingerprint(),
        )
    };
    let (fast, fp_fast) = count(false);
    let (full, fp_full) = count(true);
    assert!(
        fast <= 60 * 12,
        "略過背景：只有 sprite 0 所在的幾條線要算，實際 {fast} 次"
    );
    assert!(
        full >= 60 * 200,
        "完整路徑：每條可見掃描線都算，實際 {full} 次"
    );
    assert_eq!(fp_fast, fp_full, "兩條路徑的行為必須完全相同");
}

/// mapper 0–3 都宣告「不觀察 CHR 讀取」（優化生效的前提）。新增 mapper 時要在這裡加上並正確宣告。
#[test]
fn all_current_mappers_declare_that_they_do_not_observe_chr_reads() {
    use crate::test_support::{cnrom_test_rom, mmc1_churn_rom, uxrom_test_rom};
    for (name, rom) in [
        ("NROM", rendering_rom()),
        ("MMC1", mmc1_churn_rom()),
        ("UxROM", uxrom_test_rom()),
        ("CNROM", cnrom_test_rom()),
    ] {
        let cart = crate::Cartridge::from_ines(&rom).unwrap();
        assert!(!cart.mapper.observes_chr_reads(), "{name}");
    }
}
