# NetWorkClipboard (nwc)

> 同一局域网内，两台电脑共享同一块剪贴板。

[![CI](https://github.com/EzrealJ/nwc/actions/workflows/ci.yml/badge.svg)](https://github.com/EzrealJ/nwc/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/EzrealJ/nwc)](https://github.com/EzrealJ/nwc/releases)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS-blue)

局域网剪贴板同步工具：在 Windows 上复制文字、截图或文件，macOS 上直接粘贴，反之亦然。托盘/菜单栏常驻，纯原生实现，无任何运行时依赖——Windows 单 exe 约 2MB、macOS 单进程，私有内存约 2MB。

## 特性

- **端到端加密** — scrypt 派生密钥 + AES-256-GCM，局域网上无明文
- **零配置起步** — 自动发现对端，两端口令一致即通；也支持手动指定
- **文字 / 图片 / 文件** — 大文件 256KB 分块流式传输，内存占用恒定
- **设备配对** — 可只与指定设备同步，公共局域网里防误传
- **剪贴板历史** — 文字/图片/文件三类型统一记录，可搜索，双击重新复制
- **收件箱中转** — 同名文件原位覆盖（文件名永远稳定），按天自动清理不膨胀
- **可靠传输** — 换网自愈、断线补发、回声抑制（不会同步给自己）
- **体验细节** — 传输进度实时显示，收文件点通知直达收件箱

## 快速开始

从 [Releases](https://github.com/EzrealJ/nwc/releases) 下载：

| 文件 | 平台 | 说明 |
|---|---|---|
| `NetWorkClipboard-v*-win-x64.zip` | Windows 10+ | 解压即用 `nwc.exe` |
| `NetWorkClipboard-v*-macos-arm64-app.zip` | macOS (Apple Silicon) | **推荐**，解压得 `.app`，双击即用 |
| `NetWorkClipboard-v*-macos-x64-app.zip` | macOS (Intel) | 同上 |
| `*-macos-*.zip`（不带 `-app`） | macOS | 裸二进制，终端调试用 |

1. 两端各装一份，启动后从菜单打开「设置…」，**口令设成一样**即可自动连上
2. macOS 未做公证：首次打开需右键 →「打开」，或 `xattr -cr NetWorkClipboard.app`

## 使用

```
nwc            # 托盘/菜单栏模式（默认）
nwc --cli      # 控制台模式（日志直出，调试用）
```

托盘/菜单栏菜单：暂停同步、历史记录、打开收件箱、已知对端（配对）、设置、开机自启。

配置文件 `~/.networkclipboard/config.json`，两端设置窗口已覆盖常用项（合并写，保存自动重启）：

| 配置项 | 默认 | 说明 |
|---|---|---|
| `token` | — | 共享口令，两端必须一致 |
| `port` | `8767` | 监听/连接端口 |
| `name` | 主机名 | 对端显示的设备名 |
| `role` | `auto` | `auto` / `server`（只监听）/ `client`（只连接） |
| `inboxDir` | `~/NetWorkClipboard/inbox` | 收件目录（中转缓存，非归档） |
| `inboxKeepDays` | `14` | 收件保留天数，`0` 关闭自动清理 |
| `inboxRenameOnConflict` | `false` | 同名文件冲突时保留副本改名（默认原位覆盖） |
| `historyEnabled` / `historyLimit` | `true` / `20` | 剪贴板历史开关与条数上限 |

## 构建

需要 Rust stable（rustup 安装，MSVC 或 GNU 工具链均可）：

```bash
cargo build --release -p nwc-mac                                  # macOS
cargo build --release -p nwc-win --target x86_64-pc-windows-gnu   # Windows 交叉编译
scripts/package.sh                                                # 本地打包（产物进 dist/）
```

> Windows 交叉编译需 `rustup target add x86_64-pc-windows-gnu` + `brew install mingw-w64`；`windows` crate 固定在 0.57（0.58+ 在 windows-gnu 下有符号冲突，勿盲目升级）。

## 测试

```bash
cargo test -p nwc-core                     # 单元 + 双引擎端到端（任意平台）
cargo test -p nwc-mac -- --test-threads=1  # macOS 真机（真实剪贴板读写）
cargo test -p nwc-win -- --test-threads=1  # Windows 真机
```

测试金字塔（CI 全矩阵执行）：单元（协议/加密/引擎/配置）→ 平台真机（剪贴板读写往返）→ 端到端（双引擎经真实 TCP 互发，字节级断言一致）。

## 排查

- **日志**：`~/.networkclipboard/logs/`，按天滚动保留 7 天；图片/文件未同步时日志会记录剪贴板实际格式与失败原因
- **连不上**：检查两端口令、系统防火墙、是否被 AP 隔离；工具会自动过滤虚拟网卡与 Clash fake-IP 网段（198.18.0.0/15）
- **粘贴不出图片**：工具在两端写入多种格式（Windows PNG+DIBV5，macOS TIFF+PNG），个别应用只认特定格式时请带日志反馈
- **重复内容不同步**：10 秒内复制相同内容会被回声抑制吞掉，属预期行为

## 结构

纯逻辑与平台壳分离，`nwc-core` 两端共享：

| crate | 职责 |
|---|---|
| `nwc-core` | 协议帧、加密、发现、传输、同步引擎、配置（平台无关，含全部测试） |
| `nwc-win` | Windows 壳：托盘、剪贴板（事件驱动）、设置对话框、开机自启 |
| `nwc-mac` | macOS 壳：菜单栏、NSPasteboard、LaunchAgent、历史/设置窗口 |

开发约定见 [AGENTS.md](AGENTS.md)。
