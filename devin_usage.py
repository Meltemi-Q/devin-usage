# -*- coding: utf-8 -*-
"""devin_usage.py — Devin App / CLI / Cloud 用量采集与报表

数据源（全部实测可用，2026-09-17）：
  1. quota    POST https://server.codeium.com/exa.seat_management_pb.SeatManagementService/GetUserStatus
              Connect JSON 直连（无 Authorization header，apiKey 放 body.metadata）
              → 周配额剩余%、超额余额(micros USD)、plan 周期、配额重置时间、模型 creditMultiplier
  2. cloud    GET  https://api.devin.ai/v3/organizations/{org}/sessions
              Bearer devin-session-token → 云端会话 + acus_consumed
  3. local    cli/sessions.db（只读打开；Win %APPDATA%/Devin/cli，mac/Linux ~/.local/share/devin/cli）
              → 本地会话(App+CLI)：模型/mode/cwd/起止/标题/消息数/工具调用数
              metadata.client_meta["cognition.ai/requestingTabId"] 存在 → App 会话，否则 CLI

存储：本目录 data/usage.db（自己的库，绝不写 Devin 的库）。
只依赖 Python 标准库。token 每次运行时现读 credentials.toml，重新登录后自动生效。

用法：
  python devin_usage.py collect            # 采集一轮（幂等，可反复跑）
  python devin_usage.py report             # 汇总报表（本周）
  python devin_usage.py report --today|--week|--month|--all
  python devin_usage.py quota              # 只看配额
  python devin_usage.py sessions -n 20     # 最近会话明细
  python devin_usage.py report --json      # JSON 输出
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import re
import socket
import sqlite3
import ssl
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
DATA_DIR = HERE / "data"
DB_PATH = DATA_DIR / "usage.db"

IS_MAC = sys.platform == "darwin"
IS_WIN = sys.platform == "win32"
if IS_MAC:
    DEVIN_DIR = Path.home() / "Library/Application Support/Devin"
    CLI_DIR = Path.home() / ".local/share/devin/cli"
elif IS_WIN:
    APPDATA = Path(os.environ.get("APPDATA", str(Path.home() / "AppData/Roaming")))
    DEVIN_DIR = APPDATA / "Devin"
    CLI_DIR = DEVIN_DIR / "cli"
else:  # Linux：CLI-only，凭证分散在 XDG data 与 XDG config
    XDG_DATA = Path(os.environ.get("XDG_DATA_HOME", str(Path.home() / ".local/share")))
    XDG_CONF = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config")))
    DEVIN_DIR = XDG_DATA / "devin"
    CLI_DIR = DEVIN_DIR / "cli"
    LINUX_DIRS = [DEVIN_DIR, XDG_CONF / "devin"]
CRED_FILES = [DEVIN_DIR / "credentials.toml", CLI_DIR / "credentials.toml"]
if not IS_WIN and not IS_MAC:
    CRED_FILES = [d / "credentials.toml" for d in LINUX_DIRS] + CRED_FILES
    CONFIG_FILES = [d / "config.json" for d in LINUX_DIRS]
else:
    CONFIG_FILES = [DEVIN_DIR / "config.json"]
LOCAL_DB = CLI_DIR / "sessions.db"
TOKEN_FILE = DATA_DIR / "devin-token.txt"   # 无 credentials 时的手动 token 兜底

API_SERVER = "https://server.codeium.com"
DEVIN_API = "https://api.devin.ai"
GUS_RPC = f"{API_SERVER}/exa.seat_management_pb.SeatManagementService/GetUserStatus"

UA = "devin-usage/1.0"

# 本机设备标识：USAGE_DEVICE 环境变量可覆盖（用于多设备汇总区分来源）
DEVICE = os.environ.get("USAGE_DEVICE") or socket.gethostname() or "unknown"


# ---------------------------------------------------------------- creds

def read_token() -> str:
    """session token：credentials.toml（Win/Mac CLI 登录产物）→ data/devin-token.txt 兜底。"""
    for p in CRED_FILES:
        try:
            m = re.search(r'windsurf_api_key\s*=\s*"([^"]+)"', p.read_text(encoding="utf-8"))
            if m:
                return m.group(1)
        except OSError:
            continue
    try:
        t = TOKEN_FILE.read_text(encoding="utf-8").strip()
        if t:
            return t
    except OSError:
        pass
    raise RuntimeError(f"找不到 token（{CRED_FILES[0]} 或 {TOKEN_FILE}）")


def read_org(token=None) -> "str | None":
    for p in CONFIG_FILES:
        try:
            return json.loads(p.read_text(encoding="utf-8"))["devin"]["org_id"]
        except Exception:
            continue
    if token:  # Mac 无 config.json → 用 /v3/self 发现
        try:
            req = urllib.request.Request(f"{DEVIN_API}/v3/self",
                                         headers={"Authorization": f"Bearer {token}",
                                                  "User-Agent": UA})
            return json.loads(urllib.request.urlopen(req, timeout=15).read())["org_id"]
        except Exception:
            pass
    return None


# ---------------------------------------------------------------- store

SCHEMA = """
CREATE TABLE IF NOT EXISTS quota_snapshots (
  ts INTEGER PRIMARY KEY,
  plan_name TEXT, teams_tier TEXT,
  weekly_quota_remaining_pct REAL,
  overage_balance_micros INTEGER,
  available_prompt_credits INTEGER,
  plan_start TEXT, plan_end TEXT,
  daily_reset INTEGER, weekly_reset INTEGER,
  n_models INTEGER,
  raw_json TEXT
);
CREATE TABLE IF NOT EXISTS cloud_sessions (
  session_id TEXT PRIMARY KEY,
  title TEXT, status TEXT, status_detail TEXT, origin TEXT, category TEXT,
  user_id TEXT, created_at INTEGER, updated_at INTEGER,
  acus_consumed REAL, n_prs INTEGER,
  first_seen INTEGER, last_seen INTEGER
);
CREATE TABLE IF NOT EXISTS local_sessions (
  session_id TEXT PRIMARY KEY,
  source TEXT,                 -- 'app' | 'cli' | 'unknown'
  model TEXT, agent_mode TEXT, backend_type TEXT, cwd TEXT, title TEXT,
  created_at INTEGER, last_activity_at INTEGER,
  credit_cost REAL, acu_cost REAL,
  n_user INTEGER, n_assistant INTEGER, n_tool INTEGER, n_system INTEGER,
  n_tool_calls INTEGER, n_prompts INTEGER,
  tok_in INTEGER, tok_out INTEGER, tok_cache_read INTEGER, tok_cache_write INTEGER,
  gen_ms INTEGER,
  first_seen INTEGER, last_seen INTEGER
);
CREATE TABLE IF NOT EXISTS model_multipliers (
  model_uid TEXT PRIMARY KEY, label TEXT, credit_multiplier REAL,
  updated_at INTEGER
);
CREATE TABLE IF NOT EXISTS model_prices (
  prio INTEGER PRIMARY KEY,
  prefix TEXT, in_per_1m REAL, out_per_1m REAL, cr_per_1m REAL, cw_per_1m REAL,
  note TEXT
);
CREATE TABLE IF NOT EXISTS collect_runs (
  ts INTEGER, source TEXT, status TEXT, detail TEXT
);
-- 事件级用量账本：Cursor CSV(真实计费口径) + Antigravity gen_metadata(本地真实计数)
CREATE TABLE IF NOT EXISTS usage_events (
  app TEXT NOT NULL,              -- 'cursor' | 'antigravity' | ...
  event_key TEXT NOT NULL,
  ts INTEGER NOT NULL,            -- unix 秒
  day TEXT NOT NULL,              -- 本地 YYYY-MM-DD
  model TEXT,
  kind TEXT,                      -- cursor: Included/On-demand...; antigravity: 'gen'
  session_id TEXT,                -- 可关联的会话/trajectory id
  tok_in INTEGER DEFAULT 0, tok_out INTEGER DEFAULT 0,
  tok_cache_read INTEGER DEFAULT 0, tok_cache_write INTEGER DEFAULT 0,
  cost_usd REAL,                  -- CSV 真实扣费；Included 为 NULL
  meta TEXT,                      -- json 附加
  PRIMARY KEY(app, event_key)
);
-- 每日活动量（Cursor tab/composer 行数等，非 token 指标）
CREATE TABLE IF NOT EXISTS daily_activity (
  app TEXT NOT NULL, day TEXT NOT NULL, metric TEXT NOT NULL, value INTEGER,
  PRIMARY KEY(app, day, metric)
);
-- 各应用配额快照：cursor=usage-summary，antigravity=本地 language server
CREATE TABLE IF NOT EXISTS app_quota (
  app TEXT NOT NULL, label TEXT NOT NULL, ts INTEGER NOT NULL,
  pct_remaining REAL,             -- 剩余百分比 0-100
  used REAL, lim REAL,            -- 原始用量/上限（单位随应用）
  resets_at INTEGER,              -- unix 秒，可空
  meta TEXT,                      -- json 附加（套餐/成员类型等）
  PRIMARY KEY(app, label, ts)
);
-- 自有 kv：存放刷新后的 cursor access token 等（不改对方库）
CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT);
CREATE INDEX IF NOT EXISTS idx_local_created ON local_sessions(created_at);
CREATE INDEX IF NOT EXISTS idx_cloud_created ON cloud_sessions(created_at);
CREATE INDEX IF NOT EXISTS idx_events_day ON usage_events(day);
CREATE INDEX IF NOT EXISTS idx_events_model ON usage_events(app, model);
"""


def db() -> sqlite3.Connection:
    DATA_DIR.mkdir(exist_ok=True)
    c = sqlite3.connect(DB_PATH)
    # daily_activity 老表主键无 device——重建（PK 无法用 ALTER 改）
    da_cols = {r[1] for r in c.execute("PRAGMA table_info(daily_activity)")}
    if da_cols and "device" not in da_cols:
        c.execute("ALTER TABLE daily_activity RENAME TO daily_activity_old")
        c.executescript("""CREATE TABLE daily_activity (
          app TEXT NOT NULL, day TEXT NOT NULL, metric TEXT NOT NULL, value INTEGER,
          device TEXT DEFAULT '', PRIMARY KEY(app, day, metric, device));
          INSERT OR REPLACE INTO daily_activity (app,day,metric,value,device)
            SELECT app,day,metric,value,'' FROM daily_activity_old;
          DROP TABLE daily_activity_old;""")
        c.commit()
    c.executescript(SCHEMA)
    # 轻量迁移：老库补 token 列
    have = {r[1] for r in c.execute("PRAGMA table_info(local_sessions)")}
    for col in ("tok_in", "tok_out", "tok_cache_read", "tok_cache_write", "gen_ms"):
        if col not in have:
            c.execute(f"ALTER TABLE local_sessions ADD COLUMN {col} INTEGER")
    # 多设备标签列（Rust 采集器已用；此处同步迁移保持兼容）
    for tbl in ("usage_events", "app_quota", "local_sessions",
                "cloud_sessions", "quota_snapshots", "daily_activity"):
        cols = {r[1] for r in c.execute(f"PRAGMA table_info({tbl})")}
        if "device" not in cols:
            c.execute(f"ALTER TABLE {tbl} ADD COLUMN device TEXT DEFAULT ''")
    # 一次性修正：grok/zcode/antigravity 旧行的 tok_in 含缓存，扣除重复部分
    if not c.execute(
            "SELECT 1 FROM kv WHERE key='fix_cache_dedup_v1'").fetchone():
        c.execute("""UPDATE usage_events
                     SET tok_in = MAX(0, tok_in - tok_cache_read - tok_cache_write)
                     WHERE app IN ('grok','zcode','antigravity')""")
        c.execute("INSERT OR REPLACE INTO kv VALUES('fix_cache_dedup_v1','1')")
        c.commit()
    return c


def device() -> str:
    """本机标签（同步汇总的行归属标识）。"""
    return os.environ.get("USAGE_DEVICE") or os.environ.get(
        "COMPUTERNAME") or os.environ.get("HOSTNAME") or \
        subprocess.run(["hostname"], capture_output=True, text=True
                       ).stdout.strip() or "unknown"


def _tag_device(c):
    """collect 末尾：本机新采的未打标签行统一补 device。"""
    for tbl in ("usage_events", "local_sessions", "app_quota",
                "cloud_sessions", "quota_snapshots", "daily_activity"):
        c.execute(f"UPDATE {tbl} SET device=? WHERE device IS NULL OR device=''",
                  (device(),))
    c.commit()


def log_run(c, source, status, detail=""):
    c.execute("INSERT INTO collect_runs VALUES (?,?,?,?)",
              (int(time.time()), source, status, detail[:500]))


def _ro(path) -> sqlite3.Connection:
    """只读打开第三方 sqlite：优先 URI mode=ro，失败（macOS URI 解析问题）退回普通连接。
    普通连接也绝不写入——只用 SELECT。"""
    try:
        con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        con.execute("PRAGMA query_only=1")
        return con
    except sqlite3.Error:
        con = sqlite3.connect(str(path))
        con.execute("PRAGMA query_only=1")
        return con


# ---------------------------------------------------------------- collect

def _post_json(url, payload, timeout=25):
    req = urllib.request.Request(
        url, data=json.dumps(payload).encode(), method="POST",
        headers={"Content-Type": "application/json",
                 "Connect-Protocol-Version": "1", "User-Agent": UA})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def collect_quota(c, token):
    """GetUserStatus → quota_snapshots + model_multipliers。"""
    meta = {"ideName": "windsurf", "ideVersion": "3.10.27",
            "extensionName": "windsurf", "extensionVersion": "3.10.27",
            "apiKey": token, "locale": "en", "os": "Windows",
            "sessionId": str(uuid.uuid4()), "requestId": "1"}
    j = _post_json(GUS_RPC, {"metadata": meta})
    us = j.get("userStatus", {})
    ps = us.get("planStatus", {})
    pi = ps.get("planInfo", {})
    models = (us.get("cascadeModelConfigData", {}) or {}).get("clientModelConfigs", [])
    now = int(time.time())
    # 5 分钟内重复 collect 不重复写快照
    last = c.execute("SELECT MAX(ts) FROM quota_snapshots").fetchone()[0] or 0
    if now - last >= 300:
        c.execute("""INSERT INTO quota_snapshots
            (ts,plan_name,teams_tier,weekly_quota_remaining_pct,
             overage_balance_micros,available_prompt_credits,plan_start,plan_end,
             daily_reset,weekly_reset,n_models,raw_json,device)
            VALUES (?,?,?,?,?,?,?,?,?,?,?,NULL,?)""",
                  (now, pi.get("planName"), pi.get("teamsTier"),
                   ps.get("weeklyQuotaRemainingPercent"),
                   _int(ps.get("overageBalanceMicros")),
                   _int(ps.get("availablePromptCredits")),
                   ps.get("planStart"), ps.get("planEnd"),
                   _int(ps.get("dailyQuotaResetAtUnix")),
                   _int(ps.get("weeklyQuotaResetAtUnix")),
                   len(models), device()))
        c.execute("INSERT INTO kv(k,v) VALUES('quota.devin.raw',?) "
                  "ON CONFLICT(k) DO UPDATE SET v=excluded.v", (json.dumps(j),))
    for m in models:
        uid = m.get("modelUid")
        if uid:
            c.execute("""INSERT INTO model_multipliers VALUES (?,?,?,?)
                         ON CONFLICT(model_uid) DO UPDATE SET
                         label=excluded.label, credit_multiplier=excluded.credit_multiplier,
                         updated_at=excluded.updated_at""",
                      (uid, m.get("label"), _f(m.get("creditMultiplier")), now))
    log_run(c, "quota", "ok",
            f"weekly_remaining={ps.get('weeklyQuotaRemainingPercent')}% "
            f"overage={_int(ps.get('overageBalanceMicros'))}")
    return ps


def collect_cloud(c, token, org):
    """v3 REST 云端会话 → cloud_sessions（upsert）。"""
    if not org:
        log_run(c, "cloud", "skip", "no org_id in config.json")
        return 0
    now = int(time.time())
    n = 0
    cursor = None
    seen_first = set()
    for _ in range(50):                       # 页数上限保护
        url = f"{DEVIN_API}/v3/organizations/{org}/sessions?limit=100"
        if cursor:
            url += f"&cursor={cursor}"
        req = urllib.request.Request(url, headers={
            "Authorization": f"Bearer {token}", "User-Agent": UA})
        with urllib.request.urlopen(req, timeout=25) as r:
            page = json.loads(r.read())
        items = page.get("items", [])
        if not items:
            break
        if items[0].get("session_id") in seen_first:
            break                             # cursor 参数无效时防死循环
        seen_first.add(items[0].get("session_id"))
        for it in items:
            sid = it.get("session_id")
            if not sid:
                continue
            c.execute("""INSERT INTO cloud_sessions
                (session_id,title,status,status_detail,origin,category,user_id,
                 created_at,updated_at,acus_consumed,n_prs,first_seen,last_seen)
                VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)
                ON CONFLICT(session_id) DO UPDATE SET
                 title=excluded.title, status=excluded.status,
                 status_detail=excluded.status_detail, origin=excluded.origin,
                 category=excluded.category, updated_at=excluded.updated_at,
                 acus_consumed=excluded.acus_consumed, n_prs=excluded.n_prs,
                 last_seen=excluded.last_seen""",
                (sid, it.get("title"), it.get("status"), it.get("status_detail"),
                 it.get("origin"), it.get("category"), it.get("user_id"),
                 _int(it.get("created_at")), _int(it.get("updated_at")),
                 _f(it.get("acus_consumed")), len(it.get("pull_requests") or []),
                 now, now))
            n += 1
        if not page.get("has_next_page"):
            break
        cursor = page.get("end_cursor")
        if not cursor:
            break
    log_run(c, "cloud", "ok", f"{n} sessions")
    return n


def collect_local(c):
    """本地 sessions.db（只读）→ local_sessions（upsert + 消息统计）。"""
    if not LOCAL_DB.exists():
        log_run(c, "local", "skip", f"{LOCAL_DB} 不存在")
        return 0
    now = int(time.time())
    src = _ro(LOCAL_DB)
    n = 0
    rows = src.execute("""
        SELECT s.id, s.model, s.agent_mode, s.backend_type, s.working_directory,
               s.title, s.created_at, s.last_activity_at, s.metadata,
               (SELECT count(*) FROM message_nodes m
                 WHERE m.session_id=s.id AND json_extract(m.chat_message,'$.role')='user'),
               (SELECT count(*) FROM message_nodes m
                 WHERE m.session_id=s.id AND json_extract(m.chat_message,'$.role')='assistant'),
               (SELECT count(*) FROM message_nodes m
                 WHERE m.session_id=s.id AND json_extract(m.chat_message,'$.role')='tool'),
               (SELECT count(*) FROM message_nodes m
                 WHERE m.session_id=s.id AND json_extract(m.chat_message,'$.role')='system'),
               (SELECT count(*) FROM tool_call_state t WHERE t.session_id=s.id),
               (SELECT count(*) FROM prompt_history p WHERE p.session_id=s.id)
        FROM sessions s WHERE COALESCE(s.hidden,0)=0""").fetchall()
    for (sid, model, mode, backend, cwd, title, created, active, meta_s,
         nu, na, nt, ns, ntc, np_) in rows:
        meta = {}
        try:
            meta = json.loads(meta_s) if meta_s else {}
        except Exception:
            pass
        cm = meta.get("client_meta") or {}
        source = ("app" if cm.get("cognition.ai/requestingTabId") else
                  "cli" if model else "unknown")
        c.execute("""INSERT INTO local_sessions
            (session_id,source,model,agent_mode,backend_type,cwd,title,
             created_at,last_activity_at,credit_cost,acu_cost,
             n_user,n_assistant,n_tool,n_system,n_tool_calls,n_prompts,
             first_seen,last_seen)
            VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
            ON CONFLICT(session_id) DO UPDATE SET
             source=excluded.source, model=excluded.model,
             agent_mode=excluded.agent_mode, backend_type=excluded.backend_type,
             cwd=excluded.cwd, title=excluded.title,
             last_activity_at=excluded.last_activity_at,
             credit_cost=excluded.credit_cost, acu_cost=excluded.acu_cost,
             n_user=excluded.n_user, n_assistant=excluded.n_assistant,
             n_tool=excluded.n_tool, n_system=excluded.n_system,
             n_tool_calls=excluded.n_tool_calls, n_prompts=excluded.n_prompts,
             last_seen=excluded.last_seen""",
            (sid, source, model, mode, backend, cwd, title, created, active,
             _f(meta.get("total_credit_cost")), _f(meta.get("total_acu_cost")),
             nu, na, nt, ns, ntc, np_, now, now))
        n += 1
    # 真实 token 用量：每个 assistant 消息的 metadata.metrics
    # {input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, total_time_ms}
    for row in src.execute("""
        SELECT session_id,
               sum(json_extract(chat_message,'$.metadata.metrics.input_tokens')),
               sum(json_extract(chat_message,'$.metadata.metrics.output_tokens')),
               sum(json_extract(chat_message,'$.metadata.metrics.cache_read_tokens')),
               sum(json_extract(chat_message,'$.metadata.metrics.cache_creation_tokens')),
               sum(json_extract(chat_message,'$.metadata.metrics.total_time_ms'))
        FROM message_nodes
        WHERE json_extract(chat_message,'$.role')='assistant'
        GROUP BY session_id""").fetchall():
        c.execute("""UPDATE local_sessions SET tok_in=?, tok_out=?, tok_cache_read=?,
                     tok_cache_write=?, gen_ms=? WHERE session_id=?""",
                  (_int(row[1]), _int(row[2]), _int(row[3]), _int(row[4]),
                   _int(row[5]), row[0]))
    # 消息级事件：按消息时间归日（会话级 tok_* 全堆在创建日，
    # 跨天会话会把历史消耗算到创建日 → usage_events 按 node 时间戳记）
    for row in src.execute("""
        SELECT m.session_id, m.node_id, m.created_at,
               json_extract(m.chat_message,'$.metadata.generation_model'),
               json_extract(m.chat_message,'$.metadata.metrics.input_tokens'),
               json_extract(m.chat_message,'$.metadata.metrics.output_tokens'),
               json_extract(m.chat_message,'$.metadata.metrics.cache_read_tokens'),
               json_extract(m.chat_message,'$.metadata.metrics.cache_creation_tokens'),
               json_extract(m.chat_message,'$.metadata.metrics.total_time_ms'),
               s.model, s.metadata
        FROM message_nodes m JOIN sessions s ON s.id = m.session_id
        WHERE COALESCE(s.hidden,0)=0
          AND json_extract(m.chat_message,'$.role')='assistant'
          AND json_extract(m.chat_message,'$.metadata.metrics.input_tokens') IS NOT NULL""").fetchall():
        sid, nid, ts, gmodel, ti, to, tcr, tcw, gms, smodel, meta_s = row
        if not ts:
            continue
        try:
            meta = json.loads(meta_s) if meta_s else {}
        except Exception:
            meta = {}
        kind = "app" if (meta.get("client_meta") or {}).get("cognition.ai/requestingTabId") else "cli"
        model = gmodel or smodel or "?"
        c.execute("""INSERT OR IGNORE INTO usage_events
                     (app,event_key,ts,day,model,kind,session_id,
                      tok_in,tok_out,tok_cache_read,tok_cache_write,cost_usd,meta,device)
                     VALUES('devin',?,?,?,?,?,?,?,?,?,?,NULL,?,?)""",
                  (f"devin:{sid}:{nid}", ts,
                   datetime.fromtimestamp(ts).strftime("%Y-%m-%d"),
                   model, kind, sid,
                   _int(ti), _int(to), _int(tcr), _int(tcw),
                   json.dumps({"gen_ms": gms}), device()))
    src.close()
    sync_prices(c)
    log_run(c, "local", "ok", f"{n} sessions")
    return n


# ---------------------------------------------------------------- cursor

CURSOR_EXPORT_URL = "https://cursor.com/api/dashboard/export-usage-events-csv"
CURSOR_REFRESH_URL = "https://api2.cursor.sh/oauth/token"
CURSOR_CLIENT_ID = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB"   # Cursor 官方公开 client_id


def _cursor_state_db() -> Path:
    if IS_MAC:
        return Path.home() / "Library/Application Support/Cursor/User/globalStorage/state.vscdb"
    if IS_WIN:
        return APPDATA / "Cursor/User/globalStorage/state.vscdb"
    xdg = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config")))
    return xdg / "Cursor/User/globalStorage/state.vscdb"


def _jwt_payload(tok: str) -> dict:
    try:
        part = tok.split(".")[1]
        part += "=" * (-len(part) % 4)
        return json.loads(base64.urlsafe_b64decode(part))
    except Exception:
        return {}


def _cursor_item(key: str) -> "str | None":
    """从 Cursor state.vscdb ItemTable 读一个值（只读）。"""
    dbp = _cursor_state_db()
    try:
        con = _ro(dbp)
        row = con.execute("SELECT value FROM ItemTable WHERE key=?", (key,)).fetchone()
        con.close()
        return row[0] if row else None
    except Exception:
        return None


def cursor_access_token(c) -> "str | None":
    """kv 里刷新过的 token 优先；否则读 Cursor 自己的 ItemTable。
    临期/过期时用 refreshToken 换新（结果只写自己的 kv，不动 Cursor 的库）。"""
    tok = c.execute("SELECT value FROM kv WHERE key='cursor.access'").fetchone()
    rt = c.execute("SELECT value FROM kv WHERE key='cursor.refresh'").fetchone()
    access, refresh = (tok and tok[0]), (rt and rt[0])
    if not access:
        access = _cursor_item("cursorAuth/accessToken")
        refresh = _cursor_item("cursorAuth/refreshToken") or refresh
    if not access and not refresh:
        return None
    exp = _jwt_payload(access).get("exp", 0) if access else 0
    if exp - time.time() > 300:          # 还有 >5min，直接用
        return access
    if not refresh:
        return access                    # 过期但没 refresh，先凑合
    try:
        req = urllib.request.Request(
            CURSOR_REFRESH_URL, method="POST",
            data=json.dumps({"grant_type": "refresh_token",
                             "client_id": CURSOR_CLIENT_ID,
                             "refresh_token": refresh}).encode(),
            headers={"Content-Type": "application/json", "User-Agent": UA})
        r = json.loads(urllib.request.urlopen(req, timeout=20).read())
        new_tok = r.get("access_token")
        if new_tok:
            c.execute("INSERT OR REPLACE INTO kv VALUES('cursor.access',?)", (new_tok,))
            if r.get("refresh_token"):
                c.execute("INSERT OR REPLACE INTO kv VALUES('cursor.refresh',?)",
                          (r["refresh_token"],))
            return new_tok
    except Exception:
        pass
    return access


def _collect_cursor_csv(c, token) -> int:
    """服务端 CSV 导出 → usage_events（真实 token + 真实 Cost 列）。"""
    import csv
    import hashlib
    import io
    pay = _jwt_payload(token)
    uid = (pay.get("sub") or "").split("|")[-1]
    if not uid:
        raise RuntimeError("cursor JWT 无 sub")
    end = int(time.time() * 1000)
    start = end - 60 * 86400 * 1000       # 滚动 60 天窗口，INSERT OR IGNORE 去重
    url = (f"{CURSOR_EXPORT_URL}?startDate={start}&endDate={end}&strategy=tokens")
    req = urllib.request.Request(url, headers={
        "Cookie": f"WorkosCursorSessionToken={uid}%3A%3A{token}",
        "Accept": "text/csv", "User-Agent": UA})
    body = urllib.request.urlopen(req, timeout=40).read().decode("utf-8", "replace")
    if not body.startswith("Date"):
        raise RuntimeError(f"CSV 响应异常: {body[:80]}")
    seen = {}
    n = 0
    for row in csv.DictReader(io.StringIO(body)):
        def gi(col):
            try:
                return int((row.get(col) or "0").replace(",", "").strip() or 0)
            except ValueError:
                return 0
        canon = "|".join(str(row.get(k, "")) for k in
                         ("Date", "Kind", "Model", "Max Mode", "Input (w/ Cache Write)",
                          "Input (w/o Cache Write)", "Cache Read", "Output Tokens"))
        h = hashlib.sha1(canon.encode()).hexdigest()[:16]
        seen[h] = seen.get(h, 0) + 1                      # 同内容并发行去重
        key = f"{h}:{seen[h]}"
        try:
            ts = int(datetime.fromisoformat(
                row["Date"].replace("Z", "+00:00")).timestamp())
        except Exception:
            continue
        cost_raw = (row.get("Cost") or "").strip()
        try:
            cost = float(cost_raw.lstrip("$")) if cost_raw not in ("", "Included") else None
        except ValueError:
            cost = None
        day = datetime.fromtimestamp(ts).strftime("%Y-%m-%d")
        model = (row.get("Model") or "").strip()
        kind = (row.get("Kind") or "").strip()
        meta = json.dumps({"max_mode": (row.get("Max Mode") or "").strip() == "Yes",
                           "cost_raw": cost_raw,
                           "agent_id": row.get("Cloud Agent ID") or ""})
        cur = c.execute("""INSERT OR IGNORE INTO usage_events
            (app,event_key,ts,day,model,kind,session_id,
             tok_in,tok_out,tok_cache_read,tok_cache_write,cost_usd,meta)
            VALUES('cursor',?,?,?,?,?,?,?,?,?,?,?,?)""",
            (key, ts, day, model, kind, row.get("Cloud Agent ID") or None,
             gi("Input (w/ Cache Write)") + gi("Input (w/o Cache Write)"),
             gi("Output Tokens"), gi("Cache Read"), gi("Input (w/ Cache Write)"),
             cost, meta))
        n += cur.rowcount
    return n


def _collect_cursor_local(c) -> int:
    """state.vscdb 只读：composerHeaders + bubbles → local_sessions(source='cursor')；
    aiCodeTracking.dailyStats → daily_activity。增量：只扫 lastUpdatedAt 变了的 composer。"""
    dbp = _cursor_state_db()
    if not dbp.exists():
        return 0
    src = _ro(dbp)
    now = int(time.time())
    n = 0
    try:
        headers = src.execute(
            "SELECT composerId, createdAt, lastUpdatedAt, value FROM composerHeaders").fetchall()
    except sqlite3.Error:
        headers = []
    for cid, created_ms, updated_ms, hval in headers:
        have = c.execute("SELECT last_seen FROM local_sessions WHERE session_id=?",
                         (f"cursor:{cid}",)).fetchone()
        if have and have[0] and updated_ms and updated_ms // 1000 <= have[0]:
            continue                            # 未变化，跳过
        # 逐 bubble 读 value 在 11GB 库上太贵（~23s/会话）。消息数从
        # composerData.fullConversationHeadersOnly 推——单行一次读。
        row = src.execute("SELECT value FROM cursorDiskKV WHERE key=?",
                          (f"composerData:{cid}",)).fetchone()
        name = None
        try:
            name = json.loads(hval).get("name") if hval else None
        except Exception:
            pass
        nu = na = 0
        model = None
        if row and row[0]:
            try:
                cd = json.loads(row[0])
                name = name or cd.get("name")
                model = ((cd.get("modelConfig") or {}).get("modelName")) or None
                for b in cd.get("fullConversationHeadersOnly") or []:
                    if b.get("type") == 1:
                        nu += 1
                    elif b.get("type") == 2:
                        na += 1
            except Exception:
                pass
        c.execute("""INSERT INTO local_sessions
            (session_id,source,model,title,created_at,last_activity_at,
             n_user,n_assistant,n_tool,n_system,n_tool_calls,n_prompts,
             tok_in,tok_out,tok_cache_read,tok_cache_write,first_seen,last_seen)
            VALUES (?,?,?,?,?,?,?,?,0,0,0,?,0,0,0,0,?,?)
            ON CONFLICT(session_id) DO UPDATE SET
             model=excluded.model, title=excluded.title,
             last_activity_at=excluded.last_activity_at,
             n_user=excluded.n_user, n_assistant=excluded.n_assistant,
             n_prompts=excluded.n_prompts,
             last_seen=excluded.last_seen""",
            (f"cursor:{cid}", "cursor", model, name,
             (created_ms or 0) // 1000, (updated_ms or 0) // 1000,
             nu, na, nu, now, now))
        n += 1
    # Tab/Composer 日行统计
    for (key, val) in src.execute(
            "SELECT key,value FROM ItemTable WHERE key LIKE 'aiCodeTracking.dailyStats.%'"):
        try:
            d = json.loads(val)
            day = d.get("date") or key.rsplit(".", 1)[-1]
            for metric in ("tabSuggestedLines", "tabAcceptedLines",
                           "composerSuggestedLines", "composerAcceptedLines"):
                if metric in d:
                    c.execute("INSERT OR REPLACE INTO daily_activity "
                              "(app,day,metric,value,device) "
                              "VALUES('cursor',?,?,?,?)",
                              (day, metric, int(d[metric] or 0), device()))
        except Exception:
            continue
    src.close()
    return n


CURSOR_USAGE_URL = "https://cursor.com/api/usage-summary"


def _cursor_usage_summary(c, token) -> int:
    """Cursor 配额：api/usage-summary → app_quota。
    返回 used/limit/percentUsed + billingCycleEnd（=额度重置日）。"""
    pay = _jwt_payload(token)
    uid = (pay.get("sub") or "").split("|")[-1]
    if not uid:
        return 0
    req = urllib.request.Request(CURSOR_USAGE_URL, headers={
        "Cookie": f"WorkosCursorSessionToken={uid}%3A%3A{token}",
        "Accept": "application/json", "User-Agent": UA})
    j = json.loads(urllib.request.urlopen(req, timeout=20).read())
    plan = (j.get("individualUsage") or {}).get("plan") or {}
    now = int(time.time())
    try:
        resets = int(datetime.fromisoformat(
            (j.get("billingCycleEnd") or "").replace("Z", "+00:00")).timestamp())
    except Exception:
        resets = None
    n = 0
    tot = plan.get("totalPercentUsed")
    if tot is not None:
        c.execute("INSERT OR REPLACE INTO app_quota "
                  "(app,label,ts,pct_remaining,used,lim,resets_at,meta) "
                  "VALUES('cursor','plan',?,?,?,?,?,?)",
                  (now, 100.0 - float(tot), plan.get("used"), plan.get("limit"), resets,
                   json.dumps({"membership": j.get("membershipType"),
                               "auto_pct": plan.get("autoPercentUsed"),
                               "api_pct": plan.get("apiPercentUsed")})))
        n += 1
    for label, key in (("auto", "autoPercentUsed"), ("api", "apiPercentUsed")):
        p = plan.get(key)
        if p is not None:
            c.execute("INSERT OR REPLACE INTO app_quota "
                      "(app,label,ts,pct_remaining,used,lim,resets_at,meta) "
                      "VALUES('cursor',?,?,?,?,?,?,?)",
                      (label, now, 100.0 - float(p), None, None, resets, None))
            n += 1
    if j.get("membershipType"):
        c.execute("INSERT OR REPLACE INTO kv VALUES('cursor.plan',?)",
                  (j["membershipType"],))
    return n


def collect_cursor(c):
    if not _cursor_state_db().exists():
        log_run(c, "cursor", "skip", "state.vscdb 不存在")
        return 0
    n_sess = _collect_cursor_local(c)
    token = cursor_access_token(c)
    n_ev = 0
    if token:
        try:
            n_ev = _collect_cursor_csv(c, token)
        except Exception as e:
            log_run(c, "cursor-csv", "error", str(e))
        try:
            n_q = _cursor_usage_summary(c, token)
            log_run(c, "cursor-quota", "ok", f"{n_q} 行")
        except Exception as e:
            log_run(c, "cursor-quota", "error", str(e))
    else:
        log_run(c, "cursor-csv", "skip", "无 access token")
    log_run(c, "cursor", "ok", f"{n_sess} composers, +{n_ev} events")
    return n_sess


# ---------------------------------------------------------------- antigravity

def _pb_fields(data: bytes):
    """极简 protobuf 解码：产出 (field_no, wire_type, value)。value: varint=int, len-delimited=bytes。"""
    out = []
    i, n = 0, len(data)
    while i < n:
        # tag varint
        tag = 0
        shift = 0
        while i < n:
            b = data[i]; i += 1
            tag |= (b & 0x7f) << shift
            shift += 7
            if not b & 0x80:
                break
        fn, wt = tag >> 3, tag & 7
        if fn == 0:
            break
        if wt == 0:
            v = 0
            shift = 0
            while i < n:
                b = data[i]; i += 1
                v |= (b & 0x7f) << shift
                shift += 7
                if not b & 0x80:
                    break
            out.append((fn, wt, v))
        elif wt == 2:
            ln = 0
            shift = 0
            while i < n:
                b = data[i]; i += 1
                ln |= (b & 0x7f) << shift
                shift += 7
                if not b & 0x80:
                    break
            out.append((fn, wt, data[i:i + ln]))
            i += ln
        elif wt == 1:
            out.append((fn, wt, data[i:i + 8])); i += 8
        elif wt == 5:
            out.append((fn, wt, data[i:i + 4])); i += 4
        else:
            break
    return out


def _pb_get(fields, no, wt=None):
    for f in fields:
        if f[0] == no and (wt is None or f[1] == wt):
            return f[2]
    return None


def _pb_ts(msg: bytes) -> "int | None":
    """google.protobuf.Timestamp → unix 秒。"""
    v = _pb_get(_pb_fields(msg), 1, 0)
    return int(v) if v and v > 0 else None


def _agy_gen_event(blob: bytes):
    """gen_metadata.data → dict|None。结构(逆向自 openusage#1139):
    field1(wrap){ 19:modelID 21:label 4:usage{1:sys 2:in 3:out 5:cacheRead} 9:timing{4:Timestamp} }"""
    wrap = _pb_get(_pb_fields(blob), 1, 2)
    if not wrap:
        return None
    w = _pb_fields(wrap)
    model = _pb_get(w, 19, 2)
    label = _pb_get(w, 21, 2)
    usage = _pb_get(w, 4, 2)
    if usage is None:
        return None
    u = _pb_fields(usage)
    sys_tok = int(_pb_get(u, 1, 0) or 0)
    tin = int(_pb_get(u, 2, 0) or 0)
    tout = int(_pb_get(u, 3, 0) or 0)
    tcr = int(_pb_get(u, 5, 0) or 0)
    if not (model or label or tin or tout or tcr or sys_tok):
        return None
    timing = _pb_get(w, 9, 2)
    ts = _pb_ts(_pb_get(_pb_fields(timing), 4, 2) or b"") if timing else None
    def _s(b):
        try:
            return b.decode("utf-8").strip() or None if b else None
        except Exception:
            return None
    return {"model": _s(model), "label": _s(label),
            # Gemini 口径：promptTokenCount 含 cachedContent，扣掉避免重复计
            "tin": max(0, sys_tok + tin - tcr), "tout": tout, "tcr": tcr,
            "ts": ts}


def collect_antigravity(c):
    """~/.gemini/antigravity*/conversations/*.db（只读）→ usage_events + local_sessions。"""
    try:
        collect_antigravity_quota(c)
    except Exception as e:
        log_run(c, "antigravity-quota", "error", str(e))
    roots = sorted(Path.home().glob(".gemini/antigravity*/conversations"))
    dbs = [p for r in roots for p in r.glob("*.db")]
    if not dbs:
        log_run(c, "antigravity", "skip", "无 conversations/*.db")
        return 0
    now = int(time.time())
    n_ev = n_sess = 0
    for p in dbs:
        try:
            src = _ro(p)
            meta = src.execute(
                "SELECT trajectory_id, cascade_id FROM trajectory_meta LIMIT 1").fetchone()
            tid = (meta[0] or meta[1]) if meta else p.stem
            steps_ts = {}
            for idx, md in src.execute("SELECT idx, metadata FROM steps"):
                if md:
                    t = _pb_ts(_pb_get(_pb_fields(md), 1, 2) or b"")
                    if t:
                        steps_ts[idx] = t
            evs = []
            for idx, blob in src.execute("SELECT idx, data FROM gen_metadata"):
                ev = _agy_gen_event(blob or b"")
                if not ev:
                    continue
                ts = ev["ts"] or steps_ts.get(idx)
                if not ts:
                    continue                    # 无时间戳的事件不入账
                evs.append((idx, ts, ev))
            src.close()
            models = set()
            t_in = t_out = t_cr = 0
            ts_list = []
            for idx, ts, ev in evs:
                key = f"{tid}:{idx}"
                day = datetime.fromtimestamp(ts).strftime("%Y-%m-%d")
                cur = c.execute("""INSERT OR IGNORE INTO usage_events
                    (app,event_key,ts,day,model,kind,session_id,
                     tok_in,tok_out,tok_cache_read,tok_cache_write,cost_usd,meta)
                    VALUES('antigravity',?,?,?,?,'gen',?,?,?,?,0,NULL,?)""",
                    (key, ts, day, ev["model"] or ev["label"] or "?",
                     f"agy:{tid}", ev["tin"], ev["tout"], ev["tcr"],
                     json.dumps({"label": ev["label"], "model_id": ev["model"]})))
                n_ev += cur.rowcount
                models.add(ev["label"] or ev["model"] or "?")
                t_in += ev["tin"]; t_out += ev["tout"]; t_cr += ev["tcr"]
                ts_list.append(ts)
            if ts_list:
                c.execute("""INSERT INTO local_sessions
                    (session_id,source,model,title,created_at,last_activity_at,
                     n_user,n_assistant,n_tool,n_system,n_tool_calls,n_prompts,
                     tok_in,tok_out,tok_cache_read,tok_cache_write,first_seen,last_seen)
                    VALUES (?,?,?,?,?,?,0,?,0,0,0,0,?,?,?,0,?,?)
                    ON CONFLICT(session_id) DO UPDATE SET
                     model=excluded.model, last_activity_at=excluded.last_activity_at,
                     n_assistant=excluded.n_assistant, tok_in=excluded.tok_in,
                     tok_out=excluded.tok_out, tok_cache_read=excluded.tok_cache_read,
                     last_seen=excluded.last_seen""",
                    (f"agy:{tid}", "antigravity", ",".join(sorted(models))[:200],
                     p.stem, min(ts_list), max(ts_list), len(ts_list),
                     t_in, t_out, t_cr, now, now))
                n_sess += 1
        except Exception:
            continue
    log_run(c, "antigravity", "ok", f"{n_sess} conversations, +{n_ev} events")
    return n_sess


# ---- antigravity 配额：本机 language server（IDE 运行时才可用）
# 机制同官方 quota-watcher 扩展：进程命令行拿 --csrf_token，
# netstat/lsof 找监听端口，POST Connect-RPC 拿 quotaInfo{remainingFraction,resetTime}

def _antigravity_ls_procs():
    """运行中的 antigravity language server → [(pid, csrf_token)]。"""
    procs = []
    try:
        if IS_WIN:
            ps = ("Get-CimInstance Win32_Process | "
                  "Where-Object {$_.CommandLine -match 'app_data_dir'} | "
                  "Select-Object ProcessId,CommandLine | ConvertTo-Json -Compress")
            out = subprocess.run(
                ["powershell", "-NoProfile", "-Command", ps],
                capture_output=True, text=True, timeout=25, errors="replace",
                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0)).stdout
            items = json.loads(out) if out.strip() else []
            if isinstance(items, dict):
                items = [items]
            for it in items:
                cl = it.get("CommandLine") or ""
                if re.search(r"--app_data_dir[=\s]+antigravity\b", cl, re.I):
                    m = re.search(r"--csrf_token[=\s]+([\w-]+)", cl)
                    procs.append((int(it["ProcessId"]),
                                  m.group(1) if m else None))
        else:
            out = subprocess.run(["ps", "-eo", "pid,args"],
                                 capture_output=True, text=True,
                                 timeout=15).stdout
            for line in out.splitlines():
                if re.search(r"--app_data_dir[=\s]+antigravity\b", line):
                    try:
                        pid = int(line.strip().split(None, 1)[0])
                    except ValueError:
                        continue
                    m = re.search(r"--csrf_token[=\s]+([\w-]+)", line)
                    procs.append((pid, m.group(1) if m else None))
    except Exception:
        pass
    return procs


def _antigravity_ports(pid):
    """进程监听的 TCP 端口列表。"""
    try:
        if IS_WIN:
            out = subprocess.run(["netstat", "-ano", "-p", "tcp"],
                                 capture_output=True, text=True,
                                 errors="replace", timeout=15).stdout
            ports = []
            for line in out.splitlines():
                parts = line.split()
                if (len(parts) >= 5 and parts[-1] == str(pid)
                        and "LISTENING" in parts[-2].upper()):
                    m = re.search(r":(\d+)$", parts[1])
                    if m:
                        ports.append(int(m.group(1)))
            return ports
        out = subprocess.run(
            ["lsof", "-nP", "-iTCP", "-sTCP:LISTEN", "-a", "-p", str(pid)],
            capture_output=True, text=True, timeout=15).stdout
        return [int(m.group(1))
                for m in re.finditer(r":(\d+)\s+\(LISTEN\)", out)]
    except Exception:
        return []


def _ag_ls_post(port, path, csrf, use_https=True):
    body = json.dumps({"metadata": {"ideName": "antigravity",
                                    "extensionName": "antigravity",
                                    "ideVersion": "1.0", "locale": "en"}}).encode()
    scheme = "https" if use_https else "http"
    req = urllib.request.Request(
        f"{scheme}://127.0.0.1:{port}{path}", data=body, method="POST",
        headers={"Content-Type": "application/json",
                 "Connect-Protocol-Version": "1",
                 "X-Codeium-Csrf-Token": csrf, "User-Agent": UA})
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    return json.loads(urllib.request.urlopen(
        req, timeout=6, context=ctx if use_https else None).read())


def _ag_ls_call(pid, csrf, method):
    """对本机 language server 发一个 RPC，https 失败退回 http。返回 dict|None。"""
    for port in _antigravity_ports(pid):
        for https in (True, False):
            try:
                return _ag_ls_post(port, method, csrf, use_https=https)
            except Exception:
                continue
    return None


def collect_antigravity_quota(c) -> int:
    """RetrieveUserQuotaSummary → 每池 周限额+5小时限额 写 app_quota（与 IDE 官方页同口径）。
    兜底用 GetUserStatus 的 per-model quotaInfo（只有 5h 窗口）。IDE 不在跑记 skip。"""
    n = 0
    now = int(time.time())
    for pid, csrf in _antigravity_ls_procs():
        if not csrf:
            continue
        resp = _ag_ls_call(
            pid, csrf,
            "/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary")
        groups = ((resp or {}).get("response") or {}).get("groups") or []
        if groups:
            for g in groups:
                gname = g.get("displayName") or ""
                pool = "Gemini" if "Gemini" in gname else "Claude · GPT"
                for b in g.get("buckets") or []:
                    frac = _f(b.get("remainingFraction"))
                    if frac is None:
                        continue
                    rt = b.get("resetTime")
                    try:
                        resets = int(datetime.fromisoformat(
                            rt.replace("Z", "+00:00")).timestamp()) if rt else None
                    except Exception:
                        resets = None
                    win = "周" if b.get("window") == "weekly" else "5h"
                    c.execute("INSERT OR REPLACE INTO app_quota "
                              "(app,label,ts,pct_remaining,used,lim,resets_at,meta) "
                              "VALUES('antigravity',?,?,?,?,?,?,?)",
                              (f"{pool} · {win}", now, frac * 100,
                               None, None, resets, "{}"))
                    n += 1
            break                                # 第一个可用进程就够
        # 兜底：GetUserStatus 的 per-model quotaInfo（5h 窗口）
        resp = _ag_ls_call(
            pid, csrf,
            "/exa.language_server_pb.LanguageServerService/GetUserStatus")
        if not resp:
            continue
        us = resp.get("userStatus") or {}
        configs = ((us.get("cascadeModelConfigData") or {})
                   .get("clientModelConfigs")
                   or resp.get("clientModelConfigs") or [])
        pools = {}
        for cfg in configs:
            qi = cfg.get("quotaInfo")
            if not qi:
                continue
            frac = _f(qi.get("remainingFraction"))
            if frac is None:
                continue
            rt = qi.get("resetTime")
            try:
                resets = int(datetime.fromisoformat(
                    rt.replace("Z", "+00:00")).timestamp()) if rt else None
            except Exception:
                resets = None
            label = (cfg.get("label")
                     or (cfg.get("modelOrAlias") or {}).get("model") or "?")
            pool = "Gemini" if label.startswith("Gemini") else "Claude · GPT"
            cur = pools.get(pool)
            if cur is None or frac < cur[0]:
                pools[pool] = (frac, resets)
        for pool, (frac, resets) in pools.items():
            c.execute("INSERT OR REPLACE INTO app_quota "
                      "VALUES('antigravity',?,?,?,?,?,?,?)",
                      (f"{pool} · 5h", now, frac * 100, None, None, resets, "{}"))
            n += 1
        break                                    # 第一个可用进程就够
    log_run(c, "antigravity-quota",
            "ok" if n else "skip",
            f"{n} 行" if n else "IDE 未运行/端口不可用")
    return n


# ---------------------------------------------------------------- zcode / grok

def _read_jsonl_incremental(c, app: str, path: Path):
    """JSONL 只增不改：按 kv 里的字节偏移续读新行。产出解析后的 dict。"""
    key = f"off:{app}:{path}"
    off = 0
    try:
        row = c.execute("SELECT value FROM kv WHERE key=?", (key,)).fetchone()
        off = int(row[0]) if row else 0
    except Exception:
        pass
    size = path.stat().st_size
    if off > size:                               # 文件被截断/轮转 → 重读
        off = 0
    if off == size:
        return
    with open(path, "rb") as f:
        f.seek(off)
        chunk = f.read()
    c.execute("INSERT OR REPLACE INTO kv VALUES(?,?)", (key, str(size)))
    for raw in chunk.decode("utf-8", "replace").splitlines():
        if not raw.strip():
            continue
        try:
            yield json.loads(raw)
        except Exception:
            continue


def _iso_ts(s) -> "int | None":
    try:
        return int(datetime.fromisoformat(
            str(s).replace("Z", "+00:00")).timestamp())
    except Exception:
        return None


def collect_zcode(c) -> int:
    """~/.zcode/cli/agents/*/*/transcript.jsonl → usage_events + local_sessions。
    model_request 带模型名(uuid/name)，model_complete 带 usage，按 turnId 关联。"""
    files = sorted(Path.home().glob(".zcode/cli/agents/*/*/transcript.jsonl"))
    if not files:
        log_run(c, "zcode", "skip", "无 transcript.jsonl")
        return 0
    n_ev = 0
    sess = {}                                    # sessionId → 聚合
    for p in files:
        try:
            turn_model = {}                      # turnId → model（本文件内）
            for e in _read_jsonl_incremental(c, "zcode", p):
                t, pay = e.get("type"), e.get("payload") or {}
                sid = e.get("sessionId") or p.parent.stem
                ts = _iso_ts(e.get("timestamp"))
                if t == "model_request":
                    m = pay.get("model") or ""
                    turn_model[e.get("turnId") or ""] = m.split("/")[-1] or m
                elif t == "model_complete":
                    u = pay.get("usage") or {}
                    if not u.get("totalTokens"):
                        continue
                    model = turn_model.get(e.get("turnId") or "", "?")
                    day = (datetime.fromtimestamp(ts).strftime("%Y-%m-%d")
                           if ts else "1970-01-01")
                    cur = c.execute("""INSERT OR IGNORE INTO usage_events
                        (app,event_key,ts,day,model,kind,session_id,
                         tok_in,tok_out,tok_cache_read,tok_cache_write,cost_usd,meta)
                        VALUES('zcode',?,?,?,?,'gen',?,?,?,?,?,NULL,?)""",
                        (e.get("id") or str(uuid.uuid4()), ts or 0, day, model,
                         f"zcode:{sid}",
                         # inputTokens 含 cacheRead/cacheWrite，扣掉避免重复计
                         max(0, (u.get("inputTokens") or 0)
                             - (u.get("cacheReadTokens") or 0)
                             - (u.get("cacheWriteTokens") or 0)),
                         u.get("outputTokens") or 0,
                         u.get("cacheReadTokens") or 0,
                         u.get("cacheWriteTokens") or 0,
                         json.dumps({"querySource": pay.get("querySource")})))
                    n_ev += cur.rowcount
                    s = sess.setdefault(sid, {"model": model, "n": 0,
                                              "t0": ts or 0, "t1": ts or 0,
                                              "ti": 0, "to": 0, "tcr": 0})
                    s["n"] += 1
                    s["model"] = model if model != "?" else s["model"]
                    s["t0"] = min(s["t0"], ts or s["t0"])
                    s["t1"] = max(s["t1"], ts or s["t1"])
                    s["ti"] += u.get("inputTokens") or 0
                    s["to"] += u.get("outputTokens") or 0
                    s["tcr"] += u.get("cacheReadTokens") or 0
        except Exception:
            continue
    now = int(time.time())
    n_sess = 0
    for sid, s in sess.items():
        c.execute("""INSERT INTO local_sessions
            (session_id,source,model,title,created_at,last_activity_at,
             n_user,n_assistant,n_tool,n_system,n_tool_calls,n_prompts,
             tok_in,tok_out,tok_cache_read,tok_cache_write,first_seen,last_seen)
            VALUES (?, 'zcode', ?, ?, ?, ?, 0, ?, 0,0,0,0, ?,?,?,0, ?,?)
            ON CONFLICT(session_id) DO UPDATE SET
             model=excluded.model, last_activity_at=excluded.last_activity_at,
             n_assistant=excluded.n_assistant, tok_in=excluded.tok_in,
             tok_out=excluded.tok_out, tok_cache_read=excluded.tok_cache_read,
             last_seen=excluded.last_seen""",
            (f"zcode:{sid}", s["model"], sid, s["t0"], s["t1"], s["n"],
             s["ti"], s["to"], s["tcr"], now, now))
        n_sess += 1
    log_run(c, "zcode", "ok", f"{n_sess} sessions, +{n_ev} events")
    return n_sess


GROK_PROXY = "https://cli-chat-proxy.grok.com/v1"


def _grok_access_token():
    """~/.grok/auth.json → 未过期 access token。
    刷新交给 grok CLI 自己做（refresh_token 会轮换，碰了可能顶掉 CLI 会话）。"""
    try:
        d = json.loads((Path.home() / ".grok/auth.json").read_text(encoding="utf-8"))
    except Exception:
        return None
    now = int(time.time())
    for k in d.values():
        tok = k.get("key")
        exp = _iso_ts(k.get("expires_at"))
        if tok and (exp is None or exp > now + 60):
            return tok
    return None


def _grok_headers(tok):
    try:
        ver = (json.loads((Path.home() / ".grok/version.json")
                          .read_text(encoding="utf-8")).get("version") or "1.0.0")
    except Exception:
        ver = "1.0.0"
    return {"Authorization": f"Bearer {tok}", "User-Agent": f"xai-grok-cli/{ver}",
            "x-grok-client-version": ver, "x-grok-client-identifier": "xai-grok-cli",
            "Accept": "application/json"}


def collect_grok_quota(c) -> int:
    """Grok 周额度：GET cli-chat-proxy.grok.com/v1/billing?format=credits。
    与 CLI 自带 billing 面板（WEEKLY/MONTHLY）同源：
    config.currentPeriod{type:WEEKLY, start, end} + creditUsagePercent(已用%)
    + productUsage 分产品(GrokChat/GrokBuild)。resets_at = currentPeriod.end。"""
    tok = _grok_access_token()
    if not tok:
        log_run(c, "grok-quota", "skip", "无有效 access token")
        return 0
    req = urllib.request.Request(f"{GROK_PROXY}/billing?format=credits",
                                 headers=_grok_headers(tok))
    cfg = (json.loads(urllib.request.urlopen(req, timeout=15).read())
           or {}).get("config") or {}
    period = cfg.get("currentPeriod") or {}
    resets = _iso_ts(period.get("end") or cfg.get("billingPeriodEnd"))
    try:
        pay = json.loads((Path.home() / ".grok/settings_cache.json")
                         .read_text(encoding="utf-8")).get("payload")
        pay = json.loads(pay) if isinstance(pay, str) else (pay or {})
        tier = (pay.get("settings") or {}).get("subscription_tier_display")
    except Exception:
        tier = None
    meta = json.dumps({"period": period.get("type"), "tier": tier})
    if tier:
        c.execute("INSERT OR REPLACE INTO kv VALUES('grok.plan',?)", (tier,))
    now = int(time.time())
    n = 0
    used = cfg.get("creditUsagePercent")
    if used is not None:
        c.execute("INSERT OR REPLACE INTO app_quota "
                  "(app,label,ts,pct_remaining,used,lim,resets_at,meta) "
                  "VALUES('grok','周额度',?,?,?,?,?,?)",
                  (now, max(0.0, 100.0 - float(used)), float(used), 100.0,
                   resets, meta))
        n += 1
    for pu in cfg.get("productUsage") or []:
        u = pu.get("usagePercent")
        if u is None:
            continue
        c.execute("INSERT OR REPLACE INTO app_quota "
                  "(app,label,ts,pct_remaining,used,lim,resets_at,meta) "
                  "VALUES('grok',?,?,?,?,?,?,?)",
                  (pu.get("product") or "?", now, max(0.0, 100.0 - float(u)),
                   float(u), 100.0, resets, None))
        n += 1
    return n


def collect_grok(c) -> int:
    """~/.grok/sessions/*/*/{updates.jsonl,summary.json} → usage_events + local_sessions。
    turn_completed.usage 含全量 token + costUsdTicks(1e-9 USD) + modelUsage 分模型。"""
    try:
        n_q = collect_grok_quota(c)
        log_run(c, "grok-quota", "ok", f"{n_q} 行")
    except Exception as e:
        log_run(c, "grok-quota", "error", str(e))
    roots = Path.home() / ".grok/sessions"
    if not roots.exists():
        log_run(c, "grok", "skip", "~/.grok/sessions 不存在")
        return 0
    n_ev = n_sess = 0
    for upd in roots.glob("*/*/updates.jsonl"):
        try:
            for e in _read_jsonl_incremental(c, "grok", upd):
                u = (e.get("params") or {}).get("update") or {}
                if u.get("sessionUpdate") != "turn_completed":
                    continue
                usage = u.get("usage") or {}
                if not usage.get("totalTokens"):
                    continue
                sid = (e.get("params") or {}).get("sessionId") or upd.parent.name
                ts = e.get("timestamp") or 0
                day = datetime.fromtimestamp(ts).strftime("%Y-%m-%d")
                pid = u.get("prompt_id") or str(uuid.uuid4())
                mu = usage.get("modelUsage") or {}
                items = mu.items() if mu else [("?", usage)]
                for model, mu1 in items:
                    cost = mu1.get("costUsdTicks")
                    cur = c.execute("""INSERT OR IGNORE INTO usage_events
                        (app,event_key,ts,day,model,kind,session_id,
                         tok_in,tok_out,tok_cache_read,tok_cache_write,cost_usd,meta)
                        VALUES('grok',?,?,?,?,'gen',?,?,?,?,?,?,?)""",
                        (f"{sid}:{pid}:{model}", ts, day, model,
                         f"grok:{sid}",
                         # inputTokens 含 cachedRead/cacheCreation，扣掉避免重复计
                         max(0, (mu1.get("inputTokens") or 0)
                             - (mu1.get("cachedReadTokens") or 0)
                             - (mu1.get("cacheCreationTokens") or 0)),
                         mu1.get("outputTokens") or 0,
                         mu1.get("cachedReadTokens") or 0,
                         mu1.get("cacheCreationTokens") or 0,
                         (cost / 1e9) if isinstance(cost, (int, float)) else None,
                         json.dumps({"modelCalls": mu1.get("modelCalls"),
                                     "apiMs": mu1.get("apiDurationMs"),
                                     "reasoning": mu1.get("reasoningTokens")})))
                    n_ev += cur.rowcount
        except Exception:
            continue
    # 会话级：summary.json
    now = int(time.time())
    for sm in roots.glob("*/*/summary.json"):
        try:
            j = json.loads(sm.read_text(encoding="utf-8"))
            sid = (j.get("info") or {}).get("id") or sm.parent.name
            t0 = _iso_ts(j.get("created_at"))
            t1 = _iso_ts(j.get("last_active_at") or j.get("updated_at"))
            c.execute("""INSERT INTO local_sessions
                (session_id,source,model,title,cwd,created_at,last_activity_at,
                 n_user,n_assistant,n_tool,n_system,n_tool_calls,n_prompts,
                 tok_in,tok_out,tok_cache_read,tok_cache_write,first_seen,last_seen)
                VALUES (?, 'grok', ?, ?, ?, ?, ?, ?, 0,0,0,0,0, 0,0,0,0, ?,?)
                ON CONFLICT(session_id) DO UPDATE SET
                 model=excluded.model, title=excluded.title,
                 last_activity_at=excluded.last_activity_at,
                 n_user=excluded.n_user, last_seen=excluded.last_seen""",
                (f"grok:{sid}", j.get("current_model_id") or "?",
                 j.get("session_summary") or sid,
                 (j.get("info") or {}).get("cwd") or "",
                 t0 or 0, t1 or 0, j.get("num_messages") or 0, now, now))
            n_sess += 1
        except Exception:
            continue
    log_run(c, "grok", "ok", f"{n_sess} sessions, +{n_ev} events")
    return n_sess


def collect_claude(c) -> int:
    """~/.claude/projects/**/*.jsonl → usage_events + local_sessions。
    assistant 消息 message.usage 为 Anthropic 真实计费口径；
    input_tokens 不含缓存（无需扣除）。流式重复写 → 按 message.id 去重。"""
    root = Path.home() / ".claude/projects"
    if not root.exists():
        log_run(c, "claude", "skip", "~/.claude/projects 不存在")
        return 0
    n_ev = 0
    sess = {}                                    # sessionId → 聚合
    seen = set()                                 # 本批内 message.id 去重
    for fp in root.glob("**/*.jsonl"):
        try:
            for e in _read_jsonl_incremental(c, "claude", fp):
                msg = e.get("message") or {}
                u = msg.get("usage") or {}
                if not u.get("output_tokens"):
                    continue
                mid = msg.get("id") or e.get("uuid")
                if mid in seen:
                    continue
                seen.add(mid)
                sid = e.get("sessionId") or fp.stem
                ts = _iso_ts(e.get("timestamp")) or 0
                day = (datetime.fromtimestamp(ts).strftime("%Y-%m-%d")
                       if ts else "1970-01-01")
                model = msg.get("model") or "?"
                i_ = u.get("input_tokens") or 0
                o_ = u.get("output_tokens") or 0
                cr = u.get("cache_read_input_tokens") or 0
                cw = u.get("cache_creation_input_tokens") or 0
                st = u.get("server_tool_use") or {}
                cur = c.execute("""INSERT OR IGNORE INTO usage_events
                    (app,event_key,ts,day,model,kind,session_id,
                     tok_in,tok_out,tok_cache_read,tok_cache_write,cost_usd,meta)
                    VALUES('claude',?,?,?,?,'gen',?,?,?,?,?,NULL,?)""",
                    (mid, ts, day, model, f"claude:{sid}", i_, o_, cr, cw,
                     json.dumps({"ws": st.get("web_search_requests"),
                                 "wf": st.get("web_fetch_requests"),
                                 "tier": u.get("service_tier")})))
                n_ev += cur.rowcount
                s = sess.setdefault(sid, {"model": model, "n": 0,
                                          "t0": ts, "t1": ts,
                                          "cwd": e.get("cwd") or "",
                                          "ti": 0, "to": 0, "tcr": 0, "tcw": 0})
                s["n"] += 1
                s["model"] = model if model != "?" else s["model"]
                s["t0"] = min(s["t0"], ts); s["t1"] = max(s["t1"], ts)
                s["ti"] += i_; s["to"] += o_; s["tcr"] += cr; s["tcw"] += cw
        except Exception:
            continue
    now = int(time.time())
    n_sess = 0
    for sid, s in sess.items():
        title = Path(s["cwd"]).name if s["cwd"] else sid
        c.execute("""INSERT INTO local_sessions
            (session_id,source,model,title,cwd,created_at,last_activity_at,
             n_user,n_assistant,n_tool,n_system,n_tool_calls,n_prompts,
             tok_in,tok_out,tok_cache_read,tok_cache_write,first_seen,last_seen)
            VALUES (?, 'claude', ?, ?, ?, ?, ?, 0, ?, 0,0,0,0, ?,?,?,?, ?,?)
            ON CONFLICT(session_id) DO UPDATE SET
             model=excluded.model, last_activity_at=excluded.last_activity_at,
             n_assistant=excluded.n_assistant, tok_in=excluded.tok_in,
             tok_out=excluded.tok_out, tok_cache_read=excluded.tok_cache_read,
             tok_cache_write=excluded.tok_cache_write, last_seen=excluded.last_seen""",
            (f"claude:{sid}", s["model"], title, s["cwd"],
             s["t0"], s["t1"], s["n"],
             s["ti"], s["to"], s["tcr"], s["tcw"], now, now))
        n_sess += 1
    log_run(c, "claude", "ok", f"{n_sess} sessions, +{n_ev} events")
    return n_sess


def collect_codex(c) -> int:
    """~/.codex/sessions/**/rollout-*.jsonl + archived_sessions/ → usage_events。
    Codex CLI 与桌面 App 写同一目录。token_usage_record 按响应计；
    input_tokens 含 cached+cache_write（OpenAI 惯例）需扣除；
    turn_context 提供 turn→model；event_msg/token_count 带 rate_limits
    （ChatGPT 直连才有，走代理为 null）。"""
    root = Path.home() / ".codex"
    files = sorted((root / "sessions").glob("**/*.jsonl")) + \
        sorted((root / "archived_sessions").glob("*.jsonl"))
    if not files:
        log_run(c, "codex", "skip", "~/.codex/sessions 不存在")
        return 0
    n_ev = 0
    sess = {}
    turn_model = {}                                # turn_id → model
    for fp in files:
        try:
            for e in _read_jsonl_incremental(c, "codex", fp):
                ts = _iso_ts(e.get("timestamp")) or 0
                pay = e.get("payload") or {}
                t = e.get("type")
                if t == "session_meta":
                    sid = pay.get("session_id") or pay.get("id") or fp.stem
                    s = sess.setdefault(sid, {"model": "?", "n": 0, "t0": ts,
                                              "t1": ts, "cwd": "",
                                              "provider": "codex",
                                              "ti": 0, "to": 0, "tcr": 0, "tcw": 0})
                    s["cwd"] = pay.get("cwd") or s["cwd"]
                    s["provider"] = pay.get("model_provider") or s["provider"]
                    s["t0"] = min(s["t0"], ts)
                elif t == "turn_context":
                    if pay.get("turn_id") and pay.get("model"):
                        turn_model[pay["turn_id"]] = pay["model"]
                elif t == "token_usage_record":
                    rid = pay.get("response_id")
                    if not rid:
                        continue
                    sid = pay.get("session_id") or pay.get("thread_id") or fp.stem
                    tid = pay.get("turn_id") or ""
                    model = turn_model.get(tid, "?")
                    u = pay.get("usage") or {}
                    inp = u.get("input_tokens") or 0
                    cr = u.get("cached_input_tokens") or 0
                    cw = u.get("cache_write_input_tokens") or 0
                    i_ = max(0, inp - cr - cw)
                    o_ = u.get("output_tokens") or 0
                    provider = sess.get(sid, {}).get("provider", "codex")
                    day = (datetime.fromtimestamp(ts).strftime("%Y-%m-%d")
                           if ts else "1970-01-01")
                    cur = c.execute("""INSERT OR IGNORE INTO usage_events
                        (app,event_key,ts,day,model,kind,session_id,
                         tok_in,tok_out,tok_cache_read,tok_cache_write,cost_usd,meta)
                        VALUES('codex',?,?,?,?,?,?,?,?,?,?,NULL,?)""",
                        (f"codex:{rid}", ts, day, model, provider,
                         f"codex:{sid}", i_, o_, cr, cw,
                         json.dumps({"reasoning": u.get("reasoning_output_tokens"),
                                     "turn": tid, "provider": provider})))
                    n_ev += cur.rowcount
                    s = sess.setdefault(sid, {"model": "?", "n": 0, "t0": ts,
                                              "t1": ts, "cwd": "",
                                              "provider": "codex",
                                              "ti": 0, "to": 0, "tcr": 0, "tcw": 0})
                    s["n"] += 1
                    s["model"] = model if model != "?" else s["model"]
                    s["t0"] = min(s["t0"], ts); s["t1"] = max(s["t1"], ts)
                    s["ti"] += i_; s["to"] += o_; s["tcr"] += cr; s["tcw"] += cw
                elif t == "event_msg" and pay.get("type") == "token_count":
                    rl = pay.get("rate_limits") or {}
                    for lk in ("primary", "secondary"):
                        w = rl.get(lk) or {}
                        pct = w.get("used_percent")
                        reset = w.get("resets_at") or w.get("reset_at")
                        if pct is None or reset is None:
                            continue
                        c.execute(
                            "INSERT OR REPLACE INTO app_quota VALUES('codex',?,?,?,?,?,?,?,?)",
                            (lk, ts, max(0.0, 100.0 - pct), pct, 100.0, reset,
                             json.dumps({"window_minutes": w.get("window_minutes"),
                                         "limit_id": rl.get("limit_id")}),
                             device()))
        except Exception:
            continue
    now = int(time.time())
    n_sess = 0
    for sid, s in sess.items():
        title = Path(s["cwd"]).name if s["cwd"] else sid
        c.execute("""INSERT INTO local_sessions
            (session_id,source,model,title,cwd,created_at,last_activity_at,
             n_user,n_assistant,n_tool,n_system,n_tool_calls,n_prompts,
             tok_in,tok_out,tok_cache_read,tok_cache_write,first_seen,last_seen)
            VALUES (?, 'codex', ?, ?, ?, ?, ?, 0, ?, 0,0,0,0, ?,?,?,?, ?,?)
            ON CONFLICT(session_id) DO UPDATE SET
             model=excluded.model, last_activity_at=excluded.last_activity_at,
             n_assistant=excluded.n_assistant, tok_in=excluded.tok_in,
             tok_out=excluded.tok_out, tok_cache_read=excluded.tok_cache_read,
             tok_cache_write=excluded.tok_cache_write, last_seen=excluded.last_seen""",
            (f"codex:{sid}", s["model"], title, s["cwd"],
             s["t0"], s["t1"], s["n"],
             s["ti"], s["to"], s["tcr"], s["tcw"], now, now))
        n_sess += 1
    log_run(c, "codex", "ok", f"{n_sess} sessions, +{n_ev} events")
    return n_sess


# 等效价格表（每 1M token 美元，prefix 首个命中者胜；"" 为兜底）
# 公开 API 刊例价；swe-2 无公开价，默认按 Sonnet 档折算。
# 可在 data/prices.json 覆盖/新增：{"rules":[["prefix",in,out,cr,cw],...]}
BUILTIN_PRICES = [
    ("swe-2",       3.00, 15.00, 0.30,  3.75,  "proxy:sonnet"),
    ("claude-opus", 15.00, 75.00, 1.50, 18.75, ""),
    ("claude-haiku", 0.80,  4.00, 0.08,  1.00, ""),
    ("claude",       3.00, 15.00, 0.30,  3.75, ""),
    ("gpt-6-astra",  1.25, 10.00, 0.125, 1.25, "proxy:gpt-frontier"),
    ("gpt",          1.25, 10.00, 0.125, 1.25, ""),
    ("gemini",       1.25, 10.00, 0.31,  1.25, ""),
    ("deepseek",     0.27,  1.10, 0.07,  0.27, ""),
    ("kimi",         0.60,  2.50, 0.15,  0.60, ""),
    ("grok",         3.00, 15.00, 0.30,  3.75, "proxy:frontier"),
    ("glm",          0.60,  2.20, 0.11,  0.55, "z.ai"),
    ("fable",        3.00, 15.00, 0.30,  3.75, "proxy:sonnet"),
    ("",             3.00, 15.00, 0.30,  3.75, "default:sonnet"),
]

PRICES_JSON = DATA_DIR / "prices.json"


def load_price_rules():
    """prices.json 覆盖规则在前，内置兜底在后。"""
    rules = []
    try:
        for r in json.loads(PRICES_JSON.read_text(encoding="utf-8")).get("rules", []):
            rules.append((str(r[0]), float(r[1]), float(r[2]), float(r[3]),
                          float(r[4]), "prices.json"))
    except Exception:
        pass
    rules.extend(BUILTIN_PRICES)
    return rules


def sync_prices(c):
    """把价格规则写进库，托盘和 report 共用同一份。"""
    c.execute("DELETE FROM model_prices")
    c.executemany(
        "INSERT INTO model_prices VALUES (?,?,?,?,?,?,?)",
        [(i, *r) for i, r in enumerate(load_price_rules())])


def price_of(model, rules):
    m = (model or "").lower()
    for r in rules:
        if m.startswith(r[0]):
            return r[1:]
    return rules[-1][1:]


def cost_usd(tin, tout, tcr, tcw, price):
    return (tin * price[0] + tout * price[1] + tcr * price[2]
            + tcw * price[3]) / 1e6


def _int(v):
    try:
        return int(v)
    except (TypeError, ValueError):
        return None


def _f(v):
    try:
        return float(v)
    except (TypeError, ValueError):
        return None


def cmd_collect(args):
    c = db()
    token = None
    try:
        token = read_token()
    except Exception as e:
        log_run(c, "auth", "error", str(e))
    org = read_org(token) if token else None
    t0 = time.time()
    results = {}
    for name, fn in (("quota", lambda: collect_quota(c, token)),
                     ("cloud", lambda: collect_cloud(c, token, org)),
                     ("local", lambda: collect_local(c)),
                     ("cursor", lambda: collect_cursor(c)),
                     ("antigravity", lambda: collect_antigravity(c)),
                     ("zcode", lambda: collect_zcode(c)),
                     ("grok", lambda: collect_grok(c)),
                     ("claude", lambda: collect_claude(c)),
                     ("codex", lambda: collect_codex(c))):
        if args.only and name != args.only:
            continue
        if name in ("quota", "cloud") and not token:
            log_run(c, name, "skip", "无 token")
            results[name] = "SKIP:无token"
            continue
        try:
            results[name] = fn()
        except Exception as e:
            log_run(c, name, "error", str(e))
            results[name] = f"ERROR: {e}"
    _tag_device(c)
    c.commit()
    print(f"collect done in {time.time()-t0:.1f}s: {results}")


# ---------------------------------------------------------------- report

def _ts(v, fmt="%Y-%m-%d %H:%M"):
    return datetime.fromtimestamp(v).strftime(fmt) if v else "-"


def _tok(v):
    """token 数量人性化: 1234→1.2k, 4500000→4.5M"""
    v = v or 0
    for unit, div in (("B", 1e9), ("M", 1e6), ("k", 1e3)):
        if abs(v) >= div:
            return f"{v/div:.1f}{unit}"
    return str(int(v))


def _day_start(offset_days=0):
    d = datetime.now().replace(hour=0, minute=0, second=0, microsecond=0)
    return int(d.timestamp()) - offset_days * 86400


def cmd_quota(args):
    c = db()
    row = c.execute("""SELECT * FROM quota_snapshots ORDER BY ts DESC LIMIT 1""").fetchone()
    if not row:
        print("还没有配额快照，先跑 collect"); return
    # 前 12 列固定；后续列（device 等）忽略
    (ts, plan, tier, wq, ov, pc, ps, pe, dr, wr, nm, _raw) = row[:12]
    ov_usd = (ov or 0) / 1e6
    print(f"[{_ts(ts)}] plan={plan}({tier})  promptCredits={'∞' if pc == -1 else pc}")
    print(f"  周配额剩余: {wq}%   (重置 {_ts(wr)})")
    print(f"  日配额重置: {_ts(dr)}   超额余额: ${ov_usd:.2f}   计费周期: {ps} ~ {pe}")
    print(f"  模型目录: {nm} 个")
    hist = c.execute("""SELECT ts, weekly_quota_remaining_pct, overage_balance_micros
                        FROM quota_snapshots ORDER BY ts DESC LIMIT 15""").fetchall()
    if len(hist) > 1:
        print("  最近快照:")
        for hts, hwq, hov in reversed(hist):
            print(f"    {_ts(hts)}  weekly={hwq}%  overage=${(hov or 0)/1e6:.2f}")


def cmd_report(args):
    c = db()
    if args.today:
        since, label = _day_start(), "今天"
    elif args.week:
        since, label = _day_start(6), "近 7 天"
    elif args.month:
        since, label = _day_start(29), "近 30 天"
    else:
        since, label = 0, "全部"

    out = {}

    # quota 最新 + 区间起止对比
    q_new = c.execute("SELECT ts,weekly_quota_remaining_pct,overage_balance_micros,plan_name FROM quota_snapshots ORDER BY ts DESC LIMIT 1").fetchone()
    q_old = c.execute("SELECT ts,weekly_quota_remaining_pct,overage_balance_micros FROM quota_snapshots WHERE ts<=? ORDER BY ts DESC LIMIT 1", (since,)).fetchone()
    quota = {"latest": None, "delta": None}
    if q_new:
        quota["latest"] = {"ts": q_new[0], "plan": q_new[3],
                           "weekly_remaining_pct": q_new[1],
                           "overage_usd": round((q_new[2] or 0) / 1e6, 4)}
        if q_old and q_old[1] is not None and q_new[1] is not None:
            quota["delta"] = {"weekly_pct_used": round(q_old[1] - q_new[1], 2),
                              "overage_usd_delta": round(((q_new[2] or 0) - (q_old[2] or 0)) / 1e6, 4)}
    out["quota"] = quota

    # local sessions
    ls = c.execute("""SELECT source, model, count(*),
                      sum(n_user), sum(n_assistant), sum(n_tool_calls), sum(n_prompts),
                      sum(last_activity_at - created_at), sum(credit_cost), sum(acu_cost),
                      sum(tok_in), sum(tok_out), sum(tok_cache_read), sum(tok_cache_write)
                      FROM local_sessions WHERE created_at>=?
                      AND source NOT IN ('cursor','antigravity','zcode','grok','claude','codex')
                      GROUP BY source, model
                      ORDER BY 3 DESC""", (since,)).fetchall()
    out["local_by_model"] = [
        {"source": r[0], "model": r[1] or "(default)", "sessions": r[2],
         "user_msgs": r[3], "assistant_msgs": r[4], "tool_calls": r[5],
         "prompts": r[6], "lifespan_seconds": max(r[7] or 0, 0),
         "credit_cost": r[8] or 0, "acu_cost": r[9] or 0,
         "tok_in": r[10] or 0, "tok_out": r[11] or 0,
         "tok_cache_read": r[12] or 0, "tok_cache_write": r[13] or 0}
        for r in ls]
    tot = c.execute("""SELECT count(*), sum(n_user), sum(n_tool_calls),
                       sum(last_activity_at-created_at),
                       sum(tok_in), sum(tok_out), sum(tok_cache_read), sum(tok_cache_write)
                       FROM local_sessions
                       WHERE created_at>=?
                       AND source NOT IN ('cursor','antigravity','zcode','grok','claude','codex')""",
                       (since,)).fetchone()
    out["local_totals"] = {"sessions": tot[0] or 0, "user_msgs": tot[1] or 0,
                           "tool_calls": tot[2] or 0,
                           "lifespan_seconds": max(tot[3] or 0, 0),
                           "tok_in": tot[4] or 0, "tok_out": tot[5] or 0,
                           "tok_cache_read": tot[6] or 0, "tok_cache_write": tot[7] or 0}

    # cloud sessions
    cs = c.execute("""SELECT count(*), sum(acus_consumed), sum(n_prs)
                      FROM cloud_sessions WHERE created_at>=?""", (since,)).fetchone()
    out["cloud"] = {"sessions": cs[0] or 0, "acus": round(cs[1] or 0, 3),
                    "prs": cs[2] or 0}

    if args.json:
        print(json.dumps({"range": label, **out}, ensure_ascii=False, indent=1))
        return

    print(f"===== Devin 用量报表 · {label} =====")
    q = quota["latest"]
    if q:
        s = f"配额[{q['plan']}]: 周剩余 {q['weekly_remaining_pct']}%  超额余额 ${q['overage_usd']:.2f}"
        if quota["delta"]:
            s += f"  | 区间消耗 {quota['delta']['weekly_pct_used']}pct, ${quota['delta']['overage_usd_delta']:+.4f}"
        print(s)
    t = out["local_totals"]
    print(f"本地会话(App+CLI): {t['sessions']} 个 | 用户消息 {t['user_msgs']} | "
          f"工具调用 {t['tool_calls']} | 会话时长合计 {t['lifespan_seconds']/3600:.1f}h")
    print(f"  token: 输入 {_tok(t['tok_in'])} · 输出 {_tok(t['tok_out'])} · "
          f"缓存读 {_tok(t['tok_cache_read'])} · 缓存写 {_tok(t['tok_cache_write'])} · "
          f"总计 {_tok(t['tok_in']+t['tok_out']+t['tok_cache_read']+t['tok_cache_write'])}")
    cl = out["cloud"]
    print(f"云端会话: {cl['sessions']} 个 | ACU {cl['acus']} | PR {cl['prs']}")
    rules = load_price_rules()
    total_usd = 0.0
    print("--- 按来源/模型 ---")
    for r in out["local_by_model"]:
        usd = cost_usd(r["tok_in"], r["tok_out"], r["tok_cache_read"],
                       r["tok_cache_write"], price_of(r["model"], rules))
        total_usd += usd
        mdl = r['model'].split(',')[-1].lstrip('?')
        print(f"  {r['source']:7} {mdl:32} sess={r['sessions']:3} "
              f"msg={r['user_msgs']:4} tool={r['tool_calls']:4} "
              f"{r['lifespan_seconds']/60:6.1f}min"
              f"  in={_tok(r['tok_in'])} out={_tok(r['tok_out'])}"
              f" cr={_tok(r['tok_cache_read'])} cw={_tok(r['tok_cache_write'])}"
              f"  ≈${usd:.2f}" +
                  (f" credit={r['credit_cost']} acu={r['acu_cost']}"
                   if r["credit_cost"] or r["acu_cost"] else ""))
    print(f"等效成本合计 ≈ ${total_usd:.2f}  "
          f"(按公开 API 价折算；swe-2 无公开价按 Sonnet 档，改价见 data/prices.json)")

    # 其他应用：usage_events（cursor=服务端真实口径，antigravity=本地 gen 记录）
    ev = c.execute("""SELECT app, model, count(*), sum(tok_in), sum(tok_out),
                      sum(tok_cache_read), sum(cost_usd)
                      FROM usage_events WHERE ts>=? GROUP BY app, model
                      ORDER BY 1, 4 DESC""", (since,)).fetchall()
    if ev:
        print("--- 其他应用（事件级真实 token）---")
        for app, model, n, ti, to, tcr, cost in ev:
            usd = cost_usd(ti or 0, to or 0, tcr or 0, 0,
                           price_of(model, rules))
            cost_s = (f" 实扣${cost:.2f}" if cost else f" ≈${usd:.2f}")
            print(f"  {app:11} {((model or '?').split(',')[-1].lstrip('?')):34} req={n:5} "
                  f"in={_tok(ti)} out={_tok(to)} cr={_tok(tcr)}{cost_s}")
        real = c.execute("SELECT sum(cost_usd) FROM usage_events WHERE ts>=?",
                         (since,)).fetchone()[0]
        if real:
            print(f"  其中 Cursor 服务端实扣: ${real:.2f}")
    acts = c.execute("""SELECT day, sum(CASE WHEN metric='tabAcceptedLines' THEN value END),
                       sum(CASE WHEN metric='composerAcceptedLines' THEN value END)
                       FROM daily_activity WHERE app='cursor' AND day>=?
                       GROUP BY day ORDER BY day DESC LIMIT 10""",
                     (datetime.fromtimestamp(since).strftime("%Y-%m-%d")
                      if since else "0000-00-00",)).fetchall()
    if acts:
        print("--- Cursor 行级采纳（AI 写代码量）---")
        for day, tab, comp in acts:
            print(f"  {day}  tab={tab or 0}行  composer={comp or 0}行")


def cmd_sessions(args):
    c = db()
    rows = c.execute("""SELECT session_id, source, model, created_at, last_activity_at,
                        n_user, n_tool_calls, title, cwd
                        FROM local_sessions ORDER BY created_at DESC LIMIT ?""",
                     (args.n,)).fetchall()
    for r in rows:
        dur = max(0, (r[4] or 0) - (r[3] or 0))
        print(f"{_ts(r[3])} {r[1] or '?':7} {(r[2] or '-'):28} "
              f"{dur//60:4}min msg={r[5] or 0:3} tool={r[6] or 0:3}  {(r[7] or '')[:40]}")
    crows = c.execute("""SELECT created_at, title, status, origin, acus_consumed
                         FROM cloud_sessions ORDER BY created_at DESC LIMIT ?""",
                      (args.n,)).fetchall()
    if crows:
        print("--- 云端会话 ---")
        for r in crows:
            print(f"{_ts(r[0])} {r[3] or '-':8} {r[2] or '-':10} acu={r[4] or 0}  {(r[1] or '')[:40]}")


def cmd_watch(args):
    """每 N 秒重采 local 并打印一行实时用量（验证落库延迟/盯用量）。"""
    import itertools
    for _ in itertools.count():
        try:
            c = db()
            collect_local(c)
            c.commit()
            t = c.execute("""SELECT count(*), sum(tok_in), sum(tok_out),
                    sum(tok_cache_read), sum(tok_cache_write), sum(n_tool_calls)
                    FROM local_sessions""").fetchone()
            rules = load_price_rules()
            usd = 0.0
            for mdl, ti, to, tr, tw in c.execute(
                    """SELECT model, sum(tok_in), sum(tok_out), sum(tok_cache_read),
                       sum(tok_cache_write) FROM local_sessions GROUP BY model"""):
                usd += cost_usd(ti or 0, to or 0, tr or 0, tw or 0,
                                price_of(mdl, rules))
            c.close()
            print(f"{datetime.now().strftime('%H:%M:%S')}  "
                  f"sess={t[0]} in={_tok(t[1])} out={_tok(t[2])} "
                  f"cr={_tok(t[3])} cw={_tok(t[4])} tool={t[5]} ≈${usd:.2f}",
                  flush=True)
        except Exception as e:
            print(f"{datetime.now().strftime('%H:%M:%S')}  ERROR {e}", flush=True)
        time.sleep(args.i)


def cmd_runs(args):
    c = db()
    for r in c.execute("SELECT * FROM collect_runs ORDER BY ts DESC LIMIT ?", (args.n,)):
        print(f"{_ts(r[0])} {r[1]:6} {r[2]:6} {r[3]}")


def main():
    ap = argparse.ArgumentParser(prog="devin_usage",
                                 description="Devin App/CLI/Cloud 用量统计")
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("collect", help="采集一轮")
    p.add_argument("--only", choices=["quota", "cloud", "local"])
    p = sub.add_parser("report", help="汇总报表")
    g = p.add_mutually_exclusive_group()
    g.add_argument("--today", action="store_true")
    g.add_argument("--week", action="store_true")
    g.add_argument("--month", action="store_true")
    p.add_argument("--json", action="store_true")
    p = sub.add_parser("quota", help="配额快照")
    p = sub.add_parser("sessions", help="会话明细")
    p.add_argument("-n", type=int, default=20)
    p = sub.add_parser("runs", help="采集健康日志")
    p.add_argument("-n", type=int, default=30)
    p = sub.add_parser("watch", help="实时监控（每 N 秒重采 local 打印一行）")
    p.add_argument("-i", type=int, default=10, help="间隔秒")
    args = ap.parse_args()
    {"collect": cmd_collect, "report": cmd_report, "quota": cmd_quota,
     "sessions": cmd_sessions, "runs": cmd_runs, "watch": cmd_watch}[args.cmd](args)


if __name__ == "__main__":
    main()
