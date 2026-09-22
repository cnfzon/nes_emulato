//! 存檔（save state）的編碼／解碼。
//!
//! 用 postcard 是因為它是 no_std 友善、緊湊、決定性的二進位格式（同一份輸入
//! 永遠編碼出同一份位元組），這對 rollback netplay 的 checksum 比對很重要。

use serde::{Deserialize, Serialize};

use crate::error::StateError;

pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_allocvec(value).expect("記憶體內的 NES 狀態必定可以序列化")
}

pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, StateError> {
    postcard::from_bytes(bytes).map_err(StateError::Decode)
}
