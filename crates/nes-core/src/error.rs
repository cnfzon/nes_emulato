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

    #[error("ROM 沒有 PRG-ROM（header 宣告 0 個 PRG bank）")]
    NoPrgRom,

    #[error("four-screen（4 螢幕）nametable 卡帶尚未支援")]
    FourScreenUnsupported,

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

    /// 解碼成功，但內部資料不符合結構性不變量（例如 RAM/VRAM/OAM/CHR-RAM/
    /// PRG-RAM 的長度跟硬體規格對不上）。這代表存檔本身已經損毀或被竄改，
    /// 不能安全地拿來當作模擬狀態繼續跑，因此明確拒絕而不是照樣載入後讓
    /// 之後的記憶體存取 panic 或算出垃圾結果。
    #[error("存檔資料已損毀：內部欄位長度與預期不符")]
    Corrupt,

    /// 存檔屬於另一份 ROM（`rom_hash` 對不上目前已載入的 ROM）。
    #[error("存檔屬於不同的 ROM（目前 ROM 雜湊 {expected:#018x}，存檔雜湊 {found:#018x}）")]
    RomMismatch { expected: u64, found: u64 },
}
