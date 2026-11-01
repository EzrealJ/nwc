//! macOS 剪贴板：NSPasteboard 直接访问。
//!
//! - 变更检测：changeCount 进程内轮询（NSPasteboard 无通知 API，但相比 Node 版
//!   每 400ms spawn 4-6 个 osascript/pbpaste/sips 子进程，这里是零子进程、微秒级读取）
//! - 读：文件（NSFilenames）> 文本 > 图片（PNGf 优先原始字节，TIFF/JPEG 经 ImageIO 转 PNG）
//! - 写：文本 / PNGf（原始 PNG 字节直接写入，无重编码损耗）/ 文件列表（writeObjects NSURL）
//! - 回声：写后取 changeCount 基线 + 1500ms suppress（对应 Node 版 macos.js）

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::ClassType;
use objc2_app_kit::NSPasteboard;
use objc2_foundation::{NSArray, NSData, NSString, NSURL};

use nwc_core::sync::ClipEvent;

pub struct MacPasteboard {
    suppress_until: Mutex<Option<Instant>>,
    last_change: AtomicU64,
    stop: Arc<AtomicBool>,
    token: Mutex<(String, Option<Instant>)>,
}

const TOKEN_TTL: Duration = Duration::from_millis(1500);

impl nwc_core::sync::ClipboardWriter for MacPasteboard {
    fn set_text(&self, s: &str) -> bool {
        let ok = unsafe { write_text(s) };
        self.after_write(format!("t:{s}"));
        ok
    }
    fn set_image(&self, png: &[u8]) -> bool {
        let ok = unsafe { write_image_png(png) };
        self.after_write(format!("i:{}", nwc_core::util::sha256_hex(png)));
        ok
    }
    fn set_files(&self, paths: &[PathBuf]) -> bool {
        let ok = unsafe { write_files(paths) };
        let tok = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        self.after_write(format!("f:{tok}"));
        ok
    }
}

impl MacPasteboard {
    pub fn start(tx: Sender<ClipEvent>, poll_ms: u64) -> Arc<MacPasteboard> {
        let pb = Arc::new(MacPasteboard {
            suppress_until: Mutex::new(None),
            last_change: AtomicU64::new(0),
            stop: Arc::new(AtomicBool::new(false)),
            token: Mutex::new((String::new(), None)),
        });
        // 初始化基线
        unsafe {
            pb.last_change.store(current_change_count(), Ordering::SeqCst);
        }
        let p2 = Arc::clone(&pb);
        std::thread::Builder::new()
            .name("nwc-pasteboard".into())
            .spawn(move || {
                let interval = Duration::from_millis(poll_ms.clamp(50, 5000));
                loop {
                    if p2.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(interval);
                    let now = unsafe { current_change_count() };
                    if now != p2.last_change.load(Ordering::SeqCst) {
                        p2.last_change.store(now, Ordering::SeqCst);
                        p2.handle_change(&tx);
                    }
                }
            })
            .ok();
        pb
    }

    fn after_write(&self, token: String) {
        unsafe {
            self.last_change.store(current_change_count(), Ordering::SeqCst);
        }
        *self.suppress_until.lock().unwrap() = Some(Instant::now() + Duration::from_millis(1500));
        *self.token.lock().unwrap() = (token, Some(Instant::now()));
    }

    fn suppressed(&self) -> bool {
        self.suppress_until
            .lock()
            .unwrap()
            .map(|t| Instant::now() < t)
            .unwrap_or(false)
    }

    fn token_hit(&self, tok: &str) -> bool {
        if tok.is_empty() {
            return true;
        }
        let t = self.token.lock().unwrap();
        t.1.map(|at| at.elapsed() < TOKEN_TTL).unwrap_or(false) && t.0 == tok
    }

    fn handle_change(&self, tx: &Sender<ClipEvent>) {
        if self.suppressed() {
            return;
        }
        unsafe {
            // 文件 > 文本 > 图片
            if let Some(paths) = read_files() {
                let tok = format!("f:{}", paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n"));
                if !self.token_hit(&tok) {
                    *self.token.lock().unwrap() = (tok.clone(), Some(Instant::now()));
                    let _ = tx.send(ClipEvent::Files(paths));
                }
                return;
            }
            if let Some(text) = read_text() {
                if !text.is_empty() {
                    let tok = format!("t:{text}");
                    if !self.token_hit(&tok) {
                        *self.token.lock().unwrap() = (tok.clone(), Some(Instant::now()));
                        let _ = tx.send(ClipEvent::Text(text));
                    }
                    return;
                }
            }
            if let Some(png) = read_image_png() {
                let tok = format!("i:{}", nwc_core::util::sha256_hex(&png));
                if !self.token_hit(&tok) {
                    *self.token.lock().unwrap() = (tok, Some(Instant::now()));
                    let _ = tx.send(ClipEvent::Image(png));
                }
            }
        }
    }
}

// ---- 底层访问（NSPasteboard 可在后台线程使用） ----

unsafe fn general() -> Retained<NSPasteboard> {
    NSPasteboard::generalPasteboard()
}

unsafe fn current_change_count() -> u64 {
    general().changeCount() as u64
}

unsafe fn read_text() -> Option<String> {
    let pb = general();
    let types = pb.types()?;
    let has_text = types.iter().any(|t| {
        let name = t.to_string();
        name.contains("utf8") || name.contains("utxt") || name.contains("NSStringPboardType")
    });
    if !has_text {
        return None;
    }
    let t = NSString::from_str("public.utf8-plain-text");
    let s = pb.stringForType(&t)?;
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

unsafe fn read_files() -> Option<Vec<PathBuf>> {
    let pb = general();
    let types = pb.types()?;
    let has_files = types.iter().any(|t| {
        let n = t.to_string();
        n.contains("file-url") || n.contains("NSFilenames")
    });
    if !has_files {
        return None;
    }
    // NSFilenamesPboardType 属性列表（对应 Node 版的三级回退首选）
    if let Some(list) = pb.propertyListForType(&filenames_type()) {
        if let Ok(arr) = Retained::downcast::<NSArray>(list) {
            let mut paths: Vec<PathBuf> = Vec::new();
            for obj in arr.iter() {
                if let Ok(ns) = Retained::downcast::<NSString>(obj.clone()) {
                    paths.push(PathBuf::from(ns.to_string()));
                }
            }
            if !paths.is_empty() {
                return Some(paths);
            }
        }
    }
    None
}

unsafe fn filenames_type() -> Retained<NSString> {
    NSString::from_str("NSFilenamesPboardType")
}

unsafe fn read_image_png() -> Option<Vec<u8>> {
    let pb = general();
    let types = pb.types()?;
    let owned: Vec<Retained<NSString>> = types.to_vec();
    let find = |frag: &str| -> Option<Retained<NSString>> {
        owned
            .iter()
            .find(|t| t.to_string().to_lowercase().contains(frag))
            .cloned()
    };
    // PNGf 直接拿原始字节
    if let Some(t) = find("pngf").or_else(|| find("public.png")) {
        if let Some(data) = pb.dataForType(&t) {
            let bytes = data.to_vec();
            if nwc_core::util::is_png(&bytes) {
                return Some(bytes);
            }
        }
    }
    // TIFF/JPEG → ImageIO 转 PNG
    if let Some(t) = find("tiff").or_else(|| find("jpeg")).or_else(|| find("public.jpeg")) {
        if let Some(data) = pb.dataForType(&t) {
            return convert_to_png(&data);
        }
    }
    None
}

/// ImageIO：CGImageSource → CGImageDestination(PNG)
unsafe fn convert_to_png(data: &Retained<NSData>) -> Option<Vec<u8>> {
    use objc2::rc::Retained;
    #[allow(non_snake_case)]
    #[repr(C)]
    struct CFDictionary(std::ffi::c_void);
    unsafe extern "C" {
        fn CGImageSourceCreateWithData(data: *const std::ffi::c_void, opts: *const std::ffi::c_void) -> *mut std::ffi::c_void;
        fn CGImageDestinationCreateWithData(dst: *const std::ffi::c_void, uti: *const std::ffi::c_void, count: usize, opts: *const std::ffi::c_void) -> *mut std::ffi::c_void;
        fn CGImageDestinationAddImageFromSource(dst: *mut std::ffi::c_void, src: *mut std::ffi::c_void, index: usize, opts: *const std::ffi::c_void) -> Bool;
        fn CGImageDestinationFinalize(dst: *mut std::ffi::c_void) -> Bool;
        fn CFDataCreateMutable(alloc: *const std::ffi::c_void, capacity: usize) -> *mut std::ffi::c_void;
        fn CFDataGetBytePtr(data: *const std::ffi::c_void) -> *const u8;
        fn CFDataGetLength(data: *const std::ffi::c_void) -> usize;
        fn CFRelease(cf: *const std::ffi::c_void);
        fn UTTypeCreatePreferredIdentifierForTag(tag: *const std::ffi::c_void, str_: *const std::ffi::c_void, bundle: *const std::ffi::c_void) -> *mut std::ffi::c_void;
    }
    let src = CGImageSourceCreateWithData(Retained::as_ptr(data) as *const std::ffi::c_void, std::ptr::null());
    if src.is_null() {
        return None;
    }
    let mut_data = CFDataCreateMutable(std::ptr::null(), 0);
    if mut_data.is_null() {
        CFRelease(src);
        return None;
    }
    // "public.png" UTI 常量字符串（UTF-16 LE C-string）
    let png_uti: Vec<u16> = "public.png".encode_utf16().chain(std::iter::once(0)).collect();
    let dst = CGImageDestinationCreateWithData(mut_data, png_uti.as_ptr() as *const _, 1, std::ptr::null());
    if dst.is_null() {
        CFRelease(mut_data);
        CFRelease(src);
        return None;
    }
    if !CGImageDestinationAddImageFromSource(dst, src, 0, std::ptr::null()).is_true() {
        CFRelease(dst);
        CFRelease(mut_data);
        CFRelease(src);
        return None;
    }
    let ok = CGImageDestinationFinalize(dst).is_true();
    CFRelease(dst);
    CFRelease(src);
    if !ok {
        CFRelease(mut_data);
        return None;
    }
    let len = CFDataGetLength(mut_data);
    let ptr = CFDataGetBytePtr(mut_data);
    let out = std::slice::from_raw_parts(ptr, len).to_vec();
    CFRelease(mut_data);
    let _: Option<CFDictionary> = None;
    let _ = Retained::<NSString>::retain; // 引用占位
    Some(out)
}

unsafe fn write_text(s: &str) -> bool {
    let pb = general();
    let ns = NSString::from_str("public.utf8-plain-text");
    pb.clearContents();
    pb.setString_forType(&NSString::from_str(s), &ns)
}

unsafe fn write_image_png(png: &[u8]) -> bool {
    if !nwc_core::util::is_png(png) {
        return false;
    }
    let pb = general();
    let data = NSData::with_bytes(png);
    pb.clearContents();
    pb.setData_forType(Some(&data), &NSString::from_str("public.png"))
}

unsafe fn write_files(paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return false;
    }
    let urls: Vec<Retained<NSURL>> = paths
        .iter()
        .map(|p| NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy())))
        .collect();
    let objs: Vec<Retained<ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting>>> =
        urls.iter().map(|u| ProtocolObject::from_retained(u.clone())).collect();
    let arr = NSArray::from_retained_slice(&objs);
    let pb = general();
    pb.clearContents();
    pb.writeObjects(&arr)
}
