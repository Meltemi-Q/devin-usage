# 采集器逆向手册

各应用用量数据的来源、格式、字段口径与验证方法。应用升级后统计失准时，
按对应小节重新核对字段即可修复。采集器实现在 `devin-usage-tray/src/agent.rs`
（`collect_*` 函数），Python 兼容版在 `devin_usage.py`。

通用约定：

- 所有事件写 `usage_events(app,event_key,ts,day,model,kind,session_id,
  tok_in,tok_out,tok_cache_read,tok_cache_write,cost_usd,meta,device)`，
  `INSERT OR IGNORE` 幂等，`event_key` 为去重主键
- 会话写 `local_sessions`，`session_id` 带应用前缀（`claude:xxx`、`codex:xxx`），
  `source` 列区分应用
- JSONL 类数据源用 kv 表 `off:{app}:{path}` 记字节偏移增量读（文件只增不改）
- **token 口径坑**：grok/zcode/codex 的 `inputTokens`/`input_tokens` **包含缓存**，
  入库时已扣除；claude/devin 的 input 不含缓存，直接用
- 配额写 `app_quota(app,label,ts,pct剩余,used,limit,resets_at,meta,device)`

## Devin（桌面 App + CLI + Cloud）

### 配额 `collect_quota`

- 端点 `POST server.codeium.com/exa.seat_management_pb.SeatManagementService/GetUserStatus`
- Connect JSON：`body.metadata.apiKey` 带 token，**无 Auth header**
- 返回：周配额剩余%、日/周重置、超额余额 USD、plan 周期、全模型 creditMultiplier
- token 来源：Devin `config.json`/凭证文件，运行时现读，重新登录自动生效

### 云端会话 `collect_cloud`

- `GET api.devin.ai/v3/organizations/{org}/sessions`（Bearer session token）
- org_id：Windows 读 `config.json`，Mac 无此文件时 `GET /v3/self` 自动发现
- 字段：`acus_consumed`、状态、PR 数 → `cloud_sessions`

### 本地会话 + 消息级 token `collect_local`

- 库：`sessions.db`（**只读**）
  - Win `%APPDATA%/Devin/cli/` · macOS/Linux `~/.local/share/devin/cli/`
- `sessions` 表：模型/mode/cwd/起止/消息数；`metadata.client_meta
  ["cognition.ai/requestingTabId"]` 存在 → `kind=app`（桌面端），否则 `cli`
- **消息级事件**（关键修复）：`message_nodes` 表每条 assistant 消息
  `chat_message.metadata.metrics` = `{input/output/cache_read/cache_creation
  _tokens, total_time_ms, ttft_ms, tpot_ms}`，`metadata.generation_model` =
  真实调用模型，`message_nodes.created_at` = 消息时间戳（秒）
- 归日按**消息时间**而非会话创建日——跨天会话不会把历史 token 堆到创建日
- `compactor` 是 Devin 内部上下文压缩模型，单独成族正常
- 打开方式：`sqlite3 'file:...sessions.db?mode=ro'`；macOS 上 URI 模式曾打不开，
  `_ro()` 有普通只读兜底

## Cursor

### 服务端计费事件 `collect_cursor`（最准）

- `GET cursor.com/api/dashboard/export-usage-events-csv`（Cookie
  `WorkosCursorSessionToken={userId}%3A%3A{accessToken}`）
- token：`state.vscdb` 的 `cursorAuth/*` 读，过期走
  `api2.cursor.sh/oauth/token` 刷新（公开 client_id），刷新结果只写自己 kv
- CSV 每次请求一行：Input(w/wo Cache Write)/Cache Read/Output/Cost
  （Ultra 订阅内 Cost=`Included`）→ **计费级真实数据**
- CSV 的 input 字段本身就分离缓存，无重复计问题
- `cost_usd` 列存订阅外实扣金额

### 本地会话与配额

- 会话：`state.vscdb` 只读 `composerHeaders` + `cursorDiskKV`
  （不逐条读 bubble——库太大）
- 日行：`ItemTable` 的 `aiCodeTracking.dailyStats.*`（tab 建议/采纳行数）
- 配额：`GET cursor.com/api/usage-summary` → `individualUsage.plan`：
  `used/limit`（included 额度，美分级）+ `totalPercentUsed`（仪表盘口径
  百分比）+ `autoPercentUsed`/`apiPercentUsed`（Auto 路由 vs API 价
  请求的成分占比）+ `billingCycleEnd` 重置日。`onDemand` 未启用为 null
- **配额是账号级数据**：多设备同账号时面板按 label 取最新 ts 合并成一行
  （两头消耗同一池，重复显示没意义）。所有 app 的配额行都按此口径归并

## Antigravity（Google IDE）

### 会话 token（protobuf 逆向）

- 库：`~/.gemini/antigravity*/conversations/*.db` 只读，`gen_metadata` 表
- protobuf 字段：`field1{19:modelID 21:label 4:usage{1:系统prompt 2:输入
  3:输出 5:缓存读} 9:timing{4:时间戳}}`；时间戳缺失回退 `steps.metadata`
- `input` 含缓存读（Gemini API 惯例）——已扣除
- `trajectory_meta` 取 tid 为空串时所有库事件 key 相撞 → 全被 IGNORE，
  取值逻辑 `(meta[0] or meta[1]) or p.stem` 兜底

### 配额（language server）

- 本机 `language_server` 进程 `127.0.0.1:port`，命令行
  `--app_data_dir antigravity` + `--csrf_token` 定位，netstat/lsof 找端口
- 主接口 `RetrieveUserQuotaSummary`：**每池 × 周+5小时双限额**
  （Gemini / Claude·GPT 两池，remainingFraction + resetTime）
- 旧的 `GetUserStatus` 只有 5h 限额，不是周限额——别用错
- IDE 未开时无端口 → skip；仅运行时可用

## ZCode

- `~/.zcode/cli/agents/*/*/transcript.jsonl`（增量偏移读）
- `model_complete.usage{input/output/cacheRead/cacheWriteTokens}`；
  模型名由同 turn `model_request.payload.model` 关联（`uuid/name` 取末段）
- `inputTokens` 含缓存读+缓存写——已扣除
- 无配额接口

## Grok（grok CLI / grok-build）

### 会话

- `~/.grok/sessions/*/*/updates.jsonl`（增量偏移读）+
  `summary.json` 会话元数据
- `turn_completed.usage{input/output/cachedRead/cacheCreationTokens,
  modelCalls, apiDurationMs}` + **`costUsdTicks` 真实成本**（1e-9 USD）
  + `modelUsage` 分模型明细
- `inputTokens` 含 `cachedReadTokens`——已扣除
- 会话 ts 是**秒**（不是毫秒）

### 配额（CLI billing 面板同款接口）

- `GET https://cli-chat-proxy.grok.com/v1/billing?format=credits`
  - **`?format=credits` 是关键**——不带参数只回月度账单（订阅制全 0）
- 返回：`currentPeriod{type:WEEKLY,start,end}`、`creditUsagePercent`
  （已用%→入库转剩余%）、`productUsage`（GrokChat/GrokBuild 分产品）、
  `isUnifiedBillingUser`（true 时共享周池）
- 认证：`~/.grok/auth.json` access token（JWT 带 `grok-cli:access` scope），
  **只在未过期时探测**，refresh 交给 CLI 自己（OIDC refresh 会轮换）
- 必须头：`x-grok-client-version`（读 `~/.grok/version.json`），缺了 4xx
- 套餐名（SuperGrok Heavy）在 `settings_cache.json` 的
  `payload.settings.subscription_tier_display`（payload 是二次 JSON 字符串）
- 推理响应也带 CF 限流头 `x-ratelimit-remaining-*`，但采集不烧额度走 billing

## Claude Code

- `~/.claude/projects/**/*.jsonl`（增量偏移读；旧版本会清理老文件，
  早期 token 明细本地可能已不在）
- 每条 assistant 消息 `message.usage{input/output/cache_read_input/
  cache_creation_input_tokens}` + `message.model` + `message.id`
- **按 `message.id` 去重**：流式写入会重复落盘同一条消息
- `input_tokens` **不含**缓存（与 grok/codex 相反！）直接用
- `server_tool_use`（web_search/fetch 次数）、`service_tier` 进 meta
- 注意非 Claude 模型也可能出现（用户配过路由转发会记录真实模型）

## Codex（Codex CLI + Codex Desktop，一家一套数据）

- `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` +
  `~/.codex/archived_sessions/*.jsonl`（增量偏移读）
- 桌面 App 和 CLI **写同一目录**，一个采集器全覆盖
- 事件类型（外层 `{timestamp,ordinal,type,payload}`）：
  - `session_meta`：session_id、cwd、originator（Codex Desktop）、
    cli_version、`model_provider`（cli_proxy/wawapi/openai——用户路由）
  - `turn_context`：`turn_id → model` 映射（每轮真实模型，
    走反代时是 gpt-6-astra/gemini-3-flash 等）
  - **`token_usage_record`**：per-response `{session_id,turn_id,
    response_id,usage{input,cached_input,cache_write_input,output,
    reasoning_output,total}}` → `event_key=codex:{response_id}`
  - `event_msg`/`token_count`：`info` 累计 + `rate_limits
    {primary,secondary}`（ChatGPT 直连账号才有；走代理时 null）
- token 口径：`input_tokens` 含 cached + cache_write（OpenAI 惯例）——已扣除；
  `reasoning_output_tokens` ⊂ output_tokens，只进 meta
- 配额：rate_limits 非空时写 app_quota（primary/secondary 双窗口）；
  本机走代理全是 null 属正常
- auth：`~/.codex/auth.json` chatgpt OAuth 三件套（暂只用于将来官方配额接口）

## 验证方法（统计失准时的排查顺序）

1. `collect` 输出里该应用是 `skip` 还是数字——skip 看 `runs` 日志里的原因
   （路径不存在/端口没起/token 过期）
2. 直接数源数据：JSONL 行数、sqlite 表行数、protobuf 解析条数，
   和 `usage_events` 里该 app 的行数对账
3. 对不上的常见原因：增量偏移 kv 错乱（删 `off:{app}:*` 重扫）、
   event_key 撞车（空 sid/turn 字段）、except 吞错（临时去掉 try 打异常）、
   input 含缓存重复计、跨天会话按创建日归账
4. 配额不对：先确认接口口径（remaining% vs used%、5h vs 周 vs 月窗口），
   和应用自家 UI/用量页对同一时刻的数字

## 升级跟进清单

应用更新后统计失效按此排查：

- 数据源路径改了 → 更新 root 目录探测
- JSONL 字段名/嵌套改了 → 对照新旧各打一条事件比字段
- sqlite schema 变了 → `.schema` 对照采集 SQL
- protobuf 字段号变了 → 重新看字段（`gen_metadata` 那套是猜的，
  label/usage/timing 大括号结构一般稳定）
- 配额接口 401/404 → 在应用二进制里搜端点字符串找新接口
  （`strings *.exe | grep -iE "quota|billing|usage|rate"`）
- token/凭证位置变了 → 看应用新版本的 auth 文件结构
