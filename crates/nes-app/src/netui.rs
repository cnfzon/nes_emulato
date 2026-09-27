//! Netplay 的 UI 輔助（Phase 4d）：連線大廳與統計疊加層用的**純資料邏輯**（可單元測試，不需要 GUI），
//! 加上兩個很小的 egui 繪圖函式（折線圖）。視窗本身的版面在 `app.rs`。
//!
//! - [`RecentAddrs`]：最近連線過的位址（只存在記憶體，設定檔留到 Phase 5）。
//! - [`parse_port`]／[`parse_join_addr`]：大廳輸入欄的檢查與給使用者看的錯誤訊息。
//! - [`NetHistory`]：最近 10 秒的 ping 與每秒 rollback 次數（疊加層的折線）。
//! - [`overlay_lines`]：疊加層的文字內容。
//! - [`draw_line_chart`]：用 egui 的 painter 自己畫的折線（不引入繪圖 crate）。

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Duration;

use eframe::egui;
use nes_net::Mode;

use crate::commands::{NetPhase, NetStatus};

/// 最近連線過的位址最多記幾筆。
pub const MAX_RECENT: usize = 8;
/// 折線圖涵蓋的秒數（統計每秒一筆）。
pub const CHART_SECONDS: usize = 10;
/// Host 等待超過這麼久還沒有人連進來，就提示檢查防火牆。
pub const FIREWALL_HINT_AFTER: Duration = Duration::from_secs(10);
/// 加入者等待房主回應的上限（與 `nes-net` 的 `HANDSHAKE_TIMEOUT` 相同，只用來顯示「n / 10 秒」）。
pub const JOIN_TIMEOUT_SECS: u64 = 10;
/// 超過這麼久沒有收到對方的封包，疊加層與狀態列就提早警告（session 的斷線逾時是 5 秒）。
pub const SILENCE_WARN_AFTER: Duration = Duration::from_millis(1500);

// ---- 最近連線過的位址 --------------------------------------------------------------

/// 最近連線過的位址：最新的在最前面、不重複、最多 [`MAX_RECENT`] 筆。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecentAddrs {
    list: Vec<SocketAddr>,
}

impl RecentAddrs {
    pub fn remember(&mut self, addr: SocketAddr) {
        self.list.retain(|a| *a != addr);
        self.list.insert(0, addr);
        self.list.truncate(MAX_RECENT);
    }

    pub fn iter(&self) -> impl Iterator<Item = &SocketAddr> {
        self.list.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn clear(&mut self) {
        self.list.clear();
    }
}

// ---- 輸入欄的檢查 ------------------------------------------------------------------

/// 房主的監聽 port。0 是合法的（系統挑一個，實際的 port 會顯示在等待畫面），但對「要告訴對方」沒有用，
/// 所以大廳用 [`parse_port`] 要求 1–65535。
pub fn parse_port(text: &str) -> Result<u16, String> {
    match text.trim().parse::<u32>() {
        Ok(p @ 1..=65535) => Ok(p as u16),
        Ok(_) => Err("port 必須在 1–65535 之間（例如 7000）".to_string()),
        Err(_) => Err("port 必須是整數（例如 7000）".to_string()),
    }
}

/// 加入者輸入的「IP:port」。錯誤訊息說明缺什麼。
pub fn parse_join_addr(text: &str) -> Result<SocketAddr, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("請輸入房主的 IP:port，例如 192.168.1.10:7000".to_string());
    }
    let addr: SocketAddr = text.parse().map_err(|_| {
        if text.contains(':') {
            "格式不正確：必須是「IPv4:port」，例如 192.168.1.10:7000".to_string()
        } else {
            "缺少 port：格式必須是「IP:port」，例如 192.168.1.10:7000".to_string()
        }
    })?;
    if addr.port() == 0 {
        return Err("port 不能是 0".to_string());
    }
    if addr.ip().is_unspecified() {
        return Err("IP 不能是 0.0.0.0：請輸入房主的區網 IP".to_string());
    }
    Ok(addr)
}

// ---- 等待中的提示 ------------------------------------------------------------------

/// Host 是否該提示檢查防火牆（等待已超過 [`FIREWALL_HINT_AFTER`]）。
pub fn firewall_hint_due(waited: Duration) -> bool {
    waited >= FIREWALL_HINT_AFTER
}

/// 防火牆提示的文字（要放行的是 UDP，不是 TCP）。
pub fn firewall_hint(port: u16) -> String {
    format!(
        "已等待超過 {} 秒仍然沒有人連進來。請檢查：\n\
         ① Windows 防火牆是否允許本程式使用 UDP port {port}（第一次建立房間時跳出的提示要選「允許」；\
         錯過了可到「Windows Defender 防火牆 → 允許應用程式通過防火牆」勾選 nes-app，\
         或新增一條允許 UDP {port} 的輸入規則）；\n\
         ② 對方輸入的 IP 與 port 是否與上面顯示的相同（多張網卡、VPN 開著時，顯示的 IP 可能不是對方所在網段的那個）；\n\
         ③ 雙方是否在同一個區域網路（本程式不做 NAT 穿透，跨網際網路需要路由器設定 port forwarding）。",
        FIREWALL_HINT_AFTER.as_secs()
    )
}

// ---- 折線的歷史資料 -----------------------------------------------------------------

/// 最近 [`CHART_SECONDS`] 個樣本（每秒一個；`None`＝那一秒沒有值，畫成斷線）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Series {
    samples: VecDeque<Option<f32>>,
}

impl Series {
    pub fn push(&mut self, value: Option<f32>) {
        self.samples.push_back(value);
        while self.samples.len() > CHART_SECONDS {
            self.samples.pop_front();
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = Option<f32>> + '_ {
        self.samples.iter().copied()
    }

    pub fn last(&self) -> Option<f32> {
        self.samples.back().copied().flatten()
    }

    /// 最大值（至少 `floor`，避免整條線是 0 時除以 0、線貼在最上面）。
    pub fn max_at_least(&self, floor: f32) -> f32 {
        self.iter().flatten().fold(floor, f32::max)
    }
}

/// 疊加層的兩條折線：ping（毫秒）與每秒 rollback 次數。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NetHistory {
    pub ping_ms: Series,
    pub rollbacks_per_sec: Series,
    last_seq: Option<u32>,
}

impl NetHistory {
    /// 統計每秒更新一次（`NetStatus::seq` 每次加 1）；同一個 `seq` 只記一次，所以階段改變等其他原因
    /// 重發的 `NetStatus` 不會把折線灌滿。回傳這次有沒有記下新樣本。
    pub fn record(&mut self, status: &NetStatus) -> bool {
        if !matches!(status.phase, NetPhase::Connected { .. }) {
            return false;
        }
        if self.last_seq == Some(status.seq) {
            return false;
        }
        self.last_seq = Some(status.seq);
        let s = &status.stats;
        self.ping_ms
            .push(s.rtt.map(|r| (r.as_secs_f64() * 1000.0) as f32));
        self.rollbacks_per_sec
            .push(s.rollback.map(|r| r.rollbacks_per_sec));
        true
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

// ---- 疊加層的文字 -------------------------------------------------------------------

/// 一行疊加層文字；`warn`＝用警告色顯示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayLine {
    pub text: String,
    pub warn: bool,
}

fn line(text: String) -> OverlayLine {
    OverlayLine { text, warn: false }
}

fn ms_text(d: Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1000.0)
}

fn rate_text(bytes_per_sec: u32) -> String {
    if bytes_per_sec >= 1024 {
        format!("{:.1} KB/s", f64::from(bytes_per_sec) / 1024.0)
    } else {
        format!("{bytes_per_sec} B/s")
    }
}

/// 統計疊加層的文字：模式、ping、每秒 rollback 次數、平均與最大重跑深度、重跑耗時、幀數優勢、
/// 預測準確率、stall 次數、雙向頻寬、音訊 underrun 次數（折線另外畫）。
pub fn overlay_lines(status: &NetStatus) -> Vec<OverlayLine> {
    let NetPhase::Connected {
        player,
        input_delay,
        mode,
    } = status.phase
    else {
        return vec![line("Netplay 未連線".to_string())];
    };
    let s = &status.stats;
    let mut out = vec![line(format!(
        "模式 {mode}｜玩家 {}｜input delay {input_delay} 幀｜已確認 {} 幀",
        player + 1,
        s.frame
    ))];
    out.push(line(format!(
        "ping {}",
        s.rtt.map_or("—".to_string(), |r| format!(
            "{:.0} ms",
            r.as_secs_f64() * 1000.0
        ))
    )));
    match (mode, &s.rollback) {
        (Mode::Rollback, Some(rb)) => {
            out.push(line(format!(
                "rollback {:.1} 次/秒（累計 {}）",
                rb.rollbacks_per_sec, rb.rollbacks
            )));
            out.push(line(format!(
                "重跑深度 平均 {:.1}／最大 {} 幀",
                rb.avg_depth, rb.max_depth
            )));
            out.push(line(format!(
                "重跑耗時 平均 {}／最大 {}",
                ms_text(rb.resim_time_avg),
                ms_text(rb.resim_time_max)
            )));
            out.push(line(format!(
                "幀數優勢 {:+.1}（對方回報 {:+.1}）",
                rb.frame_advantage, rb.remote_advantage
            )));
            out.push(line(format!(
                "預測準確率 {}（對 {}／錯 {}）",
                rb.prediction_accuracy()
                    .map_or("—".to_string(), |a| format!("{:.1}%", a * 100.0)),
                rb.prediction_correct,
                rb.prediction_wrong
            )));
        }
        _ => out.push(line(
            "lockstep：沒有預測與重跑（延遲高時會 stall）".to_string(),
        )),
    }
    out.push(line(format!(
        "stall {} 次（{:.2} 秒）",
        s.stalls,
        s.stall_time.as_secs_f64()
    )));
    out.push(line(format!(
        "頻寬 ↑ {} ↓ {}",
        rate_text(s.send_bytes_per_sec),
        rate_text(s.recv_bytes_per_sec)
    )));
    out.push(line(format!("音訊 underrun {} 次", status.audio_underruns)));
    if s.packets_ignored > 0 {
        out.push(line(format!("已忽略封包 {}", s.packets_ignored)));
    }
    if s.silent_for >= SILENCE_WARN_AFTER {
        out.push(OverlayLine {
            text: format!(
                "⚠ 已 {:.1} 秒沒有收到對方的封包（超過 5 秒會中斷連線）",
                s.silent_for.as_secs_f64()
            ),
            warn: true,
        });
    }
    out
}

// ---- 折線圖 -------------------------------------------------------------------------

/// 把樣本換算成折線的頂點：x 均勻分布（最新的在右邊），y 依 `max` 縮放（0 在底部）。
/// `None` 的樣本把線切成幾段。純函式，方便測試。
pub fn chart_segments(samples: &[Option<f32>], rect: egui::Rect, max: f32) -> Vec<Vec<egui::Pos2>> {
    let mut segments: Vec<Vec<egui::Pos2>> = Vec::new();
    let mut current: Vec<egui::Pos2> = Vec::new();
    let steps = (CHART_SECONDS - 1).max(1) as f32;
    // 樣本不滿 10 個時靠右對齊（最新的永遠在最右邊）。
    let offset = CHART_SECONDS.saturating_sub(samples.len());
    for (i, sample) in samples.iter().enumerate() {
        match sample {
            Some(v) => {
                let x = rect.left() + rect.width() * (offset + i) as f32 / steps;
                let t = (v / max).clamp(0.0, 1.0);
                let y = rect.bottom() - rect.height() * t;
                current.push(egui::pos2(x, y));
            }
            None => {
                if !current.is_empty() {
                    segments.push(std::mem::take(&mut current));
                }
            }
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// 畫一條最近 10 秒的折線（標題、目前值與縱軸最大值標在圖上）。
pub fn draw_line_chart(
    ui: &mut egui::Ui,
    title: &str,
    unit: &str,
    series: &Series,
    color: egui::Color32,
    size: egui::Vec2,
) {
    let (response, painter) = ui.allocate_painter(size, egui::Sense::hover());
    let rect = response.rect;
    painter.rect_filled(rect, 2.0, egui::Color32::from_black_alpha(90));
    painter.rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(1.0, egui::Color32::from_white_alpha(60)),
        egui::StrokeKind::Inside,
    );
    let plot = rect.shrink2(egui::vec2(4.0, 14.0));
    let max = series.max_at_least(1.0);
    let samples: Vec<Option<f32>> = series.iter().collect();
    for points in chart_segments(&samples, plot, max) {
        if points.len() == 1 {
            painter.circle_filled(points[0], 1.5, color);
        } else {
            painter.add(egui::Shape::line(points, egui::Stroke::new(1.5, color)));
        }
    }
    let font = egui::FontId::monospace(10.0);
    let current = series
        .last()
        .map_or("—".to_string(), |v| format!("{v:.1} {unit}"));
    painter.text(
        rect.left_top() + egui::vec2(4.0, 1.0),
        egui::Align2::LEFT_TOP,
        format!("{title}  {current}"),
        font.clone(),
        egui::Color32::WHITE,
    );
    painter.text(
        rect.right_top() + egui::vec2(-4.0, 1.0),
        egui::Align2::RIGHT_TOP,
        format!("max {max:.0}"),
        font,
        egui::Color32::from_white_alpha(140),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use nes_net::{RollbackStats, Stats};

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn connected(mode: Mode, seq: u32, stats: Stats) -> NetStatus {
        NetStatus {
            phase: NetPhase::Connected {
                player: 1,
                input_delay: 2,
                mode,
            },
            stats,
            seq,
            audio_underruns: 3,
        }
    }

    #[test]
    fn recent_addresses_are_newest_first_unique_and_capped() {
        let mut r = RecentAddrs::default();
        assert!(r.is_empty());
        for i in 0..12 {
            r.remember(addr(&format!("192.168.1.{i}:7000")));
        }
        assert_eq!(r.iter().count(), MAX_RECENT);
        assert_eq!(r.iter().next(), Some(&addr("192.168.1.11:7000")));
        // 再連一次舊的：移到最前面，不重複。
        r.remember(addr("192.168.1.8:7000"));
        let v: Vec<_> = r.iter().copied().collect();
        assert_eq!(v[0], addr("192.168.1.8:7000"));
        assert_eq!(
            v.iter().filter(|a| **a == addr("192.168.1.8:7000")).count(),
            1
        );
        assert_eq!(v.len(), MAX_RECENT);
        r.clear();
        assert!(r.is_empty());
    }

    #[test]
    fn port_and_address_validation_explain_what_is_wrong() {
        assert_eq!(parse_port(" 7000 "), Ok(7000));
        assert_eq!(parse_port("65535"), Ok(65535));
        for bad in ["0", "65536", "-1", "abc", ""] {
            assert!(parse_port(bad).is_err(), "{bad}");
        }
        assert_eq!(
            parse_join_addr(" 192.168.1.10:7000 "),
            Ok(addr("192.168.1.10:7000"))
        );
        assert!(parse_join_addr("").unwrap_err().contains("請輸入"));
        assert!(
            parse_join_addr("192.168.1.10")
                .unwrap_err()
                .contains("缺少 port")
        );
        assert!(
            parse_join_addr("192.168.1.10:x")
                .unwrap_err()
                .contains("格式不正確")
        );
        assert!(
            parse_join_addr("192.168.1.10:0")
                .unwrap_err()
                .contains("port 不能是 0")
        );
        assert!(
            parse_join_addr("0.0.0.0:7000")
                .unwrap_err()
                .contains("0.0.0.0")
        );
        assert!(
            parse_join_addr("host.example:7000").is_err(),
            "不做 DNS 解析"
        );
    }

    #[test]
    fn the_firewall_hint_appears_after_exactly_ten_seconds_and_names_the_port() {
        assert!(!firewall_hint_due(Duration::from_millis(9_999)));
        assert!(firewall_hint_due(Duration::from_secs(10)));
        let text = firewall_hint(7000);
        assert!(
            text.contains("UDP port 7000") && text.contains("防火牆"),
            "{text}"
        );
        assert!(text.contains("10 秒"));
    }

    #[test]
    fn history_keeps_the_last_ten_seconds_and_records_each_stats_update_once() {
        let mut h = NetHistory::default();
        let rb = |per_sec: f32| Stats {
            rtt: Some(Duration::from_millis(40)),
            rollback: Some(RollbackStats {
                rollbacks_per_sec: per_sec,
                ..RollbackStats::default()
            }),
            ..Stats::default()
        };
        for seq in 1..=15u32 {
            assert!(h.record(&connected(Mode::Rollback, seq, rb(seq as f32))));
            // 階段改變等原因重發同一份統計：不重複記錄。
            assert!(!h.record(&connected(Mode::Rollback, seq, rb(seq as f32))));
        }
        assert_eq!(h.ping_ms.len(), CHART_SECONDS);
        let rollbacks: Vec<f32> = h.rollbacks_per_sec.iter().flatten().collect();
        assert_eq!(rollbacks, (6..=15).map(|v| v as f32).collect::<Vec<_>>());
        assert_eq!(h.ping_ms.last(), Some(40.0));
        // 沒連線時不記錄；lockstep 沒有 rollback 就是空的樣本（斷線）。
        assert!(!h.record(&NetStatus::default()));
        h.clear();
        assert!(h.record(&connected(Mode::Lockstep, 1, Stats::default())));
        assert_eq!(h.rollbacks_per_sec.last(), None);
        assert_eq!(h.ping_ms.len(), 1);
    }

    #[test]
    fn chart_segments_scale_align_right_and_break_at_missing_samples() {
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(90.0, 100.0));
        // 三個樣本靠右對齊：x 位於第 7、8、9 格（共 9 格＝寬 90）。
        let segs = chart_segments(&[Some(0.0), Some(5.0), Some(10.0)], rect, 10.0);
        assert_eq!(segs.len(), 1);
        assert_eq!(
            segs[0],
            vec![
                egui::pos2(70.0, 100.0),
                egui::pos2(80.0, 50.0),
                egui::pos2(90.0, 0.0)
            ]
        );
        // 超過 max 的夾在頂端；None 把線切成兩段。
        let segs = chart_segments(&[Some(1.0), None, Some(100.0)], rect, 10.0);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[1][0].y, 0.0);
        assert!(chart_segments(&[None, None], rect, 1.0).is_empty());
    }

    #[test]
    fn overlay_shows_every_required_statistic_for_rollback() {
        let stats = Stats {
            mode: Mode::Rollback,
            rtt: Some(Duration::from_millis(48)),
            frame: 1234,
            stalls: 2,
            stall_time: Duration::from_millis(80),
            send_bytes_per_sec: 1900,
            recv_bytes_per_sec: 800,
            packets_ignored: 5,
            rollback: Some(RollbackStats {
                rollbacks: 30,
                rollbacks_per_sec: 2.5,
                avg_depth: 1.8,
                max_depth: 4,
                resim_time_avg: Duration::from_micros(420),
                resim_time_max: Duration::from_micros(1100),
                frame_advantage: 0.8,
                remote_advantage: -0.7,
                prediction_correct: 90,
                prediction_wrong: 10,
                ..RollbackStats::default()
            }),
            ..Stats::default()
        };
        let text: String = overlay_lines(&connected(Mode::Rollback, 1, stats))
            .iter()
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        for needle in [
            "模式 rollback",
            "ping 48 ms",
            "rollback 2.5 次/秒",
            "平均 1.8／最大 4",
            "重跑耗時 平均 0.42 ms／最大 1.10 ms",
            "幀數優勢 +0.8",
            "預測準確率 90.0%",
            "stall 2 次",
            "↑ 1.9 KB/s ↓ 800 B/s",
            "音訊 underrun 3 次",
            "已忽略封包 5",
        ] {
            assert!(text.contains(needle), "缺少「{needle}」：\n{text}");
        }
        assert!(!text.contains('⚠'), "沒有沉默就沒有警告");
    }

    #[test]
    fn overlay_lockstep_and_silence_warning_and_idle() {
        let stats = Stats {
            mode: Mode::Lockstep,
            silent_for: Duration::from_millis(3200),
            ..Stats::default()
        };
        let lines = overlay_lines(&connected(Mode::Lockstep, 1, stats));
        assert!(lines.iter().any(|l| l.text.contains("lockstep：沒有預測")));
        let warn = lines.iter().find(|l| l.warn).expect("必須有警告行");
        assert!(
            warn.text.contains("3.2 秒沒有收到對方的封包"),
            "{}",
            warn.text
        );
        assert_eq!(
            overlay_lines(&NetStatus::default())[0].text,
            "Netplay 未連線"
        );
    }
}
