//! 網路封包格式，用 postcard 編碼。
//!
//! `Input` 封包會把「最近 N 幀」的輸入一起帶上（冗餘設計），而不是只帶最新
//! 一幀：UDP 會掉包，與其偵測掉包後要求對方重傳（多一次來回、增加延遲），
//! 不如讓每個封包本身就帶著足夠的歷史，讓下一個成功送達的封包自然把缺口
//! 補上。

use nes_core::Buttons;
use serde::{Deserialize, Serialize};

/// 每個 `Input` 封包攜帶的歷史幀數。
pub const INPUT_HISTORY_LEN: usize = 8;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Msg {
    /// 連線建立時互相確認版本與 ROM 是否一致。
    Hello {
        version: u32,
        rom_hash: u64,
    },
    /// 由 host 端宣告對局參數。
    Start {
        input_delay: u8,
        local_player: u8,
    },
    /// 冗餘輸入封包：`inputs[0]` 對應 `start_frame`，`inputs[i]` 對應
    /// `start_frame + i`。
    Input {
        start_frame: u64,
        inputs: Vec<Buttons>,
    },
    /// 確認收到對方輸入到哪一幀。
    Ack {
        frame: u64,
    },
    /// 某一幀的狀態雜湊，用來偵測 desync。
    Checksum {
        frame: u64,
        hash: u64,
    },
    /// RTT 量測。
    Ping {
        t: u64,
    },
    Pong {
        t: u64,
    },
}

impl Msg {
    pub fn encode(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Msg, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Msg) {
        let bytes = msg.encode().expect("encode should succeed");
        let decoded = Msg::decode(&bytes).expect("decode should succeed");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn hello_roundtrips() {
        roundtrip(Msg::Hello {
            version: 1,
            rom_hash: 0xDEAD_BEEF,
        });
    }

    #[test]
    fn start_roundtrips() {
        roundtrip(Msg::Start {
            input_delay: 2,
            local_player: 0,
        });
    }

    #[test]
    fn input_roundtrips_with_history() {
        let inputs = (0..INPUT_HISTORY_LEN as u8)
            .map(Buttons::from_bits_truncate)
            .collect();
        roundtrip(Msg::Input {
            start_frame: 100,
            inputs,
        });
    }

    #[test]
    fn ack_roundtrips() {
        roundtrip(Msg::Ack { frame: 42 });
    }

    #[test]
    fn checksum_roundtrips() {
        roundtrip(Msg::Checksum {
            frame: 42,
            hash: 0x1234_5678_9ABC_DEF0,
        });
    }

    #[test]
    fn ping_pong_roundtrip() {
        roundtrip(Msg::Ping { t: 1000 });
        roundtrip(Msg::Pong { t: 1000 });
    }
}
