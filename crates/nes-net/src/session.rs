//! Rollback netplay 的核心排程邏輯。
//!
//! 這個模組本身**不**擁有 `Nes`——它只決定「接下來該對模擬核心做什麼」
//! （存檔／讀檔／推進一幀），實際執行交給呼叫端（`nes-app` 的 emu 執行緒）。
//! 這樣 `nes-net` 完全不需要依賴 GUI/執行緒/計時器，維持可獨立測試。

use std::collections::BTreeMap;

use nes_core::Buttons;

use crate::protocol::Msg;

/// `RollbackSession::advance` 回傳的一步指令，由呼叫端逐一執行在自己持有
/// 的 `Nes` 實例上。
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// 對第 `frame` 幀存檔（呼叫端呼叫 `Nes::save_state` 並保留起來）。
    SaveState { frame: u64 },
    /// 讀回先前對 `frame` 存的檔，回到那一幀「跑完之後」的狀態。
    LoadState { frame: u64 },
    /// 用給定的雙人輸入推進一幀（呼叫端呼叫 `Nes::run_frame(inputs)`）。
    AdvanceFrame { frame: u64, inputs: [Buttons; 2] },
}

/// Rollback netplay 會話狀態機。
///
/// # Rollback 演算法概述
///
/// 傳統的「lockstep」netplay 每一幀都要等雙方輸入到齊才能往下跑，一旦網路
/// 延遲高，操作手感就會跟著延遲。Rollback（GGPO 風格）的作法反過來：
///
/// 1. **樂觀預測（predict）**：本地端不等對方，直接用「對方上一幀的輸入」
///    當作這一幀的預測值，正常推進模擬（`AdvanceFrame`）。
/// 2. **持續存檔（checkpoint）**：每推進一幀，都對那一幀存檔
///    （`SaveState`），存檔會保留最近 N 幀（N 由 `input_delay` 與允許的
///    最大 rollback 深度決定），舊的存檔可以丟棄。
/// 3. **輸入送達 → 比對（reconcile）**：透過 [`Msg::Input`] 收到對方「真正」
///    的輸入後，如果它跟之前預測的值不同，代表預測錯了：
///    - 用 `LoadState` 讀回「最後一次雙方輸入都確定」的那一幀存檔；
///    - 用已確定的（本地 + 對方）真實輸入，依序重新 `AdvanceFrame` 追到
///      目前最新的一幀（這就是「rollback」：時間軸上退回去，再重新播放）。
/// 4. **確認幀（confirm）**：當某一幀的雙方輸入都已知且一致，該幀就變成
///    `confirmed_frame`，之前的存檔可以清掉，往後不會再對它 rollback。
/// 5. **Desync 偵測**：定期交換 [`Msg::Checksum`]（`Nes::state_hash`），
///    如果同一幀的雜湊在雙方不一致，代表模擬邏輯出現不確定性（例如誤用了
///    `HashMap` 迭代順序、讀到系統時間等），應該立即中止對局並回報，而不是
///    悄悄繼續跑出兩份不同的遊戲。
///
/// 這個 struct 目前只定義資料結構與方法簽名；方法本體先回傳空結果或合理
/// 預設值（不 panic），真正的排程邏輯留到後續階段依上面的演算法實作。
///
/// `#[allow(dead_code)]`：`input_delay` / `current_frame` / `remote_inputs`
/// 目前只被建構與存放，要等 `handle_msg` / `advance` 實作排程邏輯後才會被
/// 讀取。
#[derive(Debug)]
#[allow(dead_code)]
pub struct RollbackSession {
    local_player: u8,
    input_delay: u8,
    /// 雙方輸入都已確認、不會再 rollback 的最後一幀。
    confirmed_frame: u64,
    /// 本地端已經推進到的最後一幀（可能包含尚未確認的預測輸入）。
    current_frame: u64,
    /// 用 `BTreeMap` 而非 `HashMap`：跟 `nes-core` 一樣的理由——rollback
    /// 重播需要照幀號「決定性地」由小到大走訪，`HashMap` 的迭代順序不保證
    /// 穩定。
    local_inputs: BTreeMap<u64, Buttons>,
    remote_inputs: BTreeMap<u64, Buttons>,
}

impl RollbackSession {
    pub fn new(local_player: u8, input_delay: u8) -> Self {
        Self {
            local_player,
            input_delay,
            confirmed_frame: 0,
            current_frame: 0,
            local_inputs: BTreeMap::new(),
            remote_inputs: BTreeMap::new(),
        }
    }

    pub fn local_player(&self) -> u8 {
        self.local_player
    }

    pub fn confirmed_frame(&self) -> u64 {
        self.confirmed_frame
    }

    /// 記錄本地玩家在某一幀輸入的按鍵，稍後由 [`RollbackSession::advance`]
    /// 打包成 [`Msg::Input`] 送出。
    pub fn add_local_input(&mut self, frame: u64, input: Buttons) {
        self.local_inputs.insert(frame, input);
    }

    /// 處理一則收到的網路訊息。
    ///
    /// TODO: 依訊息種類更新狀態——
    /// - `Msg::Input`：把對方輸入寫進 `remote_inputs`，若跟先前預測不同，
    ///   標記需要從對應幀開始 rollback。
    /// - `Msg::Ack`：對方已收到本地輸入到哪一幀，可以清掉更早的存檔。
    /// - `Msg::Checksum`：跟本地同一幀的 `state_hash` 比對，不一致就回報
    ///   desync。
    /// - `Msg::Ping`：立刻回覆 `Msg::Pong`（由呼叫端負責實際送出）。
    pub fn handle_msg(&mut self, _msg: Msg) {}

    /// 產出「接下來該做什麼」的指令序列，由呼叫端依序套用在自己的 `Nes`
    /// 實例上。
    ///
    /// TODO: 真正的實作會依「是否需要 rollback」分兩種情況：
    /// - 沒有預測錯誤：只推進一幀，回傳單一 `AdvanceFrame`。
    /// - 偵測到預測錯誤：回傳
    ///   `[LoadState{確認幀}, AdvanceFrame×N（追到目前幀), ...]`
    ///   讓呼叫端照序執行完成 rollback + replay。
    pub fn advance(&mut self) -> Vec<Request> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_starts_at_frame_zero() {
        let session = RollbackSession::new(0, 2);
        assert_eq!(session.confirmed_frame(), 0);
        assert_eq!(session.local_player(), 0);
    }

    #[test]
    fn add_local_input_does_not_panic_and_advance_returns_empty_by_default() {
        let mut session = RollbackSession::new(0, 2);
        session.add_local_input(0, Buttons::A);
        assert!(session.advance().is_empty());
    }

    #[test]
    fn handle_msg_does_not_panic_on_any_variant() {
        let mut session = RollbackSession::new(1, 2);
        session.handle_msg(Msg::Hello {
            version: 1,
            rom_hash: 0,
        });
        session.handle_msg(Msg::Ping { t: 0 });
    }
}
