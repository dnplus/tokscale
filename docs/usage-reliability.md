# Usage 額度讀取穩定化：盤點與設計

狀態：設計稿，等待確認。盤點基準：`origin/main` c12e6e70（已含上游 39ecb83a／#1557）。PR #38 以 `origin/feat/submit-embedded-quota` fef31071 為準（尚未合併）。

## 1. 完成條件

1. 不論是 token 過期、401/403、429/5xx、逾時還是格式錯誤，只要讀取失敗，而且之前讀到過這個額度池，`tokscale usage`、TUI、submit 都會呈現「最後已知值＋觀測時間＋狀態」，而不是讓卡片消失。
2. 「未知」不會被畫成 0% 或 100%。
3. 每一種失敗都有一句白話的恢復方法，並且指向擁有登入資料的那個 App，例如「開一次 `claude`」。tokscale 不代替其他 App 寫入登入資料。
4. submit 上傳給 PA Cafe 的每個額度池都帶 `observedAt` 和 `status`（`ok`／`stale`／`error`／`unknown`）。
5. 每個改動都有負向對照測試：舊程式紅、新程式綠，另外有一個「不該變」的對照。

## 2. 現有機制

| 機制 | 位置 | 實際行為 |
|---|---|---|
| `UsageFreshness {fetched_at, stale}` | `usage/mod.rs:79` | **只有 Claude 會填。** CLI 卡片顯示 `Cached/Updated <UTC>`，TUI 在 stale 時用黃字。 |
| `diagnostic_disposition` | `usage/mod.rs:760` | 用 `display_name` 配對 output：有 stale output 時顯示 `showing cached usage`，有一般 output 時顯示 `usage available`，都沒有時顯示 `skipped`。所以其他 provider 失敗時一律是 `skipped`。 |
| 啟動快取 `subscription-usage-cache.json` | `usage/mod.rs:306-380` | 只給 TUI 開啟時先畫面用，300 秒過期，會丟掉 Claude 和 Warp。它不是失敗時的退路。 |
| Claude 樣本（#1557） | `claude_state.rs`、`claude.rs:770-885` | 以 access token 的 SHA-256 當 key，存在 `cache/claude-samples/`。60 秒內直接沿用不打 API，stale 最多保留 3600 秒。**只有 429、cooldown 中、另一個請求正在跑、快取目錄不能寫這四種情況會拿出 stale 樣本。** |
| Claude cooldown | `claude.rs:206-238`、`claude_state.rs` | 429 時依 `Retry-After` 決定，沒有這個 header 就用 60·2^(n-1) 秒，上限 3600 秒，寫兩份檔案。5xx 不會進 cooldown。 |
| HTTP 逾時（#1506） | `helpers.rs:11` | 30 秒，例外有：Colab 15 秒、Grok 12 秒（串行最多 4 次）、Amp CLI 15 秒、Antigravity 5 秒＋agy 35 秒。 |
| TUI 更新 | `tui/app.rs:742` | `subscription_usage = report.outputs`。**這次讀取失敗的 provider，卡片會直接從畫面上消失。** |
| 內嵌額度（PR #38） | `embedded_quota.rs` | 池狀態只有 `ok` 和 `error`，錯誤分類靠比對訊息字串。出錯時會把 windows 清成 null。**沒有 account id 也沒有 email 的 output 整個丟掉**，Claude、Colab、Warp、Z.ai、MiniMax、Kimi 等都因此不會上傳。只有報告層級的 `observedAt`，而且一律是當下時間，所以 stale 的 Claude 會被當成新資料。 |

## 3. 實測證據（沙箱 HOME＋loopback，不碰真實登入資料）

腳本在 session scratchpad 的 `ev/run.sh`：只啟用 Colab 和 Warp，其他 provider 用 `disabledProviders` 關掉，避免碰到 Keychain。

| 情境 | 實際輸出 |
|---|---|
| Colab 正常 | 卡片顯示 `12.50 CU left`，但 JSON 裡是 `remaining_percent: 100.0`，這是假百分比。 |
| Colab 401 | `Colab usage request failed (HTTP 401 Unauthorized); log in with colab-cli if authentication expired — skipped`，沒有卡片。 |
| Colab 429／503 | 和 401 **同一句話**，也叫使用者「log in」，會誤導。 |
| Colab token 過期、refresh 成功 | 正常，refresh 後的 token 只放在記憶體。 |
| Colab token 過期、refresh token 失效 | `Colab OAuth refresh failed (HTTP 400 Bad Request); log in with colab-cli — skipped` |
| Colab 卡住 | 15.5 秒後出現 `Colab usage request failed — skipped`，看不出是逾時。 |
| Colab 回 HTML 或 `{}` | `Invalid Colab usage response`／`missing a valid compute-unit balance — skipped` |
| Warp 快取 9 天前 | 卡片照常顯示，附 `last synced 9 days ago … — usage available`，但沒有 freshness，所以不會黃字也不會標示 Cached。 |
| Warp 快取少了 `workspaces` 欄位 | `Invalid Warp aggregate usage cache — skipped`，解析不夠寬容。 |
| 完全沒登入 | stdout 空白、exit 0；`--json` 印 `[]`。 |
| **Claude：先成功，再 401，再 429**（臨時 loopback 單元測試，沒有 commit） | 成功時 1 張卡。401 時 0 張卡，訊息是 `stored access token was rejected (HTTP 401)… Run 'claude'…`。接著的 429 也是 0 張卡（`HTTP 429; retry in 60s`）。**401 把上次的樣本清掉了**（`claude.rs:846`，`state.sample = None`）。 |

**Claude「閒置後讀不到」的根因**：Claude Code 的 access token 大約幾小時就會過期，tokscale 照設計不續期，所以會收到 401。這時 `claude.rs:846` 會把 token 範圍內的樣本清掉，#1557 的快取完全派不上用場。就算不清，樣本也只能活 3600 秒，閒置超過一小時一樣會不見。

## 4. 現況矩陣

代號：✅ 正常卡片。🟡 卡片＋警告。⛔ 沒有卡片，只有一行 `— skipped`。🫥 完全安靜，沒有卡片也沒有訊息。0/100 表示把未知畫成假的 0% 或 100%。∅ 卡片沒有任何一列數值。

| Provider | token 過期但可續期 | 過期且 tokscale 不續期 | 閒置很久 | 401/403 | 429/5xx | 卡住 | 格式錯 | 額度未知 | 沒登入 |
|---|---|---|---|---|---|---|---|---|---|
| Claude | 不適用（刻意不續期） | ⛔，且清掉樣本 | ⛔（樣本超過 3600 秒就丟） | ⛔，清樣本，訊息好 | 429 🟡 stale；5xx ⛔ 清樣本 | ⛔ 30 秒，清樣本 | ⛔（`utilization` 是必填欄位） | ∅ | 🫥 |
| Codex | 自己的帳號庫：續期並寫回 tokscale 自己的庫 | ⛔，訊息好（「Run 'codex'」） | ⛔ | 同左 | ⛔ 沒有 cooldown | ⛔ | ⛔ | 🟡∅ 有 Warning（upstream-small-prs 正在處理） | 🫥 |
| Cursor | 只在記憶體續期，每次執行都重新續期 | ⛔ | ⛔ | 續期後重試一次，失敗就 ⛔ | ⛔；plan/sand 失敗會被吞掉，可能退回舊算法 | ⛔ 最壞約 120 秒 | ⛔ | 省略 | 🫥 |
| Grok Bot | 不續期，過期的帳號被安靜略過 | 🫥 | 🫥 | 每個帳號各自 ⛔ | 同左 | 同左 | 同左 | 卡片隱藏（照設計） | 🫥 |
| Grok | 不續期 | ⛔，`NEEDS_AUTH` 內部字串外露 | ⛔ | ⛔，一大串合併訊息 | ⛔ | ⛔ 12 秒×4 | ⛔ | **0/100**（`grok.rs:630`、`:276`） | 🫥 |
| Copilot | 不適用 | ⛔，訊息好 | ⛔ | ⛔ | ⛔ | ⛔，原始 reqwest 錯誤 | ⛔ | unlimited 顯示 100%（有標示） | 🫥 |
| Warp | 不適用（只讀本地快取） | 不適用 | 🟡，但沒有 stale 標記（upstream-small-prs 正在處理） | 不適用 | 不適用 | 不適用 | ⛔，`workspaces` 必填 | 0/100 資訊列（有標示）；可能 ∅ | 🫥 |
| OpenCode Go | 不適用 | ⛔，訊息好 | ⛔ | 401 ⛔；403 權限不足時 🫥 | ⛔ | ⛔，原始錯誤 | ⛔ | 省略（upstream-small-prs 正在處理） | 🫥 |
| Antigravity | 交給 agy 處理，不寫入 | ⛔，訊息好 | ⛔ | 被吞掉，改走 agy | 同左 | 最壞約 45 秒 | ⛔ | 省略；可能 ∅ | 🫥 |
| Colab | 只在記憶體續期 | ⛔ | ⛔ | 續期後重試一次，失敗 ⛔ | ⛔，**訊息叫你重新登入** | ⛔ 15 秒，看不出是逾時 | ⛔ | **0/100**（CU 餘額） | 🫥 |
| Kimi | **續期，並覆寫 Kimi CLI 自己的登入檔**（只留 5 個欄位） | ⛔，`NEEDS_AUTH` 外露 | ⛔ | 續期後重試 | ⛔ | ⛔ | ⛔ | ∅ | 🫥 |
| Amp | 不適用（API key） | 不適用 | ⛔ | ⛔，只有籠統的 `HTTP 401` | ⛔ | ⛔ | ⛔ | **0/100**（美元餘額） | 🫥 |
| MiniMax | 不適用 | 不適用 | ⛔ | ⛔，訊息好 | ⛔ | ⛔，原始錯誤 | ∅ | **欄位全缺時顯示 100% 已用**（`minimax.rs:211`） | 🫥 |
| MiniMax Token Plan | 不適用 | 不適用 | — | **只有 `eprintln`，不經過 diagnostics** | 同左 | 同左 | 同左 | 省略 | 🫥 |
| Z.ai | 不適用 | 不適用 | ⛔ | ⛔，籠統訊息 | ⛔ | ⛔ | ⛔ | ∅ | 🫥 |
| Sakana | 不適用（cookie） | 不適用 | ⛔ | ⛔，訊息好 | ⛔ | ⛔ | 當成已登出 ⛔ | 省略 | 🫥 |

三個介面的對應：
- `tokscale usage`：⛔ 就是 stderr 的一行訊息。
- TUI：⛔ 就是**卡片消失**，狀態列顯示「loaded with N issues」。
- submit 內嵌額度：⛔ 只有在帶 account id 的情況下才會上傳 `status:error`，而且 windows 是 null；不帶 account 的 provider 會整個消失。

## 5. 設計

### 原則
- 一個額度池只顯示一張卡。最後已知值用來**取代**失敗的那張卡，不會多出第二張。
- refresh 一律唯讀。最後已知值只存 tokscale 自己讀到的觀測結果，不存任何登入資料，也不寫其他 App 的檔案。
- fail closed：最後已知值一定標成 `stale`，附上觀測時間，並保留原本的錯誤診斷。
- 已經過了 `resets_at` 的時窗，舊數值必定錯誤，所以改成顯示「已重置，未知」，不沿用舊數字。

### D1. Claude 不再因為 401 清掉樣本（可送上游）
- `claude.rs:846`：401/403/5xx/逾時的時候保留 token 範圍內的樣本，回傳 `limited_report`，也就是「stale 樣本＋原本的診斷」。key 本來就是 token 的雜湊，同一個 token 一定是同一個帳號，不會串到別的帳號。
- 延長 stale 可顯示的時間：從 3600 秒改成 7 天。但 `fresh()`（60 秒）和 cooldown 上限（3600 秒）不動。已經過了 `resets_at` 的時窗依上面的原則移除。
- 401 的訊息改成：「Claude Code 登入已過期，開一次 `claude` 讓它自己續期就好，不需要重新登入」。
- `Window.utilization` 改成可以缺：少了這個欄位就略過那個時窗，不讓整份解析失敗。
- 風險：上游原本的註解是「Rejected credentials must not remain eligible on startup」。我們的說法是：stale 樣本本來就不會被當成 fresh，啟動快取也已經排除 Claude。這點要在 PR 說明裡講清楚。
- 測試：負向對照是「401 之後換一個 token」→ 0 張卡；正向是「同一個 token 401」→ 1 張 stale 卡；另外「超過 resets_at 的時窗被移除」。

### D2. 通用最後已知值（可送上游）
- 在 `mod.rs` 加一層 `last_known`：每次成功就把 output 存到 `cache/usage-last-known/<provider>/<identity>.json`。identity 用 `account.id`，沒有就用 email，再沒有就用 `default`，存的時候做雜湊。
- provider 回 `Err` 而且**診斷能對應到同一個 identity** 時，用最後已知值補上卡片，`freshness = {fetched_at: 觀測時間, stale: true}`。這樣 disposition 自然會變成 `showing cached usage`，TUI 也會顯示黃字。
- 不補卡片的情況：provider 回報「沒有登入」或「沒有這個方案」（因為 `has_credentials` 是 false）；身分對不上（換了帳號）；超過 7 天。
- Claude 不走這一層，它有自己以 token 為範圍的 state。
- TUI 改成「保留這次失敗的那些卡片」，所以 `app.rs:742` 不需要另外處理。
- 測試的選擇矩陣：provider 成功或失敗 × identity 相同或不同 × 年齡在 7 天內或超過 × 有沒有被停用。

### D3. 錯誤分類與白話恢復方法（可送上游）
- `UsageFetchDiagnostic` 加一個 `reason`：`auth_expired`、`auth_missing`、`rate_limited`、`unavailable`、`timeout`、`malformed`、`unknown`。這個欄位加上 `#[serde(default)]`，舊 JSON 照樣能讀。在 provider 產生錯誤的地方就設好，不靠事後比對字串。
- `helpers` 加一個 `http_failure(provider, status, recovery)`，統一訊息格式：

  ```text
  <白話原因>；<怎麼恢復>（HTTP <code>）
  ```

  429/5xx 不再叫使用者重新登入。逾時要明講「逾時（30 秒）」。
- 順帶修正：Grok 和 Kimi 的 `NEEDS_AUTH` 外露；Amp 和 Z.ai 的 401 訊息；Colab 429/5xx 的誤導訊息；MiniMax Token Plan 改走正規的 diagnostics。

### D4. 未知和 0 分開（可送上游，避開 upstream-small-prs 已經在做的部分）
- Grok `grok.rs:630` 和 `:276`：拿不到用量時省略那個時窗，不再補 0。
- MiniMax `minimax.rs:211`：欄位全缺時省略，不再顯示 100% 已用。
- 有方案但完全沒有時窗的卡片（∅）：統一加一列「額度未知」並附 Warning，比照 Codex 現在的做法。
- 本身就沒有百分比的資訊列，例如 Colab 的 CU、Amp 的美元餘額、Warp 的花費：`UsageMetric` 加 `percent_known: bool`，預設 true，序列化時省略預設值，`usage --json` 的陣列格式不變。這幾個 provider 設成 false。TUI 不畫進度條，內嵌額度的 `remainingPercent` 送 null。
- Codex、Copilot、OpenCode Go 的未知額度，以及 Warp 的 stale，由 upstream-small-prs 負責，我等它的分支好了再接上去。

### D5. Kimi 不再寫 Kimi CLI 的登入檔（可送上游，但改變了行為，需要你同意）
- 照「不代替別的 App 寫登入資料」的原則：續期後的 token 只放在記憶體（像 Cursor、Colab 那樣），不寫回 Kimi CLI 的檔案。
- 這裡有風險：如果 Kimi 的 refresh token 會輪換，在記憶體裡續期會讓 Kimi CLI 手上的 refresh token 失效，問題就跟 Claude #1005 一樣。比較穩的做法是完全不續期，只告訴使用者「開一次 kimi」。我建議採用這個做法。

### F1. 只屬於 PA Cafe：內嵌額度帶上狀態和時間（等 #38 合併後才改 `embedded_quota.rs`）
- 每個額度池加 `observedAt`：stale 的用 `freshness.fetched_at`，其他用這次的時間。
- `status` 擴充成 `ok`、`stale`、`error`、`unknown`。`stale` 時 windows **保留**最後已知值，`error` 附 D3 的 `reason`。
- 錯誤分類改用 D3 的 `reason`，不再比對字串。
- **補上沒有身分的 provider**：Claude、Colab、Warp、Z.ai、MiniMax、Kimi 等沒有 account 也沒有 email 時，identity 改用 `provider|device`，也就是「這台機器上的這個池」。這樣才傳得上去。現在這些 provider 完全不會出現在 PA Cafe。
- 額度未知的時窗（`percent_known=false`）送 null，並帶 `label`。
- **接收端需要配合改（不在這個 repo）**：PA Cafe 的接收端在另一個 repo（PR #38 提到的 `pa-cafe` 分支 `fix/quota-readers-cards`）。`packages/frontend` 是上游 tokscale.ai 的網站，裡面沒有 quota 接收端。需要的配合：
  - 接受 `stale`／`unknown` 狀態和每個池各自的 `observedAt`。
  - 收到 `error` 時保留伺服器上次存的值，並顯示「最後更新於」。
  - 接受 `provider|device` 這種身分。

## 6. PR 拆分

| # | 分支 | 目標 | 內容 | 依賴 |
|---|---|---|---|---|
| 1 | `up/claude-keep-sample-on-auth-failure` | 從 upstream/main 開，PR 開到 dnplus | D1 | 無 |
| 2 | `up/usage-failure-reasons` | 同上 | D3 | 無 |
| 3 | `up/usage-unknown-not-zero` | 同上 | D4（只做 Grok、MiniMax、∅、`percent_known`） | 接在 upstream-small-prs 的 unknown-quota 分支之後 |
| 4 | `up/usage-last-known-good` | 同上 | D2 | 2 |
| 5 | `up/kimi-read-only-auth` | 同上 | D5 | 需要你同意 |
| 6 | `feat/embedded-quota-freshness` | 從 origin/main 開 | F1 | #38 合併；1、2 合進 fork |

上游 PR 一律不開，只推 `up/*` 分支，再在 dnplus 開 PR。

## 7. 前後對照（目標）

| 情境 | 現在 | 之後 |
|---|---|---|
| Claude token 過期（閒置） | 卡片消失，PA Cafe 收不到 | stale 卡顯示「Cached 時間」，加一句「Claude Code 登入過期，開一次 claude 就好」；PA Cafe 收到 `stale`＋`observedAt` |
| 任一 provider 401/429/5xx/逾時，之前成功過 | 卡片消失，TUI 上整張卡不見 | stale 卡＋原因＋恢復方法，最多保留 7 天 |
| 從來沒成功過，現在失敗 | `— skipped` | 一樣 `— skipped`，但原因是白話 |
| 額度未知 | 0%、100% 或空卡 | 「額度未知」，不畫進度條，PA Cafe 收到 null |
| 時窗已經過了 resets_at 的舊值 | 不適用 | 「已重置，未知」 |
| 換了帳號 | 不適用 | 不沿用舊帳號的值（負向測試） |
