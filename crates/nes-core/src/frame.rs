//! 畫面緩衝區：256x240 RGBA8。
//!
//! 真正的畫面由 PPU 渲染器（`ppu/render.rs`）直接寫進這個緩衝區；
//! [`FrameBuffer::render_test_pattern`] 是 Phase 0 遺留的「可辨識的測試畫面」：依 `frame_count` 捲動的漸層，加上一塊會隨
//! 玩家輸入變色的色塊。這張畫面完全是 `(frame_count, input)` 的函式，藉此
//! 驗證「執行緒 → 輸入 → 畫面」這條管線在 GUI 端是通的，且不破壞決定性。

use crate::joypad::Buttons;

pub const WIDTH: usize = 256;
pub const HEIGHT: usize = 240;

/// 一張 RGBA8 畫面。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FrameBuffer {
    pixels: Vec<u8>,
}

impl Default for FrameBuffer {
    fn default() -> Self {
        Self::blank()
    }
}

impl FrameBuffer {
    /// 建立一張全黑畫面。
    pub fn blank() -> Self {
        Self {
            pixels: vec![0u8; WIDTH * HEIGHT * 4],
        }
    }

    /// 取得底層 RGBA8 位元組，供 GUI 端建立 texture 使用。
    pub fn as_bytes(&self) -> &[u8] {
        &self.pixels
    }

    /// 畫面內容的 xxh3-64 雜湊。給黃金畫面（golden frame）回歸測試用：只存雜湊，
    /// 不存圖片。
    pub fn hash64(&self) -> u64 {
        xxhash_rust::xxh3::xxh3_64(&self.pixels)
    }

    /// 第 `y` 列的 RGBA 位元組（長度 `WIDTH * 4`）。給 PPU 渲染器整列寫入用。
    pub(crate) fn row_mut(&mut self, y: usize) -> &mut [u8] {
        &mut self.pixels[y * WIDTH * 4..(y + 1) * WIDTH * 4]
    }

    fn set_pixel(&mut self, x: usize, y: usize, rgba: [u8; 4]) {
        let i = (y * WIDTH + x) * 4;
        self.pixels[i..i + 4].copy_from_slice(&rgba);
    }

    /// Phase 0 佔位畫面產生器。PPU 渲染器（Phase 2）已取代它在 `Nes::run_frame`
    /// 中的角色，保留它作為「尚未載入 ROM」等情境的測試畫面與管線診斷用。
    ///
    /// 畫面內容只由 `frame_count` 與 `input` 決定，不讀取任何其他狀態，
    /// 因此兩個吃到相同輸入序列的 `Nes` 實例永遠會畫出一模一樣的畫面。
    pub fn render_test_pattern(&mut self, frame_count: u64, input: [Buttons; 2]) {
        let shift = (frame_count % WIDTH as u64) as usize;
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let r = ((x + shift) % 256) as u8;
                let g = ((y + shift / 2) % 256) as u8;
                let b = (((x + y) / 2 + shift) % 256) as u8;
                self.set_pixel(x, y, [r, g, b, 0xFF]);
            }
        }

        // 左上角 32x32 的指示色塊：顏色隨玩家 1 目前按下的方向鍵改變，
        // 用來肉眼確認「鍵盤輸入 -> emu 執行緒 -> 畫面」這條路徑有作用。
        let color = indicator_color(input[0]);
        for y in 0..32.min(HEIGHT) {
            for x in 0..32.min(WIDTH) {
                self.set_pixel(x, y, color);
            }
        }
    }
}

fn indicator_color(buttons: Buttons) -> [u8; 4] {
    let r = if buttons.contains(Buttons::LEFT) {
        0xFF
    } else {
        0x30
    };
    let g = if buttons.contains(Buttons::UP) {
        0xFF
    } else {
        0x30
    };
    let b = if buttons.contains(Buttons::RIGHT) {
        0xFF
    } else {
        0x30
    };
    let flash = if buttons.contains(Buttons::DOWN) {
        0xFF
    } else {
        0x30
    };
    [r, g, b.max(flash), 0xFF]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_frame_has_correct_size() {
        let fb = FrameBuffer::blank();
        assert_eq!(fb.as_bytes().len(), WIDTH * HEIGHT * 4);
    }

    #[test]
    fn render_test_pattern_is_pure_function_of_state_and_input() {
        let mut a = FrameBuffer::blank();
        let mut b = FrameBuffer::blank();
        let input = [Buttons::LEFT, Buttons::empty()];
        a.render_test_pattern(42, input);
        b.render_test_pattern(42, input);
        assert_eq!(a.as_bytes(), b.as_bytes());
    }

    #[test]
    fn different_input_changes_indicator_block() {
        let mut a = FrameBuffer::blank();
        let mut b = FrameBuffer::blank();
        a.render_test_pattern(1, [Buttons::empty(), Buttons::empty()]);
        b.render_test_pattern(1, [Buttons::LEFT, Buttons::empty()]);
        assert_ne!(a.as_bytes(), b.as_bytes());
    }
}
