# NetWorkClipboard (nwc)

局域网剪贴板同步小工具（Rust）：**托盘常驻 + 原生剪贴板 + 加密传输**。
Windows 端单个 ~2MB exe、私有内存 ~2MB；macOS 端单进程、剪贴板轮询零子进程（仅通知/打开文件等偶发动作调用系统命令）；无需安装任何运行时。

## 下载

从 [Releases](https://github.com/EzrealJ/nwc/releases) 获取：

| 文件 | 平台 | 说明 |
|---|---|---|
| `NetWorkClipboard-v*-win-x64.zip` | Windows 10+ x64 | 解压即用 `nwc.exe` |
| `NetWorkClipboard-v*-macos-arm64-app.zip` | macOS（Apple Silicon） | **推荐**：解压得 `NetWorkClipboard.app`，双击即用 |
| `NetWorkClipboard-v*-macos-x64-app.zip` | macOS（Intel） | 同上 |
| `NetWorkClipboard-v*-macos-arm64.zip` / `-x64.zip` | macOS | 裸二进制，终端调试用（不要给裸文件加 `.app` 后缀，那不是合法 bundle） |

> macOS 版未签名/未公证：首次打开需右键 →「打开」，或先执行 `xattr -cr NetWorkClipboard.app` 去除隔离属性。

## 特性

- 两端共享同一口令，scrypt 派生密钥 + AES-256-GCM 加密传输
- 局域网自动发现对端，手动 `--target` 指定亦可
- **已知对端与配对**：每次成功握手自动登记设备（`peers.json`）；菜单「已知对端」可选择配对设备——默认优先连接它（IP 漂移按设备 ID 识别），开启「仅与配对设备同步」后非配对设备一律拒绝（多台设备共用口令时防止剪贴板流向错误机器）
- 大文件 256KB 分块流式传输（能力自动协商，内存占用恒定）
- 剪贴板图片双向同步：Windows 端 PNG + CF_DIBV5 双格式写入、DIB（24/32bpp）自动转码；macOS 端 TIFF + PNG + PNGf 三格式写入，各类粘贴目标都能识别
- 文字剪贴板历史（默认 20 条，重启保留；Windows 双击条目 / macOS 菜单栏「历史记录」点击条目即可重新复制并同步对端）
- 收到文件时托盘气泡 / 系统通知提示；收件箱自动按天清理（中转缓存语义，默认保留 14 天）
- 单实例保护（Windows 命名互斥量 / macOS flock），双开自动拦截
- 自动过滤 198.18.0.0/15（Clash/Mihomo 等 fake-IP TUN 网段，避免误判对端）
- 两端文件日志（`~/.networkclipboard/logs/`，按天滚动保留 7 天），剪贴板内容识别失败时自动记录格式诊断信息

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

- 配置文件：`~/.networkclipboard/config.json`（Windows 设置对话框读写，保存后自动重启生效；macOS 菜单「编辑配置」用文本编辑器打开）
- 收件箱默认 `~/NetWorkClipboard/inbox`——**它是文件中转缓存而非归档存储**：重复接收同一文件按内容哈希幂等覆盖（永远只占一份），同名但内容不同才以 `名字 (N).ext` 避让；自动按保留天数清理（默认 14 天，`inboxKeepDays` 配置，`0` 关闭；启动与每批次接收后触发），也可在菜单「清空收件箱…」手动清理；日志在 `~/.networkclipboard/logs/`（按天滚动、保留 7 天，两端 GUI 模式均落盘）
- 托盘/菜单栏菜单：暂停/恢复同步、历史记录、打开收件箱、打开日志、设置/编辑配置、开机自启（Windows 写 HKCU 注册表无 UAC；macOS 用 LaunchAgent）、退出
- 历史记录：Windows 是独立窗口（双击粘贴、可删单条）；macOS 在菜单栏子菜单里（点击粘贴、**按住 Option 点击删除该条**）。默认保留 20 条（`~/.networkclipboard/history.json`），可在 config.json 用 `historyEnabled` / `historyLimit` 调整
- macOS 菜单栏为 SF Symbol 状态图标（连接/扫描/暂停等），悬停显示状态文字
- 排查问题：两端看 `~/.networkclipboard/logs/` 当天日志；图片/文件没同步时，日志会记录剪贴板实际格式列表与转码失败原因

## 构建

需要 Rust stable（rustup 安装，MSVC 或 GNU 工具链均可）。

```bash
# Windows（生成 target/release/nwc.exe）
cargo build --release -p nwc-win

# macOS（生成 target/release/nwc-mac）
cargo build --release -p nwc-mac

# macOS 上交叉编译 Windows 版（需先 brew install mingw-w64）
rustup target add x86_64-pc-windows-gnu
cargo build --release -p nwc-win --target x86_64-pc-windows-gnu
```

> Windows GNU 工具链注意：`windows` crate 固定在 0.57（0.58+ 经 windows-link 走 raw-dylib，在 windows-gnu 上需要 dlltool 且与 rlib 复用有符号冲突）；如用 GNU 工具链需保证 `dlltool.exe`/`as.exe` 可用（rustup self-contained 目录或 WinLibs）。

发布：推 `v*` 标签（或在 Actions 页手动触发 `release` 工作流）自动构建三端产物并发 Release；macOS 产物包含裸二进制与 `.app` bundle 两种。

## 测试

```bash
cargo test -p nwc-core                          # 平台无关单元测试 + 双引擎回环/端到端（任意系统）
cargo test -p nwc-win -- --test-threads=1       # Windows 剪贴板真机测试（需在 Windows 上）
cargo test -p nwc-mac -- --test-threads=1       # macOS NSPasteboard 真机测试（需在 macOS 上）
```

测试金字塔（CI 自动执行，见 `.github/workflows/ci.yml`）：

| 层 | 内容 | 跑在哪 |
|---|---|---|
| 单元 | DIB 编解码 / 协议 / 加密 / 同步引擎 / 配置（含平台 cfg 分支） | Linux（win/mac 复跑锁定分支行为） |
| 平台真机 | 剪贴板读写往返（win：PNG+DIBV5 双格式；mac：TIFF/PNG/PNGf、NSURL↔NSFilenames） | windows / macos runner 实机 |
| 端到端 | 双引擎经真实 TCP 互发文本/图片，断言对端写入字节级一致 | Linux |

CI 触发已收敛（PR 全矩阵；main 仅代码变更时跑；纯文档不触发），发版工作流在出产物前会先跑完同一套测试。

## 资源占用（Windows 实测）

单进程，私有内存 1.8MB（工作集 10.5MB，含系统 DLL 共享页），单文件 2.1MB。
