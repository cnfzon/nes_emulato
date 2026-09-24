# Phase 3 手動測試清單（需要 GUI 與真實遊戲，交給人確認）

自動化測試驗證了 mapper 的暫存器邏輯、bank 對應、mirroring 查詢、存讀檔與 rollback 重播，
但那些都是**合成 ROM** 與公開的 test ROM；**沒有**用真實遊戲驗證過 MMC1／UxROM／CNROM，
也沒有驗證過 Debugger「Mapper」分頁與玩家 2 按鍵在 GUI 上的實際行為。以下請用你自己合法取得
的 ROM 手動確認。

## 準備

```bash
cargo build --release -p nes-app
cargo run -p nes-test -- info <rom.nes>     # 看 "Mapper: N" 與 "Mirroring: ..."
target\release\nes-app.exe
```

- `File → Open ROM...` 載入；`View → Debugger` 開面板，切到 **Mapper** 分頁（顯示 mapper
  名稱、bank 暫存器的原始值，以及它們**目前造成的實際 bank 對應**）。
- 按鍵：玩家 1＝方向鍵、Z = B、X = A、Enter = Start、右 Shift = Select；
  玩家 2＝W/A/S/D、F = B、G = A、R = Select、T = Start；F5 存檔、F9 讀檔。
- 目前**沒有聲音**（APU 尚未實作）。**沒有 Reset 選單項**（只有載入 ROM 會重來）。
- 每個遊戲用 `nes-test info` 確認 mapper 編號再測；下面列的遊戲只是常見例子，我沒有驗證過你手上
  那個版本的 mapper 編號。
- **回報失敗時請附**：ROM 名稱、`nes-test info` 的輸出、發生時 Mapper 分頁的截圖（bank 暫存器的值
  對除錯最有用）。

## A. MMC1（mapper 1）

常見例子：Metroid、The Legend of Zelda、Mega Man 2。

| # | 步驟 | 預期結果 |
|---|---|---|
| A1 | `nes-test info` | `Mapper: 1`；載入不出現錯誤訊息 |
| A2 | 載入後看標題畫面；Mapper 分頁 | 標題畫面圖塊與文字正常（不是亂碼或全黑）。Mapper 分頁標題是「MMC1（mapper 1）」，開機時 PRG 模式是 3（`$8000` 切換、`$C000` 固定最後一個 bank） |
| A3 | 開始遊戲、走過幾個場景／房間，同時看 Mapper 分頁 | 「PRG bank」「CHR bank 0/1」的值會隨場景變化，「PRG $8000-$BFFF」的 bank 編號跟著變；`$C000-$FFFF` 維持最後一個 bank（模式 3 時）；遊戲**沒有當機、沒有整片亂碼** |
| A4 | 觀察圖塊：切換場景後，背景與精靈的圖塊 | 每個場景的圖塊都正確（沒有拿到別的場景的圖塊、沒有缺塊或錯位）——這是 CHR bank 切換正確的證據 |
| A5 | **Mirroring**：玩會捲動畫面的部分（Metroid 的橫向／縱向房間切換、Zelda 的地圖捲動），Mapper 分頁看「Mirroring」列，Nametable 分頁看 4 張圖 | 「Mirroring」會在 Horizontal / Vertical（有的遊戲還有單畫面）之間變；Nametable 分頁的 4 張圖的相同配對隨之改變；**捲動時不出現重複的畫面、閃爍或接縫錯位**。這是 Phase 3 新增的「PPU 每次存取都查 mapper 的 mirroring」的主要驗證 |
| A6 | 有 PRG-RAM 的遊戲（Zelda 建立存檔、Metroid 用密碼——密碼不需要 RAM）：Zelda 建立角色並存檔進入遊戲 | 遊戲正常進入；Mapper 分頁「PRG-RAM」為「啟用」。**電池存檔不會寫到磁碟**（尚未實作），關閉程式後存檔會消失，這是預期的 |
| A7 | 遊玩途中按 F5，換到**另一個場景／房間**（bank 與 mirroring 都不同），按 F9 | 立刻回到按 F5 的場景，圖塊、捲動、Mapper 分頁的暫存器值都回到存檔時的內容，遊戲可繼續正常玩（驗證 mapper 狀態進存檔） |
| A8 | 先載入 MMC1 遊戲 A，按 F5；再載入另一個遊戲 B，按 F9 | 讀檔被拒絕並顯示錯誤（ROM 不符），遊戲 B 不受影響 |
| A9 | 已知限制對照：留意畫面上有沒有「在一條掃描線中間才切 CHR bank」的特效（例如狀態列與遊戲區交界） | 可能出現 **1 條掃描線**的圖塊錯位（渲染以掃描線為單位，CHR bank 切換要到下一條線才反映，見 architecture.md §13.2）。**若看到，請回報是哪個遊戲、哪個位置、是否穩定**；這不算 Phase 3 的 bug，但要知道有沒有遊戲受影響 |

## B. UxROM（mapper 2）

常見例子：Contra、Castlevania、Mega Man。

| # | 步驟 | 預期結果 |
|---|---|---|
| B1 | `nes-test info` | `Mapper: 2`；載入無錯誤 |
| B2 | 標題畫面；Mapper 分頁 | 圖塊與文字正常；Mapper 分頁顯示「UxROM（mapper 2）」，「PRG $C000-$FFFF」是「bank N（固定最後一個）」 |
| B3 | 開始遊戲、過關或切換場景，看 Mapper 分頁 | 「bank 暫存器」與「PRG $8000-$BFFF」的 bank 編號隨關卡變化；`$C000` 的 bank 不變；遊戲沒有當機 |
| B4 | UxROM 遊戲通常用 **CHR-RAM**（圖塊由程式上傳）：觀察各關卡的背景與精靈 | 圖塊正確、沒有亂碼；Pattern 分頁的兩張 tile 圖有內容（不是全空白） |
| B5 | 進入遊戲後長時間遊玩（打完一關以上） | 沒有隨機當機、畫面亂掉。**若某個關卡切換後當機或亂碼，請回報**：本專案**不模擬 bus conflict**（architecture.md §16.5），少數遊戲若靠它取值會出問題，這是第一個要懷疑的地方 |
| B6 | F5 存檔、換到別的關卡（bank 不同）、F9 | 回到存檔時的關卡與 bank，可繼續遊玩 |

## C. CNROM（mapper 3）

常見例子：Arkanoid、Solomon's Key。

| # | 步驟 | 預期結果 |
|---|---|---|
| C1 | `nes-test info` | `Mapper: 3`；載入無錯誤 |
| C2 | 標題畫面；Mapper 分頁 | 圖塊與文字正常；Mapper 分頁顯示「CNROM（mapper 3）」與「CHR bank 暫存器」 |
| C3 | 進入遊戲、切換關卡或畫面，看 Mapper 分頁與畫面 | 「CHR bank 暫存器」與「CHR $0000-$1FFF」的 8KB bank 編號會變；每個畫面的圖塊都正確（CNROM 靠換整組 CHR 換畫面），沒有拿到別的畫面的圖塊 |
| C4 | Pattern 分頁 | 兩張 tile 圖隨 CHR bank 切換而改變 |
| C5 | PRG 固定：不論怎麼玩，Mapper 分頁沒有 PRG bank 的切換項目 | 遊戲不因 PRG 而當機 |
| C6 | F5 存檔、換到別的畫面、F9 | CHR bank 與圖塊回到存檔時的內容 |
| C7 | bus conflict 提醒 | 同 B5：若切換 CHR bank 後畫面亂掉，請回報（未模擬 bus conflict） |

## D. 玩家 2 鍵盤

任選一個支援雙人**同時**遊玩的遊戲（例如 Contra 的 2P 模式；Super Mario Bros. 是輪流，不適合）。

| # | 步驟 | 預期結果 |
|---|---|---|
| D1 | 選 2 PLAYER 開始 | 兩個角色都出現 |
| D2 | 只按 W/A/S/D、F、G | 只有**玩家 2** 的角色移動／跳／射擊；玩家 1 不動 |
| D3 | 只按方向鍵、Z、X | 只有**玩家 1** 動；玩家 2 不動 |
| D4 | 兩邊同時操作 | 兩個角色各自依自己的按鍵行動，互不干擾 |
| D5 | 玩家 2 的 T（Start）、R（Select） | 可用於選單／暫停（依遊戲而定） |
| D6 | 同時按住較多鍵（例如玩家 1 的 ↑ + ← + Z 與玩家 2 的 W + D + F） | 多數鍵盤在這種組合會有 ghosting（某些鍵被吞掉）。**若某個組合失效，請記下是哪幾個鍵**——這是鍵盤硬體限制，不是模擬器的錯；想順暢雙人遊玩需要實體手把（尚未實作，需要新增依賴，等你決定） |
| D7 | F5／F9 | 仍然是存檔／讀檔，沒有被玩家 2 的按鍵蓋掉 |

## E. Debugger「Mapper」分頁本身

| # | 步驟 | 預期結果 |
|---|---|---|
| E1 | 載入 NROM 遊戲（Donkey Kong、Super Mario Bros.）開 Mapper 分頁 | 顯示「NROM（mapper 0）」與「PRG：N 個 16KB bank，無切換」 |
| E2 | 中文字型：分頁名稱與列名（「PRG 模式」「移位暫存器」「啟用／停用」等） | 中文是正常字形，不是方框 |
| E3 | 面板開著時遊玩 | 數值隨 bank 切換即時更新；面板關閉時不影響遊戲速度 |
| E4 | MMC1 遊戲：「移位暫存器」列 | 通常顯示「已寫入 0 / 5 bit」（一次序列寫入很快完成，很少停在中間） |

## F. 從 Phase 2 延續：時序模型改變後的回歸確認

Phase 3 把 catch-up 改成分段（architecture.md §13.1）。Phase 2 手動測試的 B4 曾說「若看到狀態列與遊戲畫面
交界有 1 條掃描線的錯位，要回報，這會決定要不要採用拆段 catch-up」。**現在已採用**，請用 Super Mario
Bros. 再確認一次：

| # | 步驟 | 預期結果 |
|---|---|---|
| F1 | 進入 1-1，按住右鍵水平捲動，觀察狀態列（MARIO／金幣／WORLD／TIME）與遊戲區交界 | 狀態列固定不動；交界處**沒有** 1 條線的閃爍或抖動（若仍看到，請回報是穩定的一條線還是隨捲動閃爍） |
| F2 | Donkey Kong 走一關 | 與 Phase 2 相同：精靈與背景正常、沒有多出的殘影 |
| F3 | F5／F9 存讀檔（任何 NROM 遊戲） | 正常。F5／F9 的存檔只存在**記憶體**的一個欄位（不寫磁碟，關閉程式即消失），所以不存在「舊版存檔」的相容問題；存檔格式加上版本標頭後，若之後版本不同才會被拒絕（見 architecture.md §15） |
