//! PPU 單元測試：暫存器副作用、位址空間、時序、NMI、渲染。
//!
//! 這裡直接驅動 `Ppu`（不經過 CPU），用一張測試卡帶提供 CHR-ROM。

use super::*;
use crate::test_support::{Asm, build_nrom, test_chr};

fn cart(vertical: bool) -> Cartridge {
    let mut code = Asm::new(0x8000);
    code.jmp(0x8000);
    let rom = build_nrom(&code, 0x8000, None, &[], &test_chr(), vertical);
    Cartridge::from_ines(&rom).unwrap()
}

fn write(ppu: &mut Ppu, cart: &mut Cartridge, reg: u16, value: u8) {
    ppu.write_register(reg, value, cart);
}

/// `$2006` 兩次寫入設定 v。
fn set_addr(ppu: &mut Ppu, cart: &mut Cartridge, addr: u16) {
    write(ppu, cart, 6, (addr >> 8) as u8);
    write(ppu, cart, 6, addr as u8);
}

/// 一幀 = 262 × 341 個 dot（渲染關閉時；奇數幀的少一個 dot 只在渲染開啟時發生）。
const DOTS_PER_FRAME: u32 = 262 * 341;

#[test]
fn reading_status_clears_vblank_and_write_latch() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    ppu.status = STATUS_VBLANK | STATUS_SPRITE0_HIT;
    write(&mut ppu, &mut c, 5, 0x10); // 第一次寫入 → w = true
    assert!(ppu.w);

    let value = ppu.read_register(2, &c);
    assert_eq!(value & 0xE0, STATUS_VBLANK | STATUS_SPRITE0_HIT);
    assert_eq!(ppu.status & STATUS_VBLANK, 0, "讀 $2002 清 vblank");
    assert_ne!(ppu.status & STATUS_SPRITE0_HIT, 0, "sprite 0 hit 不受影響");
    assert!(!ppu.w, "讀 $2002 重置 w");
}

#[test]
fn write_only_registers_read_back_the_io_latch() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    write(&mut ppu, &mut c, 0, 0x5A);
    assert_eq!(ppu.read_register(0, &c), 0x5A);
    assert_eq!(ppu.read_register(5, &c), 0x5A);
}

#[test]
fn scroll_writes_use_two_write_latch() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);

    write(&mut ppu, &mut c, 5, 0b1011_1101); // X = 189：coarse X = 23，fine X = 5
    assert_eq!(ppu.t & 0x1F, 23);
    assert_eq!(ppu.fine_x, 5);
    assert!(ppu.w);

    write(&mut ppu, &mut c, 5, 0b0111_1110); // Y = 126：coarse Y = 15，fine Y = 6
    assert_eq!((ppu.t >> 5) & 0x1F, 15);
    assert_eq!((ppu.t >> 12) & 0x07, 6);
    assert!(!ppu.w);
}

#[test]
fn addr_writes_use_two_write_latch_and_copy_t_to_v() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);

    write(&mut ppu, &mut c, 6, 0x3F); // 高位元組（只取 6 位元）
    assert_eq!(ppu.v, 0, "第一次寫入不改 v");
    write(&mut ppu, &mut c, 6, 0x10);
    assert_eq!(ppu.v, 0x3F10);
    assert!(!ppu.w);

    // 高位元組的 bit 6–7 被忽略（位址只有 14 bit）。
    write(&mut ppu, &mut c, 6, 0xFF);
    write(&mut ppu, &mut c, 6, 0x00);
    assert_eq!(ppu.v, 0x3F00);
}

#[test]
fn ppuctrl_write_sets_nametable_bits_of_t() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    write(&mut ppu, &mut c, 0, 0b0000_0010);
    assert_eq!(ppu.t & 0x0C00, 0x0800);
    write(&mut ppu, &mut c, 0, 0b0000_0001);
    assert_eq!(ppu.t & 0x0C00, 0x0400);
}

#[test]
fn ppudata_write_increments_by_1_or_32() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);

    set_addr(&mut ppu, &mut c, 0x2000);
    write(&mut ppu, &mut c, 7, 0x11);
    assert_eq!(ppu.v, 0x2001);

    write(&mut ppu, &mut c, 0, 0x04); // bit2：每次 +32
    set_addr(&mut ppu, &mut c, 0x2000);
    write(&mut ppu, &mut c, 7, 0x22);
    assert_eq!(ppu.v, 0x2020);
}

#[test]
fn ppudata_read_is_buffered_but_palette_is_not() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);

    set_addr(&mut ppu, &mut c, 0x2000);
    write(&mut ppu, &mut c, 7, 0xAA);
    write(&mut ppu, &mut c, 7, 0xBB);

    set_addr(&mut ppu, &mut c, 0x2000);
    assert_eq!(ppu.read_register(7, &c), 0x00, "第一次讀到的是舊緩衝");
    assert_eq!(ppu.read_register(7, &c), 0xAA, "之後每次讀到前一個位置");
    assert_eq!(ppu.read_register(7, &c), 0xBB);

    // 調色盤不經緩衝：一次就讀到。
    set_addr(&mut ppu, &mut c, 0x3F01);
    write(&mut ppu, &mut c, 7, 0x2A);
    set_addr(&mut ppu, &mut c, 0x3F01);
    assert_eq!(ppu.read_register(7, &c) & 0x3F, 0x2A);
}

#[test]
fn palette_read_fills_buffer_from_shadow_nametable() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    set_addr(&mut ppu, &mut c, 0x2F01); // 與 $3F01 的「底下」nametable 位置
    write(&mut ppu, &mut c, 7, 0x77);

    set_addr(&mut ppu, &mut c, 0x3F01);
    let _ = ppu.read_register(7, &c);
    assert_eq!(ppu.data_buffer, 0x77);
}

#[test]
fn palette_mirrors_0x3f10_family_to_0x3f00_family() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);

    set_addr(&mut ppu, &mut c, 0x3F10);
    write(&mut ppu, &mut c, 7, 0x21);
    set_addr(&mut ppu, &mut c, 0x3F00);
    assert_eq!(ppu.read_register(7, &c) & 0x3F, 0x21, "寫 $3F10 = 寫 $3F00");

    set_addr(&mut ppu, &mut c, 0x3F04);
    write(&mut ppu, &mut c, 7, 0x12);
    set_addr(&mut ppu, &mut c, 0x3F14);
    assert_eq!(ppu.read_register(7, &c) & 0x3F, 0x12, "讀 $3F14 = 讀 $3F04");

    // $3F11 不是鏡像，各自獨立。
    set_addr(&mut ppu, &mut c, 0x3F11);
    write(&mut ppu, &mut c, 7, 0x05);
    set_addr(&mut ppu, &mut c, 0x3F01);
    assert_ne!(ppu.read_register(7, &c) & 0x3F, 0x05);
}

#[test]
fn palette_region_repeats_every_32_bytes() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    set_addr(&mut ppu, &mut c, 0x3F03);
    write(&mut ppu, &mut c, 7, 0x1C);
    set_addr(&mut ppu, &mut c, 0x3F23);
    assert_eq!(ppu.read_register(7, &c) & 0x3F, 0x1C);
}

#[test]
fn horizontal_mirroring_pairs_2000_2400_and_2800_2c00() {
    let mut ppu = Ppu::default();
    let mut c = cart(false); // header：horizontal
    assert_eq!(c.info.mirroring, Mirroring::Horizontal);

    set_addr(&mut ppu, &mut c, 0x2005);
    write(&mut ppu, &mut c, 7, 0x42);
    assert_eq!(ppu.read_memory(0x2405, &c), 0x42);
    assert_ne!(ppu.read_memory(0x2805, &c), 0x42);
    assert_ne!(ppu.read_memory(0x2C05, &c), 0x42);
}

#[test]
fn vertical_mirroring_pairs_2000_2800_and_2400_2c00() {
    let mut ppu = Ppu::default();
    let mut c = cart(true);
    assert_eq!(c.info.mirroring, Mirroring::Vertical);

    set_addr(&mut ppu, &mut c, 0x2005);
    write(&mut ppu, &mut c, 7, 0x42);
    assert_eq!(ppu.read_memory(0x2805, &c), 0x42);
    assert_ne!(ppu.read_memory(0x2405, &c), 0x42);
}

#[test]
fn nametables_are_mirrored_at_3000() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    set_addr(&mut ppu, &mut c, 0x2123);
    write(&mut ppu, &mut c, 7, 0x99);
    assert_eq!(ppu.read_memory(0x3123, &c), 0x99);
}

#[test]
fn chr_rom_ignores_writes() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    let before = c.read_chr(0x0010);
    set_addr(&mut ppu, &mut c, 0x0010);
    write(&mut ppu, &mut c, 7, before ^ 0xFF);
    assert_eq!(c.read_chr(0x0010), before);
}

#[test]
fn oam_data_write_increments_addr_but_read_does_not() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    write(&mut ppu, &mut c, 3, 0x10);
    write(&mut ppu, &mut c, 4, 0xAB);
    write(&mut ppu, &mut c, 4, 0xCD);
    assert_eq!(ppu.oam_addr, 0x12);
    assert_eq!(ppu.oam[0x10], 0xAB);
    assert_eq!(ppu.oam[0x11], 0xCD);

    write(&mut ppu, &mut c, 3, 0x10);
    assert_eq!(ppu.read_register(4, &c), 0xAB);
    assert_eq!(ppu.oam_addr, 0x10, "讀 $2004 不遞增");
}

#[test]
fn oam_attribute_byte_reads_back_masked() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    write(&mut ppu, &mut c, 3, 0x02);
    write(&mut ppu, &mut c, 4, 0xFF);
    write(&mut ppu, &mut c, 3, 0x02);
    assert_eq!(ppu.read_register(4, &c), 0xE3);
}

#[test]
fn peek_register_has_no_side_effects() {
    let mut ppu = Ppu::default();
    let c = cart(false);
    ppu.status = STATUS_VBLANK;
    ppu.w = true;
    let _ = ppu.peek_register(2);
    let _ = ppu.peek_register(7);
    assert_eq!(ppu.status & STATUS_VBLANK, STATUS_VBLANK);
    assert!(ppu.w);
    assert_eq!(ppu.v, 0);
    let _ = &c;
}

// ---- 時序 -------------------------------------------------------------

#[test]
fn vblank_is_set_at_scanline_241_dot_1_and_cleared_at_prerender_dot_1() {
    let mut ppu = Ppu::default();
    let c = cart(false);

    // 走到 (241, 0)：還沒設 vblank。
    ppu.step_dots(241 * 341, &c);
    assert_eq!((ppu.scanline, ppu.cycle), (241, 0));
    assert_eq!(ppu.status & STATUS_VBLANK, 0);

    ppu.step_dots(1, &c); // 處理 dot 0
    ppu.step_dots(1, &c); // 處理 dot 1 → 設旗標
    assert_ne!(ppu.status & STATUS_VBLANK, 0);
    assert!(ppu.take_frame_done());

    // 走到 (261, 1)（尚未處理該 dot）：旗標還在；處理之後被清掉。
    ppu.step_dots(20 * 341 - 1, &c);
    assert_eq!((ppu.scanline, ppu.cycle), (261, 1));
    assert_ne!(ppu.status & STATUS_VBLANK, 0);
    ppu.step_dots(1, &c);
    assert_eq!(ppu.status & (STATUS_VBLANK | STATUS_SPRITE0_HIT), 0);
}

#[test]
fn frame_wraps_after_262_scanlines_of_341_dots_when_rendering_is_off() {
    let mut ppu = Ppu::default();
    let c = cart(false);
    ppu.step_dots(DOTS_PER_FRAME, &c);
    assert_eq!((ppu.scanline, ppu.cycle), (0, 0));
    ppu.step_dots(DOTS_PER_FRAME, &c);
    assert_eq!((ppu.scanline, ppu.cycle), (0, 0), "渲染關閉：奇偶幀一樣長");
}

#[test]
fn odd_frames_are_one_dot_shorter_when_rendering_is_on() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    write(&mut ppu, &mut c, 1, 0x08); // 顯示背景

    // 偶數幀（frame 0）：完整長度。
    ppu.step_dots(DOTS_PER_FRAME, &c);
    assert_eq!((ppu.scanline, ppu.cycle), (0, 0));
    // 奇數幀：少一個 dot，所以走完整整 DOTS_PER_FRAME 個 dot 會進入下一幀的 dot 1。
    ppu.step_dots(DOTS_PER_FRAME, &c);
    assert_eq!((ppu.scanline, ppu.cycle), (0, 1));
}

#[test]
fn nmi_fires_at_vblank_when_enabled() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    write(&mut ppu, &mut c, 0, 0x80);
    ppu.step_dots(241 * 341 + 1, &c);
    assert!(!ppu.take_nmi(), "dot 1 尚未處理");
    ppu.step_dots(1, &c);
    assert!(ppu.take_nmi());
    assert!(!ppu.take_nmi(), "一個邊緣只觸發一次");
}

#[test]
fn nmi_does_not_fire_when_disabled() {
    let mut ppu = Ppu::default();
    let c = cart(false);
    ppu.step_dots(DOTS_PER_FRAME, &c);
    assert!(!ppu.take_nmi());
}

#[test]
fn enabling_nmi_during_vblank_triggers_after_the_next_instruction() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    ppu.step_dots(241 * 341 + 2, &c); // vblank 已開始、NMI 關閉
    assert!(!ppu.take_nmi());

    write(&mut ppu, &mut c, 0, 0x80); // vblank 中把 bit7 從 0 寫成 1
    assert!(!ppu.take_nmi(), "寫入那條指令的結尾偵測不到（來不及）");
    assert!(ppu.take_nmi(), "下一條指令之後才觸發");
}

#[test]
fn reading_status_before_enabling_nmi_prevents_the_late_nmi() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    ppu.step_dots(241 * 341 + 2, &c);
    let _ = ppu.read_register(2, &c); // 清掉 vblank 旗標
    write(&mut ppu, &mut c, 0, 0x80);
    assert!(!ppu.take_nmi());
    assert!(!ppu.take_nmi());
}

#[test]
fn toggling_nmi_enable_within_vblank_produces_a_new_edge_each_time() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    ppu.step_dots(241 * 341 + 2, &c);
    write(&mut ppu, &mut c, 0, 0x80);
    assert!(!ppu.take_nmi());
    assert!(ppu.take_nmi());
    write(&mut ppu, &mut c, 0, 0x00);
    write(&mut ppu, &mut c, 0, 0x80);
    assert!(!ppu.take_nmi());
    assert!(ppu.take_nmi());
}

#[test]
fn v_is_updated_with_real_timing_when_rendering() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);

    // t：coarse X = 5、fine Y = 3、coarse Y = 9、nametable 1。
    ppu.t = 5 | (9 << 5) | (1 << 10) | (3 << 12);
    ppu.v = 0;
    write(&mut ppu, &mut c, 1, 0x08);
    ppu.t = 5 | (9 << 5) | (1 << 10) | (3 << 12);

    // pre-render 行 dot 280–304 之前，vert(v) 還沒複製（dot 256 的 Y 遞增只動 fine Y）。
    ppu.scanline = 261;
    ppu.cycle = 0;
    ppu.step_dots(280, &c);
    assert_ne!(ppu.v & 0x7BE0, ppu.t & 0x7BE0);
    ppu.step_dots(1, &c); // 處理 dot 280
    assert_eq!(ppu.v & 0x7BE0, ppu.t & 0x7BE0);

    // 可見行：dot 257 把 hori(v) 換成 hori(t)。
    ppu.scanline = 10;
    ppu.cycle = 0;
    ppu.v = 0;
    ppu.step_dots(257, &c);
    assert_ne!(ppu.v & 0x041F, ppu.t & 0x041F, "dot 257 還沒處理");
    ppu.step_dots(1, &c);
    assert_eq!(ppu.v & 0x041F, ppu.t & 0x041F);
}

#[test]
fn coarse_x_increments_every_8_dots_and_y_increments_at_dot_256() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    write(&mut ppu, &mut c, 1, 0x08);
    ppu.scanline = 5;
    ppu.cycle = 0;
    ppu.v = 0;

    ppu.step_dots(9, &c); // 處理 dot 0..=8：dot 8 遞增一次
    assert_eq!(ppu.v & 0x1F, 1);
    ppu.step_dots(8, &c);
    assert_eq!(ppu.v & 0x1F, 2);

    ppu.step_dots(256 - 17 + 1, &c); // 處理到 dot 256
    assert_eq!(ppu.v & 0x1F, 0, "32 次遞增後繞回並翻轉 nametable");
    assert_eq!(ppu.v & 0x0400, 0x0400);
    assert_eq!((ppu.v >> 12) & 7, 1, "dot 256 fine Y + 1");
}

#[test]
fn increment_y_wraps_at_row_29_and_flips_nametable() {
    let mut ppu = Ppu {
        v: 7 << 12 | 29 << 5, // fine Y = 7、coarse Y = 29
        ..Ppu::default()
    };
    ppu.increment_y();
    assert_eq!(ppu.v & 0x7BE0, 0x0800, "coarse Y 歸零、垂直 nametable 翻轉");

    ppu.v = 7 << 12 | 31 << 5;
    ppu.increment_y();
    assert_eq!(ppu.v & 0x7BE0, 0, "coarse Y 31 歸零但不翻轉");
}

#[test]
fn sprite0_hit_flag_is_set_at_the_hit_dot_not_at_scanline_start() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);

    // 背景：nametable 0 全部用 tile 3（純色 3）；精靈 0 在 (x=100, Y=39) 用 tile 3。
    set_addr(&mut ppu, &mut c, 0x2000);
    for _ in 0..960 {
        write(&mut ppu, &mut c, 7, 3);
    }
    set_addr(&mut ppu, &mut c, 0x3F00);
    for color in [0x0F, 0x16, 0x2A, 0x30] {
        write(&mut ppu, &mut c, 7, color);
    }
    ppu.oam[0..4].copy_from_slice(&[39, 3, 0x00, 100]);
    write(&mut ppu, &mut c, 1, 0x1E);
    ppu.v = 0;
    ppu.t = 0;

    // 精靈第一列在掃描線 40；命中 x = 100 → dot 101。
    ppu.scanline = 40;
    ppu.cycle = 0;
    ppu.step_dots(101, &c); // 處理 dot 0..=100
    assert_eq!(ppu.status & STATUS_SPRITE0_HIT, 0, "尚未走到命中的 dot");
    ppu.step_dots(1, &c); // 處理 dot 101
    assert_ne!(ppu.status & STATUS_SPRITE0_HIT, 0);
}

#[test]
fn sprite0_hit_is_not_reported_at_x_255() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    set_addr(&mut ppu, &mut c, 0x2000);
    for _ in 0..960 {
        write(&mut ppu, &mut c, 7, 3);
    }
    // 精靈 X = 255：只有最左邊 1 個像素在螢幕內，且落在 x = 255。
    ppu.oam[0..4].copy_from_slice(&[39, 3, 0x00, 255]);
    write(&mut ppu, &mut c, 1, 0x1E);
    ppu.v = 0;
    ppu.scanline = 40;
    ppu.cycle = 0;
    ppu.step_dots(341, &c);
    assert_eq!(ppu.status & STATUS_SPRITE0_HIT, 0);
}

#[test]
fn sprite0_hit_respects_left_edge_mask() {
    for (mask, expect_hit) in [(0x1E, true), (0x18, false)] {
        let mut ppu = Ppu::default();
        let mut c = cart(false);
        set_addr(&mut ppu, &mut c, 0x2000);
        for _ in 0..960 {
            write(&mut ppu, &mut c, 7, 3);
        }
        ppu.oam[0..4].copy_from_slice(&[39, 3, 0x00, 0]); // 完全在左側 8 像素內
        write(&mut ppu, &mut c, 1, mask);
        ppu.v = 0;
        ppu.scanline = 40;
        ppu.cycle = 0;
        ppu.step_dots(341, &c);
        assert_eq!(
            ppu.status & STATUS_SPRITE0_HIT != 0,
            expect_hit,
            "mask = {mask:#04X}"
        );
    }
}

// ---- 渲染 -------------------------------------------------------------

fn pixel(ppu: &Ppu, x: usize, y: usize) -> [u8; 3] {
    let bytes = ppu.frame_buffer.as_bytes();
    let i = (y * crate::frame::WIDTH + x) * 4;
    [bytes[i], bytes[i + 1], bytes[i + 2]]
}

fn color(index: u8) -> [u8; 3] {
    SYSTEM_PALETTE[index as usize]
}

/// 渲染 `line` 那一條掃描線（其他不管）。
fn render_line(ppu: &mut Ppu, c: &Cartridge, line: u16) {
    ppu.scanline = line;
    ppu.cycle = 0;
    ppu.step_dots(1, c);
}

fn setup_palette(ppu: &mut Ppu, c: &mut Cartridge, entries: &[u8]) {
    set_addr(ppu, c, 0x3F00);
    for &e in entries {
        write(ppu, c, 7, e);
    }
}

#[test]
fn rendering_disabled_shows_backdrop_color() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    setup_palette(&mut ppu, &mut c, &[0x21]);
    render_line(&mut ppu, &c, 10);
    assert_eq!(pixel(&ppu, 0, 10), color(0x21));
    assert_eq!(pixel(&ppu, 255, 10), color(0x21));
}

#[test]
fn background_tile_uses_pattern_and_attribute_palette() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    setup_palette(
        &mut ppu,
        &mut c,
        &[0x0F, 0x16, 0x2A, 0x30, 0x0F, 0x11, 0x21, 0x31],
    );
    // nametable (col 2, row 1) = tile 2（純色 2）。屬性選第 1 組調色盤（bit 0-1 = 1）。
    set_addr(&mut ppu, &mut c, 0x2000 + 32 + 2);
    write(&mut ppu, &mut c, 7, 2);
    // 屬性表：(col 2, row 1) 落在 32×32 像素區塊的右上 16×16 → 屬性位元組 0 的 bit 2-3。
    set_addr(&mut ppu, &mut c, 0x23C0);
    write(&mut ppu, &mut c, 7, 0b0000_0100);
    write(&mut ppu, &mut c, 1, 0x0A); // 顯示背景 + 左 8 像素
    ppu.v = 1 << 5; // coarse Y = 1

    render_line(&mut ppu, &c, 8); // 第 1 列 tile 的第一條掃描線
    assert_eq!(pixel(&ppu, 16, 8), color(0x21), "調色盤 1 的色 2");
    assert_eq!(pixel(&ppu, 23, 8), color(0x21));
    assert_eq!(pixel(&ppu, 24, 8), color(0x0F), "隔壁 tile 是透明 → 背景色");
}

#[test]
fn horizontal_fine_scroll_shifts_pixels_left() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    setup_palette(&mut ppu, &mut c, &[0x0F, 0x16, 0x2A, 0x30]);
    set_addr(&mut ppu, &mut c, 0x2000);
    write(&mut ppu, &mut c, 7, 3); // tile 0 = 純色 3，其餘 tile 0（空白）
    write(&mut ppu, &mut c, 1, 0x0A);
    ppu.v = 0;
    ppu.fine_x = 3;
    render_line(&mut ppu, &c, 0);
    // 原本 x = 0..8 是色 3；fine X = 3 之後只剩 x = 0..5 是色 3。
    assert_eq!(pixel(&ppu, 4, 0), color(0x30));
    assert_eq!(pixel(&ppu, 5, 0), color(0x0F));
}

#[test]
fn coarse_x_scroll_crosses_into_the_next_nametable() {
    let mut ppu = Ppu::default();
    let mut c = cart(true); // vertical：$2000 和 $2400 是不同的兩張
    setup_palette(&mut ppu, &mut c, &[0x0F, 0x16, 0x2A, 0x30]);
    // $2400 的 col 0 放 tile 3（純色 3）。
    set_addr(&mut ppu, &mut c, 0x2400);
    write(&mut ppu, &mut c, 7, 3);
    write(&mut ppu, &mut c, 1, 0x0A);
    // coarse X = 31：第一個 tile 是 $2000 的 col 31，第二個 tile 換到 $2400 的 col 0。
    ppu.v = 31;
    ppu.fine_x = 0;
    render_line(&mut ppu, &c, 0);
    assert_eq!(pixel(&ppu, 0, 0), color(0x0F));
    assert_eq!(
        pixel(&ppu, 8, 0),
        color(0x30),
        "第 2 個 tile 來自相鄰的 nametable"
    );
}

#[test]
fn sprite_is_drawn_with_priority_flip_and_palette() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    setup_palette(
        &mut ppu,
        &mut c,
        &[
            0x0F, 0x16, 0x2A, 0x30, 0x0F, 0x11, 0x21, 0x31, 0x0F, 0x15, 0x25, 0x35, 0x0F, 0x19,
            0x29, 0x39, // 背景
            0x0F, 0x06, 0x1A, 0x30, 0x0F, 0x12, 0x22, 0x32, // 精靈調色盤 0、1
        ],
    );
    write(&mut ppu, &mut c, 1, 0x14); // 只顯示精靈
    ppu.v = 0;

    // 精靈 0：tile 3（純色 3），調色盤 1，位置 (x=20, Y=9) → 掃描線 10–17。
    ppu.oam[0..4].copy_from_slice(&[9, 3, 0x01, 20]);
    render_line(&mut ppu, &c, 10);
    assert_eq!(pixel(&ppu, 20, 10), color(0x32));
    assert_eq!(pixel(&ppu, 27, 10), color(0x32));
    assert_eq!(pixel(&ppu, 28, 10), color(0x0F));
    assert_eq!(pixel(&ppu, 19, 10), color(0x0F));
}

#[test]
fn sprite_flip_and_lower_index_priority() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    setup_palette(&mut ppu, &mut c, &[0x0F; 16]);
    set_addr(&mut ppu, &mut c, 0x3F11);
    for color in [0x11, 0x12, 0x13] {
        write(&mut ppu, &mut c, 7, color);
    }
    write(&mut ppu, &mut c, 1, 0x14);
    ppu.v = 0;
    ppu.oam.iter_mut().for_each(|b| *b = 0xFF); // 全部藏到畫面外

    // tile 5：上半 4 個像素寬的色 3、下半 8 個像素寬的色 3。
    // 精靈 0（不翻轉）與精靈 1（水平翻轉）疊在同一個位置：精靈 0 優先。
    ppu.oam[0..4].copy_from_slice(&[9, 5, 0x00, 40]);
    ppu.oam[4..8].copy_from_slice(&[9, 5, 0x40, 40]);
    render_line(&mut ppu, &c, 10);
    assert_eq!(pixel(&ppu, 40, 10), color(0x13), "精靈 0 的左半有像素");
    assert_eq!(
        pixel(&ppu, 44, 10),
        color(0x13),
        "精靈 0 右半透明 → 露出精靈 1（翻轉後的左半）"
    );
    assert_eq!(pixel(&ppu, 47, 10), color(0x13));
    assert_eq!(pixel(&ppu, 48, 10), color(0x0F));
}

#[test]
fn sprite_behind_background_only_shows_over_transparent_background() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    setup_palette(&mut ppu, &mut c, &[0x0F, 0x16, 0x2A, 0x30]);
    set_addr(&mut ppu, &mut c, 0x3F11);
    write(&mut ppu, &mut c, 7, 0x12);
    write(&mut ppu, &mut c, 7, 0x22);
    write(&mut ppu, &mut c, 7, 0x32);
    // 背景 tile 0 位置放純色 3（不透明）；tile 1 位置維持空白（透明）。
    set_addr(&mut ppu, &mut c, 0x2000);
    write(&mut ppu, &mut c, 7, 3);
    write(&mut ppu, &mut c, 1, 0x1E);
    ppu.v = 0;
    ppu.oam.iter_mut().for_each(|b| *b = 0xFF);
    // 精靈在背景之後（attr bit5），橫跨 x = 4..12：左半在不透明背景上、右半在透明背景上。
    ppu.oam[0..4].copy_from_slice(&[9, 3, 0x20, 4]);
    render_line(&mut ppu, &c, 10);
    assert_eq!(pixel(&ppu, 5, 10), color(0x30), "背景不透明 → 背景在前");
    assert_eq!(pixel(&ppu, 9, 10), color(0x32), "背景透明 → 露出精靈");
}

#[test]
fn eight_by_sixteen_sprites_use_two_tiles_and_pick_table_from_tile_bit0() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    setup_palette(&mut ppu, &mut c, &[0x0F; 16]);
    set_addr(&mut ppu, &mut c, 0x3F11);
    for color in [0x11, 0x12, 0x13] {
        write(&mut ppu, &mut c, 7, color);
    }
    write(&mut ppu, &mut c, 0, 0x20); // 8×16 精靈
    write(&mut ppu, &mut c, 1, 0x14);
    ppu.v = 0;
    ppu.oam.iter_mut().for_each(|b| *b = 0xFF);
    // tile 6：上半 tile = 6（只有上半有像素），下半 tile = 7（空白）。
    ppu.oam[0..4].copy_from_slice(&[9, 6, 0x00, 40]);

    render_line(&mut ppu, &c, 10); // 精靈第 0 列（在 tile 6 的第 0 列）
    assert_eq!(pixel(&ppu, 40, 10), color(0x13));
    render_line(&mut ppu, &c, 10 + 8); // 第 8 列 → 下半 tile 7（空白）
    assert_eq!(pixel(&ppu, 40, 18), color(0x0F));
    render_line(&mut ppu, &c, 10 + 15); // 第 15 列：仍在精靈範圍內
    assert_eq!(pixel(&ppu, 40, 25), color(0x0F));
    render_line(&mut ppu, &c, 10 + 16); // 第 16 列：已超出
    assert_eq!(pixel(&ppu, 40, 26), color(0x0F));
}

#[test]
fn nine_sprites_on_one_scanline_set_the_overflow_flag_at_dot_256() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    write(&mut ppu, &mut c, 1, 0x18);
    ppu.v = 0;
    ppu.oam.iter_mut().for_each(|b| *b = 0xFF);
    for n in 0..9 {
        ppu.oam[n * 4..n * 4 + 4].copy_from_slice(&[9, 0, 0, (n * 10) as u8]);
    }
    ppu.scanline = 10;
    ppu.cycle = 0;
    ppu.step_dots(256, &c);
    assert_eq!(ppu.status & STATUS_SPRITE_OVERFLOW, 0);
    ppu.step_dots(1, &c);
    assert_ne!(ppu.status & STATUS_SPRITE_OVERFLOW, 0);
}

#[test]
fn eight_sprites_on_one_scanline_do_not_overflow() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    write(&mut ppu, &mut c, 1, 0x18);
    ppu.v = 0;
    ppu.oam.iter_mut().for_each(|b| *b = 0xFF);
    for n in 0..8 {
        ppu.oam[n * 4..n * 4 + 4].copy_from_slice(&[9, 0, 0, (n * 10) as u8]);
    }
    ppu.scanline = 10;
    ppu.cycle = 0;
    ppu.step_dots(341, &c);
    assert_eq!(ppu.status & STATUS_SPRITE_OVERFLOW, 0);
}

#[test]
fn grayscale_and_emphasis_change_output_colors_only() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    setup_palette(&mut ppu, &mut c, &[0x16]);
    write(&mut ppu, &mut c, 1, 0x01); // 灰階
    render_line(&mut ppu, &c, 3);
    assert_eq!(pixel(&ppu, 0, 3), color(0x10));
}

#[test]
fn save_state_roundtrip_preserves_ppu_state_but_not_the_framebuffer() {
    let mut ppu = Ppu::default();
    let mut c = cart(false);
    setup_palette(&mut ppu, &mut c, &[0x21]);
    write(&mut ppu, &mut c, 1, 0x08);
    ppu.step_dots(5000, &c);

    let bytes = crate::state::encode(&ppu);
    let restored: Ppu = crate::state::decode(&bytes).unwrap();
    assert_eq!(restored.scanline, ppu.scanline);
    assert_eq!(restored.cycle, ppu.cycle);
    assert_eq!(restored.v, ppu.v);
    assert_eq!(restored.palette, ppu.palette);
    // framebuffer 是輸出，不進存檔；還原後是全黑，下一幀會重畫。
    assert!(restored.frame_buffer.as_bytes().iter().all(|&b| b == 0));
}
