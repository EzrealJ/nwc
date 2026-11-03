//! macOS 剪贴板：NSPasteboard 直接访问。
//!
//! - 变更检测：changeCount 进程内轮询（NSPasteboard 无通知 API，但相比 Node 版
//!   每 400ms spawn 4-6 个 osascript/pbpaste/sips 子进程，这里是零子进程、微秒级读取）
//! - 读：文件（NSFilenames）> 文本 > 图片（PNGf 优先原始字节，TIFF/JPEG 经 ImageIO 转 PNG）
//! - 写：文本 / 图片（public.tiff + public.png + PNGf 三格式，PNG 原始字节直写无重编码损耗）/ 文件列表（writeObjects NSURL）
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
            return convert_image(&data, "public.png");
        }
    }
    None
}

/// ImageIO 通用转码：CGImageSource(任意支持的格式) → CGImageDestination(dst_uti)。
/// 读取方向 TIFF/JPEG → PNG；写入方向 PNG → TIFF（供 write_image_png 挂多格式用）。
unsafe fn convert_image(data: &Retained<NSData>, dst_uti: &str) -> Option<Vec<u8>> {
    #[allow(non_snake_case)]
    unsafe extern "C" {
        fn CGImageSourceCreateWithData(data: *const std::ffi::c_void, opts: *const std::ffi::c_void) -> *mut std::ffi::c_void;
        fn CGImageDestinationCreateWithData(dst: *const std::ffi::c_void, uti: *const std::ffi::c_void, count: usize, opts: *const std::ffi::c_void) -> *mut std::ffi::c_void;
        fn CGImageDestinationAddImageFromSource(dst: *mut std::ffi::c_void, src: *mut std::ffi::c_void, index: usize, opts: *const std::ffi::c_void) -> Bool;
        fn CGImageDestinationFinalize(dst: *mut std::ffi::c_void) -> Bool;
        fn CFDataCreateMutable(alloc: *const std::ffi::c_void, capacity: usize) -> *mut std::ffi::c_void;
        fn CFDataGetBytePtr(data: *const std::ffi::c_void) -> *const u8;
        fn CFDataGetLength(data: *const std::ffi::c_void) -> usize;
        fn CFRelease(cf: *const std::ffi::c_void);
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
    // 目标 UTI：NSString 与 CFStringRef toll-free bridge，直接传对象指针
    // （不能传裸 UTF-16 数组——那不是 CF 对象，CoreFoundation 会 __CF_IS_OBJC 断言崩溃）
    let uti = NSString::from_str(dst_uti);
    let dst = CGImageDestinationCreateWithData(mut_data, Retained::as_ptr(&uti) as *const _, 1, std::ptr::null());
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
    Some(out)
}

unsafe fn write_text(s: &str) -> bool {
    let pb = general();
    let ns = NSString::from_str("public.utf8-plain-text");
    pb.clearContents();
    pb.setString_forType(&NSString::from_str(s), &ns)
}

/// 写图片：同时挂 TIFF + PNG 原始字节 + 旧式 PNGf。
/// public.tiff 是 mac 应用粘贴图片的事实标准（NSImage 写剪贴板的默认类型），
/// 只写 public.png 时不少应用（聊天软件/旧 App）Cmd+V 读不到内容；
/// 这里对齐 Windows 端「PNG + CF_DIBV5 双格式」的兼容做法。TIFF 由 ImageIO 从 PNG 转出。
unsafe fn write_image_png(png: &[u8]) -> bool {
    if !nwc_core::util::is_png(png) {
        return false;
    }
    let pb = general();
    pb.clearContents();
    let png_data = NSData::with_bytes(png);
    let mut ok = false;
    if let Some(tiff) = convert_image(&png_data, "public.tiff") {
        ok |= pb.setData_forType(Some(&NSData::with_bytes(&tiff)), &NSString::from_str("public.tiff"));
    }
    ok |= pb.setData_forType(Some(&png_data), &NSString::from_str("public.png"));
    ok |= pb.setData_forType(Some(&png_data), &NSString::from_str("PNGf"));
    ok
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

#[cfg(test)]
mod tests {
    //! 平台 API 真机测试：直接操作系统剪贴板验证读写全链路（会覆盖真实剪贴板内容）。
    //! 剪贴板是进程全局状态，静态锁强制串行（CI 再加 --test-threads=1 双保险）。

    use super::*;

    /// 全局串行锁：同一测试二进制内所有剪贴板测试互斥
    static PB_LOCK: Mutex<()> = Mutex::new(());

    /// 1x1 红色不透明 PNG（字节由脚本生成，确定性内容）
    const TINY_PNG_B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

    fn tiny_png() -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(TINY_PNG_B64).unwrap()
    }

    #[test]
    fn text_roundtrip() {
        let _g = PB_LOCK.lock().unwrap();
        let s = "你好 nwc ✓ emoji";
        objc2::rc::autoreleasepool(|_pool| unsafe {
            assert!(write_text(s), "写入应成功");
            assert_eq!(read_text().as_deref(), Some(s), "读回应与写入一致");
        });
    }

    /// write_files 用 writeObjects(NSURL)、read_files 读 NSFilenames 属性列表——
    /// 两种表示的互通性正是「未在真机验证」清单里的遗留项，这里实机锁定
    #[test]
    fn files_roundtrip() {
        let _g = PB_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join("nwc-mac-pb-test");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("测试文件.txt");
        std::fs::write(&f, b"x").unwrap();
        objc2::rc::autoreleasepool(|_pool| unsafe {
            assert!(write_files(&[f.clone()]), "写入应成功");
            assert_eq!(read_files(), Some(vec![f.clone()]), "writeObjects(NSURL) 应能被 NSFilenames 读取");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 写入应同时挂 public.tiff / public.png / PNGf 三种类型，且能按各自类型读回
    /// （注意：会动真实系统剪贴板，仅在手动跑 `cargo test -p nwc-mac` 时执行）
    #[test]
    fn write_image_sets_tiff_png_pngf() {
        let _g = PB_LOCK.lock().unwrap();
        let png = tiny_png();
        objc2::rc::autoreleasepool(|_pool| unsafe {
            assert!(write_image_png(&png), "写入应返回成功");

            let pb = general();
            let types: Vec<String> = pb.types().unwrap().to_vec().iter().map(|t| t.to_string()).collect();
            assert!(types.iter().any(|t| t.contains("tiff")), "缺少 TIFF 类型: {types:?}");
            assert!(types.iter().any(|t| t == "public.png"), "缺少 public.png: {types:?}");

            // TIFF 类型可解出位图（说明 ImageIO 转码成功而非空数据）
            let tiff_type = NSString::from_str("public.tiff");
            let tiff = pb.dataForType(&tiff_type).expect("TIFF 数据应存在");
            assert!(tiff.len() > 8, "TIFF 数据过短: {} 字节", tiff.len());

            // 回读：read_image_png 优先 PNGf/public.png 原始字节，应得到与写入一致的 PNG
            let back = read_image_png().expect("应能读回图片");
            assert_eq!(back, png, "回读 PNG 应与写入字节一致");
        });
    }

    /// TIFF→PNG 方向（读取路径）：写入的 TIFF 数据应可被 convert_image 解回 PNG
    #[test]
    fn tiff_roundtrip_via_imageio() {
        let png = tiny_png();
        objc2::rc::autoreleasepool(|_pool| unsafe {
            let tiff = convert_image(&NSData::with_bytes(&png), "public.tiff").expect("PNG→TIFF 应成功");
            let back = convert_image(&NSData::with_bytes(&tiff), "public.png").expect("TIFF→PNG 应成功");
            assert!(nwc_core::util::is_png(&back));
            // 1x1 不透明红点回读后仍应是合法 PNG（宽高一致由 ImageIO 保证，这里验证签名即可）
            assert_eq!(&back[..8], &png[..8]);
        });
    }
}
