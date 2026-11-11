//! 生物力学：配速、步频、步幅的非线性耦合。
//!
//! 硬约束：`v = (cadence / 60) · stride`（v: m/s，cadence: 步/分，stride: 米）
//!
//! **这个约束必须始终成立**。早期实现里步频和步数是各自随机生成的，两者与
//! 速度对不上——服务端只要拿 `totalSteps / totalTime` 和 `totalDis / totalTime`
//! 一除就能看出矛盾：步频 192 步/分 × 步幅必然推出的速度，与报文里的配速不符。
//!
//! 步频公式由真实记录回归得到，不是拍脑袋常数。数据来自账号自身的 **6 条真机
//! 记录、560 个 10 秒分桶**（从 OBS `step_freq_json`/`speed_json` 还原）。
//! 三种模型里对数形 `cadence = 147.0 + 46.98·ln(v)` 拟合最好（R²≈0.48），
//! 线性 `131.5 + 21.9·v`（R²≈0.45）接近。真实步频均值 **178 步/分**、范围
//! 156~196，几乎只随速度缓慢变化——这和「成熟跑者步频近乎恒定、主要靠步幅
//! 调速」一致。
//!
//! 早先用的 `34.4 + 44.5·v`（来自 4 条来路不明的样本）在这份真机数据上
//! 系统性偏低：慢配速（8~9 分/km）处预测 114~118，实测 160~176，差 40~60 步。
//! 截距太低、斜率太陡，两头都错，已废弃。

/// 标准正态抽样（Box-Muller）。
///
/// 不引 `rand_distr`：它要额外拉依赖，而这里只需要一个标准正态。
fn normal_sample(rng: &mut impl rand::Rng) -> f64 {
    let u1 = rng.gen::<f64>().max(1e-12);
    let u2 = rng.gen::<f64>();
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

/// 步态：步频与步幅。
#[derive(Clone, Copy, Debug)]
pub struct Gait {
    /// 步频（步/分）。
    pub cadence: f64,
    /// 步幅（米）。
    pub stride: f64,
}

/// 某速度下的目标步频（步/分），来自真机 6 条记录 / 560 个分桶的对数回归。
///
/// `cadence = 147.0 + 46.98·ln(v)`，v 下限夹到 0.4 m/s 防止 ln 发散。
///
/// | 速度 m/s | 配速 | 模型步频 | 真机实测 |
/// |---|---|---|---|
/// | 1.78 | 9:22/km | 174 | 176 |
/// | 2.05 | 8:07/km | 181 | 167 |
/// | 2.34 | 7:08/km | 187 | 192 |
/// | 2.83 | 5:53/km | 196 | 193 |
pub fn target_cadence(v: f64) -> f64 {
    147.0 + 46.98 * v.max(0.4).ln()
}

/// 由速度与疲劳度计算步态，严格满足 `v = cadence/60 * stride`。
///
/// 步频由 [`target_cadence`] 给出（真机对数回归），疲劳只做**小幅**下调：
/// 真机数据里步频几乎只跟速度走，慢漂移主要来自配速本身变化，不是步频衰减。
pub fn gait(v: f64, fatigue: f64) -> Gait {
    let target_cad = target_cadence(v) * (1.0 - 0.03 * fatigue);
    let mut cadence = target_cad.clamp(120.0, 210.0);
    let mut stride = if cadence > 0.0 { v * 60.0 / cadence } else { 0.0 };

    // 步幅越界时反解步频，保持等式成立。下限 0.45m：真机慢配速步幅低到 0.46m。
    if stride > 1.7 {
        stride = 1.7;
        cadence = v * 60.0 / stride;
    } else if stride < 0.45 {
        stride = 0.45;
        cadence = v * 60.0 / stride;
    }
    Gait { cadence, stride }
}

/// 由速度与疲劳度取步幅（米）。
pub fn stride_for(v: f64, fatigue: f64) -> f64 {
    gait(v, fatigue).stride
}

/// 由速度与步幅反解步频（步/分）。
pub fn cadence_for(v: f64, stride: f64) -> f64 {
    (60.0 * v / stride.max(1e-6)).clamp(120.0, 235.0)
}

/// 由步频与步幅计算速度（m/s）。
pub fn speed_from(cadence: f64, stride: f64) -> f64 {
    (cadence / 60.0) * stride
}

/// 疲劳度（0..0.6）：已跑 0.5km 后开始线性抬升，封顶 0.6。
pub fn fatigue_from_km(km: f64) -> f64 {
    if km <= 0.5 {
        0.0
    } else {
        ((km - 0.5) * 1.5).min(0.6)
    }
}

/// 步频的 OU（Ornstein-Uhlenbeck）均值回归序列。
///
/// ```text
/// c_{t+1} = c_t + θ(target_t − c_t)·Δt + σ·√Δt·Z,   Z ~ N(0,1)
/// ```
///
/// 为什么不用均匀随机抖动：真实步频有**惯性**——既不会逐点乱跳，也不会
/// 一直贴在均值上，而是带着自相关地向目标回归。OU 过程正好描述这个，
/// 均匀随机数做不到（相邻点独立，序列看起来像白噪声）。
pub fn ou_cadence_series(
    targets: &[f64],
    dts: &[f64],
    seed: u64,
    theta: f64,
    sigma: f64,
    floor: f64,
    ceil: f64,
) -> Vec<f64> {
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    let mut rng = StdRng::seed_from_u64(seed);
    let mut out = Vec::with_capacity(targets.len());
    let mut c = targets.first().copied().unwrap_or(150.0);

    for (i, &target) in targets.iter().enumerate() {
        let dt = dts.get(i).copied().unwrap_or(1.0);
        c += theta * (target - c) * dt + sigma * dt.sqrt() * normal_sample(&mut rng);
        c = c.clamp(floor, ceil);
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 步频必须贴合真机记录。夹具是 6 条真机记录里 11 个整公里 lap 的
    /// (速度 m/s, 实测步频)——不含 GPS/uid，仅标量。模型（R²≈0.48）在单点上
    /// 允许散布，但整段平均绝对误差应 < 15 步，且均值落在真实量级。
    #[test]
    fn cadence_matches_real_records() {
        // (v, 实测步频)，来自 step_freq_json/laps_json 还原，已剔除一条损坏 lap
        let real: &[(f64, f64)] = &[
            (1.667, 161.0), (1.779, 175.8), (1.786, 175.7), (1.876, 159.6),
            (2.053, 166.8), (2.123, 156.1), (2.326, 180.0), (2.336, 191.6),
            (2.439, 193.8), (2.532, 180.8), (2.833, 193.3),
        ];
        let mut sum_err = 0.0;
        for &(v, want) in real {
            let c = target_cadence(v);
            sum_err += (c - want).abs();
            assert!(
                (140.0..=205.0).contains(&c),
                "v={v} 模型步频 {c:.0} 超出真实量级 140~205"
            );
        }
        let mae = sum_err / real.len() as f64;
        assert!(mae < 15.0, "模型对真机的平均绝对误差 {mae:.1} 步过大");
    }

    /// 步频随速度单调上升，但很平（真机：步幅主导调速）。
    #[test]
    fn cadence_increases_slowly_with_speed() {
        assert!(target_cadence(2.5) > target_cadence(1.5));
        // 1.5→2.8 m/s（配速 11:07→5:57）步频涨幅应温和（< 35 步），不是陡坡
        assert!(target_cadence(2.8) - target_cadence(1.5) < 35.0);
    }

    /// 等式约束必须始终成立，否则服务端一除就露馅。
    #[test]
    fn identity_always_holds() {
        for v in [1.5, 2.0, 2.5, 3.0, 3.5, 4.0] {
            for fatigue in [0.0, 0.5, 1.0] {
                let g = gait(v, fatigue);
                let back = g.cadence / 60.0 * g.stride;
                assert!(
                    (back - v).abs() < 1e-9,
                    "v={v} fatigue={fatigue} 重建速度 {back} 不符"
                );
            }
        }
    }

    #[test]
    fn gait_stays_in_envelope() {
        for v in [1.9f64, 2.5, 3.5, 5.0, 6.3] {
            for fat in [0.0f64, 0.3, 0.6] {
                let g = gait(v, fat);
                assert!((0.5..=1.7).contains(&g.stride), "stride={}", g.stride);
                assert!((110.0..=230.0).contains(&g.cadence), "cad={}", g.cadence);
                assert!((speed_from(g.cadence, g.stride) - v).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn ou_series_is_bounded_and_deterministic() {
        let n = 200;
        let targets: Vec<f64> = (0..n).map(|i| 170.0 + 0.1 * i as f64).collect();
        let dts = vec![5.0; n];
        let a = ou_cadence_series(&targets, &dts, 42, 0.25, 0.5, 110.0, 220.0);
        assert_eq!(a.len(), n);
        for c in &a {
            assert!((110.0..=220.0).contains(c), "cadence={c}");
        }
        let b = ou_cadence_series(&targets, &dts, 42, 0.25, 0.5, 110.0, 220.0);
        assert_eq!(a, b, "同种子必须同结果");
    }

    /// OU 序列必须有自相关：相邻点差值的方差应远小于白噪声。
    #[test]
    fn ou_is_not_white_noise() {
        let n = 300;
        let targets = vec![160.0; n];
        let dts = vec![10.0; n];
        let s = ou_cadence_series(&targets, &dts, 7, 0.12, 0.8, 100.0, 220.0);
        let diffs: Vec<f64> = s.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        let mean_diff = diffs.iter().sum::<f64>() / diffs.len() as f64;
        let spread = s.iter().cloned().fold(f64::MIN, f64::max)
            - s.iter().cloned().fold(f64::MAX, f64::min);
        assert!(
            mean_diff < spread * 0.5,
            "相邻差值 {mean_diff:.3} 相对整体波动 {spread:.3} 过大，不像均值回归"
        );
    }

    #[test]
    fn fatigue_is_monotonic_and_capped() {
        assert_eq!(fatigue_from_km(0.4), 0.0);
        assert!(fatigue_from_km(1.0) > 0.0);
        assert_eq!(fatigue_from_km(5.0), 0.6);
        // 上限 0.6 在 0.9km 处就到（(0.9-0.5)*1.5 = 0.6），
        // 所以要在到达上限之前比单调性
        assert!(fatigue_from_km(0.8) > fatigue_from_km(0.6));
        assert_eq!(fatigue_from_km(0.9), 0.6);
    }
}
