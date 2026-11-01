# NetWorkClipboard (nwc)

局域网剪贴板同步小工具（Rust）：**托盘常驻 + 原生剪贴板 + 加密传输**。
Windows 端单个 ~2MB exe、私有内存 ~2MB；macOS 端单进程、零子进程；无需安装任何运行时。

## 下载

从 [Releases](https://github.com/EzrealJ/nwc/releases) 获取：

| 文件 | 平台 |
|---|---|
| `NetWorkClipboard-v*-win-x64.zip` | Windows 10+ x64 |
| `NetWorkClipboard-v*-macos-arm64.zip` | macOS（Apple Silicon） |
| `NetWorkClipboard-v*-macos-x64.zip` | macOS（Intel） |

> macOS 版未签名/未公证：首次打开需右键 →「打开」，或先执行 `xattr -cr nwc-mac` 去除隔离属性。

## 特性

- 两端共享同一口令，scrypt 派生密钥 + AES-256-GCM 加密传输
- 局域网自动发现对端，手动 `--target` 指定亦可
- 大文件 256KB 分块流式传输（能力自动协商，内存占用恒定）
- 文字剪贴板历史（默认 20 条，重启保留；双击条目重新复制并同步对端）
- 自动过滤 198.18.0.0/15（Clash/Mihomo 等 fake-IP TUN 网段，避免误判对端）

## 结构

```
├── crates/
│   ├── nwc-core/    # 纯逻辑（两端共享）：协议帧、加密、传输、发现、回声抑制、同步引擎、配置
│   ├── nwc-win/     # Windows 壳：托盘(Win32)、剪贴板(WM_CLIPBOARDUPDATE 事件驱动)、设置对话框、开机自启、文件日志
│   └── nwc-mac/     # macOS 壳：菜单栏(NSStatusItem)、NSPasteboard、getifaddrs、LaunchAgent
```

## 使用

```
nwc                # 托盘/菜单栏模式（默认）：状态图标 + 右键菜单
nwc --cli          # 控制台模式（日志直出，调试/双实例用）
nwc --help         # 全部参数
```

（macOS 上二进制名为 `nwc-mac`）

- 配置文件：`~/.networkclipboard/config.json`（设置对话框读写，保存后自动重启生效）
- 收件箱默认 `~/NetWorkClipboard/inbox`；日志在 `~/.networkclipboard/logs/`（按天滚动、保留 7 天）
- 托盘菜单：历史记录、暂停/恢复同步、打开收件箱、打开日志、设置、开机自启（Windows 写 HKCU 注册表无 UAC；macOS 用 LaunchAgent）、退出
- 历史记录：文字版剪贴板历史，默认保留 20 条（`~/.networkclipboard/history.json`），可在 config.json 用 `historyEnabled` / `historyLimit` 调整；清空用历史窗口的「清空全部」

## 构建

需要 Rust stable（rustup 安装，MSVC 或 GNU 工具链均可）。

```bash
# Windows（生成 target/release/nwc.exe）
cargo build --release -p nwc-win

# macOS（生成 target/release/nwc-mac）
cargo build --release -p nwc-mac
```

> Windows GNU 工具链注意：`windows` crate 固定在 0.57（0.58+ 经 windows-link 走 raw-dylib，在 windows-gnu 上需要 dlltool 且与 rlib 复用有符号冲突）；如用 GNU 工具链需保证 `dlltool.exe`/`as.exe` 可用（rustup self-contained 目录或 WinLibs）。

发布：推 `v*` 标签（或在 Actions 页手动触发 `release` 工作流）自动构建三端产物并发 Release。

## 测试

```bash
cargo test -p nwc-core          # 单元测试 + Rust↔Rust 回环回归
```

## macOS 首次验证清单（在 Mac 上执行）

代码已通过 `cargo check --target aarch64-apple-darwin` / `x86_64-apple-darwin` 类型检查，但未在真机运行过。首次在 Mac 上：

```bash
# 1. 安装 Rust（如未装）：curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# 2. 构建
cargo build --release -p nwc-mac
# 3. 控制台模式对 Windows 端做互通冒烟：
./target/release/nwc-mac --cli --target <Windows机器IP> --token <口令>
#    复制文本/截图/文件，观察 ↑/↓ 日志与收件箱落盘
# 4. 菜单栏模式（默认）：去掉 --cli，检查状态栏图标与菜单
# 5. 若菜单动作无响应：检查 macOS 15+ 的「允许访问本地网络」弹窗
```

已知未在真机验证的点：NSPasteboard 读写的具体格式行为、菜单栏图标刷新、LaunchAgent 路径转义。若 `cargo build` 报错，优先看 objc2 API 差异（版本已锁 Cargo.toml）。

## 资源占用（Windows 实测）

单进程，私有内存 1.8MB（工作集 10.5MB，含系统 DLL 共享页），单文件 2.1MB。
