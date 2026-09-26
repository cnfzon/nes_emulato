//! 真實 UDP 的整合測試（**唯一使用真實時間的測試**）。
//!
//! - `hello_roundtrip_over_udp`：兩個 [`UdpTransport`] 綁在 127.0.0.1，互傳一則編碼過的 `Hello`。
//! - `two_udp_sessions_play_600_frames_and_end_with_identical_fingerprints`：兩個 `LockstepSession`
//!   經由真實的 UDP socket 連線，各跑 600 幀，兩端逐幀指紋相同，且等於離線標準答案。
//!
//! # 為什麼這裡要用真實時間、逾時為什麼這麼寬鬆
//!
//! 真實 socket 需要真實的時間才能送達，所以這裡把系統時鐘（`Instant`）當作 session 的 `now`。
//! 其餘所有測試（握手、等價性、斷線、破壞性測試）都用虛擬時鐘，完全不等待。為了不因機器負載
//! 而偶發失敗：
//!
//! - 幀**不依 60 Hz 節拍**推進，有輸入就跑（`paced = false`），整場測試只取決於 loopback 的速度
//!   （通常不到 1 秒）；
//! - 整場的逾時上限是 [`HANG_GUARD`]＝60 秒，**只用來在真的卡死時讓測試失敗而不是永遠掛著**，
//!   不是時序假設；
//! - session 的 5 秒斷線逾時只在「完全沒收到封包」時才觸發，loopback 上不會發生（若機器被卡住超過
//!   5 秒，這個測試才可能失敗；一般開發機不會）。

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use nes_core::RomId;
use nes_core::test_support::input_probe_rom;
use nes_net::sim::{Endpoint, expected_log};
use nes_net::{
    LockstepSession, Msg, SessionConfig, Transport, UdpTransport, protocol::PROTOCOL_VERSION,
};

const HANG_GUARD: Duration = Duration::from_secs(60);

fn recv_until(transport: &mut UdpTransport, want: usize) -> Vec<nes_net::Datagram> {
    let start = Instant::now();
    let mut got = Vec::new();
    while got.len() < want && start.elapsed() < HANG_GUARD {
        got.extend(transport.recv(start.elapsed()));
        std::thread::sleep(Duration::from_millis(2));
    }
    got
}

#[test]
fn hello_roundtrip_over_udp() {
    let mut host = UdpTransport::listen("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let mut client = UdpTransport::connect(host.local_addr().unwrap()).unwrap();

    let hello = Msg::Hello {
        protocol_version: PROTOCOL_VERSION,
        core_behavior_version: nes_core::CORE_BEHAVIOR_VERSION,
        rom_id: RomId::of_file(b"rom"),
    };
    client.send(Duration::ZERO, &hello.encode().unwrap());

    let received = recv_until(&mut host, 1);
    assert_eq!(received.len(), 1);
    assert_eq!(Msg::decode(&received[0].data), Ok(hello));
}

#[test]
fn two_udp_sessions_play_600_frames_and_end_with_identical_fingerprints() {
    const FRAMES: u32 = 600;
    let rom = input_probe_rom();
    let rom_id = RomId::of_file(&rom);
    let script_seed = 0xFEED;
    let delay = 2;

    let host_transport = UdpTransport::listen("127.0.0.1:0".parse().unwrap()).unwrap();
    let host_addr: SocketAddr =
        ([127, 0, 0, 1], host_transport.local_addr().unwrap().port()).into();
    let client_transport = UdpTransport::connect(host_addr).unwrap();

    let mut a = Endpoint::new(
        LockstepSession::new(SessionConfig::host(rom_id, delay, 0xABCD)),
        host_transport,
        &rom,
        script_seed,
        FRAMES,
    );
    let mut b = Endpoint::new(
        LockstepSession::new(SessionConfig::client(rom_id)),
        client_transport,
        &rom,
        script_seed,
        FRAMES,
    );

    let start = Instant::now();
    while !(a.done() && b.done()) {
        let now = start.elapsed();
        assert!(
            now < HANG_GUARD,
            "{HANG_GUARD:?} 內沒有跑完：A {} 幀、B {} 幀；事件 A {:?} / B {:?}",
            a.frames(),
            b.frames(),
            a.events,
            b.events
        );
        let before = a.frames() + b.frames();
        a.tick(now, false);
        b.tick(now, false);
        if a.frames() + b.frames() == before {
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    assert_eq!(a.frames(), FRAMES, "A 事件 {:?}", a.events);
    assert_eq!(b.frames(), FRAMES, "B 事件 {:?}", b.events);
    let (la, lb) = (a.log.as_ref().unwrap(), b.log.as_ref().unwrap());
    assert_eq!(la.fingerprints(), lb.fingerprints(), "兩端指紋必須逐幀相同");
    let expected = expected_log(&rom, script_seed, delay, FRAMES).unwrap();
    assert_eq!(
        la.fingerprints(),
        expected.fingerprints(),
        "必須等於離線重播"
    );
    assert_eq!(
        la.to_replay(FRAMES, 60).encode(),
        lb.to_replay(FRAMES, 60).encode()
    );
    println!(
        "真實 UDP 600 幀：耗時 {:?}，A stall {} 次、B stall {} 次，A 送 {} 位元組",
        start.elapsed(),
        a.session.stats().stalls,
        b.session.stats().stalls,
        a.session.stats().bytes_sent
    );
}
