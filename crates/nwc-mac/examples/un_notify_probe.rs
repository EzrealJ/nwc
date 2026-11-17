//! 通知路径最小复现探针（手动跑）：逐步执行 statusbar::flush_notify 的调用序列，
//! 打印每步结果，定位 NSException 抛出点。需在有 bundle 的环境运行
//! （否则 currentNotificationCenter 为 nil，走不到有问题的路径）。
//!
//! 用法：
//!   cargo build --release -p nwc-mac --example un_notify_probe
//!   # 塞进临时 bundle 从终端跑，stderr 可见异常信息
//!   mkdir -p /tmp/nwc-probe.app/Contents/MacOS
//!   cp target/release/examples/un_notify_probe /tmp/nwc-probe.app/Contents/MacOS/
//!   cp crates/nwc-mac/probe-Info.plist /tmp/nwc-probe.app/Contents/Info.plist 2>/dev/null || true
//!   /tmp/nwc-probe.app/Contents/MacOS/un_notify_probe

#![cfg(target_os = "macos")]

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, ClassType, MainThreadMarker};
use objc2_foundation::NSString;
use objc2_user_notifications::{
    UNMutableNotificationContent, UNNotificationRequest, UNUserNotificationCenter,
};

fn step(name: &str) {
    println!("▶ {name}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn main() {
    let mtm = MainThreadMarker::new().expect("须在主线程运行");
    unsafe {
        step("1. currentNotificationCenter（raw）");
        let center: *mut AnyObject =
            msg_send![UNUserNotificationCenter::class(), currentNotificationCenter];
        println!("   center null = {}", center.is_null());
        if center.is_null() {
            println!("   无 bundle，探针无法继续；请放进 .app 运行");
            return;
        }

        step("2. requestAuthorization（completionHandler = nil，v0.4.0 原样对照）");
        let _: () = msg_send![
            center,
            requestAuthorizationWithOptions: 7u64,
            completionHandler: Option::<&AnyObject>::None
        ];
        println!("   调用返回（未崩）");

        step("3. alloc/init UNMutableNotificationContent");
        let content: Retained<UNMutableNotificationContent> =
            msg_send![mtm.alloc::<UNMutableNotificationContent>(), init];
        println!("   content ok");

        step("4. setTitle/setBody");
        content.setTitle(&NSString::from_str("探针标题"));
        content.setBody(&NSString::from_str("探针正文"));
        println!("   ok");

        step("5. 构造 UNNotificationRequest");
        let ident = NSString::from_str("probe-1");
        let req: Retained<UNNotificationRequest> = msg_send![
            UNNotificationRequest::class(),
            requestWithIdentifier: &*ident,
            content: &*content,
            trigger: Option::<&AnyObject>::None,
        ];
        println!("   request ok");

        step("6. addNotificationRequest（withCompletionHandler = nil，修正后 selector）");
        let _: () = msg_send![center, addNotificationRequest: &*req, withCompletionHandler: Option::<&AnyObject>::None];
        println!("   add 返回（未崩）；睡 6 秒观察系统回调/弹框阶段…");
        std::thread::sleep(std::time::Duration::from_secs(6));
        println!("✓ 全部步骤通过，进程仍存活");
    }
}
