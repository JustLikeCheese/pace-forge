//! 会话与设备身份的持久化。
//!
//! 两层必须分开存：
//!
//! - **设备身份**（`identity.json`）：`device_id` 全生命周期复用。每次随机生成
//!   会让服务端认成「另一台设备」，直接撞上 `10121 此账号已在其他设备登录`。
//! - **登录会话**（`session.json`）：登录成功即落盘，登出时删除。
//!
//! 会话**必须落盘**，不能只放内存。只在内存的后果是进程一重启 token 就没了，
//! 而服务端那边的会话还活着 —— 此时想登出也拿不出凭证，账号会被锁一小时。
//!
//! 设计要点：
//! `load_identity` / `save_session` / `clear_session` 三段式。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 登录会话。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub unid: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub campus_name: String,
    #[serde(default)]
    pub depart: String,
    #[serde(default)]
    pub grade_class: String,
    /// 登录时使用的设备标识，登出时用于确认是同一台机
    #[serde(default)]
    pub device_id: String,
}

impl Session {
    pub fn is_valid(&self) -> bool {
        !self.token.is_empty() && !self.uid.is_empty()
    }
}

/// 数据目录：`$HOME/.pace-forge/`。
pub fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = PathBuf::from(home).join(".pace-forge");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 账号的校区中心（BD-09），从打卡点质心学来，存在 `home.json`。
///
/// 没配围栏的学校原来一律拿 (30.5, 104.0)（成都）兜底当定位锚点：拉打卡点
/// 时把成都坐标当成跑者位置发给服务端，网页地图也跳到成都。这里记住真实
/// 校区，之后所有兜底都用它。键是 uid；`"*"` 存最近一次，给还没学到的账号用。
pub fn load_home(uid: &str) -> Option<(f64, f64)> {
    let map: std::collections::HashMap<String, [f64; 2]> = read_json("home.json")?;
    map.get(uid).or_else(|| map.get("*")).map(|v| (v[0], v[1]))
}

/// 记住账号的校区中心（BD-09）。
pub fn save_home(uid: &str, lat: f64, lng: f64) {
    if !(lat.is_finite() && lng.is_finite()) || (lat == 0.0 && lng == 0.0) {
        return;
    }
    let mut map: std::collections::HashMap<String, [f64; 2]> =
        read_json("home.json").unwrap_or_default();
    map.insert(uid.to_string(), [lat, lng]);
    map.insert("*".to_string(), [lat, lng]);
    let _ = write_json("home.json", &map);
}

fn read_json<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    let text = std::fs::read_to_string(data_dir().join(name)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_json<T: Serialize>(name: &str, value: &T) -> Result<(), String> {
    let path = data_dir().join(name);
    let json = serde_json::to_string_pretty(value).map_err(|e| format!("序列化 {name} 失败: {e}"))?;
    // 会话文件含 token，权限收紧到 0600
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(std::path::Path::new(".")));
    let mut f = std::fs::File::create(&path)
        .map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    use std::io::Write;
    f.write_all(json.as_bytes())
        .map_err(|e| format!("写入 {} 失败: {e}", path.display()))
}

/// 读取会话。没有或损坏时返回空会话。
pub fn load_session() -> Session {
    read_json("session.json").unwrap_or_default()
}

/// 写入会话（登录成功后立即调用）。
pub fn save_session(s: &Session) -> Result<(), String> {
    write_json("session.json", s)
}

/// 删除会话文件。
pub fn clear_session() {
    let _ = std::fs::remove_file(data_dir().join("session.json"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_session() -> Session {
        Session {
            uid: "123".into(),
            token: "abc".into(),
            unid: "88".into(),
            username: "13800000000".into(),
            name: "测试".into(),
            campus_name: "某大学".into(),
            depart: "计算机".into(),
            grade_class: "2024级".into(),
            device_id: "deadbeef".into(),
        }
    }

    #[test]
    fn session_roundtrip() {
        let mut s = temp_session();
        // 用独立文件名避免污染真实会话
        s.name = "往返测试".into();
        write_json("session_test.json", &s).unwrap();
        let back: Session = read_json("session_test.json").unwrap();
        assert_eq!(back.uid, "123");
        assert_eq!(back.token, "abc");
        assert_eq!(back.name, "往返测试");
        let _ = std::fs::remove_file(data_dir().join("session_test.json"));
    }

    #[test]
    fn missing_session_is_empty_not_error() {
        clear_session();
        let s = load_session();
        assert!(s.token.is_empty());
        assert!(!s.is_valid());
    }

    #[test]
    fn validity_requires_token_and_uid() {
        let mut s = temp_session();
        assert!(s.is_valid());
        s.token.clear();
        assert!(!s.is_valid(), "没有 token 就不算有效会话");
        s.token = "x".into();
        s.uid.clear();
        assert!(!s.is_valid(), "没有 uid 也不算了");
    }

    #[test]
    fn data_dir_is_created() {
        let d = data_dir();
        assert!(d.exists(), "数据目录应自动创建");
        assert!(d.to_string_lossy().contains(".pace-forge"));
    }
}
