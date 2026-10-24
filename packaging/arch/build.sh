#!/usr/bin/env bash
# 从当前工作树一键打出 openuuyc-clansty 的 Arch 包（.pkg.tar.zst）。
#
# 干的事就是替你把 PKGBUILD 需要的环境变量摆好再调 makepkg：源码直接用工作树
# （不 clone、不重新拉依赖），默认复用工作树的 target/ 增量编译。
#
#   ./packaging/arch/build.sh          出包，产物在 target/arch/
#   ./packaging/arch/build.sh -i       出包并装上
#   ./packaging/arch/build.sh -I -s    另起目录全量编译，缺的依赖自动装
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

info() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m警告:\033[0m %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
用法: packaging/arch/build.sh [选项]

  -i, --install    打出后立刻 sudo pacman -U 装上
  -s, --syncdeps   缺构建依赖时用 sudo pacman -S 补上（透传给 makepkg）
  -n, --noconfirm  不向 makepkg 确认任何事，CI 里必须带（否则装依赖那步会等人）
  -I, --isolated   不碰工作树的 target/，另起目录全量编译（很慢，第一次要
                   一二十分钟）；默认复用工作树的 target/ 做增量
  -h, --help       显示本帮助

环境变量（已设置时按你的值走）:
  BUILDDIR           makepkg 的工作目录，默认 $XDG_CACHE_HOME/openuuyc-arch
  CARGO_TARGET_DIR   编译产物目录，默认 <仓库>/target
  PKGDEST            包输出目录，默认 <仓库>/target/arch

本脚本打的是当前工作树的内容。要从远端 clansty 分支打，别用它，
直接 cd packaging/arch && makepkg -f（那时源码由 makepkg 自己 clone）。
EOF
}

install_pkg=false
isolated=false
makepkg_flags=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    -i | --install) install_pkg=true ;;
    -s | --syncdeps) makepkg_flags+=(--syncdeps) ;;
    -n | --noconfirm) makepkg_flags+=(--noconfirm) ;;
    -I | --isolated) isolated=true ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) die "不认识的选项：$1（--help 看用法）" ;;
  esac
  shift
done

# makepkg 自己会拒绝 root，但先说清楚更好：只有安装那一步需要 sudo。
[[ ${EUID:-$(id -u)} -ne 0 ]] || die "makepkg 不能以 root 运行，请用普通用户跑这个脚本"
command -v makepkg >/dev/null 2>&1 ||
  die "找不到 makepkg，先装 base-devel：sudo pacman -S --needed base-devel"
[[ -f ${SCRIPT_DIR}/PKGBUILD ]] || die "找不到 ${SCRIPT_DIR}/PKGBUILD"
[[ -f ${ROOT}/Cargo.toml ]] || die "${ROOT} 不像是 openuuyc 仓库"

BUILDDIR="$(realpath -m "${BUILDDIR:-${XDG_CACHE_HOME:-$HOME/.cache}/openuuyc-arch}")"
# makepkg 会在 BUILDDIR 里建 pkg/ 并把它设成 0111；一旦落在仓库里，cargo 遍历
# 工作树时会撞上 "Permission denied (os error 13)" 把构建打断，所以挡在前面。
case "${BUILDDIR}/" in
  "${ROOT}/"*) die "BUILDDIR 不能在仓库里（${ROOT}）：makepkg 建的 pkg/ 会让 cargo 报权限错" ;;
esac

if [[ -z ${CARGO_TARGET_DIR:-} ]]; then
  if $isolated; then
    # 独立目录是为了不和主仓库里正在跑的 cargo 抢 target/ 的锁，代价是全量重编一遍。
    CARGO_TARGET_DIR="${BUILDDIR}/target"
  else
    CARGO_TARGET_DIR="${ROOT}/target"
  fi
fi
CARGO_TARGET_DIR="$(realpath -m "$CARGO_TARGET_DIR")"
PKGDEST="$(realpath -m "${PKGDEST:-${ROOT}/target/arch}")"
mkdir -p "$BUILDDIR" "$PKGDEST"

pkgbuild_clean=false
if command -v git >/dev/null 2>&1 && git -C "$ROOT" rev-parse --git-dir >/dev/null 2>&1; then
  head_line="$(git -C "$ROOT" log -1 --format='%h %s')"
  # 只看已跟踪文件：.claude/、本脚本这类未跟踪的东西不会进包，报出来只是噪音。
  if [[ -n $(git -C "$ROOT" status --porcelain --untracked-files=no) ]]; then
    warn "工作树有未提交改动，它们会被一起打进包里（HEAD: ${head_line}）"
  else
    info "HEAD: ${head_line}"
  fi
  git -C "$ROOT" diff --quiet HEAD -- "${SCRIPT_DIR}/PKGBUILD" && pkgbuild_clean=true
fi

export BUILDDIR CARGO_TARGET_DIR PKGDEST OPENUUYC_LOCAL_SRC="$ROOT"

# 出的是给别人装的包，编译标志必须钉死：环境里只要残留一句 -C target-cpu=native
# （跑性能对比时很常见），rustc 就会把 AVX-512 / AVX-VNNI 编进连运行时派发都没有的
# 通用路径，包只能在构建机那一代 CPU 上跑。这里清一遍只是防外面传进来的：
# /etc/makepkg.conf.d/rust.conf 会在 makepkg 启动时重新 export 它自己的 RUSTFLAGS，
# 真起作用的那次覆盖在 PKGBUILD 的 build() 里。
if [[ -n ${RUSTFLAGS:-}${CARGO_ENCODED_RUSTFLAGS:-}${CARGO_BUILD_RUSTFLAGS:-} ]]; then
  warn "忽略继承来的编译标志（${RUSTFLAGS:-}${CARGO_ENCODED_RUSTFLAGS:-}${CARGO_BUILD_RUSTFLAGS:-}）"
fi
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_RUSTFLAGS

cd "$SCRIPT_DIR"

info "开始打包（BUILDDIR=${BUILDDIR}）"
$isolated && info "isolated：全量编译到 ${CARGO_TARGET_DIR}，第一次会比较久"
makepkg --force "${makepkg_flags[@]}"

# 产物名里的 .r<提交数>.g<hash> 要等 makepkg 建好 $srcdir 才对得出 pkgver()，
# 所以事前算不准，直接取刚写进去的那个（mtime 最新）。
pkg="$(ls -t "$PKGDEST"/openuuyc-clansty-*.pkg.tar.zst 2>/dev/null | head -n1 || true)"
[[ -n $pkg && -f $pkg ]] || die "makepkg 跑完了，却没在 ${PKGDEST} 里找到包"

# makepkg 会把 pkgver() 算出的版本写回 PKGBUILD（VCS 源的既定行为，--holdver 也拦不住），
# 于是每打一次包仓库里就多一处 diff。构建前这文件是干净的，这里顺手还原掉。
if $pkgbuild_clean && ! git -C "$ROOT" diff --quiet HEAD -- "${SCRIPT_DIR}/PKGBUILD"; then
  git -C "$ROOT" checkout -- "${SCRIPT_DIR}/PKGBUILD"
  info "已还原 makepkg 写回的 pkgver（PKGBUILD）"
fi

( cd "$(dirname "$pkg")" && sha256sum "$(basename "$pkg")" >"$(basename "$pkg").sha256" )

if $install_pkg; then
  info "安装 $(basename "$pkg")"
  sudo pacman -U "$pkg"
fi

info "产物：$pkg"
ls -lh "$pkg"
cat "${pkg}.sha256"
info "安装：sudo pacman -U $(basename "$pkg")"
