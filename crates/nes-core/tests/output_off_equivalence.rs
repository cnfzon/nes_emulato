//! 輸出關閉（rollback 重跑幀用）與輸出開啟，行為指紋必須逐幀相同。
//!
//! Phase 4c.1 讓 PPU 在輸出關閉時只計算「會影響行為」的部分（overflow 與 sprite 0 hit：只畫 sprite 0，
//! 而且這條線上沒有 sprite 0 的不透明像素時連背景都不算）。這個測試用**會用到 sprite 0 hit／overflow 的
//! 真實 test ROM**，對每個 ROM 各跑兩個實例——一個輸出開、一個輸出關（另一份逐幀切換）——逐幀比對指紋。
//! 這些 ROM 放在被 gitignore 的 `roms/nes-test-roms/`（與 repo 根目錄的 Spacegulls，若存在），
//! 不存在時略過（同 `golden_frames`）。

use std::path::PathBuf;

use nes_core::{Buttons, FrameInput, Nes};

const ROMS: &[&str] = &[
    "sprite_hit_tests_2005.10.05/01.basics.nes",
    "sprite_hit_tests_2005.10.05/02.alignment.nes",
    "sprite_hit_tests_2005.10.05/03.corners.nes",
    "sprite_hit_tests_2005.10.05/04.flip.nes",
    "sprite_hit_tests_2005.10.05/05.left_clip.nes",
    "sprite_hit_tests_2005.10.05/06.right_edge.nes",
    "sprite_hit_tests_2005.10.05/07.screen_bottom.nes",
    "sprite_hit_tests_2005.10.05/08.double_height.nes",
    "sprite_hit_tests_2005.10.05/09.timing_basics.nes",
    "sprite_hit_tests_2005.10.05/10.timing_order.nes",
    "sprite_hit_tests_2005.10.05/11.edge_timing.nes",
    "blargg_ppu_tests_2005.09.15b/sprite_ram.nes",
    "ppu_vbl_nmi/rom_singles/01-vbl_basics.nes",
    "ppu_vbl_nmi/rom_singles/09-even_odd_frames.nes",
    "scrolltest/scroll.nes",
    "oam_read/oam_read.nes",
];

fn roms_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../roms/nes-test-roms")
}

fn script(i: u64) -> FrameInput {
    FrameInput::new(
        Buttons::from_bits_truncate((i / 4).wrapping_mul(37) as u8),
        Buttons::from_bits_truncate((i / 5).wrapping_mul(91) as u8),
    )
}

fn compare(name: &str, rom: &[u8], frames: u64) {
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
            "{name}：輸出關閉在第 {} 幀分歧",
            i + 1
        );
        assert_eq!(
            toggling.behavior_fingerprint(),
            fp,
            "{name}：逐幀切換在第 {} 幀分歧",
            i + 1
        );
    }
    assert_eq!(off.save_state(), on.save_state(), "{name}：存檔位元組");
}

#[test]
fn output_off_is_behaviorally_identical_on_real_roms() {
    let dir = roms_dir();
    let mut checked = 0;
    for rel in ROMS {
        let Ok(rom) = std::fs::read(dir.join(rel)) else {
            eprintln!("略過（沒有 {rel}）");
            continue;
        };
        compare(rel, &rom, 400);
        checked += 1;
    }
    // Spacegulls（repo 根目錄，被 gitignore）：真實遊戲、有 sprite 0 hit。
    if let Ok(rom) =
        std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Spacegulls-1.1.nes"))
    {
        compare("Spacegulls", &rom, 1500);
        checked += 1;
    }
    eprintln!("輸出開／關比對了 {checked} 個 ROM");
}
