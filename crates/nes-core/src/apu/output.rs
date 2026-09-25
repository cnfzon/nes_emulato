//! 音訊輸出管線：混音 → 降頻 → 濾波。
//!
//! **這是輸出，不是狀態**：與 `Ppu::frame_buffer` 同地位，不進 save state、不參與
//! `state_hash`。只有這裡可以用浮點數；`load_state` 之後濾波器與尚未取走的取樣都會
//! 重設（設定值——取樣率、開關、聲道遮罩——由 `Nes::load_state` 保留）。
//!
//! 管線：
//! 1. **混音**：NESdev wiki 的非線性混音（查表版本）：
//!    `pulse_out = 95.52 / (8128 / (p1 + p2) + 100)`、
//!    `tnd_out = 163.67 / (24329 / (3t + 2n + d) + 100)`，兩者相加。
//! 2. **降頻**：APU 以 CPU 速率（1.789773 MHz）工作，輸出取樣率由 app 指定。每個輸出取樣是
//!    「那段時間內混音電平的時間平均」（box filter）：`Apu` 每次有事件的時候把「這段時間內電平
//!    不變」的區間餵進來（`integrate`），所以不必逐 cycle 取樣。
//! 3. **濾波**：真實 NES 的類比輸出級——90 Hz 與 442 Hz 兩個一階 high-pass、14 kHz 一階
//!    low-pass，在輸出取樣率上以一階 IIR 實作。

use std::collections::VecDeque;

/// NTSC CPU 時脈（Hz）。
pub const CPU_CLOCK_HZ: f64 = 1_789_772.727_272_7;

/// 預設輸出取樣率。
pub const DEFAULT_SAMPLE_RATE: f64 = 48_000.0;
/// 取樣率允許的範圍（超出就夾到範圍內）。
pub const MIN_SAMPLE_RATE: f64 = 8_000.0;
pub const MAX_SAMPLE_RATE: f64 = 192_000.0;

/// 沒有人取走取樣時最多保留幾個（超過就丟掉最舊的），避免命令列工具長時間跑而無限成長。
const MAX_BUFFERED_SAMPLES: usize = 1 << 17;

/// 五個聲道的遮罩位元（設為 1 = 聽得到）。
pub const CHANNEL_PULSE1: u8 = 1 << 0;
pub const CHANNEL_PULSE2: u8 = 1 << 1;
pub const CHANNEL_TRIANGLE: u8 = 1 << 2;
pub const CHANNEL_NOISE: u8 = 1 << 3;
pub const CHANNEL_DMC: u8 = 1 << 4;
pub const ALL_CHANNELS: u8 = 0x1F;

const fn build_pulse_table() -> [f32; 31] {
    let mut table = [0.0f32; 31];
    let mut n = 1;
    while n < 31 {
        table[n] = (95.52 / (8128.0 / n as f64 + 100.0)) as f32;
        n += 1;
    }
    table
}

const fn build_tnd_table() -> [f32; 203] {
    let mut table = [0.0f32; 203];
    let mut n = 1;
    while n < 203 {
        table[n] = (163.67 / (24329.0 / n as f64 + 100.0)) as f32;
        n += 1;
    }
    table
}

static PULSE_TABLE: [f32; 31] = build_pulse_table();
/// 索引 = `3 * triangle + 2 * noise + dmc`（最大 `3*15 + 2*15 + 127 = 202`）。
static TND_TABLE: [f32; 203] = build_tnd_table();

/// 一階 high-pass：`y[n] = a * (y[n-1] + x[n] - x[n-1])`。
#[derive(Debug, Clone, Copy, Default)]
struct HighPass {
    alpha: f64,
    prev_in: f64,
    prev_out: f64,
}

impl HighPass {
    fn new(cutoff_hz: f64, rate: f64) -> Self {
        let rc = 1.0 / (2.0 * std::f64::consts::PI * cutoff_hz);
        let dt = 1.0 / rate;
        Self {
            alpha: rc / (rc + dt),
            prev_in: 0.0,
            prev_out: 0.0,
        }
    }

    fn process(&mut self, x: f64) -> f64 {
        let y = self.alpha * (self.prev_out + x - self.prev_in);
        self.prev_in = x;
        self.prev_out = y;
        y
    }
}

/// 一階 low-pass：`y[n] = y[n-1] + a * (x[n] - y[n-1])`。
#[derive(Debug, Clone, Copy, Default)]
struct LowPass {
    alpha: f64,
    prev_out: f64,
}

impl LowPass {
    fn new(cutoff_hz: f64, rate: f64) -> Self {
        let rc = 1.0 / (2.0 * std::f64::consts::PI * cutoff_hz);
        let dt = 1.0 / rate;
        Self {
            alpha: dt / (rc + dt),
            prev_out: 0.0,
        }
    }

    fn process(&mut self, x: f64) -> f64 {
        self.prev_out += self.alpha * (x - self.prev_out);
        self.prev_out
    }
}

/// 輸出管線的全部狀態（不進 save state）。
#[derive(Debug, Clone)]
pub struct AudioOut {
    /// 輸出開關；關閉時完全不混音、不產生取樣。
    pub(super) enabled: bool,
    /// 聽得到的聲道（`CHANNEL_*` 遮罩）。
    pub(super) channel_mask: u8,
    sample_rate: f64,
    /// 一個輸出取樣涵蓋多少個 CPU cycle。
    cycles_per_sample: f64,
    /// 距離目前這個輸出取樣結束還有多少 CPU cycle。
    cycles_left: f64,
    /// 目前這個輸出取樣已累積的「電平 × cycle」。
    accum: f64,
    hp90: HighPass,
    hp442: HighPass,
    lp14k: LowPass,
    samples: VecDeque<f32>,
}

impl Default for AudioOut {
    fn default() -> Self {
        let mut out = Self {
            enabled: true,
            channel_mask: ALL_CHANNELS,
            sample_rate: DEFAULT_SAMPLE_RATE,
            cycles_per_sample: 0.0,
            cycles_left: 0.0,
            accum: 0.0,
            hp90: HighPass::default(),
            hp442: HighPass::default(),
            lp14k: LowPass::default(),
            samples: VecDeque::new(),
        };
        out.set_sample_rate(DEFAULT_SAMPLE_RATE);
        out
    }
}

impl AudioOut {
    /// 更改輸出取樣率（夾在 [`MIN_SAMPLE_RATE`, `MAX_SAMPLE_RATE`]）。動態速率控制會頻繁呼叫，
    /// 所以只重算係數，不清除濾波器狀態與已產生的取樣。
    pub fn set_sample_rate(&mut self, hz: f64) {
        let hz = if hz.is_finite() {
            hz.clamp(MIN_SAMPLE_RATE, MAX_SAMPLE_RATE)
        } else {
            DEFAULT_SAMPLE_RATE
        };
        self.sample_rate = hz;
        self.cycles_per_sample = CPU_CLOCK_HZ / hz;
        if self.cycles_left <= 0.0 || self.cycles_left > self.cycles_per_sample {
            self.cycles_left = self.cycles_per_sample;
        }
        self.hp90 = HighPass {
            prev_in: self.hp90.prev_in,
            prev_out: self.hp90.prev_out,
            ..HighPass::new(90.0, hz)
        };
        self.hp442 = HighPass {
            prev_in: self.hp442.prev_in,
            prev_out: self.hp442.prev_out,
            ..HighPass::new(442.0, hz)
        };
        self.lp14k = LowPass {
            prev_out: self.lp14k.prev_out,
            ..LowPass::new(14_000.0, hz)
        };
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// 清掉濾波器與尚未取走的取樣（`load_state`、`reset` 之後用）。設定值不動。
    pub fn reset_signal(&mut self) {
        self.accum = 0.0;
        self.cycles_left = self.cycles_per_sample;
        self.hp90.prev_in = 0.0;
        self.hp90.prev_out = 0.0;
        self.hp442.prev_in = 0.0;
        self.hp442.prev_out = 0.0;
        self.lp14k.prev_out = 0.0;
        self.samples.clear();
    }

    /// 把「設定值」（不含訊號）從舊的實例帶到新的實例；`load_state` 用。
    pub fn adopt_settings(&mut self, other: &AudioOut) {
        self.enabled = other.enabled;
        self.channel_mask = other.channel_mask;
        self.set_sample_rate(other.sample_rate);
        self.reset_signal();
    }

    /// 混音電平：五個聲道的原始輸出（依遮罩）→ 非線性混音，0.0–約 1.0。
    pub(super) fn mix(&self, pulse1: u8, pulse2: u8, triangle: u8, noise: u8, dmc: u8) -> f32 {
        let m = self.channel_mask;
        let pick = |bit: u8, v: u8| if m & bit != 0 { v as usize } else { 0 };
        let pulse = pick(CHANNEL_PULSE1, pulse1) + pick(CHANNEL_PULSE2, pulse2);
        let tnd = 3 * pick(CHANNEL_TRIANGLE, triangle)
            + 2 * pick(CHANNEL_NOISE, noise)
            + pick(CHANNEL_DMC, dmc);
        PULSE_TABLE[pulse.min(30)] + TND_TABLE[tnd.min(202)]
    }

    /// 電平 `level` 在接下來 `cycles` 個 CPU cycle 內保持不變：累積進目前的輸出取樣，
    /// 跨過取樣邊界時產出取樣。
    pub(super) fn integrate(&mut self, level: f32, cycles: u32) {
        let level = level as f64;
        let mut remaining = cycles as f64;
        while remaining >= self.cycles_left {
            self.accum += level * self.cycles_left;
            remaining -= self.cycles_left;
            let averaged = self.accum / self.cycles_per_sample;
            self.emit(averaged);
            self.accum = 0.0;
            self.cycles_left = self.cycles_per_sample;
        }
        self.accum += level * remaining;
        self.cycles_left -= remaining;
    }

    fn emit(&mut self, x: f64) {
        let y = self.hp90.process(x);
        let y = self.hp442.process(y);
        let y = self.lp14k.process(y);
        if self.samples.len() >= MAX_BUFFERED_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(y as f32);
    }

    /// 把累積的取樣附加到 `out` 並清空內部緩衝區。
    pub fn take_samples(&mut self, out: &mut Vec<f32>) {
        out.extend(self.samples.drain(..));
    }

    /// 目前累積、尚未取走的取樣數（測試與 app 的診斷用）。
    pub fn buffered(&self) -> usize {
        self.samples.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixer_matches_the_nesdev_formulas() {
        let out = AudioOut::default();
        // 全靜音。
        assert_eq!(out.mix(0, 0, 0, 0, 0), 0.0);
        // 兩個 pulse 都是 15：95.52 / (8128/30 + 100)。
        let expected = 95.52 / (8128.0 / 30.0 + 100.0);
        assert!((out.mix(15, 15, 0, 0, 0) as f64 - expected).abs() < 1e-6);
        // tnd：三角 15、雜訊 15、DMC 127。
        let n = 3.0 * 15.0 + 2.0 * 15.0 + 127.0;
        let expected = 163.67 / (24329.0 / n + 100.0);
        assert!((out.mix(0, 0, 15, 15, 127) as f64 - expected).abs() < 1e-6);
    }

    #[test]
    fn channel_mask_silences_individual_channels() {
        let mut out = AudioOut::default();
        let full = out.mix(15, 15, 15, 15, 127);
        out.channel_mask = ALL_CHANNELS & !CHANNEL_PULSE1;
        assert_eq!(out.mix(15, 0, 0, 0, 0), 0.0);
        assert!(out.mix(15, 15, 15, 15, 127) < full);
    }

    #[test]
    fn integrate_produces_the_expected_number_of_samples() {
        let mut out = AudioOut::default();
        // 1 秒的 CPU cycle → 約 48000 個取樣。
        let mut left = CPU_CLOCK_HZ as u64;
        while left > 0 {
            let step = left.min(29_780) as u32;
            out.integrate(0.5, step);
            left -= step as u64;
        }
        let n = out.buffered() as i64;
        assert!((n - 48_000).abs() <= 1, "取樣數 {n}");
    }

    #[test]
    fn dc_is_removed_by_the_high_pass_filters() {
        let mut out = AudioOut::default();
        // 100 × 29780 cycle ≈ 1.7 秒，低於緩衝上限，開頭的瞬態才不會被丟掉。
        for _ in 0..100 {
            out.integrate(0.5, 29_780);
        }
        let mut samples = Vec::new();
        out.take_samples(&mut samples);
        let tail = &samples[samples.len() - 100..];
        assert!(tail.iter().all(|s| s.abs() < 1e-3), "DC 應被濾掉");
        // 一開始（電平剛跳上去）有明顯的瞬態。
        assert!(samples[..10].iter().any(|s| s.abs() > 0.05));
    }

    #[test]
    fn sample_rate_is_clamped_and_nan_is_rejected() {
        let mut out = AudioOut::default();
        out.set_sample_rate(1.0);
        assert_eq!(out.sample_rate(), MIN_SAMPLE_RATE);
        out.set_sample_rate(1e9);
        assert_eq!(out.sample_rate(), MAX_SAMPLE_RATE);
        out.set_sample_rate(f64::NAN);
        assert_eq!(out.sample_rate(), DEFAULT_SAMPLE_RATE);
    }

    #[test]
    fn buffer_is_bounded_when_nobody_drains_it() {
        let mut out = AudioOut::default();
        for _ in 0..1000 {
            out.integrate(0.3, 29_780);
        }
        assert!(out.buffered() <= MAX_BUFFERED_SAMPLES);
    }
}
