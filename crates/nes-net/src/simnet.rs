//! 網路模擬層：包在任一 [`Transport`] 外層，依「虛擬時鐘」模擬丟包、固定延遲、抖動（自然造成
//! 亂序）與重複封包。
//!
//! - **虛擬時鐘**：所有時間都來自呼叫端傳入的 `now`；不讀系統時間、不 sleep。測試不需要真的等待，
//!   結果只由種子與呼叫順序決定（完全可重現）。
//! - **作用在送出端**：`send` 時決定這個封包丟不丟、複製幾份、各自的送達時間；之後任何一次
//!   `send`／`recv` 只要 `now` 已經到了送達時間，就把它交給內層 transport。所以兩端各包一層，
//!   就是雙向、各自獨立的損傷。
//! - **延遲是單程**：`delay` ＝ 一個封包從送出到可被對方收到的基本時間，RTT 約為 2 × `delay`
//!   （不含兩端輪詢的間隔）。每個封包的實際延遲是 `delay ± jitter`（均勻分布，最小 0），所以
//!   `jitter > 0` 時封包會亂序。
//! - **順序**：送達時間相同的封包保持送出順序（以序號打破平手），所以 `jitter = 0` 時不會亂序。

use std::collections::BinaryHeap;
use std::net::SocketAddr;
use std::time::Duration;

use crate::rng::SplitMix64;
use crate::transport::{Datagram, Transport};

/// 網路條件。預設＝理想網路（不丟包、零延遲、無抖動、不重複）。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct NetworkConfig {
    /// 丟包率（0–1）。
    pub loss: f64,
    /// 單程的基本延遲。
    pub delay: Duration,
    /// 抖動：每個封包的延遲在 `delay ± jitter` 之間均勻分布。
    pub jitter: Duration,
    /// 重複封包率（0–1）：被送出的封包再多送一份（各自獨立的延遲）的機率。
    pub duplicate: f64,
}

impl NetworkConfig {
    pub const IDEAL: NetworkConfig = NetworkConfig {
        loss: 0.0,
        delay: Duration::ZERO,
        jitter: Duration::ZERO,
        duplicate: 0.0,
    };

    /// 完全斷線：所有封包都丟掉。
    pub fn blackout(self) -> Self {
        Self { loss: 1.0, ..self }
    }
}

#[derive(Debug)]
struct Pending {
    deliver_at: Duration,
    seq: u64,
    to: Option<SocketAddr>,
    data: Vec<u8>,
}

impl PartialEq for Pending {
    fn eq(&self, other: &Self) -> bool {
        (self.deliver_at, self.seq) == (other.deliver_at, other.seq)
    }
}
impl Eq for Pending {}
impl PartialOrd for Pending {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Pending {
    /// `BinaryHeap` 是最大堆積：反向比較，最早送達（同時則最早送出）的在最上面。
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (other.deliver_at, other.seq).cmp(&(self.deliver_at, self.seq))
    }
}

/// 統計：模擬層對封包做了什麼（測試與 `netsim` 報告用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SimStats {
    pub sent: u64,
    pub dropped: u64,
    pub duplicated: u64,
    pub delivered: u64,
}

pub struct SimulatedTransport<T> {
    inner: T,
    config: NetworkConfig,
    rng: SplitMix64,
    queue: BinaryHeap<Pending>,
    seq: u64,
    stats: SimStats,
}

impl<T: Transport> SimulatedTransport<T> {
    pub fn new(inner: T, config: NetworkConfig, seed: u64) -> Self {
        Self {
            inner,
            config,
            rng: SplitMix64::new(seed),
            queue: BinaryHeap::new(),
            seq: 0,
            stats: SimStats::default(),
        }
    }

    /// 執行中改變網路條件（例如中途斷線）。已經在路上的封包不受影響。
    pub fn set_config(&mut self, config: NetworkConfig) {
        self.config = config;
    }

    pub fn config(&self) -> NetworkConfig {
        self.config
    }

    pub fn stats(&self) -> SimStats {
        self.stats
    }

    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    /// 還在路上（尚未送達）的封包數。
    pub fn in_flight(&self) -> usize {
        self.queue.len()
    }

    fn latency(&mut self) -> Duration {
        let jitter = self.config.jitter.as_nanos() as f64;
        // 在 [-jitter, +jitter] 均勻取值；只有 jitter > 0 才消耗亂數。
        let offset = if jitter > 0.0 {
            (self.rng.next_f64() * 2.0 - 1.0) * jitter
        } else {
            0.0
        };
        let nanos = (self.config.delay.as_nanos() as f64 + offset).max(0.0);
        Duration::from_nanos(nanos as u64)
    }

    fn enqueue(&mut self, now: Duration, to: Option<SocketAddr>, data: &[u8]) {
        self.stats.sent += 1;
        if self.rng.chance(self.config.loss) {
            self.stats.dropped += 1;
            return;
        }
        let copies = if self.rng.chance(self.config.duplicate) {
            self.stats.duplicated += 1;
            2
        } else {
            1
        };
        for _ in 0..copies {
            let deliver_at = now + self.latency();
            self.seq += 1;
            self.queue.push(Pending {
                deliver_at,
                seq: self.seq,
                to,
                data: data.to_vec(),
            });
        }
    }

    /// 把送達時間已到的封包交給內層 transport。
    fn pump(&mut self, now: Duration) {
        while let Some(top) = self.queue.peek() {
            if top.deliver_at > now {
                break;
            }
            let Some(p) = self.queue.pop() else { break };
            self.stats.delivered += 1;
            match p.to {
                Some(addr) => self.inner.send_to(now, addr, &p.data),
                None => self.inner.send(now, &p.data),
            }
        }
    }
}

impl<T: Transport> Transport for SimulatedTransport<T> {
    fn send(&mut self, now: Duration, data: &[u8]) {
        self.enqueue(now, None, data);
        self.pump(now);
    }

    fn send_to(&mut self, now: Duration, addr: SocketAddr, data: &[u8]) {
        self.enqueue(now, Some(addr), data);
        self.pump(now);
    }

    fn recv(&mut self, now: Duration) -> Vec<Datagram> {
        self.pump(now);
        self.inner.recv(now)
    }

    fn set_peer(&mut self, addr: SocketAddr) {
        self.inner.set_peer(addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::InMemoryTransport;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn pair(
        config: NetworkConfig,
        seed: u64,
    ) -> (SimulatedTransport<InMemoryTransport>, InMemoryTransport) {
        let (a, b) = InMemoryTransport::pair();
        (SimulatedTransport::new(a, config, seed), b)
    }

    fn payloads(t: &mut impl Transport, now: Duration) -> Vec<u32> {
        t.recv(now)
            .into_iter()
            .map(|d| u32::from_le_bytes(d.data.try_into().unwrap()))
            .collect()
    }

    #[test]
    fn ideal_network_delivers_everything_in_order_immediately() {
        let (mut a, mut b) = pair(NetworkConfig::IDEAL, 1);
        for i in 0..100u32 {
            a.send(ms(0), &i.to_le_bytes());
        }
        assert_eq!(payloads(&mut b, ms(0)), (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn fixed_delay_holds_packets_until_the_virtual_clock_reaches_them() {
        let cfg = NetworkConfig {
            delay: ms(100),
            ..NetworkConfig::IDEAL
        };
        let (mut a, mut b) = pair(cfg, 1);
        a.send(ms(0), &7u32.to_le_bytes());
        assert!(payloads(&mut b, ms(0)).is_empty());
        // 內層要等 `a` 被驅動（send／recv）才會收到封包：模擬層是主動 pump 的。
        a.recv(ms(99));
        assert!(payloads(&mut b, ms(99)).is_empty());
        a.recv(ms(100));
        assert_eq!(payloads(&mut b, ms(100)), vec![7]);
    }

    #[test]
    fn loss_rate_is_roughly_calibrated_and_reproducible() {
        let run = |seed| {
            let cfg = NetworkConfig {
                loss: 0.3,
                ..NetworkConfig::IDEAL
            };
            let (mut a, mut b) = pair(cfg, seed);
            for i in 0..10_000u32 {
                a.send(ms(0), &i.to_le_bytes());
            }
            payloads(&mut b, ms(0))
        };
        let got = run(5);
        assert!((6_600..7_400).contains(&got.len()), "收到 {}", got.len());
        assert_eq!(got, run(5), "相同種子必須得到完全相同的結果");
        assert_ne!(got, run(6));
    }

    #[test]
    fn jitter_reorders_packets_but_never_loses_them() {
        let cfg = NetworkConfig {
            delay: ms(100),
            jitter: ms(80),
            ..NetworkConfig::IDEAL
        };
        let (mut a, mut b) = pair(cfg, 3);
        for i in 0..200u32 {
            a.send(ms(i as u64 * 16), &i.to_le_bytes());
        }
        a.recv(ms(10_000));
        let got = payloads(&mut b, ms(10_000));
        assert_eq!(got.len(), 200);
        let mut sorted = got.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..200).collect::<Vec<_>>());
        assert_ne!(got, sorted, "抖動 80ms、間隔 16ms 的封包應該會亂序");
    }

    #[test]
    fn delays_stay_within_delay_plus_or_minus_jitter() {
        let cfg = NetworkConfig {
            delay: ms(100),
            jitter: ms(30),
            ..NetworkConfig::IDEAL
        };
        let (mut a, mut b) = pair(cfg, 11);
        for i in 0..500u32 {
            a.send(ms(0), &i.to_le_bytes());
        }
        a.recv(ms(69));
        assert!(payloads(&mut b, ms(69)).is_empty(), "最早也要 70ms");
        a.recv(ms(130));
        assert_eq!(payloads(&mut b, ms(130)).len(), 500, "最晚 130ms 全部送達");
    }

    #[test]
    fn duplicates_deliver_extra_copies() {
        let cfg = NetworkConfig {
            duplicate: 0.25,
            ..NetworkConfig::IDEAL
        };
        let (mut a, mut b) = pair(cfg, 9);
        for i in 0..10_000u32 {
            a.send(ms(0), &i.to_le_bytes());
        }
        let got = payloads(&mut b, ms(0)).len();
        assert!(
            (12_000..13_000).contains(&got),
            "10000 個封包、25% 重複 → 收到 {got}"
        );
        assert_eq!(a.stats().duplicated as usize, got - 10_000);
    }

    #[test]
    fn blackout_drops_everything_from_then_on_but_not_what_is_already_in_flight() {
        let cfg = NetworkConfig {
            delay: ms(50),
            ..NetworkConfig::IDEAL
        };
        let (mut a, mut b) = pair(cfg, 2);
        a.send(ms(0), &1u32.to_le_bytes());
        a.set_config(cfg.blackout());
        a.send(ms(1), &2u32.to_le_bytes());
        a.recv(ms(1_000));
        assert_eq!(payloads(&mut b, ms(1_000)), vec![1]);
        assert_eq!(a.stats().dropped, 1);
    }

    #[test]
    fn works_over_any_transport_including_boxed_ones() {
        let (a, mut b) = InMemoryTransport::pair();
        let boxed: Box<dyn Transport> = Box::new(a);
        let mut sim = SimulatedTransport::new(boxed, NetworkConfig::IDEAL, 1);
        sim.send(ms(0), &5u32.to_le_bytes());
        assert_eq!(payloads(&mut b, ms(0)), vec![5]);
    }
}
