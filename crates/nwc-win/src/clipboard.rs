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
    /// 诊断日志通道（剪贴板变化但内容识别失败时上报格式列表/原因，壳层写日志）
    diag: Option<Sender<String>>,
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
    /// 启动剪贴板监听线程；事件经 tx 上抛（等窗口就绪后返回）。
    /// diag 可选：识别失败时上报诊断信息（格式列表/解码失败原因）
    pub fn start(tx: Sender<ClipEvent>, diag: Option<Sender<String>>) -> Arc<WinClipboard> {
        let clip = Arc::new(WinClipboard {
            inner: Arc::new(Inner {
                tx,
                diag,
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
    if let Some(png) = read_image_png(inner) {
        let tok = format!("i:{}", nwc_core::util::sha256_hex(&png));
        if !inner.token_hit(&tok) {
            inner.token_remember(&tok);
            let _ = inner.tx.send(ClipEvent::Image(png));
        }
        return;
    }
    // 三类内容都没识别出来：上报剪贴板实际格式列表（排查“复制了却没同步”的关键线索）
    inner.diag_report(|| format!("剪贴板变化但未识别出可同步内容，现有格式: {}", enumerate_format_names()));
}

impl Inner {
    fn diag_report(&self, msg: impl FnOnce() -> String) {
        if let Some(d) = &self.diag {
            let _ = d.send(msg());
        }
    }
}

/// 枚举剪贴板当前全部格式（标准格式给名字，注册格式取注册名）
fn enumerate_format_names() -> String {
    use windows::Win32::System::DataExchange::{CloseClipboard, EnumClipboardFormats, GetClipboardFormatNameW, OpenClipboard};
    unsafe {
        if !OpenClipboard(None).is_ok() {
            return "(打开剪贴板失败)".into();
        }
        let mut out: Vec<String> = Vec::new();
        let mut f: u32 = 0;
        loop {
            f = EnumClipboardFormats(f);
            if f == 0 {
                break;
            }
            let name = match f {
                1 => "CF_TEXT".into(),
                2 => "CF_BITMAP".into(),
                3 => "CF_METAFILEPICT".into(),
                7 => "CF_OEMTEXT".into(),
                8 => "CF_DIB".into(),
                13 => "CF_UNICODETEXT".into(),
                15 => "CF_HDROP".into(),
                17 => "CF_DIBV5".into(),
                _ => {
                    let mut buf = [0u16; 128];
                    let n = GetClipboardFormatNameW(f, &mut buf);
                    if n > 0 {
                        String::from_utf16_lossy(&buf[..n as usize])
                    } else {
                        format!("#{f}")
                    }
                }
            };
            out.push(name);
        }
        let _ = CloseClipboard();
        if out.is_empty() {
            "(空)".into()
        } else {
            out.join(", ")
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

/// 读图：优先注册格式 "PNG"（原始字节），回退 CF_DIBV5 / CF_DIB 转 PNG。
/// 失败路径经 inner.diag 上报原因（识别不了至少留线索）
fn read_image_png(inner: &Inner) -> Option<Vec<u8>> {
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
        inner.diag_report(|| format!("PNG 格式数据 {} 字节但无 PNG 签名，已跳过", bytes.len()));
    }
    for fmt in [CF_DIBV5, CF_DIB] {
        let bytes = with_open_clipboard(OPEN_RETRY_READ, || unsafe {
            if !IsClipboardFormatAvailable(fmt.0 as u32).is_ok() {
                return None;
            }
            let Ok(h) = GetClipboardData(fmt.0 as u32) else { return None };
            copy_hglobal(h)
        });
        if let Some(bytes) = bytes {
            match nwc_core::dib::dib_to_png(&bytes) {
                Some(png) => return Some(png),
                None => inner.diag_report(|| {
                    format!(
                        "DIB 转码失败（{}），当前仅支持 24/32bpp BI_RGB/BI_BITFIELDS",
                        nwc_core::dib::describe(&bytes)
                    )
                }),
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
    let Some(dib) = nwc_core::dib::png_to_dibv5(png_bytes) else { return false };
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

#[cfg(test)]
mod tests {
    //! 平台 API 真机测试：直接操作系统剪贴板，验证写入/读取全链路。
    //! 剪贴板是进程全局状态，用静态锁强制串行（CI 再加 --test-threads=1 双保险）。

    use super::*;

    /// 全局串行锁：同一测试二进制内所有剪贴板测试互斥
    static CLIP_LOCK: Mutex<()> = Mutex::new(());

    /// 1x1 红色不透明 PNG（与 mac 端测试同源字节，确定性内容）
    const TINY_PNG_B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

    fn tiny_png() -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(TINY_PNG_B64).unwrap()
    }

    /// 测试用 Inner（read_image_png 需要；diag=None 即静默）
    fn test_inner() -> Inner {
        let (tx, _rx) = std::sync::mpsc::channel();
        Inner {
            tx,
            diag: None,
            self_seq: Mutex::new(0),
            token: Mutex::new((String::new(), None)),
        }
    }

    #[test]
    fn text_roundtrip() {
        let _g = CLIP_LOCK.lock().unwrap();
        let s = "你好 nwc ✓ emoji";
        assert!(write_text(s), "写入应成功");
        assert_eq!(read_text().as_deref(), Some(s), "读回应与写入一致");
    }

    #[test]
    fn image_roundtrip_png_and_dibv5() {
        let _g = CLIP_LOCK.lock().unwrap();
        let png = tiny_png();
        assert!(write_image(&png), "写入应成功");
        // PNG 注册格式与 CF_DIBV5 应同时挂在剪贴板上（多格式兼容的关键）
        let png_fmt = register_png_format();
        with_open_clipboard(OPEN_RETRY_READ, || unsafe {
            assert!(IsClipboardFormatAvailable(png_fmt.0 as u32).is_ok(), "应存在 PNG 注册格式");
            assert!(IsClipboardFormatAvailable(CF_DIBV5.0 as u32).is_ok(), "应存在 CF_DIBV5");
            Some(())
        })
        .unwrap();
        // 读回应拿到原始 PNG 字节（PNG 格式优先、无损直读）
        assert_eq!(read_image_png(&test_inner()).as_deref(), Some(&png[..]));
    }

    #[test]
    fn image_roundtrip_dib_only() {
        // 模拟只放 DIBV5 的来源（如截图工具）：CF_DIBV5 → 解码回 PNG
        let _g = CLIP_LOCK.lock().unwrap();
        let png = tiny_png();
        let dib = nwc_core::dib::png_to_dibv5(&png).unwrap();
        with_open_clipboard(OPEN_RETRY_WRITE, || unsafe {
            assert!(EmptyClipboard().is_ok());
            let Some(h) = alloc_hglobal(&dib) else { return None };
            assert!(SetClipboardData(CF_DIBV5.0 as u32, HANDLE(h.0 as isize)).is_ok());
            Some(true)
        })
        .unwrap();
        let back = read_image_png(&test_inner()).expect("纯 DIBV5 也应能读出图片");
        assert_eq!(&back[..8], &png[..8], "解码结果应为合法 PNG（同签名）");
    }

    #[test]
    fn files_roundtrip() {
        let _g = CLIP_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join("nwc-win-clip-test");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("测试文件.txt");
        std::fs::write(&f, b"x").unwrap();
        assert!(write_files(&[f.clone()]), "写入应成功");
        assert_eq!(read_files(), Some(vec![f]), "读回路径应一致");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
