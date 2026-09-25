//! 與存檔格式無關的行為指紋（[`crate::Nes::behavior_fingerprint`]）用的雜湊器。
//!
//! 指紋把狀態欄位的**數值**依固定順序寫進 xxh3；不經過 serde、postcard 或任何序列化格式，
//! 所以存檔格式改變（增減欄位、換編碼、改 header）時它不會變，只有模擬行為改變才會變。
//! 每個整數以固定寬度 little-endian 寫入、`bool` 寫成一個位元組（0/1）、可變長度的資料
//! （RAM 等）以 `u32` 長度為前綴，所以欄位邊界不會混淆。
//!
//! **完整的欄位規格與順序見 `docs/architecture.md` §18.2。新增任何會影響模擬的狀態欄位，
//! 都必須同時：(1) 在對應的 `fingerprint` 方法寫入、(2) 更新該規格。** 測試
//! `every_serialized_state_field_is_covered_by_the_fingerprint` 會逐一竄改存檔裡的每個
//! 欄位，漏掉的欄位會讓它失敗。
//!
//! 排除：framebuffer 與音訊輸出管線（輸出，rollback 重跑時會關閉）、靜態的 ROM 內容與
//! iNES header（由 `rom_id` 識別）。

use xxhash_rust::xxh3::Xxh3Default;

pub(crate) struct Fp(Xxh3Default);

impl Fp {
    pub(crate) fn new() -> Self {
        Self(Xxh3Default::new())
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.0.update(&[v]);
    }

    pub(crate) fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }

    pub(crate) fn u16(&mut self, v: u16) {
        self.0.update(&v.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.0.update(&v.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.0.update(&v.to_le_bytes());
    }

    /// `u32` 長度前綴 + 原始位元組。
    pub(crate) fn bytes(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.0.update(v);
    }

    pub(crate) fn finish(self) -> u64 {
        self.0.digest()
    }
}
