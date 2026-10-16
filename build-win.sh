#!/bin/sh
# 在 Linux 上交叉编译一个原生 Windows EXE，用来做真机测试。
#
# 产物没有 cargo dist 的自解压 launcher 外壳、没有签名，装包和自更新流程仍然只能在
# Windows 上跑完整的 `cargo dist`。这一步踩过的坑记在 AGENTS_QA.md 里。
#
# 用法：./build-win.sh [--debug]     默认 release
set -eu

cd "$(dirname "$0")"

TARGET=x86_64-pc-windows-msvc
PROFILE_DIR=release
BUILD_ARGS=--release
if [ "${1:-}" = "--debug" ]; then
    PROFILE_DIR=debug
    BUILD_ARGS=
fi

missing=
command -v cargo-xwin >/dev/null 2>&1 || missing="$missing cargo-xwin( cargo install cargo-xwin --locked )"
for tool in clang-cl lld-link llvm-lib; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool(LLVM)"
done
WINDRES=
for candidate in /opt/llvm-mingw/bin/x86_64-w64-mingw32-windres \
                 "$(command -v x86_64-w64-mingw32-windres 2>/dev/null || true)" \
                 "$(command -v llvm-windres 2>/dev/null || true)"; do
    if [ -n "$candidate" ] && [ -x "$candidate" ]; then
        WINDRES=$candidate
        break
    fi
done
[ -n "$WINDRES" ] || missing="$missing windres(llvm-mingw 或 LLVM)"
if [ -n "$missing" ]; then
    echo "缺少构建依赖：$missing" >&2
    exit 1
fi

rustup target list --installed 2>/dev/null | grep -qx "$TARGET" \
    || echo "提示：还没装 $TARGET，先跑 rustup target add $TARGET" >&2

# build.rs 里的 embed-resource 在非 Windows 主机上固定走 llvm-rc，还会把工作目录切到
# rc 文件所在的 assets/，于是 rc 里的 "assets/icon.ico" 被解析成 assets/assets/icon.ico。
# 让它以为自己在用 GNU windres —— windres 分支不切工作目录，路径就对了。
WRAP_DIR=$(mktemp -d)
trap 'rm -rf "$WRAP_DIR"' EXIT
cat > "$WRAP_DIR/rcwrap" <<EOF
#!/bin/sh
for arg in "\$@"; do
    if [ "\$arg" = "-V" ]; then
        echo "GNU windres (GNU Binutils) 2.40"
        exit 0
    fi
done
exec $WINDRES "\$@"
EOF
chmod +x "$WRAP_DIR/rcwrap"

echo "交叉编译 $TARGET（$PROFILE_DIR）…"
RC="$WRAP_DIR/rcwrap" cargo xwin build --target "$TARGET" --locked $BUILD_ARGS

BIN="target/$TARGET/$PROFILE_DIR/OpenUUYC.exe"
echo
ls -lh "$BIN"
echo "产物：$(pwd)/$BIN"
