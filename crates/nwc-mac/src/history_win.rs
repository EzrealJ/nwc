//! 历史记录窗口：NSWindow + NSSearchField + NSTableView（对齐 Windows 端 history_win.rs 的功能面）。
//!
//! - 数据直接读 ~/.networkclipboard/history.json（引擎每次变更都会落盘），搜索框实时过滤
//! - 双击行 / 「重新复制」：文本与图片放回剪贴板并同步对端；文件在 Finder 中显示
//! - 操作经 EngineCmd 发给引擎（粘贴/删除/清空），完成后从磁盘重载
//! - 生命周期与 settings.rs 相同：thread_local 持有控制器，关窗退役

#![cfg(target_os = "macos")]

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2::{define_class, msg_send, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSButton, NSSearchField, NSScrollView, NSTableColumn, NSTableView,
    NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use nwc_core::history::{load, EntryKind, HistoryEntry};
use nwc_core::sync::EngineCmd;

/// objc2-foundation 未导出 NSMakeRect，手工构造
fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect { origin: NSPoint::new(x, y), size: NSSize::new(w, h) }
}

/// 窗口客户区尺寸与网格（与 settings.rs 同一套）
const CW: f64 = 480.0;
const CH: f64 = 430.0;
const PAD: f64 = 20.0;
const EDIT_H: f64 = 24.0;
const ROW_GAP: f64 = 8.0;
const BLOCK_GAP: f64 = 14.0;

struct Ivars {
    window: Retained<NSWindow>,
    search: Retained<NSSearchField>,
    table: Retained<NSTableView>,
    cmd_tx: Sender<EngineCmd>,
    state_dir: PathBuf,
    /// 搜索过滤后的视图（表格与操作共用）
    view: RefCell<Vec<HistoryEntry>>,
}

define_class! {
    #[unsafe(super(objc2::runtime::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "NwcHistoryController"]
    #[ivars = Ivars]
    struct NwcHistoryController;

    unsafe impl NSObjectProtocol for NwcHistoryController {}

    #[allow(non_snake_case)]
    impl NwcHistoryController {
        // ---- NSTableViewDataSource / NSControlTextEditingDelegate ----
        // AppKit 运行时动态调用（respondsToSelector 探测），无需显式协议类型；
        // dataSource/delegate 经 msg_send 挂接（见 open_history）。

        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _tv: &NSTableView) -> isize {
            self.ivars().view.borrow().len() as isize
        }

        /// 返回 Retained NSString：raw 指针交还 AppKit（调用方持有并释放）
        #[unsafe(method(tableView:objectValueForTableColumn:row:))]
        fn value_at(
            &self,
            _tv: &NSTableView,
            _col: &NSTableColumn,
            row: isize,
        ) -> *mut AnyObject {
            let v = self.ivars().view.borrow();
            let Some(e) = v.get(row as usize) else {
                return Retained::into_raw(NSString::from_str("")).cast();
            };
            let line = format!(
                "[{}]  {}  {}  {}",
                kind_tag(e.kind),
                fmt_time(e.at),
                if e.peer { "对端" } else { "本机" },
                preview(&e.text)
            );
            Retained::into_raw(NSString::from_str(&line)).cast()
        }

        /// 搜索框文字变化 → 实时过滤
        #[unsafe(method(controlTextDidChange:))]
        fn text_did_change(&self, _notification: &objc2_foundation::NSNotification) {
            self.reload();
        }
    }

    #[allow(non_snake_case)]
    impl NwcHistoryController {
        /// 双击行 / 重新复制按钮：按类型分发
        #[unsafe(method(activateRow:))]
        unsafe fn activate_row(&self, _sender: *mut AnyObject) {
            let e = self.selected_entry();
            let Some(e) = e else { return };
            match e.kind {
                EntryKind::Text => {
                    let _ = self.ivars().cmd_tx.send(EngineCmd::PasteText(e.text));
                }
                EntryKind::Image => {
                    if let Some(path) = e.data {
                        let _ = self.ivars().cmd_tx.send(EngineCmd::PasteImage(path));
                    }
                }
                EntryKind::File => {
                    if let Some(path) = e.data {
                        let _ = std::process::Command::new("open").arg("-R").arg(&path).spawn();
                        return; // Finder 显示不改历史，无需重载
                    }
                }
            }
            self.reload();
        }

        #[unsafe(method(deleteRow:))]
        unsafe fn delete_row(&self, _sender: *mut AnyObject) {
            let Some(e) = self.selected_entry() else { return };
            let _ = self.ivars().cmd_tx.send(EngineCmd::RemoveHistory(e.key().to_string()));
            self.reload();
        }

        #[unsafe(method(clearRows:))]
        unsafe fn clear_rows(&self, _sender: *mut AnyObject) {
            let _ = self.ivars().cmd_tx.send(EngineCmd::ClearHistory);
            self.reload();
        }

        #[unsafe(method(closeWindow:))]
        unsafe fn close_window(&self, _sender: *mut AnyObject) {
            self.ivars().window.close();
            CONTROLLER.with(|c| *c.borrow_mut() = None);
        }
    }
}

impl NwcHistoryController {
    fn selected_entry(&self) -> Option<HistoryEntry> {
        unsafe {
            let row = self.ivars().table.selectedRow();
            if row < 0 {
                return None;
            }
            self.ivars().view.borrow().get(row as usize).cloned()
        }
    }

    /// 从磁盘重载并应用搜索过滤，刷新表格
    fn reload(&self) {
        let iv = self.ivars();
        let query = iv.search.stringValue().to_string().to_lowercase();
        let entries = load(&iv.state_dir);
        let view: Vec<HistoryEntry> = entries
            .iter()
            .filter(|e| query.is_empty() || e.text.to_lowercase().contains(&query))
            .cloned()
            .collect();
        *iv.view.borrow_mut() = view;
        iv.table.reloadData();
    }
}

thread_local! {
    /// 已打开的历史窗口控制器（防重复开窗 + 维持生命周期；AppKit 对象仅主线程）
    static CONTROLLER: std::cell::RefCell<Option<Retained<NwcHistoryController>>> =
        const { std::cell::RefCell::new(None) };
}

/// 由 self 反查 MainThreadMarker 的替代已由 MainThreadOnly 的 `.mtm()` 提供

unsafe fn as_obj<T: ClassType>(r: &T) -> &AnyObject {
    unsafe { std::mem::transmute(r) }
}

/// 打开历史记录窗口（须在主线程调用）
pub fn open_history(mtm: MainThreadMarker, cmd_tx: Sender<EngineCmd>, state_dir: PathBuf) {
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
        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Resizable;
        let window: Retained<NSWindow> = unsafe {
            msg_send![
                mtm.alloc::<NSWindow>(),
                initWithContentRect: rect(0.0, 0.0, CW, CH),
                styleMask: style,
                backing: NSBackingStoreType::Buffered,
                defer: false,
            ]
        };
        window.setTitle(&NSString::from_str("NetWorkClipboard 历史记录"));
        window.setContentMinSize(NSSize::new(400.0, 300.0));

        let view = window.contentView().unwrap();
        // AppKit 默认坐标系原点在左下角（y 向上增长）：按「从上往下」排版时统一翻转
        let content_h = view.frame().size.height;

        let mut place = |v: &AnyObject, x: f64, yy: f64, w: f64, h: f64| {
            let flipped_y = content_h - yy - h;
            let _: () = msg_send![v, setFrame: rect(x, flipped_y, w, h)];
            let _: () = msg_send![&*view, addSubview: v];
        };

        // ── 搜索框 ──
        let search: Retained<NSSearchField> = unsafe { msg_send![mtm.alloc::<NSSearchField>(), init] };
        place(as_obj(&*search), PAD, PAD, CW - PAD * 2.0, EDIT_H);

        // ── 表格（单列，无表头） ──
        let table: Retained<NSTableView> = unsafe { msg_send![mtm.alloc::<NSTableView>(), init] };
        let column = NSTableColumn::new(mtm);
        column.setTitle(&NSString::from_str("条目"));
        table.addTableColumn(&column);
        table.setHeaderView(None);
        table.setRowHeight(22.0);
        table.setAllowsColumnSelection(false);
        let scroll = NSScrollView::new(mtm);
        scroll.setDocumentView(Some(&table));
        scroll.setHasVerticalScroller(true);
        let table_h = CH - PAD - EDIT_H - ROW_GAP - BLOCK_GAP - 30.0 - PAD;
        place(as_obj(&*scroll), PAD, PAD + EDIT_H + ROW_GAP, CW - PAD * 2.0, table_h);

        // ── 控制器（先建才能挂 target/delegate） ──
        let ctrl: Retained<NwcHistoryController> = {
            let mut alloc = NwcHistoryController::alloc(mtm);
            let alloc = alloc.set_ivars(Ivars {
                window: window.clone(),
                search: search.clone(),
                table: table.clone(),
                cmd_tx,
                state_dir,
                view: RefCell::new(Vec::new()),
            });
            msg_send![super(alloc), init]
        };

        // 搜索实时过滤（delegate）+ 数据源 + 双击行激活
        unsafe {
            let _: () = msg_send![&*search, setDelegate: as_obj(&*ctrl)];
            let _: () = msg_send![&*table, setDataSource: as_obj(&*ctrl)];
        }
        table.setTarget(Some(unsafe { as_obj(&*ctrl) }));
        table.setDoubleAction(Some(objc2::sel!(activateRow:)));

        // ── 按钮行 ──
        let by = CH - PAD - 30.0;
        let full = CW - PAD * 2.0;
        let paste_btn = NSButton::buttonWithTitle_target_action(
            &NSString::from_str("重新复制"),
            Some(unsafe { as_obj(&*ctrl) }),
            Some(objc2::sel!(activateRow:)),
            mtm,
        );
        place(as_obj(&*paste_btn), PAD, by, 96.0, 30.0);
        let delete_btn = NSButton::buttonWithTitle_target_action(
            &NSString::from_str("删除"),
            Some(unsafe { as_obj(&*ctrl) }),
            Some(objc2::sel!(deleteRow:)),
            mtm,
        );
        place(as_obj(&*delete_btn), PAD + 104.0, by, 68.0, 30.0);
        let clear_btn = NSButton::buttonWithTitle_target_action(
            &NSString::from_str("清空全部"),
            Some(unsafe { as_obj(&*ctrl) }),
            Some(objc2::sel!(clearRows:)),
            mtm,
        );
        place(as_obj(&*clear_btn), PAD + 180.0, by, 88.0, 30.0);
        let hint_w = full - (PAD + 276.0);
        let hint = unsafe {
            objc2_app_kit::NSTextField::labelWithString(
                &NSString::from_str("文本/图片双击重新复制（并同步对端）；文件打开所在位置"),
                mtm,
            )
        };
        place(as_obj(&*hint), PAD + 276.0, by + 8.0, hint_w, 16.0);

        ctrl.reload();
        window.center();
        window.makeKeyAndOrderFront(None);
        let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        app.activateIgnoringOtherApps(true);
        CONTROLLER.with(|c| *c.borrow_mut() = Some(ctrl));
    }
}

/// 类型标签：文 / 图 / 档
fn kind_tag(k: EntryKind) -> &'static str {
    match k {
        EntryKind::Text => "文",
        EntryKind::Image => "图",
        EntryKind::File => "档",
    }
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

/// unix 毫秒 → 本地 "MM-dd HH:mm"（朴素换算：用本地时区偏移）
fn fmt_time(ms: u64) -> String {
    // 本地时区偏移（秒）：NSDateComponents 太重，直接用 libc localtime_r
    let secs = (ms / 1000) as i64;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = secs as libc::time_t;
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}-{:02} {:02}:{:02}", tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}
