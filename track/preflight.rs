//! 提交前自检。
//!
//! 目标：**宁可本地失败，也不把注定被驳回的报文发出去**。
//!
//! 服务端驳回一次不只是失败——错误次数会被记录，异常次数多了会触发风控，
//! 后续正常提交也可能被拦。所以能在本地拦住的，绝不放到线上试。
//!
//! 分三级：
//! - `Fatal`：必然被驳回，直接阻断（里程对不上、时间戳错位、打卡点未命中）
//! - `Warn`：可能被驳回，需要人工确认（配速过快、步频异常）
//! - `Info`：仅提示（数据看起来不太自然，但合规）

use super::biomech::gait;
use super::geo::distance;
use super::simulate::{SimParams, SimResult};
use super::TrackPoint;

/// 问题等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// 仅提示
    Info,
    /// 可能被驳回
    Warn,
    /// 必然被驳回
    Fatal,
}

/// 一条检查结果。
#[derive(Debug, Clone)]
pub struct Issue {
    pub level: Level,
    pub code: &'static str,
    pub message: String,
}

impl Issue {
    fn fatal(code: &'static str, message: impl Into<String>) -> Self {
        Self { level: Level::Fatal, code, message: message.into() }
    }
    fn warn(code: &'static str, message: impl Into<String>) -> Self {
        Self { level: Level::Warn, code, message: message.into() }
    }
    fn info(code: &'static str, message: impl Into<String>) -> Self {
        Self { level: Level::Info, code, message: message.into() }
    }
}

/// 自检报告。
#[derive(Debug, Clone)]
pub struct Report {
    pub issues: Vec<Issue>,
}

impl Report {
    /// 有没有致命问题（有就不能提交）。
    pub fn blocked(&self) -> bool {
        self.issues.iter().any(|i| i.level == Level::Fatal)
    }

    pub fn fatal_count(&self) -> usize {
        self.issues.iter().filter(|i| i.level == Level::Fatal).count()
    }

    pub fn warn_count(&self) -> usize {
        self.issues.iter().filter(|i| i.level == Level::Warn).count()
    }

    /// 一句话摘要。
    pub fn summary(&self) -> String {
        if self.issues.is_empty() {
            return "全部通过".into();
        }
        let f = self.fatal_count();
        let w = self.warn_count();
        let i = self.issues.len() - f - w;
        format!("{f} 致命 / {w} 警告 / {i} 提示")
    }

    /// 逐行文本，供日志或界面展示。
    pub fn lines(&self) -> Vec<String> {
        self.issues
            .iter()
            .map(|i| {
                let tag = match i.level {
                    Level::Fatal => "✗ 致命",
                    Level::Warn => "! 警告",
                    Level::Info => "· 提示",
                };
                format!("{tag} [{}] {}", i.code, i.message)
            })
            .collect()
    }
}

/// 自检输入。
pub struct Check<'a> {
    pub points: &'a [TrackPoint],
    pub sim: &'a SimResult,
    pub params: &'a SimParams,
    /// 必过的打卡点（纬度, 经度, 名称）。
    pub checkpoints: &'a [(f64, f64, String)],
    /// 报文里的总里程。
    pub total_distance_m: i64,
    /// 报文里的总时长。
    pub total_time_sec: i64,
    /// 报文里的总步数。
    pub total_steps: i32,
    /// 报文里的起止时间戳。
    pub start_time_ms: i64,
    pub stop_time_ms: i64,
    /// 服务端规则允许的配速区间（分/公里），`None` 表示不校验。
    pub pace_range: Option<(f64, f64)>,
    /// 服务端规则允许的步频区间（步/分钟），`None` 表示不校验。
    ///
    /// 原来这里硬编码 [100, 230]，而服务端实发 60~250。拿着一把更窄的
    /// 假尺子量自己，会误拦合法步频，也放过了真正越界的区间。
    pub step_range: Option<(i64, i64)>,
}

/// 执行全部检查。
pub fn inspect(c: &Check) -> Report {
    let mut issues = Vec::new();

    // ── 一、时间轴 ──
    if c.start_time_ms <= 0 || c.stop_time_ms <= 0 {
        issues.push(Issue::fatal("TIME_MISSING", "起止时间戳缺失"));
    } else if c.stop_time_ms <= c.start_time_ms {
        issues.push(Issue::fatal(
            "TIME_ORDER",
            format!("stopTime({}) 不晚于 startTime({})", c.stop_time_ms, c.start_time_ms),
        ));
    } else {
        let span = (c.stop_time_ms - c.start_time_ms) / 1000;
        if (span - c.total_time_sec).abs() > 2 {
            issues.push(Issue::fatal(
                "TIME_SPAN",
                format!("时间跨度 {span}s 与 totalTime {}s 不一致", c.total_time_sec),
            ));
        }
    }

    if c.points.is_empty() {
        issues.push(Issue::fatal("TRACK_EMPTY", "轨迹为空"));
        return Report { issues };
    }

    // 首尾时间戳必须精确落在报文声明的边界上
    let first_t = c.points.first().map(|p| p.time_ms).unwrap_or(0);
    let last_t = c.points.last().map(|p| p.time_ms).unwrap_or(0);
    if first_t != c.start_time_ms {
        issues.push(Issue::fatal(
            "TRACK_START",
            format!("首点时间 {first_t} ≠ startTime {}", c.start_time_ms),
        ));
    }
    if last_t != c.stop_time_ms {
        issues.push(Issue::fatal(
            "TRACK_STOP",
            format!("末点时间 {last_t} ≠ stopTime {}", c.stop_time_ms),
        ));
    }

    // 时间必须严格递增（重复时间戳会让速度无穷大）
    let mut dup = 0;
    for w in c.points.windows(2) {
        if w[1].time_ms <= w[0].time_ms {
            dup += 1;
        }
    }
    if dup > 0 {
        issues.push(Issue::fatal(
            "TRACK_TIME_ORDER",
            format!("{dup} 处时间戳未严格递增"),
        ));
    }

    // ── 二、里程 ──
    let xy: Vec<(f64, f64)> = c.points.iter().map(|p| (p.lat, p.lon)).collect();
    let measured = super::geo::path_length(&xy);
    let declared = c.total_distance_m as f64;
    if (measured - declared).abs() > 2.0 {
        issues.push(Issue::fatal(
            "DIST_MISMATCH",
            format!("折线实测 {measured:.1}m 与 totalDis {declared:.0}m 不符"),
        ));
    }

    // ── 三、横坐标合法性 ──
    let bad_geo = c
        .points
        .iter()
        .filter(|p| {
            !(-90.0..=90.0).contains(&p.lat)
                || !(-180.0..=180.0).contains(&p.lon)
                || (p.lat == 0.0 && p.lon == 0.0)
        })
        .count();
    if bad_geo > 0 {
        issues.push(Issue::fatal(
            "GEO_INVALID",
            format!("{bad_geo} 个坐标越界或为 (0,0)"),
        ));
    }

    // 相邻点跳变（传送门式瞬移，会被判异常）
    let mut max_jump = 0.0f64;
    for w in c.points.windows(2) {
        let d = distance((w[0].lat, w[0].lon), (w[1].lat, w[1].lon));
        if d > max_jump {
            max_jump = d;
        }
    }
    if max_jump > 200.0 {
        issues.push(Issue::fatal(
            "GEO_JUMP",
            format!("相邻点最大跳变 {max_jump:.0}m，超过 200m 会被判定瞬移"),
        ));
    } else if max_jump > 60.0 {
        issues.push(Issue::warn(
            "GEO_JUMP_SOFT",
            format!("相邻点最大跳变 {max_jump:.0}m，偏大"),
        ));
    }

    // ── 四、打卡点 ──
    for (lat, lng, name) in c.checkpoints {
        let best = c
            .points
            .iter()
            .map(|p| distance((p.lat, p.lon), (*lat, *lng)))
            .fold(f64::INFINITY, f64::min);
        if best > 0.5 {
            issues.push(Issue::fatal(
                "CP_MISS",
                format!("打卡点 {name} 最近距离 {best:.1}m，未命中（要求 ≤0.5m）"),
            ));
        }
    }

    // ── 五、分桶 ──
    let expect_buckets = (c.total_time_sec / 10).max(1) as usize;
    if c.sim.speed_buckets.len() != expect_buckets {
        issues.push(Issue::fatal(
            "BUCKET_COUNT",
            format!(
                "速度桶 {} 个，应为 {expect_buckets} 个",
                c.sim.speed_buckets.len()
            ),
        ));
    }
    let step_sum: i32 = c.sim.step_buckets.iter().map(|b| b.steps_num).sum();
    if step_sum != c.total_steps {
        issues.push(Issue::fatal(
            "BUCKET_STEPS",
            format!("步数分桶和 {step_sum} ≠ totalSteps {}", c.total_steps),
        ));
    }
    // 空桶：连续多个 10 秒完全没位移，服务端会判定为静止
    let empty = c
        .sim
        .speed_buckets
        .iter()
        .filter(|b| b.distance < 0.5)
        .count();
    if empty > 0 {
        issues.push(Issue::warn(
            "BUCKET_EMPTY",
            format!("{empty} 个速度桶位移不足 0.5m，接近静止"),
        ));
    }

    // 分桶自相关过低的提示（白噪声特征）
    let counts: Vec<f64> = c.sim.step_buckets.iter().map(|b| b.steps_num as f64).collect();
    if counts.len() >= 8 {
        let mean = counts.iter().sum::<f64>() / counts.len() as f64;
        let den: f64 = counts.iter().map(|x| (x - mean).powi(2)).sum();
        if den > 1e-9 {
            let num: f64 = counts.windows(2).map(|w| (w[0] - mean) * (w[1] - mean)).sum();
            let ac = num / den;
            if ac < 0.1 {
                issues.push(Issue::info(
                    "BUCKET_AC_LOW",
                    format!("步数一阶自相关 {ac:.3} 偏低，序列接近白噪声"),
                ));
            }
        }
    }

    // ── 六、物理合理性 ──
    let avg_v = c.total_distance_m as f64 / c.total_time_sec.max(1) as f64;
    // 配速：分/公里
    let pace = 1000.0 / avg_v / 60.0;
    if let Some((lo, hi)) = c.pace_range {
        // 注意：规则里的 speedTop/speedBottom 是分/公里，数值越大越慢
        let (fast, slow) = (lo.min(hi), lo.max(hi));
        if pace < fast || pace > slow {
            issues.push(Issue::fatal(
                "PACE_OUT",
                format!("配速 {pace:.2} 分/km 超出允许区间 [{fast}, {slow}]"),
            ));
        }
    }
    if pace < 3.0 {
        issues.push(Issue::warn(
            "PACE_FAST",
            format!("配速 {pace:.2} 分/km 快得离谱（世界纪录约 2.83）"),
        ));
    }

    let avg_cad = c.total_steps as f64 * 60.0 / c.total_time_sec.max(1) as f64;
    let (cad_lo, cad_hi) = c.step_range.unwrap_or((100, 230));
    if !((cad_lo as f64)..=(cad_hi as f64)).contains(&avg_cad) {
        issues.push(Issue::fatal(
            "CADENCE_OUT",
            format!("平均步频 {avg_cad:.0} 步/分 超出服务端区间 [{cad_lo}, {cad_hi}]"),
        ));
    }

    // 步态自洽：用报告的步数反跑步态，看是否与速度对得上
    let g = gait(avg_v, 0.0);
    let implied = c.total_steps as f64 * 60.0 / c.total_time_sec.max(1) as f64;
    if (implied - g.cadence).abs() > 25.0 {
        issues.push(Issue::warn(
            "GAIT_MISMATCH",
            format!(
                "步频 {implied:.0} 与该速度下的典型步频 {:.0} 差 {:.0} 步",
                g.cadence,
                (implied - g.cadence).abs()
            ),
        ));
    }

    // 步幅
    let stride = if implied > 0.0 { avg_v * 60.0 / implied } else { 0.0 };
    if !(0.4..=1.8).contains(&stride) {
        issues.push(Issue::warn(
            "STRIDE_OUT",
            format!("推算步幅 {stride:.2}m 超出常见范围"),
        ));
    }

    // ── 七、里程与时长的一致性 ──
    if c.total_time_sec <= 0 {
        issues.push(Issue::fatal("DURATION", "totalTime 必须为正"));
    }
    if c.total_distance_m <= 0 {
        issues.push(Issue::fatal("DISTANCE", "totalDis 必须为正"));
    }
    if (c.sim.actual_length_m - c.total_distance_m as f64).abs() > 2.0 {
        issues.push(Issue::fatal(
            "SIM_DIST",
            format!(
                "合成里程 {:.1}m 与报文 {}m 不符",
                c.sim.actual_length_m, c.total_distance_m
            ),
        ));
    }

    Report { issues }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::track::{plan, simulate, Checkpoint, PlanRequest};

    fn sample() -> (Vec<TrackPoint>, SimResult, SimParams) {
        let req = PlanRequest {
            center_lat: 30.5,
            center_lng: 104.0,
            checkpoints: vec![
                Checkpoint { lat: 30.5008, lng: 104.0008, glat: 30.5008, glon: 104.0008, name: "A".into() },
                Checkpoint { lat: 30.5008, lng: 104.0024, glat: 30.5008, glon: 104.0024, name: "B".into() },
            ],
            total_distance_m: 2000.0,
            total_sec: 720,
            start_ms: 1_730_000_000_000,
            sport_type: 1,
            sample_step_m: 5.0,
        };
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        use rand::SeedableRng;
        let t = plan(&req, &mut rng).unwrap();
        let pts: Vec<(f64, f64, i64)> =
            t.points.iter().map(|p| (p.lat, p.lon, p.time_ms)).collect();
        let sp = SimParams {
            total_distance_m: t.actual_length_m,
            total_sec: 720,
            start_ms: 1_730_000_000_000,
            target_steps: Some(2300),
            seed: 1,
            ..Default::default()
        };
        let sim = simulate(&pts, &sp).unwrap();
        (t.points, sim, sp)
    }

    fn check<'a>(
        pts: &'a [TrackPoint],
        sim: &'a SimResult,
        sp: &'a SimParams,
        cps: &'a [(f64, f64, String)],
    ) -> Check<'a> {
        Check {
            points: pts,
            sim,
            params: sp,
            checkpoints: cps,
            // 必须和合成轨迹一致：SIM_DIST 检查的是「报文里程 vs 轨迹实测」，
            // 两者本来就该相等（flow 里报文直接用 real_len）。
            total_distance_m: sim.actual_length_m.round() as i64,
            total_time_sec: 720,
            total_steps: 2300,
            start_time_ms: 1_730_000_000_000,
            stop_time_ms: 1_730_000_720_000,
            pace_range: None,
            step_range: None,
        }
    }

    #[test]
    fn healthy_track_passes() {
        let (pts, sim, sp) = sample();
        let cps = vec![
            (30.5008, 104.0008, "A".to_string()),
            (30.5008, 104.0024, "B".to_string()),
        ];
        let r = inspect(&check(&pts, &sim, &sp, &cps));
        assert!(!r.blocked(), "正常轨迹不该被阻断: {:?}", r.lines());
    }

    #[test]
    fn detects_dist_mismatch() {
        let (pts, sim, sp) = sample();
        let mut c = check(&pts, &sim, &sp, &[]);
        c.total_distance_m = 3000; // 谎报里程
        let r = inspect(&c);
        assert!(r.blocked());
        assert!(r.issues.iter().any(|i| i.code == "DIST_MISMATCH"));
    }

    #[test]
    fn detects_time_misalignment() {
        let (pts, sim, sp) = sample();
        let mut c = check(&pts, &sim, &sp, &[]);
        c.start_time_ms += 5000; // 首点对不上
        let r = inspect(&c);
        assert!(r.blocked());
        assert!(r.issues.iter().any(|i| i.code == "TRACK_START"));
    }

    #[test]
    fn detects_missed_checkpoint() {
        let (pts, sim, sp) = sample();
        let cps = vec![(31.0, 105.0, "远在天边".to_string())];
        let r = inspect(&check(&pts, &sim, &sp, &cps));
        assert!(r.blocked());
        assert!(r.issues.iter().any(|i| i.code == "CP_MISS"));
    }

    #[test]
    fn detects_pace_out_of_range() {
        let (pts, sim, sp) = sample();
        let mut c = check(&pts, &sim, &sp, &[]);
        c.pace_range = Some((9.0, 12.0)); // 只允许极慢
        let r = inspect(&c);
        assert!(r.blocked());
        assert!(r.issues.iter().any(|i| i.code == "PACE_OUT"));
    }

    #[test]
    fn detects_step_sum_mismatch() {
        let (pts, sim, sp) = sample();
        let mut c = check(&pts, &sim, &sp, &[]);
        c.total_steps = 9999;
        let r = inspect(&c);
        assert!(r.blocked());
        assert!(r.issues.iter().any(|i| i.code == "BUCKET_STEPS"));
    }

    #[test]
    fn report_summary_readable() {
        let (pts, sim, sp) = sample();
        let mut c = check(&pts, &sim, &sp, &[]);
        c.total_distance_m = 9999;
        let r = inspect(&c);
        assert!(!r.summary().is_empty());
        assert!(!r.lines().is_empty());
        assert!(r.lines()[0].contains("致命"));
    }
}
