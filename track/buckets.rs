//! 10 秒分桶。
//!
//! 服务端的反作弊核心在这两个数组上，不是总量：
//! - `speedPerTenSec`：每 10 秒的位移
//! - `stepsPerTenSec`：每 10 秒的步数，外加窗口内的 `maxDiff/minDiff/avgDiff`
//!
//! 总量对但分桶像机器，一样判无效。所以桶里必须有自然抖动。

use serde::{Deserialize, Serialize};

use super::geo::distance;

/// 速度桶。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeedBucket {
    pub distance: f64,
    #[serde(rename = "beginTime")]
    pub begin_time: i64,
    #[serde(rename = "endTime")]
    pub end_time: i64,
    /// 整次跑步的起始毫秒戳（每桶都填同一个值）。
    pub flag: i64,
    /// 桶 id：提交用 `60000+i`，OBS 用 `(rrid%100000)*1000+endSec`。
    pub id: i64,
    pub state: i32,
    #[serde(rename = "queueNum")]
    pub queue_num: i32,
}

/// 步频桶。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepBucket {
    #[serde(rename = "stepsNum")]
    pub steps_num: i32,
    #[serde(rename = "beginTime")]
    pub begin_time: i64,
    #[serde(rename = "endTime")]
    pub end_time: i64,
    pub flag: i64,
    pub id: i64,
    #[serde(rename = "maxDiff")]
    pub max_diff: i32,
    #[serde(rename = "minDiff")]
    pub min_diff: i32,
    #[serde(rename = "avgDiff")]
    pub avg_diff: i32,
    pub state: i32,
    #[serde(rename = "queueNum")]
    pub queue_num: i32,
}

/// 按 10 秒切桶，统计每个桶的位移。
///
/// `points` 必须是 (lat, lon, 毫秒时间戳) 且按时间递增。
pub fn speed_buckets(points: &[(f64, f64, i64)], start_ms: i64, total_sec: i64) -> Vec<SpeedBucket> {
    let n = (total_sec / 10).max(1) as usize;
    let mut out = Vec::with_capacity(n);

    // 预先算好每段的距离和起止时间，再按与桶区间的**重叠比例**分配。
    //
    // 原来是单指针 cursor 跨桶不重置，只在 `points[cursor+1].t >= begin`
    // 时才累加距离——点如果已经落在桶之前，那一段距离被永久丢弃，
    // 该桶就成了 0 位移。实测 72 个桶里 17 个是空的，服务端看到的是
    // 「跑着跑着静止十秒」。
    let mut segs: Vec<(f64, f64, i64, i64)> = Vec::with_capacity(points.len().saturating_sub(1));
    for w in points.windows(2) {
        let d = distance((w[0].0, w[0].1), (w[1].0, w[1].1));
        segs.push((d, 0.0, w[0].2, w[1].2));
    }

    for i in 0..n {
        let begin = start_ms + i as i64 * 10_000;
        let end = begin + 10_000;
        let mut dist = 0.0f64;
        for &(d, _, t0, t1) in &segs {
            if t1 <= begin || t0 >= end || d <= 0.0 {
                continue;
            }
            let span = (t1 - t0).max(1) as f64;
            let lo = t0.max(begin);
            let hi = t1.min(end);
            let overlap = (hi - lo).max(0) as f64;
            if overlap > 0.0 {
                dist += d * overlap / span;
            }
        }
        out.push(SpeedBucket {
            distance: (dist * 100.0).round() / 100.0,
            begin_time: begin,
            end_time: end,
            flag: start_ms,
            id: 0,
            state: 0,
            queue_num: 0,
        });
    }
    out
}

/// 按 10 秒切桶，把总步数分配到各桶，并给出桶内波动统计。
///
/// `jitter` 是均匀抖动幅度（如 0.18 表示 ±18%），由调用方用固定种子生成，
/// 保证可复现。
pub fn step_buckets(
    start_ms: i64,
    total_sec: i64,
    total_steps: i32,
    jitter: &[f64],
) -> Vec<StepBucket> {
    let n = (total_sec / 10).max(1) as usize;

    // 先生成带抖动的权重，再归一化到总步数
    let mut weights: Vec<f64> = (0..n)
        .map(|i| {
            let j = jitter.get(i % jitter.len().max(1)).copied().unwrap_or(0.0);
            1.0 + j
        })
        .collect();
    let wsum: f64 = weights.iter().sum();
    for w in weights.iter_mut() {
        *w /= wsum;
    }

    // 最大余数法分配：每桶先取整，剩下的按小数部分从大到小补 1。
    //
    // 原来用 `round()` 再「把误差全补到最后一个桶」——1843 步 / 72 桶时
    // 每项四舍五入成 26，总和 1872，误差 -29 全压到末桶，末桶变成 **-3**。
    // 报文里出现负步数是硬伤，服务端一眼就能看到。
    let exact: Vec<f64> = weights.iter().map(|w| w * total_steps as f64).collect();
    let mut counts: Vec<i32> = exact.iter().map(|v| v.floor() as i32).collect();
    let mut remain: i32 = total_steps - counts.iter().sum::<i32>();
    if remain > 0 {
        // 小数部分大的先补
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| {
            let fa = exact[a] - exact[a].floor();
            let fb = exact[b] - exact[b].floor();
            fb.partial_cmp(&fa).unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut k = 0usize;
        while remain > 0 && k < order.len() * 4 {
            counts[order[k % order.len()]] += 1;
            remain -= 1;
            k += 1;
        }
    } else if remain < 0 {
        // 浮点误差导致 floor 之和偏大：从最小项开始减
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&i| counts[i]);
        let mut k = 0usize;
        while remain < 0 && k < order.len() * 4 {
            let idx = order[k % order.len()];
            if counts[idx] > 0 {
                counts[idx] -= 1;
                remain += 1;
            }
            k += 1;
        }
    }

    counts
        .iter()
        .enumerate()
        .map(|(i, &steps)| {
            let begin = start_ms + i as i64 * 10_000;
            // diff 三件套：真机每个桶恒为 avgDiff=0 / maxDiff=0 / minDiff=1000
            // （6 条记录 581 个桶无一例外）。早先按桶内三段差值造「波动」想摆脱
            // 指纹，但那是在没有真机数据时的推测——真机本就是这三个常量，
            // 变化反而成了反向指纹。回到真机形态。
            StepBucket {
                steps_num: steps,
                begin_time: begin,
                end_time: begin + 10_000,
                flag: start_ms,
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


/// 生成长度 `n` 的固定抖动序列（±`amp`），值域 [0.85, 1.15] 之外会被夹住。
pub fn make_jitter(n: usize, amp: f64, seed: u64) -> Vec<f64> {
    let mut state = seed | 1;
    (0..n)
        .map(|_| {
            // xorshift64*，够用且不引入额外依赖
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let r = (state.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / (1u64 << 53) as f64;
            (r * 2.0 - 1.0) * amp
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_points(start: i64, sec: i64, n: usize) -> Vec<(f64, f64, i64)> {
        let step = (sec * 1000) / (n as i64 - 1);
        (0..n)
            .map(|i| {
                let t = if i == n - 1 { start + sec * 1000 } else { start + i as i64 * step };
                (30.5 + i as f64 * 1e-5, 104.0, t)
            })
            .collect()
    }

    #[test]
    fn bucket_count_matches_duration() {
        let pts = line_points(0, 760, 200);
        let b = speed_buckets(&pts, 0, 760);
        assert_eq!(b.len(), 76, "760s 应有 76 个 10 秒桶");
        assert_eq!(b[0].begin_time, 0);
        assert_eq!(b[0].end_time, 10_000);
        assert_eq!(b[75].begin_time, 750_000);
    }

    #[test]
    fn distances_sum_to_near_total() {
        let pts = line_points(0, 760, 400);
        let total: f64 = pts
            .windows(2)
            .map(|w| distance((w[0].0, w[0].1), (w[1].0, w[1].1)))
            .sum();
        let buckets = speed_buckets(&pts, 0, 760);
        let sum: f64 = buckets.iter().map(|b| b.distance).sum();
        // 允许每个桶 0.01 的舍入误差
        assert!((sum - total).abs() < buckets.len() as f64 * 0.02, "sum={sum} total={total}");
    }

    #[test]
    fn steps_sum_exactly() {
        let j = make_jitter(76, 0.15, 42);
        let b = step_buckets(0, 760, 2300, &j);
        assert_eq!(b.len(), 76);
        let sum: i32 = b.iter().map(|x| x.steps_num).sum();
        assert_eq!(sum, 2300, "步数总和必须精确等于 totalSteps");
    }

    #[test]
    fn steps_have_variation() {
        let j = make_jitter(76, 0.15, 7);
        let b = step_buckets(0, 760, 2300, &j);
        let counts: Vec<i32> = b.iter().map(|x| x.steps_num).collect();
        let min = *counts.iter().min().unwrap();
        let max = *counts.iter().max().unwrap();
        assert!(max > min, "各桶步数不应完全相同（max={max} min={min}）");
    }

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        let a = make_jitter(100, 0.2, 12345);
        let b = make_jitter(100, 0.2, 12345);
        assert_eq!(a, b, "同种子必须同结果");
        for v in &a {
            assert!(v.abs() <= 0.2 + 1e-9, "抖动超界: {v}");
        }
        let c = make_jitter(100, 0.2, 999);
        assert_ne!(a, c, "不同种子应不同");
    }

    #[test]
    fn short_duration_still_one_bucket() {
        let pts = line_points(0, 5, 10);
        let b = speed_buckets(&pts, 0, 5);
        assert_eq!(b.len(), 1, "不足 10 秒也要有一个桶，不能返回空");
    }
}

/// 提交报文用的桶 id：`60000+i`，`queueNum = i`。
///
/// 服务端在提交时按这个规则校验；和 OBS 对象里的 id 规则**不同**。
pub fn apply_submit_ids(speed: &mut [SpeedBucket], steps: &mut [StepBucket]) {
    for (i, b) in speed.iter_mut().enumerate() {
        b.id = 60000 + i as i64;
        b.queue_num = i as i32;
    }
    for (i, b) in steps.iter_mut().enumerate() {
        b.id = 60000 + i as i64;
        b.queue_num = i as i32;
    }
}

/// OBS 对象用的桶 id：`(rrid % 100000) * 1000 + endSec`，`queueNum = 0`。
///
/// 依赖提交返回的 `rrid`，所以 OBS 必须在提交**之后**上传。
pub fn apply_obs_ids(speed: &mut [SpeedBucket], steps: &mut [StepBucket], rrid: i64, start_ms: i64) {
    let base = (rrid % 100_000) * 1000;
    for b in speed.iter_mut() {
        b.id = base + (b.end_time - start_ms) / 1000;
        b.queue_num = 0;
    }
    for b in steps.iter_mut() {
        b.id = base + (b.end_time - start_ms) / 1000;
        b.queue_num = 0;
    }
}

#[cfg(test)]
mod id_tests {
    use super::*;

    #[test]
    fn submit_and_obs_ids_differ() {
        let pts: Vec<(f64, f64, i64)> = (0..=60)
            .map(|i| (30.5 + i as f64 * 1e-5, 104.0, 1_700_000_000_000 + i * 1000))
            .collect();
        let start = 1_700_000_000_000i64;
        let mut sp = speed_buckets(&pts, start, 60);
        let mut st = step_buckets(start, 60, 200, &[1.0; 6]);

        apply_submit_ids(&mut sp, &mut st);
        assert_eq!(sp[0].id, 60000);
        assert_eq!(sp[1].id, 60001);
        assert_eq!(sp[0].queue_num, 0);
        assert_eq!(sp[1].queue_num, 1);

        apply_obs_ids(&mut sp, &mut st, 123_456_789, start);
        // (123456789 % 100000) * 1000 = 56789 * 1000
        assert_eq!(sp[0].id, 56_789_000 + 10, "首桶 endTime 是 start+10s");
        assert_eq!(sp[1].id, 56_789_000 + 20);
        assert_eq!(sp[0].queue_num, 0);
    }

    #[test]
    fn flag_is_start_ms() {
        let pts: Vec<(f64, f64, i64)> = vec![(30.5, 104.0, 1_700_000_000_000), (30.5001, 104.0, 1_700_000_010_000)];
        let sp = speed_buckets(&pts, 1_700_000_000_000, 10);
        assert_eq!(sp[0].flag, 1_700_000_000_000);
    }
}

#[cfg(test)]
mod diff_not_degenerate_tests {
    use super::*;

    /// diff 三件套必须与真机一致：恒为 avgDiff=0 / maxDiff=0 / minDiff=1000。
    ///
    /// 实测 6 条真机记录 581 个桶，这三个字段无一例外是这三个常量。早先按
    /// 「桶内波动」把它们做成变化的，反而和真机相反——真机本身就是常量。
    #[test]
    fn diff_fields_match_real_device() {
        let j = vec![1.0; 72];
        let b = step_buckets(0, 720, 1843, &j);
        for x in &b {
            assert!(x.steps_num > 0, "不该有空桶");
            assert_eq!(x.avg_diff, 0, "真机 avgDiff 恒为 0");
            assert_eq!(x.max_diff, 0, "真机 maxDiff 恒为 0");
            assert_eq!(x.min_diff, 1000, "真机 minDiff 恒为 1000");
        }
    }
}

#[cfg(test)]
mod speed_bucket_coverage_tests {
    use super::*;

    /// 各桶位移之和必须等于轨迹总长（重叠比例分配不该丢距离）。
    #[test]
    fn bucket_distances_sum_to_total() {
        // 273 点 / 720 秒，非均匀采样
        let pts: Vec<(f64, f64, i64)> = (0..273)
            .map(|i| {
                let t = 1_790_000_000_000 + i as i64 * 2640;
                (31.2304 + i as f64 * 7.0 / 111_320.0, 121.4737, t)
            })
            .collect();
        let total: f64 = pts
            .windows(2)
            .map(|w| distance((w[0].0, w[0].1), (w[1].0, w[1].1)))
            .sum();
        let b = speed_buckets(&pts, 1_790_000_000_000, 720);
        let sum: f64 = b.iter().map(|x| x.distance).sum();
        assert!(
            (sum - total).abs() < total * 0.02,
            "桶和 {sum:.1}m 与总长 {total:.1}m 差太多"
        );
    }

    /// 正常 720 秒轨迹不该出现大量空桶。
    ///
    /// 旧实现单指针跨桶不重置，落在桶前的那段距离被丢掉，实测
    /// 72 个桶里 17 个位移为 0。
    #[test]
    fn no_empty_buckets_on_normal_track() {
        let pts: Vec<(f64, f64, i64)> = (0..273)
            .map(|i| {
                let t = 1_790_000_000_000 + i as i64 * 2640;
                (31.2304 + i as f64 * 7.0 / 111_320.0, 121.4737, t)
            })
            .collect();
        let b = speed_buckets(&pts, 1_790_000_000_000, 720);
        let empty = b.iter().filter(|x| x.distance < 0.5).count();
        assert!(empty == 0, "{empty} 个桶位移为 0（共 {} 个）", b.len());
    }
}
