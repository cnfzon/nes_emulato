//! 握手、拒絕、逾時、斷線與 session 的邊界行為。全部用虛擬時鐘（不 sleep、不讀系統時間），
//! 結果只由設定與種子決定。

use std::net::SocketAddr;
use std::time::Duration;

use nes_core::{Buttons, CORE_BEHAVIOR_VERSION, RomId};
use nes_net::protocol::{DisconnectReason, MAX_INPUTS_PER_PACKET, PROTOCOL_VERSION};
use nes_net::sim::{MatchConfig, expected_log, run_match};
use nes_net::{
    Datagram, EndReason, Event, InMemoryTransport, LockstepSession, Msg, NetworkConfig,
    RejectReason, SessionConfig, SimulatedTransport, Status, Transport,
};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn rom_id() -> RomId {
    RomId::of_file(b"handshake test rom")
}

type Sim = SimulatedTransport<InMemoryTransport>;

/// 兩個 session（只有輸入，沒有 `Nes`）＋模擬網路＋虛擬時鐘。
struct Pair {
    host: LockstepSession,
    client: LockstepSession,
    th: Sim,
    tc: Sim,
    now: Duration,
    host_events: Vec<Event>,
    client_events: Vec<Event>,
}

impl Pair {
    fn new(host: SessionConfig, client: SessionConfig, net: NetworkConfig, seed: u64) -> Self {
        let (a, b) = InMemoryTransport::pair();
        Self {
            host: LockstepSession::new(host),
            client: LockstepSession::new(client),
            th: SimulatedTransport::new(a, net, seed * 2),
            tc: SimulatedTransport::new(b, net, seed * 2 + 1),
            now: Duration::ZERO,
            host_events: Vec::new(),
            client_events: Vec::new(),
        }
    }

    fn ok(net: NetworkConfig, seed: u64) -> Self {
        Self::new(
            SessionConfig::host(rom_id(), 2, 0x1234),
            SessionConfig::client(rom_id()),
            net,
            seed,
        )
    }

    fn step(&mut self) {
        self.host.poll(self.now, &mut self.th);
        self.client.poll(self.now, &mut self.tc);
        while let Some(e) = self.host.poll_event() {
            self.host_events.push(e);
        }
        while let Some(e) = self.client.poll_event() {
            self.client_events.push(e);
        }
        self.now += ms(1);
    }

    /// 最多跑到虛擬時間 `limit`，直到 `done` 成立；回傳是否成立。
    fn run_until(&mut self, limit: Duration, done: impl Fn(&Pair) -> bool) -> bool {
        while self.now < limit {
            self.step();
            if done(self) {
                return true;
            }
        }
        false
    }

    fn connected(&self) -> bool {
        self.host.is_running() && self.client.is_running()
    }
}

fn connected_event(events: &[Event]) -> Option<(u8, u8)> {
    events.iter().find_map(|e| match e {
        Event::Connected {
            player,
            input_delay,
        } => Some((*player, *input_delay)),
        _ => None,
    })
}

fn disconnected(events: &[Event]) -> Option<(EndReason, Option<u32>)> {
    events.iter().find_map(|e| match e {
        Event::Disconnected {
            reason,
            peer_frames,
        } => Some((*reason, *peer_frames)),
        _ => None,
    })
}

// ---- 握手 ---------------------------------------------------------------------

#[test]
fn handshake_succeeds_and_assigns_players_and_the_hosts_input_delay() {
    let mut p = Pair::new(
        SessionConfig::host(rom_id(), 5, 0x1234),
        SessionConfig::client(rom_id()),
        NetworkConfig::IDEAL,
        1,
    );
    assert!(p.run_until(ms(1000), Pair::connected));
    assert_eq!(
        connected_event(&p.host_events),
        Some((0, 5)),
        "Host 是玩家 1"
    );
    assert_eq!(
        connected_event(&p.client_events),
        Some((1, 5)),
        "Client 是玩家 2，input delay 由 Host 決定（Client 自己的預設是 2）"
    );
    assert_eq!(p.host.session_id(), 0x1234);
    assert_eq!(p.client.session_id(), 0x1234);
    assert_eq!((p.host.local_player(), p.client.local_player()), (0, 1));
}

#[test]
fn input_delay_is_clamped_to_the_maximum() {
    let mut p = Pair::new(
        SessionConfig::host(rom_id(), 200, 1),
        SessionConfig::client(rom_id()),
        NetworkConfig::IDEAL,
        1,
    );
    assert!(p.run_until(ms(1000), Pair::connected));
    assert_eq!(p.host.input_delay(), 8);
    assert_eq!(p.client.input_delay(), 8);
}

fn rejection_with(mutate_client: impl Fn(&mut SessionConfig)) -> (Pair, Option<EndReason>) {
    let mut client = SessionConfig::client(rom_id());
    mutate_client(&mut client);
    let mut p = Pair::new(
        SessionConfig::host(rom_id(), 2, 7),
        client,
        NetworkConfig::IDEAL,
        1,
    );
    p.run_until(ms(2000), |p| p.client.is_ended());
    let reason = p.client.end_reason();
    (p, reason)
}

#[test]
fn a_different_protocol_version_is_rejected_with_the_reason() {
    let (p, reason) = rejection_with(|c| c.protocol_version = PROTOCOL_VERSION + 1);
    assert_eq!(
        reason,
        Some(EndReason::Rejected(RejectReason::ProtocolVersion {
            host: PROTOCOL_VERSION,
            client: PROTOCOL_VERSION + 1
        }))
    );
    assert_eq!(p.host.status(), Status::Waiting, "Host 仍然在等別人");
    let text = reason.unwrap().to_string();
    assert!(text.contains("協定版本"), "{text}");
}

#[test]
fn a_different_core_behavior_version_is_rejected_with_the_reason() {
    let (p, reason) = rejection_with(|c| c.core_behavior_version = CORE_BEHAVIOR_VERSION + 1);
    assert_eq!(
        reason,
        Some(EndReason::Rejected(RejectReason::CoreVersion {
            host: CORE_BEHAVIOR_VERSION,
            client: CORE_BEHAVIOR_VERSION + 1
        }))
    );
    assert_eq!(p.host.status(), Status::Waiting);
    assert!(reason.unwrap().to_string().contains("核心版本"));
}

#[test]
fn a_different_rom_is_rejected_with_the_reason() {
    let other = RomId::of_file(b"some other game");
    let (p, reason) = rejection_with(|c| c.rom_id = other);
    assert_eq!(
        reason,
        Some(EndReason::Rejected(RejectReason::RomMismatch {
            host: rom_id(),
            client: other
        }))
    );
    assert_eq!(p.host.status(), Status::Waiting);
    let text = reason.unwrap().to_string();
    assert!(
        text.contains("ROM") && text.contains(&rom_id().short()) && text.contains(&other.short())
    );
    // UI 收到的是 Disconnected 事件，帶著同一個原因。
    assert_eq!(
        disconnected(&p.client_events).map(|(r, _)| r),
        reason,
        "Client 端要有 Disconnected 事件供 UI 顯示"
    );
}

#[test]
fn the_host_keeps_waiting_after_a_rejection_and_accepts_a_good_client_later() {
    let (a, b) = InMemoryTransport::pair();
    let mut th = a;
    let mut host = LockstepSession::new(SessionConfig::host(rom_id(), 2, 9));
    // 壞 client（ROM 不同）先來，被拒絕；好 client 再來（同一條線路）。
    let mut bad = SessionConfig::client(RomId::of_file(b"wrong"));
    bad.handshake_timeout = Some(ms(500));
    let mut bad_client = LockstepSession::new(bad);
    let mut tb = b;
    let mut now = Duration::ZERO;
    while now < ms(500) && !bad_client.is_ended() {
        host.poll(now, &mut th);
        bad_client.poll(now, &mut tb);
        now += ms(1);
    }
    assert!(matches!(
        bad_client.end_reason(),
        Some(EndReason::Rejected(RejectReason::RomMismatch { .. }))
    ));
    assert_eq!(host.status(), Status::Waiting);

    let mut good_client = LockstepSession::new(SessionConfig::client(rom_id()));
    while now < ms(2000) && !good_client.is_running() {
        host.poll(now, &mut th);
        good_client.poll(now, &mut tb);
        now += ms(1);
    }
    assert!(host.is_running() && good_client.is_running());
}

#[test]
fn handshake_completes_under_30_percent_loss_for_many_seeds() {
    let net = NetworkConfig {
        loss: 0.30,
        delay: ms(50),
        jitter: ms(20),
        duplicate: 0.05,
    };
    let mut slowest = Duration::ZERO;
    for seed in 0..200 {
        let mut p = Pair::ok(net, seed);
        assert!(
            p.run_until(Duration::from_secs(9), Pair::connected),
            "種子 {seed}：30% 丟包下 9 秒內沒有完成握手"
        );
        assert_eq!(connected_event(&p.host_events), Some((0, 2)));
        assert_eq!(connected_event(&p.client_events), Some((1, 2)));
        slowest = slowest.max(p.now);
    }
    println!("200 組種子、30% 丟包下握手完成，最慢 {slowest:?}");
}

#[test]
fn a_lost_accept_is_recovered_because_the_host_answers_a_repeated_hello() {
    // 只丟 Host → Client 方向的封包：Client 收不到 Accept 也持續送 Hello，Host 重送 Accept。
    let (a, b) = InMemoryTransport::pair();
    let mut th = SimulatedTransport::new(a, NetworkConfig::IDEAL, 1);
    let mut tc = SimulatedTransport::new(b, NetworkConfig::IDEAL, 2);
    let mut host = LockstepSession::new(SessionConfig::host(rom_id(), 2, 3));
    let mut client = LockstepSession::new(SessionConfig::client(rom_id()));
    th.set_config(NetworkConfig::IDEAL.blackout());
    let mut now = Duration::ZERO;
    while now < ms(1000) {
        host.poll(now, &mut th);
        client.poll(now, &mut tc);
        now += ms(1);
    }
    assert!(host.is_running(), "Host 收到 Hello 就進入 Running");
    assert!(!client.is_running(), "Client 一個 Accept 都沒收到");
    th.set_config(NetworkConfig::IDEAL);
    while now < ms(2000) && !client.is_running() {
        host.poll(now, &mut th);
        client.poll(now, &mut tc);
        now += ms(1);
    }
    assert!(
        client.is_running(),
        "網路恢復後，Host 重送的 Accept 讓 Client 連上"
    );
    assert_eq!(client.session_id(), 3);
}

#[test]
fn the_client_gives_up_after_the_handshake_timeout_with_a_readable_reason() {
    let mut p = Pair::ok(NetworkConfig::IDEAL.blackout(), 1);
    assert!(p.run_until(Duration::from_secs(11), |p| p.client.is_ended()));
    assert!(p.now >= Duration::from_secs(10) && p.now < Duration::from_secs(11));
    assert_eq!(p.client.end_reason(), Some(EndReason::HandshakeTimeout));
    let text = p.client.end_reason().unwrap().to_string();
    assert!(text.contains("逾時") && text.contains("防火牆"), "{text}");
    assert_eq!(
        disconnected(&p.client_events),
        Some((EndReason::HandshakeTimeout, None))
    );
    assert_eq!(
        p.host.status(),
        Status::Waiting,
        "Host 沒有逾時，等到使用者取消"
    );
}

#[test]
fn cancelling_while_waiting_or_connecting_ends_immediately() {
    let mut p = Pair::ok(NetworkConfig::IDEAL.blackout(), 1);
    p.step();
    p.host.disconnect(p.now);
    p.client.disconnect(p.now);
    assert_eq!(p.host.end_reason(), Some(EndReason::LocalLeft));
    assert_eq!(p.client.end_reason(), Some(EndReason::LocalLeft));
}

/// 沒有位址概念的 transport 之外，Host 已有對手時，別的位址送來 Hello 要回覆「房間已滿」。
#[test]
fn a_third_party_hello_gets_room_full() {
    struct Scripted {
        inner: InMemoryTransport,
        inject: Vec<Datagram>,
        sent_to: Vec<(SocketAddr, Vec<u8>)>,
    }
    impl Transport for Scripted {
        fn send(&mut self, now: Duration, data: &[u8]) {
            self.inner.send(now, data);
        }
        fn send_to(&mut self, _now: Duration, addr: SocketAddr, data: &[u8]) {
            self.sent_to.push((addr, data.to_vec()));
        }
        fn recv(&mut self, now: Duration) -> Vec<Datagram> {
            let mut got = self.inner.recv(now);
            got.append(&mut self.inject);
            got
        }
    }

    let (a, b) = InMemoryTransport::pair();
    let mut th = Scripted {
        inner: a,
        inject: Vec::new(),
        sent_to: Vec::new(),
    };
    let mut tc = b;
    let mut host = LockstepSession::new(SessionConfig::host(rom_id(), 2, 1));
    let mut client = LockstepSession::new(SessionConfig::client(rom_id()));
    let mut now = Duration::ZERO;
    while now < ms(500) && !(host.is_running() && client.is_running()) {
        host.poll(now, &mut th);
        client.poll(now, &mut tc);
        now += ms(1);
    }
    assert!(host.is_running());
    th.sent_to.clear();

    let stranger: SocketAddr = "203.0.113.9:4000".parse().unwrap();
    let hello = Msg::Hello {
        protocol_version: PROTOCOL_VERSION,
        core_behavior_version: CORE_BEHAVIOR_VERSION,
        rom_id: rom_id(),
    };
    th.inject.push(Datagram {
        data: hello.encode().unwrap(),
        from: Some(stranger),
        stranger: true,
    });
    // 陌生人的其他封包（Input／Disconnect）一律忽略，不影響連線。
    th.inject.push(Datagram {
        data: Msg::Disconnect {
            session_id: 1,
            reason: DisconnectReason::Left,
            frames_completed: 0,
        }
        .encode()
        .unwrap(),
        from: Some(stranger),
        stranger: true,
    });
    host.poll(now, &mut th);
    assert!(host.is_running(), "陌生人的 Disconnect 不能中斷連線");
    assert_eq!(th.sent_to.len(), 1);
    assert_eq!(th.sent_to[0].0, stranger);
    assert_eq!(
        Msg::decode(&th.sent_to[0].1),
        Ok(Msg::Reject {
            reason: RejectReason::RoomFull
        })
    );
}

/// 對方用不同版本的協定（標頭的版本號不同，本體可能完全不同）打招呼：Host 也要回覆明確的原因。
#[test]
fn a_hello_with_a_different_wire_version_is_rejected_readably() {
    let (a, mut b) = InMemoryTransport::pair();
    let mut th = a;
    let mut host = LockstepSession::new(SessionConfig::host(rom_id(), 2, 1));
    let mut bytes = Msg::Hello {
        protocol_version: PROTOCOL_VERSION,
        core_behavior_version: CORE_BEHAVIOR_VERSION,
        rom_id: rom_id(),
    }
    .encode()
    .unwrap();
    bytes[4] = 9; // 標頭版本號
    b.send(Duration::ZERO, &bytes);
    host.poll(Duration::ZERO, &mut th);
    let replies = b.recv(Duration::ZERO);
    assert_eq!(replies.len(), 1);
    assert_eq!(
        Msg::decode(&replies[0].data),
        Ok(Msg::Reject {
            reason: RejectReason::ProtocolVersion {
                host: PROTOCOL_VERSION,
                client: 9
            }
        })
    );
    assert_eq!(host.status(), Status::Waiting);
}

// ---- 垃圾封包、session_id、輸入的邊界 --------------------------------------------------

#[test]
fn garbage_and_foreign_session_packets_are_ignored() {
    let mut p = Pair::ok(NetworkConfig::IDEAL, 1);
    assert!(p.run_until(ms(500), Pair::connected));
    // 直接對 Host 的收件匣灌垃圾（透過 Client 那一端的 transport 送出）。
    for junk in [
        vec![],
        vec![0u8; 3],
        b"NESN\x01\x00".to_vec(),
        vec![0xFF; 600],
        b"GET / HTTP/1.1\r\n\r\n".to_vec(),
    ] {
        p.tc.send(p.now, &junk);
    }
    // 不同 session_id 的 Disconnect／Input／Checksum：丟棄。
    for msg in [
        Msg::Disconnect {
            session_id: 0xBAD,
            reason: DisconnectReason::Left,
            frames_completed: 0,
        },
        Msg::Input {
            session_id: 0xBAD,
            start_frame: 0,
            inputs: vec![Buttons::all(); 4],
        },
        Msg::Checksum {
            session_id: 0xBAD,
            frame: 60,
            fingerprint: 1,
        },
    ] {
        p.tc.send(p.now, &msg.encode().unwrap());
    }
    for _ in 0..50 {
        p.step();
    }
    assert!(
        p.connected(),
        "垃圾封包與不同 session_id 的封包不能影響連線"
    );
    assert_eq!(p.host.end_reason(), None);
}

#[test]
fn a_hostile_input_packet_cannot_grow_memory_or_overflow() {
    let mut p = Pair::ok(NetworkConfig::IDEAL, 1);
    assert!(p.run_until(ms(500), Pair::connected));
    let sid = p.host.session_id();
    for (start, count) in [
        (u32::MAX - 3, MAX_INPUTS_PER_PACKET), // 幀號溢位
        (1_000_000, MAX_INPUTS_PER_PACKET),    // 太遠
        (0, MAX_INPUTS_PER_PACKET),
    ] {
        let msg = Msg::Input {
            session_id: sid,
            start_frame: start,
            inputs: vec![Buttons::all(); count],
        };
        p.tc.send(p.now, &msg.encode().unwrap());
    }
    // 對不存在的幀 Ack：不能讓 acked 超過本地輸入。
    p.tc.send(
        p.now,
        &Msg::Ack {
            session_id: sid,
            frame: u32::MAX,
        }
        .encode()
        .unwrap(),
    );
    for _ in 0..20 {
        p.step();
    }
    assert!(p.connected());
    // Host 仍然能正常取樣（沒有被亂 Ack 弄壞內部狀態）。
    assert!(p.host.local_input_wanted());
}

#[test]
fn local_input_is_only_accepted_once_per_frame_so_stalls_cannot_pile_it_up() {
    let mut p = Pair::ok(NetworkConfig::IDEAL.blackout(), 1);
    // 沒有連上之前不接受輸入。
    assert!(!p.host.add_local_input(Buttons::A));
    // 直接讓 Host 連上（用另一組理想網路），然後只餵輸入不推進幀。
    let mut q = Pair::ok(NetworkConfig::IDEAL, 1);
    assert!(q.run_until(ms(500), Pair::connected));
    let mut accepted = 0;
    for _ in 0..100 {
        if q.host.add_local_input(Buttons::A) {
            accepted += 1;
        }
    }
    assert_eq!(
        accepted, 1,
        "沒有推進幀，只能取樣一次（D 幀的預填輸入之外再一個）"
    );
    p.step();
}

// ---- stall 統計 ---------------------------------------------------------------

#[test]
fn a_stall_is_counted_once_and_timed_until_the_input_arrives() {
    let mut p = Pair::ok(NetworkConfig::IDEAL, 1);
    assert!(p.run_until(ms(500), Pair::connected));
    for _ in 0..10 {
        p.step(); // 讓雙方預填的空輸入互相送到
    }
    // 只驅動 Host（Client 不再取樣、不推進）。Host 有 Client 預填的 D 幀輸入，所以能推進 D 幀。
    let mut now = p.now;
    let mut frames = 0;
    loop {
        p.host.poll(now, &mut p.th);
        if p.host.local_input_wanted() {
            p.host.add_local_input(Buttons::empty());
        }
        if p.host.next_ready_frame(now).is_none() {
            break;
        }
        p.host.frame_done(|| 0);
        frames += 1;
        now += ms(16);
    }
    assert_eq!(frames, 2);
    assert_eq!(p.host.stats().stalls, 1);
    // 一直等不到：仍然只算一次；stall 結束之前時間不累計。
    for i in 1..=10 {
        assert!(p.host.next_ready_frame(now + ms(i * 10)).is_none());
    }
    assert_eq!(p.host.stats().stalls, 1, "連續等不到只算一次 stall");
    assert!(p.host.stats().stall_time.is_zero());
    let events: Vec<Event> = std::iter::from_fn(|| p.host.poll_event()).collect();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Stalled { frame: 2 }))
            .count(),
        1
    );

    // Client 終於送來輸入：stall 結束，累計 stall 時間、送出 Resumed。
    let resume_at = now + ms(250);
    p.client.add_local_input(Buttons::empty());
    p.client.next_ready_frame(resume_at); // Client 自己也可能在等（不影響本測試）
    for _ in 0..5 {
        p.client.poll(resume_at, &mut p.tc);
    }
    p.host.poll(resume_at, &mut p.th);
    assert!(p.host.next_ready_frame(resume_at).is_some());
    let stats = p.host.stats();
    assert_eq!(stats.stalls, 1);
    assert_eq!(stats.stall_time, ms(250));
    assert!(matches!(
        p.host.poll_event(),
        Some(Event::Resumed { frame: 2, waited }) if waited == ms(250)
    ));
}

// ---- 斷線 ---------------------------------------------------------------------

#[test]
fn both_ends_report_a_timeout_after_the_network_goes_dead() {
    let frames = 1200;
    let rom = nes_core::test_support::input_probe_rom();
    let expected = expected_log(&rom, 0x5EED, 2, frames).unwrap();
    for seed in 0..5 {
        let cfg = MatchConfig {
            frames,
            seed,
            network: NetworkConfig {
                loss: 0.05,
                delay: ms(20),
                jitter: ms(5),
                duplicate: 0.0,
            },
            blackout_at: Some(Duration::from_secs(3)),
            ..MatchConfig::default()
        };
        let r = run_match(&rom, &cfg, &expected).unwrap();
        assert!(!r.timed_out);
        for (i, e) in r.ends.iter().enumerate() {
            assert_eq!(
                e.end_reason,
                Some(EndReason::Timeout),
                "種子 {seed} 端點 {i}：事件 {:?}",
                e.events
            );
            let (reason, peer_frames) = disconnected(&e.events).expect("必須有 Disconnected 事件");
            assert_eq!((reason, peer_frames), (EndReason::Timeout, None));
            assert!(
                e.frames > 0 && e.frames < frames,
                "中途斷線：{} 幀",
                e.frames
            );
        }
        // 斷線發生在「丟棄所有封包」之後至少 5 秒（最後一個收到的封包在斷線點之前）。
        let elapsed_after = r.virtual_elapsed.saturating_sub(Duration::from_secs(3));
        assert!(
            elapsed_after >= Duration::from_secs(5) && elapsed_after < Duration::from_secs(7),
            "斷線發生在斷網後 {elapsed_after:?}"
        );
        // 斷線前雙方走過的幀，指紋仍然一致。
        assert_eq!(r.a_vs_b, first_len_diff(&r), "斷線前的幀兩端一致");
    }
}

/// 兩端長度可能差幾幀（各自停在不同的地方）；比較共同的前綴。
fn first_len_diff(r: &nes_net::sim::MatchReport) -> Option<u32> {
    let (a, b) = (r.ends[0].log.fingerprints(), r.ends[1].log.fingerprints());
    let common = a.len().min(b.len());
    if a[..common] == b[..common] {
        // 前綴相同、只有長度不同：run_match 回報的是較短者的長度。
        (a.len() != b.len()).then_some(common as u32)
    } else {
        a.iter().zip(b).position(|(x, y)| x != y).map(|i| i as u32)
    }
}

#[test]
fn a_graceful_disconnect_tells_the_peer_and_exchanges_frame_counts() {
    let mut p = Pair::ok(NetworkConfig::IDEAL, 1);
    assert!(p.run_until(ms(500), Pair::connected));
    p.host.disconnect(p.now);
    assert_eq!(p.host.status(), Status::Closing, "等對方回覆它的幀數");
    assert!(p.run_until(p.now + ms(500), |p| p.host.is_ended()
        && p.client.is_ended()));
    assert_eq!(
        disconnected(&p.host_events),
        Some((EndReason::LocalLeft, Some(0)))
    );
    assert_eq!(
        disconnected(&p.client_events),
        Some((EndReason::PeerLeft, Some(0)))
    );
    assert!(p.now < ms(1000), "有回覆時不必等滿 CLOSING_GRACE");
}

#[test]
fn a_graceful_disconnect_still_finishes_if_the_peer_never_answers() {
    let mut p = Pair::ok(NetworkConfig::IDEAL, 1);
    assert!(p.run_until(ms(500), Pair::connected));
    p.tc.set_config(NetworkConfig::IDEAL.blackout()); // Client 的回覆到不了
    p.host.disconnect(p.now);
    assert!(p.run_until(p.now + Duration::from_secs(3), |p| p.host.is_ended()));
    assert_eq!(
        disconnected(&p.host_events),
        Some((EndReason::LocalLeft, None))
    );
}

#[test]
fn a_session_never_panics_or_progresses_after_it_has_ended() {
    let mut p = Pair::ok(NetworkConfig::IDEAL, 1);
    assert!(p.run_until(ms(500), Pair::connected));
    p.client.disconnect(p.now);
    assert!(p.run_until(p.now + ms(500), |p| p.client.is_ended()
        && p.host.is_ended()));
    for _ in 0..100 {
        p.step();
    }
    assert!(p.host.next_ready_frame(p.now).is_none());
    assert!(!p.host.add_local_input(Buttons::A));
    let events_before = p.host_events.len();
    p.step();
    assert_eq!(p.host_events.len(), events_before, "結束後不再產生事件");
}
