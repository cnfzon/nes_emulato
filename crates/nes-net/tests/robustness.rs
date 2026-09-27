//! Phase 4d：語意層級的強健性測試。
//!
//! 4b 已證明「任何位元組序列都不會 panic」；這裡驗證**格式正確、但內容不可能來自遵守協定的對方**的封包：
//! 幀號遠超出合理範圍、冗餘輸入超過上限、同一幀收到不同的輸入、握手完成後又收到 Accept／Hello、
//! 來自非對方位址的封包……這些必須以明確的原因中止連線（[`EndReason::ProtocolViolation`]），或安全地忽略；
//! 絕不能 panic、造成 desync，或讓緩衝區無限增長。以及封包洪流下所有佇列維持有界。
//!
//! 做法：被測的 [`Session`] 接一個「腳本化的對端」（[`Wire`]）——測試自己扮演對方，想送什麼封包就送什麼、
//! 想讓它來自哪個位址就是哪個位址（包含 `stranger` 標記，真實的 `UdpTransport` 才會設定的旗標），
//! 並檢查 session 送出的每一個封包。虛擬時鐘，不 sleep、不讀系統時間。

use std::net::SocketAddr;
use std::time::Duration;

use nes_core::{Buttons, CORE_BEHAVIOR_VERSION, RomId};
use nes_net::protocol::{DisconnectReason, MAX_INPUTS_PER_PACKET, PROTOCOL_VERSION};
use nes_net::rng::SplitMix64;
use nes_net::session::{MAX_DATAGRAMS_PER_POLL, MAX_QUEUED_EVENTS, MAX_UNACKED_INPUTS};
use nes_net::{
    Datagram, EndReason, Event, Mode, Msg, PlayerInput, ProtocolError, RejectReason, Request,
    Session, SessionConfig, Status, Transport, Violation,
};

const PEER: &str = "10.0.0.2:5000";
const STRANGER: &str = "10.0.0.9:6000";
const SID: u32 = 0x5EED_0001;

fn rom_id() -> RomId {
    RomId::of_file(b"robustness test rom")
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

/// 對方（合法的那一位）在第 `f` 幀的輸入。
fn peer_in(f: u32) -> PlayerInput {
    PlayerInput::from(Buttons::from_bits_truncate((f.wrapping_mul(7) + 3) as u8))
}

/// 腳本化的對端：`inbox` 是下一次 `recv` 會交給 session 的封包，`sent` 是 session 送出的所有封包。
#[derive(Default)]
struct Wire {
    inbox: Vec<Datagram>,
    sent: Vec<(Option<SocketAddr>, Vec<u8>)>,
    locked_to: Option<SocketAddr>,
}

impl Transport for Wire {
    fn send(&mut self, _now: Duration, data: &[u8]) {
        self.sent.push((self.locked_to, data.to_vec()));
    }
    fn send_to(&mut self, _now: Duration, addr: SocketAddr, data: &[u8]) {
        self.sent.push((Some(addr), data.to_vec()));
    }
    fn recv(&mut self, _now: Duration) -> Vec<Datagram> {
        std::mem::take(&mut self.inbox)
    }
    fn set_peer(&mut self, addr: SocketAddr) {
        self.locked_to = Some(addr);
    }
}

fn from_peer(msg: &Msg) -> Datagram {
    Datagram {
        data: msg.encode().unwrap(),
        from: Some(addr(PEER)),
        stranger: false,
    }
}

fn from_stranger(msg: &Msg) -> Datagram {
    Datagram {
        data: msg.encode().unwrap(),
        from: Some(addr(STRANGER)),
        stranger: true,
    }
}

struct Rig {
    s: Session,
    w: Wire,
    now: Duration,
    mode: Mode,
    events: Vec<Event>,
    /// 目前為止 session 送出的所有訊息（已解碼），與它們的目的位址。
    out: Vec<(Option<SocketAddr>, Msg)>,
    max_sent_input: Option<u32>,
    /// 到目前為止交給 `run_frame` 的每一幀輸入（lockstep）或新確認的幀（rollback）的「對方按鍵」。
    peer_buttons_seen: Vec<Buttons>,
    next_peer_frame: u32,
    samples: u32,
}

impl Rig {
    /// Host：對方（Client）用 Hello 連上來。
    fn host(mode: Mode) -> Rig {
        let cfg = SessionConfig::host(rom_id(), 2, SID).with_mode(mode);
        let mut rig = Rig::bare(Session::new(cfg), mode);
        rig.w.inbox.push(from_peer(&Msg::Hello {
            protocol_version: PROTOCOL_VERSION,
            core_behavior_version: CORE_BEHAVIOR_VERSION,
            rom_id: rom_id(),
        }));
        rig.step();
        assert!(rig.s.is_running(), "握手應該成功");
        rig
    }

    /// Client：房主用 Accept 接受我們。
    fn client(mode: Mode) -> Rig {
        let cfg = SessionConfig::client(rom_id()).with_mode(mode);
        let mut rig = Rig::bare(Session::new(cfg), mode);
        rig.step(); // 送出 Hello
        assert!(
            rig.out.iter().any(|(_, m)| matches!(m, Msg::Hello { .. })),
            "Client 應該送出 Hello"
        );
        rig.w.inbox.push(from_peer(&accept(mode)));
        rig.step();
        assert!(rig.s.is_running(), "握手應該成功");
        rig
    }

    fn bare(s: Session, mode: Mode) -> Rig {
        Rig {
            s,
            w: Wire::default(),
            now: ms(1),
            mode,
            events: Vec::new(),
            out: Vec::new(),
            max_sent_input: None,
            peer_buttons_seen: Vec::new(),
            next_peer_frame: 0,
            samples: 0,
        }
    }

    fn feed(&mut self, msg: Msg) {
        self.w.inbox.push(from_peer(&msg));
    }

    fn feed_stranger(&mut self, msg: Msg) {
        self.w.inbox.push(from_stranger(&msg));
    }

    /// 虛擬時間前進 1 ms 並 poll 一次；收集事件與 session 送出的封包。
    fn step(&mut self) {
        self.now += ms(1);
        self.poll_now();
    }

    fn poll_now(&mut self) {
        self.s.poll(self.now, &mut self.w);
        while let Some(e) = self.s.poll_event() {
            self.events.push(e);
        }
        for (to, bytes) in std::mem::take(&mut self.w.sent) {
            let msg = Msg::decode(&bytes).expect("session 送出的封包必須是合法的");
            if let Msg::Input {
                start_frame,
                inputs,
                ..
            } = &msg
                && !inputs.is_empty()
            {
                let last = start_frame + inputs.len() as u32 - 1;
                self.max_sent_input = Some(self.max_sent_input.map_or(last, |m| m.max(last)));
            }
            self.out.push((to, msg));
        }
    }

    /// 直接前進虛擬時間（不 poll），用來測「過了 N 秒之後」。
    fn advance_time(&mut self, by: Duration) {
        self.now += by;
    }

    /// 讓連線閒置 `secs` 秒：每 500 ms 由對方送一個 Ping（保持連線）。
    fn idle_for(&mut self, secs: u64) {
        for _ in 0..(secs * 2) {
            self.advance_time(ms(500));
            self.feed(Msg::Ping {
                session_id: SID,
                timestamp_us: 0,
            });
            self.poll_now();
        }
    }

    fn ended_with(&self) -> Option<EndReason> {
        self.s.end_reason()
    }

    fn violation(&self) -> Option<Violation> {
        match self.s.end_reason() {
            Some(EndReason::ProtocolViolation(v)) => Some(v),
            _ => None,
        }
    }

    fn desync_events(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, Event::Desync { .. }))
            .count()
    }

    fn sent_a_disconnect(&self) -> bool {
        self.out.iter().any(|(_, m)| {
            matches!(
                m,
                Msg::Disconnect {
                    reason: DisconnectReason::Left,
                    ..
                }
            )
        })
    }

    fn input_msg(start_frame: u32, inputs: Vec<PlayerInput>) -> Msg {
        Msg::Input {
            session_id: SID,
            start_frame,
            inputs,
            sender_frame: start_frame.min(100),
            frame_advantage: 0,
            confirmed: None,
        }
    }

    /// 合法的對方推進 `frames` 幀：每幀送一個輸入（可選：確認我們送過的輸入），我們用它模擬一幀。
    /// 記錄我們看到的「對方按鍵」（＝玩家 2），呼叫端可以比對它們是不是對方真正送的。
    fn drive(&mut self, frames: u32, ack: bool) {
        for _ in 0..frames {
            if !self.s.is_running() {
                return;
            }
            let f = self.next_peer_frame;
            if ack && let Some(m) = self.max_sent_input {
                self.feed(Msg::Ack {
                    session_id: SID,
                    frame: m,
                });
            }
            self.feed(Rig::input_msg(f, vec![peer_in(f)]));
            self.step();
            let my = PlayerInput::from(Buttons::from_bits_truncate(self.samples as u8));
            match self.mode {
                Mode::Lockstep => {
                    if self.s.local_input_wanted() {
                        self.s.add_local_input(my);
                        self.samples += 1;
                    }
                    if let Some(input) = self.s.next_ready_frame(self.now) {
                        self.peer_buttons_seen.push(input.p2);
                        self.s.frame_done(|| 0);
                    }
                }
                Mode::Rollback => {
                    let plan = self.s.advance(self.now, my);
                    if plan.sampled.is_some() {
                        self.samples += 1;
                    }
                    for r in &plan.requests {
                        if let Request::SaveState { frame } = *r {
                            self.s.state_saved(frame, u64::from(frame));
                        }
                    }
                    for c in self.s.drain_confirmed() {
                        self.peer_buttons_seen.push(c.input.p2);
                    }
                }
            }
            self.next_peer_frame += 1;
        }
    }

    /// `peer_buttons_seen` 裡的每一幀都必須是對方真正送的輸入（沒有被任何惡意封包改掉）。
    fn assert_only_honest_inputs_were_applied(&self) {
        assert!(!self.peer_buttons_seen.is_empty(), "測試本身應該推進過幀");
        for (f, seen) in self.peer_buttons_seen.iter().enumerate() {
            assert_eq!(
                *seen,
                peer_in(f as u32).buttons,
                "第 {f} 幀套用了不是對方真正送出的輸入"
            );
        }
    }
}

fn accept(mode: Mode) -> Msg {
    Msg::Accept {
        session_id: SID,
        input_delay: 2,
        player: 1,
        mode,
    }
}

const MODES: [Mode; 2] = [Mode::Lockstep, Mode::Rollback];

// ---- 1. 幀號遠超出合理範圍 -----------------------------------------------------------

#[test]
fn an_input_frame_far_in_the_future_aborts_the_connection() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        rig.feed(Rig::input_msg(1_000_000, vec![PlayerInput::NONE]));
        rig.step();
        assert_eq!(
            rig.violation(),
            Some(Violation::InputFrameTooFar { frame: 1_000_000 }),
            "{mode}"
        );
        assert!(rig.s.is_ended());
        assert!(rig.sent_a_disconnect(), "{mode}：要通知對方");
        assert_eq!(rig.desync_events(), 0, "{mode}：中止連線不是 desync");
        assert!(matches!(
            rig.events.last(),
            Some(Event::Disconnected {
                reason: EndReason::ProtocolViolation(_),
                ..
            })
        ));
        assert!(
            rig.ended_with().unwrap().to_string().contains("不合協定"),
            "給使用者看的文字要說得清楚"
        );
    }
}

#[test]
fn the_future_limit_is_exactly_256_frames_beyond_what_was_received_contiguously() {
    for mode in MODES {
        let mut ok = Rig::host(mode);
        ok.feed(Rig::input_msg(255, vec![peer_in(255)]));
        ok.step();
        assert!(ok.s.is_running(), "{mode}：領先 255 幀還在容許範圍內");

        let mut bad = Rig::host(mode);
        bad.feed(Rig::input_msg(256, vec![peer_in(256)]));
        bad.step();
        assert_eq!(
            bad.violation(),
            Some(Violation::InputFrameTooFar { frame: 256 }),
            "{mode}"
        );
    }
}

#[test]
fn a_frame_number_that_overflows_aborts_instead_of_wrapping() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        rig.feed(Rig::input_msg(
            u32::MAX - 3,
            vec![PlayerInput::NONE; MAX_INPUTS_PER_PACKET],
        ));
        rig.step();
        assert!(
            matches!(rig.violation(), Some(Violation::InputFrameTooFar { .. })),
            "{mode}：{:?}",
            rig.ended_with()
        );
    }
}

#[test]
fn an_input_frame_far_in_the_past_is_ignored_safely() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        rig.drive(700, true);
        assert!(rig.s.is_running(), "{mode}：{:?}", rig.ended_with());
        let ignored_before = rig.s.stats().packets_ignored;
        let frames_before = rig.s.frames_completed();
        // 第 0 幀：比保留的歷史（256 幀）還舊，無法比對，安全地忽略——不改變任何東西。
        rig.feed(Rig::input_msg(0, vec![PlayerInput::from(Buttons::all())]));
        rig.step();
        assert!(rig.s.is_running(), "{mode}");
        assert_eq!(rig.s.stats().packets_ignored, ignored_before + 1, "{mode}");
        assert_eq!(rig.s.frames_completed(), frames_before, "{mode}");
        // 之後照常推進，套用的都是對方真正送的輸入。
        rig.drive(30, true);
        assert!(rig.s.frames_completed() > frames_before, "{mode}");
        assert_eq!(rig.desync_events(), 0);
        rig.assert_only_honest_inputs_were_applied();
    }
}

#[test]
fn an_ack_for_a_frame_we_never_sent_aborts_the_connection() {
    for mode in MODES {
        for frame in [500, u32::MAX] {
            let mut rig = Rig::host(mode);
            rig.feed(Msg::Ack {
                session_id: SID,
                frame,
            });
            rig.step();
            assert_eq!(
                rig.violation(),
                Some(Violation::AckBeyondSent { frame }),
                "{mode}"
            );
        }
    }
}

#[test]
fn a_sender_frame_far_in_the_future_aborts_instead_of_poisoning_time_sync() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        rig.feed(Msg::Input {
            session_id: SID,
            start_frame: 0,
            inputs: vec![peer_in(0)],
            sender_frame: u32::MAX,
            frame_advantage: i8::MAX,
            confirmed: None,
        });
        rig.step();
        assert_eq!(
            rig.violation(),
            Some(Violation::SenderFrameTooFar {
                sender_frame: u32::MAX
            }),
            "{mode}"
        );
    }
}

#[test]
fn a_pong_from_the_future_or_with_an_absurd_rtt_does_not_touch_the_ping() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        rig.feed(Msg::Pong {
            session_id: SID,
            timestamp_us: 3_600_000_000, // 一小時後
        });
        rig.step();
        assert_eq!(rig.s.stats().rtt, None, "{mode}");
        rig.advance_time(Duration::from_secs(60));
        rig.feed(Msg::Pong {
            session_id: SID,
            timestamp_us: 1, // 六十秒前
        });
        rig.poll_now();
        assert_eq!(rig.s.stats().rtt, None, "{mode}：往返 60 秒不可信");
        assert!(rig.s.stats().packets_ignored >= 2);
    }
}

// ---- 2. 冗餘輸入的數量超出上限 ---------------------------------------------------------

#[test]
fn more_redundant_inputs_than_the_limit_abort_the_connection() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        let too_many = MAX_INPUTS_PER_PACKET + 1;
        let msg = Rig::input_msg(0, vec![PlayerInput::NONE; too_many]);
        // 嚴格的 `decode` 仍然拒絕它（4b 的行為）……
        assert_eq!(
            Msg::decode(&msg.encode().unwrap()),
            Err(ProtocolError::TooManyInputs(too_many))
        );
        // ……但 session 在確認 session_id 之後把它當作違規，而不是像雜訊一樣默默丟掉。
        rig.feed(msg);
        rig.step();
        assert_eq!(
            rig.violation(),
            Some(Violation::TooManyInputs { count: too_many }),
            "{mode}"
        );
        assert!(rig.sent_a_disconnect());
        assert_eq!(rig.desync_events(), 0);
    }
}

#[test]
fn exactly_the_limit_is_fine() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        let inputs: Vec<PlayerInput> = (0..MAX_INPUTS_PER_PACKET as u32).map(peer_in).collect();
        rig.feed(Rig::input_msg(0, inputs));
        rig.step();
        assert!(rig.s.is_running(), "{mode}：{:?}", rig.ended_with());
    }
}

#[test]
fn an_oversized_input_packet_with_a_foreign_session_id_is_just_ignored() {
    // 沒有正確的 session_id 就不是「對方」：不因為它而中止連線（避免路過的封包能殺掉連線）。
    for mode in MODES {
        let mut rig = Rig::host(mode);
        let mut msg = Rig::input_msg(0, vec![PlayerInput::NONE; MAX_INPUTS_PER_PACKET + 5]);
        if let Msg::Input { session_id, .. } = &mut msg {
            *session_id = SID ^ 1;
        }
        rig.feed(msg);
        rig.step();
        assert!(rig.s.is_running(), "{mode}");
    }
}

// ---- 3. 同一個已確認的幀，收到與先前不同的輸入 -----------------------------------------------

#[test]
fn a_different_input_for_a_frame_already_received_aborts_and_is_not_applied() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        rig.feed(Rig::input_msg(0, vec![peer_in(0), peer_in(1), peer_in(2)]));
        rig.step();
        assert!(rig.s.is_running());
        // 第 1 幀改成別的值（peer_in(1) 的位元反相）。
        let lie = PlayerInput::from(Buttons::from_bits_truncate(!peer_in(1).buttons.bits()));
        assert_ne!(lie, peer_in(1));
        rig.feed(Rig::input_msg(1, vec![lie]));
        rig.step();
        assert_eq!(
            rig.violation(),
            Some(Violation::ConflictingInput { frame: 1 }),
            "{mode}"
        );
        assert_eq!(rig.desync_events(), 0, "{mode}：抓到它是為了避免 desync");
        assert!(rig.sent_a_disconnect());
    }
}

#[test]
fn a_different_input_for_a_frame_that_was_already_simulated_aborts_and_the_simulation_stays_honest()
{
    for mode in MODES {
        let mut rig = Rig::host(mode);
        rig.drive(50, true);
        assert!(rig.s.is_running());
        let simulated = rig.peer_buttons_seen.len();
        assert!(simulated > 30, "{mode}：{simulated}");
        // 第 10 幀早就用過了；對方（或有人冒充）現在說它其實是別的輸入。
        let lie = PlayerInput::from(Buttons::from_bits_truncate(!peer_in(10).buttons.bits()));
        rig.feed(Rig::input_msg(10, vec![lie]));
        rig.step();
        assert_eq!(
            rig.violation(),
            Some(Violation::ConflictingInput { frame: 10 }),
            "{mode}"
        );
        assert_eq!(rig.desync_events(), 0);
        // 已經模擬過的每一幀，套用的都是對方最初送的輸入。
        rig.assert_only_honest_inputs_were_applied();
        // 結束之後不再推進。
        assert!(rig.s.next_ready_frame(rig.now).is_none());
    }
}

#[test]
fn the_same_input_repeated_is_the_normal_redundancy_and_is_not_a_violation() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        rig.drive(50, true);
        for _ in 0..5 {
            // 冗餘傳送：把最近幾幀（內容不變）再送一次。
            let start = rig.next_peer_frame - 20;
            let inputs: Vec<PlayerInput> = (start..rig.next_peer_frame).map(peer_in).collect();
            rig.feed(Rig::input_msg(start, inputs));
            rig.step();
        }
        assert!(rig.s.is_running(), "{mode}：{:?}", rig.ended_with());
        assert_eq!(rig.violation(), None);
    }
}

// ---- 4. 握手完成後又收到 Accept 或 Hello -----------------------------------------------------

fn hello(rom: RomId) -> Msg {
    Msg::Hello {
        protocol_version: PROTOCOL_VERSION,
        core_behavior_version: CORE_BEHAVIOR_VERSION,
        rom_id: rom,
    }
}

fn accepts_sent(rig: &Rig) -> usize {
    rig.out
        .iter()
        .filter(|(_, m)| matches!(m, Msg::Accept { .. }))
        .count()
}

#[test]
fn the_host_answers_a_repeated_identical_hello_during_the_grace_period_only() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        assert_eq!(accepts_sent(&rig), 1);
        // Accept 掉了，Client 重送同樣的 Hello：冪等地重送 Accept，連線不受影響。
        rig.advance_time(Duration::from_secs(2));
        rig.feed(hello(rom_id()));
        rig.poll_now();
        assert!(rig.s.is_running(), "{mode}");
        assert_eq!(accepts_sent(&rig), 2, "{mode}");
        // 過了寬限期（10 秒）還在送 Hello：不是「Accept 掉了」能解釋的，違規。
        rig.idle_for(11);
        assert!(rig.s.is_running(), "{mode}：{:?}", rig.ended_with());
        rig.feed(hello(rom_id()));
        rig.poll_now();
        assert_eq!(
            rig.violation(),
            Some(Violation::HandshakeAfterRunning { what: "Hello" }),
            "{mode}"
        );
    }
}

#[test]
fn a_hello_with_different_content_after_the_handshake_aborts() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        rig.feed(hello(RomId::of_file(b"some other rom")));
        rig.step();
        assert_eq!(
            rig.violation(),
            Some(Violation::HandshakeAfterRunning { what: "Hello" }),
            "{mode}"
        );
        assert!(rig.sent_a_disconnect());
    }
}

#[test]
fn a_host_that_receives_an_accept_after_the_handshake_aborts() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        rig.feed(accept(mode));
        rig.step();
        assert_eq!(
            rig.violation(),
            Some(Violation::HandshakeAfterRunning { what: "Accept" }),
            "{mode}"
        );
    }
}

#[test]
fn a_client_ignores_an_identical_duplicate_accept_but_aborts_on_a_different_one() {
    for mode in MODES {
        // Hello 與 Accept 在網路上重疊：房主對每個 Hello 都回 Accept，連上之後還會再收到相同的。
        let mut rig = Rig::client(mode);
        let ignored = rig.s.stats().packets_ignored;
        rig.feed(accept(mode));
        rig.step();
        assert!(rig.s.is_running(), "{mode}");
        assert_eq!(rig.s.stats().packets_ignored, ignored + 1, "{mode}");
        // 內容不同（另一個 session_id、另一個模式、另一個玩家位置）＝不可能是同一個房主的重送。
        for other in [
            Msg::Accept {
                session_id: SID ^ 0xFF,
                input_delay: 2,
                player: 1,
                mode,
            },
            Msg::Accept {
                session_id: SID,
                input_delay: 3,
                player: 1,
                mode,
            },
            Msg::Accept {
                session_id: SID,
                input_delay: 2,
                player: 0,
                mode,
            },
        ] {
            let mut r = Rig::client(mode);
            r.feed(other);
            r.step();
            assert_eq!(
                r.violation(),
                Some(Violation::HandshakeAfterRunning { what: "Accept" }),
                "{mode}"
            );
        }
    }
}

#[test]
fn a_client_that_receives_a_hello_after_the_handshake_aborts() {
    for mode in MODES {
        let mut rig = Rig::client(mode);
        rig.feed(hello(rom_id()));
        rig.step();
        assert_eq!(
            rig.violation(),
            Some(Violation::HandshakeAfterRunning { what: "Hello" }),
            "{mode}"
        );
    }
}

// ---- 5. 來自非對方位址的封包 ---------------------------------------------------------------

#[test]
fn packets_from_a_stranger_never_touch_the_session() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        rig.drive(20, true);
        let before = (rig.s.buffer_sizes(), rig.s.frames_completed());
        let ignored = rig.s.stats().packets_ignored;
        // 帶著「正確的」session_id 也一樣：來源位址不是對方，就不是對方。
        rig.feed_stranger(Rig::input_msg(before.1 + 1, vec![PlayerInput::NONE; 30]));
        rig.feed_stranger(Msg::Disconnect {
            session_id: SID,
            reason: DisconnectReason::Left,
            frames_completed: 0,
        });
        rig.feed_stranger(Msg::Disconnect {
            session_id: SID,
            reason: DisconnectReason::Desync { frame: 1 },
            frames_completed: 0,
        });
        rig.feed_stranger(Msg::Ack {
            session_id: SID,
            frame: 0,
        });
        rig.feed_stranger(Msg::Checksum {
            session_id: SID,
            frame: 60,
            fingerprint: 1,
        });
        rig.feed_stranger(accept(mode));
        rig.step();
        assert!(rig.s.is_running(), "{mode}：{:?}", rig.ended_with());
        assert_eq!(rig.s.buffer_sizes(), before.0, "{mode}：緩衝區不得改變");
        assert_eq!(rig.s.stats().packets_ignored, ignored + 6, "{mode}");
        assert_eq!(rig.desync_events(), 0);
        // 之後對方照常推進，套用的都是對方真正送的輸入。
        rig.drive(20, true);
        assert!(rig.s.is_running());
        rig.assert_only_honest_inputs_were_applied();
    }
}

#[test]
fn a_stranger_hello_gets_room_full_but_the_reply_rate_is_limited() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        for _ in 0..500 {
            rig.feed_stranger(hello(rom_id()));
        }
        rig.step();
        assert!(rig.s.is_running(), "{mode}");
        let replies: Vec<_> = rig
            .out
            .iter()
            .filter(|(to, m)| {
                *to == Some(addr(STRANGER))
                    && matches!(
                        m,
                        Msg::Reject {
                            reason: RejectReason::RoomFull
                        }
                    )
            })
            .collect();
        let first = replies.len();
        assert!(first > 0, "{mode}：第一個敲門的要收到「房間已滿」");
        assert!(
            first <= 20,
            "{mode}：500 個 Hello 只回了 {first} 個（每秒上限 20）"
        );
        // 下一秒又可以回覆。
        rig.advance_time(Duration::from_secs(2));
        rig.feed_stranger(hello(rom_id()));
        rig.poll_now();
        let now_replies = rig
            .out
            .iter()
            .filter(|(to, m)| *to == Some(addr(STRANGER)) && matches!(m, Msg::Reject { .. }))
            .count();
        assert_eq!(now_replies, first + 1, "{mode}");
    }
}

#[test]
fn rejected_hellos_are_also_rate_limited_while_the_host_keeps_waiting() {
    let cfg = SessionConfig::host(rom_id(), 2, SID);
    let mut rig = Rig::bare(Session::new(cfg), Mode::Lockstep);
    for _ in 0..300 {
        rig.w
            .inbox
            .push(from_peer(&hello(RomId::of_file(b"wrong rom"))));
    }
    rig.step();
    let rejects = rig
        .out
        .iter()
        .filter(|(_, m)| matches!(m, Msg::Reject { .. }))
        .count();
    assert!(
        (1..=20).contains(&rejects),
        "300 個錯誤的 Hello 只回了 {rejects} 個"
    );
    assert_eq!(rig.s.status(), Status::Waiting, "仍然在等對手");
    // 正確的 Hello 之後還是連得上（限速不會讓房間壞掉）。
    rig.advance_time(Duration::from_secs(2));
    rig.w.inbox.push(from_peer(&hello(rom_id())));
    rig.poll_now();
    assert!(rig.s.is_running());
}

// ---- 6. 其他：不確認輸入的對方、事件佇列 ----------------------------------------------------

#[test]
fn a_peer_that_keeps_sending_but_never_acks_cannot_make_local_inputs_pile_up() {
    let mut rig = Rig::host(Mode::Rollback);
    rig.drive(MAX_UNACKED_INPUTS + 50, false);
    assert_eq!(rig.violation(), Some(Violation::NotAcking));
    assert!(rig.s.buffer_sizes().local_inputs <= MAX_UNACKED_INPUTS as usize + 8);
}

#[test]
fn the_event_queue_is_bounded_when_the_caller_never_drains_it() {
    for mode in MODES {
        // 握手也不經過 `Rig::step`（它會取走事件）：`Connected` 一直留在佇列裡。
        let mut rig = Rig::bare(
            Session::new(SessionConfig::host(rom_id(), 2, SID).with_mode(mode)),
            mode,
        );
        rig.w.inbox.push(from_peer(&hello(rom_id())));
        rig.s.poll(rig.now, &mut rig.w);
        assert!(rig.s.is_running());
        // 不取事件：每秒一個 Stats。
        for _ in 0..(MAX_QUEUED_EVENTS * 3) {
            rig.advance_time(Duration::from_secs(1));
            rig.feed(Msg::Ping {
                session_id: SID,
                timestamp_us: 0,
            });
            rig.s.poll(rig.now, &mut rig.w);
            rig.w.sent.clear();
        }
        let sizes = rig.s.buffer_sizes();
        assert!(
            sizes.events <= MAX_QUEUED_EVENTS,
            "{mode}：{}",
            sizes.events
        );
        // 關鍵事件（Connected）不會為了騰空間被丟掉。
        let events: Vec<Event> = std::iter::from_fn(|| rig.s.poll_event()).collect();
        assert!(
            events.iter().any(|e| matches!(e, Event::Connected { .. })),
            "{mode}"
        );
    }
}

// ---- 7. 封包洪流 ---------------------------------------------------------------------------

/// 洪流中「合法但大量」的封包：Ping／Pong、不同幀號的 Checksum、內容不變的重複 Input（rollback 還帶著
/// 不同幀號的指紋）、有效的 Ack、垃圾位元組、別的位址的 Hello／Input。每一種都想讓某個佇列無限成長。
fn flood_datagrams(i: u64, rng: &mut SplitMix64, mode: Mode) -> Vec<Datagram> {
    let mut out = Vec::new();
    // 20 個封包／毫秒 ＝ 每秒 2 萬個。
    for k in 0..20u64 {
        let n = i * 20 + k;
        let frame = 60 * (n as u32 + 1);
        out.push(match n % 8 {
            0 => from_peer(&Msg::Ping {
                session_id: SID,
                timestamp_us: 0,
            }),
            1 => from_peer(&Msg::Checksum {
                session_id: SID,
                frame,
                fingerprint: n,
            }),
            2 => from_peer(&Msg::Input {
                session_id: SID,
                start_frame: 0,
                inputs: (0..64).map(peer_in).collect(),
                sender_frame: 0,
                frame_advantage: 0,
                confirmed: (mode == Mode::Rollback).then_some(nes_net::ConfirmedFingerprint {
                    frame: frame + 1,
                    fingerprint: n,
                }),
            }),
            3 => from_peer(&Msg::Ack {
                session_id: SID,
                frame: 0,
            }),
            4 => {
                let mut junk = vec![0u8; 8 + (rng.next_u64() % 400) as usize];
                for b in &mut junk {
                    *b = rng.next_u64() as u8;
                }
                Datagram {
                    data: junk,
                    from: Some(addr(PEER)),
                    stranger: false,
                }
            }
            5 => from_stranger(&hello(rom_id())),
            6 => from_stranger(&Rig::input_msg(0, vec![PlayerInput::NONE; 64])),
            _ => from_peer(&Msg::Pong {
                session_id: SID,
                timestamp_us: n,
            }),
        });
    }
    out
}

/// 每個佇列的上限（與 `session.rs`／`rollback.rs` 的常數對應）。
struct Bounds {
    checksums: usize,
    remote_inputs: usize,
    events: usize,
    planner_fingerprints: usize,
    planner_remote_fingerprints: usize,
    planner_frames: usize,
}

const BOUNDS: Bounds = Bounds {
    checksums: 32,            // MAX_PENDING_CHECKSUMS
    remote_inputs: 256 + 256, // MAX_REMOTE_AHEAD + REMOTE_HISTORY
    events: MAX_QUEUED_EVENTS,
    planner_fingerprints: 128 + 64,  // FP_HISTORY + 尚未 drain 的幀
    planner_remote_fingerprints: 64, // MAX_PENDING_REMOTE_FPS
    planner_frames: 32,              // MAX_WINDOW
};

fn assert_bounded(rig: &Rig, what: &str) {
    let s = rig.s.buffer_sizes();
    assert!(s.local_checksums <= BOUNDS.checksums, "{what}：{s:?}");
    assert!(s.remote_checksums <= BOUNDS.checksums, "{what}：{s:?}");
    assert!(s.remote_inputs <= BOUNDS.remote_inputs, "{what}：{s:?}");
    assert!(s.events <= BOUNDS.events, "{what}：{s:?}");
    assert_eq!(
        s.outbox, 0,
        "{what}：每次 poll 結束時待送佇列必須清空 {s:?}"
    );
    assert!(
        s.planner_remote_inputs <= BOUNDS.remote_inputs,
        "{what}：{s:?}"
    );
    assert!(
        s.planner_fingerprints <= BOUNDS.planner_fingerprints,
        "{what}：{s:?}"
    );
    assert!(
        s.planner_remote_fingerprints <= BOUNDS.planner_remote_fingerprints,
        "{what}：{s:?}"
    );
    assert!(s.planner_frames <= BOUNDS.planner_frames, "{what}：{s:?}");
}

#[test]
fn a_packet_flood_keeps_every_buffer_bounded_and_the_session_alive() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        rig.drive(10, true);
        let mut rng = SplitMix64::new(0xF100D);
        let mut replies_to_strangers = 0usize;
        // 5 個虛擬秒、每秒 2 萬個封包＝ 10 萬個封包，事件故意不取走。
        for i in 0..5000u64 {
            rig.advance_time(ms(1));
            rig.w.inbox.extend(flood_datagrams(i, &mut rng, mode));
            rig.s.poll(rig.now, &mut rig.w);
            for (to, _) in rig.w.sent.drain(..) {
                if to == Some(addr(STRANGER)) {
                    replies_to_strangers += 1;
                }
            }
            assert_bounded(&rig, &format!("{mode} 第 {i} 個 poll"));
        }
        assert!(rig.s.is_running(), "{mode}：{:?}", rig.ended_with());
        // 別的位址的 Hello：每秒最多回 20 個，5 秒最多 ~100 個（不是 1 萬多個）。
        assert!(
            replies_to_strangers <= 20 * 6,
            "{mode}：對陌生人回了 {replies_to_strangers} 個封包"
        );
        assert!(rig.s.stats().packets_ignored > 10_000);
        // 洪流之後仍然正常運作：合法的對方繼續推進，套用的都是對方真正送的輸入。
        let frames = rig.s.frames_completed();
        rig.drive(30, true);
        assert!(rig.s.is_running(), "{mode}：{:?}", rig.ended_with());
        assert!(
            rig.s.frames_completed() > frames,
            "{mode}：洪流之後不再推進"
        );
        rig.assert_only_honest_inputs_were_applied();
    }
}

#[test]
fn a_single_huge_burst_is_truncated_per_poll() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        let before = rig.s.stats().packets_received;
        for i in 0..100_000u64 {
            rig.w.inbox.push(from_peer(&Msg::Ping {
                session_id: SID,
                timestamp_us: i,
            }));
        }
        rig.step();
        let handled = rig.s.stats().packets_received - before;
        assert_eq!(
            handled, MAX_DATAGRAMS_PER_POLL as u64,
            "{mode}：一次 poll 最多處理 {MAX_DATAGRAMS_PER_POLL} 個"
        );
        assert!(rig.s.stats().packets_ignored >= 100_000 - MAX_DATAGRAMS_PER_POLL as u64);
        // 回覆的 Pong 也不會超過處理的封包數。
        let pongs = rig
            .out
            .iter()
            .filter(|(_, m)| matches!(m, Msg::Pong { .. }))
            .count();
        assert!(pongs <= MAX_DATAGRAMS_PER_POLL, "{mode}：{pongs}");
        assert_bounded(&rig, &format!("{mode} 大量封包之後"));
        assert!(rig.s.is_running());
    }
}

#[test]
fn hostile_packets_of_every_kind_never_panic_and_only_end_the_session_cleanly() {
    // 隨機組合各種（合法格式的）訊息、隨機來源，session 只能「繼續」或「乾淨地結束」，不 panic。
    let mut rng = SplitMix64::new(0xBAD5EED);
    for mode in MODES {
        for _ in 0..40 {
            let mut rig = if rng.next_u64().is_multiple_of(2) {
                Rig::host(mode)
            } else {
                Rig::client(mode)
            };
            for _ in 0..200 {
                let msg = random_msg(&mut rng);
                let d = if rng.next_u64().is_multiple_of(4) {
                    from_stranger(&msg)
                } else {
                    from_peer(&msg)
                };
                rig.w.inbox.push(d);
                rig.step();
                if rig.s.is_ended() {
                    break;
                }
            }
            if let Some(reason) = rig.ended_with() {
                assert!(!reason.to_string().is_empty());
            }
            assert_bounded(&rig, "隨機訊息");
        }
    }
}

fn random_msg(rng: &mut SplitMix64) -> Msg {
    let r = |rng: &mut SplitMix64| rng.next_u64();
    match r(rng) % 9 {
        0 => hello(RomId::of_file(&r(rng).to_le_bytes())),
        1 => Msg::Accept {
            session_id: if r(rng) % 2 == 0 { SID } else { r(rng) as u32 },
            input_delay: (r(rng) % 12) as u8,
            player: (r(rng) % 3) as u8,
            mode: if r(rng) % 2 == 0 {
                Mode::Lockstep
            } else {
                Mode::Rollback
            },
        },
        2 => Msg::Reject {
            reason: RejectReason::RoomFull,
        },
        3 | 4 => Msg::Input {
            session_id: if r(rng) % 8 == 0 { r(rng) as u32 } else { SID },
            start_frame: [0, 1, 5, 255, 256, 1000, u32::MAX][(r(rng) % 7) as usize],
            inputs: (0..(r(rng) % 70))
                .map(|_| PlayerInput::from(Buttons::from_bits_truncate(r(rng) as u8)))
                .collect(),
            sender_frame: [0, 10, 100, 100_000][(r(rng) % 4) as usize],
            frame_advantage: r(rng) as i8,
            confirmed: None,
        },
        5 => Msg::Ack {
            session_id: SID,
            frame: [0, 1, 2, 3, 100, u32::MAX][(r(rng) % 6) as usize],
        },
        6 => Msg::Checksum {
            session_id: SID,
            frame: (r(rng) % 500) as u32,
            fingerprint: r(rng),
        },
        7 => Msg::Ping {
            session_id: SID,
            timestamp_us: r(rng),
        },
        _ => Msg::Pong {
            session_id: SID,
            timestamp_us: r(rng),
        },
    }
}

#[test]
fn a_connection_that_ended_by_violation_stays_ended_and_quiet() {
    for mode in MODES {
        let mut rig = Rig::host(mode);
        rig.feed(Rig::input_msg(9_999, vec![PlayerInput::NONE]));
        rig.step();
        assert!(rig.s.is_ended());
        let events = rig.events.len();
        for _ in 0..50 {
            rig.feed(Rig::input_msg(0, vec![PlayerInput::NONE]));
            rig.step();
        }
        assert_eq!(rig.events.len(), events, "{mode}：結束後不再產生事件");
        assert_eq!(rig.s.status(), Status::Ended);
        assert!(rig.s.next_ready_frame(rig.now).is_none());
        assert!(!rig.s.add_local_input(Buttons::A));
    }
}
