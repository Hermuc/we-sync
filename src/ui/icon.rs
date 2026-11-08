//! 应用图标：把预处理的 32bpp BGRA 像素写入 DIBSection，再由 `CreateIconIndirect` 生成 HICON。
//!
//! 像素数据（`assets/icon16.bin` / `icon32.bin`）由 `include_bytes!` 编译进 exe，
//! 运行时不依赖任何外部文件，也不引入图像解码库。
//! 小尺寸单独预缩放（LANCZOS），托盘图标比让系统从 32px 直接缩小更清晰。
//!
//! 另有 `blank()`：造一张全透明图标给标题栏占位（见该函数注释）。

use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

const ICON16: &[u8] = include_bytes!("../../assets/icon16.bin");
const ICON32: &[u8] = include_bytes!("../../assets/icon32.bin");

/// `small = true` 生成 16×16（托盘图标），否则 32×32（窗口 / 任务栏大图标）。
pub fn make_icon(small: bool) -> Option<HICON> {
    let (size, data): (i32, &[u8]) = if small { (16, ICON16) } else { (32, ICON32) };
    let bytes = (size * size * 4) as usize;
    if data.len() != bytes {
        return None;
    }
    assemble(size, data, false)
}

/// 全透明图标：给标题栏用，让左上角不画任何图形。
///
/// 两个坑：
/// 1. `WM_SETICON(ICON_SMALL, 0)` 不行 —— 系统会回落到 exe 内嵌图标资源，图标照样出现；
/// 2. 只把颜色位图 alpha 置 0 也不行 —— `DrawIconEx` 走掩码路径（未启用 alpha 混合时）
///    会把 alpha=0 的像素画成**纯黑方块**，所以单色掩码必须全 1（= 全透明）。
pub fn blank(size: i32) -> Option<HICON> {
    if size <= 0 {
        return None;
    }
    let pixels = vec![0u8; (size * size * 4) as usize];
    assemble(size, &pixels, true)
}

/// 由 BGRA 像素组装 HICON。
///
/// `opaque_mask = false` 时单色掩码全 0（= 全部不透明，与历史实现一致）：
/// 32bpp 图标的透明度由 alpha 通道决定，掩码只在老路径里兜底。
/// `opaque_mask = true` 时掩码全 1（= 全部不绘制），用于全透明占位图标。
fn assemble(size: i32, data: &[u8], transparent: bool) -> Option<HICON> {
    unsafe {
        let color = dib_32(size, data)?;
        let mask = mask_dib(size, transparent)?;

        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(mask);
        let _ = DeleteObject(color);
        icon
    }
}

/// 32bpp 顶向下 DIBSection，直接拷入 BGRA。
unsafe fn dib_32(size: i32, data: &[u8]) -> Option<HBITMAP> {
    let mut bi = BITMAPV5HEADER {
        bV5Size: std::mem::size_of::<BITMAPV5HEADER>() as u32,
        bV5Width: size,
        bV5Height: -size, // 顶向下
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_RGB,
        ..Default::default()
    };
    bi.bV5RedMask = 0x00FF_0000;
    bi.bV5GreenMask = 0x0000_FF00;
    bi.bV5BlueMask = 0x0000_00FF;
    bi.bV5AlphaMask = 0xFF00_0000;

    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let hbm = CreateDIBSection(
        None,
        &bi as *const _ as *const BITMAPINFO,
        DIB_RGB_COLORS,
        &mut bits,
        None,
        0,
    )
    .ok()?;
    if bits.is_null() {
        let _ = DeleteObject(hbm);
        return None;
    }
    // 数据本身就是小端 BGRA（u32 = A<<24|R<<16|G<<8|B），直接整块拷贝
    std::ptr::copy_nonoverlapping(data.as_ptr(), bits as *mut u8, data.len());
    Some(hbm)
}

/// 1bpp 单色掩码；`transparent = true` 时全置 1（全透明），否则留 0（不透明）。
unsafe fn mask_dib(size: i32, transparent: bool) -> Option<HBITMAP> {
    let bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 1,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let hbm = CreateDIBSection(
        None,
        &bi,
        DIB_RGB_COLORS,
        &mut bits,
        None,
        0,
    )
    .ok()?;
    if bits.is_null() {
        let _ = DeleteObject(hbm);
        return None;
    }
    // 1bpp 每行按 4 字节对齐；CreateDIBSection 已零初始化，只需在全透明时置 1
    if transparent {
        let stride = ((size + 31) / 32 * 4) as usize;
        std::ptr::write_bytes(bits as *mut u8, 0xFF, stride * size as usize);
    }
    Some(hbm)
}
