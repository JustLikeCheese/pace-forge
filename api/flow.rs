//! 完整提交流程编排。
//!
//! 顺序不能乱，每一步的产物都是下一步的输入：
//!
//! ```text
//! ① policy   → policy 串、minDistance
//! ② fence    → 选中校区、runAreaId、围栏几何
//! ③ points   → 打卡点（BD-09）
//! ④ 规划轨迹 → 必经点全部命中的闭环
//! ⑤ retime   → 由速度反推时间戳
//! ⑥ simulate → 步数、10 秒分桶
//! ⑦ preflight→ 本地自检，不过就不发
//! ⑧ OBS      → 上传轨迹明细，拿 objectKey
//! ⑨ submit   → 提交结算
//! ⑩ 静默窗口 → 提交后一段时间内不能再有跑步请求
//! ```
//!
//! 第 ⑦ 步是安全闸：**宁可本地失败，也不要把注定被驳回的报文发出去**。
//! 驳回会记错误次数，异常多了触发风控。

use serde_json::{json, Value};

use super::client::Client;
use super::fence::{self, FencePick};
use super::obs;
use super::points::{self, PointsContext};
use super::policy::{self, PolicyInfo};
use crate::track::{
    inspect, plan, retime, simulate, Check, Checkpoint, KinParams, PlanRequest, PlanResult,
    Report, SimParams, TrackPoint,
};

/// 一次跑步的参数。
#[derive(Debug, Clone)]
pub struct RunParams {
    /// 中心点纬度（WGS84）。
    pub center_lat: f64,
    /// 中心点经度（WGS84）。
    pub center_lng: f64,
    /// 目标里程（米）。
    pub distance_m: f64,
    /// 目标时长（秒）。
    pub duration_sec: i64,
    /// 运动类型（提交报文里的 sportType）。
    pub sport_type: i32,
    /// runMode：1=校园跑，2=户外跑，3=室外锻炼。
    pub run_mode: i64,
    /// 指定步数，`None` 由模型推算。
    pub steps: Option<i32>,
    /// 随机种子。
    pub seed: u64,
    /// 采样间距（米）。
    pub sample_step_m: f64,
    /// 是否优先沿真实道路规划（OSM）。查不到路网时自动降级。
    pub prefer_roads: bool,
    /// 指定开始时刻（毫秒戳）。`None` 表示现在开跑。
    ///
    /// 补跑时由 `start_time` 模块算好传进来——那边已经保证了
    /// 「开始 + 用时 + 抖动余量 ≤ 现在」，所以这里不再二次校验。
    pub start_at_ms: Option<i64>,
    /// 手绘轨迹（BD-09 经纬度对）。`Some` 时跳过 planner，直接采用。
    ///
    /// 用户清楚哪栋楼在哪，手画比算法猜靠谱——尤其 OSM 在该校区
    /// 零建筑数据、几何规划只能凭直线连点的时候。
    pub custom_path: Option<Vec<(f64, f64)>>,
}

/// 尝试沿 OSM 真实道路规划。任何一步失败都返回 None（静默降级）。
fn try_route_on_roads(
    fence: &[(f64, f64)],
    must: &[(f64, f64)],
    want_m: f64,
    seed: u64,
    log: &mut dyn FnMut(&str),
) -> Option<crate::track::fence_route::FenceRoute> {
    use crate::track::roadnet;
    let bbox = roadnet::bbox_of(fence, 80.0);
    let net = match roadnet::fetch(bbox) {
        Ok(n) => n,
        Err(e) => {
            log(&format!("[roads] 拉路网失败（{e}），改用几何规划"));
            return None;
        }
    };
    let clipped = net.clip_to_fence(fence);
    let total = clipped.total_length_m();
    if clipped.is_empty() || total < 300.0 {
        log(&format!(
            "[roads] OSM 在该校区只有 {total:.0}m 可用道路，改走几何规划"
        ));
        return None;
    }
    log(&format!("[roads] 路网 {total:.0}m，按真实道路规划"));
    let start = must.first().copied().unwrap_or(fence[0]);
    match roadnet::route_on_roads(&clipped, start, must, want_m, seed) {
        Ok(r) => {
            // 质量门槛：路网只有几百米时，凑里程会「跳」到另一段路上，
            // 造成几十上百米的瞬移（实测 125m）。这种轨迹服务端一眼判异常，
            // 宁可退回几何规划。
            let max_gap = r
                .points
                .windows(2)
                .map(|w| crate::track::geo::distance(w[0], w[1]))
                .fold(0.0f64, f64::max);
            if max_gap > 50.0 {
                log(&format!(
                    "[roads] 路网仅 {total:.0}m，拼接出 {max_gap:.0}m 的跳变，\
                     改用几何规划"
                ));
                return None;
            }
            Some(crate::track::fence_route::FenceRoute {
                points: r.points,
                length_m: r.length_m,
                lines: 1,
            })
        }
        Err(e) => {
            log(&format!("[roads] 沿路规划失败（{e}），改用几何规划"));
            None
        }
    }
}

impl RunParams {
    /// 统一校验。CLI、Web、库调用都必须走这里。
    ///
    /// 之前三个入口各写一套：Web 拦到 300 分钟，CLI 什么都不拦，
    /// `MAX_RUN_TIME`（7200 秒 = 120 分钟）这个常量定义了却从没被引用。
    /// 结果 `run 2 200` 能一路走到提交，必然被服务端驳回，还累计风控次数。
    pub fn validate(&self) -> Result<(), String> {
        if !self.distance_m.is_finite() || self.distance_m <= 0.0 {
            return Err(format!("距离必须是正数，收到 {}", self.distance_m));
        }
        if self.distance_m > 42_000.0 {
            return Err(format!(
                "距离 {} m 超过上限 42000 m（42 公里）",
                self.distance_m
            ));
        }
        if self.duration_sec <= 0 {
            return Err(format!("时长必须为正，收到 {} 秒", self.duration_sec));
        }
        if self.duration_sec > super::submit::MAX_RUN_TIME {
            return Err(format!(
                "时长 {} 秒超过上限 {} 秒（{} 分钟）——超过这个值服务端必然驳回",
                self.duration_sec,
                super::submit::MAX_RUN_TIME,
                super::submit::MAX_RUN_TIME / 60
            ));
        }
        if let Some(st) = self.steps {
            if st < 0 {
                return Err(format!("步数不能为负，收到 {st}"));
            }
        }
        if !(-90.0..=90.0).contains(&self.center_lat) || !(-180.0..=180.0).contains(&self.center_lng) {
            return Err(format!(
                "定位锚点越界: ({}, {})",
                self.center_lat, self.center_lng
            ));
        }
        if self.sample_step_m <= 0.0 || self.sample_step_m > 100.0 {
            return Err(format!("采样间距不合理: {} m", self.sample_step_m));
        }
        // 报文的 totalTime 只到秒，采样间隔必须 ≥ 最快速度下的 1 秒，
        // 否则反解后相邻点时间戳相同，服务端会看到 0 时长段。
        // 8 m/s 上限下，1 秒 = 8 米。
        if self.sample_step_m < 8.0 && self.duration_sec > 0 {
            let avg_v = self.distance_m / self.duration_sec as f64;
            let gap = self.sample_step_m / avg_v.max(0.5);
            if gap < 1.0 {
                return Err(format!(
                    "采样间距 {}m 在 {avg_v:.2} m/s 下只有 {gap:.2} 秒，                     报文时间精度是秒，会画出 0 时长段；把间距调到 {:.0}m 以上",
                    self.sample_step_m,
                    avg_v.max(0.5)
                ));
            }
        }
        Ok(())
    }
}

impl Default for RunParams {
    fn default() -> Self {
        Self {
            center_lat: 30.5,
            center_lng: 104.0,
            distance_m: 2000.0,
            duration_sec: 720,
            sport_type: 1,
            run_mode: 1,
            steps: None,
            seed: 0,
            // 10m：真机 GPS 采样 ~10m 一个点（2km≈200 点）。早先 5m 会有 400 点，
            // 点数翻倍是密度指纹，且低于 flow 校验的 8m 阈值。
            sample_step_m: 10.0,
            prefer_roads: true,
            start_at_ms: None,
            custom_path: None,
        }
    }
}

/// 每一步的日志。
pub type Log = dyn FnMut(&str);

/// 流程产物。
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub policy: PolicyInfo,
    pub fence: Option<FencePick>,
    pub points: PointsContext,
    pub track_points: usize,
    pub actual_length_m: f64,
    pub total_steps: i32,
    pub avg_cadence: f64,
    pub report: Report,
    /// 轨迹点（含时间戳），提交与明细上传都要用。
    pub track: Vec<crate::track::TrackPoint>,
    /// 合成的步数/分桶数据。
    pub sim: crate::track::SimResult,
    /// 本次跑步的起始时间戳（毫秒）。
    pub start_ms: i64,
    /// 几何实测里程（可能和目标有小偏差）。
    pub real_length_m: i64,
    pub obs_keys: Vec<String>,
    /// 是否已提交。
    pub submitted: bool,
    pub record_id: Option<i64>,
}

/// 只做到自检为止（不发任何写请求）。
///
/// 用来在提交前确认数据没问题——大部分坑在这一步就能发现。
pub fn dry_run(client: &Client, p: &RunParams, log: &mut Log) -> Result<RunOutcome, String> {
    p.validate()?;
    // 校时：之后所有时间戳都必须落在服务器时间轴上。
    // 「开始时间 + 时长 ≤ 服务器现在」是服务端硬判据，本机时钟偏十几分钟
    // 就直接驳回，而本地自检用同一个时钟，永远发现不了这个偏差。
    let (server_now, skew) = super::gmttime::server_now_ms(client);
    match skew {
        Some(off) if off.abs() >= 1000 => log(&format!(
            "√ [clock] 已校时：服务器比本机{}{}ms",
            if off > 0 { "快" } else { "慢" },
            off.abs()
        )),
        Some(_) => log("√ [clock] 已校时：与本机一致"),
        None => log("· [clock] 校时失败，按本机时钟继续"),
    }
    let start_ms = match p.start_at_ms {
        // 补跑：时间是外部算好的，直接用它，但照样加抖动让秒数不整齐
        Some(t) => {
            use rand::Rng;
            let mut r = rand::thread_rng();
            t + r.gen_range(0..4_000i64)
        }
        // 现在开跑：加 0~4 秒偏移，每次都整秒开始太规律
        None => {
            use rand::Rng;
            let mut r = rand::thread_rng();
            server_now + r.gen_range(0..5_000i64)
        }
    };

    // ⓿ 学期进度：记分跑要先知道还差多少
    if p.run_mode == 1 {
        match super::semester::fetch_summary(client) {
            Ok(sm) => log(&format!("√ [semester] {}", sm.summary())),
            Err(e) => log(&format!("· [semester] 查询失败（不影响流程）: {e}")),
        }
        match super::semester::fetch_goal(client, p.run_mode) {
            Ok(g) if g.target_dis > 0.0 => {
                let left = g.remaining_dis(0.0) / 1000.0;
                log(&format!(
                    "√ [goal] 学期目标 {:.1}km / {} 次{}",
                    left,
                    g.target_count,
                    if g.days_left(server_now) >= 0 {
                        format!("，还剩 {} 天", g.days_left(server_now))
                    } else {
                        String::new()
                    }
                ));
            }
            Ok(_) => {}
            Err(e) => log(&format!("· [goal] 查询失败（不影响流程）: {e}")),
        }
    }

    // ① policy
    log(&format!("[policy] 拉取策略 runMode={}…", p.run_mode));
    // 记分跑在时段外会直接 11016，连策略串都拿不到。但提交报文只要
    // policy 这个字符串（实测恒为 "0"），timestamp 不进签名，点位又走
    // 独立的 /api/points，所以拿不到策略不等于不能提交。
    // PACE_STRICT_POLICY=1 可以关掉兜底，保留原来的硬失败行为。
    let strict = std::env::var("PACE_STRICT_POLICY").ok().as_deref() == Some("1");
    let pol = match policy::fetch_policy_for(client, p.run_mode) {
        Ok(v) => {
            log(&format!(
                "√ [policy] ts={} policy={} minDistance={} validTime={}",
                v.timestamp, v.policy, v.min_distance, v.valid_time
            ));
            v
        }
        Err(e) if !strict => {
            let fb = policy::fallback_policy();
            log(&format!("⚠ [policy] 拉取失败（{e}）"));
            log(&format!(
                "⚠ [policy] 改用兜底：policy={} minDistance={} validTime={}，\
照常提交看服务端是否接受",
                fb.policy, fb.min_distance, fb.valid_time
            ));
            fb
        }
        Err(e) => return Err(e),
    };

    // ② fence
    log("[fence] 拉取围栏…");
    let pick = match fence::fetch_fences(client) {
        Ok(fs) if !fs.is_empty() => {
            match fence::select_by_anchor(&fs, (p.center_lat, p.center_lng), -1) {
                Ok(pk) => {
                    if pk.fallback {
                        log(&format!(
                            "⚠ [fence] 锚点不在任何围栏内，按最近「{}」兜底（{:.0}m）",
                            pk.selected.name, pk.distance_m
                        ));
                    } else {
                        log(&format!(
                            "√ [fence] 命中「{}」id={:?}，共 {} 个围栏",
                            pk.selected.name,
                            pk.selected.fence_id,
                            fs.len()
                        ));
                    }
                    Some(pk)
                }
                Err(e) => {
                    log(&format!("⚠ [fence] 选择失败，按自由跑继续: {e}"));
                    None
                }
            }
        }
        Ok(_) => {
            log("· [fence] 服务端未配置围栏，按自由跑继续");
            None
        }
        Err(e) => {
            log(&format!("⚠ [fence] 接口失败，按自由跑继续: {e}"));
            None
        }
    };

    // ③ points
    log("[points] 拉取打卡点…");
    // 目标距离要传给点位接口：有的学校只在带真实距离时才下发点位。
    // 先按 distance/1 拉，空集才用目标距离重试（见 points::fetch_points_cached_at）。
    let pt = match points::fetch_points_cached_at(
        client,
        p.center_lat,
        p.center_lng,
        pick.as_ref().map(|k| k.selected.run_area_id.to_string()).as_deref(),
        Some(p.distance_m.round() as i64),
    ) {
        // 点位接口返回 200 但空数组：有的学校把必经点只放在围栏里，
        // 这时点位接口本来就是空的，不是「这个校区没有打卡点」。
        Ok(c) if c.points.is_empty() => {
            match pick.as_ref().filter(|k| !k.selected.pass_points.is_empty()) {
                Some(k) => {
                    let (items, bd, gcj) = fence::pass_point_items(&k.selected.pass_points);
                    log(&format!(
                        "√ [points] 点位接口为空，改用围栏「{}」下发的 {} 个必经点",
                        k.selected.name,
                        items.len()
                    ));
                    PointsContext { points: items, coords: bd, gcj_coords: gcj }
                }
                None => {
                    log("· [points] 点位接口为空，且所选围栏未下发必经点");
                    c
                }
            }
        }
        Ok(c) => {
            log(&format!(
                "√ [points] {} 个点位，分布跨度 {:.0}m",
                c.coords.len(),
                points::span_m(&c.coords)
            ));
            // 「按顺序通过所有点位」判定失败时，得知道服务端给的正式顺序
            if let Some(first) = c.points.first() {
                let keys: Vec<&String> = first
                    .as_object()
                    .map(|o| o.keys().collect())
                    .unwrap_or_default();
                log(&format!("[points] 原始字段: {keys:?}"));
                log(&format!("[points] 首点: {first}"));
                // 每个点位的 isFixed / 名字都要看：服务端的 type 3
                // 「至少通过2个普通点位」要求有 isFixed=0 的点可供通过
                for (i, raw) in c.points.iter().enumerate() {
                    let fixed = raw.get("isFixed").and_then(|v| v.as_i64()).unwrap_or(-1);
                    let name = raw.get("pointName").and_then(|v| v.as_str()).unwrap_or("");
                    log(&format!("[points]  #{i} isFixed={fixed} {name}"));
                }

            }
            c
        }
        Err(e) => {
            log(&format!("⚠ [points] 接口失败（可能限流），无打卡点继续: {e}"));
            PointsContext::default()
        }
    };

    // 拉到打卡点就记住校区中心（BD-09），之后没围栏时的兜底锚点用它，
    // 不再拿成都坐标当跑者位置发给服务端。
    if let Some((hl, hg)) = points::center_bd(&pt.coords) {
        if let Some(uid) = client.credentials().and_then(|c| c.uid.as_deref()) {
            crate::api::session::save_home(uid, hl, hg);
        }
    }

    // ④ 规划
    let cps: Vec<Checkpoint> = pt
        .coords
        .iter()
        .enumerate()
        .map(|(i, &(lat, lng))| Checkpoint {
            lat,
            lng,
            // 顺带把 GCJ-02 带上：前端高德地图按这套坐标摆点位，
            // 长度可能和 coords 不一致，取不到就退回 BD-09
            glat: pt.gcj_coords.get(i).map(|v| v.0).unwrap_or(lat),
            glon: pt.gcj_coords.get(i).map(|v| v.1).unwrap_or(lng),
            name: format!("P{}", i + 1),
        })
        .collect();
    // 有围栏就在围栏内铺路：早先不管围栏形状，按中心点画固定半径的圆，
    // 围栏一大轨迹就有一半在外面，服务端判越界，整条记录作废（距离 0.00km）。
    let fence_ring: Vec<(f64, f64)> = pick
        .as_ref()
        .map(|k| {
            // 多个多边形取最大的那块，避免拼成跨越围栏的直线
            k.selected
                .polygons
                .iter()
                .max_by(|a, b| a.len().cmp(&b.len()))
                .cloned()
                .unwrap_or_default()
        })
        .unwrap_or_default();

    // 手绘轨迹优先。
    //
    // OSM 在该校区零建筑数据，「按序串点」只能靠弧线连点，必然从楼里穿过去。
    // 用户知道哪栋楼在哪，手画一条绕开的路线比任何算法都准，所以这里直接
    // 采用，连围栏内缩和打卡点吸附都不做——点位已经在用户画的线上了。
    let track = if let Some(custom) = p.custom_path.as_ref().filter(|v| v.len() >= 2) {
        log(&format!("[track] 采用手绘轨迹 {} 个控制点", custom.len()));
        // 不传 must：手绘的整条价值就是用户自己控制形状，把没画到的点位
        // 强行插进去只会在原地拉出几百米的跳变（GEO_JUMP），把线撕烂。
        // 漏了点位由前端实时提示，用户自己补笔画。
        let mut pts = crate::track::fence_route::resample_with_checkpoints(
            custom,
            p.sample_step_m,
            &[],
            0.0,
        );
        // 首尾离得近就接上，否则一圈画完终点回到起点会算成一次瞬移
        if pts.len() >= 2 {
            let (first, last) = (pts[0], pts[pts.len() - 1]);
            let gap = crate::track::geo::distance(first, last);
            if gap > 1.0 && gap < 120.0 {
                let steps = ((gap / p.sample_step_m).ceil() as usize).max(2);
                for k in 1..steps {
                    let t = k as f64 / steps as f64;
                    pts.push((
                        last.0 + (first.0 - last.0) * t,
                        last.1 + (first.1 - last.1) * t,
                    ));
                }
                log(&format!("[track] 首尾相距 {gap:.0}m，已自动接上"));
            }
        }
        // 手绘轨迹不缩放到目标里程。
        //
        // 缩放看着很省事，但它会破坏手绘唯一的优点。用户画的时候地图上
        // 摆着 5 个点位，他是照着点位划的，所以**原尺度下点位天然就在
        // 线上**。一旦按 目标/实际 等比放大（常到 2.5~3 倍），路径整体
        // 放大而点位坐标是固定的，这个性质当场失效：点位会飘出几十米，
        // 之后无论怎么吸附都是打补丁——
        //
        //   · 吸附后 position 乱序 → 服务端「按顺序通过」false
        //   · 按顺序吸附 + 阈值收紧 → 靠后的点位直接 CP_MISS
        //   · 阈值放大到 250m → 能命中，但单点被拽 83m，detect 判异常
        //   · 缩放后重采样 → 点距回到 3m 了，点位又飘出去了
        //
        // 而且围栏是 1020×883m，绕一圈本来就有 3km 以上，正常照着点位
        // 画一圈绝不会短于 1.5km 的下限。真画短了，如实报错让用户补笔，
        // 比偷偷拉伸成一条穿过建筑物的假线要好。
        let drawn = crate::track::geo::path_length(&pts);
        if drawn < p.distance_m * 0.75 {
            log(&format!(
                "[track] 手绘 {drawn:.0}m，明显短于目标 {:.0}m；不自动拉伸（拉伸会让点位脱离轨迹），\
请在地图上画得更长一些",
                p.distance_m
            ));
        }

        // 原尺度下点位就在线边，按顺序取最近点即可精确命中
        let mut search_from = 0usize;
        for c in &cps {
            let target = (c.lat, c.lng);
            let mut best = usize::MAX;
            let mut best_d = f64::MAX;
            for (i, q) in pts.iter().enumerate().skip(search_from) {
                let d = crate::track::geo::distance(*q, target);
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            // 原尺度下 40m 内就算画到了；超出的交给前端提示补笔画
            if best != usize::MAX && best_d < 40.0 {
                pts[best] = target;
                search_from = best + 1;
            }
        }
        let n = pts.len();
        let times = crate::track::planner::assign_times(n, start_ms, p.duration_sec);
        let actual = crate::track::geo::path_length(&pts);
        log(&format!(
            "[track] 手绘轨迹缩放后 {:.0}m（原 {drawn:.0}m），{n} 点",
            actual
        ));
        crate::track::PlanResult {
            points: pts
                .into_iter()
                .zip(times)
                .map(|((lat, lng), t)| crate::track::TrackPoint { lat, lon: lng, time_ms: t })
                .collect(),
            checkpoints: cps.clone(),
            actual_length_m: actual,
        }
    } else if p.run_mode == 1 && !cps.is_empty() {
        // 记分跑 = 操场绕圈。真机轨迹是绕一个 ~280m 椭圆跑道跑 7 圈（偶尔 400m
        // 跑道 4.5 圈），不是满场的自由折线。这里按真实形状生成：圆心取打卡点
        // 质心（打卡点就在跑道上），朝向按打卡点主轴（对齐真实跑道走向），
        // 尺寸/圈数/入场尾巴/GPS 抖动都随机，最后把打卡点吸附到最近的圈上点。
        let seed = if p.seed != 0 { p.seed } else { start_ms as u64 };
        // 全体打卡点质心，先用来找出"场心"那个点。
        let gx = cps.iter().map(|c| c.lat).sum::<f64>() / cps.len() as f64;
        let gy = cps.iter().map(|c| c.lng).sum::<f64>() / cps.len() as f64;
        let klat = crate::track::geo::m_per_deg_lat(gx);
        let klng = crate::track::geo::m_per_deg_lng(gx);
        // 「田径场中」在场地内部，不是跑道外框的角，拿它算朝向/尺寸会把椭圆
        // 带偏、变圆。剔除离质心最近的那个点（就是场心），只用四角定跑道。
        let center_idx = (0..cps.len())
            .min_by(|&a, &b| {
                let da = ((cps[a].lat - gx) * klat).hypot((cps[a].lng - gy) * klng);
                let db = ((cps[b].lat - gx) * klat).hypot((cps[b].lng - gy) * klng);
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(0);
        let corners: Vec<&Checkpoint> = cps
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != center_idx || cps.len() <= 2)
            .map(|(_, c)| c)
            .collect();
        // 椭圆圆心 = 四角质心（真机跑道中心≈四角中心）。
        let cx = corners.iter().map(|c| c.lat).sum::<f64>() / corners.len() as f64;
        let cy = corners.iter().map(|c| c.lng).sum::<f64>() / corners.len() as f64;
        // 四角 PCA（二阶矩）取长轴方向——矩形的主轴是长边，不是对角线，所以
        // 比"最大跨度扫描"（会选到对角线）更对。真机这块场地算出 ~91°（竖向）。
        let (mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0);
        for c in &corners {
            let dx = (c.lng - cy) * klng;
            let dy = (c.lat - cx) * klat;
            sxx += dx * dx;
            syy += dy * dy;
            sxy += dx * dy;
        }
        let heading = if corners.len() >= 2 {
            0.5 * (2.0 * sxy).atan2(sxx - syy)
        } else {
            0.0
        };
        // 沿长轴/短轴量四角跨度。
        let span = |h: f64| {
            let (st, ct) = h.sin_cos();
            let (mut lo, mut hi) = (f64::MAX, f64::MIN);
            for c in &corners {
                let u = (c.lng - cy) * klng * ct + (c.lat - cx) * klat * st;
                lo = lo.min(u);
                hi = hi.max(u);
            }
            hi - lo
        };
        let major = span(heading);
        let minor = span(heading + std::f64::consts::FRAC_PI_2);
        // 真机跑道比四角外框大 ~35%（实测长107/宽52 对 外框78/38）。据此放大，
        // 半径夹到真机实测 20~30m。
        const FIELD_TO_TRACK: f64 = 1.35;
        let radius = (minor * FIELD_TO_TRACK / 2.0).clamp(20.0, 30.0);
        let straight = (major * FIELD_TO_TRACK - 2.0 * radius).clamp(35.0, 140.0);
        let lap = 2.0 * straight + 2.0 * std::f64::consts::PI * radius;
        let spec = crate::track::OvalSpec {
            straight,
            radius,
            heading,
            laps: (p.distance_m / lap).max(1.0),
        };
        log(&format!(
            "[track] 操场绕圈：直道{:.0}m 半径{:.0}m 朝向{:.0}° {:.1}圈，{} 个必经点",
            spec.straight, spec.radius, spec.heading.to_degrees(), spec.laps, cps.len()
        ));
        let mut pts = crate::track::campus_oval_track((cx, cy), &spec, p.sample_step_m, seed);
        // 四角打卡点真实尺寸椭圆天然就在 20m 内；只有「田径场中」这种在椭圆
        // 内部的点可能差 20m+。对这种点用**平滑凸起**而不是硬吸附：把最近那
        // 一圈的一小段点按余弦渐变整体拉向它，形成柔和外凸，不戳尖刺。
        for c in &cps {
            let target = (c.lat, c.lng);
            let (mut bi, mut bd) = (usize::MAX, f64::MAX);
            for (i, q) in pts.iter().enumerate() {
                let d = crate::track::geo::distance(*q, target);
                if d < bd { bd = d; bi = i; }
            }
            // 已在 16m 内就不动；差太远（>45m，可能是坐标异常）也不硬拉。
            if bi == usize::MAX || bd <= 16.0 || bd > 45.0 { continue; }
            let base = pts[bi];
            let (dlat, dlng) = (target.0 - base.0, target.1 - base.1);
            // 凸起半宽：差得越远铺得越宽，保证坡度平缓（每点 ≤ ~2m）
            let w = ((bd / p.sample_step_m).ceil() as usize).clamp(4, 14);
            for k in 0..=(2 * w) {
                let idx = bi as isize - w as isize + k as isize;
                if idx < 0 || idx as usize >= pts.len() { continue; }
                let idx = idx as usize;
                // 余弦窗：中心 1，两端 0
                let t = (k as f64 - w as f64) / w as f64; // -1..1
                let wgt = 0.5 * (1.0 + (std::f64::consts::PI * t).cos());
                pts[idx].0 += dlat * wgt;
                pts[idx].1 += dlng * wgt;
            }
        }
        let n = pts.len();
        let times = crate::track::planner::assign_times(n, start_ms, p.duration_sec);
        let actual = crate::track::geo::path_length(&pts);
        crate::track::PlanResult {
            points: pts
                .into_iter()
                .zip(times)
                .map(|((lat, lng), t)| crate::track::TrackPoint { lat, lon: lng, time_ms: t })
                .collect(),
            checkpoints: cps.clone(),
            actual_length_m: actual,
        }
    } else if fence_ring.len() >= 3 {
        log(&format!(
            "[track] 围栏内规划 {}m，{} 个必经点…",
            p.distance_m,
            cps.len()
        ));
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(if p.seed != 0 { p.seed } else { start_ms as u64 });
        let _ = &mut rng;

        let must: Vec<(f64, f64)> = cps.iter().map(|c| (c.lat, c.lng)).collect();
        // 路线优先级：
        //   ① 沿围栏边界内缩跑——围墙边界一定是能走的路，绝不会落在湖面
        //      或楼里；形状也像人绕着校园跑。
        //   ② 几何蛇形填充——边界周长不够或内缩失败时的兜底。
        //
        // 之前只有 ②，纯几何不看地形，轨迹直接从水面划过去。
        // 服务端判「按顺序通过所有点位」用的是点位数组顺序，那个顺序不是
        // 地理上的环形顺序（实测是锯齿）。所以骨架必须按数组顺序穿点，
        // 绕圈再漂亮也没用——顺序错一条记录就废。
        let seed = if p.seed != 0 { p.seed } else { start_ms as u64 };

        // 优先走真实道路。OSM 在部分校区完全没有数据（实测某校整个 bbox
        // 只有 1 个 landuse 标签），所以这是「能走就走」，失败静默降级到
        // 几何规划——不能因为查不到路网就不跑了。
        // 障碍轮廓：建筑 + 水体。路线生成完后再避让，不能让轨迹从楼里穿过。
        // 数据源是 Overpass（30 天缓存），拉不到就跳过，不阻断跑单。
        let blockers: Vec<Vec<(f64, f64)>> = {
            let bbox = crate::track::roadnet::bbox_of(&fence_ring, 60.0);
            let mut v: Vec<Vec<(f64, f64)>> = Vec::new();
            match crate::track::roadnet::fetch_buildings(bbox) {
                Ok(b) if !b.is_empty() => {
                    log(&format!("[block] 建筑轮廓 {} 栋", b.len()));
                    v.extend(b);
                }
                Ok(_) => log("[block] 这一带没有建筑数据"),
                Err(e) => log(&format!("[block] 建筑轮廓没拿到（{e}），本次不避让")),
            }
            if let Ok(w) = crate::track::roadnet::fetch_water(bbox) {
                if !w.is_empty() {
                    log(&format!("[block] 水域 {} 片", w.len()));
                    v.extend(w);
                }
            }
            // 本地手绘禁区。OSM 在国内校园基本是空白（实测某校区整个 bbox
            // 只有 15 个 way、76 个 node，一栋楼都没有），Overpass 拿不到轮廓
            // 时只能靠人自己在地图上圈。这份标注全账号共享，长期有效。
            let local = crate::track::roadnet::load_local_blockers();
            if !local.is_empty() {
                log(&format!("[block] 本地标注禁区 {} 块", local.len()));
                v.extend(local);
            }
            v
        };

        let on_roads = if p.prefer_roads {
            try_route_on_roads(&fence_ring, &must, p.distance_m, seed, &mut *log)
        } else {
            None
        };
        let route = if let Some(r) = on_roads {
            r
        } else if must.len() >= 2 {
            match crate::track::fence_route::route_through_in_order(
                &fence_ring,
                p.distance_m,
                &must,
                seed,
            ) {
                Ok(r) => {
                    log(&format!(
                        "[track] 按打卡点顺序串 {} 段，{:.0}m",
                        r.lines, r.length_m
                    ));
                    r
                }
                Err(e) => {
                    log(&format!("[track] 按序串点失败（{e}），退回沿边界绕圈"));
                    crate::track::fence_route::route_along_fence(
                        &fence_ring,
                        p.distance_m,
                        25.0,
                        &must,
                    )?
                }
            }
        } else {
            log("[track] 没有打卡点，沿围栏内缩 25m 跑");
            crate::track::fence_route::route_along_fence(&fence_ring, p.distance_m, 25.0, &must)?
        };
        // 沿边界是贴着围墙的折线，拐角磨圆 + 轻微摆动，别像尺子画的
        let rounded = crate::track::fence_route::round_corners(&route.points, 20.0, 6);
        let wandered = crate::track::fence_route::add_wander(
            &rounded,
            6.0,
            120.0,
            if p.seed != 0 { p.seed } else { start_ms as u64 },
        );
        // 建筑避让放在摆动之后：add_wander 会把点横推 ±6m，先避让再摆动
        // 等于白做。这里逐点推出楼体 + 逐段插贴边绕行，外扩 3m 当作墙根。
        let wandered = if blockers.is_empty() {
            wandered
        } else {
            let before = wandered.len();
            let avoided = crate::track::fence_route::avoid_blockers(&wandered, &blockers, 3.0);
            let pushed = avoided.len().saturating_sub(before);
            if pushed > 0 {
                log(&format!("[block] 绕开建筑/水面，多插 {pushed} 个点"));
            }
            avoided
        };
        let sampled = crate::track::fence_route::resample_with_checkpoints(
            &wandered,
            p.sample_step_m,
            &must,
            15.0,
        );
        // 时间戳先占位：⑤ 的 retime 会按速度剖面重算并整体覆盖，
        // 这里算一遍是白算（原来 assign_times 的结果直接被丢弃，
        // 还让人误以为时间来自两处）。占位填 start_ms 即可。
        let pts: Vec<TrackPoint> = sampled
            .iter()
            .map(|&(lat, lng)| TrackPoint {
                lat,
                lon: lng,
                time_ms: start_ms,
            })
            .collect();
        let actual = crate::track::geo::path_length(&sampled);
        PlanResult {
            points: pts,
            checkpoints: cps.clone(),
            actual_length_m: actual,
        }
    } else {
        log(&format!("[track] 规划 {}m，{} 个必经点…", p.distance_m, cps.len()));
        // 没围栏时锚点可能只是调用方的兜底坐标（Web 写死 30.5,104.0），
        // 离真实打卡点上千公里。闭环是「锚点 → 各打卡点 → 锚点」，锚点错了
        // 就会报「最短闭环 1777736m」。打卡点是服务端给的真实位置，以它为准。
        let (center_lat, center_lng) = match points::center_bd(&pt.coords) {
            Some(c)
                if crate::track::geo::distance(c, (p.center_lat, p.center_lng)) > 3000.0 =>
            {
                log(&format!(
                    "⚠ [track] 锚点 ({:.4}, {:.4}) 离打卡点 {:.0}km，改用打卡点中心 ({:.4}, {:.4})",
                    p.center_lat,
                    p.center_lng,
                    crate::track::geo::distance(c, (p.center_lat, p.center_lng)) / 1000.0,
                    c.0,
                    c.1
                ));
                c
            }
            _ => (p.center_lat, p.center_lng),
        };
        let req = PlanRequest {
            center_lat,
            center_lng,
            checkpoints: cps.clone(),
            total_distance_m: p.distance_m,
            total_sec: p.duration_sec,
            start_ms,
            sport_type: p.sport_type,
            sample_step_m: p.sample_step_m,
        };
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(if p.seed != 0 { p.seed } else { start_ms as u64 });
        let _ = &mut rng;
        plan(&req, &mut rng)?
    };
    log(&format!(
        "√ [track] {} 点，实测 {:.1}m（目标 {:.0}m，偏差 {:.2}%）",
        track.points.len(),
        track.actual_length_m,
        p.distance_m,
        (track.actual_length_m - p.distance_m).abs() / p.distance_m * 100.0
    ));

    // ⑤ retime
    //
    // 速度上下界由**服务端策略**的配速区间反推，不再硬编码 1.0~8.0。
    // 8.0 m/s = 2:05 配速，学生账号规则通常只给到 3~10 分/公里，
    // 硬编码会生成超出区间的配速，自检却因为 pace_range=None 报通过。
    let (v_floor, v_ceil) = match pol.pace_range {
        Some((a, b)) => {
            // 分/公里 → m/s；配速越小越快
            let fast = a.min(b).max(0.1);
            let slow = a.max(b).max(fast + 0.01);
            let hi = 1000.0 / (fast * 60.0);
            let lo = 1000.0 / (slow * 60.0);
            log(&format!(
                "[policy] 配速区间 {fast:.1}~{slow:.1} 分/km → 速度 {lo:.2}~{hi:.2} m/s"
            ));
            (lo.clamp(0.5, 10.0), hi.clamp(0.5, 10.0))
        }
        None => (1.0, 8.0),
    };
    let xy: Vec<(f64, f64)> = track.points.iter().map(|q| (q.lat, q.lon)).collect();
    let (times, _speeds, real_len) = retime(
        &xy,
        p.duration_sec,
        start_ms,
        start_ms as u64,
        &KinParams::default(),
        v_floor,
        v_ceil,
    );
    if times.len() != xy.len() {
        return Err("时间戳生成失败".into());
    }
    let timed: Vec<(f64, f64, i64)> = xy
        .iter()
        .zip(&times)
        .map(|((a, b), t)| (*a, *b, *t))
        .collect();
    let track_points: Vec<crate::track::TrackPoint> = xy
        .iter()
        .zip(&times)
        .map(|((a, b), t)| crate::track::TrackPoint {
            lat: *a,
            lon: *b,
            time_ms: *t,
        })
        .collect();
    log(&format!(
        "√ [retime] 时间跨度 {}s → 平均 {:.3} m/s",
        (times[times.len() - 1] - times[0]) / 1000,
        real_len as f64 / p.duration_sec as f64
    ));

    // ⑥ simulate
    let sp = SimParams {
        total_distance_m: real_len as f64,
        total_sec: p.duration_sec,
        start_ms,
        target_steps: p.steps,
        seed: if p.seed != 0 { p.seed } else { start_ms as u64 },
        ..Default::default()
    };
    let sim = simulate(&timed, &sp)?;
    log(&format!(
        "√ [simulate] {} 步，平均步频 {:.1} 步/分，{} 个分桶",
        sim.total_steps,
        sim.avg_cadence,
        sim.step_buckets.len()
    ));

    // ⑦ 自检
    //
    // 校验对象必须是**报文里那份数据**，不是内存里的 sim/params。
    // 之前 preflight 吃的是 sim，而报文是 build() 另外拼的——两边一旦
    // 分叉（历史 bug：桶被重新生成过），自检就在守一份发不出去的数据。
    let cp_tuples: Vec<(f64, f64, String)> =
        cps.iter().map(|c| (c.lat, c.lng, c.name.clone())).collect();
    // 先按真实参数把报文组装出来（纯本地计算，不联网），再把它反解成
    // 校验视图。这一步是「自检守 A、提交发 B」的根治：检查对象从内存结构
    // 换成报文 JSON 反解的结果，两者物理上是同一份数据。
    // uid/unid 用占位符——校验不看身份字段。
    let probe = super::submit::SubmitParams {
        uid: "probe".into(),
        unid: "0".into(),
        uuid: "00000000-0000-4000-8000-000000000000".into(),
        sport_type: p.sport_type,
        total_distance_m: real_len,
        total_time_sec: p.duration_sec,
        total_steps: sim.total_steps,
        start_time_ms: start_ms,
        stop_time_ms: start_ms + p.duration_sec * 1000,
        sel_distance_m: pol.min_distance,
        sel_run_time_sec: p.duration_sec,
        policy: pol.policy.to_string(),
        run_area_id: -1,
        geo_fences_json: "[]".into(),
        total_ascent: 0,
    };
    let probe_sub = super::submit::build(&probe, &track_points, &[], &sim)?;
    let view = super::submit::verify_view(&probe_sub.save_record, &probe_sub.points)?;
    log(&format!(
        "√ [verify] 从报文反解：{} 点 / {:.0}m / {}s / {} 步 / {} 个步数桶",
        view.point_count,
        view.total_distance_m,
        view.total_time_sec,
        view.total_steps,
        view.step_buckets.len()
    ));
    let view_sim = view.as_sim();

    let check = Check {
        points: &view.points,
        sim: &view_sim,
        params: &sp,
        checkpoints: &cp_tuples,
        total_distance_m: view.total_distance_m as i64,
        total_time_sec: view.total_time_sec,
        total_steps: view.total_steps,
        start_time_ms: view.start_time_ms,
        stop_time_ms: view.stop_time_ms,
        pace_range: pol.pace_range,
        step_range: pol.step_range,
    };
    let report = inspect(&check);
    if report.blocked() {
        for line in report.lines() {
            log(&format!("  {line}"));
        }
        return Err(format!("自检未通过：{}", report.summary()));
    }
    log(&format!("√ [preflight] {}", report.summary()));
    for line in report.lines() {
        log(&format!("  {line}"));
    }

    // 服务端「经过点位」到底比哪套坐标？BD-09 的 lat/lon 与 GCJ-02 的
    // glat/glon 差 ~850m，选错一套整条记录的点位项就全是 False。
    let mut order_seen: Vec<(usize, i64, String)> = Vec::new();
    for (i, raw) in pt.points.iter().enumerate() {
        let name = raw.get("pointName").and_then(|v| v.as_str()).unwrap_or("");
        let bd = (
            raw.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0),
            raw.get("lon").and_then(|v| v.as_f64()).unwrap_or(0.0),
        );
        let gcj = (
            raw.get("glat").and_then(|v| v.as_f64()).unwrap_or(0.0),
            raw.get("glon").and_then(|v| v.as_f64()).unwrap_or(0.0),
        );
        let d_bd = view
            .points
            .iter()
            .map(|p| crate::track::geo::distance((p.lat, p.lon), bd))
            .fold(f64::MAX, f64::min);
        let d_gcj = view
            .points
            .iter()
            .map(|p| {
                let (a, o) = crate::api::coord::bd09_to_gcj02(p.lat, p.lon);
                crate::track::geo::distance((a, o), gcj)
            })
            .fold(f64::MAX, f64::min);
        // 首次经过索引：服务端「按顺序通过所有点位」看的就是这个。
        // 距离 0.1m 只说明穿过了，顺序错了一样判 False。
        let first_bd = view
            .points
            .iter()
            .position(|p| crate::track::geo::distance((p.lat, p.lon), bd) < 10.0)
            .map(|x| x as i64)
            .unwrap_or(-1);
        log(&format!(
            "[point-dist] #{i} {name}: BD-09 {d_bd:.1}m / GCJ-02 {d_gcj:.1}m / 首次经过 idx={first_bd}"
        ));
        // 记录每个点位"首次经过"的索引，下面统一检查顺序
        if first_bd >= 0 {
            order_seen.push((i, first_bd, name.to_string()));
        }
    }

    // 点位顺序自检。
    //
    // 服务端的「按顺序通过所有点位」不是看 `fixed_point_json` 里的
    // position，而是自己从轨迹里找出每个点位被经过的位置，再检查这个
    // 序列是否和点位数组同序。也就是说：**position 传得再对也没用，
    // 轨迹本身必须按顺序穿过它们**。
    //
    // 实测踩过的坑：从 #0 到 #1 的那段路径向外扩得太远，中途擦过了 #4。
    // 轨迹实际经过顺序变成 0,4,1,2,3,4,0，服务端直接判 false，而日志里
    // 五个点位距离全是 0.1m、detect 满分区 100%，看起来毫无问题。
    if order_seen.len() >= 2 {
        let mut sorted = order_seen.clone();
        sorted.sort_by_key(|x| x.1);
        let bad: Vec<String> = sorted
            .windows(2)
            .filter(|w| w[1].0 < w[0].0)
            // w[0] 是索引更小的那个，所以是它抢在了 w[1] 前面
            .map(|w| format!("#{} {} 先于 #{} {} 被经过", w[0].0, w[0].2, w[1].0, w[1].2))
            .collect();
        if bad.is_empty() {
            let seq: Vec<String> = sorted.iter().map(|x| format!("#{}@{}", x.0, x.1)).collect();
            log(&format!("[point-order] 顺序正确：{}", seq.join(" → ")));
        } else {
            log(&format!(
                "[point-order] 顺序错误，服务端会判「按顺序通过所有点位」失败：{}",
                bad.join("；")
            ));
            log("[point-order] 重新规划或调整手绘轨迹，让轨迹只按点位次序穿过每个点");
        }
    }

    Ok(RunOutcome {
        policy: pol,
        fence: pick,
        points: pt,
        track_points: track.points.len(),
        actual_length_m: track.actual_length_m,
        total_steps: sim.total_steps,
        avg_cadence: sim.avg_cadence,
        report,
        track: track_points,
        sim,
        start_ms,
        real_length_m: real_len,
        obs_keys: Vec::new(),
        submitted: false,
        record_id: None,
    })
}

/// 完整流程：自检通过后上传明细并提交成绩。
///
/// `confirm` 返回 false 时在提交前中止，已上传的 OBS 对象保留（不影响）。
pub fn run_full(
    client: &Client,
    p: &RunParams,
    log: &mut Log,
    confirm: &mut dyn FnMut(&RunOutcome) -> bool,
) -> Result<RunOutcome, String> {
    // 前七步与 dry_run 完全一致
    let mut out = dry_run(client, p, log)?;

    if out.report.blocked() {
        return Err(format!("自检未通过，不提交：{}", out.report.summary()));
    }
    if !confirm(&out) {
        log("[flow] 已取消，未提交");
        return Ok(out);
    }

    // ❼.5 反检测体检：按 7.4.01 的本地规则先过一遍
    {
        // 必须是轨迹点（400 个），不是打卡点（5 个）
        let xy: Vec<(f64, f64)> = out.track.iter().map(|t| (t.lat, t.lon)).collect();
        let rep = crate::track::detect::inspect(
            &xy,
            &out.sim.speed_buckets,
            crate::track::detect::PointRule::default(),
        );
        log(&format!("[detect] {}", rep.summary()));
        // 服务端「经过点位」到底比哪套坐标？BD-09 与 GCJ-02 差 850m，
        // 选错一套整条记录的点位项就全 False（实测两条记录都这样）。
        for (i, raw) in out.points.points.iter().enumerate() {
            let name = raw.get("pointName").and_then(|v| v.as_str()).unwrap_or("");
            let bd = (
                raw.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0),
                raw.get("lon").and_then(|v| v.as_f64()).unwrap_or(0.0),
            );
            let gcj = (
                raw.get("glat").and_then(|v| v.as_f64()).unwrap_or(0.0),
                raw.get("glon").and_then(|v| v.as_f64()).unwrap_or(0.0),
            );
            let d_bd = xy
                .iter()
                .map(|p| crate::track::geo::distance(*p, bd))
                .fold(f64::MAX, f64::min);
            let d_gcj = xy
                .iter()
                .map(|p| {
                    let (a, o) = crate::api::coord::bd09_to_gcj02(p.0, p.1);
                    crate::track::geo::distance((a, o), gcj)
                })
                .fold(f64::MAX, f64::min);
            log(&format!(
                "[point-dist] #{i} {name}: BD-09 {d_bd:.1}m / GCJ-02 {d_gcj:.1}m"
            ));
        }
        for line in rep.lines().iter().take(5) {
            log(&format!("  · {line}"));
        }
        if rep.blocked() {
            return Err("点位体检未通过，提交会被判异常，已中止".into());
        }
    }

    // ⑧ 组装并提交（必须先提交，OBS 的 objectKey 和桶 id 都要 rrid）
    let start_ms = out.start_ms;
    let uid = client
        .credentials()
        .and_then(|c| c.uid.as_ref())
        .cloned()
        .unwrap_or_default();
    let unid = client
        .credentials()
        .and_then(|c| c.unid.as_ref())
        .cloned()
        .unwrap_or_default();
    let uuid = super::client::uuid_v4().to_uppercase();
    let stop_ms = start_ms + p.duration_sec * 1000;

    // fivePointJson 里的点位坐标用哪套？
    //
    // allLocJson 的 lat/lon 是 BD-09、gLat/gLng 是 GCJ-02，两套都给了。
    // fivePointJson 只有一个 lat/lon——服务端拿它和轨迹的哪一套比，
    // 决定「经过点位」成败，而两套差 850m。可用 PACE_FIXED_GPS=1 切换。
    let pos_is_index = std::env::var("PACE_POS_MODE").map(|v| v == "idx").unwrap_or(true);
    let fixed: Vec<super::submit::FixedPoint> = out
        .points
        .points
        .iter()
        .enumerate()
        .map(|(i, raw)| {
            let bd = (
                raw.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0),
                raw.get("lon").and_then(|v| v.as_f64()).unwrap_or(0.0),
            );
            // position 有两种可能语义：点位序号(1..n) 或该点位在轨迹点
            // 数组里的首次经过索引。默认用后者——服务端要按顺序校验时，
            // 需要知道每个打卡点落在轨迹的第几个采样点上。
            let first_idx = out
                .track
                .iter()
                .position(|t| {
                    crate::track::geo::distance((t.lat, t.lon), bd) < 10.0
                })
                .map(|x| x as i32)
                .unwrap_or(i as i32 + 1);
            super::submit::FixedPoint {
                lat: bd.0,
                lon: bd.1,
                position: if pos_is_index { first_idx } else { i as i32 + 1 },
                is_fixed: raw.get("isFixed").and_then(|v| v.as_i64()).unwrap_or(0),
                point_name: raw
                    .get("pointName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                glat: raw.get("glat").and_then(|v| v.as_f64()).unwrap_or(0.0),
                glon: raw.get("glon").and_then(|v| v.as_f64()).unwrap_or(0.0),
            }
        })
        .collect();

    let geo_fences = out
        .fence
        .as_ref()
        .map(|f| super::fence::canonical_json_for(&f.selected.polygons[0]))
        .unwrap_or_else(|| "[]".into());

    let params = super::submit::SubmitParams {
        uid: uid.clone(),
        unid: unid.clone(),
        uuid: uuid.clone(),
        sport_type: p.sport_type,
        total_distance_m: out.real_length_m,
        total_time_sec: p.duration_sec,
        total_steps: out.total_steps,
        start_time_ms: start_ms,
        stop_time_ms: stop_ms,
        // selDistance 用规则给的最小里程，不是用户设的目标
        sel_distance_m: out.policy.min_distance,
        sel_run_time_sec: p.duration_sec,
        policy: out.policy.policy.to_string(),
        run_area_id: out
            .fence
            .as_ref()
            .map(|f| f.selected.run_area_id)
            .unwrap_or(-1),
        geo_fences_json: geo_fences,
        total_ascent: 0,
    };

    // 报文的桶必须来自 sim，不能在这里重新生成——否则自检、提交、明细
    // 三处数据不一致（见 submit::build 的注释）。
    let submission = super::submit::build(&params, &out.track, &fixed, &out.sim)?;

    // 7.4.01 已移除 BODY_SENSORS，报文里不许出现心率字段
    let mut body = submission.save_record.clone();
    let dropped = crate::track::detect::strip_forbidden(&mut body);
    if !dropped.is_empty() {
        log(&format!("[detect] 已剔除不该有的字段: {}", dropped.join(", ")));
    }
    // 派生字段互校：爬升 / 卡路里 / 功率 / 配速 / 圈数据必须与点位列自洽。
    // 这些都是「填错了也能提交成功」的字段，回退成常量只会让服务端把整条
    // 记录判成来源可疑，所以宁可在这里拦下。
    let derived_problems = super::submit::audit_derived(&body, &submission.points);
    if !derived_problems.is_empty() {
        return Err(format!(
            "报文派生字段自检失败（未提交）：{}",
            derived_problems.join("；")
        ));
    }
    log("√ [selfcheck] 派生字段与点位列自洽（爬升/卡路里/功率/配速/圈）");
    log(&format!(
        "√ [submit] 报文已组装，{} 个轨迹点",
        out.track.len()
    ));

    log("[submit] 提交成绩…");
    // runes/runef 关联头：策略下发时刻+uid、运行uuid+开始时刻
    let runes = format!("{}{}", out.policy.timestamp, uid);
    let runef = format!("{}{}", uuid, start_ms);
    let data = super::submit::save_record(client, &body, &runes, &runef)?;
    out.submitted = true;
    out.record_id = super::submit::extract_record_id(&data);
    log(&format!("√ [submit] 提交成功，记录 id={:?}", out.record_id));

    // ⑨ OBS：用 rrid 重算桶 id 和 objectKey
    let rrid = out.record_id.unwrap_or(0);
    log(&format!("[obs] 用 rrid={rrid} 上传明细…"));
    // OBS 的 fixed_point_json 要的是裸点位数组，不是主报文那层 envelope。
    // 之前把 envelope 整个塞进来，服务端解不出点位列表。
    // OBS 的 fixed_point_json 要的是**完整 envelope**（含 fivePointJson 内层串、
    // useZip、runAreaId、geoFencesJson、freedomShowFence），不是裸点位数组。
    // 参照实现 Neko 的 build_obs_object 就是这么发的；传裸数组时服务端解析
    // 不出点位，于是「按顺序通过所有点位」永远 false。
    let five_pt = body
        .get("fivePointJson")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let payload = detail_payload_rrid(&out.track, &out.sim, p, &uid, rrid, start_ms, &uuid, &five_pt);
    // 本地时区和 UTC 各传一份：服务端拼 key 用哪个时区没有确定证据，
    // 猜错就等于轨迹对象不存在，记录点开只有标量数据。
    let keys = vec![
        super::obs::detail_key(start_ms, &uuid),
        super::obs::detail_key_utc(start_ms, &uuid),
        format!("run_data/{}/{rrid}.json", rrid / 1_000_000),
    ];
    let bytes = serde_json::to_vec(&payload).map_err(|e| format!("明细序列化失败: {e}"))?;
    let (ok, errs) = obs::upload_keys(client, &keys, &bytes);
    for e in &errs {
        log(&format!("  ⚠ {e}"));
    }
    if ok == 0 {
        log("⚠ [obs] 明细上传全部失败，记录已提交但详情页可能无轨迹");
    } else {
        log(&format!("√ [obs] {ok}/{} 个 key 上传成功", keys.len()));
    }
    out.obs_keys = keys;

    log(&format!(
        "  静默窗口 {} 秒内不要再发跑步请求",
        SILENCE_SEC
    ));

    Ok(out)
}

/// 提交后必须保持静默的时长（秒）。
///
/// 服务端会在提交后一段时间内拒绝新的跑步请求。提前再发就会被判异常。
pub const SILENCE_SEC: i64 = 600;

/// 随机配速区间（分/km）。取 5 个账号真机记分跑的整体范围（p10~p90 ≈
/// 5.4~7.9），2.0~2.4km 对应时长约 11~19 分钟，不再固定 12 分钟。
pub const PACE_RANGE: (f64, f64) = (5.6, 7.8);

/// 按种子随机一组自洽的 (距离 km, 时长 min)。
///
/// 距离 2.0~2.4km；配速在 PACE_RANGE 里随机，时长 = 距离 × 配速（精确到秒），
/// 步数由配速推导，三者保持 `v = 步频/60 × 步幅` 自洽。
pub fn random_run_spec(seed: u64) -> (f64, f64) {
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let dist = (rng.gen_range(2.00_f64..=2.40) * 100.0).round() / 100.0;
    let p = rng.gen_range(PACE_RANGE.0..=PACE_RANGE.1);
    let dur = (dist * p * 60.0).round() / 60.0; // 分钟，落到整秒
    (dist, dur)
}

/// 判断距离上次提交是否已过静默窗口。
pub fn silence_ok(last_submit_ms: i64, now_ms: i64) -> bool {
    now_ms - last_submit_ms >= SILENCE_SEC * 1000
}

/// 构造 OBS 明细载荷。
///
/// 服务端详情页会读这个对象来画轨迹，字段名要和 App 一致。
pub fn detail_payload(
    points: &[crate::track::TrackPoint],
    sim: &crate::track::SimResult,
    p: &RunParams,
    uid: &str,
) -> Value {
    let route: Vec<Value> = points
        .iter()
        .map(|q| {
            json!({
                "lat": q.lat,
                "lon": q.lon,
                "time": q.time_ms,
            })
        })
        .collect();

    let speed_per_ten: Vec<Value> = sim
        .speed_buckets
        .iter()
        .map(|b| {
            json!({
                "beginTime": b.begin_time,
                "endTime": b.end_time,
                "distance": b.distance,
            })
        })
        .collect();
    let steps_per_ten: Vec<Value> = sim
        .step_buckets
        .iter()
        .map(|b| {
            json!({
                "beginTime": b.begin_time,
                "endTime": b.end_time,
                "stepsNum": b.steps_num,
                "maxDiff": b.max_diff,
                "minDiff": b.min_diff,
                "avgDiff": b.avg_diff,
            })
        })
        .collect();

    json!({
        "uid": uid,
        "sportType": p.sport_type,
        "totalDistance": sim.actual_length_m,
        "totalTime": p.duration_sec,
        "totalSteps": sim.total_steps,
        "route": route,
        "speedPerTenSec": speed_per_ten,
        "stepsPerTenSec": steps_per_ten,
    })
}

/// 带 rrid 的明细载荷（OBS 用，桶 id 走 `(rrid%100000)*1000+endSec`）。
// OBS 明细要 10 个字段，每个都来自不同的数据源（轨迹、模拟结果、
// 账号标识、时间窗口、uuid、点位 JSON），拆结构体只会把这些东西
// 再包一层而不会减少耦合，所以这里保持平铺。
#[allow(clippy::too_many_arguments)]
pub fn detail_payload_rrid(
    points: &[crate::track::TrackPoint],
    sim: &crate::track::SimResult,
    _p: &RunParams,
    uid: &str,
    rrid: i64,
    start_ms: i64,
    uuid: &str,
    five_point_json: &str,
) -> Value {
    let mut speed = sim.speed_buckets.clone();
    let mut steps = sim.step_buckets.clone();
    crate::track::buckets::apply_obs_ids(&mut speed, &mut steps, rrid, start_ms);

    // OBS 对象是 10 个 key，每个值都 gzip + base64。
    // 之前传的是明文 JSON，结构也不对，服务端读不出轨迹，详情页就显示
    // 「这条记录没有轨迹坐标」。
    // OBS 点数集和提交用同一套 28 字段，区别只在 id / queueNum
    let pts: Vec<Value> = {
        let n = points.len();
        let mut prev = points.first().map(|q| (q.lat, q.lon)).unwrap_or((0.0, 0.0));
        let start_pt = prev;
        let mut cum = (0.0f64, 0i64, 0i64);
        let mut prev_time = points.first().map(|q| q.time_ms).unwrap_or(start_ms);
        // 打卡点索引从同一个 fixed_point_json 里取，保证提交和明细两处
        // 对「哪个点是打卡点」的判断一致
        let cp_idxs: Vec<usize> = serde_json::from_str::<Value>(five_point_json)
            .ok()
            // 传进来的是 envelope 时取内层 fivePointJson，是裸数组时直接用
            .and_then(|v| {
                v.get("fivePointJson")
                    .and_then(|x| x.as_str())
                    .and_then(|t| serde_json::from_str::<Value>(t).ok())
                    .or(Some(v))
            })
            .and_then(|v| v.as_array().cloned())
            .map(|arr| {
                arr.iter()
                    .filter_map(|f| f.get("position").and_then(|x| x.as_i64()))
                    .filter(|&x| x >= 0 && (x as usize) < n)
                    .map(|x| x as usize)
                    .collect()
            })
            .unwrap_or_default();
        points
            .iter()
            .enumerate()
            .map(|(i, q)| {
                super::submit::point_json_full(
                    q, i, n, start_ms, start_pt, &mut prev, &mut cum, &mut prev_time,
                    sim.total_steps as i64, start_ms as u64, &cp_idxs,
                )
            })
            .collect()
    };
    // 圈数据：真机每 1000m 切一圈（末圈 isFullLap=false）。原来发空数组，
    // 等于告诉服务端「这条轨迹没有分段信息」——而圈配速、分段审核都读它。
    // 与点位同源：直接从即将上传的 pts 反解 totalDis/totalTime/steps/bdA。
    let laps = super::submit::build_laps_from_points(&pts, start_ms);
    let run_wrap = json!({
        "allLocJson": Value::Array(pts).to_string(),
        "useZip": false,
    });

    json!({
        "rrid": gz_str(&rrid.to_string()),
        "uuid": gz_str(uuid),
        "uid": gz_str(uid),
        "run_data": gz_json(&run_wrap),
        "fixed_point_json": gz_str(five_point_json),
        // 原来给空字符串。App 的 RunData 表里这列是 `TEXT NOT NULL`，
        // 真实记录必定有内容；`""` 不是合法 JSON，服务端解析分段时
        // 会直接失败，点位判定（"经过必经点位"/"至少通过2个普通点位"）
        // 就永远拿不到依据。至少给一个空数组让它能解析。
        "segment_json": gz_json(&Value::Array(Vec::<Value>::new())),
        "speed_json": gz_json(&Value::Array(
            speed.iter().map(|b| serde_json::to_value(b).unwrap_or(Value::Null)).collect::<Vec<_>>()
        )),
        "step_freq_json": gz_json(&Value::Array(
            steps.iter().map(|b| serde_json::to_value(b).unwrap_or(Value::Null)).collect::<Vec<_>>()
        )),
        "laps_json": gz_json(&Value::Array(laps)),
        // 真机是 11 个键，多这一个。Inverx 的 7.3.92 builder.py:276-288
        // 和 ydsj-auto 的 gen_sw.py 两处独立实现对拍都确认 OBS_OBJECT_KEYS
        // 含 extension_json；键数本身是服务端 schema 校验项，少一个可能
        // 在详情页或审核链被静默丢弃。
        // extension_json 是**空串**的 gzip，不是 `{}` 的 gzip。
        //
        // gzip.compress(b"{}") 和 gzip.compress(b"") 差好几个字节，服务端
        // OBS 侧 schema 校验按长度/内容做比对时对不上，整条 run_data 会被
        // 判成「来源不明」，采样点连带被统一打上「未通过校验」的标记。
        // Inverx 7.3.92 running/builder.py:276-288 给的就是空串。
        "extension_json": gz(b""),
        "runFaceCheck": gz_str(""),
    })
}

/// gzip + base64。服务端解这层压缩才读得到内容。
fn gz(bytes: &[u8]) -> String {
    use base64::Engine;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    let _ = e.write_all(bytes);
    let out = e.finish().unwrap_or_default();
    base64::engine::general_purpose::STANDARD.encode(out)
}

fn gz_str(v: &str) -> String {
    gz(v.as_bytes())
}

fn gz_json(v: &Value) -> String {
    gz(v.to_string().as_bytes())
}


/// 生成两个 objectKey 并上传。
pub fn upload_detail(
    client: &Client,
    payload: &Value,
    p: &RunParams,
    start_ms: i64,
    uid: &str,
    log: &mut Log,
) -> Result<Vec<String>, String> {
    let uuid = super::client::uuid_v4();
    let keys = vec![
        obs::detail_key(start_ms, &uuid),
        obs::fallback_key(uid, p.sport_type, start_ms / 1000, &uuid),
    ];
    let bytes = serde_json::to_vec(payload).map_err(|e| format!("明细序列化失败: {e}"))?;
    log(&format!("[obs] 上传明细 {} 字节…", bytes.len()));
    let (ok, errs) = obs::upload_keys(client, &keys, &bytes);
    for e in &errs {
        log(&format!("  ⚠ {e}"));
    }
    if ok == 0 {
        return Err("OBS 全部 key 上传失败".into());
    }
    log(&format!("√ [obs] {ok}/{} 个 key 上传成功", keys.len()));
    Ok(keys)
}

#[cfg(test)]
mod obs_tests {
    use super::*;
    use base64::Engine;

    /// OBS 对象必须是 10 个键，且每个值都是 gzip+base64。
    ///
    /// 之前传的是明文 JSON，键名也不同，服务端读不出轨迹，详情页显示
    /// 「这条记录没有轨迹坐标」。
    #[test]
    fn obs_payload_is_eleven_gzipped_keys() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let p = RunParams { distance_m: 1000.0, duration_sec: 300, ..Default::default() };
        let pts: Vec<crate::track::TrackPoint> = (0..50)
            .map(|i| crate::track::TrackPoint {
                lat: 31.23 + i as f64 * 5.0 / 111320.0,
                lon: 121.47,
                time_ms: 1_793_966_000_000 + i as i64 * 1500,
            })
            .collect();
        let sim = crate::track::SimResult {
            speeds: vec![3.0; 50],
            cadences: vec![150.0; 50],
            strides: vec![1.2; 50],
            total_steps: 300,
            avg_cadence: 150.0,
            speed_buckets: vec![],
            step_buckets: vec![],
            actual_length_m: 1000.0,
        };
        let v = detail_payload_rrid(&pts, &sim, &p, "123", 99, 1_793_966_000_000, "U", "[]");

        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 11, "必须是 11 个键，实际 {keys:?}");
        for k in ["rrid", "uuid", "uid", "run_data", "fixed_point_json",
                  "segment_json", "speed_json", "step_freq_json", "laps_json", "runFaceCheck"] {
            assert!(v.get(k).is_some(), "缺 key: {k}");
        }

        // 每个值都要能 base64 解码 + gzip 解压
        for k in keys {
            let raw = v[k].as_str().unwrap_or_else(|| panic!("{k} 不是字符串"));
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(raw)
                .unwrap_or_else(|e| panic!("{k} base64 解码失败: {e}"));
            assert_eq!(&bytes[..2], &[31, 139], "{k} 不是 gzip 头");
            let mut out = String::new();
            GzDecoder::new(&bytes[..])
                .read_to_string(&mut out)
                .unwrap_or_else(|e| panic!("{k} gzip 解压失败: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_window() {
        let t0 = 1_730_000_000_000i64;
        assert!(silence_ok(t0, t0 + 600_000));
        assert!(silence_ok(t0, t0 + 700_000));
        assert!(!silence_ok(t0, t0 + 599_999));
        assert!(!silence_ok(t0, t0));
    }

    #[test]
    fn detail_payload_has_required_fields() {
        let pts = vec![
            crate::track::TrackPoint { lat: 30.5, lon: 104.0, time_ms: 1000 },
            crate::track::TrackPoint { lat: 30.5001, lon: 104.0, time_ms: 2000 },
        ];
        let sim = crate::track::SimResult {
            speeds: vec![2.0],
            cadences: vec![150.0],
            strides: vec![0.8],
            total_steps: 100,
            avg_cadence: 150.0,
            speed_buckets: vec![],
            step_buckets: vec![],
            actual_length_m: 11.1,
        };
        let p = RunParams::default();
        let v = detail_payload(&pts, &sim, &p, "12345");
        for k in ["uid", "sportType", "totalDistance", "totalTime", "totalSteps", "route"] {
            assert!(v.get(k).is_some(), "缺字段 {k}");
        }
        assert_eq!(v["route"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn default_params_are_sane() {
        let p = RunParams::default();
        assert!(p.distance_m > 0.0);
        assert!(p.duration_sec > 0);
        // 配速应该在合理区间
        let pace = 1000.0 / (p.distance_m / p.duration_sec as f64) / 60.0;
        assert!((3.0..=12.0).contains(&pace), "默认配速 {pace:.2} 不合理");
    }
}

#[cfg(test)]
mod fixed_point_payload_tests {
    /// OBS 的 fixed_point_json 必须是裸点位数组。
    ///
    /// 之前把主报文那层 envelope（{fivePointJson, useZip, runAreaId, ...}）
    /// 整个塞进去，服务端解不出点位列表，`按顺序通过所有点位` 永远 False。
    #[test]
    fn fixed_point_json_is_bare_array() {
        let raw = r#"[{"lon":123.9,"lat":47.3,"position":1}]"#;
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(v.is_array(), "必须是数组");
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert!(v[0].get("position").is_some());
    }

    /// 从 envelope 里抽裸数组：主报文是双层，OBS 要单层。
    #[test]
    fn envelope_unwraps_to_bare_array() {
        let inner = r#"[{"lon":1.0,"lat":2.0,"position":3}]"#;
        let envelope = serde_json::json!({
            "fivePointJson": inner,
            "useZip": false,
            "runAreaId": 1668,
        })
        .to_string();
        let extracted = serde_json::from_str::<serde_json::Value>(&envelope)
            .ok()
            .and_then(|v| v.get("fivePointJson").cloned())
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
        assert_eq!(extracted, inner);
        let arr: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert!(arr.is_array());
    }
}
