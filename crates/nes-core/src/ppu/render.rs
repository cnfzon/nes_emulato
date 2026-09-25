//! 掃描線渲染器。
//!
//! 每條可見掃描線在 dot 0 一次畫完整條（背景 + 精靈 + 優先順序 + sprite 0
//! hit 的位置計算），結果直接寫進 `frame_buffer` 的那一列。時序模型與限制
//! 見 `ppu/mod.rs` 的模組文件。
//!
//! 一條線的輸入是「dot 0 當下」的 v / fine X / PPUCTRL / PPUMASK / OAM /
//! 調色盤 / CHR：這條線之後（dot 1–340）才發生的暫存器寫入，要到下一條線
//! 才會反映——這是 scanline 級渲染跟硬體逐 dot 渲染的差別。

use super::{Ppu, palette};
use crate::cartridge::Cartridge;
use crate::frame::WIDTH;

/// 一條掃描線上精靈的疊合結果。
struct SpriteLine {
    /// 調色盤 RAM 索引（`0x10 | pal << 2 | color`）；`0` 代表這個像素沒有精靈。
    index: [u8; WIDTH],
    /// 精靈屬性 bit5：在背景之後。
    behind: [bool; WIDTH],
    /// 這個像素的精靈是 sprite 0。
    is_sprite0: [bool; WIDTH],
}

impl Ppu {
    /// 目前 mask 設定下，調色盤 RAM 32 格各自對應的 RGBA。
    fn color_table(&self) -> [[u8; 4]; 32] {
        let grayscale = self.mask & 0x01 != 0;
        let emphasis = self.mask >> 5;
        let mut table = [[0u8; 4]; 32];
        for (i, slot) in table.iter_mut().enumerate() {
            let index = if i & 3 == 0 { i & 0x0F } else { i };
            *slot = palette::to_rgba(self.palette[index], grayscale, emphasis);
        }
        table
    }

    /// 畫第 `line` 條可見掃描線，並登記這條線的 sprite 0 hit / overflow 事件。
    pub(super) fn render_scanline(&mut self, line: u16, cart: &Cartridge) {
        self.sprite0_hit_dot = 0;
        self.overflow_pending = false;

        // 輸出關閉時不寫 framebuffer，其餘判斷（sprite 0 hit、overflow）完全照舊。
        let output = self.output_enabled;
        let colors = if output {
            self.color_table()
        } else {
            [[0u8; 4]; 32]
        };
        let show_bg = self.mask & 0x08 != 0;
        let show_sprites = self.mask & 0x10 != 0;

        if !show_bg && !show_sprites {
            // 渲染關閉：整條線是背景色（$3F00）。
            if output {
                let backdrop = colors[0];
                self.frame_buffer
                    .row_mut(line as usize)
                    .as_chunks_mut::<4>()
                    .0
                    .fill(backdrop);
            }
            return;
        }

        let mut background = [0u8; WIDTH];
        if show_bg {
            self.render_background(cart, &mut background);
        }

        let mut sprites = SpriteLine {
            index: [0; WIDTH],
            behind: [false; WIDTH],
            is_sprite0: [false; WIDTH],
        };
        self.evaluate_sprites(line, cart, show_sprites, &mut sprites);

        let mut hit_x = None;
        if output {
            let row = self.frame_buffer.row_mut(line as usize);
            for x in 0..WIDTH {
                let bg = background[x];
                let sp = sprites.index[x];
                let chosen = if sp != 0 && (bg == 0 || !sprites.behind[x]) {
                    sp
                } else {
                    bg
                };
                if hit_x.is_none() && sprites.is_sprite0[x] && bg != 0 && x != 255 {
                    hit_x = Some(x);
                }
                row[x * 4..x * 4 + 4].copy_from_slice(&colors[chosen as usize]);
            }
        } else {
            hit_x = (0..WIDTH - 1).find(|&x| sprites.is_sprite0[x] && background[x] != 0);
        }
        if let Some(x) = hit_x {
            // 像素 x 在 dot x + 1 輸出。
            self.sprite0_hit_dot = x as u16 + 1;
        }
    }

    /// 背景：33 個 tile（含捲動造成的第 33 個部分 tile），結果寫進 `out`
    /// （`0` = 透明，其餘是調色盤 RAM 索引 `pal << 2 | color`）。
    fn render_background(&self, cart: &Cartridge, out: &mut [u8; WIDTH]) {
        // 預取（dot 328/336）已經讓 v 前進了 `prefetch_incs` 個 tile。
        let mut start = self.v;
        for _ in 0..self.prefetch_incs {
            start = if start & 0x001F == 0 {
                (start & !0x001F | 31) ^ 0x0400
            } else {
                start - 1
            };
        }

        let coarse_x = (start & 0x001F) as usize;
        let coarse_y = ((start >> 5) & 0x1F) as usize;
        let nametable = (start >> 10) & 0x03;
        let fine_y = ((start >> 12) & 0x07) as usize;
        let pattern_base: u16 = if self.ctrl & 0x10 != 0 { 0x1000 } else { 0 };
        let fine_x = self.fine_x as usize;
        let hide_left = self.mask & 0x02 == 0;

        for tile in 0..33usize {
            let tile_x = coarse_x + tile;
            let column = tile_x & 0x1F;
            // 越過第 31 欄就換到水平相鄰的 nametable。
            let table = nametable ^ ((tile_x as u16 >> 5) & 1);
            let table_base = 0x2000 | (table << 10);

            let tile_id =
                self.read_memory(table_base | (coarse_y as u16) << 5 | column as u16, cart);
            let attr_addr =
                table_base | 0x03C0 | ((coarse_y as u16 >> 2) << 3) | (column as u16 >> 2);
            let attr = self.read_memory(attr_addr, cart);
            let shift = ((coarse_y & 2) << 1) | (column & 2);
            let palette_select = (attr >> shift) & 0x03;

            let pattern = pattern_base + tile_id as u16 * 16 + fine_y as u16;
            let low = cart.read_chr(pattern);
            let high = cart.read_chr(pattern + 8);

            for bit in 0..8usize {
                let screen_x = (tile * 8 + bit).wrapping_sub(fine_x);
                if screen_x >= WIDTH {
                    continue;
                }
                let color = ((high >> (7 - bit)) & 1) << 1 | ((low >> (7 - bit)) & 1);
                if color == 0 || (hide_left && screen_x < 8) {
                    continue;
                }
                out[screen_x] = palette_select << 2 | color;
            }
        }
    }

    /// 這條掃描線的精靈：挑出最多 8 個（超過就登記 overflow），依 OAM 順序
    /// 疊合（先出現者優先）。`draw` 為 `false`（精靈顯示關閉）時只做 overflow 判斷。
    fn evaluate_sprites(&mut self, line: u16, cart: &Cartridge, draw: bool, out: &mut SpriteLine) {
        let tall = self.ctrl & 0x20 != 0;
        let height: i32 = if tall { 16 } else { 8 };
        let hide_left = self.mask & 0x04 == 0;

        let mut found = 0;
        for n in 0..64usize {
            let base = n * 4;
            let y = self.oam[base] as i32;
            // 精靈的第一列出現在 Y + 1 這條掃描線。
            let row = line as i32 - 1 - y;
            if !(0..height).contains(&row) {
                continue;
            }
            if found == 8 {
                self.overflow_pending = true;
                break;
            }
            found += 1;
            if !draw {
                continue;
            }

            let tile = self.oam[base + 1];
            let attr = self.oam[base + 2];
            let x = self.oam[base + 3] as usize;
            let flip_v = attr & 0x80 != 0;
            let flip_h = attr & 0x40 != 0;
            let row = if flip_v { height - 1 - row } else { row } as u16;

            let pattern = if tall {
                let table: u16 = if tile & 1 != 0 { 0x1000 } else { 0 };
                let top = (tile & 0xFE) as u16 + (row >> 3);
                table + top * 16 + (row & 7)
            } else {
                let table: u16 = if self.ctrl & 0x08 != 0 { 0x1000 } else { 0 };
                table + tile as u16 * 16 + row
            };
            let low = cart.read_chr(pattern);
            let high = cart.read_chr(pattern + 8);
            let palette_select = attr & 0x03;

            for px in 0..8usize {
                let screen_x = x + px;
                if screen_x >= WIDTH || (hide_left && screen_x < 8) {
                    continue;
                }
                let bit = if flip_h { px } else { 7 - px };
                let color = ((high >> bit) & 1) << 1 | ((low >> bit) & 1);
                if color == 0 || out.index[screen_x] != 0 {
                    continue;
                }
                out.index[screen_x] = 0x10 | palette_select << 2 | color;
                out.behind[screen_x] = attr & 0x20 != 0;
                out.is_sprite0[screen_x] = n == 0;
            }
        }
    }
}
