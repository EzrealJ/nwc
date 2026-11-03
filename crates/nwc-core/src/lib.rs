//! NetWorkClipboard 核心：与 Node 版（src/*.js）逐字节兼容的协议实现。
//!
//! 线上格式契约（任何偏差都会导致与 Node 版握手失败或帧解析错误）：
//! - 明文握手：紧凑 JSON + `\n`
//! - 加密帧：`[4B BE 密文长度][12B IV][16B GCM tag][密文]`，长度前缀只算密文，GCM 无 AAD
//! - 明文帧：`[1B kind][body]`，kind=1 JSON / kind=2 二进制
//! - 二进制帧 body = `[JSON header][0x0A][payload]`，以首个 0x0A 分界
//! - scrypt(token, saltC||saltS, N=16384, r=8, p=1, dkLen=32)，客户端盐在前

pub mod config;
pub mod crypto;
pub mod dib;
pub mod discovery;
pub mod echo;
pub mod history;
pub mod logger;
pub mod protocol;
pub mod sync;
pub mod timestamp;
pub mod transport;
pub mod util;
