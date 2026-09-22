//! 6502 處理器狀態暫存器（P）的旗標。

use bitflags::bitflags;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct StatusFlags: u8 {
        const CARRY     = 0b0000_0001;
        const ZERO      = 0b0000_0010;
        const INTERRUPT = 0b0000_0100;
        const DECIMAL   = 0b0000_1000;
        const BREAK     = 0b0001_0000;
        const UNUSED    = 0b0010_0000;
        const OVERFLOW  = 0b0100_0000;
        const NEGATIVE  = 0b1000_0000;
    }
}

impl Default for StatusFlags {
    /// reset 後的狀態：$24 = UNUSED | INTERRUPT。
    fn default() -> Self {
        StatusFlags::UNUSED | StatusFlags::INTERRUPT
    }
}

impl StatusFlags {
    pub fn set_zero_negative(&mut self, value: u8) {
        self.set(StatusFlags::ZERO, value == 0);
        self.set(StatusFlags::NEGATIVE, value & 0x80 != 0);
    }
}
