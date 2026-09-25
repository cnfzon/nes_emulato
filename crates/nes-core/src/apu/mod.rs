//! APU（audio processing unit，2A03 內建的音效單元）。
//!
//! 五個聲道（兩個 pulse、triangle、noise、DMC）、frame counter、`$4015` 狀態，全部只用整數
//! 並進 save state。它會影響 CPU 的行為：`$4015` 的讀值、frame IRQ、DMC IRQ、DMC 抓取樣本
//! 時暫停 CPU（見 [`DMC_STALL_CYCLES`]），所以 APU 是模擬狀態的一部分，不是純輸出。
//! 混音、降頻、濾波（[`output`]）才是輸出，不進 save state。
//!
//! # 時序模型
//!
//! - APU 由 `Bus::advance` 驅動，與 PPU 一樣採用分段 catch-up：`Cpu::step` 在指令執行前追上
//!   `N − 1` 個 cycle、執行後補最後 1 個。
//! - **存取前先跑一個 cycle**（[`Apu::sync`]）：讀寫 APU 暫存器之前，APU 額外多跑 1 個 cycle
//!   （記在 `ahead`，之後那個 cycle 的 `step` 就跳過）。這讓「寫入時 APU 已經處理過該 cycle」，
//!   與逐 cycle 模擬器（Mesen）的約定一致；frame counter 的 cycle 數與 blargg 的時序測試
//!   都是照這個約定校準的（見 `docs/architecture.md` §17）。
//! - **事件驅動**：每個計時器都記錄「距離下一次步進還有幾個 CPU cycle」，[`Apu::step`] 一次
//!   跳到最近的事件（frame counter 的下一個步驟、任一計時器到期、`$4017` 的生效延遲……），
//!   兩個事件之間所有狀態都不變，所以整段區間的混音電平是常數。

mod channels;
mod output;

pub use channels::{
    DMC_RATE_TABLE, Dmc, Envelope, LENGTH_TABLE, LengthCounter, NOISE_PERIOD_TABLE, Noise, Pulse,
    Triangle,
};
pub use output::{
    ALL_CHANNELS, AudioOut, CHANNEL_DMC, CHANNEL_NOISE, CHANNEL_PULSE1, CHANNEL_PULSE2,
    CHANNEL_TRIANGLE, CPU_CLOCK_HZ, DEFAULT_SAMPLE_RATE,
};

use crate::cartridge::Cartridge;
use crate::debug::{ApuDebug, DmcDebug, NoiseDebug, PulseDebug, TriangleDebug};
use crate::fingerprint::Fp;

/// DMC 抓一個 byte 時 CPU 被暫停的 cycle 數。
///
/// 真實硬體是 3–4 個 cycle（1 個 halt、1 個 dummy、對齊用的 1 個、實際讀取 1 個），依 CPU 正在
/// 執行的是讀還是寫、以及與 OAM DMA 是否重疊而定（重疊時甚至更少）。instruction-level 模型
/// 沒有逐 cycle 的資訊，所以固定取 4（最常見的情形）。誤差：每次抓取最多多算 1 個 cycle；
/// DMC 最快每 54 × 8 = 432 個 cycle 抓一次，所以最壞情況每秒約多暫停 4000 個 cycle（0.2%）。
/// 抓取的時間點也只到「一條指令內」的精度，而不是精確的 cycle。
///
/// **不模擬** DMC DMA 與 `$4016`／`$2007` 讀取重疊時的副作用（重複讀取造成搖桿多移位、
/// PPU 位址多前進）。理由：這是硬體的 bug，需要知道 DMA 落在 CPU 哪個 cycle；少數遊戲會為此
/// 加上重讀來繞過，模擬與否都能正常運作。
pub const DMC_STALL_CYCLES: u32 = 4;

/// frame counter 各步驟發生的 cycle（自上次重啟起算，NTSC）。[模式][步驟]
const STEP_CYCLES: [[u32; 6]; 2] = [
    [7457, 14913, 22371, 29828, 29829, 29830],
    [7457, 14913, 22371, 29829, 37281, 37282],
];
/// 各步驟的動作：0 無、1 quarter frame（包絡線、linear counter）、
/// 2 half frame（quarter 加上長度計數器與 sweep）。
const STEP_KIND: [[u8; 6]; 2] = [[1, 2, 1, 0, 2, 0], [1, 2, 1, 0, 2, 0]];

/// frame counter（`$4017`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FrameCounter {
    /// 5 步模式（`$4017` bit 7）。
    pub mode5: bool,
    /// IRQ 抑制（`$4017` bit 6）。
    pub inhibit_irq: bool,
    /// 下一個要發生的步驟（0–5）。
    pub step: u8,
    /// 自上次重啟起經過的 cycle 數。
    pub cycle: u32,
    /// `$4017` 寫入到生效的剩餘 cycle 數（0 = 沒有待生效的寫入）。
    write_delay: u8,
    new_mode5: bool,
    /// 剛用過一次 frame 時脈之後的兩個 cycle 內，不會再有第二次（避免 `$4017` 立即時脈
    /// 與自然時脈重複）。
    block: u8,
}

impl FrameCounter {
    fn fingerprint(&self, h: &mut Fp) {
        h.bool(self.mode5);
        h.bool(self.inhibit_irq);
        h.u8(self.step);
        h.u32(self.cycle);
        h.u8(self.write_delay);
        h.bool(self.new_mode5);
        h.u8(self.block);
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Apu {
    pub pulse: [Pulse; 2],
    pub triangle: Triangle,
    pub noise: Noise,
    pub dmc: Dmc,
    pub frame: FrameCounter,
    /// frame IRQ 旗標（`$4015` bit 6）。
    frame_irq: bool,
    /// 已處理的 CPU cycle 總數（含 `ahead` 提前跑掉的），用來判斷寫入時的奇偶。
    cycles: u64,
    /// 存取暫存器時提前跑掉、後面的 `step` 要跳過的 cycle 數（0 或 1）。
    ahead: u8,
    /// DMC 抓取樣本累積、尚未由 `Bus` 補上的 CPU 暫停 cycle 數。
    dmc_stall: u32,

    /// 輸出管線（不進 save state；見 `output.rs`）。
    #[serde(skip)]
    out: AudioOut,
}

impl Default for Apu {
    fn default() -> Self {
        Self::new()
    }
}

impl Apu {
    /// 冷開機。
    pub fn new() -> Self {
        let mut apu = Self {
            pulse: [Pulse::new(true), Pulse::new(false)],
            triangle: Triangle::new(),
            noise: Noise::new(),
            dmc: Dmc::new(),
            frame: FrameCounter {
                mode5: false,
                inhibit_irq: false,
                step: 0,
                cycle: 0,
                write_delay: 0,
                new_mode5: false,
                block: 0,
            },
            frame_irq: false,
            cycles: 0,
            ahead: 0,
            dmc_stall: 0,
            out: AudioOut::default(),
        };
        apu.reset(false);
        apu
    }

    /// 行為指紋（`docs/architecture.md` §18.2）。排除 `out`（輸出管線）。
    pub(crate) fn fingerprint(&self, h: &mut Fp) {
        for pulse in &self.pulse {
            pulse.fingerprint(h);
        }
        self.triangle.fingerprint(h);
        self.noise.fingerprint(h);
        self.dmc.fingerprint(h);
        self.frame.fingerprint(h);
        h.bool(self.frame_irq);
        h.u64(self.cycles);
        h.u8(self.ahead);
        h.u32(self.dmc_stall);
    }

    /// reset：冷開機（`soft = false`）或按下 reset 鍵（`soft = true`）。
    ///
    /// 硬體上兩者都等同於「寫 `$4015 = 0`、寫 `$4017`」，後者在第一條指令之前 9–12 個 cycle
    /// 發生：冷開機寫 `$00`，按 reset 則重寫最後一次寫入的模式。frame IRQ 旗標清掉；
    /// 各聲道的計時器、序列器、DMC 輸出電平不受影響。
    pub fn reset(&mut self, soft: bool) {
        self.pulse[0].set_enabled(false);
        self.pulse[1].set_enabled(false);
        self.triangle.set_enabled(false);
        self.noise.set_enabled(false);
        self.dmc.set_enabled(false, false);
        self.frame_irq = false;

        // 硬體在第一條指令之前 9–12 個 cycle 就已經當作寫過 `$4017`（blargg `09.reset_timing`）。
        // CPU 的 reset 序列（7 個 cycle）在這之後才執行，所以計數器從 0 開始、reset 序列結束時
        // 已經走了 7 個 cycle，對應「寫入生效後 7 個 cycle ＝ 寫入後 10 個 cycle」。
        self.frame.mode5 = soft && self.frame.mode5;
        self.frame.new_mode5 = self.frame.mode5;
        self.frame.inhibit_irq = false;
        self.frame.step = 0;
        self.frame.cycle = 0;
        self.frame.write_delay = 0;
        self.frame.block = 0;

        self.ahead = 0;
        self.dmc_stall = 0;
        self.out.reset_signal();
    }

    /// 檢查（從存檔還原的）內部欄位是否都在硬體可能出現的範圍內。
    pub(crate) fn is_structurally_valid(&self) -> bool {
        self.pulse.iter().all(Pulse::is_valid)
            && self.triangle.is_valid()
            && self.noise.is_valid()
            && self.dmc.is_valid()
            && self.frame.step <= 5
            // 不變式：目前的 cycle 一定還沒到下一個步驟（到了就會處理並前進）。
            && self.frame.cycle
                < STEP_CYCLES[usize::from(self.frame.mode5)][usize::from(self.frame.step)]
            && self.frame.write_delay <= 4
            && self.frame.block <= 2
            && self.ahead <= 1
            && self.dmc_stall <= 64
    }

    // ---- 輸出管線的設定（不影響模擬狀態）-------------------------------------

    pub fn set_output_enabled(&mut self, enabled: bool) {
        self.out.enabled = enabled;
        if !enabled {
            self.out.reset_signal();
        }
    }

    pub fn output_enabled(&self) -> bool {
        self.out.enabled
    }

    pub fn set_sample_rate(&mut self, hz: f64) {
        self.out.set_sample_rate(hz);
    }

    pub fn sample_rate(&self) -> f64 {
        self.out.sample_rate()
    }

    /// 設定聽得到的聲道（`CHANNEL_*` 位元，1 = 聽得到）。只影響混音，不影響模擬狀態。
    pub fn set_channel_mask(&mut self, mask: u8) {
        self.out.channel_mask = mask & ALL_CHANNELS;
    }

    pub fn channel_mask(&self) -> u8 {
        self.out.channel_mask
    }

    /// 把累積的音訊取樣搬到 `out` 裡，呼叫後內部緩衝區清空。
    pub fn take_samples(&mut self, out: &mut Vec<f32>) {
        self.out.take_samples(out);
    }

    /// 輸出管線裡尚未取走的取樣數。
    pub fn buffered_samples(&self) -> usize {
        self.out.buffered()
    }

    /// `load_state` 用：把輸出設定（開關、聲道遮罩、取樣率）從目前的 APU 帶到剛還原的 APU，
    /// 並重設濾波器與尚未取走的取樣。
    pub(crate) fn adopt_output_settings(&mut self, previous: &Apu) {
        self.out.adopt_settings(&previous.out);
    }

    // ---- 暫存器 --------------------------------------------------------

    /// CPU 存取 APU 暫存器之前呼叫：讓 APU 先處理完「存取所在的那個 cycle」。
    /// 見模組文件「存取前先跑一個 cycle」。
    pub(crate) fn sync(&mut self, cart: &Cartridge) {
        if self.ahead == 0 {
            self.run(1, cart);
            self.ahead = 1;
        }
    }

    /// 寫入 `$4000-$4013`、`$4015`、`$4017`（呼叫前要先 [`Apu::sync`]）。
    pub(crate) fn write_register(&mut self, addr: u16, value: u8) {
        match addr {
            0x4000..=0x4003 => self.pulse[0].write_register(addr, value),
            0x4004..=0x4007 => self.pulse[1].write_register(addr, value),
            0x4008..=0x400B => self.triangle.write_register(addr, value),
            0x400C..=0x400F => self.noise.write_register(addr, value),
            0x4010..=0x4013 => self.dmc.write_register(addr, value),
            0x4015 => {
                self.pulse[0].set_enabled(value & 0x01 != 0);
                self.pulse[1].set_enabled(value & 0x02 != 0);
                self.triangle.set_enabled(value & 0x04 != 0);
                self.noise.set_enabled(value & 0x08 != 0);
                self.dmc
                    .set_enabled(value & 0x10 != 0, self.cycles & 1 == 1);
            }
            0x4017 => {
                self.frame.new_mode5 = value & 0x80 != 0;
                // 若寫入落在 APU cycle 之間（CPU cycle 為奇數），生效再晚一個 cycle。
                self.frame.write_delay = if self.cycles & 1 == 1 { 4 } else { 3 };
                self.frame.inhibit_irq = value & 0x40 != 0;
                if self.frame.inhibit_irq {
                    self.frame_irq = false;
                }
            }
            _ => {}
        }
    }

    /// `$4015` 的內容（不含 bit 5，那是 open bus）。沒有副作用。
    pub fn peek_status(&self) -> u8 {
        u8::from(self.pulse[0].length.counter > 0)
            | u8::from(self.pulse[1].length.counter > 0) << 1
            | u8::from(self.triangle.length.counter > 0) << 2
            | u8::from(self.noise.length.counter > 0) << 3
            | u8::from(self.dmc.bytes_remaining > 0) << 4
            | u8::from(self.frame_irq) << 6
            | u8::from(self.dmc.irq_flag) << 7
    }

    /// 讀 `$4015`（呼叫前要先 [`Apu::sync`]）：回傳狀態並清掉 frame IRQ 旗標。
    pub(crate) fn read_status(&mut self) -> u8 {
        let status = self.peek_status();
        self.frame_irq = false;
        status
    }

    /// IRQ 線的電位（level-triggered）：frame IRQ 或 DMC IRQ。
    pub fn irq(&self) -> bool {
        self.frame_irq || self.dmc.irq_flag
    }

    /// 取走 DMC 抓取樣本累積的 CPU 暫停 cycle 數。
    pub(crate) fn take_dmc_stall(&mut self) -> u32 {
        std::mem::take(&mut self.dmc_stall)
    }

    // ---- 計時 ----------------------------------------------------------

    /// 依 CPU 消耗的 cycle 數推進 APU。`cart` 給 DMC 抓取樣本用。
    pub(crate) fn step(&mut self, cycles: u32, cart: &Cartridge) {
        let skipped = u32::from(self.ahead).min(cycles);
        self.ahead -= skipped as u8;
        self.run(cycles - skipped, cart);
    }

    fn run(&mut self, mut cycles: u32, cart: &Cartridge) {
        while cycles > 0 {
            let chunk = self.cycles_to_next_event().min(cycles);
            self.advance(chunk, cart);
            cycles -= chunk;
        }
    }

    /// 到「下一個會改變狀態的事件」還有幾個 cycle（≥ 1）。
    fn cycles_to_next_event(&self) -> u32 {
        let f = &self.frame;
        let mut n = STEP_CYCLES[usize::from(f.mode5)][usize::from(f.step)] - f.cycle;
        if f.write_delay > 0 {
            n = n.min(f.write_delay as u32);
        }
        if self.dmc.start_delay > 0 {
            n = n.min(self.dmc.start_delay as u32);
        }
        let pending = self.pulse.iter().any(|p| p.length.has_pending())
            || self.triangle.length.has_pending()
            || self.noise.length.has_pending();
        if pending {
            return 1;
        }
        n.min(self.pulse[0].cnt)
            .min(self.pulse[1].cnt)
            .min(self.triangle.cnt)
            .min(self.noise.cnt)
            .min(self.dmc.cnt)
            .max(1)
    }

    /// 推進 `d` 個 cycle。`d` 不會超過 [`Apu::cycles_to_next_event`]，所以事件只會發生在
    /// 最後一個 cycle。
    fn advance(&mut self, d: u32, cart: &Cartridge) {
        self.cycles += d as u64;

        // 混音：這 d 個 cycle 內電平不變（事件在最後一個 cycle 才發生）。
        if self.out.enabled {
            let level = self.out.mix(
                self.pulse[0].output(),
                self.pulse[1].output(),
                self.triangle.output(),
                self.noise.output(),
                self.dmc.output(),
            );
            self.out.integrate(level, d);
        }

        // frame counter。
        self.frame.block = self.frame.block.saturating_sub((d - 1).min(2) as u8);
        self.frame.cycle += d;
        let mode = usize::from(self.frame.mode5);
        let step = usize::from(self.frame.step);
        if self.frame.cycle == STEP_CYCLES[mode][step] {
            if !self.frame.mode5 && step >= 3 && !self.frame.inhibit_irq {
                self.frame_irq = true;
            }
            let kind = STEP_KIND[mode][step];
            if kind != 0 && self.frame.block == 0 {
                self.clock_frame(kind);
                self.frame.block = 2;
            }
            self.frame.step += 1;
            if self.frame.step == 6 {
                self.frame.step = 0;
                self.frame.cycle = 0;
            }
        }
        if self.frame.write_delay > 0 {
            self.frame.write_delay -= d as u8;
            if self.frame.write_delay == 0 {
                self.frame.mode5 = self.frame.new_mode5;
                self.frame.step = 0;
                self.frame.cycle = 0;
                if self.frame.mode5 && self.frame.block == 0 {
                    self.clock_frame(2);
                    self.frame.block = 2;
                }
            }
        }
        self.frame.block = self.frame.block.saturating_sub(1);

        // 延遲的長度計數器寫入（在 frame counter 之後，這樣同一個 cycle 的時脈先生效）。
        for p in &mut self.pulse {
            p.length.apply_pending();
        }
        self.triangle.length.apply_pending();
        self.noise.length.apply_pending();

        // 各聲道的計時器。
        for p in &mut self.pulse {
            if p.cnt == d {
                p.step_sequencer();
            } else {
                p.cnt -= d;
            }
        }
        if self.triangle.cnt == d {
            self.triangle.step_sequencer();
        } else {
            self.triangle.cnt -= d;
        }
        if self.noise.cnt == d {
            self.noise.step_shift_register();
        } else {
            self.noise.cnt -= d;
        }
        if self.dmc.start_delay > 0 {
            self.dmc.start_delay -= d as u8;
            if self.dmc.start_delay == 0 {
                self.dmc_fetch(cart);
            }
        }
        if self.dmc.cnt == d {
            if self.dmc.step_output() {
                self.dmc_fetch(cart);
            }
        } else {
            self.dmc.cnt -= d;
        }
    }

    /// frame counter 的時脈：`kind` 1 = quarter frame，2 = half frame（含 quarter）。
    fn clock_frame(&mut self, kind: u8) {
        for p in &mut self.pulse {
            p.envelope.tick();
        }
        self.triangle.tick_linear();
        self.noise.envelope.tick();
        if kind == 2 {
            for p in &mut self.pulse {
                p.length.tick();
                p.tick_sweep();
            }
            self.triangle.length.tick();
            self.noise.length.tick();
        }
    }

    /// DMC 向記憶體要下一個 byte（DMA）：CPU 暫停 [`DMC_STALL_CYCLES`] 個 cycle。
    fn dmc_fetch(&mut self, cart: &Cartridge) {
        if !self.dmc.wants_fetch() {
            return;
        }
        let value = cart.read_prg(self.dmc.current_addr);
        self.dmc.finish_fetch(value);
        self.dmc_stall = self.dmc_stall.saturating_add(DMC_STALL_CYCLES);
    }

    // ---- 除錯 ----------------------------------------------------------

    /// 給 Debugger 的 APU 分頁用的唯讀摘要。
    pub fn debug(&self) -> ApuDebug {
        let pulse = |p: &Pulse| PulseDebug {
            enabled: p.enabled,
            duty: p.duty,
            length: p.length.counter,
            halt: p.length.halt,
            constant: p.envelope.constant,
            volume: p.envelope.volume,
            envelope: p.envelope.decay,
            sweep_enabled: p.sweep_enabled,
            sweep_period: p.sweep_period,
            sweep_negate: p.sweep_negate,
            sweep_shift: p.sweep_shift,
            timer_period: p.timer_period,
            seq: p.seq,
            output: p.output(),
        };
        ApuDebug {
            pulse: [pulse(&self.pulse[0]), pulse(&self.pulse[1])],
            triangle: TriangleDebug {
                enabled: self.triangle.enabled,
                control: self.triangle.control,
                linear_reload: self.triangle.linear_reload_value,
                linear_counter: self.triangle.linear_counter,
                length: self.triangle.length.counter,
                timer_period: self.triangle.timer_period,
                seq: self.triangle.seq,
                output: self.triangle.output(),
            },
            noise: NoiseDebug {
                enabled: self.noise.enabled,
                mode: self.noise.mode,
                period_index: self.noise.period_index,
                length: self.noise.length.counter,
                halt: self.noise.length.halt,
                constant: self.noise.envelope.constant,
                volume: self.noise.envelope.volume,
                envelope: self.noise.envelope.decay,
                shift: self.noise.shift,
                output: self.noise.output(),
            },
            dmc: DmcDebug {
                irq_enabled: self.dmc.irq_enabled,
                looping: self.dmc.looping,
                rate_index: self.dmc.rate_index,
                sample_addr: self.dmc.sample_addr,
                sample_length: self.dmc.sample_length,
                current_addr: self.dmc.current_addr,
                bytes_remaining: self.dmc.bytes_remaining,
                output_level: self.dmc.output_level,
                irq_flag: self.dmc.irq_flag,
            },
            frame_mode5: self.frame.mode5,
            frame_inhibit_irq: self.frame.inhibit_irq,
            frame_step: self.frame.step,
            frame_cycle: self.frame.cycle,
            frame_irq: self.frame_irq,
            status: self.peek_status(),
        }
    }
}

#[cfg(test)]
mod tests;
