//! `nes-net`：netplay 的協定、傳輸層與 session 排程。
//!
//! - `protocol`：封包格式（magic + 版本 + postcard）與訊息（協定 v2）。
//! - `transport`：datagram 語意的 `Transport` trait、`UdpTransport`、`InMemoryTransport`。
//! - `simnet`：`SimulatedTransport`——依虛擬時鐘模擬丟包、延遲、抖動、重複。
//! - `session`：握手、輸入交換、Ack／重送、統計、逾時；lockstep（Phase 4b）與 rollback（Phase 4c）兩種推進模式共用。
//! - `rollback`：Phase 4c 的 rollback 規劃器（預測、比對、還原重跑的請求清單、時間同步、指紋確認）。
//! - `snapshot`：快照環形緩衝與請求執行器（`nes-app`、netsim、測試共用）。
//! - `statslog`：每秒一筆的統計 CSV（寫出、讀回、摘要）與一場連線的摘要（Phase 4d）。
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
pub mod snapshot;
pub mod statslog;
pub mod transport;

pub use protocol::{
    ConfirmedFingerprint, DisconnectReason, Mode, Msg, PlayerInput, ProtocolError, RejectReason,
};
pub use rollback::{
    ConfirmedFrame, Outcome, Plan, Request, RollbackConfig, RollbackPlanner, RollbackStats,
    Sabotage,
};
pub use session::{
    BufferSizes, DEFAULT_INPUT_DELAY, EndReason, Event, MAX_INPUT_DELAY, Role, Session,
    SessionConfig, Stats, Status, Violation,
};
pub use simnet::{NetworkConfig, SimulatedTransport};
pub use snapshot::{ExecError, ExecReport, SnapshotRing, execute};
pub use statslog::{MatchSummary, StatsRecorder, StatsRow};
pub use transport::{Datagram, InMemoryTransport, Transport, UdpTransport};
