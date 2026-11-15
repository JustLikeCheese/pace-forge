//! 某个账号本学期的跑步记录 + 每条的轨迹（给 autoSign 面板「历史」用）。
//!
//! 和 Pace Forge 自带页面同一套取法：记录列表走 runnings/records；
//! 详情接口不带坐标，轨迹从 OBS 拿（run_data 是 base64 的 gzip，里面 allLocJson
//! 的 gLat/gLng 是 GCJ-02，和高德底图同系）。
//! 只输出白名单字段，不带手机号、密码、设备 ID。

use std::io::Read;

use base64::Engine;
use serde_json::{json, Value};

use super::client::Client;
use super::daily::{login_account, logout_account, DailyConfig};
use super::{obs, records};

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// 开始时间统一成毫秒；服务端有时给秒。
fn start_ms(r: &Value) -> Option<i64> {
    let t = num(r.get("startTime").or_else(|| r.get("start_time")))?;
    Some(if t < 1e12 { (t * 1000.0) as i64 } else { t as i64 })
}

/// 候选 objectKey：真机记录是 run_data/{本地小时}/{uuid}.json，工具提交的三种都传了。
fn obs_keys(rrid: i64, uuid: Option<&str>, start: Option<i64>) -> Vec<String> {
    let mut keys = Vec::new();
    if let (Some(u), Some(t)) = (uuid.filter(|u| !u.is_empty()), start) {
        let mut seen = std::collections::BTreeSet::new();
        for v in [u.to_string(), u.to_lowercase(), u.to_uppercase()] {
            if seen.insert(v.clone()) {
                keys.push(obs::detail_key(t, &v));
            }
        }
        keys.push(obs::detail_key_utc(t, u));
    }
    if rrid > 0 {
        keys.push(format!("run_data/{}/{}.json", rrid / 1_000_000, rrid));
    }
    keys
}

/// 从 OBS 对象里取出轨迹点 [[lat, lng], ...]。
fn parse_track(obj: &Value) -> Option<Vec<[f64; 2]>> {
    let b64 = obj.get("run_data")?.as_str()?;
    let gz = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()?;
    let mut text = String::new();
    flate2::read::GzDecoder::new(&gz[..]).read_to_string(&mut text).ok()?;
    let wrap: Value = serde_json::from_str(&text).ok()?;
    let raw = wrap.get("allLocJson")?;
    let arr: Value = match raw {
        Value::String(s) => serde_json::from_str(s).ok()?,
        v => v.clone(),
    };
    let pts: Vec<[f64; 2]> = arr
        .as_array()?
        .iter()
        .filter_map(|p| {
            let lat = num(p.get("gLat")).filter(|v| *v > 0.0).or_else(|| num(p.get("lat")))?;
            let lng = num(p.get("gLng")).filter(|v| *v > 0.0).or_else(|| num(p.get("lng")))?;
            (lat > 0.0 && lng > 0.0 && lat.abs() <= 90.0 && lng.abs() <= 180.0).then_some([lat, lng])
        })
        .collect();
    (pts.len() > 1).then_some(pts)
}

fn thin(pts: &[[f64; 2]], n: usize) -> Value {
    let step = (pts.len() as f64 / n as f64).max(1.0);
    let mut out = Vec::new();
    let mut i = 0.0;
    while (i as usize) < pts.len() {
        let p = pts[i as usize];
        out.push(json!([(p[0] * 1e6).round() / 1e6, (p[1] * 1e6).round() / 1e6]));
        i += step;
    }
    let l = pts[pts.len() - 1];
    out.push(json!([(l[0] * 1e6).round() / 1e6, (l[1] * 1e6).round() / 1e6]));
    Value::Array(out)
}

fn track_of(client: &Client, rrid: i64, uuid: Option<&str>, start: Option<i64>) -> Option<Value> {
    for key in obs_keys(rrid, uuid, start) {
        if let Ok(obj) = obs::fetch_object(client, &key) {
            if let Some(pts) = parse_track(&obj) {
                return Some(thin(&pts, 200));
            }
        }
    }
    None
}

/// 一条记录 → 白名单字段。
fn clean(client: &Client, r: &Value) -> Value {
    let rrid = num(r.get("rrid").or_else(|| r.get("id")).or_else(|| r.get("recordId"))).unwrap_or(0.0) as i64;
    let start = start_ms(r);
    let uuid = r.get("uuid").and_then(|v| v.as_str());
    let mut o = json!({
        "rrid": rrid,
        "start_ms": start,
        "distance_m": num(r.get("totalDistance")).map(|v| v.round() as i64),
        "valid_distance_m": num(r.get("validDistance")).map(|v| v.round() as i64),
        "seconds": num(r.get("totalTime")).map(|v| v.round() as i64),
        "valid": r.get("completed").and_then(|v| v.as_bool()).unwrap_or(false),
        "cadence": num(r.get("avgStepFreq")).map(|v| v.round() as i64),
    });
    if let Some(s) = r.get("statusInfo").and_then(|v| v.as_str()) {
        o["status_info"] = json!(s.chars().take(60).collect::<String>());
    }
    if let Some(t) = track_of(client, rrid, uuid, start) {
        o["track"] = t;
    }
    o
}

/// 用已登录的会话拉最近 size 条记录（含轨迹），写进 out 的 records/total；失败写 error_code。
/// daily 跑完顺手调它，省一次登录。
pub fn fill(client: &Client, out: &mut Value, size: u32, tag: &str) {
    match records::list_records(client, 1, size.clamp(1, 50), 1) {
        Ok(list) => {
            let rows = list["records"].as_array().cloned().unwrap_or_default();
            out["records"] = Value::Array(rows.iter().map(|r| clean(client, r)).collect());
            out["total"] = list["total"].clone();
            println!("{tag} √ 拉到 {} 条历史记录", rows.len());
        }
        Err(e) => {
            println!("{tag} ⚠ 拉历史记录失败：{e}");
            out["error_code"] = json!(if e.contains("超时") || e.contains("Connection") { "network_failure" } else { "run_failed" });
        }
    }
}

/// 登录第 index 个账号（1 起），拉最近 size 条记录及轨迹，退出登录。
pub fn account_history(cfg: &DailyConfig, index: usize, size: u32) -> Value {
    let Some(acc) = index.checked_sub(1).and_then(|i| cfg.accounts.get(i)) else {
        return json!({"index": index, "error_code": "configuration_failed", "records": []});
    };
    let tag = format!("[{index}]");
    let mut out = json!({"index": index, "records": []});
    if let Some(n) = acc.name.as_deref().filter(|n| !n.is_empty()) {
        out["name"] = json!(n);
    }
    let mut client = match login_account(acc, &tag) {
        Ok(c) => c,
        Err(code) => {
            out["error_code"] = json!(code);
            return out;
        }
    };
    fill(&client, &mut out, size, &tag);
    logout_account(&mut client, &tag);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn start_ms_accepts_seconds_and_ms() {
        assert_eq!(start_ms(&json!({"startTime": 1_794_700_000})), Some(1_794_700_000_000));
        assert_eq!(start_ms(&json!({"startTime": 1_794_700_000_123i64})), Some(1_794_700_000_123));
        assert_eq!(start_ms(&json!({})), None);
    }

    #[test]
    fn parses_gzipped_track() {
        let inner = json!({"allLocJson": "[{\"gLat\":30.1,\"gLng\":104.2},{\"gLat\":30.2,\"gLng\":104.3},{\"lat\":0,\"lng\":0}]"});
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(inner.to_string().as_bytes()).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(enc.finish().unwrap());
        let pts = parse_track(&json!({"run_data": b64})).unwrap();
        assert_eq!(pts, vec![[30.1, 104.2], [30.2, 104.3]]);
    }

    #[test]
    fn keys_include_rrid_fallback() {
        let k = obs_keys(1_365_281_262, Some("AbC"), Some(1_794_700_000_000));
        assert!(k.iter().any(|x| x == "run_data/1365/1365281262.json"));
        assert!(k.len() >= 4);
    }
}
