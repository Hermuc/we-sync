WE-Sync 手机端 agent（可选组件）
================================

这个目录为空时，WE-Sync 一切照旧：走 MTP 或 adb push，不会尝试多通道。

要让「USB + WiFi 双链路并行」生效，需要放一个与本机 ABI 匹配的接收端二进制在这里：

    wsync-agent-aarch64      arm64-v8a（绝大多数手机）
    wsync-agent-armv7        armeabi-v7a（老机型）
    wsync-agent-x86_64       x86_64（模拟器）

（Android 的 ABI 名是 arm64-v8a，Rust 侧叫 aarch64，程序里做了映射，两种命名都能识别。）

查看本机 ABI：adb shell getprop ro.product.cpu.abi

构建（不需要 Android NDK）
--------------------------
    bash scripts/build-agent-android.sh            # aarch64，产物自动落到本目录
    bash scripts/build-agent-android.sh armv7      # 老机型

原理：编成 aarch64-unknown-linux-musl 的**静态** ELF，用 rustc 自带的 rust-lld 链接，
Android 内核可直接执行。实测在 arm64-v8a / API 36 的真机上运行正常，体积约 440 KB。
唯一的额外依赖是 target 的 std：rustup target add aarch64-unknown-linux-musl
（国内直连 static.rust-lang.org 会超时，脚本已内置 rsproxy 镜像。）

启用方式（config.toml，或在设置页「传输通道」下拉里选「多通道（USB+WiFi）」）
--------------------------------------------------------------------------
    [transport]
    prefer = "multipath"

    [transport.multipath]
    port = 5740
    chunk_kb = 1024
    agent_path = ""              # 留空 = 按上面的规则在本目录查找
    allow_single_link = false    # 仅调试用：只有一条链路也启用（不会更快）

运行期行为（绿色约定）
----------------------
  * 二进制只是 adb push 到 /data/local/tmp/wsync-agent，用完 kill + rm；
  * 不安装 APK、不需要 root/Shizuku、不改任何系统设置；
  * 目标目录先写 <文件名>.wsync，整文件 SHA-256 校验通过才 rename，
    中途断线不会留下半个壁纸；
  * agent 只写启动参数 --dir 指定的目录，不接受客户端指定的路径；
  * 只有「adb 可用」+「agent 可执行」+「两条链路」同时成立才启用；
    任何一步不满足都会静默回落到 adb push / MTP，不会让同步变慢。
