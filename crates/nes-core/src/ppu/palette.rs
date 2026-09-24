//! 2C02（NTSC）調色盤。
//!
//! 來源：NESdev Wiki「PPU palettes」頁面 "2C02 and 2C07" 一節提供的 64 色
//! sRGB 表（<https://www.nesdev.org/wiki/PPU_palettes>）。該表是用 blargg 的
//! "Full Palette" 示範 ROM 在 Nestopia 上產生的；wiki 只列出每列前 14 色
//! （`$x0`–`$xD`），`$xE`/`$xF` 兩色依 wiki 說明是黑色（`$0E`/`$0F` 為
//! `$1D` 的鏡像，正規的「黑」），這裡補成 `[0, 0, 0]`。
//! 詳見 `ATTRIBUTION.md`。
//!
//! 這份表只影響「輸出的顏色」，不影響任何模擬狀態，所以換掉調色盤不會改變
//! save state 或 rollback 的決定性。

/// 64 色系統調色盤，索引是 PPU 調色盤 RAM 裡的 6 位元值。
#[rustfmt::skip]
pub const SYSTEM_PALETTE: [[u8; 3]; 64] = [
    [84, 84, 84], [0, 30, 116], [8, 16, 144], [48, 0, 136],
    [68, 0, 100], [92, 0, 48], [84, 4, 0], [60, 24, 0],
    [32, 42, 0], [8, 58, 0], [0, 64, 0], [0, 60, 0],
    [0, 50, 60], [0, 0, 0], [0, 0, 0], [0, 0, 0],
    [152, 150, 152], [8, 76, 196], [48, 50, 236], [92, 30, 228],
    [136, 20, 176], [160, 20, 100], [152, 34, 32], [120, 60, 0],
    [84, 90, 0], [40, 114, 0], [8, 124, 0], [0, 118, 40],
    [0, 102, 120], [0, 0, 0], [0, 0, 0], [0, 0, 0],
    [236, 238, 236], [76, 154, 236], [120, 124, 236], [176, 98, 236],
    [228, 84, 236], [236, 88, 180], [236, 106, 100], [212, 136, 32],
    [160, 170, 0], [116, 196, 0], [76, 208, 32], [56, 204, 108],
    [56, 180, 204], [60, 60, 60], [0, 0, 0], [0, 0, 0],
    [236, 238, 236], [168, 204, 236], [188, 188, 236], [212, 178, 236],
    [236, 174, 236], [236, 174, 212], [236, 180, 176], [228, 196, 144],
    [204, 210, 120], [180, 222, 120], [168, 226, 144], [152, 226, 180],
    [160, 214, 228], [160, 162, 160], [0, 0, 0], [0, 0, 0],
];

/// 把一個調色盤 RAM 值（6 位元）轉成 RGBA。
///
/// - `grayscale`：PPUMASK bit0，把顏色遮成只剩灰階那一欄（`& $30`）。
/// - `emphasis`：PPUMASK bit5–7（R/G/B 強調）右移後的 3 位元。NTSC 硬體上
///   每個強調位元會把「另外兩個」色頻衰減約 25%（NESdev Wiki "PPU
///   registers" 的 PPUMASK 說明）；這裡用整數 `x * 3 / 4` 近似。純視覺
///   調整，不影響模擬狀態。
pub fn to_rgba(value: u8, grayscale: bool, emphasis: u8) -> [u8; 4] {
    let mut index = value & 0x3F;
    if grayscale {
        index &= 0x30;
    }
    let mut rgb = SYSTEM_PALETTE[index as usize];
    for (channel, value) in rgb.iter_mut().enumerate() {
        // emphasis 的 bit0/1/2 依序是 R/G/B；只要有「別的」色頻被強調，就衰減這個色頻。
        let others = emphasis & !(1 << channel) & 0b111;
        for _ in 0..others.count_ones() {
            *value = (*value as u16 * 3 / 4) as u8;
        }
    }
    [rgb[0], rgb[1], rgb[2], 0xFF]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_has_expected_anchor_colors() {
        assert_eq!(to_rgba(0x00, false, 0), [84, 84, 84, 255]);
        assert_eq!(to_rgba(0x0F, false, 0), [0, 0, 0, 255]);
        assert_eq!(to_rgba(0x30, false, 0), [236, 238, 236, 255]);
    }

    #[test]
    fn grayscale_masks_to_gray_column() {
        // $16（紅）在灰階模式下變成 $10。
        assert_eq!(to_rgba(0x16, true, 0), to_rgba(0x10, false, 0));
    }

    #[test]
    fn emphasis_attenuates_the_other_channels() {
        let plain = to_rgba(0x30, false, 0);
        let red = to_rgba(0x30, false, 0b001);
        assert_eq!(red[0], plain[0]);
        assert!(red[1] < plain[1] && red[2] < plain[2]);
    }
}
