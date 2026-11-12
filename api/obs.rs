//! OBS 对象存储：`POST /api/obs/temporary/url` 换签名 URL → `PUT` 上传。
//!
//! 跑步轨迹明细（`run_data/{YYYYMMDDHH}/{uuid}.json`）不是塞在提交报文里，
//! 而是先传到对象存储，提交时只给 objectKey。
//!
//! 两个坑：
//! - 签名响应里的 `signedUrl` 可能在 `data` 里，也可能 `data` 是字符串化的 JSON
//! - 换签名的 `method` 是 `"Put"`（首字母大写），不是 `"PUT"`

use serde_json::{json, Value};

use super::client::{expect_ok, Client};
use crate::crypto::consts::hosts;

/// OBS 签名接口路径。
pub const OBS_SIGN_PATH: &str = "/api/obs/temporary/url";

/// 桶名（服务端写死的）。
pub const BUCKET: &str = "iydsj-hbase-hot";

/// 换签名 URL。
pub fn sign_url(client: &Client, method: &str, key: &str) -> Result<String, String> {
    let body = json!({
        "bucketName": BUCKET,
        "objectKey": key,
        "method": method,
        "contentType": "application/json",
    });
    let raw = client.request(
        "POST",
        &format!("{}{}", hosts::RUN, OBS_SIGN_PATH),
        Some(&body),
    )?;
    let v = expect_ok(&raw)?;
    let data = v.get("data").unwrap_or(&v);

    // data 可能是对象，也可能是字符串化的 JSON
    let signed = if let Some(s) = data.get("signedUrl").and_then(|x| x.as_str()) {
        s.to_string()
    } else if let Some(s) = data.as_str() {
        let parsed: Value = serde_json::from_str(s).map_err(|e| format!("签名响应解析失败: {e}"))?;
        parsed
            .get("signedUrl")
            .and_then(|x| x.as_str())
            .ok_or("OBS 签名响应缺 signedUrl")?
            .to_string()
    } else if let Some(s) = v.get("signedUrl").and_then(|x| x.as_str()) {
        s.to_string()
    } else {
        return Err("OBS 签名响应缺 signedUrl".into());
    };
    Ok(signed)
}

/// PUT 上传。
pub fn put_object(signed_url: &str, payload: &[u8]) -> Result<(), String> {
    // ureq/rustls 对腾讯 COS 端点握手偶发 "tls connection init failed:
    // unexpected end of file"（curl 直连正常，说明是客户端侧的间歇性握手失败）。
    // 每次新建 agent 重试几次，避免一次抖动就让明细整条丢失、详情页没数据。
    let mut last = String::new();
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(300 * attempt as u64));
        }
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(30))
            .build();
        match agent
            .put(signed_url)
            .set("Content-Type", "application/json")
            .send_bytes(payload)
        {
            Ok(_) => return Ok(()),
            // 服务端明确拒绝（4xx/5xx）重试也没用，直接返回
            Err(e @ ureq::Error::Status(..)) => return Err(format!("OBS PUT 失败: {e}")),
            Err(e) => last = format!("OBS PUT 失败: {e}"),
        }
    }
    Err(last)
}

/// 上传到多个 objectKey，返回成功个数。
///
/// 服务端认的 key 规则有两种说法，实测都存在：
/// - `run_data/{YYYYMMDDHH}/{uuid}.json`（详情页展示用）
/// - `runData/{uid}/{sportType}/{startTime}_{uuid}.txt`（兜底）
///
/// 两个都传一遍最稳。
pub fn upload_keys(
    client: &Client,
    keys: &[String],
    payload: &[u8],
) -> (usize, Vec<String>) {
    let mut ok = 0;
    let mut errs = Vec::new();
    for key in keys {
        match sign_url(client, "Put", key).and_then(|url| put_object(&url, payload)) {
            Ok(()) => ok += 1,
            Err(e) => errs.push(format!("{key}: {e}")),
        }
    }
    (ok, errs)
}

/// 回读校验。
pub fn fetch_object(client: &Client, key: &str) -> Result<Value, String> {
    let signed = sign_url(client, "Get", key)?;
    // 同 put_object：ureq/rustls 对 COS 握手偶发失败，重试几次。
    let mut last = String::new();
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(300 * attempt as u64));
        }
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(30))
            .build();
        match agent.get(&signed).call() {
            Ok(resp) => {
                let text = resp
                    .into_string()
                    .map_err(|e| format!("OBS 读取失败: {e}"))?;
                return serde_json::from_str(&text)
                    .map_err(|e| format!("OBS 对象解析失败: {e}"));
            }
            // 404 等是"对象不存在"，不是握手抖动，不重试
            Err(e @ ureq::Error::Status(..)) => return Err(format!("OBS GET 失败: {e}")),
            Err(e) => last = format!("OBS GET 失败: {e}"),
        }
    }
    Err(last)
}

/// 服务端所在时区偏移（秒）。
///
/// objectKey 里的小时段是**本地时区**，不是 UTC。真机用
/// `time.strftime("%Y%m%d%H", time.localtime(...))`，即按设备本地时间。
/// 中国校区是 UTC+8，用 UTC 拼 key 会整整早 8 小时，服务端按本地时区
/// 去 OBS 取对象时直接取空——轨迹「不存在」，依赖轨迹的所有判定项
/// （比如按顺序通过点位）自然全假，而提交本身返回成功、里程时长步频
/// 配速四项全过，很容易把排查带到别处。
///
/// 跨时区部署时用 `PACE_TZ_OFFSET_SEC` 覆盖，别硬编码。
fn tz_offset_sec() -> i64 {
    std::env::var("PACE_TZ_OFFSET_SEC")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| (-72_000..=72_000).contains(v))
        .unwrap_or(8 * 3600)
}

/// 生成 objectKey（详情页格式，本地时区）。
pub fn detail_key(now_ms: i64, uuid: &str) -> String {
    key_at_offset(now_ms, uuid, tz_offset_sec())
}

/// 生成 objectKey，UTC 版本。
///
/// 服务端拼 key 时用哪个时区没有确定证据——它可能是 `ZoneId.systemDefault()`
/// （服务器在中国就是 UTC+8），也可能是 `ZoneOffset.UTC`。猜错的代价是
/// 服务端按自己的时区去找对象时取空，轨迹读不到，记录里只剩里程/时长/
/// 配速这些标量，一眼假。
///
/// 既然一个对象才几 KB，两个时区各传一份，不管服务端按哪个算都能命中。
/// 这比继续猜便宜得多。
pub fn detail_key_utc(now_ms: i64, uuid: &str) -> String {
    key_at_offset(now_ms, uuid, 0)
}

fn key_at_offset(now_ms: i64, uuid: &str, offset_sec: i64) -> String {
    let secs = now_ms / 1000 + offset_sec;
    let days = secs.div_euclid(86_400);
    let (y, m, d, h) = civil_from_days(days, (secs.rem_euclid(86_400)) / 3600);
    format!("run_data/{y:04}{m:02}{d:02}{h:02}/{uuid}.json")
}

/// 生成 objectKey（兜底格式）。
pub fn fallback_key(uid: &str, sport_type: i32, start_sec: i64, uuid: &str) -> String {
    format!("runData/{uid}/{sport_type}/{start_sec}_{uuid}.txt")
}

/// 由 Unix 天数换算年月日时（Howard Hinnant 算法）。
fn civil_from_days(days: i64, hour: i64) -> (i64, u32, u32, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d, hour)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_key_has_expected_shape() {
        // 2024-11-06 14:00 UTC
        let ms = 1_730_901_600_000i64;
        let k = detail_key(ms, "abc-123");
        assert!(k.starts_with("run_data/"));
        assert!(k.ends_with("/abc-123.json"));
        let stamp = &k["run_data/".len().."run_data/".len() + 10];
        assert_eq!(stamp.len(), 10, "应为 YYYYMMDDHH 十位: {stamp}");
        assert!(stamp.starts_with("2024"), "年份错误: {stamp}");
    }

    #[test]
    fn fallback_key_has_expected_shape() {
        let k = fallback_key("12345", 1, 1_730_000_000, "uuid-1");
        assert_eq!(k, "runData/12345/1/1730000000_uuid-1.txt");
    }

    #[test]
    fn civil_conversion_known_dates() {
        // 1970-01-01
        let (y, m, d, _) = civil_from_days(0, 0);
        assert_eq!((y, m, d), (1970, 1, 1));
        // 2024-02-29（闰日）
        let days = 19_782; // 2024-02-29 距 1970-01-01 的天数
        let (y, m, d, _) = civil_from_days(days, 0);
        assert_eq!((y, m, d), (2024, 2, 29), "闰日换算错误");
    }

    #[test]
    fn key_formats_are_distinct() {
        let ms = 1_730_901_600_000i64;
        let a = detail_key(ms, "u");
        let b = fallback_key("1", 1, ms / 1000, "u");
        assert_ne!(a, b);
        assert!(a.starts_with("run_data/"));
        assert!(b.starts_with("runData/"));
    }
}
