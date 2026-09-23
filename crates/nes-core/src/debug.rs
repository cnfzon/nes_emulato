//! 除錯用的唯讀快照，供 GUI 側邊的 Debugger 面板顯示。
//!
//! 這個型別刻意跟存檔格式（[`crate::Nes::save_state`]）脫鉤：它只是給人看的
//! 摘要，日後欄位可以自由增減，不會影響 rollback 的存檔相容性。

/// CPU / PPU / APU 目前狀態的唯讀摘要。
///
/// 所有欄位都反映 [`crate::Nes::debug_snapshot`] 呼叫當下的真實狀態；
/// `DebugSnapshot::default()` 只用來當作「尚未收到任何快照」的哨兵值
/// （例如 GUI 端在收到第一份快照之前的暫時狀態），不代表模擬器真的處於
/// 全 0 狀態——reset 後 SP/P 就不會是 0。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugSnapshot {
    pub cpu_pc: u16,
    pub cpu_a: u8,
    pub cpu_x: u8,
    pub cpu_y: u8,
    pub cpu_sp: u8,
    pub cpu_status: u8,
    pub cpu_cycles: u64,
    /// 目前 PC 這條指令的反組譯文字（不含暫存器/CYC 資訊，純粹是
    /// `MNEMONIC OPERAND`），方便 Debugger 面板直接顯示。
    pub cpu_disassembly: String,
    /// CPU 是否卡在 JAM/KIL 狀態。
    pub cpu_jammed: bool,
    /// 已完成的整幀數（[`crate::Nes::frame_count`]）。跟 `cpu_cycles` 出自
    /// 同一次快照，讓 GUI 可以顯示彼此一致的幀數與 cycle 數。
    pub frame_count: u64,

    pub ppu_scanline: u16,
    pub ppu_cycle: u16,
    pub ppu_frame: u64,

    pub apu_frame_counter: u8,
}
