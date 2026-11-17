//! 手动复现工具（#[ignore]，不进 CI）：向本机真实运行的 nwc 发送一个小文件，
//! 触发「收件 → 写剪贴板文件列表 → 通知」完整链路。用于排查 GUI 侧崩溃。
//!
//! 用法：
//!   NWC_TOKEN=<共享口令> cargo test -p nwc-core --test fake_peer_file -- --ignored --nocapture

use nwc_core::protocol;
use nwc_core::transport::{PeerConnection, PeerOpts};

#[test]
#[ignore = "手动：向 127.0.0.1:8767 的真实实例发文件（口令经 NWC_TOKEN 环境变量提供）"]
fn fake_peer_sends_file() {
    let token = std::env::var("NWC_TOKEN").expect("需要 NWC_TOKEN 环境变量提供共享口令");
    let opts = PeerOpts {
        node_id: "fake-peer-manual".into(),
        name: "fake-peer".into(),
        token,
        heartbeat_ms: 5000,
        heartbeat_timeout_ms: 15000,
        caps: vec![protocol::CAP_FILE_CHUNK],
    };
    let accepted = PeerConnection::connect("127.0.0.1", 8767, &opts).expect("连接真实实例失败");
    let payload: Vec<u8> = (0..2048u32).map(|i| (i % 251) as u8).collect();
    let hdr = protocol::HdrFile {
        t: "file",
        rel: "闪退复现-测试文件.txt",
        size: payload.len() as u64,
        sha256: &nwc_core::util::sha256_hex(&payload),
        index: 1,
        count: 1,
        ri: 0,
        batch_id: "fake-batch",
    };
    assert!(accepted.peer.send_bin(&hdr, &payload), "发送失败");
    println!("文件已发送，等待对端处理（收件/通知/历史）…");
    std::thread::sleep(std::time::Duration::from_secs(5));
}
