//! 子进程的平台差异收口点。
//!
//! 全项目只有这里和 `ui/` 里的绘制代码会出现平台条件编译；
//! 其余模块保持纯 std，任何平台都能编译、都能跑单测。

use std::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 隐藏子进程控制台窗口（Windows 专有；其它平台是空操作）。
///
/// 返回 `&mut Command` 便于链式调用：
/// `proc::hide_window(&mut cmd).args([...]);`
pub fn hide_window(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hide_window_is_chainable_and_does_not_run_anything() {
        let mut cmd = Command::new("cmd");
        cmd.arg("/c").arg("echo");
        let returned = hide_window(&mut cmd);
        assert_eq!(returned.get_program().to_string_lossy(), "cmd");
    }
}
