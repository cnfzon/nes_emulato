//! 錯誤型別：ROM 解析錯誤與存檔／讀檔錯誤。

use thiserror::Error;

/// 解析 iNES ROM 檔案時可能發生的錯誤。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RomError {
    #[error("ROM 檔案過小，不足以包含 16 位元組的 iNES header")]
    TooSmall,

    #[error("缺少 'NES\\x1A' magic number，不是有效的 iNES 檔案")]
    BadMagic,

    #[error("偵測到 NES 2.0 格式，本階段尚未支援")]
    Nes20Unsupported,

    #[error("mapper {0} 尚未實作")]
    UnsupportedMapper(u8),

    #[error("ROM 資料長度不足：header 宣告需要 {expected} bytes，實際只有 {actual} bytes")]
    Truncated { expected: usize, actual: usize },
}

/// 存檔（save state）編碼／解碼過程中可能發生的錯誤。
#[derive(Debug, Error)]
pub enum StateError {
    #[error("存檔資料解碼失敗: {0}")]
    Decode(#[from] postcard::Error),
}
