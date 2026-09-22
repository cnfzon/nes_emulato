//! `nes-net`：Rollback netplay 的協定、傳輸層與會話排程。
//!
//! 依賴 `nes-core` 的 `Buttons`；不依賴任何 async runtime。

pub mod protocol;
pub mod session;
pub mod transport;

pub use protocol::Msg;
pub use session::{Request, RollbackSession};
pub use transport::Transport;
