#!/usr/bin/env python3
"""把 AOSP 拼音输入法的词库（rawdict_utf16_65105_freq.txt，Apache-2.0）转成 RIME 词表格式。

iOS 键盘的「基础词库」用它：完整词库（雾凇拼音，GPL-3.0-only）不打进 App Store 的安装包，由 App 第一次
打开时下载；下完之前键盘用这个宽松协议的小词库打中文。

原文件每行：<词> <频率> <是否只在 GBK 里，0/1> <拼音，每个字一个音节，空格隔开>，UTF-16 编码。
输出每行：<词>\t<拼音，空格隔开>\t<权重>，权重 = 频率 × 100 取整（只用来排序）。
音节数和字数对不上、拼音里有非字母的行跳过，并在最后报告跳过了多少。

用法：python3 -I tools/lexicon/aosp_to_rime.py <rawdict_utf16_65105_freq.txt> <输出.dict.yaml>
"""
import sys


def convert(lines):
    entries, skipped = [], 0
    for line in lines:
        parts = line.split()
        if len(parts) < 4:
            if line.strip():
                skipped += 1
            continue
        word, freq, _gbk, syllables = parts[0], parts[1], parts[2], parts[3:]
        try:
            weight = max(1, round(float(freq) * 100))
        except ValueError:
            skipped += 1
            continue
        syllables = [s.lower() for s in syllables]
        if len(syllables) != len(word) or not all(s.isascii() and s.isalpha() for s in syllables):
            skipped += 1
            continue
        entries.append((word, " ".join(syllables), weight))
    return entries, skipped


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    source, target = sys.argv[1], sys.argv[2]
    with open(source, encoding="utf-16") as f:
        entries, skipped = convert(f)
    # 同一个词同一个读音只留权重最高的一条
    best = {}
    for word, pinyin, weight in entries:
        key = (word, pinyin)
        if weight > best.get(key, 0):
            best[key] = weight
    with open(target, "w", encoding="utf-8") as out:
        out.write("# Rime dictionary - aosp_pinyin（由 tools/lexicon/aosp_to_rime.py 生成，勿手改）\n")
        out.write("# 来源：AOSP PinyinIME jni/data/rawdict_utf16_65105_freq.txt\n")
        out.write("# Copyright (C) 2009 The Android Open Source Project，Apache License 2.0（见同目录 NOTICE）\n")
        out.write("---\nname: aosp_pinyin\nversion: \"2009\"\nsort: by_weight\n...\n")
        for (word, pinyin), weight in sorted(best.items(), key=lambda kv: (-kv[1], kv[0])):
            out.write(f"{word}\t{pinyin}\t{weight}\n")
    print(f"写出 {len(best)} 条（原文件 {len(entries)} 条有效，跳过 {skipped} 行）→ {target}")


if __name__ == "__main__":
    main()
