//! 设置窗口：NSWindow + 原生控件（对齐 Windows 端 dialog.rs 的功能面，
//! 额外覆盖历史开关/条数与收件箱清理天数）。
//!
//! - frame 布局（无 AutoLayout 依赖）；label / 编辑框 / 下拉 / 复选框 / 按钮
//! - 保存：校验 → 合并写 config.json（保留 UI 之外的键，如 scanCidr/ifaces）→ 重启生效
//! - 重启：释放单实例锁 → 拉起新实例 → 退出（见 main.rs 的 restart_process）

#![cfg(target_os = "macos")]

use std::path::PathBuf;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{define_class, msg_send, AllocAnyThread, ClassType, DefinedClass, MainThreadMarker};
use objc2_app_kit::{
    NSBackingStoreType, NSOffState, NSOnState, NSPopUpButton, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

/// objc2-foundation 未导出 NSMakeRect，手工构造
fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect { origin: NSPoint::new(x, y), size: NSSize::new(w, h) }
}

/// 窗口客户区尺寸与网格
const CW: f64 = 440.0;
const PAD: f64 = 20.0;
const LABEL_H: f64 = 16.0;
const EDIT_H: f64 = 24.0;
const ROW_GAP: f64 = 6.0;
const BLOCK_GAP: f64 = 14.0;

struct Ivars {
    cfg_path: PathBuf,
    window: Retained<NSWindow>,
    token: Retained<objc2_app_kit::NSTextField>,
    port: Retained<objc2_app_kit::NSTextField>,
    name: Retained<objc2_app_kit::NSTextField>,
    role: Retained<NSPopUpButton>,
    inbox: Retained<objc2_app_kit::NSTextField>,
    history_on: Retained<objc2_app_kit::NSButton>,
    history_limit: Retained<objc2_app_kit::NSTextField>,
    inbox_days: Retained<objc2_app_kit::NSTextField>,
}

define_class! {
    #[unsafe(super(objc2::runtime::NSObject))]
    #[name = "NwcSettingsController"]
    #[ivars = Ivars]
    struct NwcSettingsController;

    #[allow(non_snake_case)]
    impl NwcSettingsController {
        #[unsafe(method(saveSettings:))]
        unsafe fn save(&self, _sender: *mut AnyObject) {
            let iv = self.ivars();
            let token = iv.token.stringValue().to_string();
            let name = iv.name.stringValue().to_string().trim().to_string();
            let inbox = iv.inbox.stringValue().to_string().trim().to_string();
            let port_raw = iv.port.stringValue().to_string();
            let port: u64 = match port_raw.trim().parse() {
                Ok(p) if (1..=65535).contains(&p) => p,
                _ => {
                    alert(&format!("端口无效：\"{port_raw}\"（应为 1-65535 的数字）"));
                    return;
                }
            };
            let role = iv.role.titleOfSelectedItem().map(|s| s.to_string()).unwrap_or_else(|| "auto".into());
            if !matches!(role.as_str(), "auto" | "server" | "client") {
                alert(&format!("角色无效：\"{role}\""));
                return;
            }
            if name.is_empty() {
                alert("设备名不能为空");
                return;
            }
            let history_limit: u64 = match iv.history_limit.stringValue().to_string().trim().parse() {
                Ok(n) if (1..=100).contains(&n) => n,
                _ => {
                    alert("历史条数应为 1-100 的数字");
                    return;
                }
            };
            let inbox_days: u64 = match iv.inbox_days.stringValue().to_string().trim().parse() {
                Ok(n) if n <= 3650 => n,
                _ => {
                    alert("收件箱保留天数应为 0-3650（0 = 不自动清理）");
                    return;
                }
            };
            let history_on = iv.history_on.state() == NSOnState;

            if let Err(e) = save_merged(&iv.cfg_path, token, port, name, role, inbox, history_on, history_limit, inbox_days) {
                alert(&format!("写入失败：{e}"));
                return;
            }
            iv.window.close();
            crate::restart_process();
        }

        #[unsafe(method(cancelSettings:))]
        unsafe fn cancel(&self, _sender: *mut AnyObject) {
            self.ivars().window.close();
            // 控制器随窗口关闭退役
            CONTROLLER.with(|c| *c.borrow_mut() = None);
        }
    }
}

thread_local! {
    /// 已打开的设置控制器（防重复开窗 + 维持生命周期；AppKit 对象仅主线程）
    static CONTROLLER: std::cell::RefCell<Option<Retained<NwcSettingsController>>> =
        const { std::cell::RefCell::new(None) };
}

/// 打开设置窗口（须在主线程调用）
pub fn open_settings(mtm: MainThreadMarker, cfg_path: PathBuf) {
    let already_open = CONTROLLER.with(|c| {
        if let Some(ctrl) = c.borrow().as_ref() {
            unsafe { ctrl.ivars().window.makeKeyAndOrderFront(None) };
            true
        } else {
            false
        }
    });
    if already_open {
        return;
    }

    unsafe {
        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
        let window: Retained<NSWindow> = unsafe {
            msg_send![
                mtm.alloc::<NSWindow>(),
                initWithContentRect: rect(0.0, 0.0, CW, 470.0),
                styleMask: style,
                backing: NSBackingStoreType::Buffered,
                defer: false,
            ]
        };
        window.setTitle(&NSString::from_str("NetWorkClipboard 设置"));
        window.setShowsResizeIndicator(false);

        let view = window.contentView().unwrap();

        // 当前值：默认 < config.json
        let cur = read_current(&cfg_path);

        let mut y = PAD;
        let full = CW - PAD * 2.0;
        let half = (full - 16.0) / 2.0;
        let x2 = PAD + half + 16.0;

        let mut place = |v: &AnyObject, x: f64, yy: f64, w: f64, h: f64| {
            let _: () = msg_send![v, setFrame: rect(x, yy, w, h)];
            let _: () = msg_send![&*view, addSubview: v];
        };

        // ── 共享口令 ──
        let l = label(mtm, "共享口令（两端必须一致）");
        place(as_obj(&*l), PAD, y + 4.0, full, LABEL_H);
        y += LABEL_H + ROW_GAP;
        let token = edit(mtm, &cur.token);
        place(as_obj(&*token), PAD, y, full, EDIT_H);
        y += EDIT_H + BLOCK_GAP;

        // ── 端口 / 设备名 ──
        let lp = label(mtm, "端口");
        place(as_obj(&*lp), PAD, y + 4.0, half, LABEL_H);
        let ln = label(mtm, "设备名");
        place(as_obj(&*ln), x2, y + 4.0, half, LABEL_H);
        y += LABEL_H + ROW_GAP;
        let port = edit(mtm, &cur.port);
        place(as_obj(&*port), PAD, y, 100.0, EDIT_H);
        let name = edit(mtm, &cur.name);
        place(as_obj(&*name), x2, y, half, EDIT_H);
        y += EDIT_H + BLOCK_GAP;

        // ── 角色 ──
        let lr = label(mtm, "角色（自动抉择 / 只监听 / 只连接）");
        place(as_obj(&*lr), PAD, y + 4.0, full, LABEL_H);
        y += LABEL_H + ROW_GAP;
        let role: Retained<NSPopUpButton> = msg_send![
            mtm.alloc::<NSPopUpButton>(),
            initWithFrame: rect(0.0, 0.0, 160.0, EDIT_H),
            pullsDown: false,
        ];
        role.addItemsWithTitles(&objc2_foundation::NSArray::from_retained_slice(&[
            NSString::from_str("auto"),
            NSString::from_str("server"),
            NSString::from_str("client"),
        ]));
        let idx = ["auto", "server", "client"]
            .iter()
            .position(|r| *r == cur.role)
            .unwrap_or(0);
        role.selectItemAtIndex(idx as isize);
        place(as_obj(&*role), PAD, y, 160.0, EDIT_H);
        y += EDIT_H + BLOCK_GAP;

        // ── 收件箱目录 ──
        let li = label(mtm, "收件箱目录");
        place(as_obj(&*li), PAD, y + 4.0, full, LABEL_H);
        y += LABEL_H + ROW_GAP;
        let inbox = edit(mtm, &cur.inbox);
        place(as_obj(&*inbox), PAD, y, full, EDIT_H);
        y += EDIT_H + BLOCK_GAP;

        // ── 历史与清理 ──
        let history_on = objc2_app_kit::NSButton::checkboxWithTitle_target_action(
            &NSString::from_str("启用文字剪贴板历史"),
            None,
            None,
            mtm,
        );
        history_on.setState(if cur.history_on {
            NSOnState
        } else {
            NSOffState
        });
        place(as_obj(&*history_on), PAD, y, half + 40.0, EDIT_H);
        y += EDIT_H + ROW_GAP;

        let ll = label(mtm, "历史条数（1-100）");
        place(as_obj(&*ll), PAD, y + 4.0, half, LABEL_H);
        let ld = label(mtm, "收件箱保留天数（0 = 不清理）");
        place(as_obj(&*ld), x2, y + 4.0, full, LABEL_H);
        y += LABEL_H + ROW_GAP;
        let history_limit = edit(mtm, &cur.history_limit);
        place(as_obj(&*history_limit), PAD, y, 100.0, EDIT_H);
        let inbox_days = edit(mtm, &cur.inbox_days);
        place(as_obj(&*inbox_days), x2, y, 100.0, EDIT_H);
        y += EDIT_H + BLOCK_GAP + 6.0;

        // ── 按钮 ──
        let ctrl: Retained<NwcSettingsController> = {
            let mut alloc = NwcSettingsController::alloc();
            let alloc = alloc.set_ivars(Ivars {
                cfg_path,
                window: window.clone(),
                token,
                port,
                name,
                role,
                inbox,
                history_on,
                history_limit,
                inbox_days,
            });
            msg_send![super(alloc), init]
        };
        let save_btn = objc2_app_kit::NSButton::buttonWithTitle_target_action(
            &NSString::from_str("保存并重启"),
            Some(as_obj(&*ctrl)),
            Some(Sel::register(c"saveSettings:")),
            mtm,
        );
        place(as_obj(&*save_btn), PAD, y, 112.0, 30.0);
        let cancel_btn = objc2_app_kit::NSButton::buttonWithTitle_target_action(
            &NSString::from_str("取消"),
            Some(as_obj(&*ctrl)),
            Some(Sel::register(c"cancelSettings:")),
            mtm,
        );
        place(as_obj(&*cancel_btn), PAD + 122.0, y, 76.0, 30.0);
        let hint = label(mtm, "保存后程序自动重启并按新配置运行");
        place(as_obj(&*hint), PAD + 215.0, y + 8.0, full, LABEL_H);

        window.center();
        window.makeKeyAndOrderFront(None);
        let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        app.activateIgnoringOtherApps(true);
        CONTROLLER.with(|c| *c.borrow_mut() = Some(ctrl));
    }
}

// ---- 控件构造 ----

unsafe fn as_obj<T: ClassType>(r: &T) -> &AnyObject {
    unsafe { std::mem::transmute(r) }
}

fn label(mtm: MainThreadMarker, text: &str) -> Retained<objc2_app_kit::NSTextField> {
    unsafe {
        objc2_app_kit::NSTextField::labelWithString(&NSString::from_str(text), mtm)
    }
}

fn edit(mtm: MainThreadMarker, text: &str) -> Retained<objc2_app_kit::NSTextField> {
    unsafe {
        let t = objc2_app_kit::NSTextField::new(mtm);
        t.setBezeled(true);
        t.setEditable(true);
        t.setStringValue(&NSString::from_str(text));
        t
    }
}

// ---- 读写 ----

struct CurCfg {
    token: String,
    port: String,
    name: String,
    role: String,
    inbox: String,
    history_on: bool,
    history_limit: String,
    inbox_days: String,
}

fn read_current(cfg_path: &std::path::Path) -> CurCfg {
    let d = nwc_core::config::Config::default();
    let mut cur = CurCfg {
        token: String::new(),
        port: d.port.to_string(),
        name: d.name.clone(),
        role: "auto".into(),
        inbox: d.inbox_dir.to_string_lossy().into_owned(),
        history_on: d.history_enabled,
        history_limit: d.history_limit.to_string(),
        inbox_days: d.inbox_keep_days.to_string(),
    };
    if let Ok(text) = std::fs::read_to_string(cfg_path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(s) = v["token"].as_str() { cur.token = s.into(); }
            if let Some(n) = v["port"].as_u64() { cur.port = n.to_string(); }
            if let Some(s) = v["name"].as_str() { cur.name = s.into(); }
            if let Some(s) = v["role"].as_str() { cur.role = s.into(); }
            if let Some(s) = v["inboxDir"].as_str() { cur.inbox = s.into(); }
            if let Some(b) = v["historyEnabled"].as_bool() { cur.history_on = b; }
            if let Some(n) = v["historyLimit"].as_u64() { cur.history_limit = n.to_string(); }
            if let Some(n) = v["inboxKeepDays"].as_u64() { cur.inbox_days = n.to_string(); }
        }
    }
    cur
}

/// 合并写：只覆盖 UI 管理的键，保留 config.json 中其它自定义键
fn save_merged(
    cfg_path: &std::path::Path,
    token: String,
    port: u64,
    name: String,
    role: String,
    inbox: String,
    history_on: bool,
    history_limit: u64,
    inbox_days: u64,
) -> Result<(), String> {
    if let Some(dir) = cfg_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut v: serde_json::Value = std::fs::read_to_string(cfg_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let obj = v.as_object_mut().ok_or("config.json 不是 JSON 对象")?;
    obj.insert("token".into(), serde_json::json!(token));
    obj.insert("port".into(), serde_json::json!(port));
    obj.insert("name".into(), serde_json::json!(name));
    obj.insert("role".into(), serde_json::json!(role));
    if inbox.is_empty() {
        obj.remove("inboxDir");
    } else {
        obj.insert("inboxDir".into(), serde_json::json!(inbox));
    }
    obj.insert("historyEnabled".into(), serde_json::json!(history_on));
    obj.insert("historyLimit".into(), serde_json::json!(history_limit));
    obj.insert("inboxKeepDays".into(), serde_json::json!(inbox_days));
    let text = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
    std::fs::write(cfg_path, text + "\n").map_err(|e| e.to_string())
}

fn alert(msg: &str) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    unsafe {
        let a = objc2_app_kit::NSAlert::new(mtm);
        a.setMessageText(&NSString::from_str(msg));
        a.runModal();
    }
}
