//! 历史记录窗口：原生 LISTBOX + 按钮组，按需创建、关闭即释放。
//! 数据直接读 ~/.networkclipboard/history.json（引擎每次变更都会落盘）；
//! 操作经 EngineCmd 发给引擎（粘贴会同步对端、删除/清空由引擎改写文件）。

#![cfg(windows)]

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::LazyLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{FILETIME, HINSTANCE, HWND, LPARAM, LRESULT, RECT, SYSTEMTIME, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, COLOR_BTNFACE, DEFAULT_GUI_FONT, HBRUSH, HFONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetSystemMetrics, LB_GETCURSEL, LB_GETCOUNT, LB_RESETCONTENT, LB_ADDSTRING, LB_SETCURSEL,
    PostQuitMessage, RegisterClassW, SendMessageW, SetForegroundWindow, ShowWindow, TranslateMessage,
    CW_USEDEFAULT, HMENU, LBN_DBLCLK, LBN_SELCHANGE, MSG, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_SETFONT, WNDCLASSW,
    WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;

use nwc_core::history::{load, HistoryEntry};
use nwc_core::sync::EngineCmd;

const CW: i32 = 520;
const CH: i32 = 380;
const PAD: i32 = 16;

const IDC_LIST: i32 = 1301;
const IDC_PASTE: i32 = 1302;
const IDC_DELETE: i32 = 1303;
const IDC_CLEAR: i32 = 1304;
const IDC_CLOSE: i32 = 1305;

static GUI_FONT: LazyLock<HFONT> = LazyLock::new(|| unsafe {
    HFONT(GetStockObject(DEFAULT_GUI_FONT).0 as isize)
});

thread_local! {
    static CTX: std::cell::RefCell<Option<Ctx>> = const { std::cell::RefCell::new(None) };
}

struct Ctx {
    cmd_tx: Sender<EngineCmd>,
    state_dir: PathBuf,
    entries: Vec<HistoryEntry>,
}

/// 打开历史记录窗口（消息循环在托盘线程上跑；引擎线程不受影响）
pub fn open_history(cmd_tx: Sender<EngineCmd>, state_dir: PathBuf) {
    CTX.with(|c| {
        *c.borrow_mut() = Some(Ctx {
            cmd_tx,
            state_dir,
            entries: Vec::new(),
        })
    });
    unsafe {
        let hmod = GetModuleHandleW(None).unwrap_or_default();
        let hinst = HINSTANCE(hmod.0);
        let class_name = w!("NWC_HISTORY_WND");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(hist_wnd_proc),
            lpszClassName: class_name,
            hInstance: hinst,
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 as isize) + 1),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 && windows::Win32::Foundation::GetLastError().0 != 1410 {
            return;
        }
        let style = WS_OVERLAPPEDWINDOW & !WINDOW_STYLE(0x00020000 | 0x00010000);
        let mut rc = RECT { left: 0, top: 0, right: CW, bottom: CH };
        let _ = AdjustWindowRectEx(&mut rc, style, false, WINDOW_EX_STYLE(0));
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("NetWorkClipboard \u{5386}\u{53f2}\u{8bb0}\u{5f55}"),
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
        let mut wr = RECT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut wr);
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(
            hwnd,
            (sw - (wr.right - wr.left)) / 2,
            (sh - (wr.bottom - wr.top)) / 3,
            wr.right - wr.left,
            wr.bottom - wr.top,
            true,
        );
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn hist_wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            create_controls(hwnd);
            reload_list(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as i32;
            let notify = ((wp.0 >> 16) & 0xFFFF) as u32;
            if id == IDC_LIST && notify == LBN_DBLCLK as u32 {
                paste_selected(hwnd);
            } else if id == IDC_LIST && notify == LBN_SELCHANGE as u32 {
                update_button_state(hwnd);
            } else if id == IDC_PASTE {
                paste_selected(hwnd);
            } else if id == IDC_DELETE {
                delete_selected(hwnd);
            } else if id == IDC_CLEAR {
                clear_all(hwnd);
            } else if id == IDC_CLOSE {
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

unsafe fn apply_font(h: HWND) {
    let _ = SendMessageW(h, WM_SETFONT, WPARAM((*GUI_FONT).0 as usize), LPARAM(1));
}

unsafe fn ctl(parent: HWND, class: PCWSTR, text: &str, style: WINDOW_STYLE, ex: WINDOW_EX_STYLE, x: i32, y: i32, wd: i32, ht: i32, id: i32) -> HWND {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let h = CreateWindowExW(
        ex,
        class,
        PCWSTR(wide.as_ptr()),
        style,
        x,
        y,
        wd,
        ht,
        Some(&parent),
        HMENU(id as isize),
        None,
        None,
    );
    apply_font(h);
    h
}

fn create_controls(hwnd: HWND) {
    unsafe {
        let _ = ctl(hwnd, w!("STATIC"), "双击或点「重新复制」把选中项放回剪贴板（并同步到对端）",
            WS_CHILD | WS_VISIBLE, WINDOW_EX_STYLE(0), PAD, 10, CW - PAD * 2, 18, 0);
        // 列表（LBS_NOTIFY=0x1 | LBS_NOINTEGRALHEIGHT=0x100）
        let _ = ctl(hwnd, w!("LISTBOX"), "",
            WS_CHILD | WS_VISIBLE | WS_VSCROLL | WS_TABSTOP | WINDOW_STYLE(0x0001 | 0x0100),
            WINDOW_EX_STYLE(0x200), PAD, 32, CW - PAD * 2, CH - 32 - 54, IDC_LIST);
        // 按钮行
        let by = CH - 46;
        let _ = ctl(hwnd, w!("BUTTON"), "重新复制", WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            WINDOW_EX_STYLE(0), PAD, by, 96, 30, IDC_PASTE);
        let _ = ctl(hwnd, w!("BUTTON"), "删除", WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            WINDOW_EX_STYLE(0), PAD + 104, by, 68, 30, IDC_DELETE);
        let _ = ctl(hwnd, w!("BUTTON"), "清空全部", WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            WINDOW_EX_STYLE(0), PAD + 180, by, 88, 30, IDC_CLEAR);
        let _ = ctl(hwnd, w!("BUTTON"), "关闭", WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            WINDOW_EX_STYLE(0), CW - PAD - 68, by, 68, 30, IDC_CLOSE);
    }
}

/// 从磁盘重载条目并重建列表（每次操作后调用，保持与引擎落盘一致）
fn reload_list(hwnd: HWND) {
    CTX.with(|c| {
        let mut ctx = c.borrow_mut();
        let Some(ctx) = ctx.as_mut() else { return };
        ctx.entries = load(&ctx.state_dir);
        unsafe {
            let list = get_ctl(hwnd, IDC_LIST);
            let _ = SendMessageW(list, LB_RESETCONTENT, WPARAM(0), LPARAM(0));
            for e in &ctx.entries {
                let line = format!("{}  {}  {}", fmt_time(e.at), if e.peer { "对端" } else { "本机" }, preview(&e.text));
                let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
                let _ = SendMessageW(list, LB_ADDSTRING, WPARAM(0), LPARAM(wide.as_ptr() as isize));
            }
            let _ = SendMessageW(list, windows::Win32::UI::WindowsAndMessaging::LB_SETCURSEL, WPARAM(0), LPARAM(0));
        }
    });
    update_button_state(hwnd);
}

fn update_button_state(hwnd: HWND) {
    unsafe {
        let list = get_ctl(hwnd, IDC_LIST);
        let sel = SendMessageW(list, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
        let has_sel = sel >= 0;
        let cnt = SendMessageW(list, LB_GETCOUNT, WPARAM(0), LPARAM(0)).0;
        let _ = EnableWindow(get_ctl(hwnd, IDC_PASTE), has_sel);
        let _ = EnableWindow(get_ctl(hwnd, IDC_DELETE), has_sel);
        let _ = EnableWindow(get_ctl(hwnd, IDC_CLEAR), cnt > 0);
    }
}

fn selected_text(hwnd: HWND) -> Option<String> {
    CTX.with(|c| {
        let ctx = c.borrow();
        let ctx = ctx.as_ref()?;
        let sel = unsafe { SendMessageW(get_ctl(hwnd, IDC_LIST), LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 };
        if sel < 0 {
            return None;
        }
        ctx.entries.get(sel as usize).map(|e| e.text.clone())
    })
}

fn paste_selected(hwnd: HWND) {
    let Some(text) = selected_text(hwnd) else { return };
    CTX.with(|c| {
        if let Some(ctx) = c.borrow().as_ref() {
            let _ = ctx.cmd_tx.send(EngineCmd::PasteText(text));
        }
    });
    reload_list(hwnd); // 粘贴会把该条置顶
}

fn delete_selected(hwnd: HWND) {
    let Some(text) = selected_text(hwnd) else { return };
    CTX.with(|c| {
        if let Some(ctx) = c.borrow().as_ref() {
            let _ = ctx.cmd_tx.send(EngineCmd::RemoveHistory(text));
        }
    });
    reload_list(hwnd);
}

fn clear_all(hwnd: HWND) {
    CTX.with(|c| {
        if let Some(ctx) = c.borrow().as_ref() {
            let _ = ctx.cmd_tx.send(EngineCmd::ClearHistory);
        }
    });
    reload_list(hwnd);
}

fn get_ctl(hwnd: HWND, id: i32) -> HWND {
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetDlgItem(hwnd, id) }
}

/// 预览：压缩空白、单行、限长
fn preview(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            c => c,
        })
        .collect();
    let trimmed = flat.trim();
    let mut out: String = trimmed.chars().take(90).collect();
    if trimmed.chars().count() > 90 {
        out.push('…');
    }
    out
}

/// unix 毫秒 → 本地 "MM-dd HH:mm"
fn fmt_time(ms: u64) -> String {
    unsafe {
        // unix ms → FILETIME（1601 纪元，100ns 单位）
        const EPOCH_DIFF_MS: u64 = 11_644_473_600_000;
        let ft = (ms + EPOCH_DIFF_MS) * 10_000;
        let filetime = FILETIME {
            dwLowDateTime: (ft & 0xFFFF_FFFF) as u32,
            dwHighDateTime: (ft >> 32) as u32,
        };
        let mut utc = SYSTEMTIME::default();
        if FileTimeToSystemTime(&filetime, &mut utc).is_err() {
            return String::new();
        }
        let mut local = SYSTEMTIME::default();
        if SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err() {
            return String::new();
        }
        format!("{:02}-{:02} {:02}:{:02}", local.wMonth, local.wDay, local.wHour, local.wMinute)
    }
}
