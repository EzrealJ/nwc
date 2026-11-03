//! 菜单栏（NSStatusItem）：状态图标 + 菜单（历史/暂停/收件箱/日志/配置/自启/退出）。
//! 引擎事件由后台线程消费：状态写共享状态、日志落文件（nwc-core FileLogger）、通知走系统通知中心。

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{define_class, msg_send, AllocAnyThread, ClassType, DefinedClass, MainThreadMarker};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventModifierFlags, NSImage, NSMenu,
    NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSString, NSTimer, NSRunLoop, NSDefaultRunLoopMode};

use nwc_core::sync::{EngineCmd, EngineEvent, Status};

use crate::autostart;

#[derive(Default)]
pub struct UiState {
    pub status: String,
    pub paused: bool,
    /// SF Symbol 名（状态图标），随 Status 事件更新
    pub symbol: String,
}

struct Ivars {
    cmd_tx: Sender<EngineCmd>,
    inbox: PathBuf,
    cfg_path: PathBuf,
    log_path: PathBuf,
    state_dir: PathBuf,
    state: Arc<Mutex<UiState>>,
    menu: Mutex<Option<Retained<NSMenu>>>,
    item: Mutex<Option<Retained<NSStatusItem>>>,
}

define_class! {
    #[unsafe(super(NSObject))]
    #[name = "NwcMenuController"]
    #[ivars = Ivars]
    struct NwcMenuController;

    #[allow(non_snake_case)]
    impl NwcMenuController {
        #[unsafe(method(tick:))]
        unsafe fn tick(&self, _timer: *mut AnyObject) {
            let Some(mtm) = MainThreadMarker::new() else { return };
            self.refresh(mtm);
        }

        #[unsafe(method(togglePause:))]
        unsafe fn toggle_pause(&self, _sender: *mut AnyObject) {
            let paused = self.ivars().state.lock().unwrap().paused;
            let _ = self.ivars().cmd_tx.send(if paused { EngineCmd::Resume } else { EngineCmd::Pause });
        }

        /// 历史条目点击：普通点击 = 重新粘贴并同步对端；按住 Option 点击 = 删除该条
        #[unsafe(method(pasteHistory:))]
        unsafe fn paste_history(&self, sender: *mut AnyObject) {
            let Some(text) = represented_text(sender) else { return };
            // +[NSEvent modifierFlags]：此刻的全局修饰键状态（类方法，objc2 生成代码只暴露了实例版）
            let flags: NSEventModifierFlags = msg_send![NSEvent::class(), modifierFlags];
            if flags.contains(NSEventModifierFlags::Option) {
                let _ = self.ivars().cmd_tx.send(EngineCmd::RemoveHistory(text));
            } else {
                let _ = self.ivars().cmd_tx.send(EngineCmd::PasteText(text));
            }
        }

        #[unsafe(method(clearHistory:))]
        unsafe fn clear_history(&self, _sender: *mut AnyObject) {
            let _ = self.ivars().cmd_tx.send(EngineCmd::ClearHistory);
        }

        #[unsafe(method(openInbox:))]
        unsafe fn open_inbox(&self, _sender: *mut AnyObject) {
            let dir = self.ivars().inbox.clone();
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::process::Command::new("open").arg(&dir).spawn();
        }

        #[unsafe(method(openLogs:))]
        unsafe fn open_logs(&self, _sender: *mut AnyObject) {
            let p = self.ivars().log_path.clone();
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            if !p.exists() {
                let _ = std::fs::write(&p, "");
            }
            let _ = std::process::Command::new("open").arg("-t").arg(&p).spawn();
        }

        #[unsafe(method(editConfig:))]
        unsafe fn edit_config(&self, _sender: *mut AnyObject) {
            let p = self.ivars().cfg_path.clone();
            if !p.exists() {
                if let Some(d) = p.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                let _ = std::fs::write(&p, "{\n  \"token\": \"\",\n  \"port\": 8767\n}\n");
            }
            let _ = std::process::Command::new("open").arg("-t").arg(&p).spawn();
        }

        #[unsafe(method(toggleAutostart:))]
        unsafe fn toggle_autostart(&self, _sender: *mut AnyObject) {
            let _ = autostart::set_autostart(!autostart::autostart_enabled());
        }

        #[unsafe(method(quitApp:))]
        unsafe fn quit_app(&self, _sender: *mut AnyObject) {
            let _ = self.ivars().cmd_tx.send(EngineCmd::Shutdown);
            std::thread::sleep(std::time::Duration::from_millis(300));
            std::process::exit(0);
        }
    }
}

impl NwcMenuController {
    fn refresh(&self, mtm: MainThreadMarker) {
        unsafe {
            let (status, paused, symbol) = {
                let s = self.ivars().state.lock().unwrap();
                (s.status.clone(), s.paused, s.symbol.clone())
            };
            if let Some(item) = self.ivars().item.lock().unwrap().as_ref() {
                if let Some(button) = item.button(mtm) {
                    // SF Symbol 图标 + 悬停 tooltip（替代原先占一长条菜单栏的文字标题）
                    if !symbol.is_empty() {
                        if let Some(img) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                            &NSString::from_str(&symbol),
                            None,
                        ) {
                            button.setImage(Some(&img));
                        }
                    }
                    button.setToolTip(Some(&NSString::from_str(&format!("NetWorkClipboard：{status}"))));
                    button.setTitle(&NSString::from_str(""));
                }
            }
            if let Some(menu) = self.ivars().menu.lock().unwrap().as_ref() {
                rebuild_menu(menu, paused, self, mtm);
            }
        }
    }
}

/// 菜单项 representedObject 里存的完整历史文本
unsafe fn represented_text(sender: *mut AnyObject) -> Option<String> {
    if sender.is_null() {
        return None;
    }
    let obj: &AnyObject = unsafe { &*sender };
    let s: Retained<NSString> = unsafe { msg_send![obj, representedObject] };
    Some(s.to_string())
}

unsafe fn rebuild_menu(menu: &NSMenu, paused: bool, ctrl: &NwcMenuController, mtm: MainThreadMarker) {
    menu.removeAllItems();
    let items: Vec<Retained<NSMenuItem>> = vec![
        disabled_item("NetWorkClipboard", mtm),
        action_item(
            if paused { "恢复同步" } else { "暂停同步" },
            Sel::register(c"togglePause:"),
            ctrl,
            mtm,
        ),
    ];
    for it in items {
        menu.addItem(&it);
    }
    menu.addItem(&separator(mtm));
    append_history_submenu(menu, ctrl, mtm);
    menu.addItem(&separator(mtm));
    for it in [
        action_item("打开收件箱", Sel::register(c"openInbox:"), ctrl, mtm),
        action_item("打开日志", Sel::register(c"openLogs:"), ctrl, mtm),
        action_item("编辑配置", Sel::register(c"editConfig:"), ctrl, mtm),
        action_item(
            &format!("开机自启{}", if autostart::autostart_enabled() { " ✓" } else { "" }),
            Sel::register(c"toggleAutostart:"),
            ctrl,
            mtm,
        ),
        action_item("退出", Sel::register(c"quitApp:"), ctrl, mtm),
    ] {
        menu.addItem(&it);
    }
}

/// 历史记录子菜单：点条目 = 粘贴并同步；按住 Option 点 = 删除（对齐 Windows 历史窗口能力）
unsafe fn append_history_submenu(menu: &NSMenu, ctrl: &NwcMenuController, mtm: MainThreadMarker) {
    let entries = nwc_core::history::load(&ctrl.ivars().state_dir);
    let sub = NSMenu::new(mtm);
    if entries.is_empty() {
        let it = disabled_item("（暂无记录）", mtm);
        sub.addItem(&it);
    } else {
        for e in &entries {
            let one_line: String = e.text.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
            let title: String = one_line.chars().take(60).collect();
            let it = NSMenuItem::new(mtm);
            it.setTitle(&NSString::from_str(&format!(
                "{}{}",
                if e.peer { "⇄ " } else { "" },
                if one_line.chars().count() > 60 { format!("{title}…") } else { title }
            )));
            it.setAction(Some(Sel::register(c"pasteHistory:")));
            it.setTarget(Some(as_any(ctrl)));
            it.setRepresentedObject(Some(&NSString::from_str(&e.text)));
            sub.addItem(&it);
        }
        sub.addItem(&separator(mtm));
        let hint = disabled_item("按住 Option 点击可删除该条", mtm);
        sub.addItem(&hint);
        let clear = action_item("清空全部", Sel::register(c"clearHistory:"), ctrl, mtm);
        sub.addItem(&clear);
    }
    let root = NSMenuItem::new(mtm);
    root.setTitle(&NSString::from_str(format!("历史记录（{}）", entries.len()).as_str()));
    root.setSubmenu(Some(&sub));
    menu.addItem(&root);
}

fn as_any<T: objc2::ClassType>(r: &T) -> &AnyObject {
    unsafe { std::mem::transmute(r) }
}

unsafe fn separator(mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    NSMenuItem::separatorItem(mtm)
}

unsafe fn disabled_item(title: &str, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let it = NSMenuItem::new(mtm);
    it.setTitle(&NSString::from_str(title));
    it.setEnabled(false);
    it
}

unsafe fn action_item(title: &str, action: Sel, ctrl: &NwcMenuController, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let it = NSMenuItem::new(mtm);
    it.setTitle(&NSString::from_str(title));
    it.setAction(Some(action));
    it.setTarget(Some(as_any(ctrl)));
    it
}

/// 系统通知（通知中心）。nwc 无 bundle 签名，UNUserNotification 走不通，
/// 用 osascript display notification（仅通知时偶发一次子进程，常驻路径仍零子进程）
fn show_notification(title: &str, body: &str) {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("display notification \"{}\" with title \"{}\"", esc(body), esc(title));
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .spawn();
}

pub fn run(
    cmd_tx: Sender<EngineCmd>,
    evt_rx: Receiver<EngineEvent>,
    inbox: PathBuf,
    cfg_path: PathBuf,
    logger: nwc_core::logger::FileLogger,
    log_path: PathBuf,
) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        let Some(mtm) = MainThreadMarker::new() else {
            eprintln!("nwc-mac: 无法获取主线程");
            return;
        };
        let state = Arc::new(Mutex::new(UiState { status: "启动中…".into(), paused: false, symbol: String::new() }));

        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        let state_dir = cfg_path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        let ctrl: Retained<NwcMenuController> = {
            let mut alloc = NwcMenuController::alloc();
            let alloc = alloc.set_ivars(Ivars {
                cmd_tx,
                inbox,
                cfg_path,
                log_path,
                state_dir,
                state: Arc::clone(&state),
                menu: Mutex::new(None),
                item: Mutex::new(None),
            });
            unsafe { msg_send![super(alloc), init] }
        };

        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        let button = item.button(mtm).unwrap();
        button.setTitle(&NSString::from_str("NWC…"));
        let menu = NSMenu::new(mtm);
        rebuild_menu(&menu, false, &ctrl, mtm);
        item.setMenu(Some(&menu));
        *ctrl.ivars().menu.lock().unwrap() = Some(menu);
        *ctrl.ivars().item.lock().unwrap() = Some(item);

        // 引擎事件：状态 → 共享状态；日志/警告 → 文件；通知 → 系统通知中心 + 文件
        std::thread::Builder::new()
            .name("nwc-evt-pump".into())
            .spawn(move || {
                while let Ok(ev) = evt_rx.recv() {
                    let ts = nwc_core::timestamp::now_hms();
                    match ev {
                        EngineEvent::Log(m) => logger.write(&format!("[{ts}] {m}")),
                        EngineEvent::Warn(m) => logger.write(&format!("[{ts}] [警告] {m}")),
                        EngineEvent::Status(s) => {
                            {
                                let mut st = state.lock().unwrap();
                                st.status = label_of(&s);
                                st.symbol = symbol_of(&s).to_string();
                                if s == Status::Paused {
                                    st.paused = true;
                                } else if !matches!(s, Status::Stopped) {
                                    st.paused = false;
                                }
                            }
                            logger.write(&format!("[{ts}] [状态] {}", label_of(&s)));
                        }
                        EngineEvent::Notify(t, b) => {
                            logger.write(&format!("[{ts}] [通知] {t}: {b}"));
                            show_notification(&t, &b);
                        }
                    }
                }
            })
            .ok();

        // 定时刷新（300ms）
        let timer = NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
            0.3,
            as_any(&*ctrl),
            Sel::register(c"tick:"),
            None,
            true,
        );
        NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSDefaultRunLoopMode);

        app.run();
    }
}

fn label_of(s: &Status) -> String {
    match s {
        Status::Starting => "启动中".into(),
        Status::Scanning => "扫描中".into(),
        Status::Connecting(ip) => format!("连接中 {ip}"),
        Status::Connected(n) => format!("已连接 {n}"),
        Status::Listening => "监听中".into(),
        Status::Paused => "已暂停".into(),
        Status::Stopped => "已停止".into(),
    }
}

/// 状态 → SF Symbol 图标名（macOS 11+；菜单栏模板图标，自动适配深浅色）
fn symbol_of(s: &Status) -> &'static str {
    match s {
        Status::Starting => "hourglass",
        Status::Scanning | Status::Connecting(_) => "dot.radiowaves.left.and.right",
        Status::Connected(_) => "doc.on.doc",
        Status::Listening => "antenna.radiowaves.left.and.right",
        Status::Paused => "pause.circle",
        Status::Stopped => "exclamationmark.circle",
    }
}
