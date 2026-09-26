//! 固定種子的簡單 PRNG（SplitMix64），給網路模擬與測試腳本用。
//!
//! 不新增依賴：模擬網路要「可重現」的亂數，自己寫十行就夠。**只用在模擬與測試**；
//! 協定本身不需要亂數（session_id 由呼叫端提供）。

/// SplitMix64（Steele、Lea、Flood）。相同種子得到相同序列，跨平台一致。
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `[0, 1)` 的均勻分布。
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// 以機率 `p`（0–1）回傳 `true`。`p <= 0` 永遠 `false`，`p >= 1` 永遠 `true`（不消耗亂數）。
    pub fn chance(&mut self, p: f64) -> bool {
        if p <= 0.0 {
            false
        } else if p >= 1.0 {
            true
        } else {
            self.next_f64() < p
        }
    }
}

/// 一次性的雜湊（SplitMix64 的終結函式）：把 `x` 攪散成 64 位元，測試腳本用。
pub fn mix64(x: u64) -> u64 {
    SplitMix64::new(x).next_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_gives_same_sequence_and_different_seeds_differ() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        let mut c = SplitMix64::new(43);
        let xs: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        assert_eq!(xs, (0..8).map(|_| b.next_u64()).collect::<Vec<_>>());
        assert_ne!(xs, (0..8).map(|_| c.next_u64()).collect::<Vec<_>>());
    }

    /// 釘住已知的輸出（SplitMix64 種子 0 的前三個值，公開的參考實作結果），
    /// 確保這個 PRNG 不會被悄悄改掉而讓「固定種子」的模擬結果改變。
    #[test]
    fn matches_the_reference_sequence_for_seed_zero() {
        let mut r = SplitMix64::new(0);
        assert_eq!(r.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(r.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(r.next_u64(), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn chance_respects_its_bounds_and_is_roughly_calibrated() {
        let mut r = SplitMix64::new(7);
        assert!((0..100).all(|_| !r.chance(0.0)));
        assert!((0..100).all(|_| r.chance(1.0)));
        let hits = (0..20_000).filter(|_| r.chance(0.3)).count();
        assert!((5_400..6_600).contains(&hits), "30% 機率的命中數 {hits}");
    }

    #[test]
    fn next_f64_stays_in_the_unit_interval() {
        let mut r = SplitMix64::new(9);
        assert!((0..10_000).all(|_| (0.0..1.0).contains(&r.next_f64())));
    }
}
