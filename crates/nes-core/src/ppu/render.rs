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

#[cfg(test)]
thread_local! {
    /// 只給測試：`render_background` 被呼叫的次數。
    pub(crate) static BG_RENDERS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

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

        // 輸出關閉時，只有「會影響行為」的部分需要算：overflow 與 sprite 0 hit。sprite 0 hit 只取決於
        // sprite 0 的不透明像素與背景是否為不透明，其他精靈的像素與最終顏色都不影響狀態，
        // 所以只畫 sprite 0；而且這條線上沒有 sprite 0 的不透明像素時，連背景都不算
        // （`render_background`／`read_chr` 對 mapper 0–3 沒有副作用）。
        // **前提**：mapper 不觀察 CHR 讀取（`Mapper::observes_chr_reads() == false`）。MMC3 的 A12 計數、
        // MMC2 的 tile latch 這類 mapper，讀哪些位址本身就會改變狀態，一律走完整路徑。
        // 輸出開啟時完全照舊。
        let skip_unobservable = !output && !cart.mapper.observes_chr_reads();
        let mut sprites = SpriteLine {
            index: [0; WIDTH],
            behind: [false; WIDTH],
            is_sprite0: [false; WIDTH],
        };
        self.evaluate_sprites(line, cart, show_sprites, skip_unobservable, &mut sprites);

        let mut background = [0u8; WIDTH];
        if show_bg && (!skip_unobservable || sprites.is_sprite0.iter().any(|&b| b)) {
            self.render_background(cart, &mut background);
        }

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
        #[cfg(test)]
        BG_RENDERS.with(|c| c.set(c.get() + 1));
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
    /// `only_sprite0` 為 `true`（輸出關閉）時只畫 sprite 0（其餘只計數，overflow 判斷照舊）。
    fn evaluate_sprites(
        &mut self,
        line: u16,
        cart: &Cartridge,
        draw: bool,
        only_sprite0: bool,
        out: &mut SpriteLine,
    ) {
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
            if !draw || (only_sprite0 && n != 0) {
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
