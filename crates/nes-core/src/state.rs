//! 存檔（save state）的編碼／解碼。
//!
//! 用 postcard 是因為它是 no_std 友善、緊湊、決定性的二進位格式（同一份輸入
//! 永遠編碼出同一份位元組），這對 rollback netplay 的 checksum 比對很重要。
//!
//! # 檔案佈局
//!
//! ```text
//! offset 0..4   magic          "NESS"
//! offset 4..6   STATE_FORMAT_VERSION   （u16，little-endian）
//! offset 6..8   CORE_BEHAVIOR_VERSION  （u16，little-endian）
//! offset 8..    postcard 編碼的 `Nes`
//! ```
//!
//! # 兩個版本號
//!
//! - [`STATE_FORMAT_VERSION`]：**格式結構**。存檔裡有哪些欄位、順序、型別；
//!   任何會改變 postcard 編碼結果的型別修改（增減欄位、調整 enum 變體順序……）
//!   都必須遞增。
//! - [`CORE_BEHAVIOR_VERSION`]：**模擬行為**。同一份 ROM + 同一串輸入，核心算出
//!   的狀態／畫面不同（時序、mapper 行為、CPU/PPU 邏輯、開機初值……）就必須遞增。
//!   Phase 4 的 replay 格式與 netplay 握手會沿用這個版本號：版本不同的兩端不能
//!   對戰、舊 replay 不保證能重播。判定規則見 `docs/architecture.md` §15。

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::StateError;

/// 存檔開頭的 magic bytes。
pub const STATE_MAGIC: [u8; 4] = *b"NESS";
/// 存檔的格式結構版本；見模組文件。
pub const STATE_FORMAT_VERSION: u16 = 1;
/// 模擬行為版本；見模組文件。Phase 3 凍結核心時定為 1；Phase 3.1（索引定址 dummy read、
/// RMW 一律寫兩次）遞增為 2。
pub const CORE_BEHAVIOR_VERSION: u16 = 2;

/// 存檔 header 的長度（magic + 兩個 u16）。
pub const HEADER_LEN: usize = 8;

/// 存檔 header 的內容；`StateError::VersionMismatch` 用它同時表達
/// 「magic 不符」與「任一版本號不符」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateHeader {
    pub magic: [u8; 4],
    pub format_version: u16,
    pub core_version: u16,
}

impl StateHeader {
    /// 這份程式碼寫出、也只接受的 header。
    pub const CURRENT: StateHeader = StateHeader {
        magic: STATE_MAGIC,
        format_version: STATE_FORMAT_VERSION,
        core_version: CORE_BEHAVIOR_VERSION,
    };

    fn to_bytes(self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&self.magic);
        out[4..6].copy_from_slice(&self.format_version.to_le_bytes());
        out[6..8].copy_from_slice(&self.core_version.to_le_bytes());
        out
    }

    fn from_bytes(bytes: &[u8; HEADER_LEN]) -> Self {
        StateHeader {
            magic: [bytes[0], bytes[1], bytes[2], bytes[3]],
            format_version: u16::from_le_bytes([bytes[4], bytes[5]]),
            core_version: u16::from_le_bytes([bytes[6], bytes[7]]),
        }
    }
}

impl fmt::Display for StateHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "magic {:?} / 格式版本 {} / 核心行為版本 {}",
            String::from_utf8_lossy(&self.magic),
            self.format_version,
            self.core_version
        )
    }
}

pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    let body = postcard::to_allocvec(value).expect("記憶體內的 NES 狀態必定可以序列化");
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(&StateHeader::CURRENT.to_bytes());
    out.extend_from_slice(&body);
    out
}

pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, StateError> {
    let Some((header, body)) = bytes.split_first_chunk::<HEADER_LEN>() else {
        return Err(StateError::Decode(
            postcard::Error::DeserializeUnexpectedEnd,
        ));
    };
    let found = StateHeader::from_bytes(header);
    if found != StateHeader::CURRENT {
        return Err(StateError::VersionMismatch {
            expected: StateHeader::CURRENT,
            found,
        });
    }
    postcard::from_bytes(body).map_err(StateError::Decode)
}
