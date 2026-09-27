//! Phase 4d 第 6c 項：房間已滿——第三個連線被拒絕，原本的兩端仍然「等價」地跑完。
//!
//! 用**真實的 UDP**（`127.0.0.1` 上三個 socket），因為「來自別的位址」這件事只有 `UdpTransport` 才會標記
//! （`stranger`）。和 `transport_roundtrip.rs` 一樣是少數使用真實時間的測試：幀不依 60 Hz 推進（有輸入就跑），
//! [`HANG_GUARD`] 只是卡死保護，不是時序假設。
//!
//! 流程：A（Host）與 B（Client）連上、開始對戰；**之後**才讓 C 連進來（並且另外對 Host 狂送 Hello），
//! 因為要驗證的是「已經有對手了」。C 必須收到 `Rejected(RoomFull)`；A、B 必須不受影響：
//! 兩端逐幀指紋相同、等於離線標準答案、replay 位元組相同、沒有任何 desync。

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use nes_core::RomId;
use nes_core::test_support::input_probe_rom;
use nes_net::protocol::PROTOCOL_VERSION;
use nes_net::sim::{Endpoint, expected_log};
use nes_net::{
    EndReason, Event, Mode, Msg, RejectReason, Session, SessionConfig, Transport, UdpTransport,
};

const HANG_GUARD: Duration = Duration::from_secs(60);
const FRAMES: u32 = 600;

fn third_connection_is_rejected_and_the_match_is_unaffected(mode: Mode) {
    let rom = input_probe_rom();
    let rom_id = RomId::of_file(&rom);
    let script_seed = 0xF00D;
    let delay = if mode == Mode::Rollback { 1 } else { 2 };

    let host_transport = UdpTransport::listen("127.0.0.1:0".parse().unwrap()).unwrap();
    let host_addr: SocketAddr =
        ([127, 0, 0, 1], host_transport.local_addr().unwrap().port()).into();
    let mut a = Endpoint::new(
        Session::new(SessionConfig::host(rom_id, delay, 0x4242).with_mode(mode)),
        host_transport,
        &rom,
        script_seed,
        FRAMES,
    );
    let mut b = Endpoint::new(
        Session::new(SessionConfig::client(rom_id).with_input_delay(delay)),
        UdpTransport::connect(host_addr).unwrap(),
        &rom,
        script_seed,
        FRAMES,
    );

    let start = Instant::now();
    let guard = |a: &Endpoint<UdpTransport>, b: &Endpoint<UdpTransport>, what: &str| {
        assert!(
            start.elapsed() < HANG_GUARD,
            "{what}：{HANG_GUARD:?} 內沒有完成：A {} 幀、B {} 幀；事件 A {:?} / B {:?}",
            a.frames(),
            b.frames(),
            a.events,
            b.events
        );
    };

    // 1. A 與 B 先連上（房間有人了）。
    while !(a.session.is_running() && b.session.is_running()) {
        guard(&a, &b, "握手");
        a.tick(start.elapsed(), false);
        b.tick(start.elapsed(), false);
        std::thread::sleep(Duration::from_micros(200));
    }

    // 2. C（第三個連線）：一個正常的 Client session，加上另一個 socket 狂送 Hello。
    let mut c = Session::new(SessionConfig::client(rom_id).with_input_delay(delay));
    let mut c_transport = UdpTransport::connect(host_addr).unwrap();
    let mut blaster = UdpTransport::connect(host_addr).unwrap();
    let hello = Msg::Hello {
        protocol_version: PROTOCOL_VERSION,
        core_behavior_version: nes_core::CORE_BEHAVIOR_VERSION,
        rom_id,
    }
    .encode()
    .unwrap();
    c.poll(start.elapsed(), &mut c_transport); // C 的第一個 Hello
    for _ in 0..50 {
        blaster.send(start.elapsed(), &hello);
    }

    // 3. 讓 A、B 跑完，C 等到結果。
    let mut c_events = Vec::new();
    while !(a.done() && b.done() && c.is_ended()) {
        guard(&a, &b, "對戰");
        let now = start.elapsed();
        let before = a.frames() + b.frames();
        a.tick(now, false);
        b.tick(now, false);
        c.poll(now, &mut c_transport);
        while let Some(e) = c.poll_event() {
            c_events.push(e);
        }
        // 被拒絕之後 C 還是有可能再敲門（新的 session 會再收到 RoomFull）；每一輪都補一批 Hello。
        blaster.send(now, &hello);
        if a.frames() + b.frames() == before {
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    // C：被明確拒絕，原因是「房間已滿」。
    assert_eq!(
        c.end_reason(),
        Some(EndReason::Rejected(RejectReason::RoomFull)),
        "{mode}：C 的事件 {c_events:?}"
    );
    assert!(
        c.end_reason().unwrap().to_string().contains("房間已滿"),
        "給使用者看的文字必須說「房間已滿」"
    );

    // A、B：不受影響。
    assert!(a.frames() >= FRAMES && b.frames() >= FRAMES, "{mode}");
    assert_eq!(a.exec_error, None);
    assert_eq!(b.exec_error, None);
    for (name, e) in [("A", &a), ("B", &b)] {
        assert!(
            !e.events
                .iter()
                .any(|ev| matches!(ev, Event::Desync { .. } | Event::Disconnected { .. })),
            "{mode}：{name} 的事件 {:?}",
            e.events
        );
        assert!(
            e.session.is_running(),
            "{mode}：{name} 不得被第三個連線打斷"
        );
    }
    assert!(
        a.session.stats().packets_ignored > 0,
        "{mode}：Host 應該記錄到被忽略的陌生封包"
    );
    let (la, lb) = (a.log.as_ref().unwrap(), b.log.as_ref().unwrap());
    let upto = |l: &nes_net::matchlog::MatchLog| l.fingerprints()[..=FRAMES as usize].to_vec();
    assert_eq!(upto(la), upto(lb), "{mode}：兩端指紋必須逐幀相同");
    let expected = expected_log(&rom, script_seed, delay, FRAMES).unwrap();
    assert_eq!(
        upto(la),
        expected.fingerprints(),
        "{mode}：必須等於離線重播"
    );
    assert_eq!(
        la.to_replay(FRAMES, 60).encode(),
        lb.to_replay(FRAMES, 60).encode(),
        "{mode}：replay 位元組必須相同"
    );
}

#[test]
fn a_third_connection_is_rejected_with_room_full_and_lockstep_finishes_equivalently() {
    third_connection_is_rejected_and_the_match_is_unaffected(Mode::Lockstep);
}

#[test]
fn a_third_connection_is_rejected_with_room_full_and_rollback_finishes_equivalently() {
    third_connection_is_rejected_and_the_match_is_unaffected(Mode::Rollback);
}
