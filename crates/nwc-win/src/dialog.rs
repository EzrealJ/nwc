//! 设置对话框：Win32 原生控件按需创建，关闭即释放（无 GUI 框架、无常驻窗口成本）。
//!
//! 排版：统一网格（左右边距 20，label 高 18 / edit 高 24，label 相对 edit 居中对齐）；
//! 字体：所有控件 WM_SETFONT(DEFAULT_GUI_FONT)，与系统对话框观感一致；
//! 保存：先校验再写 config.json，成功后先释放单实例锁再拉起新实例（见 restart.rs）。

#![cfg(windows)]

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::LazyLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, COLOR_BTNFACE, DEFAULT_GUI_FONT, HBRUSH, HFONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDlgItem, GetMessageW,
    GetSystemMetrics, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, MessageBoxW, MoveWindow,
    AdjustWindowRectEx, PostQuitMessage, RegisterClassW, SendMessageW, SetForegroundWindow, ShowWindow,
    TranslateMessage,
    CW_USEDEFAULT, HMENU, MSG, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_SETFONT,
    WNDCLASSW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
};

/// 客户区尺寸：宽度 400；高度由布局推出（内容底 312 + 底边距 20）
const CW: i32 = 400;
const CH: i32 = 332;
/// 统一边距与行高
const PAD: i32 = 20;
const LABEL_H: i32 = 18;
const EDIT_H: i32 = 24;
const ROW_GAP: i32 = 8; // label 底 → edit 顶
const BLOCK_GAP: i32 = 14; // 字段块之间

const IDC_TOKEN: i32 = 1101;
const IDC_PORT: i32 = 1102;
const IDC_NAME: i32 = 1103;
const IDC_ROLE: i32 = 1104;
const IDC_INBOX: i32 = 1105;
const IDC_SAVE: i32 = 1201;
const IDC_CANCEL: i32 = 1202;

thread_local! {
    static CFG_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// 系统默认 GUI 字体（微软雅黑/Segoe UI 9pt，随系统），所有控件统一使用
static GUI_FONT: LazyLock<HFONT> = LazyLock::new(|| unsafe {
    // GetStockObject 返回 HGDIOBJ，DEFAULT_GUI_FONT 是标准对话框字体
    HFONT(GetStockObject(DEFAULT_GUI_FONT).0 as isize)
});

/// 打开设置对话框（消息循环在托盘线程上跑；引擎线程不受影响）
pub fn open_settings(cfg_path: &std::path::Path) {
    CFG_PATH.with(|c| *c.borrow_mut() = Some(cfg_path.to_path_buf()));
    unsafe {
        let hmod = GetModuleHandleW(None).unwrap_or_default();
        let hinst = HINSTANCE(hmod.0);
        let class_name = w!("NWC_SETTINGS_WND");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(dlg_wnd_proc),
            lpszClassName: class_name,
            hInstance: hinst,
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 as isize) + 1),
            ..Default::default()
        };
        // ERROR_CLASS_ALREADY_EXISTS = 1410：重复打开时沿用已注册的类
        if RegisterClassW(&wc) == 0 && windows::Win32::Foundation::GetLastError().0 != 1410 {
            return;
        }
        // 去掉最小化/最大化按钮（WS_MINIMIZEBOX|WS_MAXIMIZEBOX）
        let style = WS_OVERLAPPEDWINDOW & !WINDOW_STYLE(0x00020000 | 0x00010000);
        // 用 AdjustWindowRectEx 从客户区尺寸精确推外框（含标题栏/边框，随 DPI 正确）
        let mut rc = RECT { left: 0, top: 0, right: CW, bottom: CH };
        let _ = AdjustWindowRectEx(&mut rc, style, false, WINDOW_EX_STYLE(0));
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("NetWorkClipboard \u{8bbe}\u{7f6e}"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rc.right - rc.left,
            rc.bottom - rc.top,
            None,
            None,
            Some(&hinst),
            None,
        );
        if hwnd.0 == 0 {
            return;
        }
        // 屏幕居中
        let mut rc = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rc);
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let wdt = rc.right - rc.left;
        let hgt = rc.bottom - rc.top;
        let _ = MoveWindow(hwnd, (sw - wdt) / 2, (sh - hgt) / 3, wdt, hgt, true);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn dlg_wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            create_controls(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as i32;
            if id == IDC_SAVE {
                on_save(hwnd);
            } else if id == IDC_CANCEL {
                let _ = DestroyWindow(hwnd);
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

// ---- 控件创建（统一字体 + 网格布局） ----

unsafe fn apply_font(h: HWND) {
    let _ = SendMessageW(
        h,
        WM_SETFONT,
        WPARAM((*GUI_FONT).0 as usize),
        LPARAM(1), // MAKELPARAM(TRUE,0)：随字体重绘
    );
}

/// label：y 传所属 edit 的 y，内部 +3 与 24 高的输入框视觉居中
unsafe fn label(parent: HWND, text: &str, x: i32, y_edit: i32, wd: i32) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let h = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("STATIC"),
        PCWSTR(wide.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        x,
        y_edit + 3,
        wd,
        LABEL_H,
        Some(&parent),
        None,
        None,
        None,
    );
    apply_font(h);
}

unsafe fn edit(parent: HWND, id: i32, text: &str, x: i32, y: i32, wd: i32) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let h = CreateWindowExW(
        WINDOW_EX_STYLE(0x200), // WS_EX_CLIENTEDGE
        w!("EDIT"),
        PCWSTR(wide.as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(0x0080), // ES_AUTOHSCROLL
        x,
        y,
        wd,
        EDIT_H,
        Some(&parent),
        HMENU(id as isize),
        None,
        None,
    );
    apply_font(h);
}

unsafe fn button(parent: HWND, id: i32, text: &str, x: i32, y: i32, wd: i32, ht: i32) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let b = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("BUTTON"),
        PCWSTR(wide.as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP,
        x,
        y,
        wd,
        ht,
        Some(&parent),
        HMENU(id as isize),
        None,
        None,
    );
    apply_font(b);
}

fn create_controls(hwnd: HWND) {
    unsafe {
        let cfg = read_current_cfg();
        let full = CW - PAD * 2; // 360
        let half = (full - 16) / 2; // 两列布局，列间距 16
        let x2 = PAD + half + 16;

        // ── 共享口令 ──
        let mut y = PAD;
        label(hwnd, "共享口令（两端必须一致）", PAD, y + 3, full);
        y += LABEL_H + ROW_GAP;
        edit(hwnd, IDC_TOKEN, &cfg.0, PAD, y, full);
        y += EDIT_H + BLOCK_GAP;

        // ── 端口 / 设备名（两列） ──
        label(hwnd, "端口", PAD, y + 3, half);
        label(hwnd, "设备名", x2, y + 3, half);
        y += LABEL_H + ROW_GAP;
        edit(hwnd, IDC_PORT, &cfg.1, PAD, y, 100);
        edit(hwnd, IDC_NAME, &cfg.2, x2, y, half);
        y += EDIT_H + BLOCK_GAP;

        // ── 角色 ──
        label(hwnd, "角色", PAD, y + 3, 60);
        label(hwnd, "auto（自动抉择）/ server / client", PAD + 140, y + 3, full - 140);
        y += LABEL_H + ROW_GAP;
        edit(hwnd, IDC_ROLE, &cfg.3, PAD, y, 140);
        y += EDIT_H + BLOCK_GAP;

        // ── 收件箱目录 ──
        label(hwnd, "收件箱目录", PAD, y + 3, full);
        y += LABEL_H + ROW_GAP;
        edit(hwnd, IDC_INBOX, &cfg.4, PAD, y, full);
        y += EDIT_H + BLOCK_GAP + 6;

        // ── 按钮行 ──
        button(hwnd, IDC_SAVE, "保存并重启", PAD, y, 112, 30);
        button(hwnd, IDC_CANCEL, "取消", PAD + 122, y, 76, 30);
        label(hwnd, "保存后程序会自动重启并按新配置运行", PAD + 215, y + 8, full - 210);
    }
}

// ---- 读写 ----

fn read_current_cfg() -> (String, String, String, String, String) {
    let path = CFG_PATH.with(|c| c.borrow().clone()).unwrap_or_else(default_cfg_path);
    let mut token = String::new();
    let mut port = "8767".into();
    let mut name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PC".into());
    let mut role = "auto".into();
    let mut inbox = nwc_core::config::home_dir()
        .join("NetWorkClipboard")
        .join("inbox")
        .to_string_lossy()
        .into_owned();
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(s) = v["token"].as_str() {
                token = s.into();
            }
            if let Some(n) = v["port"].as_u64() {
                port = n.to_string();
            }
            if let Some(s) = v["name"].as_str() {
                name = s.into();
            }
            if let Some(s) = v["role"].as_str() {
                role = s.into();
            }
            if let Some(s) = v["inboxDir"].as_str() {
                inbox = s.into();
            }
        }
    }
    (token, port, name, role, inbox)
}

fn default_cfg_path() -> PathBuf {
    nwc_core::config::home_dir()
        .join(".networkclipboard")
        .join("config.json")
}

unsafe fn get_text(hwnd: HWND, id: i32) -> String {
    let ctl = GetDlgItem(hwnd, id);
    let len = GetWindowTextLengthW(ctl);
    let mut buf = vec![0u16; (len + 1) as usize];
    let n = GetWindowTextW(ctl, &mut buf);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn on_save(hwnd: HWND) {
    unsafe {
        // 先校验再落盘，避免写坏配置导致重启后起不来
        let token = get_text(hwnd, IDC_TOKEN);
        let name = get_text(hwnd, IDC_NAME).trim().to_string();
        let inbox = get_text(hwnd, IDC_INBOX).trim().to_string();
        let port_raw = get_text(hwnd, IDC_PORT);
        let role_raw = get_text(hwnd, IDC_ROLE);
        let Some(port) = port_raw.trim().parse::<u16>().ok().filter(|p| *p >= 1) else {
            warn_box(hwnd, &format!("端口无效：\"{port_raw}\"（应为 1-65535 的数字）"));
            return;
        };
        let role = role_raw.trim().to_string();
        if !matches!(role.as_str(), "auto" | "server" | "client") {
            warn_box(hwnd, &format!("角色无效：\"{role}\"（只能是 auto / server / client）"));
            return;
        }
        if name.is_empty() {
            warn_box(hwnd, "设备名不能为空");
            return;
        }

        let path = CFG_PATH.with(|c| c.borrow().clone()).unwrap_or_else(default_cfg_path);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // 合并写：只覆盖对话框管理的键，保留用户手写的其它键（scanCidr/inboxKeepDays 等）
        let mut v: serde_json::Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        if let Some(obj) = v.as_object_mut() {
            obj.insert("token".into(), serde_json::json!(token));
            obj.insert("port".into(), serde_json::json!(port));
            obj.insert("name".into(), serde_json::json!(name));
            obj.insert("role".into(), serde_json::json!(role));
            if inbox.is_empty() {
                obj.remove("inboxDir");
            } else {
                obj.insert("inboxDir".into(), serde_json::json!(inbox));
            }
        }
        let text = serde_json::to_string_pretty(&v).unwrap_or_default();
        if std::fs::write(&path, text + "\n").is_err() {
            warn_box(hwnd, &format!("写入失败：{}", path.display()));
            return;
        }
        let _ = DestroyWindow(hwnd);
        crate::restart::restart_process();
    }
}

unsafe fn warn_box(hwnd: HWND, msg: &str) {
    let mut wide: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    MessageBoxW(
        hwnd,
        PCWSTR(wide.as_mut_ptr()),
        w!("NetWorkClipboard"),
        windows::Win32::UI::WindowsAndMessaging::MB_OK
            | windows::Win32::UI::WindowsAndMessaging::MB_ICONWARNING,
    );
}
