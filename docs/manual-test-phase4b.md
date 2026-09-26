# Phase 4b 手動測試清單（Lockstep 連線，兩台 Windows 電腦、區網）

自動化測試已經涵蓋了：協定的編碼／解碼／模糊測試、握手（成功、三種不符被拒絕並回傳原因、30% 丟包仍能完成）、
**等價性**（連線兩端的行為指紋逐幀相同，且等於「雙方輸入合併後離線重播」的結果；三種網路條件 × 各 20 組種子 ×
3600 幀）、斷線逾時、破壞性測試、真實 UDP（127.0.0.1）跑 600 幀、emu 執行緒的 Netplay（兩個執行緒經由 loopback
連線並存下相同的 replay，不需要視窗）。細節見 [`architecture.md`](architecture.md) §19。

**但下列項目需要眼睛與手，沒有人在真實視窗與真實區網上確認過**：選單與對話框的外觀、中文字型、狀態列文字與顏色、
Windows 防火牆的行為、兩台電腦之間實際的延遲與 stall、鍵盤在兩端各自對應到正確的角色、關閉程式時對方的反應。
以下請用 **Spacegulls**（repo 根目錄的 `Spacegulls-1.1.nes`）確認。**電腦 A 是房主（玩家 1）、電腦 B 是加入者（玩家 2）。**

## 準備

兩台電腦都要有**同一版**的程式與**同一份** ROM 檔案（連 header 都必須相同；握手時會比對整個檔案的 `rom_id`）：

```bash
cargo build --release -p nes-app -p nes-test     # 交付／量測用的 nes-app 請單獨建置（見 README）
target\release\nes-app.exe
```

- 核對 ROM：兩台都執行 `target\release\nes-test.exe info Spacegulls-1.1.nes`，`rom_id:` 那一行必須完全相同
  （GUI 狀態列的 `ROM <前 16 字元>` 也一樣）。
- 兩台電腦接在**同一個區網**（同一台路由器／同一個 Wi-Fi）。Wi-Fi 的延遲抖動比有線大，若 stall 很多，改用有線或
  調高 input delay（見 D 節）。
- **鍵位（兩台電腦都用「玩家 1」的按鍵配置）**：方向鍵、Z＝B、X＝A、Enter＝Start、右 Shift＝Select。
  你在電腦 A 操作的是遊戲裡的玩家 1、在電腦 B 操作的是玩家 2——這由連線自動對應，**不要**在電腦 B 用 WASD。
- 回報失敗時請附：步驟編號、兩台電腦狀態列的截圖、彈出視窗的截圖、`netplay_replays\` 裡的檔案。

## A. 查詢 IP 與 Windows 防火牆

| # | 步驟 | 預期結果 |
|---|---|---|
| A1 | 在**電腦 A**開「命令提示字元」（或 PowerShell），執行 `ipconfig` | 找到目前上網的那張網卡（「無線區域網路介面卡 Wi-Fi」或「乙太網路卡」）底下的 **IPv4 位址**，例如 `192.168.1.23`；記下來（`192.168.` 或 `10.` 開頭才是區網位址；`169.254.` 開頭代表沒有連上網路） |
| A2 | 在電腦 B 執行 `ping 192.168.1.23`（換成 A 的 IP） | 有回應（「回覆自 …」）。若「要求等候逾時」，可能是 A 的防火牆擋掉 ping（不影響 UDP 連線）或不在同一個網段——先繼續，連不上再回頭查 |
| A3 | 電腦 A：`File → Open ROM...` 載入 Spacegulls；`Netplay → 建立房間…` | 跳出「Netplay：建立房間」視窗：port 預設 `7000`、顯示「Input delay：2 幀」，並有提示文字（ipconfig 與防火牆）；**按「建立房間」** |
| A4 | **Windows 防火牆第一次跳出「Windows Defender 防火牆已封鎖此應用程式的部分功能」** | 勾選**「私人網路」**，按「允許存取」。（家用 Wi-Fi 通常是私人網路；若你的網路被歸類為「公用網路」，要嘛勾公用、要嘛到「設定 → 網路和網際網路」把該網路改成「私人」。）**不要**關掉對話框而不選——沒有允許的話，電腦 B 的封包會被丟掉，看起來就是「一直連線中」 |
| A5 | 若沒有跳出防火牆提示、但 B 連不上 | 可能之前選過「取消」而被建立了封鎖規則：開「Windows Defender 防火牆 → 允許應用程式通過防火牆」，找到 `nes-app.exe`，勾選「私人」；找不到就按「允許其他應用程式」加入 `target\release\nes-app.exe`。或（系統管理員）`netsh advfirewall firewall add rule name="nes-netplay" dir=in action=allow protocol=UDP localport=7000` |
| A6 | 電腦 A 看狀態列 | 藍字 `[Netplay] 等待對手連線（UDP port 7000）…`；遊戲畫面停住（房主在等人時不推進遊戲）；`Netplay` 選單裡的「取消」可用 |
| A7 | 電腦 A：`Netplay` 選單、`Emulation` 選單、`File` 選單、`View → Debugger` | 「建立房間…／加入…」是灰的；`Emulation` 的 Pause／Reset／Load State 是灰的（tooltip 說明原因）；`File → Open ROM...` 是灰的；Debugger 的「暫停」「單步指令」「Trace」是灰的，面板有**黃色說明文字**（「Netplay 中停用：讀取存檔（F9）、單步指令、Trace、載入別的 ROM、暫停、Reset、錄製／播放 replay。原因：…」）；`File → Save State to File...` 與 `Save State (F5)` 仍可用 |

## B. 連線與雙人遊玩（確認畫面同步、各自操作的是正確的角色）

| # | 步驟 | 預期結果 |
|---|---|---|
| B1 | 電腦 B：`File → Open ROM...` 載入**同一份** Spacegulls；`Netplay → 加入…`，輸入 `192.168.1.23:7000`（A 的 IP 與 port），按「連線」 | 電腦 B 狀態列先顯示 `[Netplay] 正在連線到 192.168.1.23:7000…`，隨即（通常不到 1 秒）變成 `[Netplay] 已連線｜你是玩家 2｜ping N ms｜input delay 2｜stall 0 次（0.0 秒）｜↑… ↓… B/s` |
| B2 | 同一時間看電腦 A | 狀態列變成 `[Netplay] 已連線｜你是玩家 1｜…`；**兩台的遊戲都從開機畫面重新開始**（連線成功時雙方都重新開機，第 0 幀對齊），兩邊的畫面**一模一樣、同步前進**（Frame 幾乎相同，差 0–3 幀屬正常） |
| B3 | （選做，在 B1 **之前**先試）電腦 B `Netplay → 加入…`，輸入 `192.168.1.23`（沒有 port）或 `abc`，按「連線」 | 視窗內紅字「格式必須是「IP:port」，例如 192.168.1.10:7000（要包含 port）」；視窗不關閉、不會當機、不會送出連線；改成正確的 `IP:port` 再按「連線」即進入 B1 |
| B4 | 進入遊戲玩到雙人模式（Spacegulls 的標題畫面用 Start 選人數／開始）。**只在電腦 A 按鍵**（方向鍵、Z、X） | 遊戲裡**玩家 1 的角色**有反應（兩台電腦的畫面都同步顯示），玩家 2 的角色不動 |
| B5 | **只在電腦 B 按鍵**（一樣是方向鍵、Z、X，**不是** WASD） | 遊戲裡**玩家 2 的角色**有反應（兩台電腦的畫面都同步顯示），玩家 1 的角色不動 |
| B6 | 兩邊同時操作約 2–3 分鐘（可以互相搶東西／打對方） | 兩台電腦的畫面**始終一致**（對照角色位置、分數、場景）；不會出現「我這邊你打中了、你那邊沒有」。操作手感有輕微延遲（input delay 2 幀約 33 ms）屬正常 |
| B7 | 看狀態列的統計 | ping（區網通常 < 10 ms，Wi-Fi 可能 5–30 ms）、input delay 2、stall 次數與累計秒數（區網上應該很少；每次 stall 畫面會短暫停頓）、↑↓ 每秒位元組數（約 2–4 KB/s） |
| B8 | 遊戲進行中，在任一台按 **F9**、按 `Emulation → Pause`、`Reset`、Debugger 的單步 | F9 被阻擋，狀態列紅字「Netplay 中不能讀取存檔：單方面這麼做會讓雙方的模擬分歧（同步暫停／讀檔留待之後評估）」；Pause／Reset／單步是灰的；**遊戲沒有跳回舊畫面、沒有停住** |
| B9 | 在任一台按 **F5**（存檔）、`File → Save State to File...` | 可以（唯讀），不影響對戰 |
| B10 | 兩台都開 `View → Debugger` 看 Frame | 兩台的 Frame 差距很小（0–3 幀）且持續前進；stall 時 Frame 會短暫停住 |

## C. 用不同的 ROM 連線 → 被拒絕並顯示原因

先在電腦 A 按 `Netplay → 取消／中斷連線`（回到單機），重新 `建立房間…`；電腦 B 也先中斷。

| # | 步驟 | 預期結果 |
|---|---|---|
| C1 | 電腦 B：載入**另一個** ROM（例如 `roms\nestest\nestest.nes`，或任何別的 `.nes`），`Netplay → 加入…` 連到 A | B 幾乎立刻（< 1 秒）彈出黃色標題「Netplay 已結束」視窗，內容：「**連線被拒絕：ROM 不同：房主的 ROM 是 6d4b660b0ce685ed，你的是 ……。請雙方載入同一份 ROM 檔案**」（前 16 字元是各自的 `rom_id`）；狀態列同樣有紅字；視窗底下寫「已回到單機模式」 |
| C2 | 按「關閉」，然後操作 B（例如按方向鍵、`Emulation → Pause`） | B 已回到單機模式：遊戲照常運作、Netplay 的限制都解除；**沒有當機** |
| C3 | 看電腦 A | A **仍然**是 `[Netplay] 等待對手連線…`（被拒絕的人不會讓房主離開等待狀態），沒有任何錯誤彈窗；這時電腦 B 換載入 Spacegulls 再加入，可以正常連上（回到 B1） |
| C4 | （選做）電腦 B 輸入一個沒人監聽的位址，例如 `192.168.1.23:7999` | 狀態列 `[Netplay] 正在連線到 …`，約 **10 秒**後彈出「Netplay 已結束」：「連線逾時：對方沒有回應。請確認 IP 與 port 正確、房主已建立房間，且防火牆允許 UDP」；回到單機模式 |
| C5 | （選做）房主輸入已被別的程式佔用的 port（例如先開兩個 nes-app，兩個都建立房間 port 7000） | 第二個立刻彈出「無法在 port 7000 建立房間：…（這個 port 可能已被別的程式使用）」；回到單機模式 |

> 協定版本不符、核心行為版本不符，兩台電腦用同一份建置不會遇到，只有自動化測試涵蓋
> （`nes-net/tests/handshake.rs`：三種不符各自被拒絕、原因文字正確）。

## D. 一方關閉程式 → 另一方顯示斷線並回到單機模式

先重新連線（B1–B2），玩一小段。

| # | 步驟 | 預期結果 |
|---|---|---|
| D1 | **電腦 B 按視窗右上角的 ✕（或 `File → Quit`）** | 電腦 A 在 **1 秒內**狀態列顯示「對方已中斷連線。replay：…\netplay_replays\netplay-…-p1-….replay」；`[Netplay]` 標記消失；遊戲**繼續**在單機模式運作（畫面沒有消失，Netplay 的限制解除：可以暫停、Reset、F9） |
| D2 | 重新連線，這次在**電腦 B 用工作管理員強制結束 `nes-app.exe`**（模擬當機／拔網路線） | 電腦 A 的狀態列 `[Netplay]` 的 stall 次數開始增加、畫面停住；**約 5 秒後**彈出「Netplay 已結束」：「連線中斷：超過 5 秒沒有收到對方的任何封包」；回到單機模式，沒有當機（UDP 在 Windows 上對已關閉的 port 送封包會產生 `ConnectionReset` 錯誤，程式已處理，這裡實際確認） |
| D3 | 重新連線，在**電腦 A 用 `Netplay → 中斷連線`** | 兩台都回到單機模式：A 狀態列「你已中斷連線。replay：…」，B「對方已中斷連線。replay：…」；兩邊都沒有彈出錯誤視窗 |
| D4 | 中斷之後，任一台再 `Netplay → 建立房間…`／`加入…` | 可以再連一次（每次連線都重新開機、重新錄一份新的 replay） |

## E. Replay：兩台各自存下的檔案必須通過 verify，且內容完全相同

每場連線會自動錄成 replay（雙方合併後的輸入 + 行為指紋檢查點），存在**執行檔旁**的 `netplay_replays\` 資料夾
（`target\release\netplay_replays\`）：`netplay-<Unix 秒>-p<1 或 2>-<ROM 前 16 字元>.replay`。狀態列（正常結束時）與
「Netplay 已結束」視窗會顯示完整路徑。

**要得到位元組完全相同的兩個檔案，這一場請用 D3（其中一方按 `Netplay → 中斷連線`）結束**：中斷時雙方互相交換
「自己已完成的幀數」，取兩者較小值當作 replay 的長度。（D1 也可以：關閉視窗時，程式會先送出 `Disconnect` 並最多等 0.5 秒交換幀數，再結束。
D2 強制結束／斷線逾時**不會**交換幀數，兩份檔案可能差幾幀，但仍各自通過 verify。）

| # | 步驟 | 預期結果 |
|---|---|---|
| E1 | 連線、玩 1–2 分鐘、用 D3 中斷。在兩台電腦各自找到 `netplay_replays\` 裡**剛產生的**那個 `.replay`（依修改時間；檔名的 `p1`／`p2` 是玩家位置） | 各有一個檔案；`nes-test replay info <檔案>` 顯示相同的 `rom_id`、相同的總幀數 |
| E2 | 兩台各自執行 `target\release\nes-test.exe replay verify Spacegulls-1.1.nes <自己的 replay>` | 兩台都印出「**通過：N 幀、K 個檢查點全數相符**」，結束碼 0 |
| E3 | 把電腦 B 的 replay 複製到電腦 A（隨身碟、共用資料夾、通訊軟體都可以），放在同一個資料夾，例如 `C:\tmp\a.replay`（A 的檔案）與 `C:\tmp\b.replay`（B 的檔案）。**在 PowerShell** 算雜湊：`Get-FileHash -Algorithm SHA256 C:\tmp\a.replay, C:\tmp\b.replay` | 兩個檔案的 `Hash` 欄位**完全相同**。（或用命令提示字元：`certutil -hashfile C:\tmp\a.replay SHA256` 與 `certutil -hashfile C:\tmp\b.replay SHA256`，比對兩段 64 個十六進位字元） |
| E4 | 逐位元組比對：命令提示字元 `fc /b C:\tmp\a.replay C:\tmp\b.replay`；PowerShell 也可用 `(Get-FileHash C:\tmp\a.replay).Hash -eq (Get-FileHash C:\tmp\b.replay).Hash` | `fc` 印出「FC: 找不到差異」；PowerShell 印出 `True` |
| E5 | 在**電腦 A** 用 GUI 播放：`Replay → Play Replay...` 選 `a.replay`（先確認沒在 Netplay 中） | 遊戲從開機重新開始，自動照著雙方**當時的操作**重播（兩個玩家都在動），狀態列 `[播放中 1x]` 檢查點持續相符，最後 `[播放完成]`——**等於在單機重現了那一場對戰** |
| E6 | （選做）若 E3 的雜湊不同：`nes-test replay info` 看兩者的「總幀數」 | 若只是總幀數不同（差幾幀）而且較短者是較長者的前綴，代表結束時沒有交換幀數（例如用了 D2）；兩份仍各自通過 verify。若**同樣長度但雜湊不同**，這是 bug，請回報：附兩個檔案 |

## F. Input delay 與 stall（選做）

| # | 步驟 | 預期結果 |
|---|---|---|
| F1 | 電腦 A 中斷後，把 `Netplay` 選單的「Input delay」改成 5，再建立房間；B 加入 | 兩台狀態列都顯示 `input delay 5`（加入者使用房主的值）；操作延遲變大（約 83 ms），stall 更少 |
| F2 | 在 Wi-Fi 上連線，觀察 stall 次數 | 若 stall 很多（每分鐘數十次）、畫面明顯一頓一頓，提高 input delay 到 4–6 會改善；這是 lockstep 的本質（RTT 超過 input delay 幀數就會等），rollback（Phase 4c）會解決 |
| F3 | 電腦 B 的 nes-app 視窗**拖曳／縮放**視窗標題列 | Windows 的視窗拖曳會讓 UI 執行緒暫時停住，但 emu 執行緒與網路 poll 在另一個執行緒，連線不應中斷（畫面可能略有停頓）。這是回報用：若拖曳幾秒後連線斷了，請告訴我 |

## G. Desync（不易手動觸發；只確認說明）

正常情況下**不會**出現。若對戰中彈出紅色標題「Netplay：偵測到不同步（desync）」：視窗會列出自動存下的 replay 與
`.state`（在 `netplay_replays\`）與分析指令（`nes-test replay verify <rom> <replay>` 找出分歧幀範圍、
`nes-test diff-state` 比對兩台的 `.state`）。請把兩台的 `netplay_replays\` 內容都附上回報。（自動化測試以「把輸入套用到
錯誤的幀」注入故障，確認 desync 會在第 60 幀被偵測到；GUI 上的彈窗沒有辦法自動觸發，所以這一項沒有驗證過。）
