#!/usr/bin/env bash
# 构建固定版本的 llama.cpp 静态库（仅 CPU 后端），供 crates/ime-llm 的 `llama` 特性链接。
#
# 用法:
#   scripts/build_llama.sh [macos] [ios] [ios-sim] [xcframework]     # 不带参数 = 全部
#
# 环境变量:
#   LLAMA_CPP_SRC             已有的 llama.cpp 源码目录（只读取，不会修改）；未设置时从 GitHub 克隆并检出固定提交。
#                             应当是停在固定提交、工作区没有改动的 git 仓库，这样才能核对它就是固定的那个版本
#   LLAMA_CPP_ALLOW_UNPINNED  设为 1 时允许 LLAMA_CPP_SRC 不是固定提交，或工作区里有未提交的改动（仅用于试验）
#   IME_LLAMA_OUT             输出根目录，默认 <仓库>/third_party/llama
#   IME_LLAMA_IOS_MIN         iOS 最低系统版本，默认 16.4
#   IME_LLAMA_MACOS_MIN       macOS 最低系统版本，默认 13.3
#   IME_LLAMA_IOS_ARM_ARCH    iOS 真机/模拟器的 -march，默认 armv8.4-a+fp16（A13 / M1 及以上，含 dotprod）；
#                             只面向 A15 及以上机型时可改为 armv8.6-a+fp16 以启用 i8mm 内核
#   IME_LLAMA_JOBS            并行编译任务数，默认 = 逻辑核心数
#
# 输出（均在 third_party/llama/ 下，该目录已加入 .gitignore）:
#   include/                          llama.h 与它引用的 ggml 头文件
#   macos-arm64/lib/libime_llama.a    macOS arm64：llama + ggml + ggml-base + ggml-cpu 合并成的单个静态库
#   ios-arm64/lib/libime_llama.a      iOS 真机 arm64
#   ios-arm64-simulator/lib/libime_llama.a   iOS 模拟器 arm64
#   ImeLlama.xcframework              上面两个 iOS 静态库 + 头文件，Xcode 工程直接引用
#   PINNED_COMMIT                     third_party/llama/src 里那份源码的来历，只有核对过的才记提交号：
#                                       <固定提交号>          克隆并检出的，或来自停在固定提交、工作区干净的 git 仓库
#                                       <提交号>              LLAMA_CPP_ALLOW_UNPINNED=1 时用的其它提交（工作区干净）
#                                       <提交号>-dirty        LLAMA_CPP_ALLOW_UNPINNED=1 时用的、带未提交改动的工作区
#                                       unverified            LLAMA_CPP_SRC 不是 git 仓库，无法核对
#                                     内容不是固定提交号时，下次运行会重新准备源码（重新复制或克隆）
#
# 构建配置（与 iPhone 键盘扩展内存实测结论一致）:
#   - 仅 CPU：GGML_METAL=OFF（加载 Metal 后端本身就多占约 90 MiB）、GGML_BLAS=OFF、GGML_ACCELERATE=OFF、GGML_OPENMP=OFF
#   - 不构建 tools / examples / tests / server / common / mtmd
#   - 静态库，Release，无调试符号
set -euo pipefail

PIN_COMMIT="0d9eacadf2e19ff5d49df29800afc88a8e844123"
PIN_REPO="https://github.com/ggml-org/llama.cpp.git"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${IME_LLAMA_OUT:-$REPO_ROOT/third_party/llama}"
SRC="$OUT/src"
IOS_MIN="${IME_LLAMA_IOS_MIN:-16.4}"
MACOS_MIN="${IME_LLAMA_MACOS_MIN:-13.3}"
IOS_ARM_ARCH="${IME_LLAMA_IOS_ARM_ARCH:-armv8.4-a+fp16}"
JOBS="${IME_LLAMA_JOBS:-$(sysctl -n hw.logicalcpu 2>/dev/null || echo 4)}"

if [[ $# -eq 0 ]]; then
    TARGETS=(macos ios ios-sim xcframework)
else
    TARGETS=("$@")
fi
for t in "${TARGETS[@]}"; do
    case "$t" in
        macos|ios|ios-sim|xcframework) ;;
        *) echo "未知目标: $t（可选: macos ios ios-sim xcframework）" >&2; exit 2 ;;
    esac
done

command -v cmake >/dev/null || { echo "需要 cmake" >&2; exit 1; }
if command -v ninja >/dev/null; then
    GENERATOR="Ninja"
else
    GENERATOR="Unix Makefiles"
fi

# ---------------------------------------------------------------- 源码
# 清空源码目录。先删掉 PINNED_COMMIT：复制或克隆到一半被打断时，不能留下一个「已核对」的记录配一份不完整的源码
reset_source_dir() {
    rm -f "$OUT/PINNED_COMMIT"
    rm -rf "$SRC"
    mkdir -p "$SRC"
}

prepare_source() {
    if [[ -f "$OUT/PINNED_COMMIT" && -f "$SRC/CMakeLists.txt" ]] \
        && [[ "$(cat "$OUT/PINNED_COMMIT")" == "$PIN_COMMIT" ]]; then
        echo "==> 源码已就绪: $SRC ($PIN_COMMIT)"
        return
    fi
    # 写进 PINNED_COMMIT 的内容：只有核对过的源码才记提交号
    local recorded="$PIN_COMMIT"
    if [[ -n "${LLAMA_CPP_SRC:-}" ]]; then
        [[ -f "$LLAMA_CPP_SRC/CMakeLists.txt" ]] || { echo "LLAMA_CPP_SRC 不是 llama.cpp 源码目录: $LLAMA_CPP_SRC" >&2; exit 1; }
        local src_real toplevel
        src_real="$(cd "$LLAMA_CPP_SRC" && pwd -P)"
        # 必须是仓库的根目录：别的仓库里的一个子目录（比如解压出来的源码包）用 rev-parse 查到的是外层仓库的提交
        toplevel="$(git -C "$LLAMA_CPP_SRC" rev-parse --show-toplevel 2>/dev/null || true)"
        if [[ -n "$toplevel" && "$(cd "$toplevel" && pwd -P)" == "$src_real" ]] \
            && git -C "$LLAMA_CPP_SRC" rev-parse --verify -q HEAD >/dev/null 2>&1; then
            local actual dirty
            actual="$(git -C "$LLAMA_CPP_SRC" rev-parse HEAD)"
            if [[ "$actual" != "$PIN_COMMIT" && "${LLAMA_CPP_ALLOW_UNPINNED:-0}" != "1" ]]; then
                echo "LLAMA_CPP_SRC 的提交 $actual 与固定提交 $PIN_COMMIT 不一致（设 LLAMA_CPP_ALLOW_UNPINNED=1 可跳过检查）" >&2
                exit 1
            fi
            # 提交号对得上还不够：工作区里被跟踪的文件有改动时，构建的就不是那个提交
            dirty="$(git -C "$LLAMA_CPP_SRC" status --porcelain --untracked-files=no)"
            recorded="$actual"
            if [[ -n "$dirty" ]]; then
                if [[ "${LLAMA_CPP_ALLOW_UNPINNED:-0}" != "1" ]]; then
                    echo "LLAMA_CPP_SRC 的工作区有未提交的改动，构建出来的不是提交 $actual（设 LLAMA_CPP_ALLOW_UNPINNED=1 可跳过检查）:" >&2
                    echo "$dirty" | head -10 >&2
                    exit 1
                fi
                echo "警告: LLAMA_CPP_SRC 的工作区有未提交的改动，PINNED_COMMIT 记为 $actual-dirty" >&2
                recorded="$actual-dirty"
            fi
        else
            echo "警告: LLAMA_CPP_SRC 不是 git 仓库（的根目录），无法核对提交号，PINNED_COMMIT 记为 unverified" >&2
            recorded="unverified"
        fi
        reset_source_dir
        echo "==> 从 $LLAMA_CPP_SRC 复制源码（只读取，不修改原目录）"
        # 只复制构建 libllama / ggml 需要的部分
        for item in CMakeLists.txt CMakePresets.json LICENSE licenses cmake include src ggml vendor common; do
            if [[ -e "$LLAMA_CPP_SRC/$item" ]]; then
                cp -R "$LLAMA_CPP_SRC/$item" "$SRC/"
            fi
        done
    else
        command -v git >/dev/null || { echo "需要 git 或设置 LLAMA_CPP_SRC" >&2; exit 1; }
        reset_source_dir
        echo "==> 克隆 $PIN_REPO 并检出 $PIN_COMMIT"
        git init -q "$SRC"
        git -C "$SRC" remote add origin "$PIN_REPO"
        git -C "$SRC" fetch -q --depth 1 origin "$PIN_COMMIT"
        git -C "$SRC" checkout -q FETCH_HEAD
    fi
    echo "$recorded" > "$OUT/PINNED_COMMIT"
}

# ---------------------------------------------------------------- 头文件
install_headers() {
    rm -rf "$OUT/include"
    mkdir -p "$OUT/include"
    cp "$SRC/include/llama.h" "$OUT/include/"
    for h in ggml.h ggml-cpu.h ggml-backend.h ggml-alloc.h ggml-opt.h gguf.h; do
        cp "$SRC/ggml/include/$h" "$OUT/include/"
    done
}

# ---------------------------------------------------------------- 单平台构建
# build_one <名称> <额外 cmake 参数...>
build_one() {
    local name="$1"; shift
    local bdir="$OUT/build/$name"
    local ldir="$OUT/$name/lib"
    echo "==> 构建 $name"
    cmake -S "$SRC" -B "$bdir" -G "$GENERATOR" \
        -DCMAKE_BUILD_TYPE=Release \
        -DBUILD_SHARED_LIBS=OFF \
        -DLLAMA_BUILD_APP=OFF \
        -DLLAMA_BUILD_COMMON=OFF \
        -DLLAMA_BUILD_TOOLS=OFF \
        -DLLAMA_BUILD_EXAMPLES=OFF \
        -DLLAMA_BUILD_TESTS=OFF \
        -DLLAMA_BUILD_SERVER=OFF \
        -DLLAMA_BUILD_MTMD=OFF \
        -DLLAMA_OPENSSL=OFF \
        -DGGML_METAL=OFF \
        -DGGML_BLAS=OFF \
        -DGGML_ACCELERATE=OFF \
        -DGGML_OPENMP=OFF \
        -DGGML_CCACHE=OFF \
        "$@" >"$bdir.configure.log" 2>&1 || { tail -40 "$bdir.configure.log" >&2; exit 1; }
    cmake --build "$bdir" --config Release -j "$JOBS" >"$bdir.build.log" 2>&1 || { tail -60 "$bdir.build.log" >&2; exit 1; }

    local libs=()
    for lib in libllama.a libggml.a libggml-base.a libggml-cpu.a; do
        local found
        found="$(find "$bdir" -name "$lib" -type f | head -1)"
        [[ -n "$found" ]] || { echo "未找到 $lib（见 $bdir.build.log）" >&2; exit 1; }
        libs+=("$found")
    done
    rm -rf "$ldir"
    mkdir -p "$ldir"
    # 合并为单个静态库，Xcode / build.rs 只需链接一个文件
    libtool -static -o "$ldir/libime_llama.a" "${libs[@]}" 2>/dev/null
    echo "    $(du -h "$ldir/libime_llama.a" | cut -f1)  $ldir/libime_llama.a"
}

build_macos() {
    # 本机构建：GGML_NATIVE 默认开启，按当前 CPU 选择指令集
    build_one macos-arm64 \
        -DCMAKE_OSX_ARCHITECTURES=arm64 \
        -DCMAKE_OSX_DEPLOYMENT_TARGET="$MACOS_MIN"
}

build_ios() {
    build_one ios-arm64 \
        -DCMAKE_SYSTEM_NAME=iOS \
        -DCMAKE_OSX_SYSROOT=iphoneos \
        -DCMAKE_OSX_ARCHITECTURES=arm64 \
        -DCMAKE_OSX_DEPLOYMENT_TARGET="$IOS_MIN" \
        -DGGML_NATIVE=OFF \
        -DGGML_CPU_ARM_ARCH="$IOS_ARM_ARCH"
}

build_ios_sim() {
    build_one ios-arm64-simulator \
        -DCMAKE_SYSTEM_NAME=iOS \
        -DCMAKE_OSX_SYSROOT=iphonesimulator \
        -DCMAKE_OSX_ARCHITECTURES=arm64 \
        -DCMAKE_OSX_DEPLOYMENT_TARGET="$IOS_MIN" \
        -DGGML_NATIVE=OFF \
        -DGGML_CPU_ARM_ARCH="$IOS_ARM_ARCH"
}

make_xcframework() {
    local dev="$OUT/ios-arm64/lib/libime_llama.a"
    local sim="$OUT/ios-arm64-simulator/lib/libime_llama.a"
    [[ -f "$dev" && -f "$sim" ]] || { echo "xcframework 需要先构建 ios 与 ios-sim" >&2; exit 1; }
    command -v xcodebuild >/dev/null || { echo "需要 xcodebuild" >&2; exit 1; }
    echo "==> 打包 ImeLlama.xcframework"
    rm -rf "$OUT/ImeLlama.xcframework"
    xcodebuild -create-xcframework \
        -library "$dev" -headers "$OUT/include" \
        -library "$sim" -headers "$OUT/include" \
        -output "$OUT/ImeLlama.xcframework" >/dev/null
    echo "    $(du -sh "$OUT/ImeLlama.xcframework" | cut -f1)  $OUT/ImeLlama.xcframework"
}

mkdir -p "$OUT/build"
prepare_source
install_headers
for t in "${TARGETS[@]}"; do
    case "$t" in
        macos) build_macos ;;
        ios) build_ios ;;
        ios-sim) build_ios_sim ;;
        xcframework) make_xcframework ;;
    esac
done
echo "==> 完成: $OUT"
