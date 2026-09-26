# Phase 4a 手動測試清單（replay 錄製／播放／驗證、Reset、除錯工具）

自動化測試已經涵蓋了：行為指紋（欄位完整性、與存檔格式無關、釘值）、replay 的編碼／解碼／模糊測試、
錄製→播放→驗證的邏輯、竄改輸入時的分歧範圍、emu 執行緒的錄製／播放／被停用功能（`nes-app` 的 `emu.rs`
測試，不需要視窗）、`nes-test` 的子命令。細節見 [`architecture.md`](architecture.md) §18。

**但下列項目需要眼睛與手，沒有人在真實視窗上確認過**：選單項目 disabled 的外觀與 tooltip、rfd 對話框、
狀態列文字與顏色、檢查點不符的彈出視窗、中文字型、鍵盤在播放時真的沒有作用、與真實遊戲（Spacegulls）的整段流程。
以下請用 **Spacegulls**（repo 根目錄的 `Spacegulls-1.1.nes`，已被 `.gitignore` 排除）確認。

## 準備

> **存檔有兩種，位置不同**：`Emulation` 選單的 `Save State (F5)（記憶體）`／`Load State (F9)（記憶體）` 只存在記憶體，不寫檔案；
> **寫入檔案的是 `File → Save State to File...`**（存成 `.state`，供 `nes-test diff-state` 使用）。該項目要**先載入 ROM** 才會啟用
> （沒載入時是灰的）；按下後跳出存檔對話框（預設檔名 `snapshot.state`），成功時狀態列顯示「已儲存存檔：路徑」。

```bash
cargo build --release -p nes-app -p nes-test    # 或分開建置；交付用的 nes-app 請單獨建置（見 README）
target\release\nes-app.exe
```

- 先用 CLI 記下 ROM 的識別碼：`target\release\nes-test.exe info Spacegulls-1.1.nes`，看 `rom_id:` 那一行的**前 16 字元**
  （我這份檔案是 `6d4b660b0ce685ed`；你的檔案若相同就會一樣）。GUI 狀態列會顯示 `ROM <前 16 字元>`。
- 鍵位：玩家 1＝方向鍵、Z＝B、X＝A、Enter＝Start、右 Shift＝Select；玩家 2＝W A S D、F＝B、G＝A、T＝Start、R＝Select。
- 回報失敗時請附：步驟編號、狀態列與（若有）彈出視窗的截圖、`nes-test replay info` 的輸出。

## A. Reset（一般執行）

| # | 步驟 | 預期結果 |
|---|---|---|
| A1 | 尚未載入 ROM 時開 `Emulation` 選單 | `Reset (soft reset)` 是灰的（disabled）；滑過去有 tooltip「需要載入 ROM…」 |
| A2 | `File → Open ROM...` 載入 Spacegulls，玩到遊戲中（離開標題畫面） | 狀態列出現 `ROM <16 字元>`；遊戲正常 |
| A3 | `Emulation → Reset (soft reset)` | 遊戲回到開機／標題畫面（soft reset：程式重新從 reset 向量開始）；**不會**卡死或黑畫面 |
| A4 | 暫停（`Emulation → Pause`），按 `Reset`，再按「繼續」 | 暫停中 Reset 不會立刻有畫面變化；繼續後（下一幀開始前）才 reset |

## B. 錄製約 5 分鐘（雙人操作 + 一次 Reset）

| # | 步驟 | 預期結果 |
|---|---|---|
| B1 | 滑過 `Replay → Start Recording (重新開機)` | tooltip 說明「會先重新開機（power-on）再開始錄製：replay 只包含『開機狀態 + 每幀輸入』，不含存檔…」 |
| B2 | 按下 `Start Recording` | 遊戲**從開機重新開始**（回到標題畫面，不是接著剛才的進度）；狀態列出現紅字 `[錄製中] 第 N 幀（T 秒）`，N 持續增加 |
| B3 | 玩約 5 分鐘：**兩位玩家都要有操作**（玩家 1 用方向鍵／Z／X／Enter，玩家 2 用 WASD／F／G／T），約在 2:30 時按一次 `Emulation → Reset (soft reset)` | 遊戲在 reset 後回到標題畫面；狀態列仍是 `[錄製中]`，幀數繼續增加（錄製**沒有**因為 reset 中斷） |
| B4 | **錄製中按 F9** | **被阻擋**：狀態列出現紅字「錄製中不能讀取存檔：它會破壞「從開機狀態依輸入序列執行」的前提」；遊戲畫面**沒有**跳回任何舊狀態；`[錄製中]` 的幀數繼續增加 |
| B5 | 錄製中開 `Emulation` 選單 | `Load State (F9)（記憶體）` 是灰的，滑過去有 tooltip 說明原因；`Save State (F5)（記憶體）` 仍可用；`Reset` 仍可用 |
| B6 | 錄製中開 `File` 選單 | `Open ROM...` 是灰的（tooltip 說明）；`Save State to File...` 可用 |
| B7 | 錄製中 `View → Debugger`，看控制列 | 「單步指令」與「Trace 到檔案…」是灰的；面板顯示**黃色說明文字**（「錄製／播放 replay 中停用：讀取存檔（F9）、單步指令、Trace、載入別的 ROM。…暫停與『單步一幀』仍可用」） |
| B8 | 錄製中按 Debugger 的「暫停」，再按「單步一幀」幾次 | 暫停可用；每按一次「單步一幀」，狀態列 `[錄製中]` 的幀數 +1（單步的幀**會被錄進** replay）；「單步指令」仍是灰的 |
| B9 | 按「繼續」，再玩幾秒，`Replay → Stop and Save Recording...` | 跳出存檔對話框（副檔名 `.replay`，預設檔名 `recording.replay`）；存成 `spacegulls.replay`；狀態列出現「已儲存 replay：…（N 位元組）」；`[錄製中]` 消失 |
| B10 | （選做）再錄一次，這次在存檔對話框按「取消」 | 狀態列顯示「尚未儲存 replay（可用 File → Save Recording As… 再存）」；`File → Save Recording As...` 可用，按下後可再存 |

## C. 用 GUI 驗證

| # | 步驟 | 預期結果 |
|---|---|---|
| C1 | `Replay → Play Replay...`，選 `spacegulls.replay` | 遊戲**從開機重新開始**，並自動照著錄製的操作進行；狀態列綠字 `[播放中 1x] 第 n/N 幀｜檢查點 k/K 已驗證相符｜鍵盤輸入已停用`，n、k 持續增加，**k 一直跟著前進、不會卡住** |
| C2 | 播放中亂按鍵盤（方向鍵、Z、X、WASD…） | **完全沒有作用**（遊戲照 replay 進行）；播放中的 Reset 選單項目是灰的 |
| C3 | 播放中按 F9、開 `Emulation` 選單、開 Debugger | F9 被阻擋（紅字「播放 replay 中不能讀取存檔…」）；`Load State`、`Open ROM...`、「單步指令」、Trace 都是灰的並有說明 |
| C4 | 約 2:30 處（錄製時 Reset 的位置）觀察畫面 | 遊戲在和錄製時**相同的時間點**回到標題畫面（reset 被重現）；檢查點仍全部相符 |
| C5 | `Replay → 播放速度` 選 `2x（靜音）`，再選 `最快（靜音）` | 2x：約兩倍速、沒有聲音；最快：很快跑完（約 15–20 秒內，取決於機器）、沒有聲音、畫面持續更新；兩者的狀態列檢查點數仍持續增加 |
| C6 | 播放結束 | 狀態列綠字 `[播放完成] N 幀，K 個檢查點全數相符`；模擬**自動暫停**在最後一幀；沒有彈出視窗 |
| C7 | 載入**另一個** ROM（先確認沒在錄製／播放：`File → Open ROM...` 選任何別的 `.nes`），再 `Play Replay...` 選 `spacegulls.replay` | 狀態列紅字：「無法播放 replay：replay 屬於另一份 ROM（replay 的 rom_id `6d4b660b0ce685ed`，目前載入的 `…`）」；**沒有**進入播放 |
| C8 | 選一個不是 replay 的檔案（例如隨便一個 `.txt` 改名成 `.replay`） | 狀態列紅字「replay 檔案無效：…」；不當機 |

## D. 用 CLI 驗證

```bash
target\release\nes-test.exe replay info spacegulls.replay
target\release\nes-test.exe replay verify Spacegulls-1.1.nes spacegulls.replay
echo %ERRORLEVEL%
```

| # | 步驟 | 預期結果 |
|---|---|---|
| D1 | `replay info` | 顯示：格式版本 1；錄製時的核心行為版本 3（目前的核心 3，相符）；`rom_id` 前 16 字元與 GUI／`info` 一致；總幀數約 **18000**（5 分鐘 × 60.0988，你實際錄的長度為準）；**reset: 1 次**（顯示發生在第幾幀，應大約在 2:30 ≈ 第 9000 幀附近）；檢查點每 60 幀一個 |
| D2 | `replay verify` | 印出「通過：N 幀、K 個檢查點全數相符」與耗時（約 15 秒：驗證模式關閉輸出，約 1200 幀/秒）；`%ERRORLEVEL%` 為 **0** |
| D3 | 用 `replay verify` 驗證一個別的 ROM（例如 `roms\nes-test-roms\...` 任一個）| 印出「拒絕驗證：replay 屬於另一份 ROM…」，`%ERRORLEVEL%` **非 0** |

## E. 破壞性測試（自己動手）

**E1 竄改某一幀的輸入**：把下面的腳本存成 `tamper.py`（Python 3），它把 replay 裡**單一幀**的輸入改掉並重新壓縮，檢查點原封不動。

```python
import struct, sys
src, dst, frame, mask = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4], 16)
d = open(src, 'rb').read()
(n,) = struct.unpack_from('<I', d, 30); pos = 34; frames = []
for _ in range(n):
    p1, p2, fl, c = struct.unpack_from('<BBBI', d, pos); pos += 7
    frames += [(p1, p2, fl)] * c
tail = d[pos:]
p1, p2, fl = frames[frame - 1]; frames[frame - 1] = (p1 ^ mask, p2, fl)
runs = []
for f in frames:
    if runs and runs[-1][0] == f: runs[-1][1] += 1
    else: runs.append([f, 1])
out = bytearray(d[:30]) + struct.pack('<I', len(runs))
for (p1, p2, fl), c in runs: out += struct.pack('<BBBI', p1, p2, fl, c)
open(dst, 'wb').write(out + tail)
```

```bash
python tamper.py spacegulls.replay tampered.replay 3001 FF
target\release\nes-test.exe replay verify Spacegulls-1.1.nes tampered.replay
```

| # | 步驟 | 預期結果 |
|---|---|---|
| E1 | 竄改第 3001 幀（換成你 replay 裡遊戲有在讀輸入的任一幀） | `verify` 失敗、結束碼非 0；回報「第一個不符的檢查點」與「分歧可能開始的幀範圍」，**範圍包含 3001**（間隔 60 時，範圍是 3001–3060）。（我用自己的 10000 幀 replay 做過：第 3001 幀 → 3001–3060；第 4321 幀 → 4321–4380；第 9999 幀 → 9961–10000。）**注意**：若被改的那一幀遊戲根本沒讀輸入（例如載入畫面），狀態不會有差異，`verify` 會通過——換一幀再試。 |
| E2 | 用 GUI 播放 `tampered.replay` | 播放到不符的檢查點時**立即暫停**，彈出「Replay 驗證失敗」視窗，顯示第幾幀不符、分歧範圍（包含被竄改的幀）、預期／實際指紋；狀態列紅字 `[檢查點不符]…`；視窗可關閉 |
| E3 | 用十六進位編輯器把 `spacegulls.replay` 的**最後一個位元組**改成別的值（那是最後一個檢查點指紋的最高位元組）| `verify` 失敗，指出最後一幀的檢查點不符，範圍是最後一個間隔 |
| E4 | 把 `spacegulls.replay` 截掉最後 10 個位元組 | `replay info` 與 `replay verify` 都回報「replay 檔案被截斷…」，結束碼非 0，不當機 |

## F. 存檔比對（`diff-state`）

| # | 步驟 | 預期結果 |
|---|---|---|
| F1 | GUI 播放 `spacegulls.replay`，在狀態列顯示第 **N** 幀時按「暫停」（記下暫停後狀態列的幀數 N；用 Debugger 的「單步一幀」把 N 調成整數也可以），`File → Save State to File...` 存成 `gui.state` | 狀態列顯示「已儲存存檔：…」 |
| F2 | `target\release\nes-test.exe save-state Spacegulls-1.1.nes cli.state --replay spacegulls.replay --frames N` | 印出「已寫入 cli.state：第 N 幀（行為指紋 …）」 |
| F3 | `target\release\nes-test.exe diff-state gui.state cli.state` | 印出「兩份存檔的狀態完全相同」，`%ERRORLEVEL%` 為 0（GUI 播放到第 N 幀的狀態與 CLI 重播到第 N 幀逐位元相同） |
| F4 | 再用 `--frames` 差 1（N+1）另存 `cli2.state`，`diff-state gui.state cli2.state` | 列出不同的欄位，例如 `frame_count: 0x… vs 0x…`、`ppu.…`、`ram: … 個元素不同，分成 … 段：[0x0010..=0x0013] …`（大型陣列只列索引範圍，不傾印整個陣列）；`%ERRORLEVEL%` 為 1 |

## G. 其他

| # | 步驟 | 預期結果 |
|---|---|---|
| G1 | 中文字型：檢查 `Replay` 選單、tooltip、狀態列、彈出視窗、Debugger 的黃色說明 | 中文顯示正常，沒有方框（狀態列用 `[錄製中]`／`[播放中]` 之類的文字標記，不用符號字元） |
| G2 | 在 `Replay` 選單確認 `Start Recording`／`Play Replay...` 在**尚未載入 ROM**時是灰的、tooltip 說明 | 灰的；tooltip「需要先載入 ROM，且不能已在錄製／播放中」 |
| G3 | 播放中 `Replay → Stop Replay` | 回到一般執行（從目前狀態繼續，鍵盤重新有作用）；`Open ROM...`／`Load State` 恢復可用 |
| G4 | 錄製完成後**立刻**再 `Start Recording` | 再次從開機開始（標題畫面），而不是接續上一次 |
