# devin-usage

Devin App / CLI / Cloud 的本地用量统计 + 系统托盘工具。

逆向自 Devin Desktop 的真实数据面：**配额余额、云端 ACU、本地每个会话的真实
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

前置要求：Python 3（仅标准库）；托盘需要 Rust 工具链构建
（`cd devin-usage-tray && cargo build --release`），不构建则只装采集。

## 功能

- `devin_usage.py`：单文件 Python（**仅标准库**），采集 + 报表 + 实时监控
- `devin-usage-tray`：Rust 托盘图标，60s 刷新配额/SWE-2 用量，图标随配额变色
- **多应用覆盖**：Devin App/CLI/Cloud + Cursor + Antigravity
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

### Antigravity

| 源 | 位置 | 采到什么 |
|---|---|---|
| 本地会话库 | `~/.gemini/antigravity*/conversations/*.db` 只读，`gen_metadata` 表 protobuf | 每次生成的**真实 token**：`field1{19:modelID 21:label 4:usage{1:系统prompt 2:输入 3:输出 5:缓存读} 9:timing{4:时间戳}}`，时间戳缺失时回退 `steps.metadata` |

解码方法参考开源实现 [openusage#1139](https://github.com/robinebers/openusage/pull/1139)。
`.pb` 旧格式/加密文件跳过。

## 用法

```bash
python devin_usage.py collect          # 采集一轮（~30s，幂等；--only 可单采某源）
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

**面板模式**（`--panel`）：egui 轻量窗口——顶部 Devin / Cursor / Antigravity
三个 Tab，各应用套餐/配额/计费完全独立、互不混计：

- **Devin**：配额进度条 + 配额趋势线（仅 Devin 有配额概念）、SWE-2 汇总、
  每日 token 堆叠柱、分模型族/具体型号表、等效成本
- **Cursor**：套餐（plan）+ 会话/请求概要、每日 token 柱（usage_events 事件级）、
  分模型表（含订阅外实扣）、行级采纳统计
- **Antigravity**：会话/生成概要、每日 token 柱、分模型表、按 API 价折算

每个 Tab 的图表/构成条/模型表/底部成本行都只统计所选应用。
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
