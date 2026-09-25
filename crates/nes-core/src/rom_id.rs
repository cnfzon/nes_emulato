//! ROM 識別碼：對「整個 ROM 檔案（含 iNES header 與 trainer）」計算的 xxh3-128。
//!
//! 取代 Phase 4a 之前只涵蓋 PRG + CHR 的 `rom_hash`：header 的 mapper／mirroring 位元不同，
//! 就是不同的卡帶，不該被視為同一份 ROM。存檔、replay 與（4b 起的）netplay 握手都用它確認
//! 「雙方載入的是同一份檔案」。
//!
//! 位元組表示採 xxHash 的 canonical 形式（高 64 位元在前的 big-endian），與 `xxh128sum`
//! 印出的十六進位字串相同，所以可以用外部工具核對。UI 與 CLI 顯示前 16 個十六進位字元
//! （[`RomId::short`]）。

use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub struct RomId([u8; 16]);

impl RomId {
    /// 對整個 ROM 檔案計算。
    pub fn of_file(bytes: &[u8]) -> Self {
        Self(xxhash_rust::xxh3::xxh3_128(bytes).to_be_bytes())
    }

    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// 前 16 個十六進位字元（UI／CLI 的顯示用）。
    pub fn short(&self) -> String {
        self.0[..8].iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// 完整的 32 個十六進位字元。
impl fmt::Display for RomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for RomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RomId({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_the_whole_file_including_the_header() {
        let mut a = vec![0u8; 64];
        a[..4].copy_from_slice(b"NES\x1A");
        let mut b = a.clone();
        b[6] ^= 0x01; // 只有 header 的 mirroring 位元不同
        assert_ne!(RomId::of_file(&a), RomId::of_file(&b));
        assert_eq!(RomId::of_file(&a), RomId::of_file(&a.clone()));
    }

    #[test]
    fn display_is_canonical_hex_and_short_is_its_first_16_chars() {
        let id = RomId::of_file(b"hello");
        let full = id.to_string();
        assert_eq!(full.len(), 32);
        assert!(full.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(id.short(), &full[..16]);
        // 與 xxh3_128 的 u128 值一致（高位在前）。
        assert_eq!(
            full,
            format!("{:032x}", xxhash_rust::xxh3::xxh3_128(b"hello"))
        );
    }
}
