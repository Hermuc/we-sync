//! 把预编译的 Windows 图标资源链接进 exe（资源管理器 / 任务栏显示用）。
//!
//! `assets/app.res` 由 `assets/app.rc` + Windows SDK 的 rc.exe 预先生成并随源码提供，
//! 因此构建机不需要安装 Windows SDK；图标数据会随 exe 一起发布，运行时不依赖外部文件。

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR 未设置");
    let res = format!("{dir}\\assets\\app.res");
    println!("cargo:rerun-if-changed=assets/app.res");
    println!("cargo:rustc-link-arg-bin=we-sync={res}");
}
