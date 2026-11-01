//! Windows 原生剪贴板：隐藏窗口 + AddClipboardFormatListener + WM_CLIPBOARDUPDATE 事件驱动
//! （替代 Node 版常驻 PowerShell + 60ms 轮询）。
//!
//! 回声抑制（平台层，对应 ps1 代理里的序列号吸收 + 内容令牌去重）：
//! 1. 自己写入后记录 GetClipboardSequenceNumber，变更事件序列号相同则直接吸收；
//! 2. 内容令牌 2000ms TTL：相同内容（读回形态）不重复上报。
//!
//! 图像：读优先注册格式 "PNG"（原始字节，无损），回退 CF_DIBV5/CF_DIB → PNG 转码；
//! 写同时挂 PNG 原始字节 + CF_DIBV5，各类粘贴目标都能识别。

#![cfg(windows)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GlobalFree, BOOL, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
};
use windows::Win32::System::Ole::{CLIPBOARD_FORMAT, CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::{DragQueryFileW, DROPFILES};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, GetWindowLongPtrW,
    PostMessageW, RegisterClassW, SetWindowLongPtrW, TranslateMessage, GWLP_USERDATA, MSG, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_CLIPBOARDUPDATE, WM_CLOSE, WM_DESTROY, WNDCLASSW,
};

use nwc_core::sync::ClipEvent;

const TOKEN_TTL: Duration = Duration::from_millis(2000);
const OPEN_RETRY_READ: (u32, Duration) = (5, Duration::from_millis(60));
const OPEN_RETRY_WRITE: (u32, Duration) = (10, Duration::from_millis(90));

pub struct WinClipboard {
    inner: Arc<Inner>,
    hwnd: Arc<AtomicIsize>,
}

struct Inner {
    tx: Sender<ClipEvent>,
    /// 自己写入后的剪贴板序列号：相同则吸收
    self_seq: Mutex<u64>,
    /// 内容令牌（读回形态）+ 记录时间
    token: Mutex<(String, Option<Instant>)>,
}

impl Inner {
    fn absorb(&self, seq_now: u64) -> bool {
        seq_now != 0 && *self.self_seq.lock().unwrap() == seq_now
    }
    fn token_hit(&self, tok: &str) -> bool {
        if tok.is_empty() {
            return true; // 空内容不上报
        }
        let t = self.token.lock().unwrap();
        t.1.map(|at| at.elapsed() < TOKEN_TTL).unwrap_or(false) && t.0 == tok
    }
    fn token_remember(&self, tok: &str) {
        *self.token.lock().unwrap() = (tok.to_string(), Some(Instant::now()));
    }
}

impl nwc_core::sync::ClipboardWriter for WinClipboard {
    fn set_text(&self, s: &str) -> bool {
        let ok = write_text(s);
        self.after_self_write(format!("t:{}", s));
        ok
    }
    fn set_image(&self, png_bytes: &[u8]) -> bool {
        let ok = write_image(png_bytes);
        // 图片读回形态与写入字节可能不同（DIBV5 重编码），令牌用写入内容的哈希，
        // 读回时若拿到相同 PNG 原始字节即命中；序列号吸收是第一道防线
        self.after_self_write(format!("i:{}", nwc_core::util::sha256_hex(png_bytes)));
        ok
    }
    fn set_files(&self, paths: &[PathBuf]) -> bool {
        let ok = write_files(paths);
        let tok = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        self.after_self_write(format!("f:{tok}"));
        ok
    }
}

impl WinClipboard {
    /// 启动剪贴板监听线程；事件经 tx 上抛（等窗口就绪后返回）
    pub fn start(tx: Sender<ClipEvent>) -> Arc<WinClipboard> {
        let clip = Arc::new(WinClipboard {
            inner: Arc::new(Inner {
                tx,
                self_seq: Mutex::new(0),
                token: Mutex::new((String::new(), None)),
            }),
            hwnd: Arc::new(AtomicIsize::new(0)),
        });
        let inner = Arc::clone(&clip.inner);
        let hwnd_flag = Arc::clone(&clip.hwnd);
        std::thread::Builder::new()
            .name("nwc-clipboard".into())
            .spawn(move || message_loop(inner, hwnd_flag))
            .expect("spawn clipboard thread");
        let deadline = Instant::now() + Duration::from_secs(3);
        while clip.hwnd.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(15));
        }
        clip
    }

    pub fn request_stop(&self) {
        let h = self.hwnd.load(Ordering::SeqCst);
        if h != 0 {
            unsafe {
                let _ = PostMessageW(HWND(h), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn after_self_write(&self, token: String) {
        let seq = unsafe { GetClipboardSequenceNumber() } as u64;
        *self.inner.self_seq.lock().unwrap() = seq;
        self.inner.token_remember(&token);
    }
}

// ---- 监听线程（消息泵） ----

fn message_loop(inner: Arc<Inner>, hwnd_flag: Arc<AtomicIsize>) {
    unsafe {
        let hmod = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
        let hinst = windows::Win32::Foundation::HINSTANCE(hmod.0);
        let class_name = w!("NWC_CLIP_WND");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            lpszClassName: class_name,
            hInstance: hinst,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return;
        }
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!(""),
            WINDOW_STYLE(0), // 隐藏消息窗口（不能是 message-only：收不到剪贴板广播）
            0,
            0,
            0,
            0,
            None,
            None,
            Some(&hinst),
            None,
        );
        if hwnd.0 == 0 {
            return;
        }
        let ptr = Box::into_raw(Box::new(Arc::clone(&inner)));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
        hwnd_flag.store(hwnd.0, Ordering::SeqCst);
        let _ = AddClipboardFormatListener(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
        // 消息循环结束（WM_QUIT）：窗口已毁，释放 userdata
        if ptr as usize != 0 {
            drop(Box::from_raw(ptr));
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if p != 0 {
                let inner = &*(p as *const Arc<Inner>);
                on_clipboard_update(inner);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn on_clipboard_update(inner: &Inner) {
    let seq = unsafe { GetClipboardSequenceNumber() } as u64;
    if inner.absorb(seq) {
        return;
    }
    // 格式优先级：文件 > 文本 > 图片（文本+图片并存时按文本，与 Node 版一致）
    if let Some(paths) = read_files() {
        let tok = format!("f:{}", paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join("\n"));
        if !inner.token_hit(&tok) {
            inner.token_remember(&tok);
            let _ = inner.tx.send(ClipEvent::Files(paths));
        }
        return;
    }
    if let Some(text) = read_text() {
        let tok = format!("t:{}", text);
        if !inner.token_hit(&tok) {
            inner.token_remember(&tok);
            let _ = inner.tx.send(ClipEvent::Text(text));
        }
        return;
    }
    if let Some(png) = read_image_png() {
        let tok = format!("i:{}", nwc_core::util::sha256_hex(&png));
        if !inner.token_hit(&tok) {
            inner.token_remember(&tok);
            let _ = inner.tx.send(ClipEvent::Image(png));
        }
    }
}

// ---- 读取 ----

fn with_open_clipboard<T>(retries: (u32, Duration), f: impl Fn() -> Option<T>) -> Option<T> {
    for i in 0..retries.0 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            let r = f();
            unsafe { let _ = CloseClipboard(); };
            return r;
        }
        if i + 1 < retries.0 {
            std::thread::sleep(retries.1);
        }
    }
    None
}

fn read_text() -> Option<String> {
    with_open_clipboard(OPEN_RETRY_READ, || unsafe {
        if !IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_ok() {
            return None;
        }
        let Ok(h) = GetClipboardData(CF_UNICODETEXT.0 as u32) else { return None };
        let hg = HGLOBAL(h.0 as *mut _);
        let ptr = GlobalLock(hg) as *const u16;
        if ptr.is_null() {
            return None;
        }
        let len = GlobalSize(hg) / 2;
        let mut end = len;
        for i in 0..len {
            if *ptr.add(i as usize) == 0 {
                end = i;
                break;
            }
        }
        let slice = std::slice::from_raw_parts(ptr, end as usize);
        let s = String::from_utf16_lossy(slice);
        let _ = GlobalUnlock(hg);
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    })
}

fn read_files() -> Option<Vec<PathBuf>> {
    with_open_clipboard(OPEN_RETRY_READ, || unsafe {
        if !IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() {
            return None;
        }
        let Ok(h) = GetClipboardData(CF_HDROP.0 as u32) else { return None };
        let drop = windows::Win32::UI::Shell::HDROP(h.0);
        let cnt = DragQueryFileW(drop, u32::MAX, None);
        if cnt == 0 {
            return None;
        }
        let mut out = Vec::with_capacity(cnt as usize);
        for i in 0..cnt {
            let need = DragQueryFileW(drop, i, None);
            let mut buf = vec![0u16; need as usize + 1];
            let n = DragQueryFileW(drop, i, Some(&mut buf));
            if n > 0 {
                buf.truncate(n as usize);
                out.push(PathBuf::from(String::from_utf16_lossy(&buf)));
            }
        }
        (!out.is_empty()).then_some(out)
    })
}

fn register_png_format() -> CLIPBOARD_FORMAT {
    let name: Vec<u16> = "PNG\0".encode_utf16().collect();
    CLIPBOARD_FORMAT(unsafe { RegisterClipboardFormatW(PCWSTR(name.as_ptr())) } as u16)
}

/// 读图：优先注册格式 "PNG"（原始字节），回退 CF_DIBV5 / CF_DIB 转 PNG
fn read_image_png() -> Option<Vec<u8>> {
    let png_fmt = register_png_format();
    if let Some(bytes) = with_open_clipboard(OPEN_RETRY_READ, || unsafe {
        if !IsClipboardFormatAvailable(png_fmt.0 as u32).is_ok() {
            return None;
        }
        let Ok(h) = GetClipboardData(png_fmt.0 as u32) else { return None };
        copy_hglobal(h)
    }) {
        if nwc_core::util::is_png(&bytes) {
            return Some(bytes);
        }
    }
    for (fmt, v5) in [(CF_DIBV5, true), (CF_DIB, false)] {
        let bytes = with_open_clipboard(OPEN_RETRY_READ, || unsafe {
            if !IsClipboardFormatAvailable(fmt.0 as u32).is_ok() {
                return None;
            }
            let Ok(h) = GetClipboardData(fmt.0 as u32) else { return None };
            copy_hglobal(h)
        });
        if let Some(bytes) = bytes {
            if let Some(png) = dib_to_png(&bytes, v5) {
                return Some(png);
            }
        }
    }
    None
}

unsafe fn copy_hglobal(h: HANDLE) -> Option<Vec<u8>> {
    let hg = HGLOBAL(h.0 as *mut _);
    let ptr = GlobalLock(hg) as *const u8;
    if ptr.is_null() {
        return None;
    }
    let len = GlobalSize(hg);
    let v = std::slice::from_raw_parts(ptr, len).to_vec();
    let _ = GlobalUnlock(hg);
    Some(v)
}

// ---- 写入 ----

unsafe fn alloc_hglobal(data: &[u8]) -> Option<HGLOBAL> {
    let Ok(h) = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, data.len()) else { return None };
    let ptr = GlobalLock(h) as *mut u8;
    if ptr.is_null() {
        let _ = GlobalFree(h);
        return None;
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
    let _ = GlobalUnlock(h);
    Some(h)
}

fn write_text(s: &str) -> bool {
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let mut bytes = Vec::with_capacity(wide.len() * 2);
    for c in wide {
        bytes.extend_from_slice(&c.to_le_bytes());
    }
    with_open_clipboard(OPEN_RETRY_WRITE, || unsafe {
        let Some(h) = alloc_hglobal(&bytes) else { return None };
        if EmptyClipboard().is_err() {
            let _ = GlobalFree(h);
            return None;
        }
        if SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(h.0 as isize)).is_err() {
            let _ = GlobalFree(h);
            return None;
        }
        Some(true)
    })
    .unwrap_or(false)
}

fn write_files(paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return false;
    }
    // DROPFILES { pFiles=sizeof, fWide=1 } + 双 NUL 结尾的宽字符路径列表
    let mut blob: Vec<u8> = Vec::new();
    let df = DROPFILES {
        pFiles: std::mem::size_of::<DROPFILES>() as u32,
        pt: Default::default(),
        fNC: BOOL(0),
        fWide: BOOL(1),
    };
    unsafe {
        blob.extend_from_slice(std::slice::from_raw_parts(
            &df as *const _ as *const u8,
            std::mem::size_of::<DROPFILES>(),
        ));
    }
    for p in paths {
        let wide: Vec<u16> = p.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
        for c in wide {
            blob.extend_from_slice(&c.to_le_bytes());
        }
    }
    blob.extend_from_slice(&[0u8, 0]); // 列表结尾第二个 NUL
    with_open_clipboard(OPEN_RETRY_WRITE, || unsafe {
        let Some(h) = alloc_hglobal(&blob) else { return None };
        if EmptyClipboard().is_err() {
            let _ = GlobalFree(h);
            return None;
        }
        if SetClipboardData(CF_HDROP.0 as u32, HANDLE(h.0 as isize)).is_err() {
            let _ = GlobalFree(h);
            return None;
        }
        Some(true)
    })
    .unwrap_or(false)
}

fn write_image(png_bytes: &[u8]) -> bool {
    if !nwc_core::util::is_png(png_bytes) {
        return false;
    }
    let Some(dib) = png_to_dibv5(png_bytes) else { return false };
    with_open_clipboard(OPEN_RETRY_WRITE, || unsafe {
        if EmptyClipboard().is_err() {
            return None;
        }
        let fmt = register_png_format();
        if let Some(h) = alloc_hglobal(png_bytes) {
            if SetClipboardData(fmt.0 as u32, HANDLE(h.0 as isize)).is_err() {
                let _ = GlobalFree(h);
            }
        }
        if let Some(h) = alloc_hglobal(&dib) {
            if SetClipboardData(CF_DIBV5.0 as u32, HANDLE(h.0 as isize)).is_err() {
                let _ = GlobalFree(h);
            }
        }
        Some(true)
    })
    .unwrap_or(false)
}

// ---- 图像转换（png crate，纯 Rust） ----

/// 从字节切片安全读小端 u32/i32
fn rd32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

/// 位掩码提取并归一到 0..255
fn mask_extract(v: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let count = mask.count_ones();
    let raw = (v & mask) >> shift;
    if count >= 8 {
        (raw >> (count - 8)) as u8
    } else {
        ((raw.saturating_mul(255)) / ((1u32 << count) - 1).max(1)) as u8
    }
}

/// DIB(V5) → PNG。
/// BITMAPV5HEADER 偏移：Size@0 Width@4 Height@8 Planes@12 BitCount@16 Compression@20
/// ClrUsed@36 RedMask@44 GreenMask@48 BlueMask@52 AlphaMask@56（头大小 124）。
/// 支持 24/32bpp BI_RGB 与 BI_BITFIELDS；自底向上/自顶向下。
fn dib_to_png(dib: &[u8], v5: bool) -> Option<Vec<u8>> {
    let hdr_len = if v5 { 124 } else { 40 };
    if dib.len() < hdr_len {
        return None;
    }
    let width = rd32(dib, 4)? as i32;
    let width = width.unsigned_abs() as usize;
    let height_raw = rd32(dib, 8)? as i32;
    let (height, top_down) = if height_raw < 0 {
        (height_raw.unsigned_abs() as usize, true)
    } else {
        (height_raw as usize, false)
    };
    let planes = rd32(dib, 12)?;
    let bpp = rd32(dib, 16)? as usize;
    let compression = rd32(dib, 20)?;
    let clr_used = rd32(dib, 36)? as usize;
    if planes != 1 || (bpp != 24 && bpp != 32) || (compression != 0 && compression != 3) || width == 0 || height == 0 {
        return None;
    }
    let (rm, gm, bm, am) = if v5 {
        (
            rd32(dib, 44).unwrap_or(0x00FF0000),
            rd32(dib, 48).unwrap_or(0x0000FF00),
            rd32(dib, 52).unwrap_or(0x000000FF),
            rd32(dib, 56).unwrap_or(0),
        )
    } else if compression == 3 {
        // 掩码在 BITMAPINFOHEADER 之后的 3 个 DWORD
        (
            rd32(dib, 40)?,
            rd32(dib, 44)?,
            rd32(dib, 48)?,
            0u32,
        )
    } else if bpp == 32 {
        (0x00FF0000, 0x0000FF00, 0x000000FF, 0)
    } else {
        (0, 0, 0, 0) // 24bpp 固定 BGR 序
    };
    let px_off = hdr_len
        + if !v5 && compression == 3 { 12 } else { 0 }
        + clr_used * 4;
    let stride = (width * bpp / 8 + 3) & !3usize;
    let px = dib.get(px_off..)?;
    if px.len() < stride * height {
        return None;
    }
    let mut rgba: Vec<u8> = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        let y = if top_down { row } else { height - 1 - row };
        let line = &px[y * stride..y * stride + width * bpp / 8];
        for x in 0..width {
            let p = &line[x * bpp / 8..x * bpp / 8 + bpp / 8];
            if bpp == 24 {
                rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
            } else {
                let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                let a = if am != 0 { mask_extract(v, am) } else { 255 };
                rgba.extend_from_slice(&[mask_extract(v, rm), mask_extract(v, gm), mask_extract(v, bm), a]);
            }
        }
    }
    encode_png(width as u32, height as u32, &rgba)
}

/// PNG → BITMAPV5HEADER + 自底向上 BGRA 像素
fn png_to_dibv5(png_bytes: &[u8]) -> Option<Vec<u8>> {
    let dec = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let rgba = &buf[..info.buffer_size()];
    let stride = (w * 4 + 3) & !3usize;
    let mut dib = vec![0u8; 124 + stride * h];
    dib[0..4].copy_from_slice(&124u32.to_le_bytes());
    dib[4..8].copy_from_slice(&(w as u32).to_le_bytes());
    dib[8..12].copy_from_slice(&(h as u32).to_le_bytes()); // 正数 = 自底向上
    dib[12..16].copy_from_slice(&1u32.to_le_bytes());
    dib[16..20].copy_from_slice(&32u32.to_le_bytes());
    dib[20..24].copy_from_slice(&0u32.to_le_bytes()); // BI_RGB
    dib[24..28].copy_from_slice(&((stride * h) as u32).to_le_bytes());
    dib[36..40].copy_from_slice(&0u32.to_le_bytes()); // ClrUsed
    dib[44..48].copy_from_slice(&0x00FF0000u32.to_le_bytes()); // RedMask
    dib[48..52].copy_from_slice(&0x0000FF00u32.to_le_bytes()); // GreenMask
    dib[52..56].copy_from_slice(&0x000000FFu32.to_le_bytes()); // BlueMask
    dib[56..60].copy_from_slice(&0xFF000000u32.to_le_bytes()); // AlphaMask
    dib[60..64].copy_from_slice(&0x73524742u32.to_le_bytes()); // LCS_sRGB
    dib[112..116].copy_from_slice(&4u32.to_le_bytes()); // LCS_GM_IMAGES intent
    for row in 0..h {
        let src = &rgba[row * w * 4..(row + 1) * w * 4];
        let dst = &mut dib[124 + (h - 1 - row) * stride..124 + (h - 1 - row) * stride + w * 4];
        for x in 0..w {
            dst[x * 4] = src[x * 4 + 2];
            dst[x * 4 + 1] = src[x * 4 + 1];
            dst[x * 4 + 2] = src[x * 4];
            dst[x * 4 + 3] = src[x * 4 + 3];
        }
    }
    Some(dib)
}

fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(out)
}
