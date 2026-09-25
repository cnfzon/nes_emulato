//! 音訊輸出：emu 執行緒產生取樣 → 無鎖環形緩衝區 → cpal 的 callback。
//!
//! 這是課程主題「作業系統與應用程式的關係」的報告素材之一：兩條執行緒（emu 執行緒與
//! 系統音訊執行緒）靠共享的原子變數與一條 lock-free 的環形緩衝區溝通。
//!
//! # 設計
//!
//! - **模擬的節拍仍由 emu 執行緒掌控**（見 `emu.rs`），音訊裝置只是消費者，不驅動模擬。
//! - **環形緩衝區用 [`rtrb`](https://crates.io/crates/rtrb)**（單一生產者、單一消費者、wait-free）：
//!   `Producer` 由 emu 執行緒持有（[`AudioProducer`]）、`Consumer` 由 cpal callback 持有
//!   （[`CallbackState`]），各自獨占一端，型別系統就保證了「只有一個生產者、一個消費者」。
//!   **callback 內沒有配置記憶體、沒有鎖**：只有 `pop`、原子讀寫與寫入裝置給的緩衝區。
//!   為什麼不自製：無鎖資料結構的正確性很難用測試證明，而本階段自製版本已經出過一次競態
//!   （見 `docs/architecture.md` §17.9）。
//! - **動態速率控制**（[`RateController`]）：每幀依緩衝區填充程度，在 ±0.5% 內微調
//!   `Nes::set_audio_sample_rate`。緩衝區比目標多就少產生一點取樣（降低取樣率），少就多產生
//!   一點。比例控制器（緩衝區偏離目標 25% 就用滿 ±0.5%，時間常數約 2.5 秒）；填充量再經
//!   指數平滑，避免「每幀一次的鋸齒」造成音高抖動。吸收的是「emu 計時器與音訊裝置時脈」之間
//!   最多 ±0.5% 的長期漂移，不是短暫卡頓。
//! - **目標延遲約 50 ms**（[`TARGET_LATENCY_MS`]）：callback 要等緩衝區累積到目標量才開始播放
//!   （prebuffer），跑乾（underrun）就靜音並重新累積，而不是一直斷斷續續。上限是目標的 3 倍
//!   （150 ms）：emu 執行緒被卡住又一次補幀時多出來的取樣直接丟掉，延遲不會永久累積。
//! - **暫停靜音**：暫停時 callback 輸出靜音並丟掉緩衝區裡殘留的取樣；繼續時重新累積。
//! - **沒有音訊裝置**：`AudioOutput::start` 回傳「沒有裝置」的狀態（[`AudioShared::is_active`]
//!   為 `false`），程式照常執行，emu 執行緒把取樣直接丟掉，UI 顯示提示。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use rtrb::{Consumer, Producer, RingBuffer};

/// 目標延遲（毫秒）。
pub const TARGET_LATENCY_MS: f64 = 50.0;
/// 動態速率控制的調整幅度上限（±0.5%）。
pub const MAX_RATE_ADJUST: f64 = 0.005;
/// 填充量的指數平滑係數（每幀一次）。
const FILL_SMOOTHING: f64 = 0.05;
/// 比例控制器的增益：緩衝區偏離目標 `1 / RATE_GAIN`（25%）時調整量達到上限。增益太低的話，
/// 漂移接近 0.5% 時穩態誤差會大到緩衝區跑乾（單元測試 `dynamic_rate_control_...` 驗證）。
const RATE_GAIN: f64 = 4.0;
/// 緩衝區上限相對於目標量的倍數（超過就丟掉新的取樣）。
const MAX_FILL_FACTOR: usize = 3;
/// 環形緩衝區的容量相對於目標量的倍數（比上限再寬鬆一點）。
const CAPACITY_FACTOR: usize = 5;
/// 開始播放（或 underrun 後重新開始）時的淡入長度（取樣數），避免爆音。
const FADE_IN_SAMPLES: u32 = 512;
/// 音量變化的單極平滑係數（每個取樣），避免拖動滑桿時的拉鍊雜音。
const GAIN_SMOOTHING: f32 = 0.002;

// ---- 共享狀態與生產者 ------------------------------------------------------------

/// emu 執行緒、UI 執行緒與音訊 callback 共享的狀態（全部是原子變數；取樣本身走 rtrb）。
pub struct AudioShared {
    /// 裝置的取樣率（Hz）；0 = 沒有音訊裝置。
    device_rate: AtomicU32,
    /// 環形緩衝區的容量（取樣數）。
    capacity: usize,
    /// 暫停中：callback 輸出靜音並丟掉殘留取樣。
    paused: AtomicBool,
    /// 主音量（含靜音，`f32` 位元）。
    gain: AtomicU32,
    /// 累計 underrun 次數（播放中緩衝區跑乾）。
    underruns: AtomicU64,
    /// 累計被丟掉的取樣數（緩衝區超過上限）。
    dropped: AtomicU64,
    /// 目前給核心的輸出取樣率（動態速率控制調整後，`f64` 位元），UI 顯示用。
    current_rate: AtomicU64,
    /// 清空緩衝區的請求次數：emu 執行緒遞增，callback 發現與自己記的不同就清空。
    flush_generation: AtomicU32,
    /// 緩衝量（取樣數）的近似值：生產者與消費者各自在操作之後更新，給 UI 顯示用。
    fill: AtomicUsize,
}

impl AudioShared {
    fn new(device_rate: u32, capacity: usize) -> Self {
        Self {
            device_rate: AtomicU32::new(device_rate),
            capacity,
            paused: AtomicBool::new(false),
            gain: AtomicU32::new(1.0f32.to_bits()),
            underruns: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            current_rate: AtomicU64::new(f64::from(device_rate).to_bits()),
            flush_generation: AtomicU32::new(0),
            fill: AtomicUsize::new(0),
        }
    }

    pub fn is_active(&self) -> bool {
        self.device_rate() > 0
    }

    pub fn device_rate(&self) -> u32 {
        self.device_rate.load(Ordering::Relaxed)
    }

    /// 目標填充量（取樣數）。
    pub fn target_fill(&self) -> usize {
        target_fill(self.device_rate())
    }

    /// 緩衝量上限（取樣數）。
    fn max_fill(&self) -> usize {
        self.target_fill() * MAX_FILL_FACTOR
    }

    /// 目前緩衝的取樣數（近似值，UI 顯示用）。
    pub fn fill(&self) -> usize {
        self.fill.load(Ordering::Relaxed)
    }

    /// 目前緩衝的長度（毫秒）。
    pub fn fill_ms(&self) -> f64 {
        match self.device_rate() {
            0 => 0.0,
            rate => self.fill() as f64 * 1000.0 / f64::from(rate),
        }
    }

    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn current_rate(&self) -> f64 {
        f64::from_bits(self.current_rate.load(Ordering::Relaxed))
    }

    /// 設定主音量（0.0–1.0）與靜音。
    pub fn set_volume(&self, volume: f32, muted: bool) {
        let gain = if muted { 0.0 } else { volume.clamp(0.0, 1.0) };
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }
}

/// 環形緩衝區的生產者端，由 emu 執行緒獨占（`rtrb::Producer` 不能複製、也不能共享）。
pub struct AudioProducer {
    shared: Arc<AudioShared>,
    producer: Producer<f32>,
}

impl AudioProducer {
    pub fn shared(&self) -> &Arc<AudioShared> {
        &self.shared
    }

    pub fn is_active(&self) -> bool {
        self.shared.is_active()
    }

    pub fn device_rate(&self) -> u32 {
        self.shared.device_rate()
    }

    pub fn target_fill(&self) -> usize {
        self.shared.target_fill()
    }

    /// 目前緩衝的取樣數（生產者的視角：容量減去空位；消費者剛消費的還沒看到，略偏高）。
    pub fn fill(&self) -> usize {
        self.shared.capacity - self.producer.slots()
    }

    pub fn set_current_rate(&self, hz: f64) {
        self.shared
            .current_rate
            .store(hz.to_bits(), Ordering::Relaxed);
    }

    /// emu 執行緒：暫停／繼續。暫停時 callback 輸出靜音並丟掉緩衝區裡殘留的取樣。
    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Release);
    }

    /// emu 執行緒：送出取樣。讓緩衝量最多到 [`AudioShared::max_fill`]，超過的部分丟掉
    /// （計入 `dropped`）。
    pub fn push_samples(&mut self, samples: &[f32]) {
        if !self.is_active() || samples.is_empty() {
            return;
        }
        let room = self.shared.max_fill().saturating_sub(self.fill());
        let n = samples.len().min(room);
        let (pushed, _) = self.producer.push_partial_slice(&samples[..n]);
        let accepted = pushed.len();
        if accepted < samples.len() {
            self.shared
                .dropped
                .fetch_add((samples.len() - accepted) as u64, Ordering::Relaxed);
        }
        self.shared.fill.store(self.fill(), Ordering::Relaxed);
    }

    /// 載入 ROM 等大幅改變聲音內容的時候，丟掉緩衝區裡舊的取樣。
    ///
    /// 只有消費者能取走資料，所以這只是「請求」：callback 在下一次執行時清空並重新累積。
    pub fn flush(&self) {
        self.shared.flush_generation.fetch_add(1, Ordering::AcqRel);
    }
}

/// 建立一組音訊通道：共享狀態、生產者（給 emu 執行緒）、callback 狀態（含消費者，給 cpal callback）。
pub fn audio_channel(device_rate: u32) -> (Arc<AudioShared>, AudioProducer, CallbackState) {
    let capacity = target_fill(device_rate) * CAPACITY_FACTOR;
    let (producer, consumer) = RingBuffer::new(capacity.max(1));
    let shared = Arc::new(AudioShared::new(device_rate, capacity.max(1)));
    (
        Arc::clone(&shared),
        AudioProducer {
            shared: Arc::clone(&shared),
            producer,
        },
        CallbackState::new(consumer),
    )
}

/// 沒有音訊裝置：生產者什麼都不做（`push_samples` 直接返回），emu 執行緒把取樣丟掉。
pub fn disabled_audio() -> AudioProducer {
    audio_channel(0).1
}

/// 目標填充量（取樣數）。
fn target_fill(device_rate: u32) -> usize {
    (f64::from(device_rate) * TARGET_LATENCY_MS / 1000.0).round() as usize
}

// ---- 動態速率控制 ---------------------------------------------------------------

/// 動態速率控制：每幀呼叫一次 [`RateController::update`]，得到要交給核心的輸出取樣率。
#[derive(Debug, Clone)]
pub struct RateController {
    /// 平滑後的填充量（取樣數）。
    smoothed: f64,
}

impl RateController {
    pub fn new(target: usize) -> Self {
        Self {
            smoothed: target as f64,
        }
    }

    /// 重新開始（載入 ROM、暫停後繼續）：平滑值回到目標。
    pub fn reset(&mut self, target: usize) {
        self.smoothed = target as f64;
    }

    /// `fill`：目前緩衝區的取樣數；`target`：目標填充量；`device_rate`：裝置取樣率（Hz）。
    /// 回傳的取樣率落在 `device_rate × (1 ± 0.5%)`：緩衝區偏滿就降低（少產生取樣），偏空就
    /// 提高。
    pub fn update(&mut self, fill: usize, target: usize, device_rate: f64) -> f64 {
        if target == 0 {
            return device_rate;
        }
        self.smoothed += (fill as f64 - self.smoothed) * FILL_SMOOTHING;
        let error = (self.smoothed - target as f64) / target as f64;
        let adjust = (error * RATE_GAIN * MAX_RATE_ADJUST).clamp(-MAX_RATE_ADJUST, MAX_RATE_ADJUST);
        device_rate * (1.0 - adjust)
    }
}

// ---- 音訊 callback ------------------------------------------------------------

/// callback 自己的狀態（只由 callback 使用，不共享）：環形緩衝區的消費者端與播放狀態。
pub struct CallbackState {
    consumer: Consumer<f32>,
    /// 已經累積夠了、正在播放。
    playing: bool,
    /// 目前的音量（向目標音量平滑靠近）。
    gain: f32,
    /// 淡入已經進行到第幾個取樣。
    fade: u32,
    /// 已經處理到第幾次 flush 請求。
    flush_seen: u32,
}

impl CallbackState {
    fn new(consumer: Consumer<f32>) -> Self {
        Self {
            consumer,
            playing: false,
            gain: 1.0,
            fade: 0,
            flush_seen: 0,
        }
    }

    /// 丟掉緩衝區裡全部的取樣（只有消費者能做）。
    fn clear(&mut self) {
        let n = self.consumer.slots();
        if let Ok(chunk) = self.consumer.read_chunk(n) {
            chunk.commit_all();
        }
    }
}

/// 把緩衝區的取樣寫進裝置給的輸出緩衝區（單聲道複製到所有聲道）。
///
/// **在音訊執行緒上執行，不得配置記憶體、不得上鎖等待**：只有 rtrb 的 `pop`、原子讀寫與
/// 寫入 `data`。
pub fn fill_output<T: SizedSample + FromSample<f32>>(
    data: &mut [T],
    channels: usize,
    shared: &AudioShared,
    state: &mut CallbackState,
) {
    let channels = channels.max(1);
    let silence = T::from_sample(0.0f32);

    let generation = shared.flush_generation.load(Ordering::Acquire);
    if generation != state.flush_seen {
        state.flush_seen = generation;
        state.clear();
        state.playing = false;
    }

    if shared.paused.load(Ordering::Acquire) {
        // 暫停：完全靜音，殘留的取樣也不能播出來。
        state.clear();
        state.playing = false;
        shared.fill.store(0, Ordering::Relaxed);
        data.fill(silence);
        return;
    }

    let frames = data.len() / channels;
    if !state.playing {
        if state.consumer.slots() < shared.target_fill().max(1) {
            shared.fill.store(state.consumer.slots(), Ordering::Relaxed);
            data.fill(silence);
            return;
        }
        state.playing = true;
        state.fade = 0;
    }

    let target_gain = f32::from_bits(shared.gain.load(Ordering::Relaxed));
    let mut ran_dry = false;

    for frame in data.chunks_exact_mut(channels) {
        let sample = match state.consumer.pop() {
            Ok(sample) => sample,
            Err(_) => {
                ran_dry = true;
                0.0
            }
        };
        state.gain += (target_gain - state.gain) * GAIN_SMOOTHING;
        let fade = if state.fade < FADE_IN_SAMPLES {
            state.fade += 1;
            state.fade as f32 / FADE_IN_SAMPLES as f32
        } else {
            1.0
        };
        let out = T::from_sample(sample * state.gain * fade);
        frame.fill(out);
    }
    // 不足整數個 frame 的尾巴（不會發生，保險）。
    let rest = data.len() - frames * channels;
    if rest > 0 {
        let start = data.len() - rest;
        data[start..].fill(silence);
    }

    shared.fill.store(state.consumer.slots(), Ordering::Relaxed);
    if ran_dry {
        // 播放中緩衝區跑乾：靜音並重新累積。
        shared.underruns.fetch_add(1, Ordering::Relaxed);
        state.playing = false;
    }
}

// ---- 裝置 ---------------------------------------------------------------------

/// 音訊輸出：持有 cpal 的 `Stream`（必須留在建立它的執行緒，即 UI 執行緒）與共享狀態。
pub struct AudioOutput {
    pub shared: Arc<AudioShared>,
    /// 生產者端：交給 emu 執行緒（[`AudioOutput::take_producer`]）。
    producer: Option<AudioProducer>,
    /// 裝置名稱（除錯／UI 顯示用）；沒有裝置時為 `None`。
    pub device_name: Option<String>,
    /// 給使用者看的狀態說明（沒有裝置、開啟失敗時的提示）。
    pub notice: Option<String>,
    _stream: Option<cpal::Stream>,
}

impl AudioOutput {
    /// 開啟預設輸出裝置。找不到裝置或開啟失敗時**不 panic**：回傳沒有裝置的狀態與提示。
    pub fn start() -> Self {
        match Self::try_start() {
            Ok(output) => output,
            Err(message) => {
                log::warn!("音訊無法使用：{message}");
                let producer = disabled_audio();
                Self {
                    shared: Arc::clone(producer.shared()),
                    producer: Some(producer),
                    device_name: None,
                    notice: Some(format!("音訊無法使用（{message}），將無聲執行")),
                    _stream: None,
                }
            }
        }
    }

    /// 取走生產者端（只能取一次）。
    pub fn take_producer(&mut self) -> Option<AudioProducer> {
        self.producer.take()
    }

    fn try_start() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "找不到預設的音訊輸出裝置".to_string())?;
        let supported = device
            .default_output_config()
            .map_err(|e| format!("查詢裝置設定失敗：{e}"))?;
        let format = supported.sample_format();
        let config = supported.config();
        let channels = usize::from(config.channels);
        let (shared, producer, callback) = audio_channel(config.sample_rate);

        let stream = match format {
            cpal::SampleFormat::F32 => {
                build_stream::<f32>(&device, config, channels, &shared, callback)
            }
            cpal::SampleFormat::I16 => {
                build_stream::<i16>(&device, config, channels, &shared, callback)
            }
            cpal::SampleFormat::U16 => {
                build_stream::<u16>(&device, config, channels, &shared, callback)
            }
            other => return Err(format!("不支援的取樣格式 {other}")),
        }
        .map_err(|e| format!("開啟音訊串流失敗：{e}"))?;
        stream
            .play()
            .map_err(|e| format!("啟動音訊串流失敗：{e}"))?;

        let device_name = device.description().ok().map(|d| d.to_string());
        Ok(Self {
            shared,
            producer: Some(producer),
            device_name,
            notice: None,
            _stream: Some(stream),
        })
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    shared: &Arc<AudioShared>,
    mut state: CallbackState,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let shared = Arc::clone(shared);
    device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            fill_output(data, channels, &shared, &mut state);
        },
        |err| log::error!("音訊串流錯誤：{err}"),
        None,
    )
}

/// 診斷：在真實的音訊裝置上跑 `seconds` 秒，驗證串流能開啟、callback 有在消費取樣、
/// 動態速率控制在真實時脈下能讓緩衝區穩定。音量設為 0，**不會發出聲音**。
/// 以 `nes-app --audio-selftest` 執行。回傳報告文字。
pub fn selftest(seconds: f64) -> String {
    use std::time::{Duration, Instant};

    let mut output = AudioOutput::start();
    let shared = Arc::clone(&output.shared);
    let mut report = String::new();
    if let Some(notice) = &output.notice {
        return format!("{notice}\n");
    }
    let Some(mut producer) = output.take_producer() else {
        return "內部錯誤：沒有生產者\n".to_string();
    };
    report.push_str(&format!(
        "裝置：{}（{} Hz）\n",
        output.device_name.as_deref().unwrap_or("（未知）"),
        shared.device_rate()
    ));
    shared.set_volume(0.0, false);

    let device = f64::from(shared.device_rate());
    let target = shared.target_fill();
    let mut controller = RateController::new(target);
    let mut rate = device;
    let frame = Duration::from_secs_f64(1.0 / 60.0988);
    let start = Instant::now();
    let mut next = start;
    let mut carry = 0.0f64;
    let mut phase = 0.0f64;
    let (mut min_fill, mut max_fill, mut sum, mut count) = (usize::MAX, 0usize, 0.0f64, 0u64);
    let mut chunk: Vec<f32> = Vec::new();
    while start.elapsed().as_secs_f64() < seconds {
        // 每幀產生 rate / 60.0988 個取樣的 440 Hz 正弦波（音量 0，只是資料）。
        carry += rate * frame.as_secs_f64();
        let n = carry as usize;
        carry -= n as f64;
        chunk.clear();
        for _ in 0..n {
            chunk.push((phase * std::f64::consts::TAU).sin() as f32 * 0.3);
            phase = (phase + 440.0 / device).fract();
        }
        producer.push_samples(&chunk);
        rate = controller.update(producer.fill(), target, device);
        producer.set_current_rate(rate);
        if start.elapsed().as_secs_f64() > 2.0 {
            let fill = producer.fill();
            min_fill = min_fill.min(fill);
            max_fill = max_fill.max(fill);
            sum += fill as f64;
            count += 1;
        }
        next += frame;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
    }
    let ms = |samples: f64| samples * 1000.0 / device;
    report.push_str(&format!(
        "跑了 {seconds:.0} 秒（前 2 秒不計）：緩衝 最小 {:.1} ms／平均 {:.1} ms／最大 {:.1} ms（目標 {:.0} ms）\n",
        ms(min_fill as f64),
        ms(sum / count.max(1) as f64),
        ms(max_fill as f64),
        TARGET_LATENCY_MS
    ));
    report.push_str(&format!(
        "underrun {} 次；丟棄取樣 {}；最終輸出取樣率 {:.2} Hz（調整 {:+.3}%）\n",
        shared.underruns(),
        shared.dropped(),
        rate,
        (rate / device - 1.0) * 100.0
    ));
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn setup() -> (Arc<AudioShared>, AudioProducer, CallbackState) {
        audio_channel(RATE)
    }

    fn callback(shared: &AudioShared, state: &mut CallbackState, frames: usize) -> Vec<f32> {
        let mut data = vec![0.0f32; frames * 2];
        fill_output(&mut data, 2, shared, state);
        data
    }

    /// 讓緩衝區累積到剛好夠開始播放。
    fn prime(producer: &mut AudioProducer) {
        let n = producer.target_fill();
        producer.push_samples(&vec![0.5; n]);
    }

    #[test]
    fn callback_waits_for_the_prebuffer_and_then_plays() {
        let (shared, mut producer, mut state) = setup();
        producer.push_samples(&vec![0.5; producer.target_fill() / 2]);
        assert!(callback(&shared, &mut state, 256).iter().all(|s| *s == 0.0));
        assert_eq!(shared.underruns(), 0, "還沒開始播放就不算 underrun");

        prime(&mut producer);
        let out = callback(&shared, &mut state, 256);
        assert!(out.iter().any(|s| *s > 0.0), "累積夠了就開始播放");
        assert_eq!(out[0], out[1], "兩個聲道相同");
        // 淡入：第一個取樣接近 0，之後逐漸變大。
        assert!(out[0] < 0.01 && out[400] > out[10]);
    }

    #[test]
    fn callback_counts_an_underrun_once_and_reprimes() {
        let (shared, mut producer, mut state) = setup();
        prime(&mut producer);
        // 目標量 2400 個取樣；每次 1000 frame：第 3 次會跑乾。
        for _ in 0..2 {
            callback(&shared, &mut state, 1000);
        }
        assert_eq!(shared.underruns(), 0);
        callback(&shared, &mut state, 1000);
        assert_eq!(shared.underruns(), 1);
        // 之後一直沒有新資料：靜音，不會重複計數。
        for _ in 0..5 {
            assert!(
                callback(&shared, &mut state, 1000)
                    .iter()
                    .all(|s| *s == 0.0)
            );
        }
        assert_eq!(shared.underruns(), 1);
    }

    #[test]
    fn pausing_silences_the_output_and_discards_leftover_samples() {
        let (shared, mut producer, mut state) = setup();
        prime(&mut producer);
        callback(&shared, &mut state, 64);
        assert!(state.consumer.slots() > 0);

        producer.set_paused(true);
        assert!(callback(&shared, &mut state, 512).iter().all(|s| *s == 0.0));
        assert_eq!(state.consumer.slots(), 0, "殘留取樣被丟掉");
        assert_eq!(shared.fill(), 0);

        // 繼續之後：要重新累積到目標量才會有聲音，而且不會播出暫停前的舊取樣。
        producer.set_paused(false);
        producer.push_samples(&vec![0.25; 100]);
        assert!(callback(&shared, &mut state, 512).iter().all(|s| *s == 0.0));
        assert_eq!(shared.underruns(), 0, "暫停不算 underrun");
    }

    #[test]
    fn flush_is_executed_by_the_callback_and_forces_a_reprime() {
        let (shared, mut producer, mut state) = setup();
        prime(&mut producer);
        callback(&shared, &mut state, 64);
        assert!(state.consumer.slots() > 0);

        producer.flush();
        assert!(callback(&shared, &mut state, 64).iter().all(|s| *s == 0.0));
        assert_eq!(state.consumer.slots(), 0);
        assert_eq!(shared.underruns(), 0);
        // 之後照常：累積夠了又會播放。
        prime(&mut producer);
        assert!(callback(&shared, &mut state, 512).iter().any(|s| *s > 0.0));
    }

    #[test]
    fn volume_and_mute_scale_the_output_smoothly() {
        let (shared, mut producer, mut state) = setup();
        shared.set_volume(0.5, false);
        producer.push_samples(&vec![1.0; producer.target_fill() * 2]);
        // 音量是平滑地靠近目標的：前幾個 callback 還在過渡，取最後一個的峰值。
        let mut peak = 0.0f32;
        for _ in 0..8 {
            peak = callback(&shared, &mut state, 256)
                .into_iter()
                .fold(0.0f32, f32::max);
        }
        assert!(
            peak > 0.45 && peak < 0.55,
            "音量 0.5 → 峰值約 0.5，實際 {peak}"
        );

        shared.set_volume(0.5, true);
        for _ in 0..30 {
            callback(&shared, &mut state, 256);
        }
        assert!(
            callback(&shared, &mut state, 256)
                .iter()
                .all(|s| s.abs() < 1e-3)
        );
    }

    #[test]
    fn integer_sample_formats_convert_without_panicking() {
        let (shared, mut producer, mut state) = setup();
        prime(&mut producer);
        let mut i16_data = vec![0i16; 512];
        fill_output(&mut i16_data, 2, &shared, &mut state);
        let mut u16_data = vec![0u16; 512];
        fill_output(&mut u16_data, 1, &shared, &mut state);
    }

    #[test]
    fn producer_drops_samples_beyond_the_latency_cap() {
        let (shared, mut producer, _state) = setup();
        let cap = producer.target_fill() * MAX_FILL_FACTOR;
        producer.push_samples(&vec![0.1; cap + 500]);
        assert_eq!(producer.fill(), cap);
        assert_eq!(shared.fill(), cap);
        assert_eq!(shared.dropped(), 500);
    }

    #[test]
    fn disabled_output_ignores_everything() {
        let mut producer = disabled_audio();
        assert!(!producer.is_active());
        producer.push_samples(&[0.5; 100]);
        assert_eq!(producer.fill(), 0);
        assert_eq!(producer.shared().dropped(), 0);
        assert_eq!(producer.shared().fill_ms(), 0.0);
    }

    /// 兩條執行緒：emu 端經 `AudioProducer` 連續送遞增的取樣，消費端從 `CallbackState` 的
    /// consumer 依序取走；取到的必須嚴格連續遞增（不遺失、不重複、不亂序）。正確性由 rtrb
    /// 保證，這裡驗證我們的用法（緩衝量上限、`fill()` 的計算）沒有破壞它。不依賴時間：
    /// 兩邊都只在「沒空間／沒資料」時讓出時間片重試。
    #[test]
    fn producer_and_consumer_agree_across_threads() {
        let (_shared, mut producer, mut state) = setup();
        let total = 200_000usize;
        let max = producer.target_fill() * MAX_FILL_FACTOR;
        let sender = std::thread::spawn(move || {
            let mut next = 0usize;
            while next < total {
                let n = 97.min(total - next);
                if producer.fill() + n > max {
                    std::thread::yield_now();
                    continue;
                }
                let chunk: Vec<f32> = (next..next + n).map(|v| v as f32).collect();
                producer.push_samples(&chunk);
                next += n;
            }
            producer.shared().dropped()
        });
        let mut expected = 0usize;
        while expected < total {
            match state.consumer.pop() {
                Ok(v) => {
                    assert_eq!(v, expected as f32);
                    expected += 1;
                }
                Err(_) => std::thread::yield_now(),
            }
        }
        assert_eq!(sender.join().unwrap(), 0, "沒有取樣被丟掉");
    }

    /// 反覆以不規則的批次大小送入／取出，讓讀寫位置多次繞過環形緩衝區的尾端：順序必須始終連續。
    #[test]
    fn samples_stay_in_order_when_the_ring_wraps_around() {
        let (_shared, mut producer, mut state) = setup();
        let (mut next_in, mut next_out) = (0usize, 0usize);
        for round in 0..2_000usize {
            let n = 1 + (round * 37) % 900;
            let chunk: Vec<f32> = (next_in..next_in + n).map(|v| v as f32).collect();
            let before = producer.fill();
            producer.push_samples(&chunk);
            let accepted = producer.fill() - before;
            next_in += accepted;
            // 只有被接受的前 `accepted` 個取樣進了緩衝區（其餘被丟掉）；下一批從這裡接續。
            let take = 1 + (round * 53) % 700;
            for _ in 0..take {
                match state.consumer.pop() {
                    Ok(v) => {
                        assert_eq!(v, next_out as f32);
                        next_out += 1;
                    }
                    Err(_) => break,
                }
            }
        }
        assert!(next_out > 100_000, "繞過尾端很多次（取出 {next_out}）");
    }

    /// UI 顯示用的填充量：生產者送入之後、callback 取走之後都要反映。
    #[test]
    fn shared_fill_reflects_the_producer_and_the_callback() {
        let (shared, mut producer, mut state) = setup();
        assert_eq!(shared.fill(), 0);
        prime(&mut producer);
        assert_eq!(shared.fill(), producer.target_fill());
        assert!((shared.fill_ms() - TARGET_LATENCY_MS).abs() < 0.1);
        callback(&shared, &mut state, 100);
        assert_eq!(shared.fill(), producer.target_fill() - 100);
    }

    #[test]
    fn rate_controller_stays_within_half_a_percent() {
        let mut rc = RateController::new(2400);
        let base = 48_000.0;
        for _ in 0..500 {
            let hi = rc.update(1_000_000, 2400, base);
            assert!(hi >= base * (1.0 - MAX_RATE_ADJUST) - 1e-9);
        }
        let lowest = rc.update(1_000_000, 2400, base);
        assert!(
            (lowest - base * (1.0 - MAX_RATE_ADJUST)).abs() < 1e-6,
            "太滿：降到 −0.5%"
        );
        for _ in 0..500 {
            rc.update(0, 2400, base);
        }
        let highest = rc.update(0, 2400, base);
        assert!(
            (highest - base * (1.0 + MAX_RATE_ADJUST)).abs() < 1e-6,
            "太空：升到 +0.5%"
        );

        let mut rc = RateController::new(2400);
        assert_eq!(rc.update(2400, 2400, base), base, "剛好在目標：不調整");
    }

    /// 模擬：emu 以 60.0988 fps 每幀產生一批取樣，裝置消費的時脈與 emu 的時脈相差 `drift`
    /// （±0.3%）。動態速率控制必須讓緩衝區長時間維持穩定：沒有 underrun、沒有丟取樣、
    /// 延遲不持續累積。（純數值模擬，不碰硬體與執行緒。）
    #[test]
    fn dynamic_rate_control_keeps_the_buffer_stable_under_clock_drift() {
        for drift in [-0.003f64, -0.001, 0.0, 0.001, 0.003] {
            let (shared, mut producer, mut state) = setup();
            let target = producer.target_fill();
            let device = f64::from(RATE);
            let mut rc = RateController::new(target);
            let mut rate = device;

            let frame_time = 1.0 / 60.0988;
            // 裝置的實際消費速率 = 標稱 × (1 + drift)；callback 每 10 ms 一次。
            let device_actual = device * (1.0 + drift);
            let callback_frames = 480usize;
            let callback_period = callback_frames as f64 / device_actual;

            let (mut t, mut next_frame, mut next_cb) = (0.0f64, 0.0f64, 0.0f64);
            let mut produced_carry = 0.0f64;
            let (mut min_fill, mut max_fill) = (usize::MAX, 0usize);
            let seconds = 600.0; // 10 分鐘
            let mut scratch = vec![0.0f32; 4096];
            while t < seconds {
                if next_frame <= next_cb {
                    t = next_frame;
                    next_frame += frame_time;
                    // 這一幀核心產生 rate × frame_time 個取樣。
                    produced_carry += rate * frame_time;
                    let n = produced_carry as usize;
                    produced_carry -= n as f64;
                    producer.push_samples(&vec![0.3; n]);
                    rate = rc.update(producer.fill(), target, device);
                } else {
                    t = next_cb;
                    next_cb += callback_period;
                    fill_output(&mut scratch[..callback_frames], 1, &shared, &mut state);
                    if t > 30.0 {
                        min_fill = min_fill.min(state.consumer.slots());
                        max_fill = max_fill.max(state.consumer.slots());
                    }
                }
            }
            eprintln!(
                "漂移 {drift:+.3}：緩衝 {:.1}–{:.1} ms，最終取樣率 {rate:.1} Hz",
                min_fill as f64 / device * 1000.0,
                max_fill as f64 / device * 1000.0
            );
            assert_eq!(shared.underruns(), 0, "漂移 {drift:+}：underrun");
            assert_eq!(shared.dropped(), 0, "漂移 {drift:+}：丟取樣");
            let (lo, hi) = (
                min_fill as f64 / device * 1000.0,
                max_fill as f64 / device * 1000.0,
            );
            assert!(
                lo > 15.0 && hi < 95.0,
                "漂移 {drift:+}：緩衝 {lo:.0}–{hi:.0} ms"
            );
        }
    }

    /// 對照組：沒有動態速率控制時，同樣的漂移會讓緩衝區持續累積或跑乾。
    #[test]
    fn without_rate_control_drift_eventually_breaks_the_buffer() {
        let (shared, mut producer, mut state) = setup();
        let device = f64::from(RATE);
        let frame_time = 1.0 / 60.0988;
        let device_actual = device * 0.997; // 裝置比 emu 慢 0.3%
        let callback_frames = 480usize;
        let callback_period = callback_frames as f64 / device_actual;
        let (mut t, mut next_frame, mut next_cb) = (0.0f64, 0.0f64, 0.0f64);
        let mut carry = 0.0f64;
        let mut scratch = vec![0.0f32; 4096];
        while t < 120.0 {
            if next_frame <= next_cb {
                t = next_frame;
                next_frame += frame_time;
                carry += device * frame_time;
                let n = carry as usize;
                carry -= n as f64;
                producer.push_samples(&vec![0.3; n]);
            } else {
                t = next_cb;
                next_cb += callback_period;
                fill_output(&mut scratch[..callback_frames], 1, &shared, &mut state);
            }
        }
        assert!(shared.dropped() > 0, "緩衝區塞滿、開始丟取樣");
    }
}
