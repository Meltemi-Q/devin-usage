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
import json
import os
import re
import sqlite3
import sys
import time
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
CREATE INDEX IF NOT EXISTS idx_local_created ON local_sessions(created_at);
CREATE INDEX IF NOT EXISTS idx_cloud_created ON cloud_sessions(created_at);
"""


def db() -> sqlite3.Connection:
    DATA_DIR.mkdir(exist_ok=True)
    c = sqlite3.connect(DB_PATH)
    c.executescript(SCHEMA)
    # 轻量迁移：老库补 token 列
    have = {r[1] for r in c.execute("PRAGMA table_info(local_sessions)")}
    for col in ("tok_in", "tok_out", "tok_cache_read", "tok_cache_write", "gen_ms"):
        if col not in have:
            c.execute(f"ALTER TABLE local_sessions ADD COLUMN {col} INTEGER")
    return c


def log_run(c, source, status, detail=""):
    c.execute("INSERT INTO collect_runs VALUES (?,?,?,?)",
              (int(time.time()), source, status, detail[:500]))


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
        c.execute("""INSERT INTO quota_snapshots VALUES (?,?,?,?,?,?,?,?,?,?,?,?)""",
                  (now, pi.get("planName"), pi.get("teamsTier"),
                   ps.get("weeklyQuotaRemainingPercent"),
                   _int(ps.get("overageBalanceMicros")),
                   _int(ps.get("availablePromptCredits")),
                   ps.get("planStart"), ps.get("planEnd"),
                   _int(ps.get("dailyQuotaResetAtUnix")),
                   _int(ps.get("weeklyQuotaResetAtUnix")),
                   len(models), json.dumps(j)))
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
    src = sqlite3.connect(f"file:{LOCAL_DB}?mode=ro", uri=True)
    src.execute("PRAGMA query_only=1")
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
    src.close()
    sync_prices(c)
    log_run(c, "local", "ok", f"{n} sessions")
    return n


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
                     ("local", lambda: collect_local(c))):
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
    (ts, plan, tier, wq, ov, pc, ps, pe, dr, wr, nm, _raw) = row
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
                      FROM local_sessions WHERE created_at>=? GROUP BY source, model
                      ORDER BY 3 DESC""", (since,)).fetchall()
    out["local_by_model"] = [
        {"source": r[0], "model": r[1] or "(default)", "sessions": r[2],
         "user_msgs": r[3], "assistant_msgs": r[4], "tool_calls": r[5],
         "prompts": r[6], "lifespan_seconds": r[7] or 0,
         "credit_cost": r[8] or 0, "acu_cost": r[9] or 0,
         "tok_in": r[10] or 0, "tok_out": r[11] or 0,
         "tok_cache_read": r[12] or 0, "tok_cache_write": r[13] or 0}
        for r in ls]
    tot = c.execute("""SELECT count(*), sum(n_user), sum(n_tool_calls),
                       sum(last_activity_at-created_at),
                       sum(tok_in), sum(tok_out), sum(tok_cache_read), sum(tok_cache_write)
                       FROM local_sessions
                       WHERE created_at>=?""", (since,)).fetchone()
    out["local_totals"] = {"sessions": tot[0] or 0, "user_msgs": tot[1] or 0,
                           "tool_calls": tot[2] or 0, "lifespan_seconds": tot[3] or 0,
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
        print(f"  {r['source']:7} {r['model']:32} sess={r['sessions']:3} "
              f"msg={r['user_msgs']:4} tool={r['tool_calls']:4} "
              f"{r['lifespan_seconds']/60:6.1f}min"
              f"  in={_tok(r['tok_in'])} out={_tok(r['tok_out'])}"
              f" cr={_tok(r['tok_cache_read'])} cw={_tok(r['tok_cache_write'])}"
              f"  ≈${usd:.2f}" +
                  (f" credit={r['credit_cost']} acu={r['acu_cost']}"
                   if r["credit_cost"] or r["acu_cost"] else ""))
    print(f"等效成本合计 ≈ ${total_usd:.2f}  "
          f"(按公开 API 价折算；swe-2 无公开价按 Sonnet 档，改价见 data/prices.json)")


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
