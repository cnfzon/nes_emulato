//! 包裝 `std::net::UdpSocket` 的最小傳輸層：non-blocking send/poll，不用 async。

use std::io;
use std::net::{SocketAddr, UdpSocket};

use crate::protocol::Msg;

/// 收一個封包用的緩衝區大小；比乙太網路 MTU(1500) 小，避免 IP 分片。
const RECV_BUFFER_SIZE: usize = 1200;

/// 一條指向單一對端（peer-to-peer，非 lobby/relay）的 UDP 連線。
#[derive(Debug)]
pub struct Transport {
    socket: UdpSocket,
    peer: SocketAddr,
}

impl Transport {
    /// 綁定本地位址，並記住對端位址。Socket 立刻設成 non-blocking，讓
    /// [`Transport::poll`] 可以在 emu/net 迴圈裡每幀呼叫而不會卡住。
    pub fn bind(local: SocketAddr, peer: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(local)?;
        socket.set_nonblocking(true)?;
        Ok(Self { socket, peer })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn send(&self, msg: &Msg) -> io::Result<()> {
        let bytes = msg
            .encode()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.socket.send_to(&bytes, self.peer)?;
        Ok(())
    }

    /// 把目前 socket 緩衝區裡所有等待中的封包收下來並解碼。
    ///
    /// 解碼失敗（例如收到非本協定的雜訊封包）的資料會被直接丟棄，不會讓
    /// 呼叫端 panic。
    pub fn poll(&self) -> Vec<Msg> {
        let mut out = Vec::new();
        let mut buf = [0u8; RECV_BUFFER_SIZE];
        loop {
            match self.socket.recv_from(&mut buf) {
                Ok((n, _addr)) => {
                    if let Ok(msg) = Msg::decode(&buf[..n]) {
                        out.push(msg);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        out
    }
}
