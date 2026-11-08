//! WE-Sync —— Wallpaper Engine 视频壁纸自动同步到手机（绿色便携 / 模块化）。

pub mod config;
pub mod discover;
pub mod engine;
pub mod model;
pub mod state;
pub mod transport;
pub mod ui;
pub mod util;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
