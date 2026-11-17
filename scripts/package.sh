#!/usr/bin/env bash
# 本地打包脚本：产物与 .github/workflows/release.yml 一致（mac 打包步骤同源；
# Windows zip 在 CI 走 Compress-Archive、本地走 bsdtar，格式等价）。
#
# 用法（先自行完成构建，本脚本只做打包，不隐藏构建错误）：
#   cargo build --release -p nwc-mac                                # macOS（本机架构）
#   cargo build --release -p nwc-win --target x86_64-pc-windows-gnu # Windows 交叉编译
#   scripts/package.sh [TAG]     # TAG 缺省 = v{Cargo.toml 版本}
#
# 产物（dist/）：
#   NetWorkClipboard-$TAG-macos-<arch>.zip      裸二进制（终端调试用）
#   NetWorkClipboard-$TAG-macos-<arch>-app.zip  .app bundle（Finder 双击即用，推荐交付物）
#   NetWorkClipboard-$TAG-win-x64.zip           Windows exe
#
# 规则（见 AGENTS.md）：macOS 必须产出 .app bundle，不能只交裸二进制。

set -euo pipefail
cd "$(dirname "$0")/.."

TAG="${1:-}"
if [ -z "$TAG" ]; then
    VER=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
    TAG="v$VER"
fi
ARCH_VER="${TAG#v}"

mkdir -p dist

# 陈旧产物防护：二进制 mtime 早于最近一次源码变更 → 跳过该产物并警告
# （防旧版本残留混进新版本产物；只构建部分平台时其它残留不阻塞本地产物）
newest_src=$(find crates Cargo.toml Cargo.lock -type f -print0 2>/dev/null | xargs -0 stat -f '%m %N' 2>/dev/null | sort -rn | head -1 | cut -d' ' -f1)
check_fresh() { # $1 = 二进制路径；陈旧返回 1
    local bin_mtime
    bin_mtime=$(stat -f '%m' "$1")
    if [ -z "$newest_src" ] || [ "$bin_mtime" -lt "$newest_src" ]; then
        echo "警告：跳过陈旧构建 $1（早于最近一次源码变更，请重新构建）" >&2
        return 1
    fi
}

# ---- macOS：本机构建产物（target/release 或 target/<triple>/release）----
package_mac() { # $1 = 产物目录前缀, $2 = arch 名
    local bin_dir="$1" arch="$2"
    [ -x "$bin_dir/nwc-mac" ] || return 1
    check_fresh "$bin_dir/nwc-mac" || return 1
    tar -a -cf "dist/NetWorkClipboard-$TAG-macos-$arch.zip" -C "$bin_dir" nwc-mac
    # .app bundle（Finder 双击即用；裸二进制仅适合终端调试）
    local APP="NetWorkClipboard.app"
    rm -rf "$APP"
    mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
    cp "$bin_dir/nwc-mac" "$APP/Contents/MacOS/nwc-mac"
    cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleName</key><string>NetWorkClipboard</string>
  <key>CFBundleDisplayName</key><string>NetWorkClipboard</string>
  <key>CFBundleIdentifier</key><string>com.ezrealj.nwc</string>
  <key>CFBundleExecutable</key><string>nwc-mac</string>
  <key>CFBundleVersion</key><string>$ARCH_VER</string>
  <key>CFBundleShortVersionString</key><string>$ARCH_VER</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSUIElement</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>NetWorkClipboard 需要访问本地网络来发现并连接你的另一台电脑，同步剪贴板。</string>
</dict>
</plist>
PLIST
    codesign -s - --force "$APP" >/dev/null 2>&1 || true
    tar -a -cf "dist/NetWorkClipboard-$TAG-macos-$arch-app.zip" "$APP"
    rm -rf "$APP"
}

if [ -x "target/aarch64-apple-darwin/release/nwc-mac" ]; then
    package_mac "target/aarch64-apple-darwin/release" arm64
fi
if [ -x "target/x86_64-apple-darwin/release/nwc-mac" ]; then
    package_mac "target/x86_64-apple-darwin/release" x64 || true
fi
package_mac "target/release" "$(uname -m | sed 's/arm64/arm64/;s/x86_64/x64/')" || true

# ---- Windows：交叉编译产物 ----
if [ -x "target/x86_64-pc-windows-gnu/release/nwc.exe" ] && check_fresh "target/x86_64-pc-windows-gnu/release/nwc.exe"; then
    tar -a -cf "dist/NetWorkClipboard-$TAG-win-x64.zip" -C target/x86_64-pc-windows-gnu/release nwc.exe
fi

# ---- 校验 zip 格式（与 CI 相同：坏 zip 直接失败）----
python3 - <<'PY'
import glob, zipfile
files = sorted(glob.glob('dist/*.zip'))
assert files, 'no zip found in dist/'
for f in files:
    z = zipfile.ZipFile(f)
    bad = z.testzip()
    assert bad is None, f'{f}: corrupt at {bad}'
    print(f'{f} OK')
PY

ls -la dist/
