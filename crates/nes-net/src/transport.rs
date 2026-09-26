//! 傳輸層：datagram 語意（不保證送達、不保證順序、可能重複），非阻塞。
//!
//! `Transport` 只搬位元組，不認得協定；編碼／解碼在 `protocol`，握手與排程在 `session`。
//! 所有方法都帶「目前時間」`now`（由呼叫端傳入，`nes-net` 的邏輯不讀系統時間）：真實的
//! [`UdpTransport`] 用不到它，[`crate::simnet::SimulatedTransport`] 用它依虛擬時鐘計算封包
//! 的送達時間，測試因此不需要真的等待、結果完全可重現。
//!
//! 實作：
//! - [`UdpTransport`]：`std::net::UdpSocket`（non-blocking）。握手完成（[`Transport::set_peer`]）
//!   之後，來自其他位址的封包會被標記為 `stranger`。
//! - [`InMemoryTransport`]：同一個行程內的一對端點（測試用）。
//! - [`crate::simnet::SimulatedTransport`]：包在任一 transport 外層，模擬丟包、延遲、抖動、重複。

use std::collections::VecDeque;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 收到的一個 datagram。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Datagram {
    pub data: Vec<u8>,
    /// 來源位址（沒有位址概念的 transport，例如記憶體內的，是 `None`）。
    pub from: Option<SocketAddr>,
    /// 已經鎖定對端之後，來自「別人」的封包。session 只會拿它回覆「房間已滿」，其餘一律忽略。
    pub stranger: bool,
}

impl Datagram {
    pub fn from_peer(data: Vec<u8>) -> Self {
        Self {
            data,
            from: None,
            stranger: false,
        }
    }
}

pub trait Transport {
    /// 送給目前的對端。沒有對端（尚未鎖定）或送不出去時靜默丟棄——UDP 本來就不保證送達，
    /// session 有自己的重送。
    fn send(&mut self, now: Duration, data: &[u8]);

    /// 送給指定位址（Host 在握手時，對端還沒鎖定，要回覆來源）。沒有位址概念的 transport
    /// 直接當作 [`Transport::send`]。
    fn send_to(&mut self, now: Duration, _addr: SocketAddr, data: &[u8]) {
        self.send(now, data);
    }

    /// 以非阻塞方式取出所有已到達的封包。
    fn recv(&mut self, now: Duration) -> Vec<Datagram>;

    /// 鎖定對端（握手完成）。之後只接受來自這個位址的封包。預設不做事。
    fn set_peer(&mut self, _addr: SocketAddr) {}
}

impl<T: Transport + ?Sized> Transport for Box<T> {
    fn send(&mut self, now: Duration, data: &[u8]) {
        (**self).send(now, data);
    }
    fn send_to(&mut self, now: Duration, addr: SocketAddr, data: &[u8]) {
        (**self).send_to(now, addr, data);
    }
    fn recv(&mut self, now: Duration) -> Vec<Datagram> {
        (**self).recv(now)
    }
    fn set_peer(&mut self, addr: SocketAddr) {
        (**self).set_peer(addr);
    }
}

// ---- 記憶體內 -----------------------------------------------------------------

type Queue = Arc<Mutex<VecDeque<Vec<u8>>>>;

/// 同一個行程內的一對端點：A 送的 B 收得到，反之亦然。立即送達、不丟包、保持順序。
#[derive(Debug)]
pub struct InMemoryTransport {
    outgoing: Queue,
    incoming: Queue,
}

impl InMemoryTransport {
    pub fn pair() -> (Self, Self) {
        let a_to_b: Queue = Arc::default();
        let b_to_a: Queue = Arc::default();
        (
            Self {
                outgoing: a_to_b.clone(),
                incoming: b_to_a.clone(),
            },
            Self {
                outgoing: b_to_a,
                incoming: a_to_b,
            },
        )
    }
}

impl Transport for InMemoryTransport {
    fn send(&mut self, _now: Duration, data: &[u8]) {
        if let Ok(mut q) = self.outgoing.lock() {
            q.push_back(data.to_vec());
        }
    }

    fn recv(&mut self, _now: Duration) -> Vec<Datagram> {
        match self.incoming.lock() {
            Ok(mut q) => q.drain(..).map(Datagram::from_peer).collect(),
            Err(_) => Vec::new(),
        }
    }
}

// ---- UDP ----------------------------------------------------------------------

/// 收一個封包用的緩衝區：比協定的上限大，超過上限的封包會在解碼時被拒絕。
const RECV_BUFFER_SIZE: usize = 2048;
/// 一次 `recv` 最多處理幾個封包：避免有人狂灌封包時卡住 emu 執行緒。
const MAX_DATAGRAMS_PER_RECV: usize = 512;

/// `std::net::UdpSocket`（non-blocking）。
///
/// - **Client**：[`UdpTransport::connect`]——綁定任意 port，對端＝ Host 的位址。
/// - **Host**：[`UdpTransport::listen`]——綁定指定 port，對端未知；握手完成時 session 呼叫
///   [`Transport::set_peer`] 鎖定。鎖定之後只接受來自對端位址的封包，其他位址的封包標記為
///   `stranger`。
#[derive(Debug)]
pub struct UdpTransport {
    socket: UdpSocket,
    peer: Option<SocketAddr>,
}

impl UdpTransport {
    /// Host：在 `local` 上監聽（例如 `0.0.0.0:7000`；port 0 ＝ 系統挑一個，用 [`Self::local_addr`] 查詢）。
    pub fn listen(local: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(local)?;
        socket.set_nonblocking(true)?;
        Ok(Self { socket, peer: None })
    }

    /// Client：對端是 `host`。本地位址依對端的位址族選任意 port。
    pub fn connect(host: SocketAddr) -> io::Result<Self> {
        let any: SocketAddr = if host.is_ipv4() {
            (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
        };
        let socket = UdpSocket::bind(any)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            peer: Some(host),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn peer(&self) -> Option<SocketAddr> {
        self.peer
    }
}

impl Transport for UdpTransport {
    fn send(&mut self, _now: Duration, data: &[u8]) {
        if let Some(peer) = self.peer {
            let _ = self.socket.send_to(data, peer);
        }
    }

    fn send_to(&mut self, _now: Duration, addr: SocketAddr, data: &[u8]) {
        let _ = self.socket.send_to(data, addr);
    }

    fn recv(&mut self, _now: Duration) -> Vec<Datagram> {
        let mut out = Vec::new();
        let mut buf = [0u8; RECV_BUFFER_SIZE];
        for _ in 0..MAX_DATAGRAMS_PER_RECV {
            match self.socket.recv_from(&mut buf) {
                Ok((n, from)) => out.push(Datagram {
                    data: buf[..n].to_vec(),
                    from: Some(from),
                    stranger: self.peer.is_some_and(|p| p != from),
                }),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                // Windows：先前對一個已關閉的 port 送出的封包，會讓下一次 `recv_from` 回傳
                // `ConnectionReset`（ICMP port unreachable）。這不是致命錯誤（對方關掉了程式，
                // session 會靠逾時發現），必須跳過而不是中斷或結束。
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
                Err(_) => break,
            }
        }
        out
    }

    fn set_peer(&mut self, addr: SocketAddr) {
        self.peer = Some(addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Duration = Duration::ZERO;

    #[test]
    fn in_memory_pair_delivers_in_order_both_ways_and_only_once() {
        let (mut a, mut b) = InMemoryTransport::pair();
        a.send(T0, b"one");
        a.send(T0, b"two");
        b.send(T0, b"back");
        let got: Vec<Vec<u8>> = b.recv(T0).into_iter().map(|d| d.data).collect();
        assert_eq!(got, vec![b"one".to_vec(), b"two".to_vec()]);
        assert!(b.recv(T0).is_empty(), "取出之後不會再收到");
        let back: Vec<Vec<u8>> = a.recv(T0).into_iter().map(|d| d.data).collect();
        assert_eq!(back, vec![b"back".to_vec()]);
    }

    #[test]
    fn in_memory_recv_never_blocks_when_empty() {
        let (mut a, _b) = InMemoryTransport::pair();
        assert!(a.recv(T0).is_empty());
    }

    fn drain_until(t: &mut UdpTransport, want: usize) -> Vec<Datagram> {
        // 迴圈上限只是卡死保護：loopback 幾乎立刻送達。
        let mut got = Vec::new();
        for _ in 0..2000 {
            got.extend(t.recv(T0));
            if got.len() >= want {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        got
    }

    #[test]
    fn udp_locks_onto_the_peer_and_marks_strangers() {
        let mut host = UdpTransport::listen("127.0.0.1:0".parse().unwrap()).unwrap();
        let host_addr = host.local_addr().unwrap();
        let mut client = UdpTransport::connect(host_addr).unwrap();
        let mut stranger = UdpTransport::connect(host_addr).unwrap();

        // 鎖定之前，Host 收得到任何人的封包。
        client.send(T0, b"hello");
        let got = drain_until(&mut host, 1);
        assert_eq!(got.len(), 1);
        assert!(!got[0].stranger);
        let client_addr = got[0].from.unwrap();

        // 鎖定之後：Client 的封包是正常的，別人的被標記為 stranger。
        host.set_peer(client_addr);
        client.send(T0, b"from client");
        stranger.send(T0, b"from stranger");
        let got = drain_until(&mut host, 2);
        assert_eq!(got.len(), 2);
        for d in &got {
            match d.data.as_slice() {
                b"from client" => assert!(!d.stranger),
                b"from stranger" => assert!(d.stranger),
                other => panic!("意外的封包 {other:?}"),
            }
        }

        // Host → Client（鎖定後的 send 走對端位址）。
        host.send(T0, b"reply");
        let got = drain_until(&mut client, 1);
        assert_eq!(got[0].data, b"reply");
        assert!(!got[0].stranger);
    }

    #[test]
    fn udp_send_without_a_peer_is_silently_dropped() {
        let mut host = UdpTransport::listen("127.0.0.1:0".parse().unwrap()).unwrap();
        host.send(T0, b"nobody to receive this");
        assert!(host.recv(T0).is_empty());
    }

    #[test]
    fn udp_recv_survives_sending_to_a_closed_port() {
        // 對已關閉的 port 送封包（Windows 上下一次 recv 會得到 ConnectionReset）：不可 panic，
        // 之後仍能正常收封包。
        let closed = {
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            s.local_addr().unwrap()
        };
        let mut a = UdpTransport::connect(closed).unwrap();
        a.send(T0, b"into the void");
        std::thread::sleep(Duration::from_millis(50));
        let _ = a.recv(T0); // Windows：可能在這裡吃到 ConnectionReset
        // `a` 綁在 0.0.0.0:port；從 127.0.0.1 送給它，之後仍要收得到。
        let a_port = a.local_addr().unwrap().port();
        let mut sender = UdpTransport::connect(([127, 0, 0, 1], a_port).into()).unwrap();
        sender.send(T0, b"still alive");
        let got = drain_until(&mut a, 1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, b"still alive");
    }
}
