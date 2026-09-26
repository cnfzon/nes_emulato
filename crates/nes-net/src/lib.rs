//! `nes-net`：netplay 的協定、傳輸層與 session 排程。
//!
//! - `protocol`：封包格式（magic + 版本 + postcard）與訊息。
//! - `transport`：datagram 語意的 `Transport` trait、`UdpTransport`、`InMemoryTransport`。
//! - `simnet`：`SimulatedTransport`——依虛擬時鐘模擬丟包、延遲、抖動、重複。
//! - `session`：Phase 4b 的 lockstep session（握手、輸入交換、Ack／重送、指紋檢查、統計）。
//! - `rollback`：Phase 4c 才實作的 rollback 排程（目前只有骨架）。
//! - `matchlog`：把一場對戰的雙方輸入與指紋記成標準的 replay。
//! - `sim`：無視窗的對戰模擬器（虛擬時鐘、兩個 session、兩個 `Nes`），`nes-test netsim` 與測試共用。
//! - `rng`：固定種子的 SplitMix64（不新增依賴）。
//!
//! 依賴 `nes-core`（`Buttons`、`FrameInput`、`RomId`、行為版本與指紋）；不依賴任何 async runtime，
//! **session 的邏輯不讀系統時間**：所有與時間有關的函式都由呼叫端傳入 `now`。

pub mod matchlog;
pub mod protocol;
pub mod rng;
pub mod rollback;
pub mod session;
pub mod sim;
pub mod simnet;
pub mod transport;

pub use protocol::{DisconnectReason, Msg, ProtocolError, RejectReason};
pub use rollback::{Request, RollbackSession};
pub use session::{
    DEFAULT_INPUT_DELAY, EndReason, Event, LockstepSession, MAX_INPUT_DELAY, Role, SessionConfig,
    Stats, Status,
};
pub use simnet::{NetworkConfig, SimulatedTransport};
pub use transport::{Datagram, InMemoryTransport, Transport, UdpTransport};
