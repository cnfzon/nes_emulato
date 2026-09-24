//! 鍵盤對應：兩位玩家各一張「實體按鍵 → NES 按鈕」表。
//!
//! 對應表是資料（常數陣列）而不是散在 `if` 裡，這樣可以用單元測試檢查
//! 「兩位玩家沒有共用任何按鍵」「沒有蓋到應用程式的快捷鍵」，之後改對應表也不會
//! 悄悄產生衝突。
//!
//! # 玩家 2 的方案與理由
//!
//! | NES 按鈕 | 玩家 1 | 玩家 2 |
//! |---|---|---|
//! | 方向 | ↑ ↓ ← → | W A S D |
//! | B | Z | F |
//! | A | X | G |
//! | Select | 右 Shift | R |
//! | Start | Enter | T |
//!
//! - **玩家 1 維持原樣**：已有使用者習慣，也寫在 README 與手動測試文件裡，不動。
//! - **玩家 2 用鍵盤左上角的 WASD 群**：與玩家 1 的所有按鍵完全不重疊；WASD 是最普及的
//!   左手方向鍵組合，F/G 緊接在 D 的右邊、R/T 在它們正上方，維持「方向 + B/A + Select/
//!   Start」和 NES 手把相同的相對位置（A 在 B 右邊、Start 在 Select 右邊）。
//! - **避開應用程式快捷鍵**：F5（存檔）、F9（讀檔）都是 F 鍵列，與字母鍵無關。
//! - **已知限制**：多數鍵盤有 key rollover / ghosting 限制，同時按下的鍵太多時
//!   某些組合會被硬體吞掉；兩人共用一把鍵盤時，玩家 1 的 Z/X 與玩家 2 的 WASD 位置相鄰。
//!   想要順暢的雙人對戰，實體手把才是正解（見 Phase 3 報告：需要新增依賴，等使用者決定）。

use eframe::egui::Key;
use nes_core::Buttons;

pub type KeyMap = [(Key, Buttons); 8];

pub const PLAYER1_KEYS: KeyMap = [
    (Key::ArrowUp, Buttons::UP),
    (Key::ArrowDown, Buttons::DOWN),
    (Key::ArrowLeft, Buttons::LEFT),
    (Key::ArrowRight, Buttons::RIGHT),
    (Key::Z, Buttons::B),
    (Key::X, Buttons::A),
    (Key::Enter, Buttons::START),
    (Key::ShiftRight, Buttons::SELECT),
];

pub const PLAYER2_KEYS: KeyMap = [
    (Key::W, Buttons::UP),
    (Key::S, Buttons::DOWN),
    (Key::A, Buttons::LEFT),
    (Key::D, Buttons::RIGHT),
    (Key::F, Buttons::B),
    (Key::G, Buttons::A),
    (Key::R, Buttons::SELECT),
    (Key::T, Buttons::START),
];

/// 存檔／讀檔快捷鍵（`app.rs` 使用）。
pub const HOTKEY_SAVE_STATE: Key = Key::F5;
pub const HOTKEY_LOAD_STATE: Key = Key::F9;

/// 應用程式自己用掉的快捷鍵；搖桿對應不得使用（有單元測試把關）。
#[cfg(test)]
const APP_HOTKEYS: [Key; 2] = [HOTKEY_SAVE_STATE, HOTKEY_LOAD_STATE];

/// 依 `is_down`（某個實體鍵目前是否按著）組出一位玩家的按鈕狀態。
pub fn buttons_from_keys(map: &KeyMap, is_down: impl Fn(Key) -> bool) -> Buttons {
    let mut buttons = Buttons::empty();
    for &(key, button) in map {
        buttons.set(button, is_down(key));
    }
    buttons
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_players_share_no_physical_key() {
        for (k1, _) in PLAYER1_KEYS {
            assert!(
                PLAYER2_KEYS.iter().all(|&(k2, _)| k1 != k2),
                "{k1:?} 同時被兩位玩家使用"
            );
        }
    }

    #[test]
    fn joypad_keys_do_not_shadow_app_hotkeys() {
        for map in [PLAYER1_KEYS, PLAYER2_KEYS] {
            for (key, _) in map {
                assert!(!APP_HOTKEYS.contains(&key), "{key:?} 是應用程式快捷鍵");
            }
        }
    }

    #[test]
    fn each_map_covers_all_eight_buttons_exactly_once() {
        for map in [PLAYER1_KEYS, PLAYER2_KEYS] {
            let mut all = Buttons::empty();
            for (_, button) in map {
                assert!(!all.intersects(button), "{button:?} 被對應了兩次");
                all |= button;
            }
            assert_eq!(all, Buttons::all());
        }
    }

    #[test]
    fn each_map_uses_distinct_keys() {
        for map in [PLAYER1_KEYS, PLAYER2_KEYS] {
            for (i, (a, _)) in map.iter().enumerate() {
                assert!(map[i + 1..].iter().all(|(b, _)| a != b));
            }
        }
    }

    #[test]
    fn buttons_follow_the_keys_that_are_down() {
        let down = [Key::W, Key::G, Key::T];
        let p2 = buttons_from_keys(&PLAYER2_KEYS, |k| down.contains(&k));
        assert_eq!(p2, Buttons::UP | Buttons::A | Buttons::START);
        // 同一組按下的鍵對玩家 1 沒有任何作用。
        let p1 = buttons_from_keys(&PLAYER1_KEYS, |k| down.contains(&k));
        assert_eq!(p1, Buttons::empty());
    }
}
