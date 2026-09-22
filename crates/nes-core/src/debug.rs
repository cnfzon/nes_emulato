//! 除錯用的唯讀快照，供 GUI 側邊的 Debugger 面板顯示。
//!
//! 這個型別刻意跟存檔格式（[`crate::Nes::save_state`]）脫鉤：它只是給人看的
//! 摘要，日後欄位可以自由增減，不會影響 rollback 的存檔相容性。

/// CPU / PPU / APU 目前狀態的唯讀摘要。
///
/// Phase 0 大部分欄位都還是預設值，因為 CPU/PPU/APU 尚未實作；等 Phase 1
/// 把 6502 與 PPU 掃描線邏輯接上後，這些欄位就會反映真實狀態。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DebugSnapshot {
    pub cpu_pc: u16,
    pub cpu_a: u8,
    pub cpu_x: u8,
    pub cpu_y: u8,
    pub cpu_sp: u8,
    pub cpu_status: u8,
    pub cpu_cycles: u64,

    pub ppu_scanline: u16,
    pub ppu_cycle: u16,
    pub ppu_frame: u64,

    pub apu_frame_counter: u8,
}
