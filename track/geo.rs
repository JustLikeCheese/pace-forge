//! 平面几何与地理计算。
//!
//! 校园尺度（几公里内）用等距圆柱投影足够精确，误差在厘米级，
//! 比 haversine 快一个数量级，且便于做向量缩放。

/// 一度纬度对应的米数。
pub const M_PER_DEG_LAT: f64 = 111_320.0;

/// 一度经度对应的米数（随纬度收缩）。
/// 每度纬度对应多少米（约 111_320，随纬度变化很小）。
pub fn m_per_deg_lat(_lat: f64) -> f64 {
    111_320.0
}

pub fn m_per_deg_lng(lat: f64) -> f64 {
    M_PER_DEG_LAT * lat.to_radians().cos().abs().max(0.1)
}

/// 两点间距离（米）。
pub fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lon1) = a;
    let (lat2, lon2) = b;
    let dy = (lat2 - lat1) * M_PER_DEG_LAT;
    let dx = (lon2 - lon1) * m_per_deg_lng((lat1 + lat2) / 2.0);
    (dx * dx + dy * dy).sqrt()
}

/// 折线总长（米）。
pub fn path_length(points: &[(f64, f64)]) -> f64 {
    points.windows(2).map(|w| distance(w[0], w[1])).sum()
}

/// 沿折线等距取点，返回 `n` 个点（含起点，不含终点）。
///
/// 用于把「少量拐点」展开成「密集轨迹点」。
pub fn sample_along(path: &[(f64, f64)], n: usize) -> Vec<(f64, f64)> {
    if path.len() < 2 || n == 0 {
        return Vec::new();
    }
    let total = path_length(path);
    if total <= 0.0 {
        return vec![path[0]; n];
    }
    let step = total / n as f64;
    let mut out = Vec::with_capacity(n);
    let mut seg = 0usize;
    let mut seg_start = 0.0f64;
    let mut seg_len = distance(path[0], path[1]);

    for i in 0..n {
        let target = step * i as f64;
        while seg + 1 < path.len() - 1 && seg_start + seg_len < target {
            seg_start += seg_len;
            seg += 1;
            seg_len = distance(path[seg], path[seg + 1]);
        }
        let t = if seg_len > 0.0 {
            ((target - seg_start) / seg_len).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (a, b) = (path[seg], path[seg + 1]);
        out.push((a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t));
    }
    out
}

/// 把折线整体绕 `base` 缩放，使总长等于 `target`。
///
/// 以首点为基准是严格的相似变换：每段向量同比例缩放，总长精确等于
/// `factor` 倍原长，一轮即收敛。以质心为基准会让锚点复位破坏相似性，
/// 迭代可能反向增长。
///
/// `fixed` 里的下标坐标会被复位（打卡点不能动）。
///
/// 目标里程的容差（米）。
///
/// 服务端只判「最低里程 + 点位按顺序经过」，2 km 跑成 1.7 km 或 2.3 km 都照收。
/// 为了把 2000.0 m 凑成 2000.0 m 而去缩放/绕圈，反而会把轨迹拧成不自然的形状，
/// 甚至绕到楼里去。所以留 15% 的口子，最多 300 m。
pub fn length_tolerance(target_m: f64) -> f64 {
    (target_m * 0.15).clamp(60.0, 300.0)
}

/// 返回 `(最终长度, 是否确实改动了坐标)`。
pub fn scale_to_length(
    points: &mut [(f64, f64)],
    target: f64,
    fixed: &[usize],
    tolerance: f64,
) -> (f64, bool) {
    if points.len() < 3 || target <= 0.0 {
        return (path_length(points), false);
    }
    let original = path_length(points);
    if original <= 0.0 {
        return (0.0, false);
    }

    let base = points[0];
    let snapshot: Vec<(usize, (f64, f64))> =
        fixed.iter().map(|&i| (i, points[i])).collect();

    for _ in 0..20 {
        let current = path_length(points);
        if (current - target).abs() <= tolerance || current <= 0.0 {
            break;
        }
        let factor = target / current;
        for p in points.iter_mut() {
            p.0 = base.0 + (p.0 - base.0) * factor;
            p.1 = base.1 + (p.1 - base.1) * factor;
        }
        for &(i, (lat, lon)) in &snapshot {
            points[i] = (lat, lon);
        }
    }

    let final_len = path_length(points);
    (final_len, (final_len - original).abs() > tolerance)
}

/// 把一点平移到目标位置，并让邻域按线性衰减跟着移动，避免轨迹出现尖刺。
///
/// 直接单点位移会在折线上留下一个突兀的折角，服务端轨迹图上很明显。
pub fn nudge_to(
    points: &mut [(f64, f64)],
    index: usize,
    target: (f64, f64),
    spread: usize,
) {
    if index >= points.len() {
        return;
    }
    let (lat0, lon0) = points[index];
    let dlat = target.0 - lat0;
    let dlon = target.1 - lon0;

    // 中心点精确命中
    points[index] = target;

    // 两侧按 1/(k+1) 衰减平移
    for k in 1..=spread {
        if k > 0 && index >= k {
            let w = 1.0 / (k as f64 + 1.0);
            let i = index - k;
            points[i].0 += dlat * w;
            points[i].1 += dlon * w;
        }
        if index + k < points.len() {
            let w = 1.0 / (k as f64 + 1.0);
            let i = index + k;
            points[i].0 += dlat * w;
            points[i].1 += dlon * w;
        }
    }
}

/// 在折线上找离 `target` 最近的下标。
pub fn nearest_index(points: &[(f64, f64)], target: (f64, f64)) -> usize {
    let mut best = 0usize;
    let mut best_d = f64::INFINITY;
    for (i, p) in points.iter().enumerate() {
        let d = distance(*p, target);
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_is_metric() {
        let a = (30.5, 104.0);
        let b = (30.501, 104.0);
        let d = distance(a, b);
        // 0.001 度纬度 ≈ 111.32m
        assert!((d - 111.32).abs() < 1.0, "d={d}");
        assert_eq!(distance(a, a), 0.0);
        assert!((distance(a, b) - distance(b, a)).abs() < 1e-9);
    }

    #[test]
    fn sample_spreads_evenly() {
        let path = vec![(30.5, 104.0), (30.51, 104.0)];
        let pts = sample_along(&path, 100);
        assert_eq!(pts.len(), 100);
        // 首点贴起点，末点接近但不等于终点
        assert!(distance(pts[0], path[0]) < 0.01);
        assert!(distance(pts[99], path[1]) > 0.5);
        // 相邻间距应均匀
        let gaps: Vec<f64> = pts.windows(2).map(|w| distance(w[0], w[1])).collect();
        let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
        for g in &gaps {
            assert!((g - mean).abs() / mean < 0.05, "gap {g} 偏离均值 {mean}");
        }
    }

    #[test]
    fn scale_hits_target_exactly() {
        let mut pts = vec![
            (30.5, 104.0),
            (30.505, 104.002),
            (30.508, 104.006),
            (30.5, 104.0),
        ];
        let target = 2000.0;
        let (len, changed) = scale_to_length(&mut pts, target, &[0], 0.1);
        assert!(changed);
        assert!((len - target).abs() < 0.5, "len={len} target={target}");
        // 首点必须没动
        assert_eq!(pts[0], (30.5, 104.0));
    }

    #[test]
    fn scale_respects_fixed_points() {
        let mut pts = vec![
            (30.5, 104.0),
            (30.505, 104.002),
            (30.508, 104.006),
            (30.51, 104.009),
        ];
        let anchor = (30.508, 104.006);
        scale_to_length(&mut pts, 1500.0, &[0, 2], 0.1);
        assert_eq!(pts[2], anchor, "打卡点坐标不能被缩放改掉");
    }

    #[test]
    fn nudge_moves_neighborhood_smoothly() {
        let mut pts: Vec<(f64, f64)> = (0..20).map(|i| (30.5, 104.0 + i as f64 * 1e-4)).collect();
        let before = pts[10];
        let target = (30.50005, 104.0015);
        nudge_to(&mut pts, 10, target, 4);
        assert_eq!(pts[10], target, "中心点必须精确命中");
        // 邻点位移应小于中心位移
        let center_move = distance(before, target);
        let neighbor_move = distance((30.5, 104.0 + 9.0 * 1e-4), pts[9]);
        assert!(neighbor_move < center_move, "邻点位移应更小");
        // 远处的点不受影响
        assert_eq!(pts[0], (30.5, 104.0));
    }

    #[test]
    fn nearest_finds_closest() {
        let pts = vec![(30.5, 104.0), (30.6, 104.1), (30.7, 104.2)];
        assert_eq!(nearest_index(&pts, (30.601, 104.101)), 1);
    }
}
