//! 托盘：Shell_NotifyIcon + 状态图标切换 + 右键菜单 + 气泡通知。
//! 托盘窗口跑在自己的消息泵线程上，通过 channel 与引擎/主线程通信。

#![cfg(windows)]

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NOTIFYICONDATAW,
    NIM_ADD, NIM_DELETE, NIM_MODIFY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuItem, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, EnableMenuItem, GetMessageW, GetWindowLongPtrW, MessageBoxW,
    PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow, SetWindowLongPtrW,
    GetCursorPos, TrackPopupMenu, TranslateMessage, GWLP_USERDATA, HICON, HMENU, MENU_ITEM_FLAGS, MSG,
    MB_ICONWARNING, MB_OK, MF_BYCOMMAND, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    SM_CXSCREEN, SW_SHOWNORMAL, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_DESTROY, WM_RBUTTONUP, WM_CONTEXTMENU,
    WM_LBUTTONUP, WM_RBUTTONDBLCLK, WNDCLASSW,
};

use nwc_core::sync::{EngineCmd, Status};

use crate::icon::{destroy_icon, make_icon, IconKind};


/// 托盘回调消息
const WM_TRAY: u32 = WM_APP + 1;
/// 主线程 → 托盘线程：更新状态
pub const WM_UPDATE_STATUS: u32 = WM_APP + 2;
/// 主线程 → 托盘线程：气泡通知
pub const WM_NOTIFY: u32 = WM_APP + 3;

const IDM_STATUS: u32 = 1;
const IDM_PAUSE: u32 = 2;
const IDM_HISTORY: u32 = 8;
const IDM_INBOX: u32 = 3;
const IDM_LOGS: u32 = 4;
const IDM_SETTINGS: u32 = 5;
const IDM_AUTOSTART: u32 = 6;
const IDM_EXIT: u32 = 7;
const IDM_CLEAR_INBOX: u32 = 9;
const IDM_PEER_STRICT: u32 = 10;
const IDM_PEER_CLEAR: u32 = 11;
/// 已知对端动态条目基址：ID = IDM_PEER_BASE + index
const IDM_PEER_BASE: u32 = 100;

pub struct TrayShared {
    pub icon_kind: Mutex<IconKind>,
    pub tooltip: Mutex<String>,
    pub paused: Mutex<bool>,
    /// 气泡请求 (标题, 正文)
    pub balloon: Mutex<Option<(String, String)>>,
}

/// 托盘控制句柄（主线程持有）
pub struct TrayHandle {
    pub hwnd: isize,
    pub shared: Arc<TrayShared>,
}

pub enum TrayCmd {
    Pause,
    Resume,
    Exit,
    Settings,
}

/// 启动托盘线程。返回主线程用的控制句柄。
pub fn start_tray(
    cmd_tx: Sender<EngineCmd>,
    inbox_dir: PathBuf,
    log_path: PathBuf,
    cfg_path: PathBuf,
    state_dir: PathBuf,
) -> TrayHandle {
    let shared = Arc::new(TrayShared {
        icon_kind: Mutex::new(IconKind::Scanning),
        tooltip: Mutex::new("NetWorkClipboard：启动中…".into()),
        paused: Mutex::new(false),
        balloon: Mutex::new(None),
    });
    let s2 = Arc::clone(&shared);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<isize>();
    std::thread::Builder::new()
        .name("nwc-tray".into())
        .spawn(move || {
            tray_loop(s2, cmd_tx, inbox_dir, log_path, cfg_path, state_dir, ready_tx);
        })
        .expect("spawn tray");
    let hwnd = ready_rx.recv().unwrap_or(0);
    TrayHandle { hwnd, shared }
}

struct TrayCtx {
    shared: Arc<TrayShared>,
    cmd_tx: Sender<EngineCmd>,
    inbox_dir: PathBuf,
    log_path: PathBuf,
    cfg_path: PathBuf,
    state_dir: PathBuf,
    nid: NOTIFYICONDATAW,
}

fn tray_loop(
    shared: Arc<TrayShared>,
    cmd_tx: Sender<EngineCmd>,
    inbox_dir: PathBuf,
    log_path: PathBuf,
    cfg_path: PathBuf,
    state_dir: PathBuf,
    ready: Sender<isize>,
) {
    unsafe {
        let hmod = GetModuleHandleW(None).unwrap_or_default();
        let hinst = HINSTANCE(hmod.0);
        let class_name = w!("NWC_TRAY_WND");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(tray_wnd_proc),
            lpszClassName: class_name,
            hInstance: hinst,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            let _ = ready.send(0);
            return;
        }
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!(""),
            WINDOW_STYLE(0),
            0, 0, 0, 0,
            None,
            None,
            Some(&hinst),
            None,
        );
        if hwnd.0 == 0 {
            let _ = ready.send(0);
            return;
        }
        let ctx = Box::new(TrayCtx {
            shared,
            cmd_tx,
            inbox_dir,
            log_path,
            cfg_path,
            state_dir,
            nid: NOTIFYICONDATAW::default(),
        });
        let ptr = Box::into_raw(ctx);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
        let _ = ready.send(hwnd.0);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
        if ptr as usize != 0 {
            drop(Box::from_raw(ptr));
        }
    }
}

unsafe extern "system" fn tray_wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            let p = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if p != 0 {
                let ctx = &*(p as *const TrayCtx);
                // lParam 是鼠标事件
                match lp.0 as u32 {
                    WM_RBUTTONUP | WM_CONTEXTMENU | WM_RBUTTONDBLCLK => show_menu(ctx, hwnd),
                    WM_LBUTTONUP => {} // 左键：预留（可扩展为打开状态面板）
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_UPDATE_STATUS => {
            let p = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if p != 0 {
                let ctx = &mut *(p as *mut TrayCtx);
                update_icon(ctx, hwnd);
            }
            LRESULT(0)
        }
        WM_NOTIFY => {
            let p = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if p != 0 {
                let ctx = &mut *(p as *mut TrayCtx);
                show_balloon(ctx, hwnd);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn ensure_added(ctx: &mut TrayCtx, hwnd: HWND) {
    if ctx.nid.cbSize == 0 {
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uCallbackMessage = WM_TRAY;
        let kind = *ctx.shared.icon_kind.lock().unwrap();
        nid.hIcon = make_icon(kind).unwrap_or_default();
        set_tip(&mut nid, "NetWorkClipboard");
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            ctx.nid = nid;
        }
    }
}

fn set_tip(nid: &mut NOTIFYICONDATAW, tip: &str) {
    let mut buf = [0u16; 128];
    for (i, c) in tip.encode_utf16().take(127).enumerate() {
        buf[i] = c;
    }
    nid.szTip = buf;
}

unsafe fn update_icon(ctx: &mut TrayCtx, hwnd: HWND) {
    ensure_added(ctx, hwnd);
    if ctx.nid.cbSize == 0 {
        return;
    }
    let kind = *ctx.shared.icon_kind.lock().unwrap();
    let tip = ctx.shared.tooltip.lock().unwrap().clone();
    if let Ok(icon) = make_icon(kind) {
        let old = ctx.nid.hIcon;
        ctx.nid.hIcon = icon;
        if !old.is_invalid() {
            destroy_icon(old);
        }
    }
    set_tip(&mut ctx.nid, &tip);
    ctx.nid.uFlags = NIF_ICON | NIF_TIP;
    let _ = Shell_NotifyIconW(NIM_MODIFY, &ctx.nid);
}

unsafe fn show_balloon(ctx: &mut TrayCtx, hwnd: HWND) {
    ensure_added(ctx, hwnd);
    if ctx.nid.cbSize == 0 {
        return;
    }
    let Some((title, body)) = ctx.shared.balloon.lock().unwrap().take() else { return };
    let mut t = [0u16; 64];
    for (i, c) in title.encode_utf16().take(63).enumerate() {
        t[i] = c;
    }
    let mut b = [0u16; 256];
    for (i, c) in body.encode_utf16().take(255).enumerate() {
        b[i] = c;
    }
    ctx.nid.uFlags = NIF_INFO;
    ctx.nid.szInfoTitle = t;
    ctx.nid.szInfo = b;
    ctx.nid.dwInfoFlags = NIIF_INFO;
    let _ = Shell_NotifyIconW(NIM_MODIFY, &ctx.nid);
}

unsafe fn show_menu(ctx: &TrayCtx, hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let kind = *ctx.shared.icon_kind.lock().unwrap();
    let tip = ctx.shared.tooltip.lock().unwrap().clone();
    let paused = *ctx.shared.paused.lock().unwrap();
    let auto = crate::autostart::autostart_enabled();

    let status_text: String = match kind {
        IconKind::Scanning => "扫描中",
        IconKind::Listening => "监听中",
        IconKind::Connected => "已连接",
        IconKind::Paused => "已暂停",
        IconKind::Stopped => "已停止",
    }
    .to_string();
    append(menu, IDM_STATUS, &format!("{status_text} · {tip}"), MF_STRING | MF_GRAYED);
    append(menu, 0, "", MF_SEPARATOR);
    append(menu, IDM_PAUSE, if paused { "恢复同步" } else { "暂停同步" }, MF_STRING);
    append(menu, IDM_HISTORY, "历史记录…", MF_STRING);
    append(menu, IDM_INBOX, "打开收件箱", MF_STRING);
    append(menu, IDM_CLEAR_INBOX, "清空收件箱…", MF_STRING);
    append(menu, IDM_SETTINGS, "设置…", MF_STRING);
    // 已知对端子菜单：点条目配对；strict 开关；清除配对
    let peers = nwc_core::peers::load(&ctx.state_dir);
    let (preferred, strict) = nwc_core::peers::load_pref(&ctx.state_dir);
    if let Ok(sub) = CreatePopupMenu() {
        if peers.is_empty() {
            append(sub, 0, "（暂无已知设备，连接成功后自动登记）", MF_STRING | MF_GRAYED);
        } else {
            for (i, p) in peers.iter().enumerate() {
                let mark = if p.node_id == preferred { "  ✓" } else { "" };
                append(
                    sub,
                    IDM_PEER_BASE + i as u32,
                    &format!("{} · {} · #{}{}", p.name, p.last_ip, nwc_core::peers::short_id(&p.node_id), mark),
                    MF_STRING,
                );
                if p.node_id == preferred {
                    CheckMenuItem(sub, IDM_PEER_BASE + i as u32, MF_BYCOMMAND.0 | MF_CHECKED.0);
                }
            }
            append(sub, 0, "", MF_SEPARATOR);
            append(sub, IDM_PEER_STRICT, &format!("仅与配对设备同步{}", if strict { " ✓" } else { "" }), MF_STRING);
            if strict {
                CheckMenuItem(sub, IDM_PEER_STRICT, MF_BYCOMMAND.0 | MF_CHECKED.0);
            }
            if !preferred.is_empty() {
                append(sub, IDM_PEER_CLEAR, "清除配对", MF_STRING);
            }
        }
        let wide: Vec<u16> = "已知对端\0".encode_utf16().collect();
        AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(wide.as_ptr()));
    }
    append(menu, IDM_LOGS, "打开日志", MF_STRING);
    append(menu, 0, "", MF_SEPARATOR);
    append(menu, IDM_AUTOSTART, "开机自启", MF_STRING);
    if auto {
        CheckMenuItem(menu, IDM_AUTOSTART, MF_BYCOMMAND.0 | MF_CHECKED.0);
    }
    append(menu, IDM_EXIT, "退出", MF_STRING);
    let _ = EnableMenuItem(menu, IDM_STATUS, MF_BYCOMMAND | MF_GRAYED);

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = windows::Win32::UI::WindowsAndMessaging::DestroyMenu(menu);
    match cmd.0 as u32 {
        IDM_PAUSE => {
            let _ = ctx.cmd_tx.send(if paused { EngineCmd::Resume } else { EngineCmd::Pause });
        }
        IDM_HISTORY => {
            let dir = ctx.state_dir.clone();
            let tx = ctx.cmd_tx.clone();
            // 在独立线程开窗：open_history 自带消息循环，避免阻塞托盘菜单线程
            std::thread::Builder::new()
                .name("nwc-history-win".into())
                .spawn(move || crate::history_win::open_history(tx, dir))
                .ok();
        }
        IDM_INBOX => {
            let _ = std::fs::create_dir_all(&ctx.inbox_dir);
            open_path(&ctx.inbox_dir.to_string_lossy());
        }
        id if (IDM_PEER_BASE..IDM_PEER_BASE + 32).contains(&id) => {
            let peers = nwc_core::peers::load(&ctx.state_dir);
            if let Some(p) = peers.get((id - IDM_PEER_BASE) as usize) {
                let _ = ctx.cmd_tx.send(EngineCmd::SetPeerPref { node_id: Some(p.node_id.clone()), strict: None });
            }
        }
        IDM_PEER_STRICT => {
            let (_, strict) = nwc_core::peers::load_pref(&ctx.state_dir);
            let _ = ctx.cmd_tx.send(EngineCmd::SetPeerPref { node_id: None, strict: Some(!strict) });
        }
        IDM_PEER_CLEAR => {
            let _ = ctx.cmd_tx.send(EngineCmd::SetPeerPref { node_id: Some(String::new()), strict: None });
        }
        IDM_CLEAR_INBOX => {
            // 收件箱是中转缓存：删除不可逆，确认后再清
            let msg: Vec<u16> = format!("确定清空收件箱全部内容？\n{}", ctx.inbox_dir.display())
                .encode_utf16().chain(std::iter::once(0)).collect();
            let title: Vec<u16> = "NetWorkClipboard".encode_utf16().chain(std::iter::once(0)).collect();
            let ok = unsafe {
                use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONWARNING, MB_OKCANCEL};
                MessageBoxW(
                    None,
                    PCWSTR(msg.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    MB_OKCANCEL | MB_ICONWARNING,
                )
            };
            if ok == windows::Win32::UI::WindowsAndMessaging::IDOK {
                let _ = ctx.cmd_tx.send(EngineCmd::ClearInbox);
            }
        }
        IDM_LOGS => {
            if !ctx.log_path.exists() {
                let _ = std::fs::write(&ctx.log_path, "");
            }
            open_path(&ctx.log_path.to_string_lossy());
        }
        IDM_SETTINGS => {
            crate::dialog::open_settings(&ctx.cfg_path);
        }
        IDM_AUTOSTART => {
            let now = crate::autostart::set_autostart(!auto);
            if !now {
                MessageBoxW(
                    None,
                    w!("修改注册表失败"),
                    w!("NetWorkClipboard"),
                    MB_OK | MB_ICONWARNING,
                );
            }
        }
        IDM_EXIT => {
            let _ = ctx.cmd_tx.send(EngineCmd::Shutdown);
            let mut nid = NOTIFYICONDATAW::default();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = 1;
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            PostQuitMessage(0);
            // 兜底：引擎线程若恰在长传输/扫描，正常退出链路可能延迟；
            // 给 500ms 清理（接收批次/状态文件），随后强制结束进程（对齐 mac 端 quitApp）
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(500));
                std::process::exit(0);
            });
        }
        _ => {}
    }
}

fn append(menu: HMENU, id: u32, text: &str, flags: MENU_ITEM_FLAGS) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = AppendMenuW(menu, flags, id as usize, PCWSTR(wide.as_ptr()));
    }
}

fn open_path(path: &str) {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), None, None, windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL);
    }
}
