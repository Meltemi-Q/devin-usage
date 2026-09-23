//! agent.rs — devin_usage.py 的 Rust 移植：全应用用量采集 + 多设备同步。
//!
//! 与 Python 版同库同 schema，可互为替代；采集逻辑逐函数对应。
//! 子命令：collect / export [--for <dev>] / import / sync。

use std::env;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use chrono::{DateTime, Local, TimeZone};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

// ---------------------------------------------------------------- 常量/路径

const UA: &str = "devin-usage/1.0";
const DEVIN_API: &str = "https://api.devin.ai";
const GUS_RPC: &str =
    "https://server.codeium.com/exa.seat_management_pb.SeatManagementService/GetUserStatus";
const CURSOR_EXPORT_URL: &str =
    "https://cursor.com/api/dashboard/export-usage-events-csv";
const CURSOR_REFRESH_URL: &str = "https://api2.cursor.sh/oauth/token";
const CURSOR_CLIENT_ID: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";
const CURSOR_USAGE_URL: &str = "https://cursor.com/api/usage-summary";
const GROK_PROXY: &str = "https://cli-chat-proxy.grok.com/v1";

#[cfg(target_os = "macos")]
const IS_MAC: bool = true;
#[cfg(not(target_os = "macos"))]
const IS_MAC: bool = false;
#[cfg(target_os = "windows")]
const IS_WIN: bool = true;
#[cfg(not(target_os = "windows"))]
const IS_WIN: bool = false;

fn home() -> PathBuf {
    PathBuf::from(env::var("HOME").or_else(|_| env::var("USERPROFILE")).unwrap_or_default())
}

/// 从 exe 向上找项目根目录（exe 在 devin-usage-tray/target/{profile}/ 下）。
fn project_dir() -> PathBuf {
    if let Ok(exe) = env::current_exe() {
        for anc in exe.ancestors() {
            if anc.join("data").is_dir() || anc.join("devin_usage.py").is_file() {
                return anc.to_path_buf();
            }
        }
    }
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn data_dir() -> PathBuf {
    let d = project_dir().join("data");
    let _ = fs::create_dir_all(&d);
    d
}

fn db_path() -> PathBuf {
    data_dir().join("usage.db")
}

pub fn device() -> String {
    if let Ok(d) = env::var("USAGE_DEVICE") {
        if !d.is_empty() {
            return d;
        }
    }
    for k in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(d) = env::var(k) {
            if !d.is_empty() {
                return d;
            }
        }
    }
    Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

// Devin CLI/App 目录
fn devin_dirs() -> (PathBuf, PathBuf) {
    // (DEVIN_DIR, CLI_DIR)
    if IS_MAC {
        (
            home().join("Library/Application Support/Devin"),
            home().join(".local/share/devin/cli"),
        )
    } else if IS_WIN {
        let appdata = PathBuf::from(
            env::var("APPDATA").unwrap_or_else(|_| home().join("AppData/Roaming").to_string_lossy().into()),
        );
        let devin = appdata.join("Devin");
        (devin.clone(), devin.join("cli"))
    } else {
        let xdg_data = PathBuf::from(
            env::var("XDG_DATA_HOME").unwrap_or_else(|_| home().join(".local/share").to_string_lossy().into()),
        );
        (xdg_data.join("devin"), xdg_data.join("devin/cli"))
    }
}

fn cred_files() -> Vec<PathBuf> {
    let (d, cli) = devin_dirs();
    let mut v = vec![d.join("credentials.toml"), cli.join("credentials.toml")];
    if !IS_WIN && !IS_MAC {
        let xdg_conf = PathBuf::from(
            env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| home().join(".config").to_string_lossy().into()),
        );
        v.push(xdg_conf.join("devin/credentials.toml"));
    }
    v
}

fn config_files() -> Vec<PathBuf> {
    let (d, _) = devin_dirs();
    let mut v = vec![d.join("config.json")];
    if !IS_WIN && !IS_MAC {
        let xdg_conf = PathBuf::from(
            env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| home().join(".config").to_string_lossy().into()),
        );
        v.push(xdg_conf.join("devin/config.json"));
    }
    v
}

fn cursor_state_db() -> PathBuf {
    if IS_MAC {
        home().join("Library/Application Support/Cursor/User/globalStorage/state.vscdb")
    } else if IS_WIN {
        PathBuf::from(env::var("APPDATA").unwrap_or_default())
            .join("Cursor/User/globalStorage/state.vscdb")
    } else {
        let xdg = PathBuf::from(
            env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| home().join(".config").to_string_lossy().into()),
        );
        xdg.join("Cursor/User/globalStorage/state.vscdb")
    }
}

// ---------------------------------------------------------------- 小工具

fn now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn opt_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

fn opt_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

/// serde_json::Value → rusqlite 值（import 参数用）。
fn j2s(v: &Value) -> rusqlite::types::Value {
    match v {
        Value::Null => rusqlite::types::Value::Null,
        Value::Bool(b) => rusqlite::types::Value::Integer(*b as i64),
        Value::Number(n) => n
            .as_i64()
            .map(rusqlite::types::Value::Integer)
            .unwrap_or_else(|| {
                rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0))
            }),
        Value::String(s) => rusqlite::types::Value::Text(s.clone()),
        other => rusqlite::types::Value::Text(other.to_string()),
    }
}

/// sqlite 行单元格 → serde_json::Value（export 用）。
fn row_json(r: &rusqlite::Row, i: usize) -> Value {
    use rusqlite::types::ValueRef;
    match r.get_ref(i) {
        Ok(ValueRef::Null) => Value::Null,
        Ok(ValueRef::Integer(n)) => json!(n),
        Ok(ValueRef::Real(f)) => json!(f),
        Ok(ValueRef::Text(t)) => json!(String::from_utf8_lossy(t)),
        Ok(ValueRef::Blob(b)) => json!(format!("<blob {}B>", b.len())),
        Err(_) => Value::Null,
    }
}

fn vget<'a>(v: &'a Value, k: &str) -> Option<&'a Value> {
    v.get(k).filter(|x| !x.is_null())
}

fn vstr(v: &Value, k: &str) -> Option<String> {
    vget(v, k).and_then(|x| x.as_str().map(|s| s.to_string()))
}

fn iso_ts(s: &Value) -> Option<i64> {
    let s = s.as_str()?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp())
        .or_else(|| {
            // 兼容无 Z/时区后缀的 ISO
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|n| Local.from_local_datetime(&n).single().map(|d| d.timestamp()).unwrap_or(0))
        })
        .filter(|t| *t > 0)
}

fn day_of(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "1970-01-01".into())
}

fn uuid4() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 简单 HTTP：超时可控，返回 (status, body-string)。
fn http_get(url: &str, headers: &[(&str, String)], timeout_s: u64) -> Result<(u16, String), String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_s)))
        .build()
        .new_agent();
    let mut req = agent.get(url).header("User-Agent", UA);
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    match req.call() {
        Ok(mut r) => {
            let mut s = String::new();
            r.body_mut().as_reader().read_to_string(&mut s).map_err(|e| e.to_string())?;
            Ok((r.status().as_u16(), s))
        }
        Err(ureq::Error::StatusCode(code)) => {
            Err(format!("HTTP {code}"))
        }
        Err(e) => Err(e.to_string()),
    }
}

fn http_post_json(url: &str, body: &Value, headers: &[(&str, String)], timeout_s: u64)
    -> Result<(u16, String), String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_s)))
        .build()
        .new_agent();
    let mut req = agent.post(url)
        .header("User-Agent", UA)
        .header("Content-Type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    match req.send_json(body) {
        Ok(mut r) => {
            let mut s = String::new();
            r.body_mut().as_reader().read_to_string(&mut s).map_err(|e| e.to_string())?;
            Ok((r.status().as_u16(), s))
        }
        Err(ureq::Error::StatusCode(code)) => Err(format!("HTTP {code}")),
        Err(e) => Err(e.to_string()),
    }
}

/// Connect-RPC POST（GetUserStatus 等）
fn post_rpc(url: &str, payload: &Value, timeout_s: u64) -> Result<Value, String> {
    let (_, s) = http_post_json(
        url, payload,
        &[("Connect-Protocol-Version", "1".into())],
        timeout_s,
    )?;
    serde_json::from_str(&s).map_err(|e| e.to_string())
}

/// 只读打开第三方 sqlite（PRAGMA query_only）。
fn ro_conn(path: &Path) -> Option<Connection> {
    let c = Connection::open(path).ok()?;
    let _ = c.execute_batch("PRAGMA query_only=1");
    Some(c)
}

// ---------------------------------------------------------------- store

const SCHEMA: &str = r#"
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
  source TEXT,
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
CREATE TABLE IF NOT EXISTS usage_events (
  app TEXT NOT NULL,
  event_key TEXT NOT NULL,
  ts INTEGER NOT NULL,
  day TEXT NOT NULL,
  model TEXT,
  kind TEXT,
  session_id TEXT,
  tok_in INTEGER DEFAULT 0, tok_out INTEGER DEFAULT 0,
  tok_cache_read INTEGER DEFAULT 0, tok_cache_write INTEGER DEFAULT 0,
  cost_usd REAL,
  meta TEXT,
  device TEXT DEFAULT '',
  PRIMARY KEY(app, event_key)
);
CREATE TABLE IF NOT EXISTS daily_activity (
  app TEXT NOT NULL, day TEXT NOT NULL, metric TEXT NOT NULL, value INTEGER,
  device TEXT DEFAULT '',
  PRIMARY KEY(app, day, metric, device)
);
CREATE TABLE IF NOT EXISTS app_quota (
  app TEXT NOT NULL, label TEXT NOT NULL, ts INTEGER NOT NULL,
  pct_remaining REAL,
  used REAL, lim REAL,
  resets_at INTEGER,
  meta TEXT,
  device TEXT DEFAULT '',
  PRIMARY KEY(app, label, ts)
);
CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT);
CREATE INDEX IF NOT EXISTS idx_local_created ON local_sessions(created_at);
CREATE INDEX IF NOT EXISTS idx_cloud_created ON cloud_sessions(created_at);
CREATE INDEX IF NOT EXISTS idx_events_day ON usage_events(day);
CREATE INDEX IF NOT EXISTS idx_events_model ON usage_events(app, model);
"#;

pub fn open_db() -> Option<Connection> {
    let c = Connection::open(db_path()).ok()?;
    // daily_activity 老表主键无 device——重建（PK 无法用 ALTER 改）
    {
        let da_cols: Vec<String> = c
            .prepare("PRAGMA table_info(daily_activity)")
            .ok()
            .and_then(|mut s| {
                s.query_map([], |r| r.get::<_, String>(1))
                    .ok()
                    .map(|rows| rows.flatten().collect())
            })
            .unwrap_or_default();
        if !da_cols.is_empty() && !da_cols.iter().any(|x| x == "device") {
            let _ = c.execute_batch(
                "ALTER TABLE daily_activity RENAME TO daily_activity_old;",
            );
        }
    }
    let _ = c.execute_batch(SCHEMA);
    {
        let has_old: bool = c
            .prepare("SELECT 1 FROM sqlite_master WHERE name='daily_activity_old'")
            .and_then(|mut s| s.exists([]))
            .unwrap_or(false);
        if has_old {
            let _ = c.execute_batch(
                "INSERT OR REPLACE INTO daily_activity (app,day,metric,value,device)
                 SELECT app,day,metric,value,'' FROM daily_activity_old;
                 DROP TABLE daily_activity_old;",
            );
        }
    }
    // 轻量迁移：老库补列
    for (table, cols) in [
        ("local_sessions", vec!["tok_in", "tok_out", "tok_cache_read", "tok_cache_write", "gen_ms"]),
        ("usage_events", vec!["device"]),
        ("app_quota", vec!["device"]),
        ("local_sessions", vec!["device"]),
        ("cloud_sessions", vec!["device"]),
        ("quota_snapshots", vec!["device"]),
        ("daily_activity", vec!["device"]),
    ] {
        let have: Vec<String> = c
            .prepare(&format!("PRAGMA table_info({table})"))
            .ok()
            .and_then(|mut s| {
                s.query_map([], |r| r.get::<_, String>(1))
                    .ok()
                    .map(|rows| rows.flatten().collect())
            })
            .unwrap_or_default();
        for col in cols {
            if !have.iter().any(|h| h == col) {
                let _ = c.execute(&format!("ALTER TABLE {table} ADD COLUMN {col} TEXT DEFAULT ''"), []);
            }
        }
    }
    // 一次性修正：grok/zcode/antigravity 旧行 tok_in 含缓存
    let done: Option<String> = c
        .query_row("SELECT value FROM kv WHERE key='fix_cache_dedup_v1'", [], |r| r.get(0))
        .optional()
        .unwrap_or(None);
    if done.is_none() {
        let _ = c.execute(
            "UPDATE usage_events SET tok_in = MAX(0, tok_in - tok_cache_read - tok_cache_write)
             WHERE app IN ('grok','zcode','antigravity')", []);
        let _ = c.execute("INSERT OR REPLACE INTO kv VALUES('fix_cache_dedup_v1','1')", []);
    }
    // 一次性回填：老行 device='' → 本机（都是本机采的）
    let dev = device();
    for t in ["usage_events", "local_sessions", "app_quota",
              "cloud_sessions", "quota_snapshots", "daily_activity"] {
        let _ = c.execute(
            &format!("UPDATE {t} SET device=?1 WHERE device IS NULL OR device=''"),
            params![dev],
        );
    }
    Some(c)
}

fn log_run(c: &Connection, source: &str, status: &str, detail: &str) {
    let _ = c.execute(
        "INSERT INTO collect_runs VALUES (?,?,?,?)",
        params![now_s(), source, status, &detail[..detail.len().min(500)]],
    );
}

/// collect 末尾：把本地新采的未打标签行打上本机 device。
fn tag_device(c: &Connection) {
    let dev = device();
    for t in ["usage_events", "local_sessions", "app_quota",
              "cloud_sessions", "quota_snapshots", "daily_activity"] {
        let _ = c.execute(
            &format!("UPDATE {t} SET device=?1 WHERE device IS NULL OR device=''"),
            params![dev],
        );
    }
}

// ---------------------------------------------------------------- creds

fn read_token() -> Option<String> {
    let re = regex::Regex::new(r#"windsurf_api_key\s*=\s*"([^"]+)""#).unwrap();
    for p in cred_files() {
        if let Ok(txt) = fs::read_to_string(&p) {
            if let Some(m) = re.captures(&txt) {
                return Some(m[1].to_string());
            }
        }
    }
    let tf = data_dir().join("devin-token.txt");
    if let Ok(t) = fs::read_to_string(&tf) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    None
}

fn read_org(token: Option<&str>) -> Option<String> {
    for p in config_files() {
        if let Ok(txt) = fs::read_to_string(&p) {
            if let Ok(j) = serde_json::from_str::<Value>(&txt) {
                if let Some(o) = vstr(&j["devin"], "org_id") {
                    return Some(o);
                }
            }
        }
    }
    if let Some(tok) = token {
        if let Ok((_, s)) = http_get(
            &format!("{DEVIN_API}/v3/self"),
            &[("Authorization", format!("Bearer {tok}"))],
            15,
        ) {
            if let Ok(j) = serde_json::from_str::<Value>(&s) {
                return vstr(&j, "org_id");
            }
        }
    }
    None
}

// ---------------------------------------------------------------- devin quota/cloud/local

fn collect_quota(c: &Connection, token: &str) -> Option<Value> {
    let meta = json!({
        "ideName": "windsurf", "ideVersion": "3.10.27",
        "extensionName": "windsurf", "extensionVersion": "3.10.27",
        "apiKey": token, "locale": "en", "os": "Windows",
        "sessionId": uuid4(), "requestId": "1"
    });
    let j = post_rpc(GUS_RPC, &json!({"metadata": meta}), 25).ok()?;
    let us = &j["userStatus"];
    let ps = &us["planStatus"];
    let pi = &ps["planInfo"];
    let models = us["cascadeModelConfigData"]["clientModelConfigs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let now = now_s();
    let last: i64 = c
        .query_row("SELECT MAX(ts) FROM quota_snapshots", [], |r| r.get(0))
        .optional()
        .unwrap_or(None)
        .unwrap_or(0);
    if now - last >= 300 {
        let _ = c.execute(
            "INSERT INTO quota_snapshots VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
            params![
                now,
                vstr(pi, "planName"),
                vstr(pi, "teamsTier"),
                opt_f64(&ps["weeklyQuotaRemainingPercent"]),
                opt_i64(&ps["overageBalanceMicros"]),
                opt_i64(&ps["availablePromptCredits"]),
                vstr(ps, "planStart"),
                vstr(ps, "planEnd"),
                opt_i64(&ps["dailyQuotaResetAtUnix"]),
                opt_i64(&ps["weeklyQuotaResetAtUnix"]),
                models.len() as i64,
                j.to_string()
            ],
        );
    }
    for m in &models {
        if let Some(uid) = vstr(m, "modelUid") {
            let _ = c.execute(
                "INSERT INTO model_multipliers VALUES (?,?,?,?)
                 ON CONFLICT(model_uid) DO UPDATE SET
                 label=excluded.label, credit_multiplier=excluded.credit_multiplier,
                 updated_at=excluded.updated_at",
                params![uid, vstr(m, "label"), opt_f64(&m["creditMultiplier"]), now],
            );
        }
    }
    log_run(
        c, "quota", "ok",
        &format!(
            "weekly_remaining={:?}% overage={:?}",
            ps.get("weeklyQuotaRemainingPercent"),
            ps.get("overageBalanceMicros")
        ),
    );
    Some(ps.clone())
}

fn collect_cloud(c: &Connection, token: &str, org: Option<&str>) -> i64 {
    let Some(org) = org else {
        log_run(c, "cloud", "skip", "no org_id in config.json");
        return 0;
    };
    let now = now_s();
    let mut n = 0i64;
    let mut cursor: Option<String> = None;
    let mut seen_first = std::collections::HashSet::new();
    for _ in 0..50 {
        let mut url = format!("{DEVIN_API}/v3/organizations/{org}/sessions?limit=100");
        if let Some(cur) = &cursor {
            url += &format!("&cursor={cur}");
        }
        let Ok((_, body)) = http_get(
            &url,
            &[("Authorization", format!("Bearer {token}"))],
            25,
        ) else { break };
        let Ok(page) = serde_json::from_str::<Value>(&body) else { break };
        let items = page["items"].as_array().cloned().unwrap_or_default();
        if items.is_empty() {
            break;
        }
        let first_id = vstr(&items[0], "session_id");
        if let Some(fid) = &first_id {
            if seen_first.contains(fid) {
                break;
            }
            seen_first.insert(fid.clone());
        }
        for it in &items {
            let Some(sid) = vstr(it, "session_id") else { continue };
            let _ = c.execute(
                "INSERT INTO cloud_sessions
                 (session_id,title,status,status_detail,origin,category,user_id,
                  created_at,updated_at,acus_consumed,n_prs,first_seen,last_seen)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)
                 ON CONFLICT(session_id) DO UPDATE SET
                  title=excluded.title, status=excluded.status,
                  status_detail=excluded.status_detail, origin=excluded.origin,
                  category=excluded.category, updated_at=excluded.updated_at,
                  acus_consumed=excluded.acus_consumed, n_prs=excluded.n_prs,
                  last_seen=excluded.last_seen",
                params![
                    sid,
                    vstr(it, "title"),
                    vstr(it, "status"),
                    vstr(it, "status_detail"),
                    vstr(it, "origin"),
                    vstr(it, "category"),
                    vstr(it, "user_id"),
                    opt_i64(&it["created_at"]),
                    opt_i64(&it["updated_at"]),
                    opt_f64(&it["acus_consumed"]),
                    it["pull_requests"].as_array().map(|a| a.len() as i64).unwrap_or(0),
                    now,
                    now
                ],
            );
            n += 1;
        }
        if page["has_next_page"].as_bool() != Some(true) {
            break;
        }
        match vstr(&page, "end_cursor") {
            Some(cur) => cursor = Some(cur),
            None => break,
        }
    }
    log_run(c, "cloud", "ok", &format!("{n} sessions"));
    n
}

fn collect_local(c: &Connection) -> i64 {
    let (_, cli) = devin_dirs();
    let local_db = cli.join("sessions.db");
    if !local_db.exists() {
        log_run(c, "local", "skip", &format!("{} 不存在", local_db.display()));
        return 0;
    }
    let now = now_s();
    let Some(src) = ro_conn(&local_db) else {
        log_run(c, "local", "error", "sessions.db 打不开");
        return 0;
    };
    let mut n = 0i64;
    let sql = "
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
        FROM sessions s WHERE COALESCE(s.hidden,0)=0";
    if let Ok(mut st) = src.prepare(sql) {
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, Option<i64>>(9)?,
                    r.get::<_, Option<i64>>(10)?,
                    r.get::<_, Option<i64>>(11)?,
                    r.get::<_, Option<i64>>(12)?,
                    r.get::<_, Option<i64>>(13)?,
                    r.get::<_, Option<i64>>(14)?,
                ))
            })
            .map(|it| it.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        for (sid, model, mode, backend, cwd, title, created, active, meta_s,
             nu, na, nt, ns, ntc, np_) in rows {
            let meta: Value = meta_s
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null);
            let cm = &meta["client_meta"];
            let source = if vget(cm, "cognition.ai/requestingTabId").is_some() {
                "app"
            } else if model.is_some() {
                "cli"
            } else {
                "unknown"
            };
            let _ = c.execute(
                "INSERT INTO local_sessions
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
                  last_seen=excluded.last_seen",
                params![
                    sid, source, model, mode, backend, cwd, title, created, active,
                    opt_f64(&meta["total_credit_cost"]), opt_f64(&meta["total_acu_cost"]),
                    nu, na, nt, ns, ntc, np_, now, now
                ],
            );
            n += 1;
        }
    }
    // 真实 token：assistant 消息 metadata.metrics
    if let Ok(mut st) = src.prepare(
        "SELECT session_id,
               sum(json_extract(chat_message,'$.metadata.metrics.input_tokens')),
               sum(json_extract(chat_message,'$.metadata.metrics.output_tokens')),
               sum(json_extract(chat_message,'$.metadata.metrics.cache_read_tokens')),
               sum(json_extract(chat_message,'$.metadata.metrics.cache_creation_tokens')),
               sum(json_extract(chat_message,'$.metadata.metrics.total_time_ms'))
        FROM message_nodes
        WHERE json_extract(chat_message,'$.role')='assistant'
        GROUP BY session_id",
    ) {
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                ))
            })
            .map(|it| it.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        for (sid, ti, to, tcr, tcw, gms) in rows {
            let _ = c.execute(
                "UPDATE local_sessions SET tok_in=?, tok_out=?, tok_cache_read=?,
                 tok_cache_write=?, gen_ms=? WHERE session_id=?",
                params![ti, to, tcr, tcw, gms, sid],
            );
        }
    }
    // 消息级事件：按消息时间归日。会话级 tok_* 全部堆在 created_at 那天，
    // 跨天会话（一个 session 跑好几天）会把历史消耗都算到创建日，
    // 导致"今天用量"严重低估——usage_events 按 node 时间戳记。
    if let Ok(mut st) = src.prepare(
        "SELECT m.session_id, m.node_id, m.created_at,
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
           AND json_extract(m.chat_message,'$.metadata.metrics.input_tokens') IS NOT NULL",
    ) {
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, Option<String>>(10)?,
                ))
            })
            .map(|it| it.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        let dev = device();
        for (sid, nid, ts, gmodel, ti, to, tcr, tcw, gms, smodel, meta_s) in rows {
            let Some(ts) = ts else { continue };
            let meta: Value = meta_s
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null);
            let kind = if vget(&meta["client_meta"], "cognition.ai/requestingTabId").is_some() {
                "app"
            } else {
                "cli"
            };
            let model = gmodel.or(smodel).unwrap_or_else(|| "?".into());
            let emeta = json!({"gen_ms": gms});
            let _ = c.execute(
                "INSERT OR IGNORE INTO usage_events
                 (app,event_key,ts,day,model,kind,session_id,
                  tok_in,tok_out,tok_cache_read,tok_cache_write,cost_usd,meta,device)
                 VALUES('devin',?,?,?,?,?,?,?,?,?,?,NULL,?,?)",
                params![
                    format!("devin:{sid}:{nid}"), ts, day_of(ts), model, kind, sid,
                    ti, to, tcr, tcw, emeta.to_string(), dev
                ],
            );
        }
    }
    sync_prices(c);
    log_run(c, "local", "ok", &format!("{n} sessions"));
    n
}

// ---------------------------------------------------------------- cursor

fn jwt_payload(tok: &str) -> Value {
    let Some(part) = tok.split('.').nth(1) else { return Value::Null };
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(part)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

fn cursor_item(key: &str) -> Option<String> {
    let src = ro_conn(&cursor_state_db())?;
    let v: Option<String> = src
        .query_row("SELECT value FROM ItemTable WHERE key=?", params![key], |r| r.get(0))
        .optional()
        .unwrap_or(None);
    Some(v?)
}

fn kv_get(c: &Connection, key: &str) -> Option<String> {
    c.query_row("SELECT value FROM kv WHERE key=?", params![key], |r| r.get::<_, String>(0))
        .optional()
        .unwrap_or(None)
}

fn kv_set(c: &Connection, key: &str, val: &str) {
    let _ = c.execute("INSERT OR REPLACE INTO kv VALUES(?,?)", params![key, val]);
}

fn cursor_access_token(c: &Connection) -> Option<String> {
    let mut access = kv_get(c, "cursor.access");
    let mut refresh = kv_get(c, "cursor.refresh");
    if access.is_none() {
        access = cursor_item("cursorAuth/accessToken");
        if let Some(r) = cursor_item("cursorAuth/refreshToken") {
            refresh = Some(r);
        }
    }
    if access.is_none() && refresh.is_none() {
        return None;
    }
    let exp = access
        .as_deref()
        .map(|a| opt_i64(&jwt_payload(a)["exp"]).unwrap_or(0))
        .unwrap_or(0);
    if exp - now_s() > 300 {
        return access;
    }
    let Some(rt) = refresh else { return access };
    let body = json!({
        "grant_type": "refresh_token",
        "client_id": CURSOR_CLIENT_ID,
        "refresh_token": rt
    });
    if let Ok((_, s)) = http_post_json(CURSOR_REFRESH_URL, &body, &[], 20) {
        if let Ok(r) = serde_json::from_str::<Value>(&s) {
            if let Some(nt) = vstr(&r, "access_token") {
                kv_set(c, "cursor.access", &nt);
                if let Some(nr) = vstr(&r, "refresh_token") {
                    kv_set(c, "cursor.refresh", &nr);
                }
                return Some(nt);
            }
        }
    }
    access
}

fn collect_cursor_csv(c: &Connection, token: &str) -> Result<i64, String> {
    let uid = jwt_payload(token)["sub"]
        .as_str()
        .and_then(|s| s.rsplit('|').next().map(|x| x.to_string()))
        .filter(|s| !s.is_empty())
        .ok_or("cursor JWT 无 sub")?;
    let end = now_s() * 1000;
    let start = end - 60 * 86400 * 1000;
    let url = format!("{CURSOR_EXPORT_URL}?startDate={start}&endDate={end}&strategy=tokens");
    let (_, body) = http_get(
        &url,
        &[
            ("Cookie", format!("WorkosCursorSessionToken={uid}%3A%3A{token}")),
            ("Accept", "text/csv".into()),
        ],
        40,
    )?;
    if !body.starts_with("Date") {
        return Err(format!("CSV 响应异常: {}", &body[..body.len().min(80)]));
    }
    let mut rdr = csv::Reader::from_reader(body.as_bytes());
    let headers = rdr.headers().map_err(|e| e.to_string())?.clone();
    let idx = |name: &str| headers.iter().position(|h| h == name);
    let cols: Vec<Option<usize>> = [
        "Date", "Kind", "Model", "Max Mode", "Input (w/ Cache Write)",
        "Input (w/o Cache Write)", "Cache Read", "Output Tokens", "Cost",
        "Cloud Agent ID",
    ]
    .iter()
    .map(|n| idx(n))
    .collect();
    let gi = |rec: &csv::StringRecord, i: usize| -> i64 {
        cols[i]
            .and_then(|p| rec.get(p))
            .map(|s| s.replace(',', "").trim().parse::<i64>().unwrap_or(0))
            .unwrap_or(0)
    };
    let gs = |rec: &csv::StringRecord, i: usize| -> String {
        cols[i]
            .and_then(|p| rec.get(p))
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let mut seen: std::collections::HashMap<String, i64> = Default::default();
    let mut n = 0i64;
    for rec in rdr.records().flatten() {
        let canon: String = [0, 1, 2, 3, 4, 5, 6, 7]
            .iter()
            .map(|&i| gs(&rec, i))
            .collect::<Vec<_>>()
            .join("|");
        use sha1::Digest;
        let h = format!("{:x}", sha1::Sha1::digest(canon.as_bytes()));
        let h = &h[..16];
        let cnt = seen.entry(h.to_string()).or_insert(0);
        *cnt += 1;
        let key = format!("{h}:{cnt}");
        let Some(ts) = iso_ts(&Value::String(gs(&rec, 0))) else { continue };
        let cost_raw = gs(&rec, 8);
        let cost: Option<f64> = match cost_raw.as_str() {
            "" | "Included" => None,
            s => s.trim_start_matches('$').parse::<f64>().ok(),
        };
        let agent_id = gs(&rec, 9);
        let meta = json!({
            "max_mode": gs(&rec, 3) == "Yes",
            "cost_raw": cost_raw,
            "agent_id": agent_id,
        });
        let _ = c.execute(
            "INSERT OR IGNORE INTO usage_events
             (app,event_key,ts,day,model,kind,session_id,
              tok_in,tok_out,tok_cache_read,tok_cache_write,cost_usd,meta,device)
             VALUES('cursor',?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![
                key, ts, day_of(ts), gs(&rec, 2), gs(&rec, 1),
                if agent_id.is_empty() { None } else { Some(agent_id) },
                gi(&rec, 4) + gi(&rec, 5), gi(&rec, 7), gi(&rec, 6), gi(&rec, 4),
                cost, meta.to_string(), device()
            ],
        ).map(|rc| n += rc as i64);
    }
    Ok(n)
}

fn cursor_usage_summary(c: &Connection, token: &str) -> i64 {
    let Some(uid) = jwt_payload(token)["sub"]
        .as_str()
        .and_then(|s| s.rsplit('|').next().map(|x| x.to_string()))
        .filter(|s| !s.is_empty())
    else { return 0 };
    let Ok((_, body)) = http_get(
        CURSOR_USAGE_URL,
        &[("Cookie", format!("WorkosCursorSessionToken={uid}%3A%3A{token}")),
          ("Accept", "application/json".into())],
        20,
    ) else { return 0 };
    let Ok(j) = serde_json::from_str::<Value>(&body) else { return 0 };
    let plan = &j["individualUsage"]["plan"];
    let now = now_s();
    let resets = vstr(&j, "billingCycleEnd").and_then(|s| iso_ts(&Value::String(s)));
    let mut n = 0;
    if let Some(tot) = opt_f64(&plan["totalPercentUsed"]) {
        let meta = json!({
            "membership": j["membershipType"],
            "auto_pct": opt_f64(&plan["autoPercentUsed"]),
            "api_pct": opt_f64(&plan["apiPercentUsed"]),
        });
        let _ = c.execute(
            "INSERT OR REPLACE INTO app_quota VALUES('cursor','plan',?,?,?,?,?,?,?)",
            params![now, 100.0 - tot, opt_f64(&plan["used"]), opt_f64(&plan["limit"]),
                    resets, meta.to_string(), device()],
        );
        n += 1;
    }
    for (label, key) in [("auto", "autoPercentUsed"), ("api", "apiPercentUsed")] {
        if let Some(p) = opt_f64(&plan[key]) {
            let _ = c.execute(
                "INSERT OR REPLACE INTO app_quota VALUES('cursor',?,?,?,?,?,?,?,?)",
                params![label, now, 100.0 - p, rusqlite::types::Value::Null,
                        rusqlite::types::Value::Null, resets,
                        rusqlite::types::Value::Null, device()],
            );
            n += 1;
        }
    }
    if let Some(mt) = vstr(&j, "membershipType") {
        kv_set(c, "cursor.plan", &mt);
    }
    n
}

fn collect_cursor_local(c: &Connection) -> i64 {
    let dbp = cursor_state_db();
    if !dbp.exists() {
        return 0;
    }
    let Some(src) = ro_conn(&dbp) else { return 0 };
    let now = now_s();
    let mut n = 0i64;
    let headers: Vec<(String, Option<i64>, Option<i64>, Option<String>)> = src
        .prepare("SELECT composerId, createdAt, lastUpdatedAt, value FROM composerHeaders")
        .and_then(|mut s| {
            s.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .map(|it| it.flatten().collect())
        })
        .unwrap_or_default();
    for (cid, created_ms, updated_ms, hval) in headers {
        let have: Option<i64> = c
            .query_row(
                "SELECT last_seen FROM local_sessions WHERE session_id=?",
                params![format!("cursor:{cid}")],
                |r| r.get(0),
            )
            .optional()
            .unwrap_or(None);
        if let (Some(ls), Some(ums)) = (have, updated_ms) {
            if ums / 1000 <= ls {
                continue;
            }
        }
        let cdv: Option<String> = src
            .query_row(
                "SELECT value FROM cursorDiskKV WHERE key=?",
                params![format!("composerData:{cid}")],
                |r| r.get(0),
            )
            .optional()
            .unwrap_or(None);
        let mut name = hval
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .and_then(|v| vstr(&v, "name"));
        let (mut nu, mut na) = (0i64, 0i64);
        let mut model: Option<String> = None;
        if let Some(cd_s) = cdv {
            if let Ok(cd) = serde_json::from_str::<Value>(&cd_s) {
                if name.is_none() {
                    name = vstr(&cd, "name");
                }
                model = vstr(&cd["modelConfig"], "modelName");
                if let Some(bubs) = cd["fullConversationHeadersOnly"].as_array() {
                    for b in bubs {
                        match opt_i64(&b["type"]) {
                            Some(1) => nu += 1,
                            Some(2) => na += 1,
                            _ => {}
                        }
                    }
                }
            }
        }
        let _ = c.execute(
            "INSERT INTO local_sessions
             (session_id,source,model,title,created_at,last_activity_at,
              n_user,n_assistant,n_tool,n_system,n_tool_calls,n_prompts,
              tok_in,tok_out,tok_cache_read,tok_cache_write,first_seen,last_seen)
             VALUES (?,?,?,?,?,?,?,?,0,0,0,?,0,0,0,0,?,?)
             ON CONFLICT(session_id) DO UPDATE SET
              model=excluded.model, title=excluded.title,
              last_activity_at=excluded.last_activity_at,
              n_user=excluded.n_user, n_assistant=excluded.n_assistant,
              n_prompts=excluded.n_prompts,
              last_seen=excluded.last_seen",
            params![
                format!("cursor:{cid}"), "cursor", model, name,
                created_ms.unwrap_or(0) / 1000, updated_ms.unwrap_or(0) / 1000,
                nu, na, nu, now, now
            ],
        );
        n += 1;
    }
    // Tab/Composer 日行
    if let Ok(mut st) = src.prepare(
        "SELECT key,value FROM ItemTable WHERE key LIKE 'aiCodeTracking.dailyStats.%'",
    ) {
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map(|it| it.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        for (key, val) in rows {
            let Ok(d) = serde_json::from_str::<Value>(&val) else { continue };
            let day = vstr(&d, "date").unwrap_or_else(|| {
                key.rsplit('.').next().unwrap_or("").to_string()
            });
            for metric in ["tabSuggestedLines", "tabAcceptedLines",
                           "composerSuggestedLines", "composerAcceptedLines"] {
                if let Some(v) = opt_i64(&d[metric]) {
                    let _ = c.execute(
                        "INSERT OR REPLACE INTO daily_activity VALUES('cursor',?,?,?,?)",
                        params![day, metric, v, device()],
                    );
                }
            }
        }
    }
    n
}

fn collect_cursor(c: &Connection) -> i64 {
    if !cursor_state_db().exists() {
        log_run(c, "cursor", "skip", "state.vscdb 不存在");
        return 0;
    }
    let n_sess = collect_cursor_local(c);
    let token = cursor_access_token(c);
    let mut n_ev = 0i64;
    if let Some(tok) = &token {
        match collect_cursor_csv(c, tok) {
            Ok(k) => n_ev = k,
            Err(e) => log_run(c, "cursor-csv", "error", &e),
        }
        let n_q = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cursor_usage_summary(c, tok)
        }))
        .unwrap_or(0);
        log_run(c, "cursor-quota", "ok", &format!("{n_q} 行"));
    } else {
        log_run(c, "cursor-csv", "skip", "无 access token");
    }
    log_run(c, "cursor", "ok", &format!("{n_sess} composers, +{n_ev} events"));
    n_sess
}

// ---------------------------------------------------------------- antigravity

/// 极简 protobuf 解码：产出 (field_no, wire_type, value)。
fn pb_fields(data: &[u8]) -> Vec<(u32, u32, Value)> {
    let mut out = Vec::new();
    let (mut i, n) = (0usize, data.len());
    while i < n {
        let mut tag = 0u64;
        let mut shift = 0;
        while i < n {
            let b = data[i];
            i += 1;
            tag |= ((b & 0x7f) as u64) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        let (fn_, wt) = ((tag >> 3) as u32, (tag & 7) as u32);
        if fn_ == 0 {
            break;
        }
        match wt {
            0 => {
                let mut v = 0u64;
                let mut shift = 0;
                while i < n {
                    let b = data[i];
                    i += 1;
                    v |= ((b & 0x7f) as u64) << shift;
                    shift += 7;
                    if b & 0x80 == 0 {
                        break;
                    }
                }
                out.push((fn_, wt, json!(v)));
            }
            2 => {
                let mut ln = 0u64;
                let mut shift = 0;
                while i < n {
                    let b = data[i];
                    i += 1;
                    ln |= ((b & 0x7f) as u64) << shift;
                    shift += 7;
                    if b & 0x80 == 0 {
                        break;
                    }
                }
                let end = (i + ln as usize).min(n);
                out.push((fn_, wt, json!(data[i..end])));
                i = end;
            }
            1 => {
                let end = (i + 8).min(n);
                out.push((fn_, wt, json!(data[i..end])));
                i += 8;
            }
            5 => {
                let end = (i + 4).min(n);
                out.push((fn_, wt, json!(data[i..end])));
                i += 4;
            }
            _ => break,
        }
    }
    out
}

fn pb_get<'a>(fields: &'a [(u32, u32, Value)], no: u32, wt: Option<u32>) -> Option<&'a Value> {
    fields.iter().find(|f| f.0 == no && wt.map_or(true, |w| f.1 == w)).map(|f| &f.2)
}

fn pb_bytes<'a>(fields: &'a [(u32, u32, Value)], no: u32) -> Option<Vec<u8>> {
    pb_get(fields, no, Some(2))
        .and_then(|v| v.as_array().map(|a| a.iter().filter_map(|x| x.as_u64().map(|b| b as u8)).collect()))
}

fn pb_ts(msg: &[u8]) -> Option<i64> {
    let f = pb_fields(msg);
    pb_get(&f, 1, Some(0))
        .and_then(|v| v.as_i64())
        .filter(|t| *t > 0)
}

fn agy_gen_event(blob: &[u8]) -> Option<Value> {
    let wrap = pb_bytes(&pb_fields(blob), 1)?;
    let w = pb_fields(&wrap);
    let model = pb_bytes(&w, 19)
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let label = pb_bytes(&w, 21)
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let usage = pb_bytes(&w, 4)?;
    let u = pb_fields(&usage);
    let get_v = |no| pb_get(&u, no, Some(0)).and_then(|v| v.as_i64()).unwrap_or(0);
    let (sys_tok, tin, tout, tcr) = (get_v(1), get_v(2), get_v(3), get_v(5));
    if model.is_none() && label.is_none() && tin == 0 && tout == 0 && tcr == 0 && sys_tok == 0 {
        return None;
    }
    let ts = pb_bytes(&w, 9)
        .and_then(|t| pb_bytes(&pb_fields(&t), 4))
        .and_then(|t| pb_ts(&t));
    Some(json!({
        "model": model, "label": label,
        "tin": (sys_tok + tin - tcr).max(0),
        "tout": tout, "tcr": tcr, "ts": ts
    }))
}

fn collect_antigravity(c: &Connection) -> i64 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| collect_antigravity_quota(c))) {
        Ok(_) => {}
        Err(_) => log_run(c, "antigravity-quota", "error", "panic"),
    }
    let mut dbs: Vec<PathBuf> = Vec::new();
    let home = home();
    if let Ok(rd) = fs::read_dir(home.join(".gemini")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("antigravity") {
                let conv = e.path().join("conversations");
                if let Ok(cd) = fs::read_dir(&conv) {
                    dbs.extend(cd.flatten().map(|x| x.path()).filter(|p| {
                        p.extension().map(|x| x == "db").unwrap_or(false)
                    }));
                }
            }
        }
    }
    if dbs.is_empty() {
        log_run(c, "antigravity", "skip", "无 conversations/*.db");
        return 0;
    }
    let now = now_s();
    let (mut n_ev, mut n_sess) = (0i64, 0i64);
    for p in dbs {
        let r = (|| -> Result<(), String> {
            let src = ro_conn(&p).ok_or("open fail")?;
            let meta: Option<(Option<String>, Option<String>)> = src
                .query_row(
                    "SELECT trajectory_id, cascade_id FROM trajectory_meta LIMIT 1",
                    [], |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .unwrap_or(None);
            let tid = meta
                .and_then(|(a, b)| a.or(b))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| p.file_stem().unwrap_or_default().to_string_lossy().into());
            let mut steps_ts: std::collections::HashMap<i64, i64> = Default::default();
            if let Ok(mut st) = src.prepare("SELECT idx, metadata FROM steps") {
                let rows = st
                    .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<Vec<u8>>>(1)?)))
                    .map(|it| it.flatten().collect::<Vec<_>>())
                    .unwrap_or_default();
                for (idx, md) in rows {
                    if let Some(md) = md {
                        if let Some(t) = pb_bytes(&pb_fields(&md), 1).and_then(|b| pb_ts(&b)) {
                            steps_ts.insert(idx, t);
                        }
                    }
                }
            }
            let mut evs: Vec<(i64, i64, Value)> = Vec::new();
            if let Ok(mut st) = src.prepare("SELECT idx, data FROM gen_metadata") {
                let rows = st
                    .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<Vec<u8>>>(1)?)))
                    .map(|it| it.flatten().collect::<Vec<_>>())
                    .unwrap_or_default();
                for (idx, blob) in rows {
                    let Some(ev) = blob.as_deref().and_then(agy_gen_event) else { continue };
                    let ts = opt_i64(&ev["ts"]).or_else(|| steps_ts.get(&idx).copied());
                    let Some(ts) = ts else { continue };
                    evs.push((idx, ts, ev));
                }
            }
            let mut models = std::collections::BTreeSet::new();
            let (mut t_in, mut t_out, mut t_cr) = (0i64, 0i64, 0i64);
            let mut ts_list = Vec::new();
            for (idx, ts, ev) in &evs {
                let key = format!("{tid}:{idx}");
                let model = vstr(ev, "model").or_else(|| vstr(ev, "label")).unwrap_or("?".into());
                let meta = json!({"label": ev["label"], "model_id": ev["model"]});
                let rc = c.execute(
                    "INSERT OR IGNORE INTO usage_events
                     (app,event_key,ts,day,model,kind,session_id,
                      tok_in,tok_out,tok_cache_read,tok_cache_write,cost_usd,meta,device)
                     VALUES('antigravity',?,?,?,?,'gen',?,?,?,?,0,NULL,?,?)",
                    params![
                        key, ts, day_of(*ts), model, format!("agy:{tid}"),
                        opt_i64(&ev["tin"]).unwrap_or(0),
                        opt_i64(&ev["tout"]).unwrap_or(0),
                        opt_i64(&ev["tcr"]).unwrap_or(0),
                        meta.to_string(), device()
                    ],
                ).unwrap_or(0);
                n_ev += rc as i64;
                models.insert(vstr(ev, "label").or_else(|| vstr(ev, "model")).unwrap_or("?".into()));
                t_in += opt_i64(&ev["tin"]).unwrap_or(0);
                t_out += opt_i64(&ev["tout"]).unwrap_or(0);
                t_cr += opt_i64(&ev["tcr"]).unwrap_or(0);
                ts_list.push(*ts);
            }
            if !ts_list.is_empty() {
                let title = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
                let _ = c.execute(
                    "INSERT INTO local_sessions
                     (session_id,source,model,title,created_at,last_activity_at,
                      n_user,n_assistant,n_tool,n_system,n_tool_calls,n_prompts,
                      tok_in,tok_out,tok_cache_read,tok_cache_write,first_seen,last_seen)
                     VALUES (?,?,?,?,?,?,0,?,0,0,0,0,?,?,?,0,?,?)
                     ON CONFLICT(session_id) DO UPDATE SET
                      model=excluded.model, last_activity_at=excluded.last_activity_at,
                      n_assistant=excluded.n_assistant, tok_in=excluded.tok_in,
                      tok_out=excluded.tok_out, tok_cache_read=excluded.tok_cache_read,
                      last_seen=excluded.last_seen",
                    params![
                        format!("agy:{tid}"), "antigravity",
                        models.iter().cloned().collect::<Vec<_>>().join(",")
                            .chars().take(200).collect::<String>(),
                        title, ts_list.iter().min().copied().unwrap_or(0),
                        ts_list.iter().max().copied().unwrap_or(0),
                        ts_list.len() as i64, t_in, t_out, t_cr, now, now
                    ],
                );
                n_sess += 1;
            }
            Ok(())
        })();
        if r.is_err() {
            continue;
        }
    }
    log_run(c, "antigravity", "ok", &format!("{n_sess} conversations, +{n_ev} events"));
    n_sess
}

// ---- antigravity 配额：本机 language server（IDE 运行时才可用）

fn antigravity_ls_procs() -> Vec<(i64, Option<String>)> {
    let mut procs = Vec::new();
    let re_dir = regex::Regex::new(r"--app_data_dir[=\s]+antigravity\b").unwrap();
    let re_csrf = regex::Regex::new(r"--csrf_token[=\s]+([\w-]+)").unwrap();
    if IS_WIN {
        let ps = "Get-CimInstance Win32_Process | Where-Object {$_.CommandLine -match 'app_data_dir'} | Select-Object ProcessId,CommandLine | ConvertTo-Json -Compress";
        if let Ok(o) = Command::new("powershell")
            .args(["-NoProfile", "-Command", ps])
            .output()
        {
            let out = String::from_utf8_lossy(&o.stdout);
            if let Ok(j) = serde_json::from_str::<Value>(&out) {
                let items: Vec<&Value> = match &j {
                    Value::Array(a) => a.iter().collect(),
                    v if v.is_object() => vec![v],
                    _ => vec![],
                };
                for it in items {
                    let cl = vstr(it, "CommandLine").unwrap_or_default();
                    if re_dir.is_match(&cl) {
                        let csrf = re_csrf.captures(&cl).map(|m| m[1].to_string());
                        if let Some(pid) = opt_i64(&it["ProcessId"]) {
                            procs.push((pid, csrf));
                        }
                    }
                }
            }
        }
    } else if let Ok(o) = Command::new("ps").args(["-eo", "pid,args"]).output() {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            if re_dir.is_match(line) {
                let pid = line.trim().split_whitespace().next()
                    .and_then(|s| s.parse::<i64>().ok());
                if let Some(pid) = pid {
                    let csrf = re_csrf.captures(line).map(|m| m[1].to_string());
                    procs.push((pid, csrf));
                }
            }
        }
    }
    procs
}

fn antigravity_ports(pid: i64) -> Vec<u16> {
    let re_port = regex::Regex::new(r":(\d+)$").unwrap();
    if IS_WIN {
        if let Ok(o) = Command::new("netstat").args(["-ano", "-p", "tcp"]).output() {
            let out = String::from_utf8_lossy(&o.stdout);
            return out
                .lines()
                .filter_map(|l| {
                    let parts: Vec<&str> = l.split_whitespace().collect();
                    if parts.len() >= 5
                        && parts[parts.len() - 1] == pid.to_string()
                        && parts[parts.len() - 2].to_uppercase().contains("LISTENING")
                    {
                        re_port.captures(parts[1]).and_then(|m| m[1].parse().ok())
                    } else {
                        None
                    }
                })
                .collect();
        }
        vec![]
    } else {
        if let Ok(o) = Command::new("lsof")
            .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-a", "-p", &pid.to_string()])
            .output()
        {
            let out = String::from_utf8_lossy(&o.stdout);
            let re = regex::Regex::new(r":(\d+)\s+\(LISTEN\)").unwrap();
            return re.captures_iter(&out).filter_map(|m| m[1].parse().ok()).collect();
        }
        vec![]
    }
}

/// 对本机 language server POST 一个 Connect-RPC；https 失败退回 http。
fn ag_ls_call(pid: i64, csrf: &str, method: &str) -> Option<Value> {
    let body = json!({"metadata": {"ideName": "antigravity",
                                   "extensionName": "antigravity",
                                   "ideVersion": "1.0", "locale": "en"}});
    for port in antigravity_ports(pid) {
        for https in [true, false] {
            let scheme = if https { "https" } else { "http" };
            let url = format!("{scheme}://127.0.0.1:{port}{method}");
            // 自签证书 → 单独一个关闭校验的 agent
            let agent = if https {
                ureq::Agent::config_builder()
                    .timeout_global(Some(Duration::from_secs(6)))
                    .tls_config(
                        ureq::tls::TlsConfig::builder()
                            .disable_verification(true)
                            .build(),
                    )
                    .build()
                    .new_agent()
            } else {
                ureq::Agent::config_builder()
                    .timeout_global(Some(Duration::from_secs(6)))
                    .build()
                    .new_agent()
            };
            let r = agent
                .post(&url)
                .header("User-Agent", UA)
                .header("Content-Type", "application/json")
                .header("Connect-Protocol-Version", "1")
                .header("X-Codeium-Csrf-Token", csrf)
                .send_json(&body);
            if let Ok(mut resp) = r {
                if let Ok(v) = resp.body_mut().read_json::<Value>() {
                    return Some(v);
                }
            }
        }
    }
    None
}

fn collect_antigravity_quota(c: &Connection) -> i64 {
    let now = now_s();
    let mut n = 0i64;
    for (pid, csrf) in antigravity_ls_procs() {
        let Some(csrf) = csrf else { continue };
        let resp = ag_ls_call(
            pid, &csrf,
            "/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary",
        );
        let groups = resp
            .as_ref()
            .and_then(|r| r.get("response"))
            .and_then(|r| r.get("groups"))
            .and_then(|g| g.as_array().cloned())
            .unwrap_or_default();
        if !groups.is_empty() {
            for g in &groups {
                let gname = vstr(g, "displayName").unwrap_or_default();
                let pool = if gname.contains("Gemini") { "Gemini" } else { "Claude · GPT" };
                for b in g["buckets"].as_array().cloned().unwrap_or_default() {
                    let Some(frac) = opt_f64(&b["remainingFraction"]) else { continue };
                    let resets = vstr(&b, "resetTime").and_then(|s| iso_ts(&Value::String(s)));
                    let win = if vstr(&b, "window").as_deref() == Some("weekly") { "周" } else { "5h" };
                    let _ = c.execute(
                        "INSERT OR REPLACE INTO app_quota VALUES('antigravity',?,?,?,?,?,?,?,?)",
                        params![format!("{pool} · {win}"), now, frac * 100.0,
                                rusqlite::types::Value::Null,
                                rusqlite::types::Value::Null, resets, "{}", device()],
                    );
                    n += 1;
                }
            }
            break;
        }
        // 兜底：GetUserStatus 的 per-model quotaInfo（只有 5h）
        let Some(resp) = ag_ls_call(
            pid, &csrf,
            "/exa.language_server_pb.LanguageServerService/GetUserStatus",
        ) else { continue };
        let us = &resp["userStatus"];
        let configs = us["cascadeModelConfigData"]["clientModelConfigs"]
            .as_array()
            .cloned()
            .or_else(|| resp["clientModelConfigs"].as_array().cloned())
            .unwrap_or_default();
        let mut pools: std::collections::HashMap<String, (f64, Option<i64>)> = Default::default();
        for cfg in &configs {
            let qi = &cfg["quotaInfo"];
            let Some(frac) = opt_f64(&qi["remainingFraction"]) else { continue };
            let resets = vstr(qi, "resetTime").and_then(|s| iso_ts(&Value::String(s)));
            let label = vstr(cfg, "label")
                .or_else(|| vstr(&cfg["modelOrAlias"], "model"))
                .unwrap_or_else(|| "?".into());
            let pool = if label.starts_with("Gemini") { "Gemini" } else { "Claude · GPT" }.to_string();
            let cur = pools.get(&pool);
            if cur.map_or(true, |(f, _)| frac < *f) {
                pools.insert(pool, (frac, resets));
            }
        }
        for (pool, (frac, resets)) in pools {
            let _ = c.execute(
                "INSERT OR REPLACE INTO app_quota VALUES('antigravity',?,?,?,?,?,?,?,?)",
                params![format!("{pool} · 5h"), now, frac * 100.0,
                        rusqlite::types::Value::Null,
                        rusqlite::types::Value::Null, resets, "{}", device()],
            );
            n += 1;
        }
        break;
    }
    log_run(c, "antigravity-quota",
            if n > 0 { "ok" } else { "skip" },
            &if n > 0 { format!("{n} 行") } else { "IDE 未运行/端口不可用".into() });
    n
}

// ---------------------------------------------------------------- zcode / grok / claude

/// JSONL 只增不改：按 kv 偏移续读新行。
fn read_jsonl_incremental(c: &Connection, app: &str, path: &Path) -> Vec<Value> {
    let key = format!("off:{app}:{}", path.display());
    let off: i64 = kv_get(c, &key).and_then(|s| s.parse().ok()).unwrap_or(0);
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0) as i64;
    let off = if off > size { 0 } else { off };
    if off == size {
        return vec![];
    }
    let mut f = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    let _ = f.seek(SeekFrom::Start(off as u64));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    kv_set(c, &key, &size.to_string());
    String::from_utf8_lossy(&buf)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn collect_zcode(c: &Connection) -> i64 {
    let root = home().join(".zcode/cli/agents");
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = fs::read_dir(&root) {
        for e in rd.flatten() {
            if let Ok(rd2) = fs::read_dir(e.path()) {
                files.extend(
                    rd2.flatten()
                        .map(|x| x.path().join("transcript.jsonl"))
                        .filter(|p| p.exists()),
                );
            }
        }
    }
    if files.is_empty() {
        log_run(c, "zcode", "skip", "无 transcript.jsonl");
        return 0;
    }
    files.sort();
    let mut n_ev = 0i64;
    let mut sess: std::collections::HashMap<String, Value> = Default::default();
    for p in files {
        for e in read_jsonl_incremental(c, "zcode", &p) {
            let pay = &e["payload"];
            let sid = vstr(&e, "sessionId")
                .unwrap_or_else(|| p.parent().and_then(|x| x.file_stem())
                    .map(|x| x.to_string_lossy().into()).unwrap_or_default());
            let ts = iso_ts(&e["timestamp"]).unwrap_or(0);
            match vstr(&e, "type").as_deref() {
                Some("model_request") => {
                    // 暂存 turnId→model，后续 model_complete 用
                    let m = vstr(pay, "model").unwrap_or_default();
                    let m = m.rsplit('/').next().unwrap_or(&m).to_string();
                    let tid = vstr(&e, "turnId").unwrap_or_default();
                    sess.entry(format!("__tm:{sid}")).or_insert(json!({}))
                        .as_object_mut()
                        .map(|o| o.insert(tid, json!(m)));
                }
                Some("model_complete") => {
                    let u = &pay["usage"];
                    if opt_i64(&u["totalTokens"]).unwrap_or(0) == 0 {
                        continue;
                    }
                    let tid = vstr(&e, "turnId").unwrap_or_default();
                    let model = sess
                        .get(&format!("__tm:{sid}"))
                        .and_then(|m| vstr(m, &tid))
                        .unwrap_or_else(|| "?".into());
                    let (i_, o_, cr, cw) = (
                        opt_i64(&u["inputTokens"]).unwrap_or(0),
                        opt_i64(&u["outputTokens"]).unwrap_or(0),
                        opt_i64(&u["cacheReadTokens"]).unwrap_or(0),
                        opt_i64(&u["cacheWriteTokens"]).unwrap_or(0),
                    );
                    let rc = c.execute(
                        "INSERT OR IGNORE INTO usage_events
                         (app,event_key,ts,day,model,kind,session_id,
                          tok_in,tok_out,tok_cache_read,tok_cache_write,cost_usd,meta,device)
                         VALUES('zcode',?,?,?,?,'gen',?,?,?,?,?,NULL,?,?)",
                        params![
                            format!("{sid}:{tid}"), ts, day_of(ts), model,
                            format!("zcode:{sid}"),
                            (i_ - cr - cw).max(0), o_, cr, cw,
                            json!({"querySource": pay["querySource"]}).to_string(),
                            device()
                        ],
                    ).unwrap_or(0);
                    n_ev += rc as i64;
                    let s = sess.entry(sid.clone()).or_insert(json!({
                        "model": "?", "n": 0, "t0": ts, "t1": ts,
                        "ti": 0, "to": 0, "tcr": 0
                    }));
                    s["n"] = json!(opt_i64(&s["n"]).unwrap_or(0) + 1);
                    if model != "?" { s["model"] = json!(model); }
                    s["t0"] = json!(opt_i64(&s["t0"]).unwrap_or(ts).min(ts));
                    s["t1"] = json!(opt_i64(&s["t1"]).unwrap_or(ts).max(ts));
                    s["ti"] = json!(opt_i64(&s["ti"]).unwrap_or(0) + i_);
                    s["to"] = json!(opt_i64(&s["to"]).unwrap_or(0) + o_);
                    s["tcr"] = json!(opt_i64(&s["tcr"]).unwrap_or(0) + cr);
                }
                _ => {}
            }
        }
    }
    let now = now_s();
    let mut n_sess = 0i64;
    for (sid, s) in &sess {
        if sid.starts_with("__tm:") {
            continue;
        }
        let _ = c.execute(
            "INSERT INTO local_sessions
             (session_id,source,model,title,created_at,last_activity_at,
              n_user,n_assistant,n_tool,n_system,n_tool_calls,n_prompts,
              tok_in,tok_out,tok_cache_read,tok_cache_write,first_seen,last_seen)
             VALUES (?, 'zcode', ?, ?, ?, ?, 0, ?, 0,0,0,0, ?,?,?,0, ?,?)
             ON CONFLICT(session_id) DO UPDATE SET
              model=excluded.model, last_activity_at=excluded.last_activity_at,
              n_assistant=excluded.n_assistant, tok_in=excluded.tok_in,
              tok_out=excluded.tok_out, tok_cache_read=excluded.tok_cache_read,
              last_seen=excluded.last_seen",
            params![
                format!("zcode:{sid}"), vstr(s, "model"), sid,
                opt_i64(&s["t0"]).unwrap_or(0), opt_i64(&s["t1"]).unwrap_or(0),
                opt_i64(&s["n"]).unwrap_or(0),
                opt_i64(&s["ti"]).unwrap_or(0), opt_i64(&s["to"]).unwrap_or(0),
                opt_i64(&s["tcr"]).unwrap_or(0), now, now
            ],
        );
        n_sess += 1;
    }
    log_run(c, "zcode", "ok", &format!("{n_sess} sessions, +{n_ev} events"));
    n_sess
}

// ---- grok

fn grok_access_token() -> Option<String> {
    let txt = fs::read_to_string(home().join(".grok/auth.json")).ok()?;
    let d: Value = serde_json::from_str(&txt).ok()?;
    let now = now_s();
    for (_, k) in d.as_object()? {
        let tok = vstr(k, "key");
        let exp = iso_ts(&k["expires_at"]);
        if let Some(t) = tok {
            if exp.map_or(true, |e| e > now + 60) {
                return Some(t);
            }
        }
    }
    None
}

fn grok_headers(tok: &str) -> Vec<(&'static str, String)> {
    let ver = fs::read_to_string(home().join(".grok/version.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| vstr(&v, "version"))
        .unwrap_or_else(|| "1.0.0".into());
    vec![
        ("Authorization", format!("Bearer {tok}")),
        ("User-Agent", format!("xai-grok-cli/{ver}")),
        ("x-grok-client-version", ver),
        ("x-grok-client-identifier", "xai-grok-cli".into()),
        ("Accept", "application/json".into()),
    ]
}

fn collect_grok_quota(c: &Connection) -> i64 {
    let Some(tok) = grok_access_token() else {
        log_run(c, "grok-quota", "skip", "无有效 access token");
        return 0;
    };
    let url = format!("{GROK_PROXY}/billing?format=credits");
    let Ok((_, body)) = http_get(&url, &grok_headers(&tok), 15) else { return 0 };
    let Ok(j) = serde_json::from_str::<Value>(&body) else { return 0 };
    let cfg = &j["config"];
    let period = &cfg["currentPeriod"];
    let resets = iso_ts(&period["end"]).or_else(|| iso_ts(&cfg["billingPeriodEnd"]));
    let tier = fs::read_to_string(home().join(".grok/settings_cache.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            let pay = &v["payload"];
            let pay: Value = if pay.is_string() {
                serde_json::from_str(pay.as_str().unwrap()).unwrap_or(Value::Null)
            } else { pay.clone() };
            vstr(&pay["settings"], "subscription_tier_display")
        });
    let meta = json!({"period": period["type"], "tier": tier});
    if let Some(t) = &tier {
        kv_set(c, "grok.plan", t);
    }
    let now = now_s();
    let mut n = 0;
    if let Some(used) = opt_f64(&cfg["creditUsagePercent"]) {
        let _ = c.execute(
            "INSERT OR REPLACE INTO app_quota VALUES('grok','周额度',?,?,?,?,?,?,?)",
            params![now, (100.0 - used).max(0.0), used, 100.0, resets,
                    meta.to_string(), device()],
        );
        n += 1;
    }
    for pu in cfg["productUsage"].as_array().cloned().unwrap_or_default() {
        let Some(u) = opt_f64(&pu["usagePercent"]) else { continue };
        let _ = c.execute(
            "INSERT OR REPLACE INTO app_quota VALUES('grok',?,?,?,?,?,?,?,?)",
            params![vstr(&pu, "product").unwrap_or("?".into()), now,
                    (100.0 - u).max(0.0), u, 100.0, resets,
                    rusqlite::types::Value::Null, device()],
        );
        n += 1;
    }
    log_run(c, "grok-quota",
            if n > 0 { "ok" } else { "error" },
            &if n > 0 { format!("{n} 行") } else { format!("响应无额度字段: {}", &body[..body.len().min(120)]) });
    n
}

fn collect_grok(c: &Connection) -> i64 {
    // 配额日志由 collect_grok_quota 自己记（skip/ok/error）
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        collect_grok_quota(c)
    }))
    .map_err(|_| log_run(c, "grok-quota", "error", "panic"));
    let roots = home().join(".grok/sessions");
    if !roots.exists() {
        log_run(c, "grok", "skip", "~/.grok/sessions 不存在");
        return 0;
    }
    let mut upds: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = fs::read_dir(&roots) {
        for e in rd.flatten() {
            if let Ok(rd2) = fs::read_dir(e.path()) {
                for e2 in rd2.flatten() {
                    let f = e2.path().join("updates.jsonl");
                    if f.exists() { upds.push(f); }
                }
            }
        }
    }
    let mut n_ev = 0i64;
    for upd in upds {
        for e in read_jsonl_incremental(c, "grok", &upd) {
            let u = &e["params"]["update"];
            if u["sessionUpdate"].as_str() != Some("turn_completed") {
                continue;
            }
            let usage = &u["usage"];
            if opt_i64(&usage["totalTokens"]).unwrap_or(0) == 0 {
                continue;
            }
            let sid = vstr(&e["params"], "sessionId")
                .unwrap_or_else(|| upd.parent().and_then(|x| x.file_name())
                    .map(|x| x.to_string_lossy().into()).unwrap_or_default());
            let ts = opt_i64(&e["timestamp"]).unwrap_or(0);
            let pid = vstr(u, "prompt_id").unwrap_or_else(uuid4);
            let items: Vec<(String, &Value)> = match usage["modelUsage"].as_object() {
                Some(m) if !m.is_empty() => {
                    m.iter().map(|(k, v)| (k.clone(), v)).collect()
                }
                _ => vec![("?".into(), usage)],
            };
            for (model, mu) in items {
                let cost = opt_f64(&mu["costUsdTicks"]);
                let (i_, o_, cr, cw) = (
                    opt_i64(&mu["inputTokens"]).unwrap_or(0),
                    opt_i64(&mu["outputTokens"]).unwrap_or(0),
                    opt_i64(&mu["cachedReadTokens"]).unwrap_or(0),
                    opt_i64(&mu["cacheCreationTokens"]).unwrap_or(0),
                );
                let meta = json!({
                    "modelCalls": mu["modelCalls"],
                    "apiMs": mu["apiDurationMs"],
                    "reasoning": mu["reasoningTokens"],
                });
                let rc = c.execute(
                    "INSERT OR IGNORE INTO usage_events
                     (app,event_key,ts,day,model,kind,session_id,
                      tok_in,tok_out,tok_cache_read,tok_cache_write,cost_usd,meta,device)
                     VALUES('grok',?,?,?,?,'gen',?,?,?,?,?,?,?,?)",
                    params![
                        format!("{sid}:{pid}:{model}"), ts, day_of(ts), model,
                        format!("grok:{sid}"),
                        (i_ - cr - cw).max(0), o_, cr, cw,
                        cost.map(|x| x / 1e9), meta.to_string(), device()
                    ],
                ).unwrap_or(0);
                n_ev += rc as i64;
            }
        }
    }
    // 会话级：summary.json
    let now = now_s();
    let mut n_sess = 0i64;
    let mut sums: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = fs::read_dir(&roots) {
        for e in rd.flatten() {
            if let Ok(rd2) = fs::read_dir(e.path()) {
                for e2 in rd2.flatten() {
                    let f = e2.path().join("summary.json");
                    if f.exists() { sums.push(f); }
                }
            }
        }
    }
    for sm in sums {
        let Ok(txt) = fs::read_to_string(&sm) else { continue };
        let Ok(j) = serde_json::from_str::<Value>(&txt) else { continue };
        let sid = vstr(&j["info"], "id")
            .unwrap_or_else(|| sm.parent().and_then(|x| x.file_name())
                .map(|x| x.to_string_lossy().into()).unwrap_or_default());
        let t0 = iso_ts(&j["created_at"]);
        let t1 = iso_ts(&j["last_active_at"]).or_else(|| iso_ts(&j["updated_at"]));
        let _ = c.execute(
            "INSERT INTO local_sessions
             (session_id,source,model,title,cwd,created_at,last_activity_at,
              n_user,n_assistant,n_tool,n_system,n_tool_calls,n_prompts,
              tok_in,tok_out,tok_cache_read,tok_cache_write,first_seen,last_seen)
             VALUES (?, 'grok', ?, ?, ?, ?, ?, ?, 0,0,0,0,0, 0,0,0,0, ?,?)
             ON CONFLICT(session_id) DO UPDATE SET
              model=excluded.model, title=excluded.title,
              last_activity_at=excluded.last_activity_at,
              n_user=excluded.n_user, last_seen=excluded.last_seen",
            params![
                format!("grok:{sid}"), vstr(&j, "current_model_id").unwrap_or("?".into()),
                vstr(&j, "session_summary").unwrap_or_else(|| sid.clone()),
                vstr(&j["info"], "cwd").unwrap_or_default(),
                t0.unwrap_or(0), t1.unwrap_or(0),
                opt_i64(&j["num_messages"]).unwrap_or(0), now, now
            ],
        );
        n_sess += 1;
    }
    log_run(c, "grok", "ok", &format!("{n_sess} sessions, +{n_ev} events"));
    n_sess
}

// ---- claude code

fn collect_claude(c: &Connection) -> i64 {
    let root = home().join(".claude/projects");
    if !root.exists() {
        log_run(c, "claude", "skip", "~/.claude/projects 不存在");
        return 0;
    }
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                    files.push(p);
                }
            }
        }
    }
    let mut n_ev = 0i64;
    let mut seen = std::collections::HashSet::new();
    let mut sess: std::collections::HashMap<String, Value> = Default::default();
    for fp in files {
        for e in read_jsonl_incremental(c, "claude", &fp) {
            let msg = &e["message"];
            let u = &msg["usage"];
            if opt_i64(&u["output_tokens"]).unwrap_or(0) == 0 {
                continue;
            }
            let mid = vstr(msg, "id").or_else(|| vstr(&e, "uuid"));
            let Some(mid) = mid else { continue };
            if !seen.insert(mid.clone()) {
                continue;
            }
            let sid = vstr(&e, "sessionId")
                .unwrap_or_else(|| fp.file_stem().unwrap_or_default().to_string_lossy().into());
            let ts = iso_ts(&e["timestamp"]).unwrap_or(0);
            let model = vstr(msg, "model").unwrap_or_else(|| "?".into());
            let (i_, o_, cr, cw) = (
                opt_i64(&u["input_tokens"]).unwrap_or(0),
                opt_i64(&u["output_tokens"]).unwrap_or(0),
                opt_i64(&u["cache_read_input_tokens"]).unwrap_or(0),
                opt_i64(&u["cache_creation_input_tokens"]).unwrap_or(0),
            );
            let st = &u["server_tool_use"];
            let meta = json!({
                "ws": st["web_search_requests"], "wf": st["web_fetch_requests"],
                "tier": u["service_tier"],
            });
            let rc = c.execute(
                "INSERT OR IGNORE INTO usage_events
                 (app,event_key,ts,day,model,kind,session_id,
                  tok_in,tok_out,tok_cache_read,tok_cache_write,cost_usd,meta,device)
                 VALUES('claude',?,?,?,?,'gen',?,?,?,?,?,NULL,?,?)",
                params![
                    mid, ts, day_of(ts), model, format!("claude:{sid}"),
                    i_, o_, cr, cw, meta.to_string(), device()
                ],
            ).unwrap_or(0);
            n_ev += rc as i64;
            let s = sess.entry(sid.clone()).or_insert(json!({
                "model": "?", "n": 0, "t0": ts, "t1": ts,
                "cwd": e["cwd"], "ti": 0, "to": 0, "tcr": 0, "tcw": 0
            }));
            s["n"] = json!(opt_i64(&s["n"]).unwrap_or(0) + 1);
            if model != "?" { s["model"] = json!(model); }
            s["t0"] = json!(opt_i64(&s["t0"]).unwrap_or(ts).min(ts));
            s["t1"] = json!(opt_i64(&s["t1"]).unwrap_or(ts).max(ts));
            s["ti"] = json!(opt_i64(&s["ti"]).unwrap_or(0) + i_);
            s["to"] = json!(opt_i64(&s["to"]).unwrap_or(0) + o_);
            s["tcr"] = json!(opt_i64(&s["tcr"]).unwrap_or(0) + cr);
            s["tcw"] = json!(opt_i64(&s["tcw"]).unwrap_or(0) + cw);
        }
    }
    let now = now_s();
    let mut n_sess = 0i64;
    for (sid, s) in &sess {
        let cwd = vstr(s, "cwd").unwrap_or_default();
        let title = Path::new(&cwd)
            .file_name()
            .map(|x| x.to_string_lossy().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| sid.clone());
        let _ = c.execute(
            "INSERT INTO local_sessions
             (session_id,source,model,title,cwd,created_at,last_activity_at,
              n_user,n_assistant,n_tool,n_system,n_tool_calls,n_prompts,
              tok_in,tok_out,tok_cache_read,tok_cache_write,first_seen,last_seen)
             VALUES (?, 'claude', ?, ?, ?, ?, ?, 0, ?, 0,0,0,0, ?,?,?,?, ?,?)
             ON CONFLICT(session_id) DO UPDATE SET
              model=excluded.model, last_activity_at=excluded.last_activity_at,
              n_assistant=excluded.n_assistant, tok_in=excluded.tok_in,
              tok_out=excluded.tok_out, tok_cache_read=excluded.tok_cache_read,
              tok_cache_write=excluded.tok_cache_write, last_seen=excluded.last_seen",
            params![
                format!("claude:{sid}"), vstr(s, "model"), title, cwd,
                opt_i64(&s["t0"]).unwrap_or(0), opt_i64(&s["t1"]).unwrap_or(0),
                opt_i64(&s["n"]).unwrap_or(0),
                opt_i64(&s["ti"]).unwrap_or(0), opt_i64(&s["to"]).unwrap_or(0),
                opt_i64(&s["tcr"]).unwrap_or(0), opt_i64(&s["tcw"]).unwrap_or(0),
                now, now
            ],
        );
        n_sess += 1;
    }
    log_run(c, "claude", "ok", &format!("{n_sess} sessions, +{n_ev} events"));
    n_sess
}

// ---------------------------------------------------------------- 价格表

const BUILTIN_PRICES: &[(&str, f64, f64, f64, f64, &str)] = &[
    ("swe-2", 3.00, 15.00, 0.30, 3.75, "proxy:sonnet"),
    ("claude-opus", 15.00, 75.00, 1.50, 18.75, ""),
    ("claude-haiku", 0.80, 4.00, 0.08, 1.00, ""),
    ("claude", 3.00, 15.00, 0.30, 3.75, ""),
    ("gpt-6-astra", 1.25, 10.00, 0.125, 1.25, "proxy:gpt-frontier"),
    ("gpt", 1.25, 10.00, 0.125, 1.25, ""),
    ("gemini", 1.25, 10.00, 0.31, 1.25, ""),
    ("deepseek", 0.27, 1.10, 0.07, 0.27, ""),
    ("kimi", 0.60, 2.50, 0.15, 0.60, ""),
    ("grok", 3.00, 15.00, 0.30, 3.75, "proxy:frontier"),
    ("glm", 0.60, 2.20, 0.11, 0.55, "z.ai"),
    ("fable", 3.00, 15.00, 0.30, 3.75, "proxy:sonnet"),
    ("", 3.00, 15.00, 0.30, 3.75, "default:sonnet"),
];

fn sync_prices(c: &Connection) {
    let mut rules: Vec<(String, f64, f64, f64, f64, String)> = Vec::new();
    let pj = data_dir().join("prices.json");
    if let Ok(txt) = fs::read_to_string(&pj) {
        if let Ok(j) = serde_json::from_str::<Value>(&txt) {
            if let Some(arr) = j["rules"].as_array() {
                for r in arr {
                    if let Some(a) = r.as_array() {
                        if a.len() >= 5 {
                            rules.push((
                                vstr(&a[0], "").unwrap_or_default(),
                                opt_f64(&a[1]).unwrap_or(3.0),
                                opt_f64(&a[2]).unwrap_or(15.0),
                                opt_f64(&a[3]).unwrap_or(0.3),
                                opt_f64(&a[4]).unwrap_or(3.75),
                                a.get(5).and_then(|x| x.as_str()).unwrap_or("").into(),
                            ));
                        }
                    }
                }
            }
        }
    }
    for (p, i, o, cr, cw, n) in BUILTIN_PRICES {
        rules.push((p.to_string(), *i, *o, *cr, *cw, n.to_string()));
    }
    let _ = c.execute("DELETE FROM model_prices", []);
    for (i, r) in rules.iter().enumerate() {
        let _ = c.execute(
            "INSERT INTO model_prices VALUES (?,?,?,?,?,?,?)",
            params![i as i64, r.0, r.1, r.2, r.3, r.4, r.5],
        );
    }
}

// ---------------------------------------------------------------- sync

/// 各表增量导出的水位列/游标方式。wm=已确认送达的水位（None=从头）；
/// 返回 (rows, 新水位)。游标不落 kv——由调用方在确认送达后提交，
/// 避免"水位推进了但数据没送到"的丢行。
fn export_table_rows(
    c: &Connection,
    table: &str,
    wm: Option<i64>,
    exclude_dev: Option<&str>,
) -> (Vec<Value>, Option<i64>) {
    let dev_filter = exclude_dev.map(|d| format!("AND device != '{}'", d.replace('\'', "")));
    let mut rows = Vec::new();
    match table {
        // rowid 水位（insert-only 或 replace-bumps-rowid）
        "usage_events" | "app_quota" | "quota_snapshots" => {
            let cur = wm.unwrap_or(0);
            let cols = match table {
                "usage_events" => "rowid,app,event_key,ts,day,model,kind,session_id,tok_in,tok_out,tok_cache_read,tok_cache_write,cost_usd,meta,device",
                "app_quota" => "rowid,app,label,ts,pct_remaining,used,lim,resets_at,meta,device",
                _ => "rowid,ts,plan_name,teams_tier,weekly_quota_remaining_pct,overage_balance_micros,available_prompt_credits,plan_start,plan_end,daily_reset,weekly_reset,n_models,device",
            };
            let sql = format!(
                "SELECT {cols} FROM {table} WHERE rowid > ?1 {} ORDER BY rowid",
                dev_filter.clone().unwrap_or_default()
            );
            let mut maxrid = cur;
            if let Ok(mut st) = c.prepare(&sql) {
                let got = st.query_map(params![cur], |r| {
                    let mut o = serde_json::Map::new();
                    for (i, name) in cols.split(',').enumerate() {
                        o.insert(name.to_string(), row_json(r, i));
                    }
                    r.get::<_, i64>(0).map(|rid| { maxrid = maxrid.max(rid); o })
                        .map(|o| json!(o))
                });
                if let Ok(it) = got {
                    for r in it.flatten() {
                        rows.push(r);
                    }
                }
            }
            return (rows, Some(maxrid));
        }
        // last_seen 水位（upsert 原地更新）
        "local_sessions" | "cloud_sessions" => {
            let cur = wm.unwrap_or(0);
            let cols = match table {
                "local_sessions" => "session_id,source,model,agent_mode,backend_type,cwd,title,created_at,last_activity_at,credit_cost,acu_cost,n_user,n_assistant,n_tool,n_system,n_tool_calls,n_prompts,tok_in,tok_out,tok_cache_read,tok_cache_write,gen_ms,device,last_seen",
                _ => "session_id,title,status,status_detail,origin,category,user_id,created_at,updated_at,acus_consumed,n_prs,device,last_seen",
            };
            let sql = format!(
                "SELECT {cols} FROM {table} WHERE last_seen >= ?1 {}",
                dev_filter.clone().unwrap_or_default()
            );
            let mut maxls = cur;
            if let Ok(mut st) = c.prepare(&sql) {
                let got = st.query_map(params![cur], |r| {
                    let mut o = serde_json::Map::new();
                    for (i, name) in cols.split(',').enumerate() {
                        o.insert(name.to_string(), row_json(r, i));
                    }
                    if let Ok(ls) = r.get::<_, i64>(cols.split(',').count() - 1) {
                        maxls = maxls.max(ls);
                    }
                    Ok(json!(o))
                });
                if let Ok(it) = got {
                    for r in it.flatten() {
                        rows.push(r);
                    }
                }
            }
            return (rows, Some(maxls));
        }
        // 小表全量
        "daily_activity" | "model_multipliers" => {
            let cols = match table {
                "daily_activity" => "app,day,metric,value,device",
                _ => "model_uid,label,credit_multiplier,updated_at",
            };
            let sql = format!("SELECT {cols} FROM {table} WHERE 1 {}",
                              dev_filter.unwrap_or_default());
            if let Ok(mut st) = c.prepare(&sql) {
                let got = st.query_map([], |r| {
                    let mut o = serde_json::Map::new();
                    for (i, name) in cols.split(',').enumerate() {
                        o.insert(name.to_string(), row_json(r, i));
                    }
                    Ok(json!(o))
                });
                if let Ok(it) = got {
                    for r in it.flatten() { rows.push(r); }
                }
            }
        }
        _ => {}
    }
    (rows, None)
}

const SYNC_TABLES: &[&str] = &[
    "usage_events", "local_sessions", "cloud_sessions",
    "app_quota", "quota_snapshots", "daily_activity", "model_multipliers",
];

/// `export [--for <dev>] [--since t=v,...]`：NDJSON → stdout。
/// 每行 {"t":表名,"r":{...}}。--since 由拉取方传它已确认的水位；
/// 不传则从 kv exp:for:<dev>:<table> 取水位并在输出后提交。
pub fn export_cli(for_dev: Option<&str>, since: Option<&str>) {
    let Some(c) = open_db() else { return };
    tag_device(&c);
    let since_map: std::collections::HashMap<String, i64> = since
        .map(|s| {
            s.split(',')
                .filter_map(|p| {
                    let mut it = p.splitn(2, '=');
                    Some((it.next()?.to_string(), it.next()?.parse().ok()?))
                })
                .collect()
        })
        .unwrap_or_default();
    let out = std::io::stdout();
    let mut w = out.lock();
    for t in SYNC_TABLES {
        let key = format!("exp:for:{}:{t}", for_dev.unwrap_or("self"));
        let wm = since_map
            .get(*t)
            .copied()
            .or_else(|| kv_get(&c, &key).and_then(|s| s.parse().ok()));
        let (rows, new_wm) = export_table_rows(&c, t, wm, for_dev);
        for r in &rows {
            let line = json!({"t": t, "r": r});
            let _ = writeln!(w, "{line}");
        }
        // --since 模式下游标归拉方管；否则写 kv（已尽力送达）
        if since.is_none() {
            if let Some(m) = new_wm {
                if Some(m) > wm {
                    kv_set(&c, &key, &m.to_string());
                }
            }
        }
    }
    let _ = w.flush();
}

fn import_row(c: &Connection, t: &str, r: &serde_json::Map<String, Value>) {
    let g = |k: &str| j2s(r.get(k).unwrap_or(&Value::Null));
    match t {
        "usage_events" => {
            let _ = c.execute(
                "INSERT OR IGNORE INTO usage_events
                 (app,event_key,ts,day,model,kind,session_id,tok_in,tok_out,
                  tok_cache_read,tok_cache_write,cost_usd,meta,device)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                params![g("app"), g("event_key"), g("ts"), g("day"), g("model"),
                        g("kind"), g("session_id"), g("tok_in"), g("tok_out"),
                        g("tok_cache_read"), g("tok_cache_write"), g("cost_usd"),
                        g("meta"), g("device")],
            );
        }
        "app_quota" => {
            let _ = c.execute(
                "INSERT OR REPLACE INTO app_quota
                 (app,label,ts,pct_remaining,used,lim,resets_at,meta,device)
                 VALUES (?,?,?,?,?,?,?,?,?)",
                params![g("app"), g("label"), g("ts"), g("pct_remaining"),
                        g("used"), g("lim"), g("resets_at"), g("meta"), g("device")],
            );
        }
        "local_sessions" => {
            let _ = c.execute(
                "INSERT INTO local_sessions
                 (session_id,source,model,agent_mode,backend_type,cwd,title,
                  created_at,last_activity_at,credit_cost,acu_cost,
                  n_user,n_assistant,n_tool,n_system,n_tool_calls,n_prompts,
                  tok_in,tok_out,tok_cache_read,tok_cache_write,gen_ms,first_seen,last_seen,device)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?, ?,?)
                 ON CONFLICT(session_id) DO UPDATE SET
                  source=excluded.source,model=excluded.model,title=excluded.title,
                  cwd=excluded.cwd,last_activity_at=excluded.last_activity_at,
                  n_user=excluded.n_user,n_assistant=excluded.n_assistant,
                  n_tool=excluded.n_tool,n_system=excluded.n_system,
                  n_tool_calls=excluded.n_tool_calls,n_prompts=excluded.n_prompts,
                  tok_in=excluded.tok_in,tok_out=excluded.tok_out,
                  tok_cache_read=excluded.tok_cache_read,
                  tok_cache_write=excluded.tok_cache_write,
                  last_seen=excluded.last_seen,device=excluded.device",
                params![g("session_id"), g("source"), g("model"), g("agent_mode"),
                        g("backend_type"), g("cwd"), g("title"), g("created_at"),
                        g("last_activity_at"), g("credit_cost"), g("acu_cost"),
                        g("n_user"), g("n_assistant"), g("n_tool"), g("n_system"),
                        g("n_tool_calls"), g("n_prompts"), g("tok_in"), g("tok_out"),
                        g("tok_cache_read"), g("tok_cache_write"), g("gen_ms"),
                        now_s(), g("last_seen"), g("device")],
            );
        }
        "cloud_sessions" => {
            let _ = c.execute(
                "INSERT INTO cloud_sessions
                 (session_id,title,status,status_detail,origin,category,user_id,
                  created_at,updated_at,acus_consumed,n_prs,first_seen,last_seen,device)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)
                 ON CONFLICT(session_id) DO UPDATE SET
                  title=excluded.title,status=excluded.status,
                  status_detail=excluded.status_detail,updated_at=excluded.updated_at,
                  acus_consumed=excluded.acus_consumed,n_prs=excluded.n_prs,
                  last_seen=excluded.last_seen,device=excluded.device",
                params![g("session_id"), g("title"), g("status"), g("status_detail"),
                        g("origin"), g("category"), g("user_id"), g("created_at"),
                        g("updated_at"), g("acus_consumed"), g("n_prs"),
                        now_s(), g("last_seen"), g("device")],
            );
        }
        "quota_snapshots" => {
            let _ = c.execute(
                "INSERT OR REPLACE INTO quota_snapshots
                 (ts,plan_name,teams_tier,weekly_quota_remaining_pct,
                  overage_balance_micros,available_prompt_credits,plan_start,plan_end,
                  daily_reset,weekly_reset,n_models,device)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
                params![g("ts"), g("plan_name"), g("teams_tier"),
                        g("weekly_quota_remaining_pct"), g("overage_balance_micros"),
                        g("available_prompt_credits"), g("plan_start"), g("plan_end"),
                        g("daily_reset"), g("weekly_reset"), g("n_models"), g("device")],
            );
        }
        "daily_activity" => {
            let _ = c.execute(
                "INSERT OR REPLACE INTO daily_activity (app,day,metric,value,device) VALUES (?,?,?,?,?)",
                params![g("app"), g("day"), g("metric"), g("value"), g("device")],
            );
        }
        "model_multipliers" => {
            let _ = c.execute(
                "INSERT OR REPLACE INTO model_multipliers VALUES (?,?,?,?)",
                params![g("model_uid"), g("label"), g("credit_multiplier"), g("updated_at")],
            );
        }
        _ => {}
    }
}

/// `import`：stdin NDJSON → 本库（单事务，4 万行也秒级）。
pub fn import_cli() {
    let Some(mut c) = open_db() else { return };
    let stdin = std::io::stdin();
    let mut n = 0i64;
    let mut buf = String::new();
    if stdin.lock().read_to_string(&mut buf).is_err() {
        return;
    }
    let Ok(tx) = c.transaction() else { return };
    for line in buf.lines() {
        let Ok(e) = serde_json::from_str::<Value>(line) else { continue };
        let (Some(t), Some(r)) = (vstr(&e, "t"), e["r"].as_object()) else { continue };
        import_row(&tx, &t, r);
        n += 1;
    }
    let _ = tx.commit();
    eprintln!("import: {n} rows");
}

/// `sync`：与 kv sync.peer 配置的 ssh 对端双向同步。
/// 配置：kv sync.peer=vps（ssh别名），sync.remote=远程项目目录。
pub fn sync_cli() {
    let Some(mut c) = open_db() else { return };
    let Some(peer) = kv_get(&c, "sync.peer") else {
        eprintln!("sync: 未配置 sync.peer");
        return;
    };
    let remote = kv_get(&c, "sync.remote").unwrap_or_else(|| "~/devin-usage".into());
    // 远程优先用 Rust agent 二进制；不存在退回 python（过渡期）。
    // 注意必须 if/else 分组——`test && X || Y` 会把子命令参数绑错边。
    let remote_cmd = |sub: &str| {
        format!(
            "BIN={r}/devin-usage-tray/target/release/devin-usage-tray; \
             if [ -x \"$BIN\" ]; then \"$BIN\" {sub}; \
             else python3 {r}/devin_usage.py {sub}; fi",
            r = remote
        )
    };
    // 1) 推：本地增量 → 对端 import；成功后提交 exp:to:<peer>:<表> 游标
    let mut buf = Vec::new();
    let mut wms: Vec<(&str, i64, i64)> = Vec::new(); // (表, 旧水位, 新水位)
    {
        let mut w = std::io::BufWriter::new(&mut buf);
        for t in SYNC_TABLES {
            let wm = kv_get(&c, &format!("exp:to:{peer}:{t}"))
                .and_then(|s| s.parse().ok());
            let (rows, new_wm) = export_table_rows(&c, t, wm, None);
            for r in &rows {
                let line = json!({"t": t, "r": r}).to_string();
                let _ = w.write_all(line.as_bytes());
                let _ = w.write_all(b"\n");
            }
            if let (Some(old), Some(new)) = (wm, new_wm) {
                if new > old {
                    wms.push((t, old, new));
                }
            } else if let (None, Some(new)) = (wm, new_wm) {
                wms.push((t, 0, new));
            }
        }
        let _ = w.flush();
    }
    if !buf.is_empty() {
        if let Ok(mut ch) = Command::new("ssh")
            .args([&peer, &remote_cmd("import")])
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            {
                if let Some(mut si) = ch.stdin.take() {
                    let _ = si.write_all(&buf);
                }
            } // stdin drop → EOF → 对端 import 读完
            if ch.wait().map(|s| s.success()).unwrap_or(false) {
                for (t, _, new) in &wms {
                    kv_set(&c, &format!("exp:to:{peer}:{t}"), &new.to_string());
                }
                eprintln!("sync -> {peer}: {} rows", String::from_utf8_lossy(&buf).lines().count());
            }
        }
    }
    // 2) 拉：本地水位 have:<peer>:<表> 发给对端 → 导入后提交
    let me = device();
    let since_arg = SYNC_TABLES
        .iter()
        .filter_map(|t| {
            kv_get(&c, &format!("have:{peer}:{t}"))
                .and_then(|s| s.parse::<i64>().ok())
                .map(|v| format!("{t}={v}"))
        })
        .collect::<Vec<_>>()
        .join(",");
    let sub = if since_arg.is_empty() {
        format!("export --for {me}")
    } else {
        format!("export --for {me} --since {since_arg}")
    };
    if let Ok(o) = Command::new("ssh")
        .args([&peer, &remote_cmd(&sub)])
        .output()
    {
        let txt = String::from_utf8_lossy(&o.stdout);
        let mut n = 0;
        let mut max_wm: std::collections::HashMap<String, i64> = Default::default();
        {
            let Ok(tx) = c.transaction() else { return };
            for line in txt.lines() {
                if let Ok(e) = serde_json::from_str::<Value>(line) {
                    if let (Some(t), Some(r)) = (vstr(&e, "t"), e["r"].as_object()) {
                        import_row(&tx, &t, r);
                        // 行里的水位字段：rowid 表取 rowid，会话表取 last_seen
                        let wm_v = r.get("rowid").or_else(|| r.get("last_seen"))
                            .and_then(opt_i64);
                        if let Some(v) = wm_v {
                            let e = max_wm.entry(t).or_insert(0);
                            *e = (*e).max(v);
                        }
                        n += 1;
                    }
                }
            }
            let _ = tx.commit();
        }
        for (t, m) in max_wm {
            let cur: i64 = kv_get(&c, &format!("have:{peer}:{t}"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if m > cur {
                kv_set(&c, &format!("have:{peer}:{t}"), &m.to_string());
            }
        }
        if n > 0 {
            eprintln!("sync <- {peer}: {n} rows");
        }
    }
}

// ---------------------------------------------------------------- 入口

pub fn collect_cli(only: Option<&str>) {
    let Some(c) = open_db() else {
        eprintln!("db 打不开");
        return;
    };
    let token = read_token();
    if token.is_none() {
        log_run(&c, "auth", "error", "找不到 token");
    }
    let org = token.as_deref().and_then(|t| read_org(Some(t)));
    let t0 = std::time::Instant::now();
    let mut results: Vec<(&str, String)> = Vec::new();
    let want = |name: &str| only.map_or(true, |o| o == name);
    let run = |name: &'static str, f: fn(&Connection) -> i64, results: &mut Vec<(&'static str, String)>| {
        if !want(name) {
            return;
        }
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&c))) {
            Ok(n) => results.push((name, n.to_string())),
            Err(_) => {
                log_run(&c, name, "error", "panic");
                results.push((name, "PANIC".into()));
            }
        }
    };
    if want("quota") || want("cloud") {
        if let Some(tok) = &token {
            if want("quota") {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    collect_quota(&c, tok)
                        .map(|_| "ok".to_string())
                        .unwrap_or_else(|| "FAIL".into())
                })) {
                    Ok(s) => results.push(("quota", s)),
                    Err(_) => results.push(("quota", "PANIC".into())),
                }
            }
            if want("cloud") {
                let n = collect_cloud(&c, tok, org.as_deref());
                results.push(("cloud", n.to_string()));
            }
        } else {
            results.push(("quota", "SKIP:无token".into()));
            results.push(("cloud", "SKIP:无token".into()));
        }
    }
    for (name, f) in [
        ("local", collect_local as fn(&Connection) -> i64),
        ("cursor", collect_cursor),
        ("antigravity", collect_antigravity),
        ("zcode", collect_zcode),
        ("grok", collect_grok),
        ("claude", collect_claude),
    ] {
        run(name, f, &mut results);
    }
    tag_device(&c);
    let _ = c.execute_batch("PRAGMA optimize");
    let _ = c.execute("COMMIT", []);
    println!(
        "collect done in {:.1}s: {:?}",
        t0.elapsed().as_secs_f64(),
        results.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>()
    );
    // 配了 sync.peer 就自动同步
    if kv_get(&c, "sync.peer").is_some() {
        sync_cli();
    }
}
