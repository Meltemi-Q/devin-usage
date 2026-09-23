# devin-usage

Devin / Cursor / Antigravity / ZCode / Grok / Claude Code 的本地用量统计 +
系统托盘工具，跨设备汇总。

逆向自各应用的真实数据面：**配额余额、云端 ACU、每个会话的真实
token（输入/输出/缓存读/缓存写）、按模型等效美元折算**。
Windows + macOS + Linux（含无头 VPS）。

## 一键安装

| 平台 | 命令 | 装了什么 |
|---|---|---|
| Windows | `powershell -File install-task.ps1` | 15 分钟定时采集（任务计划）+ 托盘开机自启 |
| macOS | `bash install-agent-macos.sh` | 15 分钟定时采集（crontab）+ 托盘常驻（launchd，RunAtLoad+KeepAlive）+ 面板 .app |
| Linux/VPS | `bash install-agent-linux.sh` | 15 分钟定时采集（crontab）；无 GUI 不装托盘 |

卸载：各脚本加 `--uninstall` / `-Uninstall` 参数。

**采集是双保险的**：托盘进程自身每 15min 也会采一轮（进程在就有采集，
笔记本睡醒自然续上），任务计划/cron 是托盘没跑时的备份。
界面刷新每 60s；"立即采集"立即触发一轮。

前置要求：**只需 Rust 工具链**（`cd devin-usage-tray && cargo build --release`），
单个静态二进制完成采集+同步+托盘+面板。`devin_usage.py` 保留为兼容参考实现，
常规使用不再需要 Python。

## 功能

- `devin-usage-tray`：Rust 单二进制——托盘图标（60s 刷新配额、图标随配额变色）
  + 内置 15min 自动采集 + `--panel` 可视化面板 + `collect/export/import/sync` 子命令
- **多应用覆盖**：Devin App/CLI/Cloud + Cursor + Antigravity + ZCode + Grok +
  Claude Code；面板顶部 Tab 完全隔离（各应用套餐/配额/计费独立），另有"总览"Tab
- **多设备汇总**：每行数据打 `device` 标签（hostname），SSH 双向同步到 VPS 中心库；
  面板设备选择器 `全部 / Win / Mac / VPS` 切换，离线也能看所有设备
- 数据存自己的 `data/usage.db`，对各应用的库**只读不写**；token 运行时现读，
  重新登录自动生效；采集幂等可反复跑

## 数据源（逆向结果）

### Devin

| 源 | 端点/位置 | 采到什么 |
|---|---|---|
| 配额 | `POST server.codeium.com/exa.seat_management_pb.SeatManagementService/GetUserStatus`（Connect JSON，apiKey 在 body.metadata，无 Auth header） | 周配额剩余%、日/周重置时间、超额余额 USD、plan 周期、全部模型的 creditMultiplier |
| 云端 | `GET api.devin.ai/v3/organizations/{org}/sessions`（Bearer session token） | 云端会话列表 + `acus_consumed`、状态、PR 数 |
| 本地 | `sessions.db`（只读）：Win `%APPDATA%/Devin/cli/` · macOS/Linux `~/.local/share/devin/cli/` | 全部本地会话：模型/mode/cwd/起止/消息数/工具调用数；`requestingTabId` 区分 App/CLI；每条 assistant 消息 `metadata.metrics` 含真实 token + ttft/tpot |

**token 用量**：`message_nodes.chat_message.metadata.metrics` 持久化了每次响应的
`input_tokens`/`output_tokens`/`cache_read_tokens`/`cache_creation_tokens`，
采集时聚合到 `local_sessions.tok_*`。计费信号另看周配额%与云端 ACU。

### Cursor

| 源 | 位置 | 采到什么 |
|---|---|---|
| 服务端导出 | `GET cursor.com/api/dashboard/export-usage-events-csv`（Cookie `WorkosCursorSessionToken={userId}%3A%3A{accessToken}`，token 从 `state.vscdb` 的 `cursorAuth/*` 读，过期走 `api2.cursor.sh/oauth/token` 刷新，刷新结果只写自己的 kv） | **计费级真实数据**：每次请求的 input(w/wo cache write)/cache-read/output/total + Cost 列（Ultra 订阅内为 `Included`） |
| 本地会话 | `state.vscdb` 只读：`composerHeaders` + `cursorDiskKV` 的 `composerData:*` | 会话列表、标题、模型、消息数（headers 推算，不逐条读 bubble——11GB 库逐条读太慢） |
| 日行统计 | `ItemTable` 的 `aiCodeTracking.dailyStats.*` | tab/composer 每日建议与采纳行数 |
| 配额 | `GET cursor.com/api/usage-summary` | used/limit/remaining + auto/api 分别用量百分比 + **billingCycleEnd 重置日** |

### Antigravity

| 源 | 位置 | 采到什么 |
|---|---|---|
| 本地会话库 | `~/.gemini/antigravity*/conversations/*.db` 只读，`gen_metadata` 表 protobuf | 每次生成的**真实 token**：`field1{19:modelID 21:label 4:usage{1:系统prompt 2:输入 3:输出 5:缓存读} 9:timing{4:时间戳}}`，时间戳缺失时回退 `steps.metadata` |
| 配额 | 本机 language server `127.0.0.1:port`（进程命令行 `--app_data_dir antigravity` + `--csrf_token` 定位，netstat/lsof 找端口，POST `GetUserStatus`/`GetCommandModelConfigs`） | **每模型配额**：`quotaInfo{remainingFraction,resetTime}`；仅 IDE 运行时可用，IDE 关闭记 skip |

配额快照存 `app_quota` 表（app+label+ts 幂等），面板各应用 Tab 顶部显示进度条和重置倒计时。

### ZCode

| 源 | 位置 | 采到什么 |
|---|---|---|
| 会话转录 | `~/.zcode/cli/agents/*/*/transcript.jsonl`（增量读：kv 记字节偏移，JSONL 只增不改） | `model_complete.usage{input/output/cacheRead/cacheWriteTokens}` 真实 token；模型名由同 turn 的 `model_request.payload.model` 关联（`uuid/name` 取末段） |

### Grok（grok CLI / grok-build）

| 源 | 位置 | 采到什么 |
|---|---|---|
| 会话更新流 | `~/.grok/sessions/*/*/updates.jsonl`（增量读） | `turn_completed.usage{input/output/cachedRead/cacheCreationTokens, modelCalls, apiDurationMs}` + **`costUsdTicks` 真实成本**（1e-9 USD）+ `modelUsage` 分模型明细 |
| 会话元数据 | `~/.grok/sessions/*/*/summary.json` | 模型、cwd、消息数、起止时间 |

ZCode 无公开配额接口，面板显示"无配额口径"。

### Grok（grok CLI / grok-build）

|| 源 | 位置 | 采到什么 |
|---|---|---|---|
|| 会话更新流 | `~/.grok/sessions/*/*/updates.jsonl`（增量读） | `turn_completed.usage{input/output/cachedRead/cacheCreationTokens, modelCalls, apiDurationMs}` + **`costUsdTicks` 真实成本**（1e-9 USD）+ `modelUsage` 分模型明细 |
|| 会话元数据 | `~/.grok/sessions/*/*/summary.json` | 模型、cwd、消息数、起止时间 |
|| 配额 | `GET cli-chat-proxy.grok.com/v1/billing?format=credits`（access token 读 `~/.grok/auth.json`，仅在未过期时探测，刷新交给 CLI） | **周额度**已用%、`productUsage` 分产品（GrokChat/GrokBuild）、套餐名、`currentPeriod` 重置时间 |

注意：grok/zcode 的 `inputTokens` **已含缓存读**，采集时已扣除避免重复计。

### Claude Code

|| 源 | 位置 | 采到什么 |
|---|---|---|---|
|| 会话转录 | `~/.claude/projects/*/*.jsonl`（增量读字节偏移） | 每条 assistant 消息的 `message.usage`：input/output/cache_read/cache_creation 真实 token + 模型名；按 `message.id` 去重（流式重发只计一次） |

Claude 的 `input_tokens` 本身不含缓存（与 grok 相反），无需扣除。
注意：Claude Code 会清理旧 transcript，本地只保留近期会话的 token 明细。

解码方法参考开源实现 [openusage#1139](https://github.com/robinebers/openusage/pull/1139)。
`.pb` 旧格式/加密文件跳过。

## 用法

```bash
devin-usage-tray collect          # 采集一轮（幂等；末尾自动 ssh 同步到配置的 peer）
devin-usage-tray export > x.ndjson   # 增量导出（kv 游标水位；--since ts 显式水位）
devin-usage-tray import < x.ndjson   # 导入 NDJSON（INSERT OR IGNORE 幂等）
devin-usage-tray sync             # 双向同步：先推本地增量给 peer，再拉回 peer 数据
```

同步配置（写在本地库 kv 表，或用环境变量）：

```bash
# 在库里配置一次：
sqlite3 data/usage.db "INSERT OR REPLACE INTO kv VALUES('sync.peer','vps');"
sqlite3 data/usage.db "INSERT OR REPLACE INTO kv VALUES('sync.remote','/root/devin-usage');"
# sync.peer = ssh 别名（~/.ssh/config 里的 Host）；sync.remote = 对端项目目录
```

架构：`Win/Mac ──push/pull──> VPS(中心库)`，VPS 自身也跑 collect 采 CLI 类数据。
每行带 `device` 标签；账号级数据（Cursor CSV、Grok 配额）按 event_key 天然去重。

Python 兼容实现（保留，功能相同）：

```bash
python devin_usage.py collect          # 采集一轮（--only 可单采某源）
python devin_usage.py report           # 全部汇总；--today/--week/--month/--json
python devin_usage.py quota            # 配额快照 + 最近趋势
python devin_usage.py sessions -n 20   # 会话明细（本地+云端）
python devin_usage.py watch -i 10      # 实时监控（每 N 秒重采 local 打一行）
python devin_usage.py runs             # 采集健康日志
```

## 等效成本

按公开 API 刊例价折算（每 1M token，in/out/cache读/cache写 分别计价）。
规则表写入 `model_prices`，report 与托盘共用。自定义/纠正价格 →
`data/prices.json`：

```json
{"rules":[["swe-2", 3.0, 15.0, 0.30, 3.75]]}
```

`swe-2` 无公开价，默认按 Sonnet 档折算并标注 `proxy:sonnet`——这是"走 API 值
多少钱"的等效估算，不是 Devin 实际扣费（Pro 实际按周配额计）。

## 托盘工具（Rust）

```
配额[Pro]: 周剩 50% · 超额 $1.14
SWE-2 近7天: N会话 · 入X.XM · 出XXk · 缓XXXM
等效成本: 7天 $X · SWE-2 $Y · 累计 $Z
  swe-2-max  7d:N会话 出XXk ≈$Z | 总:N会话 ≈$W
[立即采集] [打开面板] [复制文本报告] [退出]
```

图标 = Devin logo 白卡 + 配额状态点（绿≥50% / 琥珀≥20% / 红<20%）。

```bash
cd devin-usage-tray && cargo build --release
# Windows: target\release\devin-usage-tray.exe
# macOS:   target/release/devin-usage-tray
```

**面板模式**（`--panel`）：egui 轻量窗口——顶部 Devin / Cursor / Antigravity /
ZCode / Grok / Claude / 总览 Tab，各应用套餐/配额/计费完全独立、互不混计；
第二行**设备选择器**（全部 / 各 hostname）过滤全部图表与配额：

- **Devin**：配额进度条 + 配额趋势线（仅 Devin 有配额概念）、SWE-2 汇总、
  每日 token 堆叠柱、分模型族/具体型号表、等效成本
- **Cursor**：套餐（plan）+ 会话/请求概要、每日 token 柱（usage_events 事件级）、
  分模型表（含订阅外实扣）、行级采纳统计
- **Antigravity**：周+5h 双配额进度条、会话/生成概要、每日 token 柱、分模型表
- **ZCode / Grok / Claude**：会话/事件概要、每日 token 柱、分模型表；
  Grok 额外有周配额条和真实扣费合计

每个 Tab 的图表/构成条/模型表/底部成本行都只统计所选应用与所选设备。
[立即采集]/[复制报告]/[置顶] 固定底栏。适合菜单栏拥挤图标被系统隐藏、
或想要桌面小组件的场景；托盘菜单"打开面板"可直接唤起。

macOS 打 App：`bash bundle-macos.sh` 生成 `~/Applications/DevinUsage.app`
（含 icns 图标），Dock/Spotlight 可启动，不占菜单栏。

依赖：tray-icon + winit + eframe(egui) + rusqlite(bundled) + arboard + image。
Windows 构建需要链接器：`x86_64-pc-windows-gnu` 工具链 + mingw（或 MSVC）。
exe 从 `target/release/` 向上自动定位 `devin_usage.py`/`data/usage.db`。

`--dump-icon` 可导出 4 种状态图标 PNG 预览。

## 平台差异备忘

**macOS**
- Devin 数据目录 `~/Library/Application Support/Devin`；CLI 会话库在
  `~/.local/share/devin/cli/sessions.db`
- 无 `config.json` → org_id 由 `GET /v3/self` 自动发现
- token 顺序：`credentials.toml` → `data/devin-token.txt`。Mac 上没登录过 CLI
  时放一份 token 到 token.txt 即可；正式做法是在 Mac 跑一次 `devin auth login`。
  （macOS 钥匙串 OSCrypt 解密需要 GUI 会话，SSH 里拿不到）

**Linux / 无头 VPS**
- 只需 `devin` CLI 登录过一次（`devin auth login`），凭证在
  `~/.local/share/devin/credentials.toml` 或 `~/.config/devin/credentials.toml`，
  两处都会自动探测
- CLI 会话库同样在 `~/.local/share/devin/cli/sessions.db`
- 无托盘（无 GUI）；用 `report`/`watch`/`sessions` 子命令查看，
  或把 `data/usage.db` 拉回有托盘的机器看

## 失效时的维护点

1. **token 失效** → `devin auth login` 重新登录（token 现读，不用改代码）
2. **GetUserStatus 拒 JSON** → 回退 protobuf（body = `proto{1: Metadata{f3: api_key}}`），
   或起本地 language_server 转发
3. **sessions.db schema 变更** → `sqlite3 sessions.db .schema` 对照 `collect_local` 的 SQL
4. **价格口径** → `data/prices.json` 覆盖，无需改代码

## License

MIT
