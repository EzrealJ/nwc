//! 菜单栏（NSStatusItem）：状态标题 + 菜单（暂停/收件箱/日志/配置/自启/退出）。
//! 引擎事件由后台线程写入共享状态，NSTimer 每 300ms 刷新标题与菜单。

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadMarker};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSString, NSTimer, NSRunLoop, NSDefaultRunLoopMode};

use nwc_core::sync::{EngineCmd, EngineEvent, Status};

use crate::autostart;

#[derive(Default)]
pub struct UiState {
    pub status: String,
    pub paused: bool,
    pub balloon: Option<(String, String)>,
}

struct Ivars {
    cmd_tx: Sender<EngineCmd>,
    inbox: PathBuf,
    cfg_path: PathBuf,
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

        #[unsafe(method(openInbox:))]
        unsafe fn open_inbox(&self, _sender: *mut AnyObject) {
            let dir = self.ivars().inbox.clone();
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::process::Command::new("open").arg(&dir).spawn();
        }

        #[unsafe(method(openLogs:))]
        unsafe fn open_logs(&self, _sender: *mut AnyObject) {
            let dir = nwc_core::config::home_dir().join(".networkclipboard").join("logs");
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::process::Command::new("open").arg(&dir).spawn();
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
            let (status, paused) = {
                let s = self.ivars().state.lock().unwrap();
                (s.status.clone(), s.paused)
            };
            if let Some(item) = self.ivars().item.lock().unwrap().as_ref() {
                if let Some(button) = item.button(mtm) {
                    button.setTitle(&NSString::from_str(&format!("NWC {status}")));
                }
            }
            if let Some(menu) = self.ivars().menu.lock().unwrap().as_ref() {
                rebuild_menu(menu, paused, self, mtm);
            }
        }
    }
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
    ];
    for it in items {
        menu.addItem(&it);
    }
}

fn as_any<T: objc2::ClassType>(r: &T) -> &AnyObject {
    unsafe { std::mem::transmute(r) }
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

pub fn run(
    cmd_tx: Sender<EngineCmd>,
    evt_rx: Receiver<EngineEvent>,
    inbox: PathBuf,
    cfg_path: PathBuf,
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
        let state = Arc::new(Mutex::new(UiState { status: "启动中…".into(), paused: false, balloon: None }));

        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        let ctrl: Retained<NwcMenuController> = {
            let mut alloc = NwcMenuController::alloc();
            let alloc = alloc.set_ivars(Ivars {
            cmd_tx,
            inbox,
            cfg_path,
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

        // 引擎事件 → 共享状态
        std::thread::Builder::new()
            .name("nwc-evt-pump".into())
            .spawn(move || {
                while let Ok(ev) = evt_rx.recv() {
                    match ev {
                        EngineEvent::Log(_) | EngineEvent::Warn(_) => {}
                        EngineEvent::Status(s) => {
                            let mut st = state.lock().unwrap();
                            st.status = label_of(&s);
                            if s == Status::Paused {
                                st.paused = true;
                            } else if !matches!(s, Status::Stopped) {
                                st.paused = false;
                            }
                        }
                        EngineEvent::Notify(_, _) => {}
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
