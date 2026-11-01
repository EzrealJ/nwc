//! 托盘图标：16x16 像素直接代码生成 HICON（免 .ico 资源文件），状态换色零渲染成本。

#![cfg(windows)]

use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, DestroyIcon, HICON, ICONINFO};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum IconKind {
    Scanning,
    Listening,
    Connected,
    Paused,
    Stopped,
}

impl IconKind {
    pub fn from_status(s: &nwc_core::sync::Status) -> IconKind {
        use nwc_core::sync::Status::*;
        match s {
            Starting | Scanning | Connecting(_) => IconKind::Scanning,
            Listening => IconKind::Listening,
            Connected(_) => IconKind::Connected,
            Paused => IconKind::Paused,
            Stopped => IconKind::Stopped,
        }
    }
}

/// 状态底色 (B, G, R)；前景一律白
fn bg_color(kind: IconKind) -> [u8; 3] {
    match kind {
        IconKind::Scanning => [0xD9, 0x6F, 0x1F],  // 橙（扫描/连接中）
        IconKind::Listening => [0xD9, 0xB4, 0x00], // 金（监听中）
        IconKind::Connected => [0x3C, 0xB0, 0x2F], // 绿（已连接）
        IconKind::Paused => [0xE1, 0x79, 0x17],    // 蓝（已暂停）
        IconKind::Stopped => [0x4C, 0x4C, 0xD9],   // 红（错误/停止）
    }
}

// 12x12 符号位图（'#' = 白色前景，'.' = 保留底色）
const GLYPH_SCAN: &[&str] = &[
    "....####....",
    "..##....##..",
    ".#....#...#.",
    "#....###...#",
    "#...#####..#",
    "#...#####..#",
    "#....###...#",
    ".#....#...#.",
    "..##....##..",
    "....####....",
    "............",
    "............",
];

const GLYPH_LISTEN: &[&str] = &[
    "....####....",
    "..##....##..",
    ".#........#.",
    "#..........#",
    "#....##....#",
    "#....##....#",
    "#....##....#",
    ".#........#.",
    "..##....##..",
    "....####....",
    "............",
    "............",
];

const GLYPH_CHECK: &[&str] = &[
    "............",
    "..........#.",
    ".........##.",
    "........##..",
    "#......##...",
        "##....##....",
    ".#####......",
    "..###.......",
    "...#........",
    "............",
    "............",
    "............",
];

const GLYPH_PAUSE: &[&str] = &[
    "............",
    "............",
    "..##....##..",
    "..##....##..",
    "..##....##..",
    "..##....##..",
    "..##....##..",
    "..##....##..",
    "..##....##..",
    "............",
    "............",
    "............",
];

const GLYPH_STOP: &[&str] = &[
    "............",
    "##......##..",
    ".##....##...",
    "..##..##....",
    "...####.....",
    "...####.....",
    "..##..##....",
    ".##....##...",
    "##......##..",
    "............",
    "............",
    "............",
];

fn glyph_of(kind: IconKind) -> &'static [&'static str] {
    match kind {
        IconKind::Scanning => GLYPH_SCAN,
        IconKind::Listening => GLYPH_LISTEN,
        IconKind::Connected => GLYPH_CHECK,
        IconKind::Paused => GLYPH_PAUSE,
        IconKind::Stopped => GLYPH_STOP,
    }
}

/// 生成 16x16 BGRA 像素：圆角底色盘 + 白色符号 + 1px 描边
fn render(kind: IconKind) -> Vec<u8> {
    const W: i32 = 16;
    const H: i32 = 16;
    let bg = bg_color(kind);
    let glyph = glyph_of(kind);
    let mut px = vec![0u8; (W * H * 4) as usize];
    for y in 0..H {
        for x in 0..W {
            let fx = x as f32 + 0.5 - 8.0;
            let fy = y as f32 + 0.5 - 8.0;
            let d = (fx * fx + fy * fy).sqrt();
            let i = ((y * W + x) * 4) as usize;
            if d > 7.7 {
                continue; // 透明
            }
            let mut c = bg;
            // 描边
            if d > 6.4 {
                for ch in c.iter_mut() {
                    *ch = ch.saturating_mul(70) / 100;
                }
            }
            // 符号（glyph 12x12 居中偏移 2,2）
            let gx = x - 2;
            let gy = y - 2;
            if (0..12).contains(&gx) && (0..12).contains(&gy) {
                let row = glyph[gy as usize];
                if row.as_bytes().get(gx as usize) == Some(&b'#') {
                    c = [0xFF, 0xFF, 0xFF];
                }
            }
            px[i] = c[0];
            px[i + 1] = c[1];
            px[i + 2] = c[2];
            px[i + 3] = 0xFF;
        }
    }
    px
}

/// CreateIconIndirect 引用的两张位图必须存活于图标整个生命周期——
/// 系统并不保证拷贝位图，创建后立即 DeleteObject 会让托盘图标变成空白占位
///（这正是首版托盘空白的原因）。状态只有 5 种且极少重建，持有成本可忽略。
static LIVE_BITMAPS: std::sync::Mutex<Vec<(&'static str, usize, usize)>> = std::sync::Mutex::new(Vec::new());

/// 生成状态图标（调用方负责 DestroyIcon；托盘常驻复用，无需频繁销毁）
pub fn make_icon(kind: IconKind) -> windows::core::Result<HICON> {
    unsafe {
        let px = render(kind);
        let mut hdr = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: 16,
            biHeight: -16, // 自顶向下
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let bmi = BITMAPINFO { bmiHeader: hdr, ..Default::default() };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let Ok(color) = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return Err(windows::core::Error::from_win32());
        };
        if bits.is_null() {
            return Err(windows::core::Error::from_win32());
        }
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u8, px.len());
        // 掩码位图全 0（不透明）：32bpp 带 alpha 通道时掩码不参与显示
        let mask_bits = vec![0u8; 4 * 16];
        let mask = CreateBitmap(16, 16, 1, 1, Some(mask_bits.as_ptr() as *const _));
        let info = ICONINFO {
            fIcon: windows::Win32::Foundation::BOOL::from(true),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        // 位图交给全局持有（不删除），见 LIVE_BITMAPS 注释
        LIVE_BITMAPS
            .lock()
            .unwrap()
            .push(("", color.0 as usize, mask.0 as usize));
        CreateIconIndirect(&info)
    }
}

pub fn destroy_icon(icon: HICON) {
    unsafe {
        let _ = DestroyIcon(icon);
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, ICONINFO};

    /// 自检：五种状态图标都能创建、句柄有效、带彩色位图（否则托盘显示空白占位）
    #[test]
    fn all_icons_create_valid() {
        for k in [
            IconKind::Scanning,
            IconKind::Listening,
            IconKind::Connected,
            IconKind::Paused,
            IconKind::Stopped,
        ] {
            let h = make_icon(k).unwrap_or_else(|e| panic!("make_icon({k:?}) failed: {e}"));
            assert!(h.0 != 0, "null icon handle");
            unsafe {
                let mut ii = ICONINFO::default();
                assert!(GetIconInfo(h, &mut ii).is_ok(), "GetIconInfoW failed");
                assert!(ii.hbmColor.0 as usize != 0, "no color bitmap");
                let _ = DestroyIcon(h);
            }
        }
    }
}
