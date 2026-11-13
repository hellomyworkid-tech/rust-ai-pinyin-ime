#!/usr/bin/env python3
"""生成「教会的词」用例 data/eval/habit_neutral_cases.tsv 和反向检验 data/eval/habit_reversal_cases.tsv
（输出已随仓库提供，修改本脚本后重新生成）。

用法（在仓库根目录）:
    python3 tools/eval/gen_habit_cases.py

用途：检验个人记忆与模型怎么分工的另一面。alternating_cases.tsv 里上文总是指向正确的词，
记忆越弱越好；这里反过来：上文给不出判断（中性的上文），用户在第一个上文里把目标词主动改选出来，
之后换了上文，键盘应当记住用户的选择，而不是又换回模型原来排在首位的词。

habit_neutral_cases.tsv 每组 5 行：同一拼音、同一个目标词、5 个不同的中性上文（上文末尾各不相同，
所以「上文相关的纠正记忆」帮不上忙）。目标词直接取 alternating_cases.tsv 里这个拼音的第二个目标，
没有核对它是不是更不常用的那个，也没有核对模型在中性上文里是不是本来就把它排在首位。本来就排在首位的组，
第 1 行用户按空格上屏，并没有主动改选（有多少组这样取决于模型）。所以用 ffi_session --teach 5 跑时按第 1 行分开统计：
第 1 行首选不对、用户点选了目标的组才算教过，后 4 行检验换了上文之后它还在不在首位；
第 1 行本来就对的组没教过，单列。

habit_reversal_cases.tsv 检验反方向：记忆太强也不行。每组 3 行，同一拼音：前 2 行和上面一样
（同一个目标词、前两个中性上文），第 1 行教会它，第 2 行看换了上文还在不在首位；第 3 行换成这个拼音的
第一个目标（另一个词），上文取它在 alternating_cases.tsv 里第一次出现的那一行，明确指向它，
看用户教过之后，上文明确要另一个词的地方模型还能不能把它选回来。用 ffi_session --teach 3 跑：
目标和第 1 行不同的行是反向行，单独统计，不算进「换了上文仍在首位」。

habit_space_cases.tsv 检验只靠空格接受养成的记忆：每组 5 行，同一拼音、同一个目标词（这个拼音的第一个目标 C），
第 1 行是 C 在 alternating_cases.tsv 里第一次出现的那一行（上文明确指向它，通常本来就是首选，用户按空格上屏），
后 4 行换成前 4 个中性上文，看这个只被接受过、没被主动改选过的词还在不在首位。用 ffi_session --teach 5 跑时，
第 1 行本来就对的组归在「没教过」，那一组的「其余行」就是要看的数。主要用来比较 Apple 那条路的几种做法：
问不问选词器，决定了这种只被接受过的词会不会在中性上文里被模型换掉。
"""

from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
EVAL = ROOT / "data" / "eval"

NEUTRAL_CONTEXTS = ["我想说的是", "刚才提到的", "大家讨论的", "这里写的是", "下面说一下"]
# 反向检验里教会目标词用的中性上文（教一次、换上文再看一次）
REVERSAL_NEUTRAL_CONTEXTS = NEUTRAL_CONTEXTS[:2]
# 只空格接受过的词：接受之后换的中性上文
SPACE_NEUTRAL_CONTEXTS = NEUTRAL_CONTEXTS[:4]


def main():
    # 拼音 -> 按文件顺序的 (上文, 目标)
    groups = {}
    order = []
    for line in (EVAL / "alternating_cases.tsv").read_text(encoding="utf-8").splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        context, target, pinyin = line.split("\t")[:3]
        if pinyin not in groups:
            groups[pinyin] = []
            order.append(pinyin)
        groups[pinyin].append((context, target.strip()))
    habit_lines = [
        "# 教会的词（由 tools/eval/gen_habit_cases.py 生成）：context\ttarget\tpinyin",
        "# 每组 5 行：同一拼音、同一个目标词（alternating_cases.tsv 里这个拼音的第二个目标，没有核对是否更不常用）、"
        "5 个不同的中性上文。",
        "# 用 ffi_session --teach 5 跑：第 1 行首选不对、用户点选了目标的组才算教过，后 4 行检验换了上文之后它还在不在首位；",
        "# 第 1 行本来就是首选的组没有主动改选（没教过），单独统计。",
    ]
    reversal_lines = [
        "# 教会之后的反向检验（由 tools/eval/gen_habit_cases.py 生成）：context\ttarget\tpinyin",
        "# 每组 3 行，同一拼音：前 2 行的目标词 W 是 alternating_cases.tsv 里这个拼音的第二个目标（与 habit_neutral_cases.tsv 相同），"
        "上文是两个中性上文；",
        "# 第 3 行换成这个拼音的第一个目标 C，上文取 C 在 alternating_cases.tsv 里第一次出现的那一行，明确指向 C。",
        "# 检验的是：用户在中性上文里教会了 W 之后，上文明确要 C 的地方模型还选不选 C。"
        "方向与「教会的词」相反：记忆太强在这里会错。",
        "# 用 ffi_session --teach 3 跑：第 1 行教会 W，第 2 行看换了上文 W 还在不在首位；第 3 行目标与第 1 行不同，"
        "是反向行，单独统计。",
    ]
    space_lines = [
        "# 只空格接受过的词（由 tools/eval/gen_habit_cases.py 生成）：context\ttarget\tpinyin",
        "# 每组 5 行，同一拼音、同一个目标词 C（alternating_cases.tsv 里这个拼音的第一个目标）：第 1 行是 C 第一次出现的那一行，"
        "上文明确指向 C；",
        "# 后 4 行换成 4 个中性上文。用 ffi_session --teach 5 跑：第 1 行本来就对的组（用户按空格上屏、没有主动改选）"
        "归在「没教过」，它的「其余行」就是只被接受过的词到了新的中性上文还在不在首位。",
    ]
    count = 0
    for pinyin in order:
        rows = groups[pinyin]
        if len(rows) < 2:
            continue
        habit = rows[1][1]
        for context in NEUTRAL_CONTEXTS:
            habit_lines.append(f"{context}\t{habit}\t{pinyin}")
        other = rows[0][1]
        other_context = next(context for context, target in rows if target == other)
        for context in REVERSAL_NEUTRAL_CONTEXTS:
            reversal_lines.append(f"{context}\t{habit}\t{pinyin}")
        reversal_lines.append(f"{other_context}\t{other}\t{pinyin}")
        space_lines.append(f"{other_context}\t{other}\t{pinyin}")
        for context in SPACE_NEUTRAL_CONTEXTS:
            space_lines.append(f"{context}\t{other}\t{pinyin}")
        count += 1
    (EVAL / "habit_neutral_cases.tsv").write_text("\n".join(habit_lines) + "\n", encoding="utf-8")
    (EVAL / "habit_reversal_cases.tsv").write_text("\n".join(reversal_lines) + "\n", encoding="utf-8")
    (EVAL / "habit_space_cases.tsv").write_text("\n".join(space_lines) + "\n", encoding="utf-8")
    print(f"habit_neutral_cases.tsv: {count} 组，{count * len(NEUTRAL_CONTEXTS)} 行")
    print(f"habit_reversal_cases.tsv: {count} 组，{count * (len(REVERSAL_NEUTRAL_CONTEXTS) + 1)} 行")
    print(f"habit_space_cases.tsv: {count} 组，{count * (len(SPACE_NEUTRAL_CONTEXTS) + 1)} 行")


if __name__ == "__main__":
    main()
