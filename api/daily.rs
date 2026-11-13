//! 无界面的每日批量：给 GitHub Actions 之类的定时任务用。
//!
//! 每个账号：登录（自动过极验）→ 今天已有记录就跳过 → 随机距离/时长真跑
//! （或只预演）→ 退出登录。账号之间随机间隔，顺序每天打乱。
//!
//! 配置走环境变量 `PACE_ACCOUNTS`（JSON），不落盘：
//!
//! ```json
//! {
//!   "home": [28.3443, 112.8256],
//!   "accounts": [
//!     {"username": "手机号", "password": "密码", "device_id": "32位hex", "home": [lat, lng]}
//!   ]
//! }
//! ```
//!
//! - `device_id`：该账号在本机用的设备 ID（`pace-forge export-ci` 导出）。CI 每次
//!   都是新机器，不给就每天随机一个，等于每天换手机登录。
//! - `home`：校区中心（BD-09）。没配围栏的学校用它当定位锚点，不给就退回默认值。
//!
//! 结果 JSON 只含序号、状态、里程、时长、步数、错误码——不含手机号、密码、
//! 姓名、token。

use serde::Deserialize;
use serde_json::{json, Value};

use super::client::{Client, DeviceProfile};
use super::flow::{self, RunParams};
use super::login::{self, LoginOutcome};

#[derive(Debug, Deserialize)]
pub struct DailyConfig {
    #[serde(default)]
    pub home: Option<[f64; 2]>,
    pub accounts: Vec<DailyAccount>,
}

#[derive(Debug, Deserialize)]
pub struct DailyAccount {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub home: Option<[f64; 2]>,
}

#[derive(Debug, Clone)]
pub struct DailyOptions {
    /// 只预演，不提交。
    pub dry: bool,
    /// 开跑前随机等待的上限（秒），让每天开始时间不一样。
    pub start_delay_max_s: u64,
    /// 账号之间随机间隔（秒），避免同一 IP 同一时刻连交。
    pub gap_s: (u64, u64),
    /// 只跑这些序号（1 起），空 = 全部。
    pub only: Vec<usize>,
}

impl Default for DailyOptions {
    fn default() -> Self {
        Self { dry: false, start_delay_max_s: 0, gap_s: (60, 240), only: Vec::new() }
    }
}

fn now_ms() -> i64 {
    crate::crypto::envelope::now_ms() as i64
}

fn iso(ms: i64) -> String {
    let secs = ms / 1000;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// 北京时间的"天"编号。
fn cn_day(ms: i64) -> i64 {
    (ms / 1000 + 8 * 3600).div_euclid(86_400)
}

fn rand_u64() -> u64 {
    use rand::RngCore;
    rand::thread_rng().next_u64()
}

fn rand_between(lo: u64, hi: u64) -> u64 {
    if hi <= lo {
        return lo;
    }
    lo + rand_u64() % (hi - lo + 1)
}

/// 今天（北京时间）是否已有一条像样的记分跑记录。
fn ran_today(client: &Client) -> Result<bool, String> {
    let v = super::records::list_records(client, 1, 5, 1)?;
    let today = cn_day(now_ms());
    Ok(v["records"].as_array().is_some_and(|rs| {
        rs.iter().any(|r| {
            let st = r["startTime"].as_i64().unwrap_or(0);
            let dist = r["totalDistance"].as_f64().unwrap_or(0.0);
            st > 0 && cn_day(st) == today && dist >= 1000.0
        })
    }))
}

/// 一个账号的结果（只含可公开字段）。
fn result(index: usize, status: &str, code: Option<&str>, extra: Value) -> Value {
    let mut v = json!({"index": index, "status": status});
    if let Some(c) = code {
        v["error_code"] = json!(c);
    }
    if let Value::Object(m) = extra {
        for (k, x) in m {
            v[k] = x;
        }
    }
    v
}

fn run_one(index: usize, acc: &DailyAccount, home: (f64, f64), opt: &DailyOptions) -> Value {
    let tag = format!("[{index}]");
    let device = match acc.device_id.as_deref().filter(|d| d.len() == 32) {
        Some(id) => DeviceProfile::with_device_id(id),
        None => {
            println!("{tag} ⚠ 没配 device_id，本次用随机设备");
            DeviceProfile::for_account(&acc.username)
        }
    };
    let mut client = Client::new(device);

    // ① 登录
    match login::login(&mut client, &acc.username, &acc.password) {
        Ok(LoginOutcome::Ok(_)) => println!("{tag} √ 登录成功"),
        Ok(LoginOutcome::NeedCaptcha { message, .. }) => {
            println!("{tag} × 登录未过验证：{message}");
            return result(index, "failure", Some("login_failed"), json!({}));
        }
        Err(e) => {
            println!("{tag} × 登录失败：{e}");
            let code = if e.contains("10121") || e.contains("其他设备") { "login_locked" } else { "login_failed" };
            return result(index, "failure", Some(code), json!({}));
        }
    }

    // ② 今天跑过就不再跑
    let out = match ran_today(&client) {
        Ok(true) => {
            println!("{tag} · 今天已有记录，跳过");
            result(index, "skipped", Some("already_done"), json!({}))
        }
        r => {
            if let Err(e) = r {
                println!("{tag} ⚠ 查今日记录失败（{e}），照常跑");
            }
            run_submit(index, &client, home, opt, &tag)
        }
    };

    // ③ 退出登录：服务端会话留着，第二天再登会 10121 锁一小时
    match login::logout(&mut client) {
        Ok(true) => println!("{tag} √ 已退出登录"),
        Ok(false) => {}
        Err(e) => println!("{tag} ⚠ 退出登录失败：{e}"),
    }
    out
}

fn run_submit(index: usize, client: &Client, home: (f64, f64), opt: &DailyOptions, tag: &str) -> Value {
    let seed = rand_u64();
    let (km, min) = flow::random_run_spec(seed);
    let dur_s = (min * 60.0).round() as i64;
    println!("{tag} 计划 {km:.2}km / {}:{:02}{}", dur_s / 60, dur_s % 60, if opt.dry { "（预演）" } else { "" });
    let p = RunParams {
        center_lat: home.0,
        center_lng: home.1,
        distance_m: km * 1000.0,
        duration_sec: dur_s,
        sport_type: 1,
        run_mode: 1,
        seed,
        ..Default::default()
    };
    let tag_owned = tag.to_string();
    let mut log = move |s: &str| {
        // 只打关键步骤，不打报文
        if s.contains('√') || s.contains('×') || s.contains('⚠') || s.contains("致命") {
            println!("{tag_owned}   {}", s.trim());
        }
    };
    let r = if opt.dry {
        flow::dry_run(client, &p, &mut log)
    } else {
        let mut yes = |_o: &flow::RunOutcome| true;
        flow::run_full(client, &p, &mut log, &mut yes)
    };
    match r {
        Ok(o) => {
            let status = if opt.dry || o.submitted { "success" } else { "failure" };
            println!(
                "{tag} {} {}m / {}:{:02} / {} 步",
                if status == "success" { "√" } else { "×" },
                o.real_length_m,
                dur_s / 60,
                dur_s % 60,
                o.total_steps
            );
            result(
                index,
                status,
                (status != "success").then_some("run_failed"),
                json!({
                    "dry": opt.dry,
                    "distance_m": o.real_length_m,
                    "duration_s": dur_s,
                    "steps": o.total_steps,
                }),
            )
        }
        Err(e) => {
            println!("{tag} × 失败：{e}");
            let code = if e.contains("自检") { "preflight_failed" } else if e.contains("超时") || e.contains("Connection") { "network_failure" } else { "run_failed" };
            result(index, "failure", Some(code), json!({"dry": opt.dry}))
        }
    }
}

/// 跑完全部账号，返回结果 JSON。账号顺序每天打乱、之间随机间隔。
pub fn run_daily(cfg: &DailyConfig, opt: &DailyOptions) -> Value {
    let started = now_ms();
    if opt.start_delay_max_s > 0 {
        let wait = rand_between(0, opt.start_delay_max_s);
        println!("随机等待 {}:{:02} 再开始", wait / 60, wait % 60);
        std::thread::sleep(std::time::Duration::from_secs(wait));
    }

    let mut order: Vec<usize> = (0..cfg.accounts.len())
        .filter(|i| opt.only.is_empty() || opt.only.contains(&(i + 1)))
        .collect();
    {
        use rand::seq::SliceRandom;
        order.shuffle(&mut rand::thread_rng());
    }

    let fallback = cfg.home.map(|h| (h[0], h[1])).unwrap_or((30.5, 104.0));
    let mut results = Vec::new();
    for (k, &i) in order.iter().enumerate() {
        if k > 0 {
            let gap = rand_between(opt.gap_s.0, opt.gap_s.1);
            println!("间隔 {}:{:02}", gap / 60, gap % 60);
            std::thread::sleep(std::time::Duration::from_secs(gap));
        }
        let acc = &cfg.accounts[i];
        let home = acc.home.map(|h| (h[0], h[1])).unwrap_or(fallback);
        results.push(run_one(i + 1, acc, home, opt));
    }
    results.sort_by_key(|r| r["index"].as_u64().unwrap_or(0));

    json!({
        "started_at": iso(started),
        "finished_at": iso(now_ms()),
        "dry": opt.dry,
        "accounts": results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_formats_utc() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_794_481_765_000), "2026-11-12T11:09:25Z");
    }

    #[test]
    fn cn_day_rolls_at_utc16() {
        // 北京时间 2026-11-13 00:00 = UTC 11-12 16:00
        let before = 1_794_499_199_000; // UTC 11-12 15:59:59
        let after = 1_794_499_200_000; // UTC 11-12 16:00:00
        assert_eq!(cn_day(after) - cn_day(before), 1);
    }

    #[test]
    fn config_parses_minimal() {
        let c: DailyConfig =
            serde_json::from_str(r#"{"accounts":[{"username":"a","password":"b"}]}"#).unwrap();
        assert_eq!(c.accounts.len(), 1);
        assert!(c.home.is_none() && c.accounts[0].device_id.is_none());
    }

    #[test]
    fn result_has_no_credentials() {
        let v = result(2, "failure", Some("login_failed"), json!({"steps": 1}));
        let s = v.to_string();
        assert!(s.contains("\"index\":2") && s.contains("login_failed"));
        assert!(!s.contains("username") && !s.contains("password"));
    }
}
