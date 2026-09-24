//! Debugger 用的 PPU 影像：pattern table 與 nametable。
//!
//! 只在 Debugger 面板真的需要時才由 `Nes::debug_ppu_views` 產生（每次呼叫
//! 要畫 2 張 128×128 加 4 張 256×240），不得每幀呼叫。純讀取、沒有副作用。

use super::{Ppu, palette};
use crate::cartridge::Cartridge;
use crate::debug::{PpuImage, PpuViews};

impl Ppu {
    /// 目前調色盤 RAM 32 格各自的 RGBA（不套用灰階/強調，Debugger 看的是原色）。
    fn view_colors(&self) -> [[u8; 4]; 32] {
        let mut table = [[0u8; 4]; 32];
        for (i, slot) in table.iter_mut().enumerate() {
            let index = if i & 3 == 0 { i & 0x0F } else { i };
            *slot = palette::to_rgba(self.palette[index], false, 0);
        }
        table
    }

    /// 產生 pattern table 與 nametable 影像。
    ///
    /// `palette_index`（0–7）選擇畫 pattern table 時用哪一組調色盤：0–3 是背景
    /// 調色盤、4–7 是精靈調色盤。超出範圍時取低 3 位元。
    pub(crate) fn build_views(&self, cart: &Cartridge, palette_index: u8) -> PpuViews {
        let colors = self.view_colors();
        let base = (palette_index & 7) as usize * 4;

        let pattern_tables = [0u16, 0x1000].map(|table_base| {
            let mut image = PpuImage::new(128, 128);
            for tile in 0..256u16 {
                let (tile_x, tile_y) = ((tile % 16) as usize, (tile / 16) as usize);
                self.draw_tile(
                    cart,
                    &mut image,
                    table_base + tile * 16,
                    tile_x * 8,
                    tile_y * 8,
                    |color| colors[if color == 0 { 0 } else { base + color as usize }],
                );
            }
            image
        });

        let pattern_base: u16 = if self.ctrl & 0x10 != 0 { 0x1000 } else { 0 };
        let nametables = [0u16, 1, 2, 3].map(|table| {
            let table_base = 0x2000 + table * 0x400;
            let mut image = PpuImage::new(256, 240);
            for row in 0..30u16 {
                for column in 0..32u16 {
                    let tile_id = self.read_memory(table_base + row * 32 + column, cart);
                    let attr =
                        self.read_memory(table_base + 0x3C0 + (row >> 2) * 8 + (column >> 2), cart);
                    let shift = ((row & 2) << 1) | (column & 2);
                    let select = ((attr >> shift) & 3) as usize * 4;
                    self.draw_tile(
                        cart,
                        &mut image,
                        pattern_base + tile_id as u16 * 16,
                        column as usize * 8,
                        row as usize * 8,
                        |color| {
                            colors[if color == 0 {
                                0
                            } else {
                                select + color as usize
                            }]
                        },
                    );
                }
            }
            image
        });

        PpuViews {
            pattern_tables,
            nametables,
        }
    }

    /// 把 `pattern_addr` 開始的一個 8×8 tile 畫到 `image` 的 `(x0, y0)`。
    fn draw_tile(
        &self,
        cart: &Cartridge,
        image: &mut PpuImage,
        pattern_addr: u16,
        x0: usize,
        y0: usize,
        color_of: impl Fn(u8) -> [u8; 4],
    ) {
        for row in 0..8u16 {
            let low = cart.read_chr(pattern_addr + row);
            let high = cart.read_chr(pattern_addr + row + 8);
            for bit in 0..8usize {
                let color = ((high >> (7 - bit)) & 1) << 1 | ((low >> (7 - bit)) & 1);
                image.set_pixel(x0 + bit, y0 + row as usize, color_of(color));
            }
        }
    }
}
