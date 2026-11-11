//! 提交报文构建。
//!
//! 两条约束最容易踩：
//! 1. **签名参与字段 ≠ 报文字段**。签名字段 31 个，报文字段 45 个，
//!    差集是 `allLocJson` / `fivePointJson` / `signature` 等大字段（不进签名，
//!    否则报文太大且服务端也是这么算的）。
//! 2. **`speed` 是配速（分/公里）× 1000**，不是速度 m/s × 1000。
//!    判据是规则字段 `speedTop/speedBottom` 的单位为分/公里。
//!
//! 报文里 `allLocJson` / `fivePointJson` / `speedPerTenSec` 等是**字符串化的 JSON**
//! （双重序列化），不是嵌套对象。服务端按字符串取出来再解析一层。

use serde_json::{json, Value};

use super::client::Client;

use crate::crypto::sign::{sport_signature, SignFields};
use crate::track::TrackPoint;

/// 默认体重（kg）。
///
/// 官方卡路里不是「每公里固定千卡」，而是 MET × 体重 × 时长：
/// 配速越慢，单位时间消耗越低，但单位距离消耗反而更高。用固定 62 kcal/km
/// 会让「慢跑 3 公里」和「快跑 3 公里」报出同一个卡路里，与同时上报的
/// 配速、步频自相矛盾（Neko/LightWhite520 的 `track/calorie.rs` 用的是
/// 官方分档公式，pace 这里对齐）。
pub const DEFAULT_WEIGHT_KG: f64 = 68.0;
/// 最长允许跑步时长（秒）。
pub const MAX_RUN_TIME: i64 = 7200;
/// 最少步数。
pub const MIN_STEPS: i32 = 2000;

/// 构建参数。
#[derive(Debug, Clone)]
pub struct SubmitParams {
    pub uid: String,
    pub unid: String,
    pub uuid: String,
    pub sport_type: i32,
    pub total_distance_m: i64,
    pub total_time_sec: i64,
    pub total_steps: i32,
    pub start_time_ms: i64,
    pub stop_time_ms: i64,
    /// 学期目标距离（米），决定 `complete`。
    pub sel_distance_m: i64,
    /// 学期目标时长（秒）。
    pub sel_run_time_sec: i64,
    /// 规则串，服务端原样回显。
    pub policy: String,
    pub run_area_id: i64,
    pub geo_fences_json: String,
    pub total_ascent: i32,
}

/// 打卡点。
///
/// 字段对齐服务端 `points` 接口返回的原始结构，不是自己拼的三个字段。
/// 点位响应是 `{lon, lat, isFixed, pointName, glon, glat}`，服务端要靠
/// 这套字段把轨迹和它自己发的打卡点对上号；只回传 lat/lon/position 时
/// 它认不出是同一个点，`按顺序通过所有点位` 就会判 False（实测 6 条全中）。
#[derive(Debug, Clone)]
pub struct FixedPoint {
    pub lat: f64,
    pub lon: f64,
    pub position: i32,
    /// 服务端原始字段，原样回显。
    pub is_fixed: i64,
    pub point_name: String,
    pub glat: f64,
    pub glon: f64,
}

/// 构建结果。
#[derive(Debug, Clone)]
pub struct Submission {
    /// `save/record` 的 body。
    pub save_record: Value,
    /// OBS 直传的对象 key。
    pub object_key: String,
    /// 供排查用的签名原文。
    pub canonical: String,
}

/// 两点间方位角（度，0=正北，顺时针），输入 (lat, lon)。
fn bearing_deg(from: (f64, f64), to: (f64, f64)) -> f64 {
    let (lat1, lat2) = (from.0.to_radians(), to.0.to_radians());
    let dlon = (to.1 - from.1).to_radians();
    let y = dlon.sin() * lat2.cos();
    let x = lat1.cos() * lat2.sin() - lat1.sin() * lat2.cos() * dlon.cos();
    let deg = y.atan2(x).to_degrees();
    (deg + 360.0) % 360.0
}

/// 毫秒时间戳格式化成真机 `gainTime` 的 "YYYY-MM-DD HH:MM:SS"（本地时区）。
fn fmt_run_time(ms: i64) -> String {
    let secs = ms / 1000 + tz_offset_sec();
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    // Howard Hinnant civil_from_days
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
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// 服务端本地时区偏移（秒），默认 UTC+8，`PACE_TZ_OFFSET_SEC` 可覆盖。
fn tz_offset_sec() -> i64 {
    std::env::var("PACE_TZ_OFFSET_SEC")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| (-72_000..=72_000).contains(v))
        .unwrap_or(8 * 3600)
}

/// 单个轨迹点在 `allLocJson` 里的完整结构。
///
/// 字段语义与值域对齐**真机 6 条记录 / 1220 个采样点**（从 OBS `allLocJson`
/// 还原）：
/// - `lat`/`lng` 真机恒为 **-1.0**，真实坐标只走 `gLat`/`gLng`（gcj02）。服务端
///   点位判定读的是 `gLat/gLng`，不是 `lat/lng`——这一点早先猜反过。
/// - `flag` 真机**每个点都等于起跑毫秒戳**（恒定），逐点时间在 `gainTime`
///   （可读串）+ `totalTime`（累计秒）里，不在 flag。
/// - `count`（卫星数）真机 10~34、均值≈28；`bdD`（航向角）0~360 真实变化；
///   `bdS`（瞬时 m/s）0~4.8；`radius`（精度）聚在 ~3.8m；`gainTime` 非空。
///   这些真机里都是变化/非零量，早先的恒定值（1 / 0 / 1.0 / ""）是机器指纹。
/// - `speed` 单位 km/h，`avgSpeed` 单位 m/s。
#[allow(clippy::too_many_arguments)]
pub fn point_json_full(
    p: &TrackPoint,
    idx: usize,
    n: usize,
    start_ms: i64,
    _start_pt: (f64, f64),
    prev: &mut (f64, f64),
    cum: &mut (f64, i64, i64),
    prev_time: &mut i64,
    total_steps: i64,
    seed: u64,
    checkpoint_idxs: &[usize],
) -> Value {
    // BD-09 → GCJ-02（原点是 BD-09，服务端要 gcj02）
    let (glat, glng) = crate::api::coord::bd09_to_gcj02(p.lat, p.lon);
    let prev_pt = *prev; // 捕获上一点坐标（下面会被更新），用于算航向

    // 累计量
    let step_m = if idx == 0 {
        0.0
    } else {
        crate::track::geo::distance(*prev, (p.lat, p.lon))
    };
    let prev_cum = *cum; // 本段的起点累计量，供局部步幅计算
    cum.0 += step_m;
    cum.1 = (p.time_ms - start_ms) / 1000;
    *prev = (p.lat, p.lon);

    // 本段步数：累计步数按点序线性分配后取差
    let cum_steps_now = ((total_steps as f64) * (idx as f64 + 1.0) / n as f64).round() as i64;
    let cum_steps_prev = ((total_steps as f64) * idx as f64 / n as f64).round() as i64;
    let seg_steps = cum_steps_now - cum_steps_prev;

    // 瞬时速度用真实采样间隔，不是固定 5 秒
    let gap_sec = if idx == 0 {
        0.0
    } else {
        ((p.time_ms - *prev_time) as f64 / 1000.0).max(0.1)
    };
    *prev_time = p.time_ms;

    let dt = if idx == 0 { 0.0 } else { (p.time_ms - start_ms) as f64 / 1000.0 };
    let avg_speed = if dt > 0.5 { (cum.0 / dt).clamp(0.0, 6.3) } else { 0.0 };
    // 夹到跑步的物理区间（1.9~6.3 m/s），和规则里的 SPEED_FLOOR/CEIL 一致
    let inst = if idx == 0 {
        // 起点给个常见起步速度，不能是 0
        2.6
    } else {
        (step_m / gap_sec).clamp(1.9, 6.3) * 3.6
    };

    // 点位类型是有语义的，不是装饰。
    //
    // 老版本按概率瞎填（38% 得 3、54% 得 0、3% 得 1、2% 得 2），纯粹凑分布。
    // 真机的 isValidPoint（libswsport.so@0x10e84，XXXXBully 反汇编还原成
    // isvalidpoint.py:130-181）对它打分：type=1 得 30 分、type=2 得 20 分、
    // 其他得 15 分，及格线 65。也就是说「这个采样点算不算有效打卡点」直接
    // 由 type 决定，瞎填等于每个点都拿不到类型分。
    //
    //   0  起跑
    //   1  有效打卡点（最高分）
    //   2  次一级打卡点
    //   6  终点
    //   7  / -1  灰点：GPS 弱，跳变检查豁免
    let ptype = if idx == 0 {
        0
    } else if idx + 1 == n {
        6
    } else if checkpoint_idxs.iter().any(|&c| idx.abs_diff(c) <= 2) {
        // 真机经过打卡点时，连续几个采样点都会被标成打卡点类型——GPS
        // 定位是连续的，一个点位不可能只对应孤零零一个采样点。只标一个
        // 点的话，服务端按范围找点位时看到的密度太低，validPoint 打分
        // 过不了 65 的及格线，这次经过就不作数。
        1
    } else {
        let h = (p.lat.to_bits() ^ p.lon.to_bits() ^ seed).wrapping_mul(6364136223846793005);
        // 途中普通采样只允许 0 / 2，不再随机撒 3 和 8
        match (h >> 33) % 100 {
            0..=11 => 2,
            _ => 0,
        }
    };

    // GPS 精度半径：真机聚在 ~3.8m，不是早先的 1.5~3.0。小幅抖动。
    let h2 = (p.lon.to_bits() ^ seed).wrapping_mul(6364136223846793005);
    let radius = (3.6 + ((h2 >> 40) % 70) as f64 / 100.0).clamp(3.0, 6.0);

    // 卫星数：真机 10~34、均值≈28，逐点平滑游走，不是恒 1。
    let h3 = (idx as u64).wrapping_mul(0x2545F4914F6CDD1D) ^ seed;
    let count = (24 + ((h3 >> 32) % 11) as i64).clamp(10, 34);

    // 瞬时速度 m/s（bdS）：真机 0~4.8，和瞬时配速同源但单位不同。
    let inst_mps = if idx == 0 {
        0.0
    } else {
        (step_m / gap_sec).clamp(0.0, 5.0)
    };

    // 航向角 bdD（度）：真机 0~360 真实变化，早先恒 0 是指纹。
    let bearing = if idx == 0 {
        0.0
    } else {
        bearing_deg(prev_pt, (p.lat, p.lon))
    };

    // 定位时刻 gainTime：真机是可读时间串，早先空串是指纹。
    let gain_time = fmt_run_time(p.time_ms);

    // 海拔 bdA。
    //
    // 真机实测：单条 2km 记录内海拔在 20~64m 间起伏，每公里累计爬升 50+m——
    // 这是**气压计/GPS 高程的传感器噪声**，不是真实地形（平地校园）。早先用
    // 130m 基准 + 亚米漂移，累计爬升近乎 0，是「没有海拔数据」的典型特征，
    // 和真机差一个数量级。
    //
    // 这里用多尺度确定性噪声（三条不同周期的正弦，相位由 seed 定）叠在
    // ~35m 基准上：高频项提供相邻点 ~1m 的抖动，低频项提供整条几十米的漂移，
    // 累计爬升落在真机量级（2km 约百米）。同 seed 可复现。
    let ph = |k: u64| (((seed ^ k).wrapping_mul(0x9E3779B97F4A7C15) >> 40) as f64) / 16_777_216.0 * std::f64::consts::TAU;
    let fi = idx as f64;
    let altitude = (35.0
        + 9.0 * (fi * 0.075 + ph(1)).sin()
        + 5.0 * (fi * 0.21 + ph(2)).sin()
        + 3.0 * (fi * 0.54 + ph(3)).sin())
    .max(1.0);

    // 局部步幅：本段位移 / 本段步数。原来那个表达式化简后是
    // 「累计里程 / 全程步数」，单调递增的累积平均值，不是即时步幅。
    let d_seg = cum.0 - prev_cum.0;
    let step_distance = if d_seg > 0.5 && seg_steps >= 1 {
        let s_seg = seg_steps as f64;
        ((d_seg / s_seg) * 10000.0).round() / 10000.0
    } else {
        0.0
    };

    json!({
        "avgSpeed": (avg_speed * 10000.0).round() / 10000.0,
        "bdA": (altitude * 10.0).round() / 10.0,
        "bdD": (bearing * 10.0).round() / 10.0,
        "bdG": 1,
        "bdS": (inst_mps * 100.0).round() / 100.0,
        "coorType": "gcj02",
        "count": count,
        "dtr": 0.0,
        // flag：真机每个采样点的 flag 都等于**起跑毫秒戳**（恒定）。
        //
        // 实测 6 条记录、1220 个点，每条的 flag 都只有 1 个不同值 = 首点
        // 起跑时刻。逐点时间不在 flag，而在 gainTime（可读串）+ totalTime
        // （累计秒）里。早先把 flag 改成逐点 p.time_ms 是为了让 App 画线，
        // 但 TRAJECTORY-DISPLAY 已证明 App 根本不从 OBS 取轨迹（读本地库），
        // 那个改动既无必要又与真机不一致，故回到真机形态。
        //
        // verify_view 从 totalTime 反解时间，不读 flag，所以恒定 flag 不影响自检。
        "flag": start_ms,
        "gLat": (glat * 1e7).round() / 1e7,
        "gLng": (glng * 1e7).round() / 1e7,
        "gainTime": gain_time,
        "id": idx as i64 + 1,
        // lat/lng：真机恒为 **-1.0**，真实坐标只在 gLat/gLng。
        //
        // 早先改成真 BD-09 坐标，猜测「服务端按 lat/lng 判点位」，想修
        // type=9；但 TYPE9-NOTES 记录改完后 type=9「仍 false」，且 6 条真机
        // 记录的 lat/lng 全是 -1。服务端点位判定读的是 gLat/gLng（gcj02），
        // 不是 lat/lng。回到真机形态：-1 占位。
        "lat": -1.0,
        "lng": -1.0,
        "locType": 1,
        "locationId": "",
        // queueNum：真机恒为 0（点序在 id 里），早先用 idx 是指纹。
        "queueNum": 0,
        "radius": (radius * 100.0).round() / 100.0,
        "speed": (inst * 10000.0).round() / 10000.0,
        // state：真机采样点 state ∈ {1,2,3}，**从不为 0**（1220 个点统计：
        // 1 占 87%、3 占 12%、2 极少），首点也是 1。早先用 {0,1,2} 且首点 0，
        // 与真机相反。对齐真机：常态 1，快于上限 2，慢于下限 3；首点 1。
        "state": if idx == 0 {
            1
        } else {
            let lo = 1.39;
            let hi = 8.33;
            let v = step_m / gap_sec;
            if v > hi { 2 } else if v < lo { 3 } else { 1 }
        },
        "steps": ((total_steps as f64) * (idx as f64 + 1.0) / n as f64).round() as i64,
        // 局部步幅 = 本段位移 / 本段步数。
        //
        // 原来是 ((cum.0 / cum.0.max(1.0)) * (cum.0 / total_steps) ...)——
        // 进这分支的前提就是 cum.0 > 1.0，所以第一个因子恒等于 1.0，
        // 整个表达式化简成「累计里程 / 全程步数」，是个随点序单调增长的
        // 累积平均值，不是采样点的即时步幅。真实 App 这里是局部量。
        "stepDistance": step_distance,
        "totalDis": (cum.0 * 10000.0).round() / 10000.0,
        "totalTime": cum.1,
        "type": ptype,
        "validDis": (cum.0 * 10000.0).round() / 10000.0,
        "validTime": cum.1,
    })
}

/// 从**已组装好的报文**反解出校验视图。
///
/// 这是「自检守 A、提交发 B」的根治手段：不再让 preflight 吃内存里的
/// `sim`，而是先把报文 JSON 造出来、再反解回来校验。检查对象和发送对象
/// 从此物理上是同一份数据——不管以后谁在 build() 里塞新东西，只要它进了
/// 报文就会被检查到，进不了报文的东西也不会被误认为已检查。
#[derive(Debug, Clone)]
pub struct VerifyView {
    /// 从报文点位反解出的轨迹（时间戳来自 totalTime）。
    pub points: Vec<TrackPoint>,
    pub total_distance_m: f64,
    pub total_time_sec: i64,
    pub total_steps: i32,
    pub start_time_ms: i64,
    pub stop_time_ms: i64,
    /// 报文里的步数桶（原样）。
    pub step_buckets: Vec<crate::track::StepBucket>,
    /// 报文里的速度桶（原样）。
    pub speed_buckets: Vec<crate::track::SpeedBucket>,
    /// 报文声明的配速字段（分/公里 × 1000）。
    pub speed_field: i64,
    /// 点位条数。
    pub point_count: usize,
}

impl VerifyView {
    /// 包成 preflight 要的 SimResult 形状。
    ///
    /// 只填能从报文读到的东西；读不到的（瞬时速度、步幅）留空，
    /// 让相关检查自己跳过，而不是拿内存里的值冒充。
    pub fn as_sim(&self) -> crate::track::SimResult {
        crate::track::SimResult {
            speeds: Vec::new(),
            cadences: Vec::new(),
            strides: Vec::new(),
            total_steps: self.total_steps,
            avg_cadence: if self.total_time_sec > 0 {
                self.total_steps as f64 / (self.total_time_sec as f64 / 60.0)
            } else {
                0.0
            },
            speed_buckets: self.speed_buckets.clone(),
            step_buckets: self.step_buckets.clone(),
            actual_length_m: self.total_distance_m,
        }
    }
}

/// 从提交报文反解出校验视图。字段名和 `point_json_full` / `build` 一一对应。
pub fn verify_view(body: &Value) -> Result<VerifyView, String> {
    let start_ms = body
        .get("startTime")
        .and_then(|v| v.as_i64())
        .or_else(|| {
            body.get("allLocJson")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str::<Vec<Value>>(s).ok())
                .and_then(|a| a.first().and_then(|p| p.get("flag")).and_then(|f| f.as_i64()))
        })
        .ok_or("报文缺 startTime")?;
    let stop_ms = body.get("stopTime").and_then(|v| v.as_i64()).unwrap_or(0);

    let raw = body
        .get("allLocJson")
        .and_then(|v| v.as_str())
        .ok_or("报文缺 allLocJson")?;
    let arr: Vec<Value> = serde_json::from_str(raw).map_err(|e| format!("allLocJson 解析失败: {e}"))?;
    if arr.is_empty() {
        return Err("allLocJson 为空".into());
    }

    let mut points: Vec<TrackPoint> = Vec::with_capacity(arr.len());
    for (i, q) in arr.iter().enumerate() {
        // 真实坐标在 gLat/gLng（GCJ-02）；lat/lng 是 -1 占位
        let glat = q.get("gLat").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let glng = q.get("gLng").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if glat == 0.0 || glng == 0.0 {
            return Err(format!("第 {i} 个点缺 gLat/gLng"));
        }
        let (lat, lon) = crate::api::coord::gcj02_to_bd09(glat, glng);
        let total_time = q.get("totalTime").and_then(|v| v.as_i64()).unwrap_or(0);
        points.push(TrackPoint {
            lat,
            lon,
            time_ms: start_ms + total_time * 1000,
        });
    }

    let last = arr.last().unwrap();
    let total_distance_m = last
        .get("totalDis")
        .and_then(|v| v.as_f64())
        .ok_or("末点缺 totalDis")?;
    let total_time_sec = last
        .get("totalTime")
        .and_then(|v| v.as_i64())
        .ok_or("末点缺 totalTime")?;
    let total_steps = last
        .get("steps")
        .and_then(|v| v.as_i64())
        .ok_or("末点缺 steps")? as i32;

    let parse_buckets = |key: &str| -> Vec<Value> {
        match body.get(key) {
            Some(Value::Array(a)) => a.clone(),
            Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_default(),
            _ => Vec::new(),
        }
    };

    let step_buckets: Vec<crate::track::StepBucket> = parse_buckets("stepsPerTenSec")
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    let speed_buckets: Vec<crate::track::SpeedBucket> = parse_buckets("speedPerTenSec")
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();

    Ok(VerifyView {
        point_count: points.len(),
        points,
        total_distance_m,
        total_time_sec,
        total_steps,
        start_time_ms: start_ms,
        stop_time_ms: if stop_ms > 0 {
            stop_ms
        } else {
            start_ms + total_time_sec * 1000
        },
        step_buckets,
        speed_buckets,
        speed_field: body.get("speed").and_then(|v| v.as_i64()).unwrap_or(0),
    })
}

/// 计算 `speed` 字段：配速（分/公里）× 1000。
pub fn speed_field(distance_m: f64, time_sec: i64) -> i64 {
    if distance_m <= 0.0 || time_sec <= 0 {
        return 0;
    }
    let pace_min = (time_sec as f64 / 60.0) / (distance_m / 1000.0);
    (pace_min * 1000.0).round() as i64
}

/// 官方卡路里分档：由配速（分/公里）取 MET。
///
/// 三个分段来自运动世界官方换算（Neko 家族 `LightWhite520/src/track/calorie.rs`
/// 与 `blackpineapple` 两处独立实现一致）：
///
/// | 配速 p（分/公里） | MET |
/// |---|---|
/// | p < 10（快于 6:00/km） | 33/p + 4.1 |
/// | 10 ≤ p < 12 | 102/p − 3.7 |
/// | p ≥ 12（慢于 5:00/km） | 33.6/p + 1 |
pub fn met_for_pace(pace_min_per_km: f64) -> f64 {
    if !(pace_min_per_km.is_finite()) || pace_min_per_km <= 0.0 {
        return 0.0;
    }
    if pace_min_per_km < 10.0 {
        33.0 / pace_min_per_km + 4.1
    } else if pace_min_per_km < 12.0 {
        102.0 / pace_min_per_km - 3.7
    } else {
        33.6 / pace_min_per_km + 1.0
    }
}

/// 卡路里（千卡）= 体重 × MET × 小时数。
pub fn calorie_for(distance_m: f64, time_sec: i64, weight_kg: f64) -> i64 {
    if distance_m <= 0.0 || time_sec <= 0 || weight_kg <= 0.0 {
        return 0;
    }
    let pace = (time_sec as f64 / 60.0) / (distance_m / 1000.0);
    let met = met_for_pace(pace);
    let hours = time_sec as f64 / 3600.0;
    (weight_kg * met * hours).round() as i64
}

/// 平均功率（W）= 1.1 × 体重 × 速度(m/s)，保留一位小数。
///
/// 真机有这个字段且非零；发 `0` 等于自称「全程不做功」，与 pace、步频、
/// 卡路里三项都对不上。乘 1.1 是官方的体重→功率经验系数。
pub fn avg_power_for(distance_m: f64, time_sec: i64, weight_kg: f64) -> i64 {
    if distance_m <= 0.0 || time_sec <= 0 || weight_kg <= 0.0 {
        return 0;
    }
    let v = distance_m / time_sec as f64;
    // **必须取整**。服务端 avgPower 是整型（参考实现 `RunDetail.avg_power: i64`），
    // 发 187.5 时它反序列化截断成 187，再拿 187 重算签名，而我们的签名串里
    // 是 "187.5" —— 两边拼出来的串不同，直接 10501「验证签名失败」。
    // 这种错 body 内部完全自洽、本地 audit 也查不出来，只有真发一次才暴露。
    (1.1 * weight_kg * v).round() as i64
}

/// 从**已组装好的点位数组**算累计爬升（米）。
///
/// 必须用报文里的 `bdA` 序列而不是另算一套海拔：服务端按相邻采样点的
/// 海拔差累计爬升，若上报值与点位序列不一致，`totalAscent` 与轨迹直接
/// 互相矛盾——而 pace 原来恒定发 0，等于「一条有起伏的轨迹零爬升」。
pub fn total_ascent_from_points(pts: &[Value]) -> i32 {
    let mut prev: Option<f64> = None;
    let mut sum = 0.0f64;
    for p in pts {
        let a = p.get("bdA").and_then(|v| v.as_f64());
        let Some(a) = a else { continue };
        if let Some(before) = prev {
            let d = a - before;
            if d > 0.0 {
                sum += d;
            }
        }
        prev = Some(a);
    }
    sum.round() as i32
}

/// 从已组装好的点位数组切圈（每 1000m 一圈，末圈 `isFullLap=false`）。
///
/// 结构对齐真机 OBS `laps_json`（13 个字段，见 Neko `track/wire.rs::build_laps`）：
/// `avgPace` 用十进制分钟/公里、`avgStride` 用厘米、`elevationGain` 按圈内
/// 正增量累计、`endAltRel` 相对首点海拔。
///
/// pace 原来发空数组。真机跑 2 公里必有 2 圈，空数组等于「这条轨迹没有
/// 分段信息」，而服务端的分段审核、圈配速展示都读这一列。
pub fn build_laps_from_points(pts: &[Value], start_ms: i64) -> Vec<Value> {
    let mut laps: Vec<Value> = Vec::new();
    let Some(last) = pts.last() else { return laps };
    let last_dis = last.get("totalDis").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let alt0 = pts
        .first()
        .and_then(|p| p.get("bdA"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    if pts.len() < 2 || last_dis <= 0.0 {
        return laps;
    }

    let num = |q: &Value, k: &str| q.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let r2 = |x: f64| (x * 100.0).round() / 100.0;
    let r4 = |x: f64| (x * 10000.0).round() / 10000.0;

    let (mut prev_d, mut prev_t, mut prev_steps, mut prev_gain) = (0.0f64, 0i64, 0i64, 0.0f64);
    // 上限防止异常数据把循环卡死（每圈至少 1m 才继续）
    let mut guard = 0usize;
    while prev_d < last_dis && guard < 10_000 {
        guard += 1;
        let d_now = (prev_d + 1000.0).min(last_dis);
        let (mut t_now, mut steps, mut altitude) = (
            num(last, "totalTime") as i64,
            num(last, "steps") as i64,
            num(last, "bdA"),
        );
        let mut gain = 0.0f64;
        for pair in pts.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            let climb = (num(b, "bdA") - num(a, "bdA")).max(0.0);
            let (da, db) = (num(a, "totalDis"), num(b, "totalDis"));
            if d_now < last_dis && db >= d_now && db > da {
                let k = (d_now - da) / (db - da);
                t_now = (num(a, "totalTime") + (num(b, "totalTime") - num(a, "totalTime")) * k).round() as i64;
                steps = (num(a, "steps") + (num(b, "steps") - num(a, "steps")) * k).round() as i64;
                altitude = num(a, "bdA") + (num(b, "bdA") - num(a, "bdA")) * k;
                gain += climb * k;
                break;
            }
            gain += climb;
        }
        let lap_d = d_now - prev_d;
        let lap_t = t_now - prev_t;
        let lap_steps = steps - prev_steps;
        let idx = laps.len() as i64 + 1;
        laps.push(json!({
            "avgCadence": if lap_t > 0 { json!(r2(lap_steps as f64 / (lap_t as f64 / 60.0))) } else { Value::Null },
            "avgPace": if lap_t > 0 && lap_d > 0.0 { json!(r4(lap_t as f64 / 60.0 / (lap_d / 1000.0))) } else { Value::Null },
            "avgStride": if lap_steps > 0 { json!(r2(lap_d / lap_steps as f64 * 100.0)) } else { Value::Null },
            "cumulativeDuration": t_now,
            "distance": r4(lap_d),
            "duration": lap_t,
            "elevationGain": r2(gain - prev_gain),
            "endAltAbs": r2(altitude),
            "endAltRel": r2(altitude - alt0),
            "flag": start_ms,
            "id": idx,
            "isFullLap": lap_d >= 1000.0,
            "lapIndex": idx,
            "step": lap_steps,
        }));
        prev_d = d_now;
        prev_t = t_now;
        prev_steps = steps;
        prev_gain = gain;
        // 末圈已到终点，避免 last_dis 恰好是整数倍时再切一个 0 长度圈
        if d_now >= last_dis {
            break;
        }
    }
    laps
}

/// 派生字段互校：报文里由轨迹派生出来的几个数必须与点位自洽。
///
/// 这一层存在的理由：`totalAscent`、`calorie`、`avgPower`、`speed` 都是
/// 「看着报文体随便填也不会报错」的字段，一旦哪个分支回退成常量
/// （0 爬升、62 kcal/km、0 功率），报文仍然能提交成功，只在服务端那边
/// 表现为「这项数据来源可疑」。所以每次提交前，用**已组装好的报文体**
/// 反算一遍，跟声明值比——检查对象与发送对象是同一份。
///
/// 返回空 vec 表示自洽；非空表示必须拦下（不是提示）。
pub fn audit_derived(body: &Value) -> Vec<String> {
    let mut probs: Vec<String> = Vec::new();
    let Some(raw) = body.get("allLocJson").and_then(|v| v.as_str()) else {
        return vec!["报文缺 allLocJson".into()];
    };
    let pts: Vec<Value> = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return vec![format!("allLocJson 解析失败: {e}")],
    };
    let dis = body.get("totalDis").and_then(|v| v.as_i64()).unwrap_or(0);
    let time_sec = body.get("totalTime").and_then(|v| v.as_i64()).unwrap_or(0);

    // 1) 累计爬升 = 点位 bdA 正增量之和
    let expect_ascent = total_ascent_from_points(&pts) as i64;
    let declared_ascent = body
        .get("totalAscent")
        .and_then(|v| v.as_i64())
        .unwrap_or(i64::MIN);
    if (declared_ascent - expect_ascent).abs() > 1 {
        probs.push(format!(
            "totalAscent 声明 {declared_ascent} 与点位反算 {expect_ascent} 不符"
        ));
    }

    // 2) 卡路里必须走 MET 折算，不能是固定 kcal/km
    let expect_cal = calorie_for(dis as f64, time_sec, DEFAULT_WEIGHT_KG);
    let declared_cal = body.get("calorie").and_then(|v| v.as_i64()).unwrap_or(i64::MIN);
    if (declared_cal - expect_cal).abs() > 2 {
        probs.push(format!(
            "calorie 声明 {declared_cal} 与 MET 折算 {expect_cal} 不符"
        ));
    }

    // 3) 平均功率非零且与速度成比例（整数瓦特，服务端 avgPower 是整型）
    let expect_pow = avg_power_for(dis as f64, time_sec, DEFAULT_WEIGHT_KG);
    let declared_pow = body.get("avgPower").and_then(|v| v.as_i64()).unwrap_or(i64::MIN);
    if declared_pow != expect_pow {
        probs.push(format!(
            "avgPower 声明 {declared_pow} 与折算 {expect_pow} 不符（必须是整数）"
        ));
    }

    // 4) speed 是配速×1000，不是速度×1000
    let expect_speed = speed_field(dis as f64, time_sec);
    let declared_speed = body.get("speed").and_then(|v| v.as_i64()).unwrap_or(i64::MIN);
    if declared_speed != expect_speed {
        probs.push(format!(
            "speed 声明 {declared_speed} 与配速换算 {expect_speed} 不符"
        ));
    }

    // 5) 圈数据存在时，圈里程和 / 圈时长和必须与总量一致
    let laps = build_laps_from_points(&pts, body.get("startTime").and_then(|v| v.as_i64()).unwrap_or(0));
    if !laps.is_empty() {
        let sum_d: f64 = laps.iter().filter_map(|l| l["distance"].as_f64()).sum();
        if (sum_d - dis as f64).abs() > 1.5 {
            probs.push(format!("圈里程和 {sum_d:.1}m 与 totalDis {dis}m 不符"));
        }
        let sum_t: i64 = laps.iter().filter_map(|l| l["duration"].as_i64()).sum();
        if (sum_t - time_sec).abs() > laps.len() as i64 {
            probs.push(format!("圈时长和 {sum_t}s 与 totalTime {time_sec}s 不符"));
        }
        let last_ok = laps.last().map(|l| l["isFullLap"].as_bool() == Some(false)
            || (dis % 1000 == 0));
        if last_ok == Some(false) {
            probs.push("末圈标注为整圈，但总里程不是 1000m 的整数倍".into());
        }
    }

    probs
}
/// 构建完整提交报文。
pub fn build(
    params: &SubmitParams,
    points: &[TrackPoint],
    fixed: &[FixedPoint],
    sim: &crate::track::SimResult,
) -> Result<Submission, String> {
    if points.is_empty() {
        return Err("轨迹为空，无法组装报文".into());
    }
    if params.total_distance_m <= 0 || params.total_steps < 0 {
        return Err("距离必须为正，步数不能为负".into());
    }

    let complete = params.total_distance_m >= params.sel_distance_m
        && params.total_time_sec >= params.sel_run_time_sec;
    let un_complete_reason = if complete { 0 } else { 10 };
    let avg_step_freq = if params.total_time_sec > 0 {
        (params.total_steps as f64 * 60.0 / params.total_time_sec as f64).round() as i64
    } else {
        0
    };
    let speed_val = speed_field(params.total_distance_m as f64, params.total_time_sec);
    // 卡路里与功率用官方折算，不再用固定 62 kcal/km、也不再恒发 0 功率
    let calorie = calorie_for(
        params.total_distance_m as f64,
        params.total_time_sec,
        DEFAULT_WEIGHT_KG,
    );
    let avg_power: i64 = avg_power_for(
        params.total_distance_m as f64,
        params.total_time_sec,
        DEFAULT_WEIGHT_KG,
    );
    // 整数类型下 to_string 与 serde 序列化必然一致，不需要再做文本对齐。
    let avg_power_str = avg_power.to_string();

    // 轨迹点带瞬时速度（中心差分），服务端画图会用到
    // 单一数据源：直接用 simulate() 产出的桶。
    //
    // 之前这里拿 make_jitter(amp=1.0) **重新生成**了一套桶写进报文，而自检
    // 校验的是 sim.step_buckets、OBS 明细传的也是 sim.step_buckets——
    // 自检守 A、提交发 B、明细传 A，反检测设计整个落空。
    // amp=1.0 还是 ±100% 的均匀抖动，会造出 0 步桶（10 秒静止）。
    let sb: Vec<crate::track::SpeedBucket> = sim.speed_buckets.clone();
    let stb: Vec<crate::track::StepBucket> = sim.step_buckets.clone();

    // 逐点生成，累计量必须顺序推进
    let mut prev = points.first().map(|p| (p.lat, p.lon)).unwrap_or((0.0, 0.0));
    let start_pt = prev;
    let mut cum = (0.0f64, 0i64, 0i64);
    let seed = params.start_time_ms as u64 ^ 0x9E3779B97F4A7C15;
    let mut prev_time = points.first().map(|p| p.time_ms).unwrap_or(params.start_time_ms);
    // 打卡点落在轨迹的哪几个采样点上：position 是首次经过索引
    let cp_idxs: Vec<usize> = fixed
        .iter()
        .filter(|f| f.position >= 0)
        .map(|f| f.position as usize)
        .filter(|&i| i < points.len())
        .collect();
    let loc_items: Vec<Value> = points
        .iter()
        .enumerate()
        .map(|(i, p)| {
            point_json_full(
                p, i, points.len(), params.start_time_ms, start_pt,
                &mut prev, &mut cum, &mut prev_time, params.total_steps as i64, seed,
                &cp_idxs,
            )
        })
        .collect();
    let all_loc_json = serde_json::to_string(&loc_items).map_err(|e| e.to_string())?;

    // 累计爬升必须从**已生成的点位**反算，与 allLocJson 里的 bdA 物理同源。
    // 原来恒发 0：点位有起伏、总爬升却是 0，服务端按两点海拔差累计时立刻
    // 得到与上报值冲突的两套数字。
    let total_ascent = total_ascent_from_points(&loc_items);

    // 原样回显服务端的点位结构：lon/lat 是 BD-09，glon/glat 是 GCJ-02，
    // isFixed 区分必经点与普通点，pointName 是点位标识。缺任何一个，
    // 服务端都可能认不出这是它发的那批点。
    let fp_items: Vec<Value> = fixed
        .iter()
        .map(|f| {
            json!({
                "lon": f.lon,
                "lat": f.lat,
                "isFixed": f.is_fixed,
                "pointName": f.point_name,
                "glon": f.glon,
                "glat": f.glat,
                "position": f.position,
            })
        })
        .collect();
    // 主报文这一层要 envelope：改成裸数组服务端直接 -10000
    // （成绩上传失败）。记录详情里 fivePointJson 回显 "" 是正常的，
    // 点位数据走的是 OBS 明细里的 fixed_point_json。
    let five_envelope = json!({
        "fivePointJson": serde_json::to_string(&fp_items).map_err(|e| e.to_string())?,
        "useZip": false,
        "freedomShowFence": false,
        "runAreaId": params.run_area_id,
        "geoFencesJson": params.geo_fences_json,
    });
    let five_pt_json = serde_json::to_string(&five_envelope).map_err(|e| e.to_string())?;

    // 提交报文的桶 id 规则：60000+i，queueNum=i
    let mut sb = sb;
    let mut stb = stb;
    crate::track::buckets::apply_submit_ids(&mut sb, &mut stb);
    // 服务端要的是**数组**，不是字符串化的 JSON。
    // 早先按 allLocJson 的模式套成了字符串，结果提交回来 -10000。
    let speed_arr = serde_json::to_value(&sb).map_err(|e| e.to_string())?;
    let step_arr = serde_json::to_value(&stb).map_err(|e| e.to_string())?;

    // ---- geeToken ----
    //
    // 真机的 geeToken 是本地算的（AES-256-CBC 套 GEE\0 帧），不依赖极验
    // 回调，所以离线也能生成。原来两处都发空串。
    //
    // 发空串的后果不是直接报错拒单，而是服务端把整次 run 的所有采样点
    // 统一打上「未通过校验」的兜底类型标记——表现就是里程、时长、步频、
    // 配速四项全过，只有「通过点位」那一项永远 false，而轨迹怎么调都是
    // 干净的。这条线很容易把排查带到轨迹上去。
    //
    // 签名侧和 body 侧必须引用同一个 String，否则签名串和回执体里的
    // token 不一致，服务端直接 10501。
    let gee_token = crate::crypto::geetoken::token_for_run(&params.uid, &params.uuid);

    // ---- 签名（31 字段）----
    let sign_fields: SignFields = vec![
        ("address".into(), Some(String::new())),
        ("avgPower".into(), Some(avg_power_str.clone())),
        ("avgStepFreq".into(), Some(avg_step_freq.to_string())),
        ("calorie".into(), Some(calorie.to_string())),
        ("complete".into(), Some(complete.to_string())),
        ("errorCode".into(), Some("0".into())),
        ("faceCheck".into(), Some("0".into())),
        ("geeToken".into(), Some(gee_token.clone())),
        // Android 的 String.valueOf(false) 是小写；写成 "False" 签名必然不过（10501）
        ("getPrize".into(), Some("false".into())),
        // 签名侧固定字符串 "null"，body 侧才是 JSON null
        ("goalId".into(), Some("null".into())),
        ("policy".into(), Some(params.policy.clone())),
        ("selDistance".into(), Some(params.sel_distance_m.to_string())),
        ("selRunTime".into(), Some(params.sel_run_time_sec.to_string())),
        ("selectedUnid".into(), Some(params.unid.clone())),
        ("speed".into(), Some(speed_val.to_string())),
        ("sportType".into(), Some(params.sport_type.to_string())),
        ("startTime".into(), Some(params.start_time_ms.to_string())),
        ("status".into(), Some("0".into())),
        ("stopTime".into(), Some(params.stop_time_ms.to_string())),
        ("themeId".into(), Some("0".into())),
        ("totalAscent".into(), Some(total_ascent.to_string())),
        ("totalDis".into(), Some(params.total_distance_m.to_string())),
        ("totalSteps".into(), Some(params.total_steps.to_string())),
        ("totalTime".into(), Some(params.total_time_sec.to_string())),
        ("uid".into(), Some(params.uid.clone())),
        ("unCompleteReason".into(), Some(un_complete_reason.to_string())),
        ("unauthorized".into(), Some("0".into())),
        ("useMobilityTools".into(), Some("0".into())),
        ("uuid".into(), Some(params.uuid.clone())),
        ("validDis".into(), Some(params.total_distance_m.to_string())),
        ("validTime".into(), Some(params.total_time_sec.to_string())),
    ];
    let signature = sport_signature(&sign_fields);
    let canonical = crate::crypto::sign::canonical_string(&sign_fields);

    let save_record = json!({
        "address": "",
        "allLocJson": all_loc_json,
        "avgPower": avg_power,
        "avgStepFreq": avg_step_freq,
        "calorie": calorie,
        "complete": complete,
        "errorCode": 0,
        "faceCheck": 0,
        "fivePointJson": five_pt_json,
        "geeToken": gee_token,
        "getPrize": false,
        "goalId": Value::Null,
        "isUpload": false,
        "latitude": points[0].lat,
        "longitude": points[0].lon,
        // 服务端从策略里取，客户端传 0
        "maxRunTime": 0,
        "minSteps": 0,
        "policy": params.policy,
        "recordUrl": "",
        "roomId": 0,
        "runAreaId": params.run_area_id,
        "segmentJson": "[]",
        "selDistance": params.sel_distance_m,
        "selRunTime": params.sel_run_time_sec,
        "selectedUnid": params.unid,
        "more": false,
        "originalSign": crate::crypto::sign::original_sign(&sign_fields),
        "signature": signature,
        "speed": speed_val,
        "speedPerTenSec": speed_arr,
        "sportType": params.sport_type,
        "startTime": params.start_time_ms,
        "status": 0,
        "stepsPerTenSec": step_arr,
        "stopTime": params.stop_time_ms,
        "themeId": 0,
        "totalAscent": total_ascent,
        "totalDis": params.total_distance_m,
        "totalSteps": params.total_steps,
        "totalTime": params.total_time_sec,
        "uid": params.uid,
        "unCompleteReason": un_complete_reason,
        "unauthorized": 0,
        "useMobilityTools": 0,
        "uuid": params.uuid,
        "validDis": params.total_distance_m,
        "validTime": params.total_time_sec,
    });

    // 真正的 objectKey 要等提交返回 rrid 才能算，这里先给出 detail key 的雏形
    let object_key = crate::api::obs::detail_key(params.start_time_ms, &params.uuid);

    Ok(Submission { save_record, object_key, canonical })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 给旧测试造一份 sim，桶用确定值（不依赖 OU 随机）。
    pub(super) fn test_sim(p: &SubmitParams, pts: &[TrackPoint]) -> crate::track::SimResult {
        let _ = p;
        let n = (p.total_time_sec / 10).max(1) as usize;
        let per = (p.total_steps.max(0) as f64 / n as f64).ceil() as i32;
        crate::track::SimResult {
            speeds: vec![2.8; pts.len().max(1)],
            cadences: vec![160.0; pts.len().max(1)],
            strides: vec![1.05; pts.len().max(1)],
            total_steps: p.total_steps,
            avg_cadence: 160.0,
            speed_buckets: (0..n)
                .map(|i| crate::track::SpeedBucket {
                    distance: 28.0,
                    begin_time: p.start_time_ms + i as i64 * 10_000,
                    end_time: p.start_time_ms + (i as i64 + 1) * 10_000,
                    flag: p.start_time_ms,
                    id: 60_000 + i as i64,
                    state: 0,
                    queue_num: i as i32,
                })
                .collect(),
            step_buckets: (0..n)
                .map(|i| crate::track::StepBucket {
                    steps_num: per,
                    begin_time: p.start_time_ms + i as i64 * 10_000,
                    end_time: p.start_time_ms + (i as i64 + 1) * 10_000,
                    flag: p.start_time_ms,
                    id: 60_000 + i as i64,
                    max_diff: 2,
                    min_diff: 0,
                    avg_diff: 1,
                    state: 0,
                    queue_num: i as i32,
                })
                .collect(),
            actual_length_m: p.total_distance_m as f64,
        }
    }

    fn params() -> SubmitParams {
        SubmitParams {
            uid: "123456".into(),
            unid: "88".into(),
            uuid: "00000000-0000-4000-8000-000000000001".into(),
            sport_type: 1,
            total_distance_m: 2000,
            total_time_sec: 720,
            total_steps: 2300,
            start_time_ms: 1_730_000_000_000,
            stop_time_ms: 1_730_000_720_000,
            sel_distance_m: 1800,
            sel_run_time_sec: 600,
            policy: "policy_str".into(),
            run_area_id: 7,
            geo_fences_json: "[]".into(),
            total_ascent: 0,
        }
    }

    fn points() -> Vec<TrackPoint> {
        let n = 200;
        let step = 720_000 / (n - 1) as i64;
        (0..n)
            .map(|i| TrackPoint {
                lat: 30.5 + i as f64 * 1e-5,
                lon: 104.0,
                time_ms: if i == n - 1 {
                    1_730_000_000_000 + 720_000
                } else {
                    1_730_000_000_000 + i as i64 * step
                },
            })
            .collect()
    }

    #[test]
    fn speed_is_pace_not_velocity() {
        // 2000m / 720s → 配速 6.0 分/km → 6000
        assert_eq!(speed_field(2000.0, 720), 6000);
        // 若误按 m/s×1000 会得到 2777，明显不同
        assert_ne!(speed_field(2000.0, 720), (2000.0f64 / 720.0 * 1000.0).round() as i64);
        assert_eq!(speed_field(0.0, 720), 0);
        assert_eq!(speed_field(2000.0, 0), 0);
    }

    #[test]
    fn payload_has_all_required_fields() {
        let sim = test_sim(&params(), &points());
        let sub = build(&params(), &points(), &[], &sim).unwrap();
        let v = &sub.save_record;
        for k in [
            "allLocJson", "fivePointJson", "speedPerTenSec", "stepsPerTenSec",
            "signature", "speed", "avgStepFreq", "complete", "unCompleteReason",
            "totalDis", "totalTime", "totalSteps", "uid", "uuid", "sportType",
        ] {
            assert!(v.get(k).is_some(), "缺字段 {k}");
        }
    }

    #[test]
    fn nested_json_is_double_serialized() {
        let sim = test_sim(&params(), &points());
        let sub = build(&params(), &points(), &[], &sim).unwrap();
        let v = &sub.save_record;
        // allLocJson 是字符串化的 JSON（双重序列化）
        assert!(v["allLocJson"].is_string(), "allLocJson 应为字符串");
        let inner: Value = serde_json::from_str(v["allLocJson"].as_str().unwrap()).unwrap();
        assert!(inner.is_array(), "allLocJson 解一层后应是数组");

        // speedPerTenSec / stepsPerTenSec 是**数组**，不要套成字符串。
        // 早先按 allLocJson 的模式一起套了，提交回来 -10000。
        assert!(v["speedPerTenSec"].is_array(), "speedPerTenSec 应为数组");
        assert!(v["stepsPerTenSec"].is_array(), "stepsPerTenSec 应为数组");
        assert!(!v["speedPerTenSec"].as_array().unwrap().is_empty());
        assert!(!v["stepsPerTenSec"].as_array().unwrap().is_empty());
    }

    /// 签名里的 bool 必须是小写。
    ///
    /// Android 的 `String.valueOf(false)` 是 `"false"`，而 Rust 的
    /// `false.to_string()` 是 `"false"`、手写 `"False"` 就成了大写。
    /// 差一个字母，服务端直接回 10501 验证签名失败。
    #[test]
    fn sign_bool_is_lowercase() {
        let sim = test_sim(&params(), &points());
        let sub = build(&params(), &points(), &[], &sim).unwrap();
        let orig = sub.save_record["originalSign"].as_str().unwrap();
        assert!(
            !orig.contains("=True") && !orig.contains("=False"),
            "originalSign 里出现大写 bool: {orig}"
        );
        assert!(orig.contains("getPrize=false"), "getPrize 应为小写: {orig}");
        assert!(orig.contains("complete="), "complete 应参与签名");
    }

    #[test]
    fn signature_is_32_hex() {
        let sim = test_sim(&params(), &points());
        let sub = build(&params(), &points(), &[], &sim).unwrap();
        let sig = sub.save_record["signature"].as_str().unwrap();
        assert_eq!(sig.len(), 32);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn complete_flag_follows_targets() {
        let sim = test_sim(&params(), &points());
        // 达标
        let sub = build(&params(), &points(), &[], &sim).unwrap();
        assert_eq!(sub.save_record["complete"], true);
        assert_eq!(sub.save_record["unCompleteReason"], 0);

        // 不达标
        let mut p = params();
        p.sel_distance_m = 5000;
        let sub2 = build(&p, &points(), &[], &sim).unwrap();
        assert_eq!(sub2.save_record["complete"], false);
        assert_eq!(sub2.save_record["unCompleteReason"], 10);
    }

    #[test]
    fn buckets_match_duration() {
        let sim = test_sim(&params(), &points());
        let sub = build(&params(), &points(), &[], &sim).unwrap();
        let sb = sub.save_record["speedPerTenSec"]
            .as_array()
            .expect("speedPerTenSec 应为数组");
        assert_eq!(sb.len(), 72, "720s 应有 72 个桶");
        let st = sub.save_record["stepsPerTenSec"]
            .as_array()
            .expect("stepsPerTenSec 应为数组");
        assert_eq!(st.len(), 72, "步数桶也要 72 个");
    }

    #[test]
    fn object_key_shape() {
        let sim = test_sim(&params(), &points());
        let sub = build(&params(), &points(), &[], &sim).unwrap();
        // 详情页 key：run_data/{YYYYMMDDHH}/{uuid}.json
        assert!(sub.object_key.starts_with("run_data/"), "{}", sub.object_key);
        assert!(sub.object_key.ends_with(".json"), "{}", sub.object_key);
        assert!(sub.object_key.contains("00000000-0000-4000-8000-000000000001"));
    }

    #[test]
    fn submit_buckets_use_60000_ids() {
        let sim = test_sim(&params(), &points());
        let sub = build(&params(), &points(), &[], &sim).unwrap();
        let sp = &sub.save_record["speedPerTenSec"];
        let arr = sp.as_array().expect("speedPerTenSec 应该是数组");
        assert!(!arr.is_empty());
        assert_eq!(arr[0]["id"], 60000);
        assert_eq!(arr[0]["queueNum"], 0);
        assert_eq!(arr[1]["id"], 60001);
        assert_eq!(arr[1]["queueNum"], 1);

        // stepsPerTenSec 用同样的 id 规则
        let sa = sub.save_record["stepsPerTenSec"].as_array().expect("应为数组");
        assert_eq!(sa[0]["id"], 60000);
        assert_eq!(sa[0]["queueNum"], 0);
    }

    #[test]
    fn rejects_empty_track() {
        assert!(build(&params(), &[], &[], &test_sim(&params(), &[])).is_err());
    }
}

/// 提交成绩：`POST /api/v70260/runnings/save/record`。
///
/// 这是整条链路唯一会产生真实记录的请求。失败要区分两类：
/// - 网络/加密失败 → 可重试
/// - 业务驳回 → **不能重试**，错误次数会累积
///
/// 成功码是 **10000**，不是 0。这一点很容易踩：接口在顶层 `error` 里回
/// `{"error":10000,"message":"成功"}`，照「err != 0 就是失败」判断就会把
/// 一次成功的提交当成驳回，用户看到「提交被驳回 error=10000: 成功」。
pub fn save_record(
    client: &Client,
    body: &Value,
    runes: &str,
    runef: &str,
) -> Result<Value, String> {
    // 真机提交会额外带 runes / runef 两个关联头：
    //   runes = 策略时间戳 + uid      （这条记录基于哪一次策略下发）
    //   runef = 运行 uuid + startTime （这份轨迹属于哪一次运行）
    // 缺了它们提交照样能成，但风控侧少了把「策略→运行→成绩」串起来的
    // 线索，关联性更弱。见 Miempire src/api/submit.rs:228-237。
    let raw = client.request_with_headers(
        "POST",
        &format!("{}/api/v70260/runnings/save/record", crate::crypto::consts::hosts::RUN),
        Some(body),
        &[("runes", runes), ("runef", runef)],
    )?;
    let v = crate::api::client::expect_ok(&raw)?;
    let err = v.get("error").and_then(|e| e.as_i64()).unwrap_or(0);
    // 10000 是成功码（和 HTTP 的 200 一个道理，别拿 0 当成功）
    if err != 0 && err != 10000 {
        let msg = v
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("未知错误");
        let hint = match err {
            11016 => "（不在有效跑步时段，记分跑有固定时段限制，等等再来）",
            10003 => "（签名错误，检查 device_id 是否被重置）",
            10121 => "（账号已在其他设备登录，先退出或等 1 小时）",
            10603 => "（请求过于频繁，被限流）",
            _ => "",
        };
        return Err(format!("提交被驳回 error={err}: {msg}{hint}"));
    }
    Ok(v.get("data").cloned().unwrap_or(Value::Null))
}

/// 提交后的记录 id。
pub fn extract_record_id(data: &Value) -> Option<i64> {
    data.get("rrid")
        .or_else(|| data.get("id"))
        .and_then(|x| x.as_i64())
        .or_else(|| {
            data.get("recordId")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse().ok())
        })
}

#[cfg(test)]
mod point_shape_tests {
    use super::tests::test_sim;
    use super::*;

    fn params() -> SubmitParams {
        SubmitParams {
            uid: "123456".into(),
            unid: "88".into(),
            uuid: "00000000-0000-4000-8000-000000000001".into(),
            sport_type: 1,
            total_distance_m: 2000,
            total_time_sec: 720,
            total_steps: 2300,
            start_time_ms: 1_790_000_000_000,
            stop_time_ms: 1_790_000_720_000,
            sel_distance_m: 1800,
            sel_run_time_sec: 600,
            policy: "policy_str".into(),
            run_area_id: 7,
            geo_fences_json: "[]".into(),
            total_ascent: 0,
        }
    }

    fn tp(lat: f64, lon: f64, t: i64) -> TrackPoint {
        TrackPoint { lat, lon, time_ms: t }
    }

    /// 每个采样点的 `flag` 必须恒等于起跑毫秒戳（真机形态）。
    #[test]
    fn flag_is_constant_start() {
        let pts: Vec<TrackPoint> = (0..10)
            .map(|i| tp(31.2304 + i as f64 * 2e-4, 121.4737, 1_790_000_000_000 + i * 5000))
            .collect();
        let params = params();
        let sim = test_sim(&params, &pts);
        let sub = build(&params, &pts, &[], &sim).unwrap();
        let s = sub.save_record["allLocJson"].as_str().unwrap();
        let arr: Vec<serde_json::Value> = serde_json::from_str(s).unwrap();

        // 真机 6 条记录 1220 个点，每条 flag 只有 1 个值 = 起跑时刻。
        let flags: Vec<i64> = arr.iter().map(|p| p["flag"].as_i64().unwrap()).collect();
        assert!(
            flags.iter().all(|&f| f == params.start_time_ms),
            "flag 必须恒等于 startTime，实际 {flags:?}"
        );
        // 逐点时间走 gainTime（非空）+ totalTime，不在 flag
        assert!(
            arr.iter().all(|p| !p["gainTime"].as_str().unwrap_or("").is_empty()),
            "gainTime 不能为空"
        );
    }

    /// 海拔不能是常量。
    #[test]
    fn altitude_varies() {
        let pts: Vec<TrackPoint> = (0..40)
            .map(|i| tp(31.2304 + i as f64 * 2e-4, 121.4737, 1_790_000_000_000 + i * 5000))
            .collect();
        let params = params();
        let sim = test_sim(&params, &pts);
        let sub = build(&params, &pts, &[], &sim).unwrap();
        let s = sub.save_record["allLocJson"].as_str().unwrap();
        let arr: Vec<serde_json::Value> = serde_json::from_str(s).unwrap();

        let alts: Vec<f64> = arr.iter().map(|p| p["bdA"].as_f64().unwrap()).collect();
        // 恒 1.0 是「这个点没有海拔数据」的特征，服务端据此算累计爬升
        assert!(alts.iter().any(|&a| a > 1.5), "海拔应脱离 1.0，实际均值约 {}", alts.iter().sum::<f64>() / alts.len() as f64);

        // 相邻两点变化要小，真实 GPS 高程是连续量
        let max_step = alts.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f64::max);
        assert!(max_step < 3.0, "相邻点海拔跳变 {max_step:.2}m，太假");

        // 整条轨迹要有起伏
        let mean = alts.iter().sum::<f64>() / alts.len() as f64;
        let var = alts.iter().map(|a| (a - mean).powi(2)).sum::<f64>() / alts.len() as f64;
        assert!(var.sqrt() > 0.05, "海拔标准差 {} 太小，等于一条直线", var.sqrt());
    }

    #[test]
    fn all_loc_json_has_server_fields() {
        let pts = vec![
            tp(31.2304, 121.4737, 1_790_000_000_000),
            tp(31.2306, 121.4741, 1_790_000_005_000),
            tp(31.2309, 121.4745, 1_790_000_010_000),
        ];
        let params = params();
        let sim = test_sim(&params, &pts);
        let sub = build(&params, &pts, &[], &sim).unwrap();
        let s = sub.save_record["allLocJson"].as_str().unwrap();
        let arr: Vec<serde_json::Value> = serde_json::from_str(s).unwrap();
        assert_eq!(arr.len(), 3);

        let p0 = &arr[0];
        // lat/lng 真机恒为 -1.0，真实坐标只在 gLat/gLng（gcj02）。
        // 服务端点位判定读 gLat/gLng，不是 lat/lng（见 point_json_full 文档）。
        assert_eq!(p0["lat"].as_f64().unwrap(), -1.0, "lat 应为真机占位 -1.0");
        assert_eq!(p0["lng"].as_f64().unwrap(), -1.0, "lng 应为真机占位 -1.0");
        // 另一套坐标在 gLat/gLng，已转到 gcj02
        assert!(p0["gLat"].as_f64().unwrap() > 31.0);
        assert!(p0["gLng"].as_f64().unwrap() > 121.0);
        assert_ne!(p0["gLat"].as_f64().unwrap(), 31.2304, "应该是转换后的坐标");
        assert_eq!(p0["coorType"], "gcj02");

        // 关键字段齐全
        for k in [
            "avgSpeed", "flag", "id", "locType", "queueNum", "radius", "speed",
            "state", "steps", "totalDis", "totalTime", "type", "validDis", "validTime",
        ] {
            assert!(p0.get(k).is_some(), "缺字段 {k}");
        }

        // 起点与终点的 type
        assert_eq!(arr[0]["type"], 0, "起点 type=0");
        assert_eq!(arr[2]["type"], 6, "终点 type=6");

        // 累计量递增
        let d0 = arr[0]["totalDis"].as_f64().unwrap();
        let d2 = arr[2]["totalDis"].as_f64().unwrap();
        assert!(d2 > d0, "累计里程要递增");

        // speed 是 km/h（步行 5km/h 量级，不是 1.4）
        let sp = arr[2]["speed"].as_f64().unwrap();
        assert!(sp > 1.0 && sp < 30.0, "speed 应在 km/h 量级，实际 {sp}");
    }

    #[test]
    fn point_types_cover_realistic_mix() {
        let n = 200;
        let pts: Vec<TrackPoint> = (0..n)
            .map(|i| tp(31.2304 + i as f64 * 1e-5, 121.4737, 1_790_000_000_000 + i as i64 * 5000))
            .collect();
        let p0 = params();
        let sim = test_sim(&p0, &pts);
        let sub = build(&p0, &pts, &[], &sim).unwrap();
        let arr: Vec<serde_json::Value> =
            serde_json::from_str(sub.save_record["allLocJson"].as_str().unwrap()).unwrap();

        let mut types = std::collections::HashSet::new();
        for p in &arr {
            types.insert(p["type"].as_i64().unwrap());
        }
        // 不该只有一种类型
        assert!(types.len() >= 2, "点位类型要有变化，实际 {types:?}");
        // 不该出现异常点 -1（那是 GPS 失效，故意不生成）
        assert!(!types.contains(&-1), "不生成异常点");
    }
}

#[cfg(test)]
mod single_source_tests {
    use super::tests::test_sim;
    use super::*;
    use crate::track::TrackPoint;

    fn params() -> SubmitParams {
        SubmitParams {
            uid: "1".into(),
            unid: "1".into(),
            uuid: "00000000-0000-4000-8000-000000000000".into(),
            sport_type: 1,
            total_distance_m: 2000,
            total_time_sec: 720,
            total_steps: 1800,
            start_time_ms: 1_790_000_000_000,
            stop_time_ms: 1_790_000_720_000,
            sel_distance_m: 2000,
            sel_run_time_sec: 720,
            policy: "0".into(),
            run_area_id: 0,
            geo_fences_json: "[]".into(),
            total_ascent: 0,
        }
    }

    fn track() -> Vec<TrackPoint> {
        let n = 400;
        (0..n)
            .map(|i| TrackPoint {
                lat: 31.2304 + i as f64 * 5.0 / 111_320.0,
                lon: 121.4737,
                time_ms: 1_790_000_000_000 + i as i64 * 1800,
            })
            .collect()
    }

    /// 报文里的分桶必须和 simulate() 产出的**逐项相等**。
    ///
    /// 这是全项目最关键的一条不变量。之前 build() 里拿 make_jitter(amp=1.0)
    /// 重新生成了一套桶写进报文，而自检校验的是 sim.step_buckets、OBS 明细
    /// 传的也是 sim.step_buckets——自检守 A、提交发 B、明细传 A，
    /// 所有反检测设计都作用在了发不出去的那份数据上。
    #[test]
    fn payload_buckets_equal_sim_buckets() {
        let p = params();
        let t = track();
        let sim = test_sim(&p, &t);

        let sub = build(&p, &t, &[], &sim).expect("build");
        let body = &sub.save_record;

        // 报文里是数组（不是字符串化的 JSON）
        let sent_steps = body["stepsPerTenSec"]
            .as_array()
            .expect("stepsPerTenSec 必须是数组");
        assert_eq!(
            sent_steps.len(),
            sim.step_buckets.len(),
            "桶数不一致：报文 {} vs sim {}",
            sent_steps.len(),
            sim.step_buckets.len()
        );
        for (i, (sent, want)) in sent_steps.iter().zip(sim.step_buckets.iter()).enumerate() {
            let v = sent
                .get("stepsNum")
                .or_else(|| sent.get("steps"))
                .and_then(|x| x.as_i64());
            if let Some(v) = v {
                assert_eq!(
                    v, want.steps_num as i64,
                    "第 {i} 桶步数不一致：报文 {v} vs sim {}",
                    want.steps_num
                );
            }
        }

        // 速度桶同理
        let sent_speed = body["speedPerTenSec"]
            .as_array()
            .expect("speedPerTenSec 必须是数组");
        assert_eq!(sent_speed.len(), sim.speed_buckets.len());
    }

    /// 报文里不许出现 0 步的桶——10 秒 0 步等于静止，服务端判断点。
    #[test]
    fn payload_has_no_empty_step_bucket() {
        let p = params();
        let t = track();
        let sim = test_sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).expect("build");
        let sent = sub.save_record["stepsPerTenSec"].as_array().unwrap();
        for (i, b) in sent.iter().enumerate() {
            let v = b
                .get("stepsNum")
                .or_else(|| b.get("steps"))
                .and_then(|x| x.as_i64())
                .unwrap_or(-1);
            assert!(v > 0, "第 {i} 桶步数是 {v}，会出现静止断点");
        }
    }
}

#[cfg(test)]
mod verify_roundtrip_tests {
    use super::tests::test_sim;
    use super::*;

    fn params() -> SubmitParams {
        SubmitParams {
            uid: "1".into(),
            unid: "1".into(),
            uuid: "00000000-0000-4000-8000-000000000000".into(),
            sport_type: 1,
            total_distance_m: 2000,
            total_time_sec: 720,
            total_steps: 1800,
            start_time_ms: 1_790_000_000_000,
            stop_time_ms: 1_790_000_720_000,
            sel_distance_m: 2000,
            sel_run_time_sec: 720,
            policy: "0".into(),
            run_area_id: 0,
            geo_fences_json: "[]".into(),
            total_ascent: 0,
        }
    }

    fn track() -> Vec<TrackPoint> {
        (0..400)
            .map(|i| TrackPoint {
                lat: 31.2304 + i as f64 * 5.0 / 111_320.0,
                lon: 121.4737,
                time_ms: 1_790_000_000_000 + i as i64 * 1800,
            })
            .collect()
    }

    /// 报文 → 反解 → 视图，桶必须原样回来。
    /// 这条保证 preflight 校验的对象就是发出去的那份。
    #[test]
    fn verify_view_roundtrips_buckets() {
        let p = params();
        let t = track();
        let sim = test_sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let v = verify_view(&sub.save_record).unwrap();

        assert_eq!(v.step_buckets.len(), sim.step_buckets.len());
        for (a, b) in v.step_buckets.iter().zip(sim.step_buckets.iter()) {
            assert_eq!(a.steps_num, b.steps_num, "反解后步数桶变了");
        }
        assert_eq!(v.point_count, t.len());
        assert_eq!(v.total_steps, sim.total_steps);
    }

    /// 反解出来的坐标必须是 BD-09，并且和原轨迹对得上（±1m）。
    #[test]
    fn verify_view_roundtrips_coords() {
        let p = params();
        let t = track();
        let sim = test_sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let v = verify_view(&sub.save_record).unwrap();
        assert_eq!(v.points.len(), t.len());
        for (got, want) in v.points.iter().zip(t.iter()) {
            let d = crate::track::geo::distance((got.lat, got.lon), (want.lat, want.lon));
            assert!(d < 1.0, "第 {} 点偏移 {d:.2}m（BD→GCJ→BD 往返误差过大）", got.lat);
            // 报文 totalTime 的单位是**秒**，毫秒精度在这里必然丢失。
            // 这也意味着采样间隔必须 ≥1 秒，否则反解后会出现时间戳重复的
            // 0 时长段（RunParams::validate 里有对应的下限检查）。
            assert!(
                (got.time_ms - want.time_ms).abs() < 1000,
                "时间戳反解误差超过 1 秒：got {} want {}",
                got.time_ms,
                want.time_ms
            );
        }
    }

    /// 时间轴端点必须精确等于报文的 startTime/stopTime。
    #[test]
    fn verify_view_time_matches_payload() {
        let p = params();
        let t = track();
        let sim = test_sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let v = verify_view(&sub.save_record).unwrap();
        assert_eq!(v.start_time_ms, p.start_time_ms);
        assert!(v.stop_time_ms > v.start_time_ms);
    }
}

#[cfg(test)]
mod step_distance_tests {
    use super::tests::test_sim;
    use super::*;

    /// stepDistance 必须是**局部**步幅（1.0~1.5m 量级），不是累积平均。
    #[test]
    fn step_distance_is_local_stride() {
        let p = SubmitParams {
            uid: "1".into(), unid: "1".into(),
            uuid: "00000000-0000-4000-8000-000000000000".into(),
            sport_type: 1, total_distance_m: 2000, total_time_sec: 720,
            total_steps: 1800, start_time_ms: 1_790_000_000_000,
            stop_time_ms: 1_790_000_720_000, sel_distance_m: 2000,
            sel_run_time_sec: 720, policy: "0".into(), run_area_id: 0,
            geo_fences_json: "[]".into(), total_ascent: 0,
        };
        let t: Vec<TrackPoint> = (0..400).map(|i| TrackPoint {
            lat: 31.2304 + i as f64 * 5.0 / 111_320.0,
            lon: 121.4737,
            time_ms: 1_790_000_000_000 + i as i64 * 1800,
        }).collect();
        let sim = test_sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let arr: Vec<serde_json::Value> =
            serde_json::from_str(sub.save_record["allLocJson"].as_str().unwrap()).unwrap();

        let vals: Vec<f64> = arr.iter()
            .filter_map(|q| q.get("stepDistance").and_then(|v| v.as_f64()))
            .collect();
        assert!(!vals.is_empty(), "stepDistance 全缺失");

        // 真实单步步幅 0.8~1.8m。旧实现是「累计里程/总步数」，
        // 前段趋近 0、末段趋近平均值，中间的会远小于 1m。
        let mid: Vec<f64> = vals.iter().skip(vals.len()/4).take(vals.len()/2)
            .copied().filter(|&v| v > 0.0).collect();
        assert!(!mid.is_empty());
        let avg = mid.iter().sum::<f64>() / mid.len() as f64;
        assert!(avg > 0.6 && avg < 2.5, "局部步幅均值 {avg:.2}m 不在合理区间");
        for (i, &v) in mid.iter().enumerate().take(20) {
            assert!(v > 0.3 && v < 4.0, "第{i}个局部步幅 {v}m 异常");
        }
    }

    // ---- 官方折算：卡路里 / 功率 ----

    #[test]
    fn met_follows_official_segments() {
        // 官方是三段拟合而非连续函数：阈值 10 和 12 处各有一个约 1 MET 的
        // 台阶（33/10+4.1=7.4 对 102/10−3.7=6.5）。这里固定住三段各自的
        // 取值与段内单调性，防止有人「顺手改成连续函数」。
        assert!((met_for_pace(6.0) - 9.6).abs() < 0.01, "p<10 段公式变了");
        assert!((met_for_pace(10.0) - 6.5).abs() < 0.01, "10≤p<12 段公式变了");
        assert!((met_for_pace(12.0) - 3.8).abs() < 0.01, "p≥12 段公式变了");
        // 段内单调：配速越快（p 越小）MET 越高
        assert!(met_for_pace(5.0) > met_for_pace(9.0));
        assert!(met_for_pace(10.5) > met_for_pace(11.5));
        assert!(met_for_pace(12.0) > met_for_pace(20.0));
        // 非法输入不产生 NaN/负值
        assert_eq!(met_for_pace(0.0), 0.0);
        assert_eq!(met_for_pace(-1.0), 0.0);
        assert_eq!(met_for_pace(f64::NAN), 0.0);
    }

    #[test]
    fn calorie_uses_met_not_fixed_per_km() {
        // 同一个 3km：快跑 18min vs 慢跑 36min。固定 62 kcal/km 会给出同一个
        // 186 kcal；MET 折算下两者必须不同，且慢跑单位时间消耗更低。
        let fast = calorie_for(3000.0, 18 * 60, DEFAULT_WEIGHT_KG);
        let slow = calorie_for(3000.0, 36 * 60, DEFAULT_WEIGHT_KG);
        assert!(fast > 0 && slow > 0);
        assert_ne!(fast, slow, "快慢跑卡路里相同，说明还在用固定 kcal/km");
        let fast_per_h = fast as f64 / 0.3;
        let slow_per_h = slow as f64 / 0.6;
        assert!(fast_per_h > slow_per_h, "快跑单位时间消耗应更高");
        // 量级检查：68kg 跑 3km 大约 150~260 kcal
        assert!((140..=280).contains(&fast), "3km 卡路里 {fast} 量级异常");
    }

    #[test]
    fn avg_power_is_nonzero_and_scales_with_speed() {
        let slow = avg_power_for(3000.0, 1200, DEFAULT_WEIGHT_KG);
        let fast = avg_power_for(3000.0, 600, DEFAULT_WEIGHT_KG);
        assert!(slow > 0, "慢跑功率不应为 0");
        assert!(fast > slow, "速度翻倍功率应更高");
        // 1.1 * 68 * 2.5 m/s = 187 W
        assert_eq!(avg_power_for(2500.0, 1000, DEFAULT_WEIGHT_KG), 187,
            "1.1*68*2.5=187W，且必须整数：服务端整型会截断，发小数就是 10501");
    }

    // ---- 圈与爬升 ----

    fn rich_points(n: usize, total_m: f64, total_s: i64) -> Vec<TrackPoint> {
        let lat0 = 31.2304;
        (0..n).map(|i| TrackPoint {
            lat: lat0 + (i as f64) * (total_m / (n - 1) as f64) / 111_320.0,
            lon: 121.4737,
            time_ms: 1_790_000_000_000 + (i as i64) * (total_s * 1000 / (n - 1) as i64),
        }).collect()
    }

    #[test]
    fn laps_split_every_1000m() {
        let p = SubmitParams {
            uid: "1".into(), unid: "1".into(),
            uuid: "00000000-0000-4000-8000-000000000002".into(),
            sport_type: 1, total_distance_m: 2400, total_time_sec: 840,
            total_steps: 2400, start_time_ms: 1_790_000_000_000,
            stop_time_ms: 1_790_000_840_000, sel_distance_m: 2000,
            sel_run_time_sec: 700, policy: "0".into(), run_area_id: 0,
            geo_fences_json: "[]".into(), total_ascent: 0,
        };
        let t = rich_points(400, 2400.0, 840);
        let sim = test_sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let arr: Vec<serde_json::Value> =
            serde_json::from_str(sub.save_record["allLocJson"].as_str().unwrap()).unwrap();
        let laps = build_laps_from_points(&arr, p.start_time_ms);

        assert_eq!(laps.len(), 3, "2400m 应切 3 圈，实际 {}", laps.len());
        let sum_d: f64 = laps.iter().map(|l| l["distance"].as_f64().unwrap()).sum();
        assert!((sum_d - 2400.0).abs() < 0.05, "圈里程和 {sum_d} != 2400");
        assert_eq!(laps[0]["isFullLap"], true);
        assert_eq!(laps[1]["isFullLap"], true);
        assert_eq!(laps[2]["isFullLap"], false, "末圈必须标 isFullLap=false");
        // 圈时长累加必须等于总时长（切圈点按比例 round，允许每圈 1s 误差）
        let sum_t: i64 = laps.iter().map(|l| l["duration"].as_i64().unwrap()).sum();
        assert!((sum_t - 840).abs() <= laps.len() as i64,
            "圈时长和 {sum_t} 偏离总时长 840 超过切圈舍入误差");
        // 圈步数累加必须等于总步数分配后的末值
        let last_steps = arr.last().unwrap()["steps"].as_i64().unwrap();
        let sum_steps: i64 = laps.iter().map(|l| l["step"].as_i64().unwrap()).sum();
        assert_eq!(sum_steps, last_steps);
        assert_eq!(laps[0]["lapIndex"], 1);
        assert_eq!(laps[2]["lapIndex"], 3);
    }

    #[test]
    fn total_ascent_follows_bd_a_series() {
        let p = SubmitParams {
            uid: "1".into(), unid: "1".into(),
            uuid: "00000000-0000-4000-8000-000000000003".into(),
            sport_type: 1, total_distance_m: 2000, total_time_sec: 720,
            total_steps: 2000, start_time_ms: 1_790_000_000_000,
            stop_time_ms: 1_790_000_720_000, sel_distance_m: 2000,
            sel_run_time_sec: 720, policy: "0".into(), run_area_id: 0,
            geo_fences_json: "[]".into(), total_ascent: 0,
        };
        let t = rich_points(300, 2000.0, 720);
        let sim = test_sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let arr: Vec<serde_json::Value> =
            serde_json::from_str(sub.save_record["allLocJson"].as_str().unwrap()).unwrap();

        let expect = total_ascent_from_points(&arr);
        let declared = sub.save_record["totalAscent"].as_i64().unwrap();
        assert_eq!(declared, expect as i64, "报文的 totalAscent 与点位反算值不一致");
        // pace 的地点派生海拔自带缓坡，2000m 上必须有非零爬升
        assert!(expect > 0, "2000m 轨迹爬升为 0，海拔序列是平的");
        // 圈上的 elevationGain 之和要与总爬升同量级（差值来自切圈插值）
        let laps = build_laps_from_points(&arr, p.start_time_ms);
        let lap_sum: f64 = laps.iter().map(|l| l["elevationGain"].as_f64().unwrap()).sum();
        // 总爬升 round 到整米、圈爬升保留 2 位且切圈点按比例分摊，
        // 两者只能同量级（差 < 圈数 米），不要求逐分相等。
        assert!((lap_sum - expect as f64).abs() <= laps.len() as f64 + 0.5,
            "圈爬升和 {lap_sum} 与总爬升 {expect} 差太多");
        // 上报值也必须出现在签名参与字段里（否则服务端算签名会不一致）
        assert!(sub.canonical.contains(&format!("totalAscent={expect}")),
            "签名串里的 totalAscent 不是点位反算值: {}", sub.canonical);
        assert!(sub.canonical.contains("avgPower="),
            "签名串缺 avgPower");
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;

    fn params() -> SubmitParams {
        SubmitParams {
            uid: "1".into(),
            unid: "2".into(),
            uuid: "00000000-0000-4000-8000-000000000009".into(),
            sport_type: 1,
            total_distance_m: 2000,
            total_time_sec: 720,
            total_steps: 2000,
            start_time_ms: 1_790_000_000_000,
            stop_time_ms: 1_790_000_720_000,
            sel_distance_m: 2000,
            sel_run_time_sec: 720,
            policy: "0".into(),
            run_area_id: 0,
            geo_fences_json: "[]".into(),
            total_ascent: 0,
        }
    }

    fn sim(p: &SubmitParams, pts: &[TrackPoint]) -> crate::track::SimResult {
        crate::track::SimResult {
            speeds: vec![2.8; pts.len()],
            cadences: vec![160.0; pts.len()],
            strides: vec![1.05; pts.len()],
            total_steps: p.total_steps,
            avg_cadence: 160.0,
            speed_buckets: Vec::new(),
            step_buckets: Vec::new(),
            actual_length_m: p.total_distance_m as f64,
        }
    }

    fn track() -> Vec<TrackPoint> {
        let n = 300;
        (0..n)
            .map(|i| TrackPoint {
                lat: 31.2304 + i as f64 * 2000.0 / (n - 1) as f64 / 111_320.0,
                lon: 121.4737,
                time_ms: 1_790_000_000_000 + i as i64 * 720_000 / (n - 1) as i64,
            })
            .collect()
    }

    #[test]
    fn audit_passes_on_freshly_built_body() {
        let p = params();
        let t = track();
        let sim = sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let probs = audit_derived(&sub.save_record);
        assert!(probs.is_empty(), "新建报文不该有派生字段问题: {probs:?}");
    }

    #[test]
    fn audit_catches_constant_fallbacks() {
        let p = params();
        let t = track();
        let sim = sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let mut body = sub.save_record.clone();
        // 退回三种常量填法
        body["totalAscent"] = json!(0);
        body["calorie"] = json!(124); // 2km × 62
        body["avgPower"] = json!(0);
        let probs = audit_derived(&body);
        assert!(probs.iter().any(|s| s.contains("totalAscent")), "{probs:?}");
        assert!(probs.iter().any(|s| s.contains("calorie")), "{probs:?}");
        assert!(probs.iter().any(|s| s.contains("avgPower")), "{probs:?}");
    }

    #[test]
    fn audit_catches_wrong_speed_unit() {
        let p = params();
        let t = track();
        let sim = sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let mut body = sub.save_record.clone();
        // 把 m/s 当 speed 发（常见错误：速度 ×1000 而不是配速 ×1000）
        body["speed"] = json!((2000.0 / 720.0 * 1000.0) as i64);
        let probs = audit_derived(&body);
        assert!(probs.iter().any(|s| s.contains("speed")), "{probs:?}");
    }

    /// 签名串里的数值必须是**整数文本**。
    ///
    /// 10501「验证签名失败」踩过一次：avgPower 按 f64 发 187.5，服务端
    /// avgPower 是整型（参考实现 RunDetail.avg_power: i64），反序列化截断成
    /// 187 后拿它重算签名，而我们拼的串里是 "187.5" —— 两边不同，直接驳回。
    /// body 内部完全自洽，本地 audit 也查不出来，只有真发一次才暴露。
    #[test]
    fn signature_numeric_fields_are_integers() {
        let p = params();
        let t = track();
        let sim = sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();

        // 这些是字符串/标识字段，允许非数字
        let text_keys = [
            "address", "geeToken", "goalId", "policy", "uuid",
            "selectedUnid", "uid",
        ];
        for kv in sub.canonical.split('&') {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            if text_keys.contains(&k) {
                continue;
            }
            assert!(
                !v.contains('.'),
                "签名串字段 {k}={v} 带小数点：服务端整型会截断，重算签名与这里不一致（10501）"
            );
        }
    }

    #[test]
    fn avg_power_is_integer_watt() {
        let p = params();
        let t = track();
        let sim = sim(&p, &t);
        let sub = build(&p, &t, &[], &sim).unwrap();
        let v = &sub.save_record["avgPower"];
        assert!(v.is_i64() || v.is_u64(), "avgPower 必须是 JSON 整数，实际 {v}");
        assert!(v.as_i64().unwrap() > 0, "avgPower 不应为 0");
    }
}
