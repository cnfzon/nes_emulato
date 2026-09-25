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
    pub ppu_ctrl: u8,
    pub ppu_mask: u8,
    pub ppu_status: u8,
    pub ppu_oam_addr: u8,
    /// loopy v / t / fine X / w。
    pub ppu_v: u16,
    pub ppu_t: u16,
    pub ppu_fine_x: u8,
    pub ppu_w: bool,
    /// 32 byte 調色盤 RAM。
    pub palette_ram: [u8; 32],
    /// 256 byte OAM 原始內容（64 個精靈 × 4 byte：Y、tile、attr、X）。
    pub oam: Vec<u8>,

    pub apu: ApuDebug,

    /// iNES mapper 編號與名稱（例如 1 / "MMC1"）。
    pub mapper_id: u8,
    pub mapper_name: String,
    /// mapper 的 bank 暫存器與它們目前造成的實際對應，`(名稱, 內容)` 列，照顯示順序。
    pub mapper_regs: Vec<(String, String)>,
}

/// 一個 pulse 聲道的暫存器與計數器。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PulseDebug {
    pub enabled: bool,
    pub duty: u8,
    /// 長度計數器目前的值（0 = 靜音）。
    pub length: u8,
    /// 長度計數器 halt（同時是包絡線的 loop）。
    pub halt: bool,
    /// 固定音量（否則用包絡線）。
    pub constant: bool,
    /// 音量／包絡週期（暫存器 bit 0–3）。
    pub volume: u8,
    /// 包絡線目前的衰減值。
    pub envelope: u8,
    pub sweep_enabled: bool,
    pub sweep_period: u8,
    pub sweep_negate: bool,
    pub sweep_shift: u8,
    pub timer_period: u16,
    pub seq: u8,
    /// 目前輸出的 4 bit 值（0–15）。
    pub output: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TriangleDebug {
    pub enabled: bool,
    pub control: bool,
    pub linear_reload: u8,
    pub linear_counter: u8,
    pub length: u8,
    pub timer_period: u16,
    pub seq: u8,
    pub output: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoiseDebug {
    pub enabled: bool,
    pub mode: bool,
    pub period_index: u8,
    pub length: u8,
    pub halt: bool,
    pub constant: bool,
    pub volume: u8,
    pub envelope: u8,
    pub shift: u16,
    pub output: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DmcDebug {
    pub irq_enabled: bool,
    pub looping: bool,
    pub rate_index: u8,
    pub sample_addr: u16,
    pub sample_length: u16,
    pub current_addr: u16,
    pub bytes_remaining: u16,
    pub output_level: u8,
    pub irq_flag: bool,
}

/// APU 的暫存器與計數器摘要（Debugger 的 APU 分頁）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApuDebug {
    pub pulse: [PulseDebug; 2],
    pub triangle: TriangleDebug,
    pub noise: NoiseDebug,
    pub dmc: DmcDebug,
    /// frame counter：5 步模式、IRQ 抑制、下一個步驟、已經過的 cycle 數。
    pub frame_mode5: bool,
    pub frame_inhibit_irq: bool,
    pub frame_step: u8,
    pub frame_cycle: u32,
    pub frame_irq: bool,
    /// `$4015` 讀值（不含 bit 5）。
    pub status: u8,
}

/// 一張 RGBA8 影像（Debugger 顯示用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PpuImage {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl PpuImage {
    pub(crate) fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            rgba: vec![0; width * height * 4],
        }
    }

    pub(crate) fn set_pixel(&mut self, x: usize, y: usize, rgba: [u8; 4]) {
        let i = (y * self.width + x) * 4;
        self.rgba[i..i + 4].copy_from_slice(&rgba);
    }
}

/// [`crate::Nes::debug_ppu_views`] 的結果：2 張 pattern table 與 4 張 nametable。
///
/// 只在 Debugger 面板開著且該分頁可見時才產生，**不得每幀計算**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PpuViews {
    /// `$0000` 與 `$1000` 兩張 pattern table，各 128×128。
    pub pattern_tables: [PpuImage; 2],
    /// 邏輯 nametable `$2000/$2400/$2800/$2C00`，各 256×240，依卡帶 mirroring
    /// 對到實體 2KB VRAM（被鏡像的兩張內容相同）。
    pub nametables: [PpuImage; 4],
}
