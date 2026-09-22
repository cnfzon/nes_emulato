//! 整合測試：兩個 [`Transport`] 各綁在 127.0.0.1 的不同 port 上，
//! 互傳一則 `Hello` 封包並確認對方能成功收到。

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use nes_net::{Msg, Transport};

fn poll_until<F: Fn(&[Msg]) -> bool>(
    transport: &Transport,
    timeout: Duration,
    done: F,
) -> Vec<Msg> {
    let start = Instant::now();
    loop {
        let msgs = transport.poll();
        if done(&msgs) || start.elapsed() > timeout {
            return msgs;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn hello_roundtrip_over_udp() {
    let addr_a: SocketAddr = "127.0.0.1:45001".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:45002".parse().unwrap();

    let transport_a = Transport::bind(addr_a, addr_b).expect("bind A failed");
    let transport_b = Transport::bind(addr_b, addr_a).expect("bind B failed");

    let hello = Msg::Hello {
        version: 1,
        rom_hash: 0x1122_3344_5566_7788,
    };
    transport_a.send(&hello).expect("send from A failed");

    let received = poll_until(&transport_b, Duration::from_secs(2), |msgs| {
        !msgs.is_empty()
    });

    assert_eq!(received, vec![hello]);
}
