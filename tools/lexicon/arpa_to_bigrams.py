#!/usr/bin/env python3
"""从 Fcitx5 libime 的标准 ARPA 语言模型（lm_sc.arpa，LGPL-2.1-or-later）提取高价值中文 Bigram 转移表。

设计原则：
1. 仅保留前后词均存在于当前词库（data/raw/*.dict.yaml + data/seed/*.dict.yaml）中的词对，零冗余词条；
2. 用 ARPA 自带的一元与二元对数概率计算同分布互信息（PMI，单位 nat）：
       PMI(w1, w2) = (log10 P(w2 | w1) - log10 P(w2)) * ln(10)
3. 按信息论最优的交叉熵期望收益（KL 贡献度）排序截断：
       score = log10 P(w1) + log10 P(w2 | w1) + log10(PMI)
   同时自动淘汰低频偶发噪声与高共现低信息量虚词对；
4. 将 PMI 线性缩放并截顶至 [min_bonus, max_bonus]，与解码器 Unigram cost / 步长惩罚标尺对齐；
5. 输出按 (prev, next) 字典序确定性排列，保证源码包离线重编逐字节一致。

用法：
    python3 -I tools/lexicon/arpa_to_bigrams.py <lm_sc.arpa> <输出.bigram.tsv> \\
        [--repo-root .] [--max-entries 500000] [--min-pmi 1.2] [--scale 0.55] [--max-bonus 4.5]
"""
import argparse
import glob
import math
import os
import sys

LN10 = math.log(10.0)


def load_lexicon_vocab(repo_root: str) -> set[str]:
    vocab: set[str] = set()
    patterns = [
        os.path.join(repo_root, "data", "raw", "*.dict.yaml"),
        os.path.join(repo_root, "data", "seed", "*.dict.yaml"),
    ]
    files = sorted(p for pat in patterns for p in glob.glob(pat) if not os.path.basename(p).startswith("."))
    if not files:
        sys.exit(f"错误：在 {repo_root}/data/{{raw,seed}} 下未找到任何 .dict.yaml 词表")
    for path in files:
        in_header = False
        with open(path, encoding="utf-8") as f:
            for line in f:
                s = line.strip()
                if not s or s.startswith("#"):
                    continue
                if s == "---":
                    in_header = True
                    continue
                if s == "...":
                    in_header = False
                    continue
                if in_header:
                    continue
                word = s.split("\t", 1)[0].strip()
                if word:
                    vocab.add(word)
    return vocab


def is_valid_token(w: str) -> bool:
    if not w or w in ("<s>", "</s>", "<unk>"):
        return False
    # 排除含制表符/空白的异常 token
    return not any(ch.isspace() for ch in w)


def extract_bigrams(
    arpa_path: str,
    vocab: set[str],
    max_entries: int,
    min_pmi: float,
    scale: float,
    max_bonus: float,
    min_bonus: float,
) -> list[tuple[str, str, float]]:
    unigrams: dict[str, float] = {}
    candidates: list[tuple[float, float, str, str]] = []
    section = 0
    total_2grams = 0
    in_vocab_2grams = 0

    with open(arpa_path, encoding="utf-8", errors="replace") as f:
        for line in f:
            s = line.strip()
            if not s:
                continue
            if s == "\\1-grams:":
                section = 1
                continue
            if s == "\\2-grams:":
                section = 2
                continue
            if s == "\\3-grams:" or s == "\\end\\":
                break

            if section == 1:
                parts = s.split("\t")
                if len(parts) >= 2:
                    w = parts[1]
                    if w in vocab:
                        try:
                            unigrams[w] = float(parts[0])
                        except ValueError:
                            pass
            elif section == 2:
                parts = s.split("\t")
                if len(parts) < 2:
                    continue
                toks = parts[1].split(" ")
                if len(toks) != 2:
                    continue
                total_2grams += 1
                w1, w2 = toks[0], toks[1]
                if not (is_valid_token(w1) and is_valid_token(w2)):
                    continue
                p1 = unigrams.get(w1)
                p2 = unigrams.get(w2)
                if p1 is None or p2 is None:
                    continue
                in_vocab_2grams += 1
                try:
                    p12 = float(parts[0])
                except ValueError:
                    continue
                pmi = (p12 - p2) * LN10
                if pmi < min_pmi:
                    continue
                bonus = min(max_bonus, pmi * scale)
                if bonus < min_bonus:
                    continue
                # KL 期望信息贡献：log10(P(w1, w2) * PMI(w1, w2))
                kl_score = p1 + p12 + math.log10(pmi)
                candidates.append((kl_score, bonus, w1, w2))

    # 确定性排序：KL 贡献降序，同分按键字典序
    candidates.sort(key=lambda item: (-item[0], item[2], item[3]))
    kept = candidates[:max_entries]
    # 输出按 (w1, w2) 字典序，便于 diff 与稳定压缩
    result = [(w1, w2, round(bonus, 3)) for _, bonus, w1, w2 in kept]
    result.sort(key=lambda x: (x[0], x[1]))
    print(
        f"[arpa_to_bigrams] ARPA 1-grams(in_vocab)={len(unigrams):,}, "
        f"2-grams={total_2grams:,}, in_vocab={in_vocab_2grams:,}, "
        f"pmi>={min_pmi}={len(candidates):,}, kept={len(result):,}",
        file=sys.stderr,
    )
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description="从 Fcitx5 libime lm_sc.arpa 提取中文 Bigram TSV")
    parser.add_argument("arpa", help="lm_sc.arpa 路径")
    parser.add_argument("output", help="输出 .bigram.tsv 路径")
    parser.add_argument("--repo-root", default=".", help="仓库根目录（用于加载词库词表）")
    parser.add_argument("--max-entries", type=int, default=500_000, help="最多保留词对数（建议略低于 2 的幂：500000 或 1000000）")
    parser.add_argument("--min-pmi", type=float, default=1.2, help="最低 PMI（nat）阈值")
    parser.add_argument("--scale", type=float, default=0.55, help="PMI 到解码器 bonus 的线性缩放系数")
    parser.add_argument("--max-bonus", type=float, default=4.5, help="单对最大 bonus 上限")
    parser.add_argument("--min-bonus", type=float, default=0.5, help="单对最小 bonus 下限")
    parser.add_argument("--source-note", default="lm_sc.arpa-20260629.tar.zst", help="写入文件头的上游来源标识")
    args = parser.parse_args()

    vocab = load_lexicon_vocab(args.repo_root)
    rows = extract_bigrams(
        args.arpa,
        vocab,
        max_entries=args.max_entries,
        min_pmi=args.min_pmi,
        scale=args.scale,
        max_bonus=args.max_bonus,
        min_bonus=args.min_bonus,
    )
    os.makedirs(os.path.dirname(os.path.abspath(args.output)), exist_ok=True)
    with open(args.output, "w", encoding="utf-8", newline="\n") as out:
        out.write("# 中文统计二元语法表（由 tools/lexicon/arpa_to_bigrams.py 生成，勿手改）\n")
        out.write(f"# 上游来源：Fcitx5 libime ({args.source_note}) https://github.com/fcitx/libime\n")
        out.write("# Copyright (C) 2022-2026 CSSlayer <wengxt@gmail.com>, LGPL-2.1-or-later\n")
        out.write(
            f"# 参数：max_entries={args.max_entries} min_pmi={args.min_pmi} "
            f"scale={args.scale} max_bonus={args.max_bonus}\n"
        )
        for w1, w2, bonus in rows:
            out.write(f"{w1}\t{w2}\t{bonus:.3f}\n")
    print(f"写出 {len(rows):,} 条 Bigram → {args.output}")


if __name__ == "__main__":
    main()
