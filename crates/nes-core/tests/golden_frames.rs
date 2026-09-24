//! 黃金畫面（golden frame）回歸測試：對「結果具決定性」的公開 test ROM，在
//! 固定輸入（不按任何鍵）下跑固定幀數，比對 framebuffer 的雜湊。
//!
//! **只存雜湊，不存圖片，也不存 ROM**：ROM 放在被 gitignore 的
//! `roms/nes-test-roms/`（來源見 `ATTRIBUTION.md`），不存在時該筆略過並印訊息，
//! 不算失敗（CI 上沒有這些檔案）。
//!
//! 這些雜湊是「目前模擬器的輸出」，不代表「跟真實硬體一致」：例如
//! `03-vbl_clear_time` 的畫面雜湊代表它目前印出的結果。PPU 有任何讓畫面改變的
//! 修改，這個測試都會失敗，提醒作者確認變更是預期的，再用
//! `nes-test golden <rom> --frames N` 取得新雜湊、更新下表。
//!
//! 產生方式：`cargo run --release -p nes-test -- golden <rom> --frames <N>`。
//! 三個 2005 年的 blargg PPU 測試（palette_ram / sprite_ram / vram_access）通過時
//! 畫面完全相同（只顯示 `$01`），所以雜湊相同是正常的。

use std::path::PathBuf;

use nes_core::{Buttons, Nes};

/// `(相對於 roms/nes-test-roms 的路徑, 幀數, framebuffer 的 xxh3-64)`
const GOLDEN: &[(&str, u64, u64)] = &[
    (
        "instr_test-v5/rom_singles/01-basics.nes",
        200,
        0xf1453374309d4316,
    ),
    (
        "instr_test-v5/rom_singles/02-implied.nes",
        300,
        0x00281071b6559df6,
    ),
    (
        "instr_test-v5/rom_singles/10-branches.nes",
        200,
        0xd0f40823e2ba6431,
    ),
    (
        "instr_test-v5/rom_singles/16-special.nes",
        200,
        0x183810354c90b830,
    ),
    (
        "ppu_vbl_nmi/rom_singles/01-vbl_basics.nes",
        300,
        0x4d24dd71ab935d87,
    ),
    (
        "ppu_vbl_nmi/rom_singles/03-vbl_clear_time.nes",
        300,
        0x1f6b10ff1995ca3f,
    ),
    (
        "ppu_vbl_nmi/rom_singles/04-nmi_control.nes",
        200,
        0xa0475c3bbe47df90,
    ),
    (
        "ppu_vbl_nmi/rom_singles/09-even_odd_frames.nes",
        200,
        0xabb0891ba54edcc2,
    ),
    ("oam_read/oam_read.nes", 200, 0xa03587bd73e02233),
    (
        "blargg_ppu_tests_2005.09.15b/palette_ram.nes",
        120,
        0x0de9bec08f758942,
    ),
    (
        "blargg_ppu_tests_2005.09.15b/sprite_ram.nes",
        120,
        0x0de9bec08f758942,
    ),
    (
        "blargg_ppu_tests_2005.09.15b/vram_access.nes",
        120,
        0x0de9bec08f758942,
    ),
    (
        "sprite_hit_tests_2005.10.05/01.basics.nes",
        200,
        0x92a8683a3eac9331,
    ),
    (
        "sprite_hit_tests_2005.10.05/02.alignment.nes",
        200,
        0x41726a8516a5bc7f,
    ),
    (
        "sprite_hit_tests_2005.10.05/03.corners.nes",
        200,
        0x2a0fddb34af5909f,
    ),
    (
        "sprite_hit_tests_2005.10.05/04.flip.nes",
        200,
        0x84a151b35d5aea14,
    ),
    (
        "sprite_hit_tests_2005.10.05/05.left_clip.nes",
        200,
        0xda6dc051e38a550b,
    ),
    (
        "sprite_hit_tests_2005.10.05/06.right_edge.nes",
        200,
        0x6fe8d4df7e7adddb,
    ),
    (
        "sprite_hit_tests_2005.10.05/07.screen_bottom.nes",
        200,
        0x3bf946ab0f2c0e11,
    ),
    (
        "sprite_hit_tests_2005.10.05/08.double_height.nes",
        200,
        0x2c91bfe4f72e8696,
    ),
    (
        "sprite_hit_tests_2005.10.05/09.timing_basics.nes",
        200,
        0xd487a1b834cf983a,
    ),
    (
        "sprite_hit_tests_2005.10.05/10.timing_order.nes",
        200,
        0xc6b9b60e5cddfbb4,
    ),
    (
        "sprite_hit_tests_2005.10.05/11.edge_timing.nes",
        200,
        0x189b937c05d5c7c1,
    ),
];

fn rom_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../roms/nes-test-roms")
}

/// 跑 `frames` 幀（不按任何鍵），回傳最後一幀畫面的雜湊。
fn frame_hash_after(rom: &[u8], frames: u64) -> u64 {
    let mut nes = Nes::from_rom(rom).expect("test ROM 應該能載入");
    let mut hash = 0;
    for _ in 0..frames {
        hash = nes.run_frame([Buttons::empty(); 2]).hash64();
    }
    hash
}

#[test]
fn test_rom_final_screens_match_golden_hashes() {
    let root = rom_root();
    if !root.is_dir() {
        eprintln!("找不到 {}，略過黃金畫面測試（不算失敗）", root.display());
        return;
    }

    let mut checked = 0;
    let mut mismatches = Vec::new();
    for &(rel, frames, expected) in GOLDEN {
        let path = root.join(rel);
        let Ok(rom) = std::fs::read(&path) else {
            eprintln!("略過 {rel}（檔案不存在）");
            continue;
        };
        let actual = frame_hash_after(&rom, frames);
        checked += 1;
        if actual != expected {
            mismatches.push(format!("{rel}: 期望 {expected:#018x}，實際 {actual:#018x}"));
        }
    }

    eprintln!("黃金畫面：檢查了 {checked} / {} 個 ROM", GOLDEN.len());
    assert!(
        mismatches.is_empty(),
        "畫面雜湊不符（若是預期的 PPU 變更，請更新 GOLDEN 表）：
{}",
        mismatches.join(
            "
"
        )
    );
}
