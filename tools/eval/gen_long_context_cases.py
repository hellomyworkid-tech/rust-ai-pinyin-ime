#!/usr/bin/env python3
"""生成上文超过 48 个字符的评测用例（输出已随仓库提供，修改本脚本后重新生成）。

用法（在仓库根目录）:
    python3 tools/eval/gen_long_context_cases.py

为什么需要：其余评测用例的上文都不超过 48 个字符，而引擎只留上文的最后 64 个字符，进程内打分器又只看
其中最后 48 个字符，并且（lm_context_tail_anchored）把窗口起点对齐到分句处。上文更长时这几点才起作用，
需要专门的用例来量。

写出两个文件：
- data/eval/long_context_cases.tsv：手写的 18 条，前面再加一句中性的开场白，上文总长 55~69 个字符
  （超过 64 个的由引擎截掉开头）。判断依据大多在最后一个分句里（精湛的、严重影响、透明、国际、核心……），
  截断、对齐之后落在打分窗口外的几乎只有开场白：14 条只丢了开场白；另外 4 条（誓言、实验、全力、公式）
  还丢了前面一个分句，但决定性的词（新郎新娘、实验楼、进攻球员和拼尽了、方程和求根）仍在窗口里。
  所以这一组检验的是上文很长、依据仍在窗口里时，截断和对齐窗口不会坏事；
  并不检验落在窗口外的依据能不能保住（没有哪一条的依据只在窗口外）；
- data/eval/long_context_mixed_cases.tsv：alternating_cases.tsv 的 107 条，每条前面拼上另外几条用例的完整句子
  （上文 + 目标 + 句号，拼音不同，直到上文不少于 56 个字符），模拟一段话里话题来回切换。
"""

from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
EVAL = ROOT / "data" / "eval"

OPENERS = [
    "说起来这件事其实挺有意思的。",
    "我想跟大家分享一件最近发生的事。",
    "前几天有个朋友问了我一个问题。",
    "这是上个月发生的一件小事。",
    "下面我简单介绍一下事情的经过。",
]

# 上文 \t 目标 \t 拼音。判断依据大多在最后一个分句里，截断、对齐之后仍在打分窗口里（见文件开头的说明）
HANDWRITTEN = """\
那天的婚礼进行到最感人的时候，新郎紧紧拉着新娘的手，当着两百多位亲朋好友的面，一字一句地念出了自己写的	誓言	shiyan
化学老师说这学期的重点是动手操作，所以每个星期四下午都会带全班同学去实验楼，今天大家又认认真真地完成了一个	实验	shiyan
据调查，这位官员在任期间多次利用职务之便为亲属的公司谋取不正当利益，纪委认定他严重滥用了手中的	权力	quanli
这场比赛对两支球队来说都是生死战，比分落后的主队在最后十分钟换上了三名进攻球员，场上每个人都拼尽了	全力	quanli
公司今年推行新的考勤制度，所有员工每天都要在系统里如实填写自己做了哪些工作，月底按照实际记录的	工时	gongshi
期末考试的数学卷子最后一道大题特别难，需要先把方程化成标准形式，然后再代入老师课上讲过的那个求根	公式	gongshi
沈从文是我最喜欢的现代作家，他笔下的湘西世界美得像一幅水墨画，我最近又重新读了一遍他的小说	边城	biancheng
王师傅在景德镇做了四十多年陶瓷，从拉坯到上釉每道工序都一丝不苟，大家都很佩服他这一手精湛的	技艺	jiyi
小区里的路灯坏了快一个月了，晚上出门黑漆漆的很不安全，居民们商量之后决定一起去向物业	反映	fanying
周末我们一家人去郊外的森林公园露营，天黑之前爸爸让我去附近捡一些干树枝，因为晚上要先	生火	shenghuo
这家工厂的产品在出厂前要经过好几道检验程序，他在这里工作了十几年，是一名非常负责的质量	检查员	jianchayuan
疫情发生以后，各国之间的贸易和人员往来都受到了很大影响，专家认为当前复杂多变的国际	形势	xingshi
新学期开学的第一天，校门口挤满了拖着行李箱的新生，按照学校的通知，大家要在九月一号到学院	报到	baodao
医生说我的眼睛近视又加深了，主要是因为每天晚上躺在床上玩手机的时间太长，这样下去会严重影响	视力	shili
搬家那天东西特别多，纸箱子装满以后合不上盖子，我跑遍了整个小区的超市才买到一卷透明	胶带	jiaodai
这篇文章讲述了一位乡村教师坚守山区三十年的故事，虽然篇幅不长，但是给了我们很多深刻的	启示	qishi
我这个人记性不太好，经常出门忘带钥匙，不过小时候和外婆在乡下生活的那些日子，至今还留在我的	记忆	jiyi
这家互联网公司最近几年发展得很快，员工从五十人增加到了两千人，他们靠的就是不断创新的核心	技术	jishu
"""


def read_cases(path):
    rows = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        context, target, pinyin = line.split("\t")[:3]
        rows.append((context, target.strip(), pinyin.strip()))
    return rows


def write(path, header, rows):
    lines = [f"# {h}" for h in header]
    lines += ["\t".join(row) for row in rows]
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def main():
    handwritten = []
    for index, line in enumerate(HANDWRITTEN.splitlines()):
        context, target, pinyin = line.split("\t")
        handwritten.append((OPENERS[index % len(OPENERS)] + context, target, pinyin))
    write(
        EVAL / "long_context_cases.tsv",
        [
            "长上文用例（由 tools/eval/gen_long_context_cases.py 生成）：context\ttarget\tpinyin",
            "手写的 18 条，前面加一句中性的开场白，上文总长 55~69 个字符；判断依据大多在最后一个分句里。",
            "用途：上文超过 48 个字符时，引擎只留最后 64 个字符，打分器只看其中最后 48 个字符并对齐到分句处。"
            "这里截断、对齐之后落在窗口外的几乎只有开场白，判断依据仍在窗口里，",
            "所以检验的是截断和对齐窗口不会坏事，不检验落在窗口外的依据能不能保住。",
        ],
        handwritten,
    )

    base = read_cases(EVAL / "alternating_cases.tsv")
    mixed = []
    n = len(base)
    for i, (context, target, pinyin) in enumerate(base):
        # 往前拼不同拼音的句子，直到上文至少 56 个字符（键盘最多传 64 个）
        previous = []
        for step in (37, 53, 71, 89, 101, 13, 29, 43, 61, 79):
            j = (i + step) % n
            other = base[j]
            if other[2] == pinyin or any(p[2] == other[2] for p in previous):
                continue
            previous.insert(0, other)
            if len("".join(c + t + "。" for c, t, _ in previous)) + len(context) >= 56:
                break
        lead = "".join(c + t + "。" for c, t, _ in previous)
        mixed.append((lead + context, target, pinyin))
    write(
        EVAL / "long_context_mixed_cases.tsv",
        [
            "长上文用例（由 tools/eval/gen_long_context_cases.py 生成）：context\ttarget\tpinyin",
            "alternating_cases.tsv 的 107 条，每条前面拼上另外几条用例的完整句子（拼音不同，直到上文不少于 56 个字符），模拟一段话里话题来回切换。",
            "判断依据在上文末尾；用途：检验前面不相干的句子会不会把模型带偏，以及打分窗口对齐分句后结果是否不变。",
        ],
        mixed,
    )

    def span(rows):
        lengths = [len(c) for c, _, _ in rows]
        return f"{min(lengths)}~{max(lengths)}"

    print(f"long_context_cases.tsv: {len(handwritten)} 条（上文 {span(handwritten)} 字符）；"
          f"long_context_mixed_cases.tsv: {len(mixed)} 条（上文 {span(mixed)} 字符）")


if __name__ == "__main__":
    main()
