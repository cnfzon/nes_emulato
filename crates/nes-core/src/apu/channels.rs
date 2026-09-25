//! APU 的五個聲道與它們共用的零件（長度計數器、包絡線）。
//!
//! 全部只用整數，全部進 save state（見 `apu/mod.rs`）。實作依據 NESdev wiki 的
//! APU、APU Length Counter、APU Envelope、APU Sweep、APU Pulse／Triangle／Noise／DMC
//! 各頁；「長度計數器的延遲寫入」與「frame counter 的細部時序」對照 blargg 的 APU 測試 ROM
//! 調整，見 `docs/architecture.md` §17。
//!
//! **計時單位一律是 CPU cycle**：每個計時器記錄「距離下一次步進還有幾個 CPU cycle」
//! （`cnt`），到 0 時步進並以「週期」重新載入。這讓 `Apu` 可以一次跳到「下一個會發生
//! 事件的 cycle」（事件驅動），而不必每個 cycle 都逐一走過。

use crate::fingerprint::Fp;

/// 長度計數器的載入值表（`$4003` 等暫存器的 bit 3–7 當索引）。
pub const LENGTH_TABLE: [u8; 32] = [
    10, 254, 20, 2, 40, 4, 80, 6, 160, 8, 60, 10, 14, 12, 26, 14, 12, 16, 24, 18, 48, 20, 96, 22,
    192, 24, 72, 26, 16, 28, 32, 30,
];

/// Pulse 的四種 duty 波形（依 Mesen 的表與遞減的序列位置；相位只影響音符的起始波形）。
const DUTY_TABLE: [[u8; 8]; 4] = [
    [0, 1, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 0, 0, 0, 0, 0],
    [0, 1, 1, 1, 1, 0, 0, 0],
    [1, 0, 0, 1, 1, 1, 1, 1],
];

/// Noise 的週期表（NTSC，單位 CPU cycle）。
pub const NOISE_PERIOD_TABLE: [u16; 16] = [
    4, 8, 16, 32, 64, 96, 128, 160, 202, 254, 380, 508, 762, 1016, 2034, 4068,
];

/// DMC 的取樣率表（NTSC，單位 CPU cycle／每個輸出 bit）。
pub const DMC_RATE_TABLE: [u16; 16] = [
    428, 380, 340, 320, 286, 254, 226, 214, 190, 160, 142, 128, 106, 84, 72, 54,
];

/// 長度計數器。
///
/// 有兩個「延遲一個 cycle 才生效」的行為（blargg 的 `len_halt_timing`／`len_reload_timing`）：
/// - 寫入 halt 旗標（`new_halt`）要到下一個 cycle 結束才複製到 `halt`；
/// - 寫入新的長度（`reload`）要到下一個 cycle 結束才載入，而且若那個 cycle 剛好被
///   frame counter 減過一次（`counter != previous`），這次載入就被忽略。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LengthCounter {
    pub counter: u8,
    pub halt: bool,
    new_halt: bool,
    /// 等待載入的長度（0 = 沒有待載入）。
    reload: u8,
    /// 寫入當下的計數值，用來判斷同一個 cycle 是否被 frame counter 減過。
    previous: u8,
}

impl LengthCounter {
    pub(super) fn fingerprint(&self, h: &mut Fp) {
        h.u8(self.counter);
        h.bool(self.halt);
        h.bool(self.new_halt);
        h.u8(self.reload);
        h.u8(self.previous);
    }

    /// 寫 halt 旗標（延遲一個 cycle 生效）。
    pub fn write_halt(&mut self, halt: bool) {
        self.new_halt = halt;
    }

    /// 寫長度索引（延遲一個 cycle 生效）。`enabled` 為 `false` 時忽略。
    pub fn load(&mut self, index: u8, enabled: bool) {
        if enabled {
            self.reload = LENGTH_TABLE[(index & 0x1F) as usize];
            self.previous = self.counter;
        }
    }

    /// frame counter 的 half-frame 時脈。
    pub fn tick(&mut self) {
        if self.counter > 0 && !self.halt {
            self.counter -= 1;
        }
    }

    /// 每個 cycle 結束（在 frame counter 之後）套用延遲的寫入。
    pub fn apply_pending(&mut self) {
        if self.reload != 0 {
            if self.counter == self.previous {
                self.counter = self.reload;
            }
            self.reload = 0;
        }
        self.halt = self.new_halt;
    }

    pub fn has_pending(&self) -> bool {
        self.reload != 0 || self.halt != self.new_halt
    }

    pub fn clear(&mut self) {
        self.counter = 0;
    }
}

/// 包絡線（Pulse 與 Noise 共用）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Envelope {
    pub looping: bool,
    pub constant: bool,
    /// 音量／包絡週期（暫存器 bit 0–3）。
    pub volume: u8,
    start: bool,
    divider: u8,
    pub decay: u8,
}

impl Envelope {
    pub(super) fn fingerprint(&self, h: &mut Fp) {
        h.bool(self.looping);
        h.bool(self.constant);
        h.u8(self.volume);
        h.bool(self.start);
        h.u8(self.divider);
        h.u8(self.decay);
    }

    pub fn write(&mut self, value: u8) {
        self.looping = value & 0x20 != 0;
        self.constant = value & 0x10 != 0;
        self.volume = value & 0x0F;
    }

    pub fn restart(&mut self) {
        self.start = true;
    }

    pub fn tick(&mut self) {
        if self.start {
            self.start = false;
            self.decay = 15;
            self.divider = self.volume;
        } else if self.divider == 0 {
            self.divider = self.volume;
            if self.decay > 0 {
                self.decay -= 1;
            } else if self.looping {
                self.decay = 15;
            }
        } else {
            self.divider -= 1;
        }
    }

    pub fn output(&self) -> u8 {
        if self.constant {
            self.volume
        } else {
            self.decay
        }
    }

    fn is_valid(&self) -> bool {
        self.volume <= 15 && self.decay <= 15 && self.divider <= 15
    }
}

/// 方波聲道。`ones_complement`：pulse 1 的 sweep 在 negate 時用 1 的補數（多減 1），
/// pulse 2 用 2 的補數。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Pulse {
    ones_complement: bool,
    pub enabled: bool,
    pub duty: u8,
    pub envelope: Envelope,
    pub length: LengthCounter,

    pub sweep_enabled: bool,
    pub sweep_period: u8,
    pub sweep_negate: bool,
    pub sweep_shift: u8,
    sweep_reload: bool,
    sweep_divider: u8,

    /// 11 bit 的計時器週期暫存器（sweep 會改寫它）。
    pub timer_period: u16,
    /// 距離下一次序列器步進的 CPU cycle 數（≥ 1）。
    pub cnt: u32,
    /// 序列器位置（0–7，遞減）。
    pub seq: u8,
}

impl Pulse {
    pub(super) fn fingerprint(&self, h: &mut Fp) {
        h.bool(self.ones_complement);
        h.bool(self.enabled);
        h.u8(self.duty);
        self.envelope.fingerprint(h);
        self.length.fingerprint(h);
        h.bool(self.sweep_enabled);
        h.u8(self.sweep_period);
        h.bool(self.sweep_negate);
        h.u8(self.sweep_shift);
        h.bool(self.sweep_reload);
        h.u8(self.sweep_divider);
        h.u16(self.timer_period);
        h.u32(self.cnt);
        h.u8(self.seq);
    }

    pub fn new(ones_complement: bool) -> Self {
        Self {
            ones_complement,
            enabled: false,
            duty: 0,
            envelope: Envelope::default(),
            length: LengthCounter::default(),
            sweep_enabled: false,
            sweep_period: 0,
            sweep_negate: false,
            sweep_shift: 0,
            sweep_reload: false,
            sweep_divider: 0,
            timer_period: 0,
            cnt: 2,
            seq: 0,
        }
    }

    pub fn write_register(&mut self, reg: u16, value: u8) {
        match reg & 3 {
            0 => {
                self.duty = value >> 6;
                self.envelope.write(value);
                self.length.write_halt(value & 0x20 != 0);
            }
            1 => {
                self.sweep_enabled = value & 0x80 != 0;
                self.sweep_period = (value >> 4) & 7;
                self.sweep_negate = value & 0x08 != 0;
                self.sweep_shift = value & 7;
                self.sweep_reload = true;
            }
            2 => self.timer_period = (self.timer_period & 0x700) | value as u16,
            _ => {
                self.timer_period = (self.timer_period & 0xFF) | ((value as u16 & 7) << 8);
                self.length.load(value >> 3, self.enabled);
                self.seq = 0;
                self.envelope.restart();
            }
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.length.clear();
        }
    }

    fn target_period(&self) -> u32 {
        let change = (self.timer_period >> self.sweep_shift) as u32;
        let period = self.timer_period as u32;
        if self.sweep_negate {
            period.saturating_sub(change + u32::from(self.ones_complement))
        } else {
            period + change
        }
    }

    fn sweep_muted(&self) -> bool {
        self.timer_period < 8 || self.target_period() > 0x7FF
    }

    /// half-frame：sweep 單元的時脈。
    pub fn tick_sweep(&mut self) {
        let adjust = self.sweep_divider == 0
            && self.sweep_enabled
            && self.sweep_shift != 0
            && !self.sweep_muted();
        if adjust {
            self.timer_period = self.target_period() as u16;
        }
        if self.sweep_divider == 0 || self.sweep_reload {
            self.sweep_divider = self.sweep_period;
            self.sweep_reload = false;
        } else {
            self.sweep_divider -= 1;
        }
    }

    /// 計時器走到 0：序列器前進一步。
    pub fn step_sequencer(&mut self) {
        self.seq = self.seq.wrapping_sub(1) & 7;
        self.cnt = (self.timer_period as u32 + 1) * 2;
    }

    pub fn output(&self) -> u8 {
        if self.length.counter == 0
            || self.sweep_muted()
            || DUTY_TABLE[(self.duty & 3) as usize][(self.seq & 7) as usize] == 0
        {
            0
        } else {
            self.envelope.output()
        }
    }

    pub(super) fn is_valid(&self) -> bool {
        self.duty < 4
            && self.timer_period <= 0x7FF
            && self.seq < 8
            && self.sweep_period < 8
            && self.sweep_shift < 8
            && self.sweep_divider < 8
            && self.cnt >= 1
            && self.cnt <= 0x1000
            && self.envelope.is_valid()
    }
}

/// 三角波聲道。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Triangle {
    pub enabled: bool,
    /// `$4008` bit 7：linear counter 的 control（同時是 length counter 的 halt）。
    pub control: bool,
    pub linear_reload_value: u8,
    pub linear_counter: u8,
    linear_reload_flag: bool,
    pub length: LengthCounter,
    pub timer_period: u16,
    pub cnt: u32,
    /// 32 步序列的位置。
    pub seq: u8,
}

impl Default for Triangle {
    fn default() -> Self {
        Self::new()
    }
}

impl Triangle {
    pub(super) fn fingerprint(&self, h: &mut Fp) {
        h.bool(self.enabled);
        h.bool(self.control);
        h.u8(self.linear_reload_value);
        h.u8(self.linear_counter);
        h.bool(self.linear_reload_flag);
        self.length.fingerprint(h);
        h.u16(self.timer_period);
        h.u32(self.cnt);
        h.u8(self.seq);
    }

    pub fn new() -> Self {
        Self {
            enabled: false,
            control: false,
            linear_reload_value: 0,
            linear_counter: 0,
            linear_reload_flag: false,
            length: LengthCounter::default(),
            timer_period: 0,
            cnt: 1,
            seq: 0,
        }
    }

    pub fn write_register(&mut self, reg: u16, value: u8) {
        match reg & 3 {
            0 => {
                self.control = value & 0x80 != 0;
                self.linear_reload_value = value & 0x7F;
                self.length.write_halt(self.control);
            }
            2 => self.timer_period = (self.timer_period & 0x700) | value as u16,
            3 => {
                self.timer_period = (self.timer_period & 0xFF) | ((value as u16 & 7) << 8);
                self.length.load(value >> 3, self.enabled);
                self.linear_reload_flag = true;
            }
            _ => {}
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.length.clear();
        }
    }

    /// quarter-frame：linear counter 的時脈。
    pub fn tick_linear(&mut self) {
        if self.linear_reload_flag {
            self.linear_counter = self.linear_reload_value;
        } else if self.linear_counter > 0 {
            self.linear_counter -= 1;
        }
        if !self.control {
            self.linear_reload_flag = false;
        }
    }

    pub fn step_sequencer(&mut self) {
        if self.length.counter > 0 && self.linear_counter > 0 {
            self.seq = (self.seq + 1) & 31;
        }
        self.cnt = self.timer_period as u32 + 1;
    }

    pub fn output(&self) -> u8 {
        // 正在跑（兩個計數器都非零）而且週期 < 2：超音波，實機上聽起來是安靜的直流偏移，
        // 所以輸出序列的平均值，避免爆音。沒在跑的時候序列停住，維持最後的值（同硬體）。
        if self.timer_period < 2 && self.length.counter > 0 && self.linear_counter > 0 {
            return 8;
        }
        if self.seq < 16 {
            15 - self.seq
        } else {
            self.seq - 16
        }
    }

    pub(super) fn is_valid(&self) -> bool {
        self.linear_reload_value < 128
            && self.linear_counter < 128
            && self.timer_period <= 0x7FF
            && self.seq < 32
            && self.cnt >= 1
            && self.cnt <= 0x801
    }
}

/// 雜訊聲道。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Noise {
    pub enabled: bool,
    pub envelope: Envelope,
    pub length: LengthCounter,
    /// `$400E` bit 7：短模式（回授取 bit 6 而不是 bit 1）。
    pub mode: bool,
    pub period_index: u8,
    pub cnt: u32,
    /// 15 bit 線性回授移位暫存器（開機為 1）。
    pub shift: u16,
}

impl Default for Noise {
    fn default() -> Self {
        Self::new()
    }
}

impl Noise {
    pub(super) fn fingerprint(&self, h: &mut Fp) {
        h.bool(self.enabled);
        self.envelope.fingerprint(h);
        self.length.fingerprint(h);
        h.bool(self.mode);
        h.u8(self.period_index);
        h.u32(self.cnt);
        h.u16(self.shift);
    }

    pub fn new() -> Self {
        Self {
            enabled: false,
            envelope: Envelope::default(),
            length: LengthCounter::default(),
            mode: false,
            period_index: 0,
            cnt: NOISE_PERIOD_TABLE[0] as u32,
            shift: 1,
        }
    }

    pub fn write_register(&mut self, reg: u16, value: u8) {
        match reg & 3 {
            0 => {
                self.envelope.write(value);
                self.length.write_halt(value & 0x20 != 0);
            }
            2 => {
                self.mode = value & 0x80 != 0;
                self.period_index = value & 0x0F;
            }
            3 => {
                self.length.load(value >> 3, self.enabled);
                self.envelope.restart();
            }
            _ => {}
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.length.clear();
        }
    }

    pub fn step_shift_register(&mut self) {
        let tap = if self.mode { 6 } else { 1 };
        let feedback = (self.shift & 1) ^ ((self.shift >> tap) & 1);
        self.shift = (self.shift >> 1) | (feedback << 14);
        self.cnt = NOISE_PERIOD_TABLE[(self.period_index & 0x0F) as usize] as u32;
    }

    pub fn output(&self) -> u8 {
        if self.shift & 1 != 0 || self.length.counter == 0 {
            0
        } else {
            self.envelope.output()
        }
    }

    pub(super) fn is_valid(&self) -> bool {
        self.period_index < 16
            && self.shift < 0x8000
            && self.cnt >= 1
            && self.cnt <= 4068
            && self.envelope.is_valid()
    }
}

/// DMC（差量調變聲道）。
///
/// 輸出單元每 `DMC_RATE_TABLE[rate]` 個 CPU cycle 處理 1 個 bit；8 個 bit 用完就從
/// 取樣緩衝區換一個新 byte，緩衝區因此變空時向記憶體要下一個 byte（DMA，見
/// `Apu::dmc_fetch`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Dmc {
    pub irq_enabled: bool,
    pub looping: bool,
    pub rate_index: u8,
    pub cnt: u32,

    /// `$4012`：取樣起始位址（`$C000 + v * 64`）。
    pub sample_addr: u16,
    /// `$4013`：取樣長度（`v * 16 + 1` byte）。
    pub sample_length: u16,
    pub current_addr: u16,
    pub bytes_remaining: u16,

    pub read_buffer: u8,
    pub buffer_empty: bool,

    pub shift_register: u8,
    /// 輸出單元剩餘的 bit 數（1–8）。
    pub bits_remaining: u8,
    pub silence: bool,
    /// 7 bit 的輸出電平。
    pub output_level: u8,

    pub irq_flag: bool,
    /// `$4015` 啟用後、DMA 開始前的延遲 cycle 數（0 = 沒有待處理）。
    pub start_delay: u8,
}

impl Default for Dmc {
    fn default() -> Self {
        Self::new()
    }
}

impl Dmc {
    pub(super) fn fingerprint(&self, h: &mut Fp) {
        h.bool(self.irq_enabled);
        h.bool(self.looping);
        h.u8(self.rate_index);
        h.u32(self.cnt);
        h.u16(self.sample_addr);
        h.u16(self.sample_length);
        h.u16(self.current_addr);
        h.u16(self.bytes_remaining);
        h.u8(self.read_buffer);
        h.bool(self.buffer_empty);
        h.u8(self.shift_register);
        h.u8(self.bits_remaining);
        h.bool(self.silence);
        h.u8(self.output_level);
        h.bool(self.irq_flag);
        h.u8(self.start_delay);
    }

    pub fn new() -> Self {
        Self {
            irq_enabled: false,
            looping: false,
            rate_index: 0,
            cnt: DMC_RATE_TABLE[0] as u32,
            sample_addr: 0xC000,
            sample_length: 1,
            current_addr: 0xC000,
            bytes_remaining: 0,
            read_buffer: 0,
            buffer_empty: true,
            shift_register: 0,
            bits_remaining: 8,
            silence: true,
            output_level: 0,
            irq_flag: false,
            start_delay: 0,
        }
    }

    pub fn write_register(&mut self, reg: u16, value: u8) {
        match reg & 3 {
            0 => {
                self.irq_enabled = value & 0x80 != 0;
                self.looping = value & 0x40 != 0;
                self.rate_index = value & 0x0F;
                if !self.irq_enabled {
                    self.irq_flag = false;
                }
            }
            1 => self.output_level = value & 0x7F,
            2 => self.sample_addr = 0xC000 | ((value as u16) << 6),
            _ => self.sample_length = ((value as u16) << 4) | 1,
        }
    }

    fn restart_sample(&mut self) {
        self.current_addr = self.sample_addr;
        self.bytes_remaining = self.sample_length;
    }

    /// `$4015` bit 4 的寫入。`odd_cycle`：寫入當下的 CPU cycle 是奇數。
    pub fn set_enabled(&mut self, enabled: bool, odd_cycle: bool) {
        if !enabled {
            self.bytes_remaining = 0;
            self.start_delay = 0;
        } else if self.bytes_remaining == 0 {
            self.restart_sample();
            // 硬體上 DMA 在啟用後 2–3 個 cycle 才開始（依 CPU cycle 奇偶）。
            self.start_delay = if odd_cycle { 3 } else { 2 };
        }
        self.irq_flag = false;
    }

    /// 輸出單元處理完一個 bit（計時器走到 0）。回傳 `true` 代表需要補一個 byte。
    pub fn step_output(&mut self) -> bool {
        self.cnt = DMC_RATE_TABLE[(self.rate_index & 0x0F) as usize] as u32;
        if !self.silence {
            if self.shift_register & 1 != 0 {
                if self.output_level <= 125 {
                    self.output_level += 2;
                }
            } else if self.output_level >= 2 {
                self.output_level -= 2;
            }
            self.shift_register >>= 1;
        }
        self.bits_remaining -= 1;
        if self.bits_remaining == 0 {
            self.bits_remaining = 8;
            if self.buffer_empty {
                self.silence = true;
            } else {
                self.silence = false;
                self.shift_register = self.read_buffer;
                self.buffer_empty = true;
                return self.bytes_remaining > 0;
            }
        }
        false
    }

    /// 需要向記憶體要一個 byte 嗎？
    pub fn wants_fetch(&self) -> bool {
        self.buffer_empty && self.bytes_remaining > 0
    }

    /// DMA 讀到的 byte 放進緩衝區，並前進位址／處理迴圈與 IRQ。
    pub fn finish_fetch(&mut self, value: u8) {
        if self.bytes_remaining == 0 {
            return;
        }
        self.read_buffer = value;
        self.buffer_empty = false;
        self.current_addr = if self.current_addr == 0xFFFF {
            0x8000
        } else {
            self.current_addr + 1
        };
        self.bytes_remaining -= 1;
        if self.bytes_remaining == 0 {
            if self.looping {
                self.restart_sample();
            } else if self.irq_enabled {
                self.irq_flag = true;
            }
        }
    }

    pub fn output(&self) -> u8 {
        self.output_level
    }

    pub(super) fn is_valid(&self) -> bool {
        self.rate_index < 16
            && (0xC000..=0xFFC0).contains(&self.sample_addr)
            && self.sample_length <= 0xFF1
            && self.current_addr >= 0x8000
            && self.bytes_remaining <= 0xFF1
            && (1..=8).contains(&self.bits_remaining)
            && self.output_level < 128
            && self.cnt >= 1
            && self.cnt <= 428
            && self.start_delay <= 3
    }
}
