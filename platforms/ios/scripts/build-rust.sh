#!/bin/zsh
# 构建 Rust 引擎并放到 Xcode 工程用得到的位置：
#   Frameworks/ImeFFI.xcframework   静态库（真机 + 模拟器，arm64）和 C 头文件，模块名 ImeFFI
#   Resources/merged_lexicon.bin    完整词库（AIMELX03，先校验再拷贝）。只打进调试构建；Release 由 App 下载
#   Resources/fallback_lexicon.bin  基础词库（AOSP 词库编译，见 scripts/build-fallback-lexicon.sh）
# 产物都不进版本库。改了 Rust 代码或词库之后重跑本脚本，再构建 Xcode 工程。
#
# 用法：scripts/build-rust.sh [--no-llama]
#   默认连同进程内 llama.cpp 打分器一起构建，需要先有 third_party/llama 下的静态库
#   （仓库根目录的 scripts/build_llama.sh ios ios-sim）；--no-llama 则只构建解码器与规则重排。
#
# 环境变量：
#   IPHONEOS_DEPLOYMENT_TARGET   默认 26.0，要与 project.yml 里的 deploymentTarget 一致
#   IME_LEXICON                  词库文件，默认 <仓库>/data/compiled/merged_lexicon.bin
set -euo pipefail

IOS_DIR="${0:A:h:h}"
ROOT="${IOS_DIR:h:h}"
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-26.0}"
LEXICON="${IME_LEXICON:-$ROOT/data/compiled/merged_lexicon.bin}"
# 第 3 步从 $ROOT/target 取静态库。外面设了 CARGO_TARGET_DIR（或 cargo 配置里的 build.target-dir）时
# cargo 会把新库写到别处，打包的却是这里的旧库，所以固定下来
export CARGO_TARGET_DIR="$ROOT/target"

FEATURES=(--features llama)
for arg in "$@"; do
    case "$arg" in
        --no-llama) FEATURES=() ;;
        *) echo "未知参数: $arg" >&2; exit 2 ;;
    esac
done

if (( ${#FEATURES} )); then
    for slice in ios-arm64 ios-arm64-simulator; do
        if [[ ! -f "$ROOT/third_party/llama/$slice/lib/libime_llama.a" ]]; then
            echo "缺少 third_party/llama/$slice/lib/libime_llama.a。" >&2
            echo "先在仓库根目录运行 scripts/build_llama.sh ios ios-sim，或加 --no-llama。" >&2
            exit 1
        fi
    done
fi

echo "== 1/4 校验词库 $LEXICON"
if [[ ! -f "$LEXICON" ]]; then
    echo "找不到词库。先在仓库根目录运行 cargo run --release -p ime-lexicon --bin lexicon-builder" >&2
    exit 1
fi
cargo run --manifest-path "$ROOT/Cargo.toml" --release -q -p ime-lexicon --bin lexicon-builder -- --verify "$LEXICON"
mkdir -p "$IOS_DIR/Resources"
# 先拷成临时文件再改名：正在跑的模拟器键盘可能还映射着旧文件，不能原地改写
cp "$LEXICON" "$IOS_DIR/Resources/merged_lexicon.bin.tmp"
mv -f "$IOS_DIR/Resources/merged_lexicon.bin.tmp" "$IOS_DIR/Resources/merged_lexicon.bin"

echo "== 2/4 基础词库"
zsh "$IOS_DIR/scripts/build-fallback-lexicon.sh"

echo "== 3/4 构建静态库（iOS $IPHONEOS_DEPLOYMENT_TARGET，${FEATURES[*]:-不含 llama}）"
for target in aarch64-apple-ios aarch64-apple-ios-sim; do
    cargo build --manifest-path "$ROOT/Cargo.toml" --release -p ime-ffi --target "$target" "${FEATURES[@]}"
done

echo "== 4/4 打包 ImeFFI.xcframework"
rm -rf "$IOS_DIR/Frameworks/ImeFFI.xcframework"
mkdir -p "$IOS_DIR/Frameworks"
xcodebuild -create-xcframework \
    -library "$ROOT/target/aarch64-apple-ios/release/libime_ffi.a" -headers "$ROOT/crates/ime-ffi/include" \
    -library "$ROOT/target/aarch64-apple-ios-sim/release/libime_ffi.a" -headers "$ROOT/crates/ime-ffi/include" \
    -output "$IOS_DIR/Frameworks/ImeFFI.xcframework" >/dev/null

du -sh "$IOS_DIR/Frameworks/ImeFFI.xcframework" "$IOS_DIR/Resources/merged_lexicon.bin" "$IOS_DIR/Resources/fallback_lexicon.bin"
echo "完成。接下来：cd platforms/ios && xcodegen generate，再构建 AIPinyin。"
