//! 把运动学与生物力学合成一条「像人跑出来的」数据流。
//!
//! 前两代做法的问题：
//!
//! **第一代**：步数按 `距离 × 1.15` 拍一个常数，分桶用均匀随机数抖动。
//! 服务端拿 `totalSteps / totalTime` 一除就能反推出步频，再和 `speed` 对不上。
//!
//! **第二代**（`buckets::make_jitter`）：分桶有随机抖动了，但仍是白噪声——
//! 相邻桶互相独立，自相关接近 0。真人的步频有惯性，统计上能分开。
//!
//! 本模块的做法：
//!
//! ```text
//! 轨迹 → 弧长/曲率 → OU 速度剖面（含弯道预判刹车）
//!      → 每点速度 + 疲劳度 → 步态 (cadence, stride)
//!      → 步频 × dt/60 → 该段步数 → 累加成总量与分桶
//! ```
//!
//! 三个量由同一个速度序列派生，**互相自洽**，不存在对不上的可能。

use super::biomech::{self, fatigue_from_km, ou_cadence_series};
use super::buckets::{speed_buckets, SpeedBucket, StepBucket};
use super::geo::{distance, path_length};
use super::kinematics::{pace_profile, KinParams};

/// 合成参数。
#[derive(Debug, Clone)]
pub struct SimParams {
    pub total_distance_m: f64,
    pub total_sec: i64,
    pub start_ms: i64,
    /// 目标步数。`None` 表示完全由步态模型决定。
    pub target_steps: Option<i32>,
    /// 速度剖面下限（m/s）。
    pub v_floor: f64,
    /// 速度剖面上限（m/s）。
    pub v_ceil: f64,
    pub kin: KinParams,
    pub seed: u64,
}

impl Default for SimParams {
    fn default() -> Self {
        Self {
            total_distance_m: 2000.0,
            total_sec: 720,
            start_ms: 0,
            target_steps: None,
            v_floor: 0.8,
            v_ceil: 6.0,
            kin: KinParams::default(),
            seed: 0,
        }
    }
}

/// 合成结果。
#[derive(Debug, Clone)]
pub struct SimResult {
    /// 逐点速度（m/s）。
    pub speeds: Vec<f64>,
    /// 逐点步频（步/分）。
    pub cadences: Vec<f64>,
    /// 逐点步幅（米）。
    pub strides: Vec<f64>,
    /// 由模型算出的总步数。
    pub total_steps: i32,
    /// 平均步频（步/分）。
    pub avg_cadence: f64,
    pub speed_buckets: Vec<SpeedBucket>,
    pub step_buckets: Vec<StepBucket>,
    /// 实际总里程（应精确等于 `total_distance_m`）。
    pub actual_length_m: f64,
}

/// 从轨迹点合成完整数据流。
pub fn simulate(points: &[(f64, f64, i64)], p: &SimParams) -> Result<SimResult, String> {
    if points.len() < 2 {
        return Err("轨迹点不足，无法合成".into());
    }
    if p.total_sec <= 0 {
        return Err("时长必须为正".into());
    }

    let xy: Vec<(f64, f64)> = points.iter().map(|(a, b, _)| (*a, *b)).collect();
    let dts: Vec<f64> = points
        .windows(2)
        .map(|w| ((w[1].2 - w[0].2) as f64 / 1000.0).max(0.001))
        .collect();

    let total_len = path_length(&xy);
    if total_len <= 0.0 {
        return Err("轨迹长度为 0".into());
    }

    // ① 速度剖面：OU + 弯道预判 + 精确命中里程
    // 以**轨迹实际长度**为准。用目标里程去凑会得到「速度×时间 = 2000m，
    // 但折线只有 888m」这种自相矛盾的结果，服务端一算就发现走的路和报的里程不符。
    let target_len = total_len;
    let v0 = target_len / p.total_sec as f64;
    let speeds = pace_profile(
        &xy,
        v0,
        &dts,
        target_len,
        p.seed,
        &p.kin,
        p.v_floor,
        p.v_ceil,
    );

    // ② 步频：由 OU 回归到「该速度的目标步频」
    //
    // 这里用两段 OU：外层的速度已经有了惯性，内层的步频再对速度做一次
    // 回归。真实跑者的步频不会严格等于公式值，会滞后于速度变化。
    let mut cad_targets = Vec::with_capacity(speeds.len());
    let mut elapsed_km = 0.0f64;
    for (i, &v) in speeds.iter().enumerate() {
        if i > 0 {
            elapsed_km += v * dts[i - 1] / 1000.0;
        }
        let fat = fatigue_from_km(elapsed_km);
        // 目标步频：真机 6 条记录 / 560 分桶的对数回归（见 biomech::target_cadence），
        // 带小幅疲劳衰减。旧的 34.4+44.5v 在慢配速处偏低 40~60 步，已废弃。
        cad_targets.push((biomech::target_cadence(v) * (1.0 - 0.03 * fat)).clamp(120.0, 210.0));
    }
    let mut cadences = ou_cadence_series(
        &cad_targets,
        &dts,
        p.seed.wrapping_add(1),
        // θ、σ 来自真机：把步频减去「该速度的目标步频」得到残差，残差 sd≈20 步/分
        // （≈3.4 步/10s）、lag-1 自相关≈0.21。按 AR(1) 反解得 θ≈0.078、σ≈6.3（dt=10s），
        // 模拟验证 sd=20.5、ac=0.19，与真机吻合。
        //
        // 旧值 σ=3.2 把 10 秒步数抖动压到真机的一半，序列过于平滑——而原始步频序列
        // 的总自相关（含速度剖面漂移）仍≈0.6，检测器看到的仍是真人量级，不受影响。
        0.078,
        6.3,
        120.0,
        232.0,
    );

    // ③ 步幅：由等式反解，保证 v = cadence/60 * stride 成立
    let mut strides = Vec::with_capacity(speeds.len());
    for i in 0..speeds.len() {
        let st = if cadences[i] > 0.0 {
            speeds[i] * 60.0 / cadences[i]
        } else {
            1.0
        };
        strides.push(st.clamp(0.5, 1.7));
    }

    // ④ 逐段步数，累加得总步数
    let mut steps_f = Vec::with_capacity(dts.len());
    for (i, &dt) in dts.iter().enumerate() {
        let cad = cadences.get(i).copied().unwrap_or(150.0);
        steps_f.push(cad * dt / 60.0);
    }
    let model_steps: f64 = steps_f.iter().sum();

    // ⑤ 与目标步数对齐（缩放步频权重，不改速度）
    let total_steps = match p.target_steps {
        Some(t) if t > 0 => {
            let k = t as f64 / model_steps.max(1e-9);
            for s in steps_f.iter_mut() {
                *s *= k;
            }
            for c in cadences.iter_mut() {
                *c *= k;
            }
            let _ = &steps_f;
            // 步频变了，步幅必须跟着重算——否则步频×步幅推不出真实速度，
            // 服务端一除就发现三组数据互相矛盾。
            for i in 0..strides.len() {
                strides[i] = if cadences[i] > 0.0 {
                    (speeds[i] * 60.0 / cadences[i]).clamp(0.5, 1.7)
                } else {
                    strides[i]
                };
            }
            t
        }
        _ => model_steps.round().max(1.0) as i32,
    };

    // ⑥ 分桶
    let sb = speed_buckets(points, p.start_ms, p.total_sec);
    let times: Vec<i64> = points.iter().map(|p| p.2).collect();
    let step_buckets = distribute_steps(&sb, &times, &cadences, total_steps);

    let avg_cadence = if p.total_sec > 0 {
        total_steps as f64 * 60.0 / p.total_sec as f64
    } else {
        0.0
    };

    Ok(SimResult {
        speeds,
        cadences,
        strides,
        total_steps,
        avg_cadence,
        speed_buckets: sb,
        step_buckets,
        actual_length_m: total_len,
    })
}

/// 把步频按时间积分到 10 秒桶。
///
/// **不能用「每段步数累加」**。段长由轨迹采样决定（本项目 5m 一段 ≈1.8s），
/// 与 10s 桶宽不成整数倍，每桶段数在 5 和 6 之间交替，步数于是出现
/// 27,33,28,35,29,35 这种高低振荡——一阶自相关 -0.674，比白噪声还假。
///
/// 正确做法：把步频看作时间上的连续函数（分段线性插值），对每个桶的
/// `[begin, end)` 区间做梯形积分，得到该桶的真实步数。
/// 这样每桶都是完整的 10 秒，不存在段数偏差。
fn distribute_steps(
    sb: &[SpeedBucket],
    times: &[i64],
    cadences: &[f64],
    total_steps: i32,
) -> Vec<StepBucket> {
    let n = sb.len().max(1);
    let mut acc = vec![0f64; n];

    if times.len() >= 2 && cadences.len() >= 2 {
        for (i, b) in sb.iter().enumerate() {
            acc[i] = integrate_cadence(times, cadences, b.begin_time, b.end_time);
        }
    } else if n > 0 {
        acc[0] = total_steps as f64;
    }

    // 归一化到目标总步数
    let sum: f64 = acc.iter().sum();
    if sum > 0.0 && total_steps > 0 {
        let k = total_steps as f64 / sum;
        for a in acc.iter_mut() {
            *a *= k;
        }
    }

    // 最大余数法取整，保证总和精确
    let mut counts: Vec<i32> = acc.iter().map(|x| x.floor().max(0.0) as i32).collect();
    let mut need = total_steps - counts.iter().sum::<i32>();
    if need > 0 {
        let mut rem: Vec<(f64, usize)> = acc
            .iter()
            .enumerate()
            .map(|(i, &x)| (x - x.floor(), i))
            .collect();
        rem.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        for (_, i) in rem {
            if need <= 0 {
                break;
            }
            counts[i] += 1;
            need -= 1;
        }
    } else if need < 0 {
        while need < 0 {
            let idx = (0..n).max_by_key(|&i| counts[i]).unwrap_or(0);
            if counts[idx] == 0 {
                break;
            }
            counts[idx] -= 1;
            need += 1;
        }
    }

    sb.iter()
        .enumerate()
        .map(|(i, b)| {
            let steps = counts[i].max(0);
            // diff 三件套：真机每个桶恒为 avgDiff=0 / maxDiff=0 / minDiff=1000
            // （6 条记录 581 个桶无一例外）。早先算桶内三段差值想造「波动」，
            // 反而成了与真机相反的指纹。回到真机常量。
            StepBucket {
                steps_num: steps,
                begin_time: b.begin_time,
                end_time: b.end_time,
                flag: b.flag,
                id: 0,
                max_diff: 0,
                min_diff: 1000,
                avg_diff: 0,
                state: 0,
                queue_num: 0,
            }
        })
        .collect()
}

/// 对 `[begin, end)` 区间积分步频（步/分），返回步数。
///
/// 线性插值 + 梯形法，端点精确落在区间边界上。
fn integrate_cadence(times: &[i64], cadences: &[f64], begin: i64, end: i64) -> f64 {
    if end <= begin || times.len() < 2 {
        return 0.0;
    }
    let cad_at = |t: f64| -> f64 {
        // 找 t 所在的段，线性插值
        if t <= times[0] as f64 {
            return cadences.first().copied().unwrap_or(150.0);
        }
        if t >= *times.last().unwrap() as f64 {
            return cadences.last().copied().unwrap_or(150.0);
        }
        let mut lo = 0usize;
        let mut hi = times.len() - 1;
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if times[mid] as f64 <= t {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        // cadences 比 times 少一个（它描述的是「段」，times 描述的是「点」），
        // 末端索引要安全取，否则桶覆盖到最后一段时会越界。
        let c_at = |i: usize| -> f64 {
            cadences
                .get(i)
                .copied()
                .or_else(|| cadences.last().copied())
                .unwrap_or(150.0)
        };
        let t0 = times[lo] as f64;
        let t1 = times[hi] as f64;
        if t1 <= t0 {
            return c_at(lo);
        }
        let w = (t - t0) / (t1 - t0);
        c_at(lo) * (1.0 - w) + c_at(hi) * w
    };

    // 梯形法：在桶内按 1 秒细分
    let step = 1000.0; // 毫秒
    let mut area = 0.0;
    let mut t = begin as f64;
    let endf = end as f64;
    while t < endf {
        let t2 = (t + step).min(endf);
        let dt = (t2 - t) / 1000.0;
        area += (cad_at(t) + cad_at(t2)) / 2.0 * dt;
        t = t2;
    }
    area / 60.0
}

/// 由轨迹几何 + 目标时长反推带时间戳的轨迹。
///
/// **这一步是必须的**。早先的实现拿到的是「等距点位 + 等距时间戳」，
/// 于是逐段速度恒定（因为 d/dt 不变），但报文里的 `speedPerTenSec`
/// 却有起伏——两者放在一起就是自相矛盾的数据。
///
/// 正确关系：`dt_i = d_i / v_i`。速度快的段用时短，累计出来的时间戳
/// 才会与速度序列一致。
pub fn retime(
    points: &[(f64, f64)],
    total_sec: i64,
    start_ms: i64,
    seed: u64,
    kin: &KinParams,
    v_floor: f64,
    v_ceil: f64,
) -> (Vec<i64>, Vec<f64>, i64) {
    let n = points.len();
    if n < 2 {
        return (Vec::new(), Vec::new(), 0);
    }
    // 段距离
    let mut d_seg = Vec::with_capacity(n - 1);
    for i in 1..n {
        d_seg.push(distance(points[i - 1], points[i]));
    }
    let total_len: f64 = d_seg.iter().sum();
    if total_len <= 0.0 {
        return (Vec::new(), Vec::new(), 0);
    }

    // 先用平均速度生成剖面（时间参数化用均匀 dt 近似）
    let dt_avg = total_sec as f64 / (n - 1) as f64;
    let dts0 = vec![dt_avg; n - 1];
    let v0 = total_len / total_sec.max(1) as f64;
    let speeds = pace_profile(points, v0, &dts0, total_len, seed, kin, v_floor, v_ceil);

    // 由速度反推每段用时，并用最大余数法分配，保证总时长精确。
    //
    // 早先的写法是先算浮点用时再四舍五入，然后试图"末点对齐"——但缩放会
    // 制造相等时间戳，递增修正把末点推过目标，自检直接判 TRACK_STOP。
    // 用权重 + 最大余数法，Σdt 从构造上就等于目标，不存在累积误差。
    let want_total = total_sec * 1000;
    let mut w = Vec::with_capacity(n - 1);
    for (i, &seg) in d_seg.iter().take(n - 1).enumerate() {
        let v = speeds.get(i).copied().unwrap_or(v0).max(1e-3);
        w.push(seg / v * 1000.0); // 毫秒
    }
    let wsum: f64 = w.iter().sum();
    let dts_ms: Vec<i64> = if wsum > 0.0 {
        // 每段至少 1 秒，不是 1ms。
        //
        // 报文的 totalTime 字段单位是**秒**（`(time_ms - start_ms) / 1000`），
        // 毫秒精度在报文里必然丢失。原来每段只保证 ≥1ms，慢速段落在同一秒
        // 里 → 反解后时间戳重复 → 自检 TRACK_TIME_ORDER 致命、服务端也会
        // 看到 0 时长段。这里直接按秒对齐。
        let min_seg = 1000i64;
        let usable = (want_total - (n as i64 - 1) * min_seg).max(0) as f64;
        let raw: Vec<f64> = w.iter().map(|x| usable * x / wsum).collect();
        let mut base: Vec<i64> = raw.iter().map(|x| x.floor().max(0.0) as i64).collect();
        let mut need = usable as i64 - base.iter().sum::<i64>();
        if need > 0 {
            let mut rem: Vec<(f64, usize)> = raw
                .iter()
                .enumerate()
                .map(|(i, &x)| (x - x.floor(), i))
                .collect();
            rem.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            for (_, idx) in rem {
                if need <= 0 {
                    break;
                }
                base[idx] += 1;
                need -= 1;
            }
        }
        base.iter().map(|x| x + min_seg).collect()
    } else {
        // 无权重时均分，同样保证每段 ≥1 秒
        let each = (want_total / (n as i64 - 1)).max(1000);
        vec![each; n - 1]
    };

    let mut times = Vec::with_capacity(n);
    let mut t = start_ms;
    times.push(t);
    for dt in &dts_ms {
        t += *dt;
        times.push(t);
    }

    (times, speeds, total_len as i64)
}

/// 校验合成结果的自洽性。
///
/// 服务端会做同样的检查，所以本地先过一遍，不合格就不提交。
pub fn validate(r: &SimResult, p: &SimParams) -> Vec<String> {
    let mut errs = Vec::new();

    // 允许 5% 偏差：轨迹几何由 planner 决定，这里只检查两者没有严重脱节
    let tol = (p.total_distance_m * 0.05).max(20.0);
    if (r.actual_length_m - p.total_distance_m).abs() > tol {
        errs.push(format!(
            "里程偏离：实测 {:.1}m vs 目标 {:.0}m",
            r.actual_length_m, p.total_distance_m
        ));
    }

    if r.speed_buckets.len() != (p.total_sec as usize / 10).max(1) {
        errs.push(format!(
            "速度桶数量不符：{} 个，应为 {}",
            r.speed_buckets.len(),
            (p.total_sec as usize / 10).max(1)
        ));
    }

    let step_sum: i32 = r.step_buckets.iter().map(|b| b.steps_num).sum();
    if step_sum != r.total_steps {
        errs.push(format!("步数分桶和 {step_sum} ≠ 总数 {}", r.total_steps));
    }

    // 步频与速度必须自洽：avg_cadence/60 * avg_stride ≈ avg_speed
    if !r.speeds.is_empty() && !r.strides.is_empty() {
        let avg_v = r.speeds.iter().sum::<f64>() / r.speeds.len() as f64;
        let avg_c = r.cadences.iter().sum::<f64>() / r.cadences.len() as f64;
        let avg_s = r.strides.iter().sum::<f64>() / r.strides.len() as f64;
        let recon = avg_c / 60.0 * avg_s;
        if (recon - avg_v).abs() > 0.05 {
            errs.push(format!(
                "步态不自洽：步频×步幅 推出 {recon:.3} m/s，实际 {avg_v:.3} m/s"
            ));
        }
    }

    errs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn straight_track(n: usize, sec: i64) -> Vec<(f64, f64, i64)> {
        let step_t = sec * 1000 / (n as i64 - 1);
        (0..n)
            .map(|i| {
                let t = if i == n - 1 { sec * 1000 } else { i as i64 * step_t };
                (30.5 + i as f64 * 4.5e-5, 104.0, 1_730_000_000_000 + t)
            })
            .collect()
    }

    fn params() -> SimParams {
        SimParams {
            total_distance_m: 2000.0,
            total_sec: 720,
            start_ms: 1_730_000_000_000,
            target_steps: None,
            ..Default::default()
        }
    }

    #[test]
    fn produces_self_consistent_stream() {
        let pts = straight_track(400, 720);
        let r = simulate(&pts, &params()).unwrap();
        assert_eq!(r.speeds.len(), pts.len() - 1);
        let errs = validate(&r, &params());
        assert!(errs.is_empty(), "自洽性检查失败: {errs:?}");
    }

    #[test]
    fn total_steps_is_physically_derived() {
        let pts = straight_track(400, 720);
        let r = simulate(&pts, &params()).unwrap();
        // 720s / 60 = 12 分钟，平均步频应落在 140~210
        assert!(
            (140.0..=215.0).contains(&r.avg_cadence),
            "平均步频 {:.1} 不合理",
            r.avg_cadence
        );
        // 12 分钟 × 170 步/分 ≈ 2040 步
        assert!(
            (1700..=2600).contains(&r.total_steps),
            "总步数 {} 不合理",
            r.total_steps
        );
    }

    #[test]
    fn target_steps_is_respected() {
        let pts = straight_track(400, 720);
        let mut p = params();
        p.target_steps = Some(2300);
        let r = simulate(&pts, &p).unwrap();
        assert_eq!(r.total_steps, 2300);
        let sum: i32 = r.step_buckets.iter().map(|b| b.steps_num).sum();
        assert_eq!(sum, 2300, "分桶和必须等于总数");
    }

    #[test]
    fn buckets_are_not_white_noise() {
        let pts = straight_track(400, 720);
        let r = simulate(&pts, &params()).unwrap();
        let counts: Vec<f64> = r.step_buckets.iter().map(|b| b.steps_num as f64).collect();

        // 用 lag-1 自相关判断时间相关性——这是唯一能区分「有惯性」和「白噪声」的量。
        // 相邻差值小并不代表有相关性：白噪声的相邻差也小（因为都围着均值转）。
        let r1 = crate::track::kinematics::lag1_autocorr(&counts);
        let bound = crate::track::kinematics::noise_upper_bound(counts.len());
        assert!(
            r1 > bound,
            "分桶步数 lag-1 自相关 {r1:.3} 未超过白噪声上界 {bound:.3}"
        );
    }

    /// 步数分桶之间不该完全独立，但也不该锁死（那会导致总步数对不上）。
    #[test]
    fn bucket_steps_sum_to_total() {
        let pts = straight_track(400, 720);
        let r = simulate(&pts, &params()).unwrap();
        let sum: i64 = r.step_buckets.iter().map(|b| b.steps_num as i64).sum();
        assert_eq!(sum, r.total_steps as i64, "分桶步数之和必须等于总步数");
    }

    #[test]
    fn rejects_degenerate_input() {
        assert!(simulate(&[], &params()).is_err());
        let mut p = params();
        p.total_sec = 0;
        assert!(simulate(&straight_track(10, 100), &p).is_err());
    }
}

#[cfg(test)]
mod time_precision_tests {
    use super::*;

    /// 相邻采样点必须至少相隔 1 秒。
    ///
    /// 报文 totalTime 单位是秒，不足 1 秒的间隔反解后会把两个点压成同一
    /// 时刻——自检报 TRACK_TIME_ORDER，服务端看到 0 时长段。
    #[test]
    fn segments_are_at_least_one_second() {
        let n = 273;
        let pts: Vec<(f64, f64)> = (0..n)
            .map(|i| (31.2304 + i as f64 * 7.0 / 111_320.0, 121.4737))
            .collect();
        let (times, _, _) = retime(&pts, 720, 1_790_000_000_000, 42, &KinParams::default(), 1.39, 8.33);
        assert_eq!(times.len(), n);
        for w in times.windows(2) {
            let dt = w[1] - w[0];
            assert!(dt >= 1000, "间隔 {dt}ms < 1 秒，报文里会重复");
        }
        // 末点不能被推过总时长
        let span = (times[n - 1] - times[0]) / 1000;
        assert!(
            (span - 720).abs() <= 2,
            "总时长 {span}s 偏离目标 720s"
        );
    }

    /// 慢速段也要满足 1 秒下限（这条最容易破）。
    #[test]
    fn slow_segments_still_have_one_second() {
        let n = 400;
        let pts: Vec<(f64, f64)> = (0..n)
            .map(|i| (31.2304 + i as f64 * 5.0 / 111_320.0, 121.4737))
            .collect();
        // 2 公里跑 40 分钟 = 0.83 m/s，接近下限
        let (times, _, _) = retime(&pts, 2400, 1_790_000_000_000, 7, &KinParams::default(), 0.5, 2.0);
        for w in times.windows(2) {
            assert!(w[1] - w[0] >= 1000, "慢速段间隔不足 1 秒");
        }
    }
}
