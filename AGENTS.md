# AGENTS.md — nwc (NetWorkClipboard)

局域网剪贴板同步小工具：Rust workspace（`crates/nwc-core` 纯逻辑 / `nwc-win` Windows 壳 / `nwc-mac` macOS 壳）。
协议与功能细节见 `README.md`；本文件只写**硬规则与工作流约定**。

## 构建与打包（硬规则）

1. **macOS 产物必须打 `.app` bundle，不能只交裸二进制**。统一用 `scripts/package.sh`（产物与 CI 一致），交付物是 `dist/NetWorkClipboard-v*-macos-<arch>-app.zip`（Finder 双击即用）；裸二进制 zip 仅作终端调试附带。
2. Windows 产物交 `dist/NetWorkClipboard-v*-win-x64.zip`（`nwc.exe`，解压即用）。
3. 本地打包流程 = 先构建后打包，两步分开跑（脚本只打包，不隐藏构建错误）：
   ```bash
   cargo build --release -p nwc-mac                                # macOS 本机
   cargo build --release -p nwc-win --target x86_64-pc-windows-gnu # Windows 交叉编译
   scripts/package.sh                    # TAG 缺省取 Cargo.toml 版本
   ```
4. 所有 zip 必须过 `package.sh` 内置的 `zipfile.testzip` 校验（CI 同款，坏 zip 直接失败）。
5. 交叉编译 Windows 需先 `rustup target add x86_64-pc-windows-gnu` + `brew install mingw-w64`；`windows` crate 固定 0.57（0.58+ 在 windows-gnu 上有符号冲突，勿升，见 README）。

## 测试（先于提交）

- 行为改动**测试先行**：先写会失败的单测锁行为，再实现，`cargo test -p nwc-core` 全绿才算完成。
- 平台真机测试：`cargo test -p nwc-mac -- --test-threads=1`（mac 上必跑）；Windows 剪贴板真机测试只能在 Windows 上跑，本机用交叉编译确认编译通过即可。
- 涉及两端壳层 UI 的改动，交叉编译 `nwc-win` + 本机编译 `nwc-mac` 都要过。

## CI / 发版（省公共资源）

- **推送才触发 CI**：PR 跑全矩阵；main 仅代码变更触发；纯文档不触发。本地提交不消耗资源。
- **发版 = 一次性**：v0.4.0 起的约定——整个迭代本地攒提交、全部测试绿后，一次 `git push` + 一个 `v*` 标签触发 release workflow，绝不反复推标签试打包。打包逻辑本地先用 `scripts/package.sh` 验证过再推。
- release workflow 出三端产物前会先跑同一套测试；本地已验证过的东西不要靠 CI 再验一遍。

## 其它约定

- 壳层 UI 直接用平台原生控件（Win32 / objc2-AppKit）；objc2 协议绑定复杂时（dataSource/delegate 类）可用 `msg_send` 动态挂接 + `define_class!` 普通方法，AppKit 运行时只看 respondsToSelector。
- 事件泵线程不碰 AppKit：需要主线程做的事（如弹通知）入队共享状态，由 300ms tick 在主线程取走处理。
- 根目录构建残留（`NetWorkClipboard.app/`）不入库（已在 .gitignore）。
