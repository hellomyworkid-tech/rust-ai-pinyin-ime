#!/usr/bin/env python3
"""从 rime-ice 基础词库生成 tools/eval 的两份对照用例（输出已随仓库提供，词库更新后可用本脚本重新生成）。

用法（在仓库根目录）:
    python3 tools/eval/gen_control_cases.py data/raw/rime_ice_base.dict.yaml data/eval [每份用例数=400]

输出:
    phrase_split_cases.tsv      有上文：4~8 字的词组在「前后两半都是词库词」的位置拆开，上文 = 前半，目标 = 后半
    phrase_nocontext_cases.tsv  无上文：4~8 字、拼音超过 8 个字母的词组（调度器只对这样的无上文输入请求大模型）

两份都按文本排序后等距抽样，结果是确定的。用例格式与 context_cases.tsv 相同：上文 \\t 目标 \\t 拼音。
"""
import os
import sys


def load_entries(path):
    """词条 -> (逐字拼音, 权重)；只保留纯汉字、每字都有注音的词条，重复词条取权重最高的注音"""
    entries = {}
    with open(path, encoding="utf-8") as f:
        for line in f:
            if line.startswith("#") or "\t" not in line:
                continue
            parts = line.rstrip("\n").split("\t")
            if len(parts) < 3:
                continue
            text, pinyin = parts[0], parts[1].split(" ")
            try:
                weight = int(parts[2])
            except ValueError:
                continue
            if not all("一" <= c <= "鿿" for c in text) or len(pinyin) != len(text):
                continue
            if text not in entries or entries[text][1] < weight:
                entries[text] = (pinyin, weight)
    return entries


def sample(rows, want):
    rows.sort()
    step = max(1, len(rows) // want)
    return rows[::step][:want]


def main():
    if len(sys.argv) not in (3, 4):
        sys.exit(__doc__)
    source, out_dir = sys.argv[1], sys.argv[2]
    want = int(sys.argv[3]) if len(sys.argv) == 4 else 400
    entries = load_entries(source)
    phrases = {t: v for t, v in entries.items() if 4 <= len(t) <= 8 and v[1] >= 3000}

    split = []
    for text, (pinyin, _) in phrases.items():
        for k in range(2, len(text) - 1):
            head, tail = text[:k], text[k:]
            if head in entries and tail in entries and entries[head][1] >= 1000 and entries[tail][1] >= 1000:
                split.append((text, head, tail, "".join(pinyin[k:])))
                break
    with open(os.path.join(out_dir, "phrase_split_cases.tsv"), "w", encoding="utf-8") as f:
        f.write("# 对照用例（由 tools/eval/gen_control_cases.py 生成）：context\ttarget\tpinyin\n")
        f.write("# 来源：rime-ice 基础词库中 4~8 字、权重不低于 3000 的词组，在「前后两半都是词库词」的位置拆开；\n")
        f.write("# 上文 = 前半，目标 = 后半，拼音取词库对该词组的注音。按文本排序后等距抽样。\n")
        f.write("# 用途：检验大模型重排在「解码器大多已经正确」的普通词组上是否把正确首选改错。\n")
        for _, head, tail, pinyin in sample(split, want):
            f.write(f"{head}\t{tail}\t{pinyin}\n")

    whole = [(t, "".join(p)) for t, (p, _) in phrases.items() if len("".join(p)) > 8]
    with open(os.path.join(out_dir, "phrase_nocontext_cases.tsv"), "w", encoding="utf-8") as f:
        f.write("# 对照用例（由 tools/eval/gen_control_cases.py 生成）：context\ttarget\tpinyin，上文为空\n")
        f.write("# 来源：rime-ice 基础词库中 4~8 字、权重不低于 3000、拼音超过 8 个字母的词组，按文本排序后等距抽样。\n")
        f.write("# 用途：检验无上文时大模型重排是否把解码器已经正确的词库词组改错。\n")
        for text, pinyin in sample(whole, want):
            f.write(f"\t{text}\t{pinyin}\n")
    print(f"词条 {len(entries)}，可拆分词组 {len(split)}，无上文词组 {len(whole)}")


if __name__ == "__main__":
    main()
