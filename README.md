# NetWorkClipboard 原生版（Rust）

Node 版的图形化重写：**托盘常驻 + 原生剪贴板 + 与 Node 版逐字节互通**。
把 Windows 端从「node.exe ~50MB + 常驻 PowerShell 代理 ~60MB」降到 **单个 2MB exe、私有内存 ~2MB**；macOS 端从「node + 每次轮询 spawn 4-6 个 osascript/pbpaste/sips 子进程」降到单进程零子进程。

## 结构

```
├── crates/
│   ├── nwc-core/    # 纯逻辑（两端共享）：协议帧、加密、传输、发现、回声抑制、同步引擎、配置
│   ├── nwc-win/     # Windows 壳：托盘(Win32)、剪贴板(WM_CLIPBOARDUPDATE 事件驱动)、设置对话框、开机自启、文件日志
│   └── nwc-mac/     # macOS 壳：菜单栏(NSStatusItem)、NSPasteboard、getifaddrs、LaunchAgent
```

- 线上协议与 `src/*.js` 的 Node 版**逐字节兼容**（握手/加密帧/全部消息字段），可与 Node 版混用、随时回退
- Rust↔Rust 之间自动协商 `fileChunk` 能力（hello 的 caps 字段，Node 版会忽略）：大文件 256KB 分块流式传输，内存恒定；与 Node 版互通时自动回退单帧模式
- 新增过滤：198.18.0.0/15（Clash/Mihomo 等 fake-IP TUN 网段，避免误判对端）

## 构建

需要 Rust stable（rustup 安装，MSVC 或 GNU 工具链均可）。

```bash
# Windows（生成 target/release/nwc.exe）
cargo build --release -p nwc-win

# macOS（生成 target/release/nwc-mac）
cargo build --release -p nwc-mac
```

> Windows GNU 工具链注意：`windows` crate 固定在 0.57（0.58+ 经 windows-link 走 raw-dylib，在 windows-gnu 上需要 dlltool 且与 rlib 复用有符号冲突）；如用 GNU 工具链需保证 `dlltool.exe`/`as.exe` 可用（rustup self-contained 目录或 WinLibs）。

## 使用

```
nwc.exe                # 托盘模式（默认）：状态图标 + 右键菜单
nwc.exe --cli          # 控制台模式（日志直出，调试/双实例用）
nwc.exe --help         # 全部参数（与 Node 版同名同义）
```

- 配置文件：`~/.networkclipboard/config.json`（字段与 Node 版 config.json 完全一致；设置对话框读写它，保存后自动重启生效）
- 收件箱默认 `~/NetWorkClipboard/inbox`；日志在 `~/.networkclipboard/logs/`（按天滚动、保留 7 天）
- 托盘菜单：暂停/恢复、打开收件箱、打开日志、设置、开机自启（HKCU 注册表，无 UAC / macOS 用 LaunchAgent）、退出
- 从 Node 版迁移：把项目根目录 config.json 的内容复制到 `~/.networkclipboard/config.json` 即可

## 测试

```bash
cargo test -p nwc-core          # 单元测试 + 回环回归
```

若仓库根存在 Node 版（`src/index.js`，需 node 可执行），会额外跑与 Node 版的真实互通回归（握手/认证/错误口令/busy/心跳，Rust 客户端↔Node 服务端、Node 客户端↔Rust 服务端双向）；独立分发时无此目录，自动跳过。

## macOS 首次验证清单（在 Mac 上执行）

代码已通过 `cargo check --target aarch64-apple-darwin` / `x86_64-apple-darwin` 类型检查，但未在真机运行过。首次在 Mac 上：

```bash
# 1. 安装 Rust（如未装）：curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# 2. 构建
cargo build --release -p nwc-mac
# 3. 控制台模式对 Windows 新版（或 Node 版）做互通冒烟：
./target/release/nwc-mac --cli --target <Windows机器IP> --token <口令>
#    复制文本/截图/文件，观察 ↑/↓ 日志与收件箱落盘
# 4. 菜单栏模式（默认）：去掉 --cli，检查状态栏图标与菜单
# 5. 若菜单动作无响应：检查 macOS 15+ 的「允许访问本地网络」弹窗
```

已知未在真机验证的点：NSPasteboard 读写的具体格式行为、菜单栏图标刷新、LaunchAgent 路径转义。若 `cargo build` 报错，优先看 objc2 API 差异（版本已锁 Cargo.toml）。

## 内存对比（Windows 实测）

| | 常驻进程 | 私有内存 | 体积 |
|---|---|---|---|
| Node 版 | node.exe + powershell.exe(WinForms) | ~110MB | 需装 Node 18+ |
| 原生版 | 单进程 | **1.8MB**（工作集 10.5MB，含系统 DLL 共享页） | **2.1MB** 单文件 |
