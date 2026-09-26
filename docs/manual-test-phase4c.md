# Phase 4c 手動測試清單（Rollback 連線）

自動化測試已經涵蓋：rollback 規劃器（預測、比對、還原重跑、預測視窗、時間同步；完全不需要 `Nes`）、快照還原等價、
**等價性**（連線兩端「已確認幀」的行為指紋逐幀相同，且等於雙方輸入合併後離線重播的結果；三種網路條件 × 各 20 組種子、
最壞情況的輸入、含 Reset）、時鐘偏差 1% 的收斂、對方停止傳送輸入時停在預測視窗內、三種破壞性測試、
真實 UDP（127.0.0.1）跑 600 幀、emu 執行緒的 rollback 對戰與 Reset 同步、協定 v1 ↔ v2 的拒絕。細節見
[`architecture.md`](architecture.md) §20。

**但下列項目需要眼睛與手，沒有人在真實視窗與真實網路上確認過**：選單與狀態列的外觀與中文字型、兩種模式的「操作手感」
差異、重跑時畫面與聲音是否有肉眼／耳朵可察覺的瑕疵、Windows 防火牆、真實區網與人為延遲下的行為、彈出視窗。
以下請用 **Spacegulls**（repo 根目錄的 `Spacegulls-1.1.nes`）確認。**電腦 A 是房主（玩家 1）、電腦 B 是加入者（玩家 2）。**

## 準備

```bash
cargo build --release -p nes-app -p nes-test     # 交付／量測用的 nes-app 請單獨建置（見 README）
target\release\nes-app.exe
```

- 雙方必須是**同一版**程式（這一版的協定是 v2；v1 是 Phase 4b，見 F 節）、**同一份** ROM
  （`target\release\nes-test.exe info Spacegulls-1.1.nes` 的 `rom_id:` 必須相同）。
- 鍵位（兩端都用「玩家 1」的按鍵配置）：方向鍵、Z＝B、X＝A、Enter＝Start、右 Shift＝Select。
- 狀態列的 rollback 統計：`[Netplay rollback] 已連線｜你是玩家 N｜ping …｜input delay …｜rollback X 次/秒（深度 平均／最大）｜幀差 ±N｜預測準確率 …｜stall …`。
  lockstep 的狀態列是 `[Netplay lockstep] 已連線｜…｜stall N 次（S 秒）｜↑… ↓… B/s`。
- 回報失敗請附：步驟編號、兩台的狀態列與彈出視窗截圖、`netplay_replays\` 裡的檔案。

## A. 單機雙開：lockstep 與 rollback 的比較（同一台電腦開兩個 `nes-app.exe`）

兩個視窗都載入同一份 ROM。視窗 1 是房主：`Netplay` 選單先選模式，再 `建立房間…`（port 7000）；視窗 2：
`Netplay → 加入…`，輸入 `127.0.0.1:7000`。

| # | 步驟 | 預期結果 |
|---|---|---|
| A1 | 視窗 1：`Netplay` 選單 | 有「模式（房主決定）」的 **rollback（預設）／lockstep** 單選、「Input delay」（rollback 預設 1、範圍 0–4；lockstep 預設 2、範圍 0–8）、「預測視窗 K」（預設 8、只在 rollback 有效）。切到 lockstep 時 input delay 自動變成 2，切回 rollback 變成 1 |
| A2 | 選 **lockstep**、建立房間，視窗 2 加入 | 兩邊狀態列 `[Netplay lockstep] 已連線｜…｜input delay 2｜stall 0 次…`；雙方畫面從開機重新開始且同步 |
| A3 | 分別在兩個視窗操作（視窗 1 控制玩家 1、視窗 2 控制玩家 2）約 1 分鐘 | 同機 loopback 幾乎沒有延遲：兩個視窗畫面一致、stall 極少（0–1 次）；操作手感有 input delay 2 幀（約 33 ms）的固定延遲 |
| A4 | 中斷連線（`Netplay → 中斷連線`），改選 **rollback**、input delay 1、K 8，重新建立房間與加入 | 狀態列 `[Netplay rollback] 已連線｜…｜input delay 1｜rollback 0.0 次/秒（深度 平均 0.0／最大 0）｜幀差 …｜預測準確率 —｜stall 0 次…`。loopback 沒有延遲，所以 rollback 次數應該是 0 或極少，預測準確率接近 100% |
| A5 | 比較 A3 與 A4 的操作手感 | rollback（input delay 1）比 lockstep（input delay 2）的操作延遲少約 1 幀（約 17 ms）。兩者在 loopback 上差異很小，這是預期的；差異要在有延遲的網路上才明顯（見 C 節） |
| A6 | 兩端的 `View → Debugger` 看 Frame | 兩邊 Frame 差距很小（0–3 幀）且持續前進 |

## B. 兩台電腦（區網）：lockstep 與 rollback 的比較

準備與防火牆步驟與 [`manual-test-phase4b.md`](manual-test-phase4b.md) 的 A 節相同（`ipconfig`、防火牆允許「私人網路」）。

| # | 步驟 | 預期結果 |
|---|---|---|
| B1 | 電腦 A 選 **lockstep**、建立房間；電腦 B `加入…` A 的 `IP:7000`；玩 2–3 分鐘 | 有線區網 ping < 10 ms：stall 很少、畫面流暢。Wi-Fi ping 較大且抖動時 stall 變多，每次 stall 畫面短暫停頓（記下 stall 次數與累計秒數） |
| B2 | 中斷後改選 **rollback**（input delay 1、K 8）重來，玩同樣長度 | 畫面**不會因為網路抖動而停頓**（預測視窗 K=8 約吸收 133 ms 的延遲）；狀態列 rollback 次數／秒、平均深度會依 ping 而有幾次到幾十次；記下「stall」（只有預測視窗滿了才會發生，應該遠少於 lockstep） |
| B3 | 比較兩種模式的操作手感與狀態列 | rollback 的按鍵反應更即時（本地輸入立刻套用）；偶爾對方角色會「瞬間跳位」（預測錯誤、還原重跑後修正），這是 rollback 的正常現象。狀態列的**預測準確率**大約在 40–90%（依對方按鍵的變化頻率）。**幀差**（本地領先對方的幀數）應維持在 ±2 以內，不會越來越大 |
| B4 | 在 B2 進行中，聽聲音 | 大多數時候聲音連續。rollback 發生時**可能出現極短暫的音訊瑕疵**（已播出的預測幀的聲音無法收回，只有最後一幀的音訊被輸出）——這是已知取捨（`architecture.md` §20.9）；若瑕疵頻繁到影響遊玩，請回報並附 ping 與狀態列數字 |
| B5 | 在 B2 進行中，在任一台按 **F9**、`Emulation → Pause`、Debugger 的單步 | 仍被阻擋（狀態列紅字「Netplay 中不能…」）；**Reset 不再被阻擋**（見 D 節） |

## C.（選做）用 Windows 的網路模擬工具加入 100 ms 延遲

可以用 [Clumsy](https://jagt.github.io/clumsy/)（第三方免費工具，需系統管理員權限）或 Windows 內建方案（如 WSL／Hyper-V 虛擬網卡
加上 `tc netem`）。以 Clumsy 為例：Filter 填 `udp and (udp.DstPort == 7000 or udp.SrcPort == 7000)`，勾 **Lag**、Inbound 與
Outbound 各設 `100`（毫秒，單程延遲；RTT 約 200 ms）；也可以再勾 Drop 10% 與 Out of order。

| # | 步驟 | 預期結果 |
|---|---|---|
| C1 | 開啟 Clumsy 延遲之後，用 **lockstep**（input delay 2）連線並玩 1 分鐘 | 遊戲變成明顯的「慢動作」（幀率掉到 20 幾 fps）、stall 次數與累計秒數快速增加——與 `nes-test netsim` 的模擬結果一致（100 ms 單程約 24 fps） |
| C2 | 同樣的延遲下改用 **rollback**（input delay 1、K 8） | 畫面維持約 60 fps，本地按鍵立即反應；狀態列 rollback 次數／秒大幅增加（每次對方輸入變化都要重跑，平均深度約 5–6 幀）、預測準確率降低，對方角色偶爾瞬間跳位。若延遲再加大到單程 200 ms，會超出預測視窗（K=8＋input delay 1 ≈ 150 ms），stall 出現、退化成部分 lockstep——把 K 調大（例如 12）可以吸收更多延遲，代價是重跑更深 |
| C3 | 關閉 Clumsy | rollback 的幀差回到 ±2 以內；lockstep 的幀率回到 60 |

## D. 連線中按 Reset：雙方同時重置

| # | 步驟 | 預期結果 |
|---|---|---|
| D1 | rollback（或 lockstep）連線中，遊戲進行到有分數／進度時，在**電腦 B**按 `Emulation → Reset (soft reset)` | 選單項目在 Netplay 中是**可用**的（不再是灰的）。按下之後（約 1–3 幀內，input delay 決定）**兩台電腦的遊戲同時**回到 reset 後的畫面（Spacegulls 回到標題／開機畫面）；兩邊畫面仍然一致 |
| D2 | 在**電腦 A**再按一次 | 同樣雙方同時重置 |
| D3 | 兩台**幾乎同時**都按 Reset | 只重置一次或兩次（各在自己的那一幀），但兩邊行為一致；不出現 desync 視窗 |
| D4 | 結束連線後用 `nes-test replay info <replay>` | 「reset 次數」等於你按的次數（去掉合併的），幀號兩台的 replay 相同（見 F 節驗證） |

## E. 結束後雙方的 replay 相同、且都通過 verify

連線結束（任一方 `Netplay → 中斷連線`）後，兩台電腦的 `target\release\netplay_replays\` 各有一份
`netplay-<秒>-p<1|2>-<ROM>.replay`。把其中一份複製到另一台（或同一台），然後：

| # | 步驟 | 預期結果 |
|---|---|---|
| E1 | 兩台各執行 `certutil -hashfile netplay-….replay SHA256`（檔名換成各自的） | **兩個雜湊值完全相同**（兩份檔案位元組相同：雙方取共同的已確認幀數） |
| E2 | 兩台各執行 `target\release\nes-test.exe replay verify Spacegulls-1.1.nes netplay-….replay` | 都印出通過、結束碼 0 |
| E3 | 對 rollback 與 lockstep 兩種模式各做一次 E1、E2 | 都成立。rollback 的 replay 只包含「已確認」的幀（預測的幀不會進 replay） |

## F. 用 v1 協定的舊版本連線 → 被拒絕並顯示原因

v1 是 Phase 4b（tag `phase-4b`）。取得並建置舊版（不會動到目前的工作目錄；`git worktree` 建立的是獨立的資料夾）：

```bash
git worktree add ..\nes-phase4b phase-4b
cd ..\nes-phase4b
cargo build --release -p nes-app
# 執行 ..\nes-phase4b\target\release\nes-app.exe
```

用完可以 `git worktree remove ..\nes-phase4b` 清掉。

| # | 步驟 | 預期結果 |
|---|---|---|
| F1 | 新版（v2）當房主、建立房間；**舊版（v1）** 載入同一份 ROM、`Netplay → 加入…` | 舊版在 1 秒內跳出說明視窗：「連線被拒絕：協定版本不符：房主使用 v2，你使用 v1。請雙方使用相同版本的程式」（舊版自己的文字）；**不是**等 10 秒後的「連線逾時」。新版房主仍然在等（狀態列 `等待對手連線`），可以接著讓新版的 Client 連上 |
| F2 | 舊版（v1）當房主、建立房間；**新版（v2）** `Netplay → 加入…` | 新版在 1 秒內跳出說明視窗：「連線被拒絕：協定版本不符：房主使用 v1，你使用 v2（v1 是 Phase 4b 的舊版，只有 lockstep；v2 新增 rollback、reset 同步與新的封包格式，兩者無法互通）。請雙方使用相同版本的程式」 |
| F3 | 兩邊都是新版但 ROM 不同 | 仍是 4b 的行為：「連線被拒絕：ROM 不同：…」 |

## 已知需要留意的地方

- rollback 的預測視窗 K 與 input delay 是**各自的本地設定**，雙方不必相同；但連線模式一定由房主決定。
- 高延遲＋高丟包時 rollback 也會 stall（預測視窗滿了），這時行為接近 lockstep；stall 統計在狀態列。
- Netplay 中仍停用：讀取存檔（F9）、暫停、單步、Trace、載入 ROM、錄製／播放 replay。
