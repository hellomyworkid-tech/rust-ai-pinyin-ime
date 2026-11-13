#!/bin/zsh
# 生成键盘的基础词库 Resources/fallback_lexicon.bin（宽松协议的小词库，不进版本库）。
#
# 完整词库来自雾凇拼音（GPL-3.0-only），不打进 App Store 的安装包，由 App 第一次打开时下载
# （见 Shared/LexiconCatalog.swift）。下完之前键盘用这个基础词库打中文（见 Keyboard/Engine/EngineFactory.swift）。
#
# 数据：data/fallback/aosp_pinyin.dict.yaml，由 AOSP 拼音输入法的词库 rawdict_utf16_65105_freq.txt
# （Apache-2.0，https://android.googlesource.com/platform/packages/inputmethods/PinyinIME 的 jni/data）
# 经 tools/lexicon/aosp_to_rime.py 转换而来。
#
# 用法：scripts/build-fallback-lexicon.sh [rawdict_utf16_65105_freq.txt]
#   给了原始文件就先转换（覆盖 data/fallback/aosp_pinyin.dict.yaml），再编译。
# 第一次生成之后，要在 project.yml 的 AIPinyinKeyboard 资源里加上 Resources/fallback_lexicon.bin
# （Xcode 拷贝不存在的资源会报错，所以没有预先写上），再 xcodegen generate。
set -euo pipefail

IOS_DIR="${0:A:h:h}"
ROOT="${IOS_DIR:h:h}"
DICT="$ROOT/data/fallback/aosp_pinyin.dict.yaml"
OUT="$IOS_DIR/Resources/fallback_lexicon.bin"

if (( $# >= 1 )); then
    mkdir -p "${DICT:h}"
    python3 -I "$ROOT/tools/lexicon/aosp_to_rime.py" "$1" "$DICT"
fi
if [[ ! -f "$DICT" ]]; then
    echo "没有 $DICT。先下载 AOSP 词库，再运行：scripts/build-fallback-lexicon.sh <rawdict_utf16_65105_freq.txt>" >&2
    exit 1
fi

mkdir -p "$IOS_DIR/Resources"
# 先写临时文件再改名：正在跑的模拟器键盘可能还映射着旧文件
cargo run --manifest-path "$ROOT/Cargo.toml" --release -q -p ime-lexicon --bin lexicon-builder -- \
    --build "$OUT.tmp" "$DICT"
mv -f "$OUT.tmp" "$OUT"
du -h "$OUT"
