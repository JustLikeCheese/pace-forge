//! 轨迹引擎：规划、采样、里程对齐、10 秒分桶。
//!
//! 全部为纯计算，不碰网络，可用单测完整验证。

pub mod buckets;
pub mod detect;
pub mod fence_route;
pub mod roadnet;
pub mod biomech;
pub mod kinematics;
pub mod preflight;
pub mod simulate;
pub mod stadium;
pub mod geo;
pub mod planner;

pub use biomech::{cadence_for, fatigue_from_km, gait, ou_cadence_series, speed_from, stride_for, Gait};
pub use buckets::{make_jitter, speed_buckets, step_buckets, SpeedBucket, StepBucket};
pub use kinematics::{arc_and_curvature, pace_profile, speed_limit_ahead, turn_speed_limit, KinParams};
pub use preflight::{inspect, Check, Issue, Level, Report};
pub use simulate::{retime, simulate, validate, SimParams, SimResult};
pub use stadium::{
    campus_oval_track, lap_length, lane_radius, looks_like_stadium, random_oval, stadium_track,
    OvalSpec, StadiumSpec,
};
pub use geo::{distance, path_length, sample_along, scale_to_length};
pub use planner::{assign_times, plan, Checkpoint, PlanRequest, PlanResult, TrackPoint};

/// 取 0.155 而不是 0.15：真机首段 86 秒 ÷ 555 秒 = 15.50%，差 3 秒就差一整个 20 秒窗口。
pub const STARTUP_RATIO: f64 = 0.155;

/// 起步窗口的秒数（segmentJson 首段时长 / state 分界）。
pub fn startup_sec(total_sec: i64) -> i64 {
    if total_sec <= 20 {
        return 0;
    }
    ((total_sec as f64 * STARTUP_RATIO).round() as i64).clamp(5, total_sec / 2)
}
