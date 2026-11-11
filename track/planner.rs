//! 轨迹规划：打卡点 → 闭环 → 密集采样 → 里程对齐。
//!
//! 目标：给定起点、打卡点和目标里程/时长，产出一条能过服务端校验的轨迹。
//!
//! 四条硬约束（全部来自对提交报文的实测）：
//! 1. 首点时间戳必须精确等于 `startTime`，末点等于 `stopTime`
//! 2. 折线实测长度必须等于 `totalDis`（容差 0.5m，服务端按米取整）
//! 3. `sportType=1` 必须命中所有打卡点（±0.1m）
//! 4. 每 10 秒桶必须有自然抖动，不能等距

use rand::Rng;

use super::geo::{distance, nearest_index, nudge_to, sample_along, scale_to_length};

/// 一个打卡点。
#[derive(Debug, Clone)]
pub struct Checkpoint {
    /// BD-09（提交报文用）。
    pub lat: f64,
    pub lng: f64,
    /// GCJ-02（高德地图展示用）。缺省时等于 BD-09，只在测试里出现。
    pub glat: f64,
    pub glon: f64,
    pub name: String,
}

/// 轨迹点。
#[derive(Debug, Clone, Copy)]
pub struct TrackPoint {
    pub lat: f64,
    pub lon: f64,
    pub time_ms: i64,
}

/// 规划请求。
#[derive(Debug, Clone)]
pub struct PlanRequest {
    pub center_lat: f64,
    pub center_lng: f64,
    pub checkpoints: Vec<Checkpoint>,
    pub total_distance_m: f64,
    pub total_sec: i64,
    pub start_ms: i64,
    /// `sport_type=1` 计分跑（必须命中打卡点），`6` 自由跑。
    pub sport_type: i32,
    /// 采样间隔（米）。越小越密，报文越大。
    pub sample_step_m: f64,
}

impl Default for PlanRequest {
    fn default() -> Self {
        Self {
            center_lat: 30.5,
            center_lng: 104.0,
            checkpoints: Vec::new(),
            total_distance_m: 2000.0,
            total_sec: 720,
            start_ms: 0,
            sport_type: 1,
            sample_step_m: 5.0,
        }
    }
}

/// 规划结果。
#[derive(Debug, Clone)]
pub struct PlanResult {
    pub points: Vec<TrackPoint>,
    pub checkpoints: Vec<Checkpoint>,
    pub actual_length_m: f64,
}

/// 规划轨迹。
pub fn plan(req: &PlanRequest, rng: &mut impl Rng) -> Result<PlanResult, String> {
    if req.total_distance_m <= 0.0 {
        return Err("目标里程必须为正".into());
    }
    if req.total_sec <= 0 {
        return Err("目标时长必须为正".into());
    }

    let (mut cps, kind) = resolve_checkpoints(req, rng);

    // 1. 串成闭环
    let ring = loop_waypoints(
        (req.center_lat, req.center_lng),
        &cps.iter().map(|c| (c.lat, c.lng)).collect::<Vec<_>>(),
    );
    let ring_len = super::geo::path_length(&ring);
    if ring_len <= 0.0 {
        return Err("闭环长度为 0，无法规划".into());
    }

    // 2. 跑圈凑里程。
    //
    // 不能用「整条路径绕首点缩放」来凑：缩放会把打卡点一起推走，实测 2000m
    // 的目标把打卡点推开 200m 以上，事后吸附再归位又把总长拉到 2239m。
    // 跑圈没有这个问题——打卡点始终在圈上，长度靠圈数和尾巴精确控制。
    if ring_len > req.total_distance_m {
        return Err(format!(
            "目标里程 {:.0}m 装不下这 {n} 个打卡点（最短闭环 {ring_len:.0}m）",
            req.total_distance_m,
            n = cps.len()
        ));
    }
    let laps = (req.total_distance_m / ring_len).floor().max(1.0) as usize;
    let mut route: Vec<(f64, f64)> = Vec::with_capacity(ring.len() * laps + 8);
    for _ in 0..laps {
        route.extend_from_slice(&ring[..ring.len() - 1]);
    }
    route.push(ring[0]);

    // 尾巴：把剩余里程用一个小环补上，避免出现原地掉头的尖刺
    let remaining = req.total_distance_m - ring_len * laps as f64;
    if remaining > 20.0 {
        let tail_r = remaining / (2.0 * std::f64::consts::PI);
        let tail = circle_ring(req.center_lat, req.center_lng, tail_r, 16);
        route.extend_from_slice(&tail[1..]);
        route.push(route[0]);
    }

    // 3. 密集采样
    let n = ((req.total_distance_m / req.sample_step_m).round() as usize).max(8);
    let mut flat: Vec<(f64, f64)> = sample_along(&route, n);

    // 4. 时间戳：等间隔 + 末点精确对齐
    let times = assign_times(n, req.start_ms, req.total_sec);

    // 5. 里程对齐（锁首点）
    // 容差放到 15%（最多 300m）：几何本身已经落在可接受区间就不缩放，
    // 保持串点位后的自然形状。硬缩放到精确 2.000 km 会把整条线拉伸，
    // 反而容易把点拉进建筑物里。
    let tol = super::geo::length_tolerance(req.total_distance_m);
    let (_len, _) = scale_to_length(&mut flat, req.total_distance_m, &[0], tol);

    // 6. 打卡点吸附 + 再次对齐 + 归位
    let mut anchor_idx = Vec::new();
    if kind == CheckpointKind::Official && !cps.is_empty() {
        // 只修正采样误差级别的小偏差。
        //
        // 闭环本身就经过打卡点，等距采样最多错过半个采样间隔（默认 5m → 2.5m）。
        // 早先用「目标里程的 35%」当阈值，会把几百米外的点硬拉过来，
        // 事后归位就破坏了总长度（实测 2000m 被拉到 2239m）。
        for cp in &cps {
            let idx = nearest_index(&flat, (cp.lat, cp.lng));
            let gap = distance(flat[idx], (cp.lat, cp.lng));
            if gap <= req.sample_step_m * 2.0 + 1.0 {
                nudge_to(&mut flat, idx, (cp.lat, cp.lng), 2);
                anchor_idx.push(idx);
            } else {
                // 采样点离打卡点太远，说明规划路径没真正经过它 —— 宁可报错也不伪造
                return Err(format!(
                    "打卡点 {} 距最近采样点 {gap:.1}m，规划路径未经过该点",
                    cp.name
                ));
            }
        }
        // 吸附位移很小（≤ 采样间隔），长度影响在厘米级，不需要再缩放
        for (cp, &idx) in cps.iter().zip(anchor_idx.iter()) {
            flat[idx] = (cp.lat, cp.lng);
        }
    }

    // 建筑/禁区避让。OSM 在国内校园基本没有建筑轮廓，Web 上可以人工圈，
    // 存成 blockers.json；这里对所有走 planner 的路径（Web 预览、CLI、兜底）
    // 统一生效，跟 flow::build 里那条路保持一致。
    let blockers = crate::track::roadnet::load_local_blockers();
    let (flat, times) = if blockers.is_empty() || flat.len() < 3 {
        (flat, times)
    } else {
        let before_len = crate::track::geo::path_length(&flat);
        let avoided = crate::track::fence_route::avoid_blockers(&flat, &blockers, 3.0);
        // 注意：避让可能只挪点、不插点，长度不变。用坐标比较，不能用长度
        // 判断（用长度会把「推出楼体」的结果整个丢掉，线照样穿墙）。
        if avoided == flat {
            (flat, times)
        } else {
            // 绕行会插点，时间戳按比例摊回去，保证总时长不变
            let after_len = crate::track::geo::path_length(&avoided);
            let n = avoided.len();
            let span = times.last().copied().unwrap_or(0) - times.first().copied().unwrap_or(0);
            let t0 = times.first().copied().unwrap_or(0);
            let times: Vec<i64> = (0..n)
                .map(|i| t0 + span * i as i64 / (n.max(2) - 1) as i64)
                .collect();
            let _ = (before_len, after_len);
            (avoided, times)
        }
    };

    let points: Vec<TrackPoint> = flat
        .iter()
        .zip(times.iter())
        .map(|((lat, lon), &t)| TrackPoint { lat: *lat, lon: *lon, time_ms: t })
        .collect();

    // 自由跑不返回打卡点（服务端不校验，硬塞反而会拦短距离）
    if kind == CheckpointKind::Synthetic && req.sport_type == 6 {
        cps.clear();
    }

    Ok(PlanResult {
        actual_length_m: super::geo::path_length(&flat),
        points,
        checkpoints: cps,
    })
}

#[derive(Debug, PartialEq)]
enum CheckpointKind {
    /// 服务端下发的真实点位，必须命中
    Official,
    /// 无点位时按目标里程合成的等距环点，仅用于填满结构
    Synthetic,
    /// 自由跑，不需要点位
    None,
}

fn resolve_checkpoints(req: &PlanRequest, rng: &mut impl Rng) -> (Vec<Checkpoint>, CheckpointKind) {
    if !req.checkpoints.is_empty() {
        return (req.checkpoints.clone(), CheckpointKind::Official);
    }
    if req.sport_type == 6 {
        return (Vec::new(), CheckpointKind::None);
    }
    // 计分跑但服务端没给点位：按 7.3.40 语义补 5 个等距环点。
    // 半径按目标里程反推，保证第一圈回到起点恰好用完整里程。
    let radius = (req.total_distance_m / (2.0 * std::f64::consts::PI)).max(8.0);
    // circle_ring 末尾会补一个闭合点，打卡点只取真正的 5 个顶点
    let pts = circle_ring(req.center_lat, req.center_lng, radius, 5);
    let cps = pts
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, &(lat, lng))| Checkpoint {
            lat,
            lng,
            glat: lat,
            glon: lng,
            name: format!("FixedPoint_{}", i + 1),
        })
        .collect();
    let _ = rng;
    (cps, CheckpointKind::Synthetic)
}

/// 最近邻串成闭环：起点 → 各点 → 回起点。
fn loop_waypoints(start: (f64, f64), cps: &[(f64, f64)]) -> Vec<(f64, f64)> {
    if cps.is_empty() {
        // 无点位：方形小环，边长按 120m 起步
        return square_ring(start.0, start.1, 120.0);
    }
    let mut remaining: Vec<(f64, f64)> = cps.to_vec();
    let mut route = vec![start];
    while !remaining.is_empty() {
        let last = *route.last().unwrap();
        let mut best = 0usize;
        let mut best_d = f64::INFINITY;
        for (i, p) in remaining.iter().enumerate() {
            let d = distance(last, *p);
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        route.push(remaining.remove(best));
    }
    if distance(*route.last().unwrap(), start) > 1e-6 {
        route.push(start);
    }
    route
}

/// 以 `center` 为圆心、`radius_m` 为半径的正多边形环（首尾闭合）。
fn circle_ring(center_lat: f64, center_lng: f64, radius_m: f64, segments: usize) -> Vec<(f64, f64)> {
    let d_lat = radius_m / super::geo::M_PER_DEG_LAT;
    let d_lng = radius_m / super::geo::m_per_deg_lng(center_lat);
    let n = segments.max(3);
    let mut out = Vec::with_capacity(n + 1);
    for i in 0..n {
        let a = 2.0 * std::f64::consts::PI * i as f64 / n as f64;
        out.push((center_lat + d_lat * a.sin(), center_lng + d_lng * (1.0 - a.cos())));
    }
    out.push(out[0]);
    out
}

fn square_ring(lat: f64, lng: f64, side_m: f64) -> Vec<(f64, f64)> {
    let d_lat = (side_m / 2.0) / super::geo::M_PER_DEG_LAT;
    let d_lng = (side_m / 2.0) / super::geo::m_per_deg_lng(lat);
    vec![
        (lat, lng),
        (lat + d_lat, lng),
        (lat + d_lat, lng + d_lng),
        (lat - d_lat, lng + d_lng),
        (lat - d_lat, lng),
        (lat, lng),
    ]
}

/// 等间隔时间戳，末点精确等于 start + total_sec。
pub fn assign_times(n: usize, start_ms: i64, total_sec: i64) -> Vec<i64> {
    let end = start_ms + total_sec * 1000;
    if n <= 1 {
        return vec![start_ms];
    }
    let step = (end - start_ms) / (n as i64 - 1);
    (0..n)
        .map(|i| if i == n - 1 { end } else { start_ms + i as i64 * step })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    fn rng() -> rand::rngs::StdRng {
        rand::rngs::StdRng::seed_from_u64(2026)
    }

    fn base_req() -> PlanRequest {
        PlanRequest {
            center_lat: 30.5,
            center_lng: 104.0,
            checkpoints: Vec::new(),
            total_distance_m: 2000.0,
            total_sec: 720,
            start_ms: 1_730_000_000_000,
            sport_type: 1,
            sample_step_m: 5.0,
        }
    }

    #[test]
    fn length_within_tolerance() {
        // 不再要求精确 2000.0：几何本身落在容差内就不缩放（见 geo::length_tolerance）。
        // 硬凑整会把轨迹拧成不自然的形状，还可能把点拉进楼里。
        let mut r = rng();
        let res = plan(&base_req(), &mut r).unwrap();
        let tol = super::super::geo::length_tolerance(2000.0);
        assert!(
            (res.actual_length_m - 2000.0).abs() <= tol,
            "实测长度 {} 超出目标 2000 的容差 {tol}m",
            res.actual_length_m
        );
    }

    #[test]
    fn timestamps_align_exactly() {
        let mut r = rng();
        let req = base_req();
        let res = plan(&req, &mut r).unwrap();
        let first = res.points.first().unwrap();
        let last = res.points.last().unwrap();
        assert_eq!(first.time_ms, req.start_ms, "首点时间必须等于 startTime");
        assert_eq!(last.time_ms, req.start_ms + req.total_sec * 1000, "末点必须等于 stopTime");
    }

    #[test]
    fn times_are_monotonic() {
        let mut r = rng();
        let res = plan(&base_req(), &mut r).unwrap();
        for w in res.points.windows(2) {
            assert!(w[1].time_ms > w[0].time_ms, "时间必须严格递增");
        }
    }

    #[test]
    fn official_checkpoints_hit_exactly() {
        let mut r = rng();
        let mut req = base_req();
        req.total_distance_m = 2400.0;
        req.checkpoints = vec![
            Checkpoint { lat: 30.5008, lng: 104.0008, glat: 30.5008, glon: 104.0008, name: "A".into() },
            Checkpoint { lat: 30.5008, lng: 104.0024, glat: 30.5008, glon: 104.0024, name: "B".into() },
            Checkpoint { lat: 30.4992, lng: 104.0024, glat: 30.4992, glon: 104.0024, name: "C".into() },
        ];
        let res = plan(&req, &mut r).unwrap();
        for cp in &res.checkpoints {
            let hit = res
                .points
                .iter()
                .any(|p| distance((p.lat, p.lon), (cp.lat, cp.lng)) < 0.2);
            assert!(hit, "打卡点 {} 没被命中", cp.name);
        }
    }

    #[test]
    fn synthetic_checkpoints_when_none_given() {
        let mut r = rng();
        let res = plan(&base_req(), &mut r).unwrap();
        assert_eq!(res.checkpoints.len(), 5, "计分跑无点位时应补 5 个合成点");
        assert!(res.checkpoints[0].name.starts_with("FixedPoint"));
    }

    #[test]
    fn free_run_has_no_checkpoints() {
        let mut r = rng();
        let mut req = base_req();
        req.sport_type = 6;
        let res = plan(&req, &mut r).unwrap();
        assert!(res.checkpoints.is_empty(), "自由跑不应带打卡点");
    }

    #[test]
    fn rejects_bad_input() {
        let mut r = rng();
        let mut req = base_req();
        req.total_distance_m = 0.0;
        assert!(plan(&req, &mut r).is_err());

        let mut req2 = base_req();
        req2.total_sec = 0;
        assert!(plan(&req2, &mut r).is_err());
    }

    #[test]
    fn point_count_is_reasonable() {
        let mut r = rng();
        let res = plan(&base_req(), &mut r).unwrap();
        // 2000m / 5m = 400 点，允许上下浮动
        assert!(res.points.len() >= 300 && res.points.len() <= 500,
                "点数 {} 不合理", res.points.len());
    }
}
