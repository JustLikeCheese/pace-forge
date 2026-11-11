//! 真实路网：从 OpenStreetMap 拉围栏内的道路，在图上路由。
//!
//! 之前是纯几何蛇形填充，完全不看地形——围栏里有湖、有河、有楼，轨迹照样
//! 直线穿过去。地图上一眼就看出不是人跑的，服务端也可能按轨迹异常判掉。
//!
//! 这里走数据源：Overpass API 拉 bbox 内的 `highway=*`（人行道、小路、校园
//! 道路、台阶都算），栅格化成图，然后沿图走。

use super::geo::{distance, m_per_deg_lat, m_per_deg_lng};
use serde_json::Value;

/// Overpass 端点。主站经常 502/504，准备了两个镜像轮询。
const OVERPASS_HOSTS: [&str; 3] = [
    "https://overpass-api.de/api/interpreter",
    "https://overpass.kumi.systems/api/interpreter",
    "https://overpass.private.coffee/api/interpreter",
];

/// 打算走的路类型。excluding motorway/trunk：校园里没有，且人不能跑。
/// 包括 footway/path/pedestrian/steps/track/service/residential/unclassified/living_street/cycleway。
const WALKABLE: &str = "footway|path|pedestrian|steps|track|service|residential|unclassified|living_street|cycleway|tertiary|secondary";

/// 一段路（OSM way 的几何）。
#[derive(Debug, Clone)]
pub struct RoadSegment {
    pub points: Vec<(f64, f64)>,
    pub kind: String,
}

/// 拉来的路网。
#[derive(Debug, Clone, Default)]
pub struct RoadNet {
    pub segments: Vec<RoadSegment>,
    /// 数据来源描述，用于日志。
    pub source: String,
}

impl RoadNet {
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// 只保留落在围栏内的路段。
    ///
    /// OSM 里的市政道路（residential/tertiary）常常横穿围栏边界，直接走会
    /// 跑出学校。裁剪后只留围栏内部的，剩下的长度不够时调用方自己决定
    /// 退回几何规划。
    pub fn clip_to_fence(&self, fence: &[(f64, f64)]) -> RoadNet {
        use super::fence_route::contains;
        let mut segments = Vec::new();
        for seg in &self.segments {
            // 把连续的「在围栏内」的点切成独立段
            let mut cur: Vec<(f64, f64)> = Vec::new();
            for &p in &seg.points {
                if contains(fence, p.0, p.1) {
                    cur.push(p);
                } else if cur.len() >= 2 {
                    segments.push(RoadSegment { points: std::mem::take(&mut cur), kind: seg.kind.clone() });
                } else {
                    cur.clear();
                }
            }
            if cur.len() >= 2 {
                segments.push(RoadSegment { points: cur, kind: seg.kind.clone() });
            }
        }
        RoadNet {
            source: format!("{} · 围栏内 {} 段", self.source, segments.len()),
            segments,
        }
    }

    /// 总长度（米）。
    pub fn total_length_m(&self) -> f64 {
        self.segments
            .iter()
            .map(|s| s.points.windows(2).map(|w| distance(w[0], w[1])).sum::<f64>())
            .sum()
    }
}

/// 从 Overpass 拉 bbox 内的可跑路网。
///
/// 失败（网络、超时、限流）返回 Err，调用方退回几何规划——不能因为拿不到
/// 路网就完全不能跑。
pub fn fetch(bbox: (f64, f64, f64, f64)) -> Result<RoadNet, String> {
    // 先看本地缓存。Overpass 主站经常 502/504，镜像也不稳，
    // 一次拉取要几十秒，缓存起来按 bbox 网格复用。
    if let Some(net) = cache_get(bbox) {
        return Ok(net);
    }
    let (min_lat, min_lon, max_lat, max_lon) = bbox;
    if !(min_lat < max_lat && min_lon < max_lon) {
        return Err("bbox 非法".into());
    }
    let query = format!(
        "[out:json][timeout:40];\
         (way[\"highway\"~\"^({WALKABLE})$\"]({min_lat},{min_lon},{max_lat},{max_lon});\
          way[\"leisure\"=\"track\"]({min_lat},{min_lon},{max_lat},{max_lon}););\
         out geom;"
    );

    let body = format!("data={}", urlencode(&query));
    let mut text = String::new();
    let mut last_err = String::new();
    for host in OVERPASS_HOSTS {
        let resp = ureq::post(host)
            .set("User-Agent", "pace-forge/0.1 (route planning)")
            .set("Content-Type", "application/x-www-form-urlencoded")
            .timeout(std::time::Duration::from_secs(70))
            .send_string(&body);
        match resp {
            Ok(r) => match r.into_string() {
                Ok(t) if t.contains("\"elements\"") => {
                    text = t;
                    break;
                }
                Ok(_) => last_err = format!("{host} 返回内容异常"),
                Err(e) => last_err = format!("{host} 读取失败: {e}"),
            },
            Err(e) => last_err = format!("{host} 请求失败: {e}"),
        }
    }
    if text.is_empty() {
        return Err(format!("所有 Overpass 端点都失败（最后一次: {last_err}）"));
    }

    let v: Value = serde_json::from_str(&text).map_err(|e| format!("路网 JSON 解析失败: {e}"))?;
    let elements = v
        .get("elements")
        .and_then(|e| e.as_array())
        .ok_or("路网响应缺少 elements")?;

    let mut segments = Vec::new();
    for el in elements {
        let Some(geom) = el.get("geometry").and_then(|g| g.as_array()) else {
            continue;
        };
        let pts: Vec<(f64, f64)> = geom
            .iter()
            .filter_map(|p| {
                let lat = p.get("lat")?.as_f64()?;
                let lon = p.get("lon")?.as_f64()?;
                Some((lat, lon))
            })
            .collect();
        if pts.len() < 2 {
            continue;
        }
        let kind = el
            .get("tags")
            .and_then(|t| t.get("highway"))
            .and_then(|h| h.as_str())
            .or_else(|| {
                el.get("tags")
                    .and_then(|t| t.get("leisure"))
                    .and_then(|h| h.as_str())
            })
            .unwrap_or("unknown")
            .to_string();
        segments.push(RoadSegment { points: pts, kind });
    }

    if segments.is_empty() {
        return Err("该区域 OSM 里没有可用道路".into());
    }

    let net = RoadNet {
        source: format!("OSM/Overpass · {} 段", segments.len()),
        segments,
    };
    cache_put(bbox, &net);
    Ok(net)
}

/// 本地文件缓存，按 bbox 保留 3 位小数（约 100m 精度）。
fn cache_path(bbox: (f64, f64, f64, f64)) -> std::path::PathBuf {
    let key = format!(
        "{:.3}_{:.3}_{:.3}_{:.3}",
        bbox.0, bbox.1, bbox.2, bbox.3
    );
    let dir = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(dir)
        .join(".pace-forge")
        .join(format!("roads-{key}.json"))
}

fn cache_get(bbox: (f64, f64, f64, f64)) -> Option<RoadNet> {
    let path = cache_path(bbox);
    let meta = std::fs::metadata(&path).ok()?;
    // 超过 7 天就重拉
    if let Ok(age) = meta.modified().and_then(|t| t.elapsed().map_err(std::io::Error::other)) {
        if age.as_secs() > 7 * 86400 {
            return None;
        }
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let arr = v.get("segments")?.as_array()?;
    let mut segments = Vec::new();
    for s in arr {
        let pts: Vec<(f64, f64)> = s
            .get("points")?
            .as_array()?
            .iter()
            .filter_map(|p| {
                let a = p.as_array()?;
                Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
            })
            .collect();
        if pts.len() >= 2 {
            segments.push(RoadSegment {
                points: pts,
                kind: s.get("kind").and_then(|k| k.as_str()).unwrap_or("unknown").into(),
            });
        }
    }
    if segments.is_empty() {
        return None;
    }
    Some(RoadNet {
        source: format!("本地缓存 · {} 段", segments.len()),
        segments,
    })
}

fn cache_put(bbox: (f64, f64, f64, f64), net: &RoadNet) {
    let path = cache_path(bbox);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let segs: Vec<Value> = net
        .segments
        .iter()
        .map(|s| {
            serde_json::json!({
                "kind": s.kind,
                "points": s.points.iter().map(|p| vec![p.0, p.1]).collect::<Vec<_>>(),
            })
        })
        .collect();
    let body = serde_json::json!({ "segments": segs });
    let _ = std::fs::write(&path, body.to_string());
}

/// 拉围栏内的水域（湖、河、池塘）。
///
/// 蛇形填充是纯几何的，不知道哪里有水，轨迹就直接从湖面上穿过去。
/// 拿到水域多边形后，扫描时在这些区域挖洞。
pub fn fetch_water(bbox: (f64, f64, f64, f64)) -> Result<Vec<Vec<(f64, f64)>>, String> {
    if let Some(w) = water_cache_get(bbox) {
        return Ok(w);
    }
    let (min_lat, min_lon, max_lat, max_lon) = bbox;
    let query = format!(
        "[out:json][timeout:30];         (way[\"natural\"=\"water\"]({min_lat},{min_lon},{max_lat},{max_lon});          way[\"waterway\"~\"river|stream|canal\"]({min_lat},{min_lon},{max_lat},{max_lon}););         out geom;"
    );
    let body = format!("data={}", urlencode(&query));
    let mut text = String::new();
    let mut last_err = String::new();
    for host in OVERPASS_HOSTS {
        match ureq::post(host)
            .set("User-Agent", "pace-forge/0.1 (route planning)")
            .set("Content-Type", "application/x-www-form-urlencoded")
            .timeout(std::time::Duration::from_secs(60))
            .send_string(&body)
        {
            Ok(r) => match r.into_string() {
                Ok(t) if t.contains("\"elements\"") => {
                    text = t;
                    break;
                }
                Ok(_) => last_err = format!("{host} 内容异常"),
                Err(e) => last_err = format!("{host} 读取失败: {e}"),
            },
            Err(e) => last_err = format!("{host} 请求失败: {e}"),
        }
    }
    if text.is_empty() {
        return Err(format!("拉水域失败: {last_err}"));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("水域 JSON 解析失败: {e}"))?;
    let mut polys = Vec::new();
    if let Some(els) = v.get("elements").and_then(|e| e.as_array()) {
        for el in els {
            let Some(geom) = el.get("geometry").and_then(|g| g.as_array()) else {
                continue;
            };
            let pts: Vec<(f64, f64)> = geom
                .iter()
                .filter_map(|p| Some((p.get("lat")?.as_f64()?, p.get("lon")?.as_f64()?)))
                .collect();
            if pts.len() >= 3 {
                polys.push(pts);
            }
        }
    }
    water_cache_put(bbox, &polys);
    Ok(polys)
}


/// 本地手绘禁区（Web 地图上圈出来的楼、施工区、封路）。
///
/// OSM 在国内校园几乎空白，Overpass 拉不到建筑轮廓，只能人工标一次。
/// 存一个文件，所有账号、所有之后的规划都用它。
pub fn load_local_blockers() -> Vec<Vec<(f64, f64)>> {
    let path = blockers_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(arr) = v.get("polys").and_then(|p| p.as_array()) {
        for poly in arr {
            let Some(pts) = poly.as_array() else { continue };
            let pts: Vec<(f64, f64)> = pts
                .iter()
                .filter_map(|p| {
                    let a = p.as_array()?;
                    Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
                })
                .collect();
            if pts.len() >= 3 {
                out.push(pts);
            }
        }
    }
    out
}

/// 保存本地禁区。
pub fn save_local_blockers(polys: &[Vec<(f64, f64)>]) -> Result<(), String> {
    let path = blockers_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败: {e}"))?;
    }
    let body = serde_json::json!({
        "polys": polys
            .iter()
            .map(|p| p.iter().map(|q| vec![q.0, q.1]).collect::<Vec<_>>())
            .collect::<Vec<_>>()
    });
    std::fs::write(&path, body.to_string()).map_err(|e| format!("写文件失败: {e}"))
}

fn blockers_path() -> std::path::PathBuf {
    let dir = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(dir).join(".pace-forge").join("blockers.json")
}

/// 拉围栏内的建筑物轮廓。
///
/// 路网数据不足时的兜底是「把打卡点按顺序用弧线串起来」——纯几何，不看楼。
/// 实测校区里 5 个打卡点分属不同楼群，串出来的弧直接从教学楼、宿舍楼中间
/// 穿过去，地图上一眼就假。拿到轮廓后由 fence_route::avoid_blockers 做避让。
pub fn fetch_buildings(bbox: (f64, f64, f64, f64)) -> Result<Vec<Vec<(f64, f64)>>, String> {
    if let Some(b) = poly_cache_get(bbox, "buildings") {
        return Ok(b);
    }
    let (min_lat, min_lon, max_lat, max_lon) = bbox;
    let query = format!(
        "[out:json][timeout:30];(way[\"building\"]({min_lat},{min_lon},{max_lat},{max_lon}););out geom;"
    );
    let polys = fetch_polys(&query, "建筑")?;
    poly_cache_put(bbox, "buildings", &polys);
    Ok(polys)
}

/// Overpass 拉 polygon 的公共部分（水体、建筑共用）。
fn fetch_polys(query: &str, what: &str) -> Result<Vec<Vec<(f64, f64)>>, String> {
    let body = format!("data={}", urlencode(query));
    let mut text = String::new();
    let mut last_err = String::new();
    for host in OVERPASS_HOSTS {
        match ureq::post(host)
            .set("User-Agent", "pace-forge/0.1 (route planning)")
            .set("Content-Type", "application/x-www-form-urlencoded")
            .timeout(std::time::Duration::from_secs(60))
            .send_string(&body)
        {
            Ok(r) => match r.into_string() {
                Ok(t) if t.contains("\"elements\"") => {
                    text = t;
                    break;
                }
                Ok(_) => last_err = format!("{host} 内容异常"),
                Err(e) => last_err = format!("{host} 读取失败: {e}"),
            },
            Err(e) => last_err = format!("{host} 请求失败: {e}"),
        }
    }
    if text.is_empty() {
        return Err(format!("拉{what}失败: {last_err}"));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{what} JSON 解析失败: {e}"))?;
    let mut polys = Vec::new();
    if let Some(els) = v.get("elements").and_then(|e| e.as_array()) {
        for el in els {
            let Some(geom) = el.get("geometry").and_then(|g| g.as_array()) else {
                continue;
            };
            let pts: Vec<(f64, f64)> = geom
                .iter()
                .filter_map(|p| Some((p.get("lat")?.as_f64()?, p.get("lon")?.as_f64()?)))
                .collect();
            if pts.len() >= 3 {
                polys.push(pts);
            }
        }
    }
    Ok(polys)
}

fn poly_cache_path(bbox: (f64, f64, f64, f64), kind: &str) -> std::path::PathBuf {
    let key = format!("{:.3}_{:.3}_{:.3}_{:.3}", bbox.0, bbox.1, bbox.2, bbox.3);
    let dir = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(dir)
        .join(".pace-forge")
        .join(format!("{kind}-{key}.json"))
}

fn poly_cache_get(bbox: (f64, f64, f64, f64), kind: &str) -> Option<Vec<Vec<(f64, f64)>>> {
    let path = poly_cache_path(bbox, kind);
    let meta = std::fs::metadata(&path).ok()?;
    if let Ok(age) = meta.modified().and_then(|t| t.elapsed().map_err(std::io::Error::other)) {
        if age.as_secs() > 30 * 86400 {
            return None;
        }
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let mut out = Vec::new();
    for poly in v.get("polys")?.as_array()? {
        let pts: Vec<(f64, f64)> = poly
            .as_array()?
            .iter()
            .filter_map(|p| {
                let a = p.as_array()?;
                Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
            })
            .collect();
        if pts.len() >= 3 {
            out.push(pts);
        }
    }
    Some(out)
}

fn poly_cache_put(bbox: (f64, f64, f64, f64), kind: &str, polys: &[Vec<(f64, f64)>]) {
    let path = poly_cache_path(bbox, kind);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = serde_json::json!({
        "polys": polys.iter().map(|p| p.iter().map(|q| vec![q.0, q.1]).collect::<Vec<_>>()).collect::<Vec<_>>()
    });
    let _ = std::fs::write(&path, body.to_string());
}

fn water_cache_path(bbox: (f64, f64, f64, f64)) -> std::path::PathBuf {
    let key = format!("{:.3}_{:.3}_{:.3}_{:.3}", bbox.0, bbox.1, bbox.2, bbox.3);
    let dir = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(dir)
        .join(".pace-forge")
        .join(format!("water-{key}.json"))
}

fn water_cache_get(bbox: (f64, f64, f64, f64)) -> Option<Vec<Vec<(f64, f64)>>> {
    let path = water_cache_path(bbox);
    let meta = std::fs::metadata(&path).ok()?;
    if let Ok(age) = meta.modified().and_then(|t| t.elapsed().map_err(std::io::Error::other)) {
        if age.as_secs() > 30 * 86400 {
            return None;
        }
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let mut out = Vec::new();
    for poly in v.get("polys")?.as_array()? {
        let pts: Vec<(f64, f64)> = poly
            .as_array()?
            .iter()
            .filter_map(|p| {
                let a = p.as_array()?;
                Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
            })
            .collect();
        if pts.len() >= 3 {
            out.push(pts);
        }
    }
    Some(out)
}

fn water_cache_put(bbox: (f64, f64, f64, f64), polys: &[Vec<(f64, f64)>]) {
    let path = water_cache_path(bbox);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = serde_json::json!({
        "polys": polys.iter().map(|p| p.iter().map(|q| vec![q.0, q.1]).collect::<Vec<_>>()).collect::<Vec<_>>()
    });
    let _ = std::fs::write(&path, body.to_string());
}

/// 最小 urlencode（只处理查询里会出现的字符）。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 一条走出来的路线。
#[derive(Debug, Clone)]
pub struct RoadRoute {
    pub points: Vec<(f64, f64)>,
    pub length_m: f64,
    /// 走过的路段数。
    pub legs: usize,
}

/// 在路网上找一条长度接近 `want_m` 的路线，且经过所有 `must` 点。
///
/// 策略是贪心最近邻串路：从最近的段出发，每次挑「离当前位置最近、且方向
/// 别立刻折返」的一段接上，走够里程就停。不是最优解，但保证全程在道路上，
/// 不会穿水穿楼。
pub fn route_on_roads(
    net: &RoadNet,
    start_hint: (f64, f64),
    must: &[(f64, f64)],
    want_m: f64,
    seed: u64,
) -> Result<RoadRoute, String> {
    if net.is_empty() {
        return Err("路网为空".into());
    }
    if want_m <= 0.0 {
        return Err("目标里程必须为正".into());
    }

    // 所有段的索引
    let mut pool: Vec<usize> = (0..net.segments.len()).collect();
    // 用 seed 打散，避免每次都走同一条路
    let mut rng_state = seed | 1;
    let mut next_rand = || {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 7;
        rng_state ^= rng_state << 17;
        rng_state
    };
    for i in (1..pool.len()).rev() {
        let j = (next_rand() as usize) % (i + 1);
        pool.swap(i, j);
    }

    // 起点不能直接用 start_hint——那是围栏顶点，可能离路网几百米，
    // 后面「跳过去太远」的判断会把它全程挡掉。先吸附到最近的路点。
    let mut cur = {
        let mut best = start_hint;
        let mut bd = f64::MAX;
        for seg in &net.segments {
            for &p in &seg.points {
                let d = distance(start_hint, p);
                if d < bd {
                    bd = d;
                    best = p;
                }
            }
        }
        best
    };
    let mut points: Vec<(f64, f64)> = vec![cur];
    let mut total = 0.0;
    let mut legs = 0usize;
    let mut used: Vec<bool> = vec![false; net.segments.len()];
    let mut pending: Vec<(f64, f64)> = must.to_vec();

    while total < want_m && legs < net.segments.len() * 3 {
        // 优先去还没走到的打卡点
        let target = if let Some(&p) = pending.first() {
            p
        } else {
            cur
        };

        // 挑一段：优先能接近 target 的，否则离 cur 最近的
        let mut best: Option<(usize, f64, bool)> = None; // (idx, score, reversed)
        for &idx in &pool {
            let seg = &net.segments[idx];
            let (a, b) = (seg.points[0], *seg.points.last().unwrap());
            let da = distance(cur, a);
            let db = distance(cur, b);
            let (near, far, rev) = if da <= db { (a, b, false) } else { (b, a, true) };

            // 这一段能不能帮我们靠近 target
            let helps = distance(far, target) < distance(near, target);
            let penalty = if used[idx] { 30.0 } else { 0.0 };
            let score = distance(cur, near) + penalty - if helps { 400.0 } else { 0.0 };

            if best.as_ref().map(|b| score < b.1).unwrap_or(true) {
                best = Some((idx, score, rev));
            }
        }

        let Some((idx, _, rev)) = best else { break };
        // 离得太远跳过去会撕出直线，跳过
        let seg = &net.segments[idx];
        let pts: Vec<(f64, f64)> = if rev {
            seg.points.iter().rev().copied().collect()
        } else {
            seg.points.clone()
        };
        if distance(cur, pts[0]) > 250.0 {
            // 断开太远，标记这段已用，换下一段
            used[idx] = true;
            legs += 1;
            if legs > net.segments.len() * 2 {
                break;
            }
            continue;
        }

        // 接上这一段
        for &p in pts.iter().skip(1) {
            let d = distance(*points.last().unwrap(), p);
            if total + d > want_m {
                break;
            }
            points.push(p);
            total += d;
        }
        cur = *points.last().unwrap();
        used[idx] = true;
        legs += 1;

        // 打卡点：走过就标记完成
        pending.retain(|&m| {
            !points.iter().any(|p| distance(*p, m) < 30.0)
        });

        if total >= want_m {
            break;
        }
    }

    if points.len() < 2 {
        return Err("路网上没走出有效路线".into());
    }

    Ok(RoadRoute {
        points,
        length_m: total,
        legs,
    })
}

/// 把路线重采样成等间距点。
pub fn resample(path: &[(f64, f64)], step_m: f64) -> Vec<(f64, f64)> {
    if path.len() < 2 || step_m <= 0.0 {
        return path.to_vec();
    }
    let mut out = vec![path[0]];
    let mut carry = 0.0;
    for w in path.windows(2) {
        let d = distance(w[0], w[1]);
        if d < 1e-9 {
            continue;
        }
        let mut pos = step_m - carry;
        while pos <= d {
            let t = pos / d;
            out.push((
                w[0].0 + (w[1].0 - w[0].0) * t,
                w[0].1 + (w[1].1 - w[0].1) * t,
            ));
            pos += step_m;
        }
        carry = d - (pos - step_m);
    }
    out
}

/// bbox 从点集算出来，外扩一点。
pub fn bbox_of(points: &[(f64, f64)], pad_m: f64) -> (f64, f64, f64, f64) {
    if points.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let lat_min = points.iter().map(|p| p.0).fold(f64::MAX, f64::min);
    let lat_max = points.iter().map(|p| p.0).fold(f64::MIN, f64::max);
    let lon_min = points.iter().map(|p| p.1).fold(f64::MAX, f64::min);
    let lon_max = points.iter().map(|p| p.1).fold(f64::MIN, f64::max);
    let mlat = m_per_deg_lat(lat_min).max(1.0);
    let mlng = m_per_deg_lng(lat_min).max(1.0);
    (
        lat_min - pad_m / mlat,
        lon_min - pad_m / mlng,
        lat_max + pad_m / mlat,
        lon_max + pad_m / mlng,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l_shape_net() -> RoadNet {
        // 一个直角路口：东西向 + 南北向各一段
        let lat0 = 31.2304;
        let lon0 = 121.4737;
        let mut segs = vec![RoadSegment {
            points: (0..=20)
                .map(|i| (lat0, lon0 + i as f64 * 10.0 / m_per_deg_lng(lat0)))
                .collect(),
            kind: "footway".into(),
        }];
        segs.push(RoadSegment {
            points: (0..=20)
                .map(|i| (lat0 + i as f64 * 10.0 / m_per_deg_lat(lat0), lon0 + 200.0 / m_per_deg_lng(lat0)))
                .collect(),
            kind: "path".into(),
        });
        RoadNet { segments: segs, source: "test".into() }
    }

    #[test]
    fn bbox_pads_outward() {
        let pts = vec![(31.23, 121.47), (31.24, 121.48)];
        let (a, b, c, d) = bbox_of(&pts, 50.0);
        assert!(a < 31.23 && b < 121.47 && c > 31.24 && d > 121.48);
    }

    #[test]
    fn route_stays_on_segments() {
        let net = l_shape_net();
        let r = route_on_roads(&net, (31.2304, 121.4737), &[], 300.0, 7).unwrap();
        assert!(r.points.len() > 5);
        // 每个点都必须落在某一段路上（端点之间线性插值也在路上）
        for p in &r.points {
            let on = net.segments.iter().any(|s| {
                s.points.windows(2).any(|w| point_on_segment(*p, w[0], w[1], 3.0))
            });
            assert!(on, "点 {p:?} 不在任何道路上");
        }
    }

    #[test]
    fn route_length_is_bounded() {
        let net = l_shape_net();
        for want in [100.0, 200.0, 400.0] {
            let r = route_on_roads(&net, (31.2304, 121.4737), &[], want, 3).unwrap();
            assert!(
                r.length_m <= want * 1.1,
                "走了 {:.0}m 超过目标 {want}m",
                r.length_m
            );
        }
    }

    #[test]
    fn empty_net_is_rejected() {
        let net = RoadNet::default();
        assert!(route_on_roads(&net, (31.23, 121.47), &[], 100.0, 1).is_err());
    }

    /// 点到线段的距离在容差内。
    fn point_on_segment(p: (f64, f64), a: (f64, f64), b: (f64, f64), tol_m: f64) -> bool {
        let mlat = m_per_deg_lat(p.0).max(1.0);
        let mlng = m_per_deg_lng(p.0).max(1.0);
        let (py, px) = (0.0, 0.0);
        let (ay, ax) = ((a.0 - p.0) * mlat, (a.1 - p.1) * mlng);
        let (by, bx) = ((b.0 - p.0) * mlat, (b.1 - p.1) * mlng);
        let (vy, vx) = (by - ay, bx - ax);
        let len2 = vy * vy + vx * vx;
        let t = if len2 < 1e-12 {
            0.0
        } else {
            (-(ay * vy + ax * vx) / len2).clamp(0.0, 1.0)
        };
        let (cy, cx) = (ay + vy * t, ax + vx * t);
        ((cy - py).powi(2) + (cx - px).powi(2)).sqrt() < tol_m
    }
}
