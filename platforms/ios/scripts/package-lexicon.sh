#!/bin/zsh
# 把完整词库打成 App 首次启动时下载的发布包。
#
# 雾凇拼音（GPL-3.0-only）编译出的词库不打进 App Store 的安装包，由 App 第一次打开时下载
# （见 Shared/LexiconCatalog.swift）。本脚本：
# 1. 先校验词库（lexicon-builder --verify）；
# 2. 用系统自带的 compression_tool 压成 xz（LZMA）：58 MB 压到约 12 MB。用系统的编码器，
#    保证 iOS 上的 Compression 框架能解开（标准 xz 工具也能解）；
# 3. 再解压回来逐字节比对；
# 4. 写出 manifest.json，并打印要抄进 LexiconCatalog.swift 的几行；
# 5. 打一个源码包（编译用的全部词表、编译工具 crates/ime-lexicon 的源码、GPL-3.0 全文和说明），和压缩包一起上传：
#    词库是 GPL 数据编译成的，发布它时要附上对应的源码（GPLv3 第 6 节）。源码包自成一体，不依赖仓库里还没推送的代码；
# 6. 把源码包解到临时目录、照说明重新编译一遍（离线，依赖用本机的 cargo 缓存），核对 SHA-256 和发布的文件一致。
#    设 SKIP_SOURCE_CHECK=1 可以跳过这一步。
#
# 产物在 build/lexicon-release/<版本>/，不进版本库。上传到 GitHub Release（标签 lexicon-<版本>）之后，
# 下载地址才能用。
#
# 用法：scripts/package-lexicon.sh <版本，如 2026.11.12；同一天第二版写 2026.11.12.2>
# 环境变量：IME_LEXICON  词库文件，默认 <仓库>/data/compiled/merged_lexicon.bin
set -euo pipefail

IOS_DIR="${0:A:h:h}"
ROOT="${IOS_DIR:h:h}"
VERSION="${1:-}"
if [[ ! "$VERSION" =~ '^[0-9]{4}\.[0-9]{1,2}\.[0-9]{1,2}(\.[0-9]+)?$' ]]; then
    echo "用法：scripts/package-lexicon.sh <版本，形如 2026.11.12 或 2026.11.12.2>" >&2
    exit 2
fi
LEXICON="${IME_LEXICON:-$ROOT/data/compiled/merged_lexicon.bin}"
OUT="$IOS_DIR/build/lexicon-release/$VERSION"
NAME="merged_lexicon-$VERSION.bin"
ARCHIVE="$OUT/$NAME.xz"

echo "== 1/6 校验词库 $LEXICON"
[[ -f "$LEXICON" ]] || { echo "找不到词库。先在仓库根目录运行 cargo run --release -p ime-lexicon --bin lexicon-builder" >&2; exit 1; }
cargo run --manifest-path "$ROOT/Cargo.toml" --release -q -p ime-lexicon --bin lexicon-builder -- --verify "$LEXICON"

echo "== 2/6 压缩"
rm -rf "$OUT"
mkdir -p "$OUT"
compression_tool -encode -a lzma -i "$LEXICON" -o "$ARCHIVE"

echo "== 3/6 解压回来比对"
ROUNDTRIP="$OUT/.roundtrip.bin"
compression_tool -decode -a lzma -i "$ARCHIVE" -o "$ROUNDTRIP"
cmp "$ROUNDTRIP" "$LEXICON"
rm -f "$ROUNDTRIP"

echo "== 4/6 写 manifest.json"
bytes=$(stat -f %z "$ARCHIVE")
sha=$(shasum -a 256 "$ARCHIVE" | cut -d' ' -f1)
installed_bytes=$(stat -f %z "$LEXICON")
installed_sha=$(shasum -a 256 "$LEXICON" | cut -d' ' -f1)
cat > "$OUT/manifest.json" <<EOF
{
  "version": "$VERSION",
  "file": "$NAME.xz",
  "bytes": $bytes,
  "sha256": "$sha",
  "installedFile": "$NAME",
  "installedBytes": $installed_bytes,
  "installedSHA256": "$installed_sha"
}
EOF
cat "$OUT/manifest.json"

echo "== 5/6 源码包"
SOURCES="$OUT/$NAME-sources"
mkdir -p "$SOURCES/data/seed" "$SOURCES/data/raw" "$SOURCES/data/compiled" "$SOURCES/crates/ime-lexicon"
cp "$ROOT"/data/seed/* "$SOURCES/data/seed/"
cp "$ROOT"/data/raw/*.dict.yaml "$SOURCES/data/raw/"
cp "$ROOT/LICENSE" "$SOURCES/LICENSE"
cp "$ROOT/Cargo.lock" "$SOURCES/Cargo.lock"
cp "$ROOT/crates/ime-lexicon/Cargo.toml" "$SOURCES/crates/ime-lexicon/"
cp -R "$ROOT/crates/ime-lexicon/src" "$SOURCES/crates/ime-lexicon/src"
# 只含 ime-lexicon 的工作区：版本、依赖、编译选项和仓库根目录的 Cargo.toml 一致
cat > "$SOURCES/Cargo.toml" <<'EOF'
[workspace]
resolver = "2"
members = ["crates/ime-lexicon"]

[workspace.package]
version = "0.2.0"
edition = "2021"
license = "GPL-3.0-or-later"
repository = "https://github.com/hellomyworkid-tech/rust-ai-pinyin-ime"

[workspace.dependencies]
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
memmap2 = "0.9"
thiserror = "1.0"

[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
EOF
cat > "$SOURCES/SOURCES.md" <<EOF
# $NAME 的源码

$NAME（AI 拼音 iOS 键盘的完整词库，AIMELX03 格式）由这里的词表编译而成：

- data/raw/rime_ice_base.dict.yaml、rime_ice_8105.dict.yaml、melt_eng_full.dict.yaml：
  雾凇拼音 https://github.com/iDvel/rime-ice ，GPL-3.0-only（各文件开头写着来源和版本）
- data/seed/：AI 拼音项目自己的种子词表和二元语法表

编译工具是 crates/ime-lexicon（lexicon-builder，来自 AI 拼音仓库
https://github.com/hellomyworkid-tech/rust-ai-pinyin-ime ，GPL-3.0-or-later），源码就在这个包里。
在这个目录下运行（需要 Rust）：

    cargo run --release -p ime-lexicon --bin lexicon-builder

结果在 data/compiled/merged_lexicon.bin，应与发布的文件逐字节相同：$installed_bytes 字节，SHA-256 $installed_sha。

协议：GPL-3.0（见 LICENSE）。
EOF
# 不带 macOS 的扩展属性和 ._ 文件：别的系统解开后，._*.dict.yaml 会被编译工具当成词表读，然后报错
COPYFILE_DISABLE=1 tar --no-mac-metadata --no-xattrs -C "$OUT" -cJf "$OUT/$NAME-sources.tar.xz" "$NAME-sources"
rm -rf "$SOURCES"

if [[ "${SKIP_SOURCE_CHECK:-}" != 1 ]]; then
    echo "== 6/6 用源码包重新编译核对"
    CHECK="$OUT/.source-check"
    rm -rf "$CHECK"
    mkdir -p "$CHECK"
    tar -C "$CHECK" -xJf "$OUT/$NAME-sources.tar.xz"
    (cd "$CHECK/$NAME-sources" && CARGO_TARGET_DIR="$CHECK/target" cargo run --release -q --offline -p ime-lexicon --bin lexicon-builder >/dev/null)
    rebuilt=$(shasum -a 256 "$CHECK/$NAME-sources/data/compiled/merged_lexicon.bin" | cut -d' ' -f1)
    if [[ "$rebuilt" != "$installed_sha" ]]; then
        echo "源码包重新编译出的词库 SHA-256 是 $rebuilt，和要发布的 $installed_sha 不一样" >&2
        exit 1
    fi
    echo "源码包重新编译出的词库和发布的文件一致"
    rm -rf "$CHECK"
fi
ls -la "$OUT"

echo
echo "抄进 Shared/LexiconCatalog.swift："
echo "    version: \"$VERSION\","
echo "    bytes: $bytes,"
echo "    sha256: \"$sha\","
echo "    installedBytes: $installed_bytes,"
echo "    installedSHA256: \"$installed_sha\","
echo
echo "发布：在 GitHub 上建标签 lexicon-$VERSION 的 Release，上传 $ARCHIVE 和 $OUT/$NAME-sources.tar.xz"
