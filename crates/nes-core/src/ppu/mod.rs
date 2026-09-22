//! PPU（picture processing unit）暫存器狀態。
//!
//! Phase 0 只放暫存器與記憶體陣列，不做任何掃描線時序或渲染邏輯 —— 目前的
//! 畫面輸出由 [`crate::frame::FrameBuffer::render_test_pattern`] 佔位。

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Ppu {
    pub ctrl: u8,         // $2000 PPUCTRL
    pub mask: u8,         // $2001 PPUMASK
    pub status: u8,       // $2002 PPUSTATUS
    pub oam_addr: u8,     // $2003 OAMADDR
    pub scroll_x: u8,     // $2005 latch (first write)
    pub scroll_y: u8,     // $2005 latch (second write)
    pub addr: u16,        // $2006 目前 VRAM 位址（已組好的 15-bit）
    pub addr_latch: bool, // $2006/$2005 的高/低位元組寫入順序 latch
    pub data_buffer: u8,  // $2007 讀取時的一個 byte 延遲緩衝

    /// 256 bytes，OAM sprite RAM。用 `Vec` 而非定長陣列是因為 serde 的
    /// `derive(Serialize, Deserialize)` 只原生支援長度 <= 32 的陣列；長度
    /// 由建構時的 `vec![0; 256]` 保證，之後不會改變。
    pub oam: Vec<u8>,
    /// 2KB nametable RAM，同上理由使用 `Vec`（長度固定為 2048）。
    pub vram: Vec<u8>,
    pub palette: [u8; 32],

    pub scanline: u16,
    pub cycle: u16,
    pub frame: u64,
    pub nmi_pending: bool,
}

impl Default for Ppu {
    fn default() -> Self {
        Self {
            ctrl: 0,
            mask: 0,
            status: 0,
            oam_addr: 0,
            scroll_x: 0,
            scroll_y: 0,
            addr: 0,
            addr_latch: false,
            data_buffer: 0,
            oam: vec![0; 256],
            vram: vec![0; 2048],
            palette: [0; 32],
            scanline: 0,
            cycle: 0,
            frame: 0,
            nmi_pending: false,
        }
    }
}

impl Ppu {
    /// 依照 CPU 消耗的週期數推進 PPU 時序（PPU 時脈是 CPU 的 3 倍）。
    ///
    /// TODO Phase 1: 實作背景 / 精靈掃描線渲染，並在 vblank 開始時設定
    /// `status` 的 bit7 與 `nmi_pending`。
    pub fn step(&mut self, _cpu_cycles: u64) {}
}
