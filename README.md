# NetWorkClipboard

**同一局域网，同一块剪贴板。**

[![CI](https://github.com/EzrealJ/nwc/actions/workflows/ci.yml/badge.svg)](https://github.com/EzrealJ/nwc/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/EzrealJ/nwc)](https://github.com/EzrealJ/nwc/releases)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS-blue)

---

在 Windows 上复制一段文字、一张截图、一个文件，走到 Mac 前，直接 ⌘V。

NetWorkClipboard 是一个局域网剪贴板同步工具。它常驻系统托盘（Windows）或菜单栏（macOS），把你复制的内容加密后同步给同一网络里的另一台电脑。没有账号，没有云端中转，没有运行时依赖。Windows 端是约 2 MB 的单文件 exe，macOS 端是单进程，内存也各自只占 2 MB 左右（Windows 实测私有内存 1.8 MB）。装好之后你不会再想起它——直到下一次 ⌘V。

## 为什么是 nwc

两台电脑之间传点东西，套路通常是：微信文件传输助手（要联网、要登录）、网盘（要上传下载）、数据线（要找线）。而这些内容本来就在你的剪贴板里。

nwc 的答案是：**剪贴板本来就该跟着人走**。只要两台电脑在同一局域网，复制即同步。

和 LocalSend、Snapdrop 这类邻域工具相比，差异在「无动作」：它们帮你把选中的文件发过去，nwc 让剪贴板本身跨设备——没有发送这个步骤。取舍同样直白：只在局域网内工作，没有手机端，也不做云端中转；换来的是两端纯原生实现（Win32 / AppKit，无 Electron、无内嵌浏览器内核）和单文件分发。

## 特性

- **即复即贴** — 复制后无需任何操作，对端剪贴板已是最新内容。文字、图片、文件都行；大文件流式分块，传多大都不多占一个字节
- **配对机制** — 咖啡馆、办公室里设备杂？把你的机器加入配对名单，剪贴板只流向它们
- **剪贴板历史** — 文字、图片、文件统一留存（默认 20 条，可调），带搜索；双击任意一条，重新复制并同步
- **收件箱有分寸** — 收到的文件进收件箱：同名文件原位覆盖，按天自动清理。它是中转站，不是垃圾场
- **断线不误事** — 换网自动重连，断线内容补发；回声抑制保证同一份内容绝不被重复同步
- **进度看得见** — 大文件传输有实时进度；文件一到就通知，点开直达收件箱

## 快速开始

从 [Releases](https://github.com/EzrealJ/nwc/releases) 下载：

| 你的电脑 | 下载 | 安装 |
|---|---|---|
| Windows 10 及以上 | `NetWorkClipboard-v*-win-x64.zip` | 解压，运行 `nwc.exe` |
| Mac · Apple Silicon | `NetWorkClipboard-v*-macos-arm64-app.zip` | 解压，双击 `.app` |
| Mac · Intel 芯片 | `NetWorkClipboard-v*-macos-x64-app.zip` | 同上（需 macOS 11+） |

> 不确定 Mac 芯片：点左上角  →「关于本机」，看「芯片」一行；2020 年末之后的机型基本都是 Apple Silicon。

两端各开一份，在菜单里打开「设置…」，把**共享口令设成一样**——这就是全部配置。连接成功后，状态图标会告诉你。

> 首次运行的两个拦路虎：macOS 未做公证，需右键 →「打开」，或执行 `xattr -cr NetWorkClipboard.app`；Windows 未签名，SmartScreen 弹窗时选「仍要运行」。

## 使用

日常操作都在托盘/菜单栏菜单里：暂停同步、历史记录、打开收件箱、已知对端（配对）、设置、开机自启。

行为细节不必记，但值得知道：

- 收件箱（`~/NetWorkClipboard/inbox`）是**中转缓存**，按天清理（默认留 14 天），同名文件覆盖原位——别把它当归档目录用
- 历史与配置都在 `~/.networkclipboard/`；设置窗口里改完、保存即生效（应用会自动重启）
- 调试或临时双开时，`nwc --cli` 让日志直打终端

进阶配置直接改 `~/.networkclipboard/config.json`（覆盖设置窗口没有的项，保存后同样自动重启）：

| 配置项 | 默认 | 说明 |
|---|---|---|
| `port` | `8767` | 监听/连接端口 |
| `role` | `auto` | `auto` / `server`（只监听）/ `client`（只连接） |
| `inboxRenameOnConflict` | `false` | `true` 时同名文件冲突保留副本改名，而非覆盖 |
| `inboxKeepDays` | `14` | 收件保留天数，`0` 关闭自动清理 |
| `historyEnabled` / `historyLimit` | `true` / `20` | 剪贴板历史开关与条数上限 |

## 常见问题

**连不上？**
先查三件事：两边的口令一致、系统防火墙放行、路由器没开 AP 隔离。nwc 会自动过滤虚拟网卡和 Clash 系 fake-IP 网段（198.18.0.0/15），避免认错对端。

**图片复制过去了，粘贴没反应？**
nwc 在两端写入多种格式（Windows：PNG + DIBV5；macOS：TIFF + PNG），绝大多数应用都能识别。个别应用仍不认时，翻一下 `~/.networkclipboard/logs/`——剪贴板实际格式和转码失败原因都记在里面。

**刚刚复制的内容没有同步？**
10 秒内复制相同内容，会被回声抑制拦下——这是特性，不是故障。另外确认两端没有处于暂停状态。

## 构建

Rust 1.85+（edition 2024 的最低要求，rustup 装最新 stable 即可；MSVC / GNU 工具链均可）：

```bash
cargo build --release -p nwc-mac                                  # macOS
cargo build --release -p nwc-win --target x86_64-pc-windows-gnu   # Windows 交叉编译
scripts/package.sh                                                # 打包（与 CI 产物一致，进 dist/）
```

交叉编译 Windows 需要 `rustup target add x86_64-pc-windows-gnu` 和 `brew install mingw-w64`。另外 `windows` crate 固定在 0.57——0.58 起在 GNU 工具链下有符号冲突，升级前先验证。

## 测试

测试金字塔：单元（协议、加密、引擎）→ 平台真机（真实剪贴板读写）→ 端到端（双引擎经真实 TCP 互发，字节级断言）。CI 三端各司其职：Linux 跑核心单元与端到端，macOS 和 Windows 另加真机测试；发布产物目前只有 Windows 与 macOS 两端。

```bash
cargo test -p nwc-core                     # 单元 + 端到端，任意平台
cargo test -p nwc-mac -- --test-threads=1  # macOS 真机
cargo test -p nwc-win -- --test-threads=1  # Windows 真机
```

## 项目结构

| crate | 职责 |
|---|---|
| `nwc-core` | 协议、加密、发现、传输、同步引擎——平台无关，承载全部测试 |
| `nwc-win` | Windows 壳：托盘、剪贴板、对话框（纯 Win32，无框架） |
| `nwc-mac` | macOS 壳：菜单栏、NSPasteboard、窗口（纯 AppKit，无框架） |

开发与打包约定（同样写给 AI 协作者）见 [AGENTS.md](AGENTS.md)。
