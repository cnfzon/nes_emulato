//! 標準 NES 搖桿：8 個按鍵 + 之後要實作的移位暫存器 / strobe 邏輯。

use bitflags::bitflags;

bitflags! {
    /// 一個手把在某一幀的按鍵狀態。
    ///
    /// 用 `u8` bitflags 表示，方便直接序列化進封包與存檔。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
    pub struct Buttons: u8 {
        const A      = 0b0000_0001;
        const B      = 0b0000_0010;
        const SELECT = 0b0000_0100;
        const START  = 0b0000_1000;
        const UP     = 0b0001_0000;
        const DOWN   = 0b0010_0000;
        const LEFT   = 0b0100_0000;
        const RIGHT  = 0b1000_0000;
    }
}

/// 單一控制器的暫存器狀態。
///
/// `strobe` / `shift` 對應真實硬體 `$4016`/`$4017` 的移位暫存器：
/// - `$4016` bit0 的 strobe 為 1 時，移位暫存器持續重載目前按鍵，讀取永遠
///   回傳 A 鍵；
/// - strobe 拉回 0 之後，每次讀取依序移出 A、B、Select、Start、上、下、左、右，
///   移出後補 1，所以 8 次之後的讀取都回傳 1。
///
/// `state` 由 `Nes::run_frame` 在一幀開始時鎖定，整幀不變（決定性）。
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct Joypad {
    pub state: Buttons,
    pub strobe: bool,
    pub shift: u8,
}

impl Joypad {
    /// 行為指紋（`docs/architecture.md` §18.2）。
    pub(crate) fn fingerprint(&self, h: &mut crate::fingerprint::Fp) {
        h.u8(self.state.bits());
        h.bool(self.strobe);
        h.u8(self.shift);
    }

    /// 寫入 `$4016` bit0。strobe 為高（或剛從高拉低）時重載移位暫存器。
    pub fn write_strobe(&mut self, strobe: bool) {
        if strobe || self.strobe {
            self.shift = self.state.bits();
        }
        self.strobe = strobe;
    }

    /// 讀取一個 bit（bit0），**有**副作用：strobe 為低時移位。
    pub fn read(&mut self) -> u8 {
        if self.strobe {
            return self.state.bits() & 1;
        }
        let bit = self.shift & 1;
        self.shift = (self.shift >> 1) | 0x80;
        bit
    }

    /// 跟 [`Joypad::read`] 相同的值，但不移位。
    pub fn peek(&self) -> u8 {
        if self.strobe {
            self.state.bits() & 1
        } else {
            self.shift & 1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strobe_then_reads_shift_out_buttons_in_order_then_ones() {
        let mut pad = Joypad {
            state: Buttons::A | Buttons::START | Buttons::RIGHT,
            ..Joypad::default()
        };
        pad.write_strobe(true);
        pad.write_strobe(false);

        let bits: Vec<u8> = (0..8).map(|_| pad.read()).collect();
        // A, B, Select, Start, Up, Down, Left, Right
        assert_eq!(bits, [1, 0, 0, 1, 0, 0, 0, 1]);
        assert_eq!(pad.read(), 1, "8 次之後回傳 1");
        assert_eq!(pad.read(), 1);
    }

    #[test]
    fn strobe_high_keeps_returning_a_button() {
        let mut pad = Joypad {
            state: Buttons::A,
            ..Joypad::default()
        };
        pad.write_strobe(true);
        assert_eq!(pad.read(), 1);
        assert_eq!(pad.read(), 1);
        pad.state = Buttons::empty();
        assert_eq!(pad.read(), 0);
    }

    #[test]
    fn peek_does_not_shift() {
        let mut pad = Joypad {
            state: Buttons::B,
            ..Joypad::default()
        };
        pad.write_strobe(true);
        pad.write_strobe(false);
        assert_eq!(pad.peek(), 0); // A 沒按
        assert_eq!(pad.peek(), 0);
        assert_eq!(pad.read(), 0);
        assert_eq!(pad.read(), 1); // B 有按
    }

    #[test]
    fn buttons_default_is_empty() {
        assert_eq!(Buttons::default(), Buttons::empty());
    }

    #[test]
    fn buttons_compose_with_bitor() {
        let combo = Buttons::LEFT | Buttons::A;
        assert!(combo.contains(Buttons::LEFT));
        assert!(combo.contains(Buttons::A));
        assert!(!combo.contains(Buttons::RIGHT));
    }
}
