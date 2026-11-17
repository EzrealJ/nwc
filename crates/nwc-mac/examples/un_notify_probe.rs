//! 通知探针：查询授权状态 + 可选发一条测试通知（NOTIFY=1 时发送）。
//! 授权状态含义：notDetermined / denied（被拒绝）/ authorized（已授权）/ provisional。
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, ClassType, MainThreadMarker};
use objc2_foundation::NSString;
use objc2_user_notifications::{
    UNMutableNotificationContent, UNNotificationRequest, UNUserNotificationCenter,
};

fn main() {
    let mtm = MainThreadMarker::new().expect("须在主线程运行");
    unsafe {
        let center: *mut AnyObject =
            msg_send![UNUserNotificationCenter::class(), currentNotificationCenter];
        if center.is_null() {
            println!("无 bundle，探针需在 .app 环境运行");
            return;
        }
        // 查询授权状态（同步等待）
        step("getNotificationSettings");
        let done = block2::RcBlock::new(move |settings: *mut AnyObject| {
            let raw: u64 = msg_send![settings, authorizationStatus];
            println!("   授权状态原始值 = {raw}  (0=未决定 1=被拒绝 2=已授权 3=临时 4=临时会话)");
        });
        let _: () = msg_send![center, getNotificationSettingsWithCompletionHandler: &*done];
        std::thread::sleep(std::time::Duration::from_secs(1));

        if std::env::var("NOTIFY").is_ok() {
            step("addNotificationRequest（发一条测试通知）");
            let content: Retained<UNMutableNotificationContent> =
                msg_send![mtm.alloc::<UNMutableNotificationContent>(), init];
            content.setTitle(&NSString::from_str("通知测试"));
            content.setBody(&NSString::from_str("如果你看到这条横幅，通知链路已通"));
            let ident = NSString::from_str("probe-test");
            let req: Retained<UNNotificationRequest> = msg_send![
                UNNotificationRequest::class(),
                requestWithIdentifier: &*ident,
                content: &*content,
                trigger: Option::<&AnyObject>::None,
            ];
            let _: () = msg_send![center, addNotificationRequest: &*req, withCompletionHandler: Option::<&AnyObject>::None];
            std::thread::sleep(std::time::Duration::from_secs(3));
            println!("   已发送");
        }
    }
}

fn step(name: &str) {
    println!("▶ {name}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}
