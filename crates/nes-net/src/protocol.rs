//! 網路協定 v1：封包格式與訊息。
//!
//! # 封包格式
//!
//! ```text
//! offset  size  內容
//! 0       4     magic "NESN"
//! 4       2     協定版本（little-endian u16，目前 1）
//! 6       …     訊息本體：postcard(Msg)（不得有多餘的位元組）
//! ```
//!
//! 整個封包（含標頭）最大 [`MAX_PACKET_SIZE`] = 512 位元組，遠小於乙太網路 MTU，避免 IP 分片
//! （分片的封包只要掉一片就整個掉）。解碼一律回傳 `Result`，**任何位元組序列都不會 panic**。
//!
//! # 幀號約定
//!
//! 與 replay（`nes-core` §18.3）一致：
//!
//! - **輸入的幀號 `f`（0 起算）**：第 `f + 1` 次 `run_frame` 使用的輸入。
//! - **`Checksum` 的幀號 `n`**：已完成 `n` 次 `run_frame` 之後的行為指紋
//!   （即 replay 檢查點的幀號）。
//! - **`Ack` 的幀號**：對方**已連續收到**的最高輸入幀號（連續＝從第 0 幀起沒有缺口）。
//!
//! # 為什麼 `Input` 帶「所有尚未確認的輸入」
//!
//! UDP 會掉包、亂序、重複。每個 `Input` 封包攜帶從「對方尚未 Ack 的最早一幀」起的所有本地輸入
//! （最多 [`MAX_INPUTS_PER_PACKET`] 幀），任何一個封包只要送達，就補上前面掉的幀；不需要
//! 「偵測掉包 → 要求重傳」的來回。輸入每幀只有 1 位元組，64 幀也才 64 位元組。

use nes_core::{Buttons, RomId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAGIC: [u8; 4] = *b"NESN";
/// 協定版本。改變訊息格式或語意時遞增；握手時雙方必須相同。
pub const PROTOCOL_VERSION: u16 = 1;
/// 一個封包（含標頭）的大小上限。
pub const MAX_PACKET_SIZE: usize = 512;
/// 一個 `Input` 封包最多攜帶幾幀輸入。
pub const MAX_INPUTS_PER_PACKET: usize = 64;
const HEADER_LEN: usize = MAGIC.len() + 2;

/// 拒絕連線的原因（Host → Client）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Error)]
pub enum RejectReason {
    #[error("協定版本不符：房主使用 v{host}，你使用 v{client}。請雙方使用相同版本的程式")]
    ProtocolVersion { host: u16, client: u16 },
    #[error(
        "模擬核心版本不符（房主 {host}，你 {client}）：雙方的程式版本不同，模擬結果會不一致。請雙方使用相同版本的程式"
    )]
    CoreVersion { host: u16, client: u16 },
    #[error(
        "ROM 不同：房主的 ROM 是 {}，你的是 {}。請雙方載入同一份 ROM 檔案",
        host.short(),
        client.short()
    )]
    RomMismatch { host: RomId, client: RomId },
    #[error("房間已滿：房主已經有對手了")]
    RoomFull,
}

/// `Disconnect` 訊息帶的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisconnectReason {
    /// 使用者主動中斷。
    Left,
    /// 偵測到雙方的行為指紋在第 `frame` 幀（已完成的幀數）不同。
    Desync { frame: u32 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Msg {
    /// Client → Host：握手。雙方協定版本、核心行為版本、ROM 都必須相同。
    Hello {
        protocol_version: u16,
        core_behavior_version: u16,
        rom_id: RomId,
    },
    /// Host → Client：接受連線。`player` 是分配給 Client 的玩家位置（0＝玩家 1、1＝玩家 2）。
    Accept {
        session_id: u32,
        input_delay: u8,
        player: u8,
    },
    /// Host → Client：拒絕連線。
    Reject {
        reason: RejectReason,
    },
    /// 發送者「自己那位玩家」的輸入：`inputs[i]` 是第 `start_frame + i` 幀的輸入。
    Input {
        session_id: u32,
        start_frame: u32,
        inputs: Vec<Buttons>,
    },
    /// 已連續收到對方輸入的最高幀號。
    Ack {
        session_id: u32,
        frame: u32,
    },
    /// 已完成 `frame` 幀之後的行為指紋（`Nes::behavior_fingerprint`）。
    Checksum {
        session_id: u32,
        frame: u32,
        fingerprint: u64,
    },
    /// RTT 量測；`timestamp_us` 由發送者填入（對方原樣送回，只有發送者解讀）。
    Ping {
        session_id: u32,
        timestamp_us: u64,
    },
    Pong {
        session_id: u32,
        timestamp_us: u64,
    },
    /// 結束連線。`frames_completed` 是發送者已完成的幀數：雙方據此決定要存的 replay 長度
    /// （取兩者較小值，兩份 replay 才會完全相同）。
    Disconnect {
        session_id: u32,
        reason: DisconnectReason,
        frames_completed: u32,
    },
}

impl Msg {
    /// 帶 `session_id` 的訊息回傳它；`Hello`／`Accept`／`Reject` 沒有（握手用）或不適用。
    pub fn session_id(&self) -> Option<u32> {
        match self {
            Msg::Input { session_id, .. }
            | Msg::Ack { session_id, .. }
            | Msg::Checksum { session_id, .. }
            | Msg::Ping { session_id, .. }
            | Msg::Pong { session_id, .. }
            | Msg::Disconnect { session_id, .. } => Some(*session_id),
            Msg::Hello { .. } | Msg::Accept { .. } | Msg::Reject { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProtocolError {
    #[error("封包太短（{0} 位元組），連標頭都放不下")]
    Truncated(usize),
    #[error("封包超過大小上限（{0} > {MAX_PACKET_SIZE} 位元組）")]
    TooLarge(usize),
    #[error("不是本協定的封包（magic 不符）")]
    BadMagic,
    /// 標頭的協定版本與本程式不同。握手時 Host 可以據此回覆 [`RejectReason::ProtocolVersion`]。
    #[error("協定版本 {0} 與本程式的 v{PROTOCOL_VERSION} 不同")]
    UnsupportedVersion(u16),
    #[error("訊息本體無法解碼：{0}")]
    Body(postcard::Error),
    #[error("訊息本體後面有 {0} 個多餘的位元組")]
    TrailingBytes(usize),
    #[error("Input 封包攜帶 {0} 幀，超過上限 {MAX_INPUTS_PER_PACKET}")]
    TooManyInputs(usize),
}

impl Msg {
    /// 編碼成一個封包（標頭 + postcard）。超過 [`MAX_PACKET_SIZE`] 回傳 `TooLarge`。
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        let out = postcard::to_extend(self, out).map_err(ProtocolError::Body)?;
        if out.len() > MAX_PACKET_SIZE {
            return Err(ProtocolError::TooLarge(out.len()));
        }
        Ok(out)
    }

    /// 解碼一個封包。對任意位元組都回傳 `Result`，不 panic、不做與輸入長度不成比例的配置。
    pub fn decode(bytes: &[u8]) -> Result<Msg, ProtocolError> {
        if bytes.len() > MAX_PACKET_SIZE {
            return Err(ProtocolError::TooLarge(bytes.len()));
        }
        if bytes.len() < HEADER_LEN {
            return Err(ProtocolError::Truncated(bytes.len()));
        }
        if bytes[..MAGIC.len()] != MAGIC {
            return Err(ProtocolError::BadMagic);
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }
        let (msg, rest) =
            postcard::take_from_bytes::<Msg>(&bytes[HEADER_LEN..]).map_err(ProtocolError::Body)?;
        if !rest.is_empty() {
            return Err(ProtocolError::TrailingBytes(rest.len()));
        }
        if let Msg::Input { inputs, .. } = &msg
            && inputs.len() > MAX_INPUTS_PER_PACKET
        {
            return Err(ProtocolError::TooManyInputs(inputs.len()));
        }
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::SplitMix64;

    fn rom() -> RomId {
        RomId::of_file(b"protocol test rom")
    }

    fn all_messages() -> Vec<Msg> {
        vec![
            Msg::Hello {
                protocol_version: PROTOCOL_VERSION,
                core_behavior_version: nes_core::CORE_BEHAVIOR_VERSION,
                rom_id: rom(),
            },
            Msg::Accept {
                session_id: 0xDEAD_BEEF,
                input_delay: 2,
                player: 1,
            },
            Msg::Reject {
                reason: RejectReason::ProtocolVersion { host: 1, client: 9 },
            },
            Msg::Reject {
                reason: RejectReason::CoreVersion { host: 3, client: 4 },
            },
            Msg::Reject {
                reason: RejectReason::RomMismatch {
                    host: rom(),
                    client: RomId::of_file(b"other"),
                },
            },
            Msg::Reject {
                reason: RejectReason::RoomFull,
            },
            Msg::Input {
                session_id: 7,
                start_frame: 100,
                inputs: (0..MAX_INPUTS_PER_PACKET as u8)
                    .map(Buttons::from_bits_truncate)
                    .collect(),
            },
            Msg::Input {
                session_id: 7,
                start_frame: u32::MAX,
                inputs: vec![],
            },
            Msg::Ack {
                session_id: 7,
                frame: 42,
            },
            Msg::Checksum {
                session_id: 7,
                frame: 60,
                fingerprint: 0x1234_5678_9ABC_DEF0,
            },
            Msg::Ping {
                session_id: 7,
                timestamp_us: 1_000_000,
            },
            Msg::Pong {
                session_id: 7,
                timestamp_us: u64::MAX,
            },
            Msg::Disconnect {
                session_id: 7,
                reason: DisconnectReason::Left,
                frames_completed: 1234,
            },
            Msg::Disconnect {
                session_id: 7,
                reason: DisconnectReason::Desync { frame: 120 },
                frames_completed: 130,
            },
        ]
    }

    #[test]
    fn every_message_roundtrips() {
        for msg in all_messages() {
            let bytes = msg.encode().expect("encode");
            assert!(bytes.len() <= MAX_PACKET_SIZE);
            assert_eq!(&bytes[..4], b"NESN");
            assert_eq!(Msg::decode(&bytes), Ok(msg));
        }
    }

    #[test]
    fn the_largest_input_packet_fits_in_the_size_limit_with_room_to_spare() {
        let msg = Msg::Input {
            session_id: u32::MAX,
            start_frame: u32::MAX,
            inputs: vec![Buttons::all(); MAX_INPUTS_PER_PACKET],
        };
        let len = msg.encode().unwrap().len();
        assert!(len < MAX_PACKET_SIZE / 4, "最大的 Input 封包 {len} 位元組");
    }

    #[test]
    fn packet_header_layout_is_pinned() {
        let bytes = Msg::Ack {
            session_id: 1,
            frame: 2,
        }
        .encode()
        .unwrap();
        assert_eq!(&bytes[..6], &[b'N', b'E', b'S', b'N', 1, 0]);
    }

    #[test]
    fn bad_magic_version_and_short_packets_are_errors() {
        let good = all_messages()[1].encode().unwrap();
        let mut bad_magic = good.clone();
        bad_magic[0] ^= 0xFF;
        assert_eq!(Msg::decode(&bad_magic), Err(ProtocolError::BadMagic));
        let mut bad_version = good.clone();
        bad_version[4] = 9;
        assert_eq!(
            Msg::decode(&bad_version),
            Err(ProtocolError::UnsupportedVersion(9))
        );
        for len in 0..HEADER_LEN {
            assert_eq!(
                Msg::decode(&good[..len]),
                Err(ProtocolError::Truncated(len))
            );
        }
    }

    #[test]
    fn oversized_packets_are_rejected_before_decoding() {
        let mut big = all_messages()[1].encode().unwrap();
        big.resize(MAX_PACKET_SIZE + 1, 0);
        assert_eq!(
            Msg::decode(&big),
            Err(ProtocolError::TooLarge(MAX_PACKET_SIZE + 1))
        );
        assert!(matches!(
            Msg::decode(&vec![0u8; 100_000]),
            Err(ProtocolError::TooLarge(100_000))
        ));
    }

    #[test]
    fn encoding_a_message_over_the_limit_fails_instead_of_producing_a_big_packet() {
        let msg = Msg::Input {
            session_id: 1,
            start_frame: 0,
            inputs: vec![Buttons::A; MAX_PACKET_SIZE * 2],
        };
        assert!(matches!(msg.encode(), Err(ProtocolError::TooLarge(_))));
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = all_messages()[8].encode().unwrap();
        bytes.push(0);
        assert_eq!(Msg::decode(&bytes), Err(ProtocolError::TrailingBytes(1)));
    }

    #[test]
    fn input_with_too_many_frames_is_rejected_by_the_decoder() {
        // 用 postcard 直接編碼（繞過 `encode` 的大小檢查以外的限制）：65 幀在大小上限內，但超過幀數上限。
        let msg = Msg::Input {
            session_id: 1,
            start_frame: 0,
            inputs: vec![Buttons::A; MAX_INPUTS_PER_PACKET + 1],
        };
        let bytes = msg.encode().unwrap();
        assert_eq!(
            Msg::decode(&bytes),
            Err(ProtocolError::TooManyInputs(MAX_INPUTS_PER_PACKET + 1))
        );
    }

    #[test]
    fn a_huge_claimed_length_does_not_allocate_or_panic() {
        // Input 的 `inputs` 長度前綴宣稱 2^63 個元素，實際只有幾個位元組。
        let mut bytes = Vec::from(MAGIC);
        bytes.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        bytes.push(3); // Msg::Input 的 variant 索引（Hello 0、Accept 1、Reject 2、Input 3）
        bytes.extend_from_slice(&[1, 0]); // session_id、start_frame
        bytes.extend_from_slice(&[0xFF; 9]);
        bytes.push(0x01);
        assert!(Msg::decode(&bytes).is_err());
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut rng = SplitMix64::new(0xC0FFEE);
        for _ in 0..30_000 {
            let len = (rng.next_u64() % 600) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            let _ = Msg::decode(&bytes);
        }
    }

    #[test]
    fn random_bytes_behind_a_valid_header_never_panic() {
        let mut rng = SplitMix64::new(0xBADC0DE);
        for _ in 0..30_000 {
            let len = (rng.next_u64() % 120) as usize;
            let mut bytes = Vec::from(MAGIC);
            bytes.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
            bytes.extend((0..len).map(|_| rng.next_u64() as u8));
            let _ = Msg::decode(&bytes);
        }
    }

    #[test]
    fn every_truncation_of_every_message_is_an_error_not_a_panic() {
        for msg in all_messages() {
            let bytes = msg.encode().unwrap();
            for len in 0..bytes.len() {
                assert!(
                    Msg::decode(&bytes[..len]).is_err(),
                    "{msg:?} 截短到 {len} 位元組竟然解碼成功"
                );
            }
        }
    }

    #[test]
    fn every_single_byte_mutation_never_panics() {
        for msg in all_messages() {
            let bytes = msg.encode().unwrap();
            for i in 0..bytes.len() {
                for bit in 0..8 {
                    let mut m = bytes.clone();
                    m[i] ^= 1 << bit;
                    let _ = Msg::decode(&m);
                }
            }
        }
    }

    #[test]
    fn reject_reasons_have_readable_messages() {
        let text = RejectReason::RomMismatch {
            host: rom(),
            client: RomId::of_file(b"x"),
        }
        .to_string();
        assert!(text.contains("ROM") && text.contains(&rom().short()));
        assert!(
            RejectReason::CoreVersion { host: 3, client: 4 }
                .to_string()
                .contains("核心版本")
        );
        assert!(
            RejectReason::ProtocolVersion { host: 1, client: 2 }
                .to_string()
                .contains("協定版本")
        );
        assert!(RejectReason::RoomFull.to_string().contains("已滿"));
    }
}
