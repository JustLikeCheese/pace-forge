//! 业务接口层。

pub mod accounts;
pub mod start_time;
pub mod client;
pub mod daily;
pub mod coord;
pub mod fence;
pub mod obs;
pub mod points;
pub mod policy;
pub mod flow;
pub mod gmttime;
pub mod gt4;
pub mod gt4image;
pub mod login;
pub mod records;
pub mod semester;
pub mod session;
pub mod submit;

pub use client::{Client, Credentials, DeviceProfile};
pub use login::{LoginOutcome, UserProfile};
