//! APU（audio processing unit）暫存器狀態。
//!
//! Phase 0 只放原始暫存器 byte，不做包絡線（envelope）/ 掃頻（sweep）/
//! 長度計數器等時序邏輯，也不產生任何取樣。

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Apu {
    pub pulse1: [u8; 4],   // $4000-$4003
    pub pulse2: [u8; 4],   // $4004-$4007
    pub triangle: [u8; 4], // $4008-$400B
    pub noise: [u8; 4],    // $400C-$400F
    pub dmc: [u8; 4],      // $4010-$4013
    pub status: u8,        // $4015
    pub frame_counter: u8, // $4017
    pub cycles: u64,
}

impl Apu {
    /// 依照 CPU 消耗的週期數推進 APU 時序。
    ///
    /// TODO Phase 1: 實作各聲道的包絡線 / 掃頻 / 長度計數器，並把混音後的
    /// 取樣塞進內部緩衝區，供 [`Apu::take_samples`] 取出。
    pub fn step(&mut self, _cpu_cycles: u64) {}

    /// 把累積的音訊取樣搬到 `out` 裡，呼叫後內部緩衝區清空。
    ///
    /// TODO Phase 1: 目前永遠不會推入任何取樣，此方法先維持「什麼都不做」
    /// 的正確行為（不 panic、不塞入假資料）。
    pub fn take_samples(&mut self, _out: &mut Vec<f32>) {}
}
