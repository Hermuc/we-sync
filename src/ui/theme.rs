//! 轻量视觉主题：暖白底 + 白卡片 + 单一宝蓝强调色（与 Wallpaper Engine 图标同色），
//! 大圆角、极轻描边、粗标题与细正文的强字重对比。字体用雅黑（笔画饱满、粗黑清晰）。
//!
//! 绘制分两层：
//! * 纯色矩形 → GDI `FillRect`（无锯齿问题，最快）；
//! * 圆角矩形 / 椭圆 / 折线 → GDI+ 抗锯齿绘制。
//!
//! 早期版本全用 GDI 的 `RoundRect` / `Ellipse` / `Polyline`，那三个 API 没有抗锯齿，
//! 圆角和斜线在 100% 缩放下就是台阶状锯齿（按钮胶囊、开关轨道、卡片圆角最明显）。
//! 这里统一换成 GDI+（`SmoothingModeAntiAlias8x8` + `PixelOffsetModeHalf`）：
//! 曲线边缘做亚像素混合，1px 描边仍落在整像素上保持锋利。
//! 所有 GDI 对象在 `Theme` 生命周期内复用，退出时统一释放。

use std::sync::Once;

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, FALSE, POINT, RECT};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::GdiPlus as gp;

pub const BG: COLORREF = COLORREF(0x00F2F7F7); // #F7F7F2 窗口底
pub const SURFACE: COLORREF = COLORREF(0x00FFFFFF); // #FFFFFF 卡片
pub const FIELD: COLORREF = COLORREF(0x00EFF4F4); // #F4F4EF 输入框浅底
pub const BORDER: COLORREF = COLORREF(0x00E2EAEA); // #EAEAE2 细描边
pub const TEXT: COLORREF = COLORREF(0x001A1C1C); // #1C1C1A 主文字
pub const TEXT_DIM: COLORREF = COLORREF(0x00848B8B); // #8B8B84 次要文字
pub const ACCENT: COLORREF = COLORREF(0x00DE6000); // #0060DE 中蓝（比原宝蓝亮一档）
pub const ACCENT_HOVER: COLORREF = COLORREF(0x00E8761A); // #1A76E8 悬停提亮
pub const ACCENT_PRESS: COLORREF = COLORREF(0x00B15300); // #0053B1 按下压暗
pub const ACCENT_SOFT: COLORREF = COLORREF(0x00FCEFE5); // #E5EFFC 选中项浅蓝底
pub const BTN_BG: COLORREF = COLORREF(0x00FFFFFF); // 次按钮白底
pub const BTN_BG_HOVER: COLORREF = COLORREF(0x00EFF4F4); // #F4F4EF
pub const BTN_BG_PRESS: COLORREF = COLORREF(0x00FCE9DC); // #DCE9FC 次按钮按下浅蓝
pub const BTN_BG_DISABLED: COLORREF = COLORREF(0x00EAF0F0); // #F0F0EA 禁用底（比窗口底略深一档）
pub const BORDER_DISABLED: COLORREF = COLORREF(0x00EAE2E2); // #E2E2EA 禁用描边（更淡）
pub const TEXT_DISABLED: COLORREF = COLORREF(0x00A0A6A6); // #A6A6A0 禁用文字
pub const TRACK_OFF: COLORREF = COLORREF(0x00D6DDDD); // #DDDDD6 开关关闭态轨道
pub const CAPTION: COLORREF = COLORREF(0x00F2F7F7); // #F7F7F2 标题栏
pub const WHITE: COLORREF = COLORREF(0x00FFFFFF);
/// 强调底上的前景字：主按钮用白字。
pub const TEXT_ON_ACCENT: COLORREF = COLORREF(0x00FFFFFF);
/// 强调底的禁用态前景字（低对比但不刺眼）。
pub const TEXT_ON_ACCENT_DIM: COLORREF = COLORREF(0x00F7D7BF); // #BFD7F7

pub struct Theme {
    pub dpi: u32,
    pub br_bg: HBRUSH,
    pub br_surface: HBRUSH,
    pub br_field: HBRUSH,
    pub br_accent_soft: HBRUSH,
    pub font: HFONT,
    pub font_bold: HFONT,
    pub font_small: HFONT,
    pub font_title: HFONT,
}

impl Theme {
    pub fn new(dpi: u32) -> Self {
        let h = |pt: i32, weight: i32| unsafe {
            CreateFontW(
                -((pt * dpi as i32) / 72),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32,
                DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
                w!("Microsoft YaHei UI"),
            )
        };
        ensure_gdiplus();
        unsafe {
            Self {
                dpi,
                br_bg: CreateSolidBrush(BG),
                br_surface: CreateSolidBrush(SURFACE),
                br_field: CreateSolidBrush(FIELD),
                br_accent_soft: CreateSolidBrush(ACCENT_SOFT),
                font: h(9, FW_NORMAL.0 as i32),
                font_bold: h(9, FW_BOLD.0 as i32),
                font_small: h(8, FW_NORMAL.0 as i32),
                font_title: h(18, FW_BOLD.0 as i32),
            }
        }
    }

    /// 逻辑像素 -> 物理像素
    pub fn px(&self, v: f32) -> i32 {
        (v * self.dpi as f32 / 96.0).round() as i32
    }
}

impl Drop for Theme {
    fn drop(&mut self) {
        unsafe {
            for b in [
                self.br_bg,
                self.br_surface,
                self.br_field,
                self.br_accent_soft,
            ] {
                let _ = DeleteObject(HGDIOBJ(b.0));
            }
            for f in [self.font, self.font_bold, self.font_small, self.font_title] {
                let _ = DeleteObject(HGDIOBJ(f.0));
            }
        }
    }
}

// ---------------------------------------------------------------- GDI+ 底座

fn ensure_gdiplus() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let input = gp::GdiplusStartupInput {
            GdiplusVersion: 1,
            DebugEventCallback: 0,
            SuppressBackgroundThread: FALSE,
            SuppressExternalCodecs: FALSE,
        };
        let mut token: usize = 0;
        // 故意不调用 GdiplusShutdown：进程退出时随 DLL 卸载即可，
        // 提前 shutdown 反而会让仍在排队的绘制消息拿到空句柄。
        let _ = gp::GdiplusStartup(&mut token, &input, std::ptr::null_mut());
    });
}

fn ok(s: gp::Status) -> bool {
    s.0 == 0
}

/// COLORREF(0x00BBGGRR) -> GDI+ ARGB(0xAARRGGBB)
fn argb(c: COLORREF) -> u32 {
    0xFF00_0000 | argb_a(c, 0xFF)
}

/// 同上，但可指定 alpha（0 = 全透明，255 = 不透明），用于光晕与高光叠加。
fn argb_a(c: COLORREF, alpha: u8) -> u32 {
    let v = c.0;
    let r = v & 0xFF;
    let g = (v >> 8) & 0xFF;
    let b = (v >> 16) & 0xFF;
    ((alpha as u32) << 24) | (r << 16) | (g << 8) | b
}

/// 绑定到 HDC 的 GDI+ 画布：开启抗锯齿 + 半像素偏移，Drop 时释放。
struct Gfx(*mut gp::GpGraphics);

impl Gfx {
    fn new(hdc: HDC) -> Option<Self> {
        ensure_gdiplus();
        unsafe {
            let mut g: *mut gp::GpGraphics = std::ptr::null_mut();
            if !ok(gp::GdipCreateFromHDC(hdc, &mut g)) || g.is_null() {
                return None;
            }
            // 8x8 超采样抗锯齿：小半径圆角也能平滑过渡
            let _ = gp::GdipSetSmoothingMode(g, gp::SmoothingModeAntiAlias8x8);
            // 半像素偏移：1px 描边正好压在整像素上，不会被摊成两行浅灰
            let _ = gp::GdipSetPixelOffsetMode(g, gp::PixelOffsetModeHalf);
            Some(Self(g))
        }
    }

    /// 圆角矩形路径（四段 90° 圆弧，半径自动收敛到短边一半）。
    fn round_path(&self, rc: &RECT, radius: i32) -> Option<*mut gp::GpPath> {
        if rc.right <= rc.left || rc.bottom <= rc.top {
            return None;
        }
        let x0 = rc.left as f32;
        let y0 = rc.top as f32;
        let x1 = rc.right as f32;
        let y1 = rc.bottom as f32;
        let r = (radius as f32).min((x1 - x0) / 2.0).min((y1 - y0) / 2.0).max(0.0);
        let d = r * 2.0;
        unsafe {
            let mut p: *mut gp::GpPath = std::ptr::null_mut();
            if !ok(gp::GdipCreatePath(gp::FillModeAlternate, &mut p)) || p.is_null() {
                return None;
            }
            // 顺时针：左上 -> 右上 -> 右下 -> 左下
            let _ = gp::GdipAddPathArc(p, x0, y0, d, d, 180.0, 90.0);
            let _ = gp::GdipAddPathArc(p, x1 - d, y0, d, d, 270.0, 90.0);
            let _ = gp::GdipAddPathArc(p, x1 - d, y1 - d, d, d, 0.0, 90.0);
            let _ = gp::GdipAddPathArc(p, x0, y1 - d, d, d, 90.0, 90.0);
            let _ = gp::GdipClosePathFigure(p);
            Some(p)
        }
    }

    fn fill_path(&self, path: *mut gp::GpPath, color: COLORREF) {
        unsafe {
            let mut br: *mut gp::GpSolidFill = std::ptr::null_mut();
            if ok(gp::GdipCreateSolidFill(argb(color), &mut br)) {
                let _ = gp::GdipFillPath(self.0, br as *mut gp::GpBrush, path);
                let _ = gp::GdipDeleteBrush(br as *mut gp::GpBrush);
            }
        }
    }

    fn stroke_path(&self, path: *mut gp::GpPath, color: COLORREF, width: f32) {
        unsafe {
            let mut pen: *mut gp::GpPen = std::ptr::null_mut();
            if ok(gp::GdipCreatePen1(argb(color), width, gp::UnitPixel, &mut pen)) {
                let _ = gp::GdipSetPenLineJoin(pen, gp::LineJoinRound);
                let _ = gp::GdipDrawPath(self.0, pen, path);
                let _ = gp::GdipDeletePen(pen);
            }
        }
    }

    /// 实心圆（可指定 alpha）：指示灯的光晕层靠它一层层叠出来。
    fn disc(&self, cx: f32, cy: f32, r: f32, color: COLORREF, alpha: u8) {
        unsafe {
            let mut br: *mut gp::GpSolidFill = std::ptr::null_mut();
            if ok(gp::GdipCreateSolidFill(argb_a(color, alpha), &mut br)) {
                let _ = gp::GdipFillEllipse(
                    self.0,
                    br as *mut gp::GpBrush,
                    cx - r,
                    cy - r,
                    r * 2.0,
                    r * 2.0,
                );
                let _ = gp::GdipDeleteBrush(br as *mut gp::GpBrush);
            }
        }
    }
}

impl Drop for Gfx {
    fn drop(&mut self) {
        unsafe {
            let _ = gp::GdipDeleteGraphics(self.0);
        }
    }
}

// ---------------------------------------------------------------- 绘制原语

/// 用纯色填满矩形。
pub fn fill(hdc: HDC, rc: &RECT, brush: HBRUSH) {
    unsafe {
        FillRect(hdc, rc, brush);
    }
}

/// 圆角矩形：实心填充 + 可选 1px 描边（抗锯齿）。
pub fn round_rect(hdc: HDC, rc: &RECT, radius: i32, color: COLORREF, border: Option<COLORREF>) {
    unsafe {
        if let Some(gfx) = Gfx::new(hdc) {
            if let Some(path) = gfx.round_path(rc, radius) {
                gfx.fill_path(path, color);
                if let Some(bc) = border {
                    gfx.stroke_path(path, bc, 1.0);
                }
                let _ = gp::GdipDeletePath(path);
                return;
            }
        }
        gdi_round(hdc, rc, radius, Some(color), border);
    }
}

/// 只描边的圆角矩形（内部透明），抗锯齿。
pub fn round_stroke(hdc: HDC, rc: &RECT, radius: i32, color: COLORREF) {
    unsafe {
        if let Some(gfx) = Gfx::new(hdc) {
            if let Some(path) = gfx.round_path(rc, radius) {
                gfx.stroke_path(path, color, 1.0);
                let _ = gp::GdipDeletePath(path);
                return;
            }
        }
        gdi_round(hdc, rc, radius, None, Some(color));
    }
}

/// GDI 兜底圆角矩形（仅 GDI+ 不可用时走，无抗锯齿）。
unsafe fn gdi_round(
    hdc: HDC,
    rc: &RECT,
    radius: i32,
    fill_color: Option<COLORREF>,
    border: Option<COLORREF>,
) {
    let br = fill_color.map(|c| CreateSolidBrush(c));
    let pen = border.map(|c| CreatePen(PS_SOLID, 1, c));
    let old_b = match br {
        Some(b) => SelectObject(hdc, HGDIOBJ(b.0)),
        None => SelectObject(hdc, GetStockObject(NULL_BRUSH)),
    };
    let old_p = match pen {
        Some(p) => SelectObject(hdc, HGDIOBJ(p.0)),
        None => SelectObject(hdc, GetStockObject(NULL_PEN)),
    };
    let _ = RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, radius * 2, radius * 2);
    SelectObject(hdc, old_b);
    SelectObject(hdc, old_p);
    if let Some(b) = br {
        let _ = DeleteObject(HGDIOBJ(b.0));
    }
    if let Some(p) = pen {
        let _ = DeleteObject(HGDIOBJ(p.0));
    }
}

/// 实心椭圆/圆（抗锯齿）。
pub fn ellipse(hdc: HDC, rc: &RECT, color: COLORREF) {
    unsafe {
        if let Some(gfx) = Gfx::new(hdc) {
            let mut br: *mut gp::GpSolidFill = std::ptr::null_mut();
            if ok(gp::GdipCreateSolidFill(argb(color), &mut br)) {
                let _ = gp::GdipFillEllipse(
                    gfx.0,
                    br as *mut gp::GpBrush,
                    rc.left as f32,
                    rc.top as f32,
                    (rc.right - rc.left) as f32,
                    (rc.bottom - rc.top) as f32,
                );
                let _ = gp::GdipDeleteBrush(br as *mut gp::GpBrush);
                return;
            }
        }
        let br = CreateSolidBrush(color);
        let old_b = SelectObject(hdc, HGDIOBJ(br.0));
        let old_p = SelectObject(hdc, GetStockObject(NULL_PEN));
        let _ = Ellipse(hdc, rc.left, rc.top, rc.right, rc.bottom);
        SelectObject(hdc, old_b);
        SelectObject(hdc, old_p);
        let _ = DeleteObject(HGDIOBJ(br.0));
    }
}

/// 指示灯：实心灯芯 + 多层半透明光晕 + 左上高光，模拟 LED 灯珠的发光感。
///
/// `(cx, cy)` 是灯芯中心，`r` 是灯芯半径。`on = false` 时用浅灰灯芯 + 极淡的
/// 一圈底色，表示「未连接」而不是「熄灭的黑洞」。
pub fn led(hdc: HDC, cx: i32, cy: i32, r: i32, color: COLORREF, on: bool) {
    let r = r.max(1) as f32;
    let (fx, fy) = (cx as f32, cy as f32);
    let core = if on { color } else { BORDER };

    if let Some(gfx) = Gfx::new(hdc) {
        if on {
            // 五层递减光晕：半径差小、alpha 差小，叠出来的溢光没有明显同心环
            for (k, a) in [(2.6_f32, 14u8), (2.2, 22), (1.85, 32), (1.55, 44), (1.25, 60)] {
                gfx.disc(fx, fy, r * k, color, a);
            }
        } else {
            gfx.disc(fx, fy, r * 1.6, core, 46);
        }
        gfx.disc(fx, fy, r, core, 255);
        if on {
            // 左上高光：灯珠看起来像有玻璃罩
            gfx.disc(fx - r * 0.30, fy - r * 0.34, r * 0.44, WHITE, 120);
        }
        return;
    }

    // 没有 GDI+ 时降级成纯色圆（光晕画不出来，但位置与大小一致）
    let rc = RECT {
        left: cx - r as i32,
        top: cy - r as i32,
        right: cx + r as i32,
        bottom: cy + r as i32,
    };
    ellipse(hdc, &rc, core);
}

/// 折线（抗锯齿 + 圆头圆角），用于 logo 波形 / 下拉箭头等斜线。
pub fn polyline(hdc: HDC, pts: &[POINT], color: COLORREF, width: i32) {
    if pts.len() < 2 {
        return;
    }
    unsafe {
        if let Some(gfx) = Gfx::new(hdc) {
            let mut pen: *mut gp::GpPen = std::ptr::null_mut();
            if ok(gp::GdipCreatePen1(
                argb(color),
                width as f32,
                gp::UnitPixel,
                &mut pen,
            )) {
                let _ = gp::GdipSetPenLineJoin(pen, gp::LineJoinRound);
                let _ = gp::GdipSetPenStartCap(pen, gp::LineCapRound);
                let _ = gp::GdipSetPenEndCap(pen, gp::LineCapRound);
                let gp_pts: Vec<gp::Point> = pts
                    .iter()
                    .map(|p| gp::Point { X: p.x, Y: p.y })
                    .collect();
                let _ = gp::GdipDrawLinesI(gfx.0, pen, gp_pts.as_ptr(), gp_pts.len() as i32);
                let _ = gp::GdipDeletePen(pen);
                return;
            }
        }
        let p = CreatePen(PS_SOLID, width, color);
        let old_p = SelectObject(hdc, HGDIOBJ(p.0));
        let _ = Polyline(hdc, pts);
        SelectObject(hdc, old_p);
        let _ = DeleteObject(HGDIOBJ(p.0));
    }
}

/// 单行文本（支持自动省略）。
pub fn text(
    hdc: HDC,
    s: &str,
    rc: &RECT,
    font: HFONT,
    color: COLORREF,
    flags: DRAW_TEXT_FORMAT,
) {
    unsafe {
        let old_f = SelectObject(hdc, HGDIOBJ(font.0));
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, color);
        let mut r = *rc;
        let mut wide: Vec<u16> = s.encode_utf16().collect();
        DrawTextW(hdc, &mut wide, &mut r, flags);
        SelectObject(hdc, old_f);
    }
}

/// 四边等量外扩（正值）/ 内缩（负值）。
pub fn outset(r: &RECT, n: i32) -> RECT {
    inflate(r, n, n, n, n)
}

/// 非对称矩形外扩：绘制「视觉框」时统一使用，避免多处手写 RECT。
pub fn inflate(r: &RECT, l: i32, t: i32, rr: i32, b: i32) -> RECT {
    RECT {
        left: r.left - l,
        top: r.top - t,
        right: r.right + rr,
        bottom: r.bottom + b,
    }
}
