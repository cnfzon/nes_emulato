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
/// `strobe` / `shift` 對應真實硬體 $4016/$4017 的移位暫存器讀取邏輯，
/// 會在 Phase 1 與 `Bus::read`/`Bus::write` 一起實作。
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct Joypad {
    pub state: Buttons,
    pub strobe: bool,
    pub shift: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

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
