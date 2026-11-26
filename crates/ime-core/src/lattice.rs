//! `lattice`: 中英统一音形晶格图（Unified Bilingual Lattice / DAG）
//!
//! 将中文多字词、单字、简拼、词典规范大小写英文词、以及大写/音系违规触发的原样英文片段
//! 统一建模在同一张有向无环图（DAG）中，实现零 Shift 键的丝滑中英混输。

use crate::pinyin::{extract_syllable_spans, FuzzyPinyinConfig, PhonotacticDetector, ShuangpinScheme, SyllableSpan};
use ime_lexicon::{CompiledLexicon, DomainTag};
use serde::{Deserialize, Serialize};

/// 晶格图中边的类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeKind {
    /// 多音节中文词条（如 `bu'zai'yi'le` -> `"不在意了"`）
    ChineseWord,
    /// 单音节汉字（如 `wo` -> `"我"`）
    ChineseChar,
    /// 首字母简拼词条（如 `jsj` -> `"计算机"`, `bg` -> `"变更"`）
    AbbrevPinyin,
    /// 命中英文词库的规范大小写单词（如 `rust` -> `"Rust"`, `api` -> `"API"`, `github` -> `"GitHub"`）
    EnglishWord,
    /// 英文前缀自动补全（如 `typesc` -> `"TypeScript"`）
    EnglishPrefixCompletion,
    /// 大写字母引导或音系学违规触发的原样英文片段
    EnglishRaw,
}

/// 晶格图中的有向边 `(start -> end)`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatticeEdge {
    pub start: usize,
    pub end: usize,
    /// 候选上屏词文本
    pub text: String,
    /// 对应的拼音或英文键说明（用于 UI 预编辑展示与调试）
    pub code_key: String,
    /// 边的类型
    pub kind: EdgeKind,
    /// 词条静态基础负对数代价（越小越优先）
    pub base_cost: f32,
    /// 垂直领域标签
    pub domain: DomainTag,
    /// 覆盖的中文音节数量（英文词视为 1 个完整语义单元）
    pub syllable_count: usize,
    /// 是否由模糊音或拼写纠错产生
    #[serde(default)]
    pub is_fuzzy: bool,
    /// 常见易读错字词的正确读音提示（如 `"馄 (hun) 饨 (tun)"`）；常规词为 `None`
    #[serde(skip)]
    pub pronunciation_hint: Option<&'static str>,
}

/// 针对输入字符串构建的中英统一有向无环图
#[derive(Debug, Clone)]
pub struct BilingualLattice {
    pub input: String,
    /// `edges_from[i]` 存储所有从字符下标 `i` 出发的候选边
    pub edges_from: Vec<Vec<LatticeEdge>>,
}

impl BilingualLattice {
    /// 构建中英统一晶格图
    pub fn build(
        input: &str,
        lexicon: &CompiledLexicon,
        fuzzy: &FuzzyPinyinConfig,
        shuangpin: ShuangpinScheme,
    ) -> Self {
        let n = input.len();
        let mut edges_from: Vec<Vec<LatticeEdge>> = vec![Vec::new(); n + 1];
        if n == 0 {
            return Self {
                input: String::new(),
                edges_from,
            };
        }

        // 1. 提取所有合法的全拼/模糊音/双拼/单字母简拼音节步
        let spans = extract_syllable_spans(input, fuzzy, shuangpin);
        let mut spans_at: Vec<Vec<&SyllableSpan>> = vec![Vec::new(); n + 1];
        for sp in &spans {
            if !sp.is_abbrev {
                spans_at[sp.start].push(sp);
            }
        }
        // 精确音节永远先于模糊纠错音节遍历（稳定排序保留各自内部由长到短的顺序），
        // 确保 `collect_chinese_edges` 判断 `has_common_exact_word_at_origin` 时同起点的精确多字词均已入列
        for bucket in &mut spans_at {
            bucket.sort_by_key(|sp| sp.fuzzy_penalty > 0.0);
        }

        // 2. 基于 Trie 节点增量游走的零拷贝 DFS：枚举从位置 i 出发的 1~6 个连续中文音节路径，无前缀时立即剪枝
        for start_pos in 0..n {
            if !spans_at[start_pos].is_empty() {
                collect_chinese_edges(
                    start_pos,
                    start_pos,
                    0,
                    0,
                    0.0,
                    0,
                    input,
                    &spans_at,
                    lexicon,
                    &mut edges_from[start_pos],
                );
                crate::supplement::collect_supplement_edges(
                    start_pos,
                    input,
                    &spans_at,
                    &mut edges_from[start_pos],
                );
                crate::corrector::collect_misread_edges(
                    start_pos,
                    input,
                    &spans_at,
                    &mut edges_from[start_pos],
                );
            } else if start_pos == 0 && (2..=6).contains(&n) && !spans_at[1].is_empty() {
                // 2.1 首音节声母简拼 + 后继全拼音节（混拼，如 `zme` -> 怎么、`sme` -> 什么、`wmen` -> 我们、`dfang` -> 地方）
                let first_c = &input[0..1];
                if first_c.as_bytes()[0].is_ascii_lowercase() {
                    let sep_nodes = collect_first_initial_sep_nodes(lexicon, 0, first_c);
                    for sep_node in sep_nodes {
                        collect_chinese_edges_from_sep(
                            0,
                            1,
                            sep_node,
                            1,
                            1.35,
                            input,
                            &spans_at,
                            lexicon,
                            &mut edges_from[0],
                        );
                    }
                }
            }
        }

        // 3. 扫描首字母简拼前缀树 (`abbrev_trie`)，支持如 `jsj` -> `计算机`, `wzj` -> `为自己 / 我在... / 我...`
        let lower_input = input.to_ascii_lowercase();
        for start_pos in 0..n {
            let suffix = &input[start_pos..];
            if !suffix.as_bytes()[0].is_ascii_lowercase() {
                continue;
            }
            let no_full_syl_at_start = spans_at[start_pos].is_empty();
            for (matched_len, entry_ids) in lexicon.abbrev_trie().common_prefix_search(suffix) {
                let end_pos = start_pos + matched_len;
                let span_str = &input[start_pos..end_pos];
                let is_whole_input = start_pos == 0 && end_pos == n;

                // 3.1 单字母声母简拼单字（如输入 `w`、`wzj` 中的 `w`/`z`/`j`、或句首/句中/句尾未成音节的孤立声母 `z`/`w`/`f`）：
                // 当该位置无完整全拼音节（`no_full_syl_at_start`）且句中不构成词典英文词前缀时，允许匹配高频声母单字，杜绝退化为原样英文字母
                if matched_len == 1 {
                    let has_eng_word_at_start =
                        crate::supplement::has_english_prefix(lexicon, &lower_input[start_pos..]);
                    if !no_full_syl_at_start
                        || (n > 6 && end_pos < n && has_eng_word_at_start)
                    {
                        continue;
                    }
                    let take_chars = if n == 1 {
                        30
                    } else if end_pos == n || n <= 6 {
                        6
                    } else {
                        3
                    };
                    let char_penalty = if n == 1 {
                        0.5
                    } else if n <= 6 {
                        2.6
                    } else if end_pos == n {
                        3.2
                    } else {
                        4.2
                    };
                    for id in entry_ids.iter().take(take_chars) {
                        let entry = lexicon.entry(id);
                        if entry.syllable_count() == 1 {
                            edges_from[start_pos].push(LatticeEdge {
                                start: start_pos,
                                end: end_pos,
                                text: entry.text().to_string(),
                                code_key: span_str.to_string(),
                                kind: EdgeKind::ChineseChar,
                                base_cost: entry.cost() + char_penalty,
                                domain: entry.domain(),
                                syllable_count: 1,
                                is_fuzzy: false,
                                pronunciation_hint: None,
                            });
                        }
                    }
                    continue;
                }

                // 3.2 多字母词组简拼（长度 >= 2）：
                // 若当前片段已被起始于同一位置的单个精确全拼音节覆盖（如 `shi` / `shiyong` 中的 `sh`），
                // 且并非整串输入，则直接跳过，杜绝全拼词首被拆成 `s'h -> 上海`
                let covered_by_single_full_syl = spans_at[start_pos]
                    .iter()
                    .any(|sp| sp.fuzzy_penalty == 0.0 && sp.end >= end_pos);
                if covered_by_single_full_syl && !is_whole_input {
                    continue;
                }
                let is_single_full_syl = crate::pinyin::is_valid_or_alias_syllable(span_str);
                let is_sentence_tail_abbrev = end_pos == n
                    && no_full_syl_at_start
                    && matched_len <= 4
                    && !crate::supplement::has_english_entry(
                        lexicon,
                        &lower_input[start_pos..end_pos],
                    );

                // 关键规则：若在长句中间（n > 6 且不在句尾纯声母处），严禁局部简拼抢夺全拼与英文词（如 `amd + de`）
                if !is_whole_input && n > 6 && !is_sentence_tail_abbrev {
                    continue;
                }

                let clean_full_syls = matched_len >= 3
                    && crate::pinyin::can_segment_as_clean_full_syllables(
                        &lower_input[start_pos..end_pos],
                    );
                let (abbrev_penalty, take_limit) = if is_single_full_syl {
                    (4.2, 6)
                } else if is_whole_input && clean_full_syls {
                    // 整串本身已能完整切分为标准全拼音节（如 `zaya -> za'ya`、`jiewa -> jie'wa`）时，
                    // 四字成语简拼（如 `z'a'y'a -> 左岸右岸`）只能排在全拼候选之后兜底，绝不反超全拼
                    (8.5 + (matched_len as f32) * 1.0, 8)
                } else if is_whole_input {
                    (1.6 + (matched_len as f32) * 0.25, 24)
                } else if no_full_syl_at_start {
                    // 纯简拼串的前缀词组（如 `wzj` 中的 `wz`）或长句末尾简拼词（如 `keyiyouhuadedf` 中的 `df` -> 地方）
                    (2.2 + (matched_len as f32) * 0.35, 20)
                } else {
                    (6.8 + (matched_len as f32) * 1.8, 6)
                };

                for id in entry_ids.iter().take(take_limit) {
                    let entry = lexicon.entry(id);
                    if entry.syllable_count() < 2 {
                        continue;
                    }
                    let cal_cost = calibrated_word_cost(entry.text(), entry.syllable_key(), entry.cost());
                    edges_from[start_pos].push(LatticeEdge {
                        start: start_pos,
                        end: end_pos,
                        text: entry.text().to_string(),
                        code_key: entry.syllable_key().to_string(),
                        kind: EdgeKind::AbbrevPinyin,
                        base_cost: cal_cost + abbrev_penalty,
                        domain: entry.domain(),
                        syllable_count: entry.syllable_count(),
                        is_fuzzy: false,
                        pronunciation_hint: None,
                    });
                }
            }

            if start_pos == 0
                && n >= 2
                && suffix.bytes().all(|b| b.is_ascii_lowercase())
                && !crate::pinyin::can_segment_as_clean_full_syllables(&lower_input)
            {
                let abbrev_pen = 1.6 + (n as f32) * 0.25;
                for sup in crate::supplement::lookup_supplement_abbrev(&lower_input) {
                    if !edges_from[0]
                        .iter()
                        .any(|e| e.end == n && e.text == sup.text)
                    {
                        edges_from[0].push(LatticeEdge {
                            start: 0,
                            end: n,
                            text: sup.text.to_string(),
                            code_key: sup.syllable_key.to_string(),
                            kind: EdgeKind::AbbrevPinyin,
                            base_cost: sup.cost + abbrev_pen,
                            domain: sup.domain,
                            syllable_count: sup.syllable_key.split('\'').count(),
                            is_fuzzy: false,
                            pronunciation_hint: None,
                        });
                    }
                }
            }

            // 3.3 句尾不完整单音节前缀补全（如输入 `zh`/`ch`/`sh`/`zho`/`zhon`/`sho`/`xio`/`dua` 等尚未敲完韵母的单字）
            let tail_len = n - start_pos;
            if (2..=5).contains(&tail_len)
                && suffix.bytes().all(|b| b.is_ascii_lowercase())
                && !crate::pinyin::is_valid_or_alias_syllable(suffix)
                && !crate::supplement::has_english_entry(lexicon, &lower_input[start_pos..n])
            {
                let take_inc = if start_pos == 0 { 24 } else { 6 };
                let inc_char_penalty = if start_pos == 0 { 1.25 } else { 2.85 };
                for id in collect_syllable_prefix_entries(lexicon, 0, suffix, false, take_inc) {
                    let entry = lexicon.entry(id);
                    if entry.syllable_count() == 1 && entry.text().chars().count() == 1 {
                        edges_from[start_pos].push(LatticeEdge {
                            start: start_pos,
                            end: n,
                            text: entry.text().to_string(),
                            code_key: suffix.to_string(),
                            kind: EdgeKind::ChineseChar,
                            base_cost: entry.cost() + inc_char_penalty,
                            domain: entry.domain(),
                            syllable_count: 1,
                            is_fuzzy: false,
                            pronunciation_hint: None,
                        });
                    }
                }
            }
        }

        // 4. 扫描英文与程序员规范词库 (`english_trie` + 专名补充树 + 形态合成)，实现无空格中英混输
        let detector = PhonotacticDetector::global();

        for start_pos in 0..n {
            let lower_suffix = &lower_input[start_pos..];
            let orig_suffix = &input[start_pos..];
            if !orig_suffix.as_bytes()[0].is_ascii_alphabetic() {
                continue;
            }

            let starts_with_upper = orig_suffix.as_bytes()[0].is_ascii_uppercase();
            let push_or_upgrade_eng = |edges: &mut Vec<LatticeEdge>,
                                       end_pos: usize,
                                       text: String,
                                       code_key: String,
                                       base_cost: f32,
                                       domain: DomainTag| {
                if let Some(existing) = edges.iter_mut().find(|e| {
                    e.end == end_pos
                        && e.kind == EdgeKind::EnglishWord
                        && e.text.eq_ignore_ascii_case(&text)
                }) {
                    if base_cost < existing.base_cost {
                        existing.text = text;
                        existing.code_key = code_key;
                        existing.base_cost = base_cost;
                        existing.domain = domain;
                    }
                    return;
                }
                edges.push(LatticeEdge {
                    start: start_pos,
                    end: end_pos,
                    text,
                    code_key,
                    kind: EdgeKind::EnglishWord,
                    base_cost,
                    domain,
                    syllable_count: 1,
                    is_fuzzy: false,
                    pronunciation_hint: None,
                });
            };

            // 4.1 精确前缀匹配词典中的英文词（如 `rust`, `api`, `github`, `open`, `source`, `bug`）
            for (matched_len, entry_ids) in lexicon.english_trie().common_prefix_search(lower_suffix) {
                let end_pos = start_pos + matched_len;
                let raw_slice = &input[start_pos..end_pos];
                let cluster_score = detector.english_cluster_score(raw_slice);

                // 如果该英文词包含非拼音辅音簇（如 `rust` 含 `st`、`python` 含 `th`、`github` 含 `th/hb`）
                // 或以大写字母引导，则给予强力的音系学英文奖励，使其在长句混输中击败碎片化拼音
                let phonotactic_bonus = (cluster_score * 0.85).min(3.2)
                    + if starts_with_upper { 2.5 } else { 0.0 };

                // 短英文词（2~3字母如 `ai`, `ui`, `ok`, `api`, `bug`, `app`）在长句中间若完全符合合法拼音，需略微克制，交由 Bigram 或场景提权决胜
                let short_overlap_penalty = if matched_len <= 2 && !starts_with_upper && n > 2 {
                    2.6
                } else if matched_len == 3 && cluster_score == 0.0 && !starts_with_upper && n > 3 {
                    0.85
                } else {
                    0.0
                };

                for id in entry_ids.iter().take(4) {
                    let entry = lexicon.entry(id);
                    push_or_upgrade_eng(
                        &mut edges_from[start_pos],
                        end_pos,
                        entry.text().to_string(),
                        entry.flat_key().into_owned(),
                        (entry.cost() - phonotactic_bonus + short_overlap_penalty).max(0.2),
                        entry.domain(),
                    );
                }
            }

            // 4.1b 扫描专有名词、数码品牌与互联网高频缩写补充树（如 `iPhone`, `iPad`, `AirPods`, `MacBook`, `WeChat`, `YouTube`, `URL`, `HDMI`）
            for (matched_len, sup_ids) in crate::supplement::english_supplement_trie().common_prefix_search(lower_suffix) {
                let end_pos = start_pos + matched_len;
                let raw_slice = &input[start_pos..end_pos];
                let cluster_score = detector.english_cluster_score(raw_slice);
                let phonotactic_bonus = (cluster_score * 0.85).min(3.2)
                    + if starts_with_upper { 2.5 } else { 0.0 };
                let short_overlap_penalty = if matched_len <= 2 && !starts_with_upper && n > 2 {
                    2.6
                } else if matched_len == 3 && cluster_score == 0.0 && !starts_with_upper && n > 3 {
                    0.85
                } else {
                    0.0
                };
                for &id in sup_ids.iter().take(4) {
                    let sup = crate::supplement::english_supplement_entry(id);
                    push_or_upgrade_eng(
                        &mut edges_from[start_pos],
                        end_pos,
                        sup.text.to_string(),
                        sup.lower_key.to_string(),
                        (sup.cost - phonotactic_bonus + short_overlap_penalty).max(0.2),
                        sup.domain,
                    );
                }
            }

            // 4.1c 英语高产形态构词规则：`i` + 词库已知英文词根 -> 驼峰专名 `i<Stem>`（如 `iMovie`, `iBook`, `iWork`, `iLife`, `iCare`, `iPhoto`）。
            //      汉语拼音没有任何音节以 `i` 开头；当整词本身不在普通英文词典中（排除 `ideal`, `image`, `input` 等原生普通词），
            //      且不处于前接汉字复韵母 `ai/ei/ui/oi` 内部时，自动合成规范驼峰词边。
            if lower_suffix.starts_with('i')
                && lower_suffix.len() >= 4
                && (start_pos == 0
                    || !matches!(
                        lower_input.as_bytes()[start_pos - 1],
                        b'a' | b'e' | b'u' | b'o'
                    ))
            {
                for (stem_len, stem_ids) in lexicon.english_trie().common_prefix_search(&lower_suffix[1..]) {
                    if stem_len < 3 {
                        continue;
                    }
                    let whole_len = 1 + stem_len;
                    let whole_lower = &lower_suffix[..whole_len];
                    if crate::supplement::has_english_entry(lexicon, whole_lower) {
                        continue;
                    }
                    let Some(top_stem_id) = stem_ids.first() else {
                        continue;
                    };
                    let stem_entry = lexicon.entry(top_stem_id);
                    let stem_text = stem_entry.text();
                    if stem_entry.cost() > 16.5 || !stem_text.is_ascii() || stem_text.len() < 3 {
                        continue;
                    }
                    let mut chars = stem_text.chars();
                    let Some(first_ch) = chars.next() else {
                        continue;
                    };
                    if !first_ch.is_ascii_alphabetic() {
                        continue;
                    }
                    let camel = format!("i{}{}", first_ch.to_ascii_uppercase(), chars.as_str());
                    let end_pos = start_pos + whole_len;
                    let cluster_score = detector.english_cluster_score(&input[start_pos..end_pos]);
                    let phonotactic_bonus = (cluster_score * 0.85).min(3.2)
                        + if starts_with_upper { 2.0 } else { 0.8 };
                    push_or_upgrade_eng(
                        &mut edges_from[start_pos],
                        end_pos,
                        camel,
                        whole_lower.to_string(),
                        (stem_entry.cost().min(10.8) - phonotactic_bonus).max(0.5),
                        DomainTag::Tech,
                    );
                }
            }

            // 4.2 句尾长英文前缀自动补全（如输入 `typesc` 自动补全 `TypeScript`、`iphon` 自动补全 `iPhone`）
            if start_pos == 0 && n >= 4 {
                let mut prefix_cands: Vec<(f32, String, String, DomainTag)> = Vec::new();
                for id in lexicon.english_trie().prefix_lookup(lower_suffix).iter().take(4) {
                    let entry = lexicon.entry(id);
                    if entry.flat_key().len() > n {
                        prefix_cands.push((
                            entry.cost(),
                            entry.text().to_string(),
                            entry.flat_key().into_owned(),
                            entry.domain(),
                        ));
                    }
                }
                for &id in crate::supplement::english_supplement_trie()
                    .prefix_lookup(lower_suffix)
                    .iter()
                    .take(4)
                {
                    let sup = crate::supplement::english_supplement_entry(id);
                    if sup.lower_key.len() > n
                        && !prefix_cands
                            .iter()
                            .any(|(_, t, _, _)| t.eq_ignore_ascii_case(sup.text))
                    {
                        prefix_cands.push((
                            sup.cost,
                            sup.text.to_string(),
                            sup.lower_key.to_string(),
                            sup.domain,
                        ));
                    }
                }
                prefix_cands.sort_by(|a, b| a.0.total_cmp(&b.0));
                for (cost, text, code_key, domain) in prefix_cands.into_iter().take(4) {
                    edges_from[0].push(LatticeEdge {
                        start: 0,
                        end: n,
                        text,
                        code_key,
                        kind: EdgeKind::EnglishPrefixCompletion,
                        base_cost: cost + 1.5,
                        domain,
                        syllable_count: 1,
                        is_fuzzy: false,
                        pronunciation_hint: None,
                    });
                }
            }
        }

        // 4.3 未登录英文词整串音系学兜底边：
        //     当整串输入（3..=28 字节纯 ASCII 字母/数字）在词库中不存在任何单步整词（中文、简拼或英文），
        //     无法由合法全拼/纠错音节与词典英文词组合覆盖（含句尾未敲完的声母前缀），且具备显著非拼音英文特征
        //     （以 `i/u/v` 开头、包含英文辅音簇 `english_cluster_score >= 2.0`、或含大写字母/数字）时，补充一条整串原样英文边，
        //     防止未知英文专名/变量名被硬拆成 `u + 你 + 非法 + i` 之类乱码碎片，同时不污染已覆盖的「中文+词典英文」混输句。
        let has_leading_common_chinese_word = edges_from[0].iter().any(|e| {
            !e.is_fuzzy
                && e.kind == EdgeKind::ChineseWord
                && e.syllable_count >= 2
                && e.end >= 4
                && e.base_cost < 12.0
        });
        if (3..=20).contains(&n)
            && !has_leading_common_chinese_word
            && input.bytes().all(|b| b.is_ascii_alphanumeric())
            && input.bytes().any(|b| b.is_ascii_alphabetic())
            && !edges_from[0].iter().any(|e| e.end == n)
            && !can_cover_with_non_abbrev_pinyin(&lower_input, &spans_at, &edges_from, lexicon)
            && (matches!(lower_input.as_bytes()[0], b'i' | b'u' | b'v')
                || detector.english_cluster_score(&lower_input) >= 2.0
                || input
                    .bytes()
                    .any(|b| b.is_ascii_uppercase() || b.is_ascii_digit()))
        {
            edges_from[0].push(LatticeEdge {
                start: 0,
                end: n,
                text: input.to_string(),
                code_key: lower_input.clone(),
                kind: EdgeKind::EnglishRaw,
                base_cost: 14.5 + (n as f32) * 1.15,
                domain: DomainTag::Tech,
                syllable_count: 1,
                is_fuzzy: false,
                pronunciation_hint: None,
            });
        }

        // 5. 大写字母引导的驼峰/原样英文 Token 或未知字符兜底边
        for start_pos in 0..n {
            let bytes = input.as_bytes();
            if bytes[start_pos] == b'\'' {
                // 显式分词符单步跳过
                edges_from[start_pos].push(LatticeEdge {
                    start: start_pos,
                    end: start_pos + 1,
                    text: String::new(),
                    code_key: "'".to_string(),
                    kind: EdgeKind::EnglishRaw,
                    base_cost: 0.0,
                    domain: DomainTag::General,
                    syllable_count: 0,
                    is_fuzzy: false,
                    pronunciation_hint: None,
                });
                continue;
            }

            if bytes[start_pos].is_ascii_uppercase() {
                let mut end_pos = start_pos + 1;
                while end_pos < n && bytes[end_pos].is_ascii_alphanumeric() {
                    end_pos += 1;
                }
                let raw_token = &input[start_pos..end_pos];
                edges_from[start_pos].push(LatticeEdge {
                    start: start_pos,
                    end: end_pos,
                    text: raw_token.to_string(),
                    code_key: raw_token.to_string(),
                    kind: EdgeKind::EnglishRaw,
                    base_cost: 4.5,
                    domain: DomainTag::Tech,
                    syllable_count: 1,
                    is_fuzzy: false,
                    pronunciation_hint: None,
                });
            }

            // 保证图在任意位置至少有一条单字符推进边（防止死路）
            if edges_from[start_pos].is_empty() {
                let ch = input[start_pos..].chars().next().unwrap();
                let end_pos = start_pos + ch.len_utf8();
                edges_from[start_pos].push(LatticeEdge {
                    start: start_pos,
                    end: end_pos,
                    text: ch.to_string(),
                    code_key: ch.to_string(),
                    kind: EdgeKind::EnglishRaw,
                    base_cost: 14.0,
                    domain: DomainTag::General,
                    syllable_count: 1,
                    is_fuzzy: false,
                    pronunciation_hint: None,
                });
            }
        }

        Self {
            input: input.to_string(),
            edges_from,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_chinese_edges<'a>(
    origin_start: usize,
    curr_pos: usize,
    curr_trie_node: u32,
    depth: usize,
    accum_fuzzy: f32,
    fuzzy_syl_count: u8,
    input: &str,
    spans_at: &[Vec<&'a SyllableSpan>],
    lexicon: &CompiledLexicon,
    out_edges: &mut Vec<LatticeEdge>,
) {
    if depth >= 6 || curr_pos >= input.len() {
        return;
    }

    // 跳过可选的显式分隔符 `'`
    let effective_pos = if input.as_bytes()[curr_pos] == b'\'' && depth > 0 {
        curr_pos + 1
    } else {
        curr_pos
    };
    if effective_pos >= input.len() {
        return;
    }

    // 若不是首个音节，先在 Trie 中走一步分隔符 `'`；若当前节点没有后续音节分支，直接返回
    let base_node = if depth == 0 {
        curr_trie_node
    } else {
        match lexicon.syllable_trie().advance_node(curr_trie_node, "'") {
            Some(node) => node,
            None => return,
        }
    };

    let syl_count = depth + 1;
    let n = input.len();
    let is_short_single_input = n <= 4;
    // 长句 (>12 字母) 中单个词最多允许 1 处模糊音/纠错；短词最多允许 2 处
    let max_fuzzy_for_input = if n > 12 { 1.7 } else { 3.3 };

    for &sp in &spans_at[effective_pos] {
        let new_fuzzy = accum_fuzzy + sp.fuzzy_penalty;
        if new_fuzzy > max_fuzzy_for_input {
            continue;
        }
        let new_fuzzy_syls = fuzzy_syl_count + u8::from(sp.fuzzy_penalty > 0.0);

        // 在 Trie 上直接推进一个音节，若词库中根本不存在以该音节序列为前缀的词条，立即剪枝！
        let Some(next_node) = lexicon.syllable_trie().advance_node(base_node, &sp.syllable) else {
            continue;
        };

        let hit_ids = lexicon.syllable_trie().exact_entries(next_node);
        let raw_span = &input[origin_start..sp.end];
        let splits_single_syl =
            syl_count >= 2 && crate::pinyin::is_unapostrophed_single_syllable(raw_span);
        // 无隔音符强拆合法单音节惩罚（如 `jie -> ji'e 饥饿`、`yue -> yu'e 余额`、`tuan -> tu'an 图案`）：
        // 适度回补词库构建期对双音节词预扣的长度加成，使高频单音节字（接/解…、越/月/约/乐、先/现、团）稳居前排，
        // 同时让常用拆分词（饥饿、余额、西安、提案、企鹅）保留在靠后的合理位置而不被挤没；
        // 用户显式输入 `ji'e`、`yu'e`、`xi'an` 时包含 `'`，`splits_single_syl` 为 false，零惩罚。
        const SINGLE_SYLLABLE_SPLIT_PENALTY: f32 = 0.70;
        if !hit_ids.is_empty() && !(splits_single_syl && new_fuzzy > 0.0) {
            let take_limit = if syl_count == 1 {
                if new_fuzzy > 0.0 {
                    // 长句中绝不用模糊音单字去拼凑碎片；短输入时最多保留前 3 个高频模糊单字
                    if n > 6 { 0 } else { 3 }
                } else if n > 6 {
                    // 句中音节保留前 8 个高频单字（覆盖「钱/区/饭/娃/账」等常用字），句首保留前 4 个供整句解码（首音节分步候选由解码器单独补齐）
                    if origin_start > 0 { 8 } else { 4 }
                } else {
                    24
                }
            } else if new_fuzzy > 0.0 {
                if n > 8 { 2 } else { 4 }
            } else if n > 8 {
                6
            } else {
                12
            };
            // 多音节词条的纠错惩罚校准到 AIMELX03 百万词尺度（`cost < 13.8`，对应 `wt >= ~2500`，涵盖「趁现在」「豪绅」「公家」「分账」）：
            // 当起始位置已存在精确多音节词条，或同一词累计了 2 次以上模糊音替换时，不给模糊音打折，杜绝「呈现再爱」「辞工甲方的」「骑缝章」反超精确输入
            let has_common_exact_word_at_origin = new_fuzzy > 0.0
                && out_edges
                    .iter()
                    .any(|e| !e.is_fuzzy && e.syllable_count >= 2 && e.base_cost < 13.6);
            let effective_fuzzy = if syl_count >= 2 {
                if new_fuzzy_syls >= 2 {
                    new_fuzzy * 1.45
                } else if has_common_exact_word_at_origin || new_fuzzy >= 3.0 {
                    new_fuzzy * 1.15
                } else {
                    new_fuzzy * 0.75
                }
            } else if is_short_single_input {
                // 单音节短输入时提高模糊音单字惩罚（约需 36x 以上字频差），确保精确同音常用字（如 `si` 的「四/死/思/似/斯/司」）
                // 排在模糊纠错单字（如 `shi` 的「是」）之前，同时仍高于生僻字
                new_fuzzy * 2.25
            } else {
                new_fuzzy
            };
            let split_penalty = if splits_single_syl {
                SINGLE_SYLLABLE_SPLIT_PENALTY
            } else {
                0.0
            };

            let scan_cap = hit_ids.len().min(24);
            for (idx, id) in hit_ids.iter().take(scan_cap).enumerate() {
                let entry = lexicon.entry(id);
                if idx >= take_limit {
                    // 封闭类语法单字（数词/常见度量衡单字，如 `qi` 的第 6 字「七」、`ling` 的第 7 字「零」、`jin` 的「斤」、`mu` 的「亩」）
                    // 与双音节复合数词（如 `qishi` 的第 7 词「七十」）在长句中不因字频位次被提前剪枝
                    let keep_closed = new_fuzzy == 0.0
                        && ((syl_count == 1 && is_closed_numeral_or_measure_char(entry.text()))
                            || (syl_count == 2 && is_closed_compound_numeral(entry.text())));
                    if !keep_closed {
                        continue;
                    }
                }
                let cal_cost = calibrated_word_cost(entry.text(), entry.syllable_key(), entry.cost());
                out_edges.push(LatticeEdge {
                    start: origin_start,
                    end: sp.end,
                    text: entry.text().to_string(),
                    code_key: entry.syllable_key().to_string(),
                    kind: if syl_count >= 2 {
                        EdgeKind::ChineseWord
                    } else {
                        EdgeKind::ChineseChar
                    },
                    base_cost: cal_cost + effective_fuzzy + split_penalty,
                    domain: entry.domain(),
                    syllable_count: syl_count,
                    is_fuzzy: new_fuzzy > 0.0,
                    pronunciation_hint: None,
                });
            }
        }

        collect_chinese_edges(
            origin_start,
            sp.end,
            next_node,
            syl_count,
            new_fuzzy,
            new_fuzzy_syls,
            input,
            spans_at,
            lexicon,
            out_edges,
        );
    }

    // 多音节词末尾不完整音节/声母补全（如 `keyiyouhuadedif` 中的 `di` + `f` -> `地方`，`zenm` -> `怎么`，`weishenm` -> `为什么`，`zenmy` -> `怎么样`）
    // 注意：若 `input[origin_start..n]` 本身已是单个合法完整音节（如 `qun`、`fan`、`zhuang`），绝不能将其韵尾（如 `qun` 中的 `n`）拆成第 2 个半音节去补全出「去年」等双字词！
    let whole_slice_lower = input[origin_start..n].to_ascii_lowercase();
    let whole_slice_is_single_syl = crate::pinyin::is_valid_or_alias_syllable(&whole_slice_lower);
    if depth >= 1 && accum_fuzzy == 0.0 && effective_pos < n && !whole_slice_is_single_syl {
        let rem_tail = &input[effective_pos..n];
        if rem_tail.len() <= 5 && rem_tail.bytes().all(|b| b.is_ascii_lowercase()) {
            let tail_is_valid_syl = crate::pinyin::is_valid_syllable(rem_tail);
            let whole_slice_is_en = !lexicon
                .english_trie()
                .exact_lookup(&whole_slice_lower)
                .is_empty();
            let inc_penalty = if whole_slice_is_en {
                2.8
            } else if !tail_is_valid_syl {
                0.95
            } else {
                1.55
            };
            // 句尾已是完整音节（如 `zhongli` 的 `li`、`shima` 的 `ma`、`youjia` 的 `jia`、`renshe` 的 `she`）时：
            // 1) 若该完整音节本身已能与前文构成常用词（`AIMELX03` 代价 `< 12.6`，对应原词频 `>= ~9000`，
            //    如 `youjia` -> 油价、`xuji` -> 续集、`lingxia` -> 零下、`renshe` -> 人设、`shijia` -> 施加/是假），
            //    绝不再将其当作“未输完的半截音节”去扩展出 `you'jian`（邮件）、`ren'shen`（人身）等跨音节干扰项；
            // 2) 仅当原样完整音节无常用词（如 `zhonggu` 仅有生僻/低频词「中古」`cost ≈ 13.45`）且补全音节最多只多 1 个尾字母
            //    且补全词显著高频（如 `gu` -> `guo` 补全「中国」）时才允许补全。
            let exact_best_cost = if tail_is_valid_syl {
                lexicon
                    .syllable_trie()
                    .advance_node(base_node, rem_tail)
                    .and_then(|nd| lexicon.syllable_trie().exact_entries(nd).first())
                    .map(|id| {
                        let e = lexicon.entry(id);
                        calibrated_word_cost(e.text(), e.syllable_key(), e.cost())
                    })
            } else {
                None
            };
            let allow_tail_completion = if !tail_is_valid_syl {
                true
            } else {
                exact_best_cost.is_none_or(|best| best >= 12.6)
            };
            if allow_tail_completion {
                let prefix_limit = if tail_is_valid_syl { 3 } else { 8 };
                for id in collect_syllable_prefix_entries(lexicon, base_node, rem_tail, false, prefix_limit) {
                    let entry = lexicon.entry(id);
                    let cal_cost = calibrated_word_cost(entry.text(), entry.syllable_key(), entry.cost());
                    if tail_is_valid_syl {
                        let last_syl_len = entry
                            .syllables()
                            .next_back()
                            .map(|s| s.len())
                            .unwrap_or(0);
                        if last_syl_len > rem_tail.len() + 1 {
                            continue;
                        }
                        if exact_best_cost.is_some_and(|best| cal_cost + 3.2 >= best) {
                            continue;
                        }
                    }
                    let extra_valid_tail_pen = if tail_is_valid_syl
                        && cal_cost >= 8.8
                        && exact_best_cost.is_none_or(|best| best >= 14.5)
                    {
                        2.6
                    } else {
                        0.0
                    };
                    out_edges.push(LatticeEdge {
                        start: origin_start,
                        end: n,
                        text: entry.text().to_string(),
                        code_key: entry.syllable_key().to_string(),
                        kind: EdgeKind::ChineseWord,
                        base_cost: cal_cost + inc_penalty + extra_valid_tail_pen,
                        domain: entry.domain(),
                        syllable_count: syl_count,
                        is_fuzzy: false,
                        pronunciation_hint: None,
                    });
                }
            }
            if rem_tail.len() == 2 && !tail_is_valid_syl && !whole_slice_is_en && syl_count + 1 <= 6 {
                for id in collect_two_initial_tail_entries(
                    lexicon,
                    base_node,
                    &rem_tail[0..1],
                    &rem_tail[1..2],
                    6,
                ) {
                    let entry = lexicon.entry(id);
                    let cal_cost = calibrated_word_cost(entry.text(), entry.syllable_key(), entry.cost());
                    out_edges.push(LatticeEdge {
                        start: origin_start,
                        end: n,
                        text: entry.text().to_string(),
                        code_key: entry.syllable_key().to_string(),
                        kind: EdgeKind::ChineseWord,
                        base_cost: cal_cost + 1.35,
                        domain: entry.domain(),
                        syllable_count: syl_count + 1,
                        is_fuzzy: false,
                        pronunciation_hint: None,
                    });
                }
            }
        }
    }
}

/// 在 `syllable_trie` 中从 `start_node` 出发沿不完整音节前缀 `prefix_slice` 前进，
/// 收集其当前音节子树（不跨越下一个 `'`）中按 `cost` 升序排列的前 `limit` 个词条 ID。
fn collect_syllable_prefix_entries(
    lexicon: &CompiledLexicon,
    start_node: u32,
    prefix_slice: &str,
    include_self: bool,
    limit: usize,
) -> Vec<u32> {
    let syllable_trie = lexicon.syllable_trie();
    let Some(p_node) = syllable_trie.advance_node(start_node, prefix_slice) else {
        return Vec::new();
    };
    let mut stack = vec![(p_node, 0u8)];
    let mut cands: Vec<(f32, u32)> = Vec::new();
    while let Some((node_idx, depth)) = stack.pop() {
        let node = syllable_trie.node(node_idx);
        if depth > 0 || include_self {
            for id in node.exact_entries() {
                let cost = lexicon.entry(id).cost();
                cands.push((cost, id));
            }
        }
        if depth < 5 {
            for (ch, child_idx) in node.transitions() {
                if ch.is_ascii_lowercase() {
                    stack.push((child_idx, depth + 1));
                }
            }
        }
    }
    cands.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    cands.dedup_by_key(|&mut (_, id)| id);
    cands.into_iter().take(limit).map(|(_, id)| id).collect()
}

/// 收集末尾连续两个声母（如 `zenmy` 中的 `m` + `y`、`shurf` 中的 `r` + `f`）对应的词条 ID
fn collect_two_initial_tail_entries(
    lexicon: &CompiledLexicon,
    start_node: u32,
    c1: &str,
    c2: &str,
    limit: usize,
) -> Vec<u32> {
    let syllable_trie = lexicon.syllable_trie();
    let Some(p1) = syllable_trie.advance_node(start_node, c1) else {
        return Vec::new();
    };
    let mut stack1 = vec![(p1, 0u8)];
    let mut after_sep_nodes = Vec::new();
    while let Some((node_idx, d)) = stack1.pop() {
        let node = syllable_trie.node(node_idx);
        if let Some(sep_child) = node.find_child(b'\'') {
            after_sep_nodes.push(sep_child);
        }
        if d < 5 {
            for (ch, child_idx) in node.transitions() {
                if ch.is_ascii_lowercase() {
                    stack1.push((child_idx, d + 1));
                }
            }
        }
    }
    let mut cands: Vec<(f32, u32)> = Vec::new();
    for sep_node in after_sep_nodes {
        for id in collect_syllable_prefix_entries(lexicon, sep_node, c2, true, limit) {
            cands.push((lexicon.entry(id).cost(), id));
        }
    }
    cands.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    cands.dedup_by_key(|&mut (_, id)| id);
    cands.into_iter().take(limit).map(|(_, id)| id).collect()
}

/// 收集从 `start_node` 起以单字母声母 `first_c` 起始且后接分隔符 `'` 的所有 Trie 节点（用于 `zme` -> 怎么、`zenmban` -> 怎么办）
fn collect_first_initial_sep_nodes(
    lexicon: &CompiledLexicon,
    start_node: u32,
    first_c: &str,
) -> Vec<u32> {
    let syllable_trie = lexicon.syllable_trie();
    let Some(p0) = syllable_trie.advance_node(start_node, first_c) else {
        return Vec::new();
    };
    let mut stack = vec![(p0, 0u8)];
    let mut sep_nodes = Vec::new();
    while let Some((node_idx, d)) = stack.pop() {
        let node = syllable_trie.node(node_idx);
        if let Some(sep_child) = node.find_child(b'\'') {
            sep_nodes.push(sep_child);
        }
        if d < 5 {
            for (ch, child_idx) in node.transitions() {
                if ch.is_ascii_lowercase() {
                    stack.push((child_idx, d + 1));
                }
            }
        }
    }
    sep_nodes
}

#[allow(clippy::too_many_arguments)]
fn collect_chinese_edges_from_sep<'a>(
    origin_start: usize,
    curr_pos: usize,
    sep_node: u32,
    depth: usize,
    base_penalty: f32,
    input: &str,
    spans_at: &[Vec<&'a SyllableSpan>],
    lexicon: &CompiledLexicon,
    out_edges: &mut Vec<LatticeEdge>,
) {
    let syl_count = depth + 1;
    for &sp in &spans_at[curr_pos] {
        if sp.fuzzy_penalty > 0.0 {
            continue;
        }
        let Some(next_node) = lexicon.syllable_trie().advance_node(sep_node, &sp.syllable) else {
            continue;
        };
        let hit_ids = lexicon.syllable_trie().exact_entries(next_node);
        for id in hit_ids.iter().take(6) {
            let entry = lexicon.entry(id);
            let cal_cost = calibrated_word_cost(entry.text(), entry.syllable_key(), entry.cost());
            out_edges.push(LatticeEdge {
                start: origin_start,
                end: sp.end,
                text: entry.text().to_string(),
                code_key: entry.syllable_key().to_string(),
                kind: EdgeKind::ChineseWord,
                base_cost: cal_cost + base_penalty,
                domain: entry.domain(),
                syllable_count: syl_count,
                is_fuzzy: false,
                pronunciation_hint: None,
            });
        }
        if syl_count < 6 && sp.end < input.len() {
            collect_chinese_edges(
                origin_start,
                sp.end,
                next_node,
                syl_count,
                0.0,
                0,
                input,
                spans_at,
                lexicon,
                out_edges,
            );
        }
    }
}

#[inline]
pub(crate) fn is_pure_numeral_char(c: char) -> bool {
    matches!(
        c,
        '一' | '二' | '两' | '三' | '四' | '五' | '六' | '七' | '八' | '九' | '十'
            | '百' | '千' | '万' | '亿' | '零' | '半' | '几'
    )
}

#[inline]
fn is_closed_numeral_or_measure_char(text: &str) -> bool {
    matches!(
        text,
        "一" | "二" | "两" | "三" | "四" | "五" | "六" | "七" | "八" | "九" | "十"
            | "百" | "千" | "万" | "亿" | "零" | "半" | "几" | "每" | "各" | "第"
            | "米" | "斤" | "克" | "吨" | "亩" | "升" | "度" | "元" | "块" | "角"
            | "毛" | "分" | "倍" | "成" | "折" | "层" | "楼" | "栋" | "号" | "岁"
            | "天" | "年" | "月" | "周" | "日" | "秒" | "瓦" | "匹" | "寸" | "尺"
            | "箱" | "盒" | "袋" | "瓶" | "杯" | "碗" | "税" | "职" | "觉" | "账"
            | "假" | "货" | "警" | "澡" | "婚" | "泳" | "款" | "费" | "单" | "票"
            | "卡" | "达" | "码" | "柜" | "付" | "还" | "结" | "退" | "转" | "寄"
    )
}

#[inline]
fn is_closed_compound_numeral(text: &str) -> bool {
    let mut chars = text.chars();
    matches!((chars.next(), chars.next(), chars.next()), (Some(a), Some(b), None) if is_pure_numeral_char(a) && is_pure_numeral_char(b))
}

/// 现代汉语高频新词/常用词与分词残余字频偏差校准：
/// 1) 开源基础词库（`rime_ice_8105` / `rime_ice_base`）基于分词后的语料统计单字词频，「四」大量被「四个/四月/四十/四川」吸收，
///    导致单字残余词频略低于补语「死」（`8.45` vs `8.24`），且公文提纲「三是/四是」权重反超基数词「三十/四十」，「十四」显著偏低；
/// 2) 对近年日常高频词（如「人设」「女频」「短剧」「受众」「尾款」「欠款」「首付」等）做温和上界校准，同时保留同音正词自身的原始竞争位次，交由搭配与语境模型决胜。
#[inline]
pub(crate) fn calibrated_word_cost(text: &str, code_key: &str, raw_cost: f32) -> f32 {
    let cap = match (text, code_key) {
        ("四", "si") => 8.05,
        ("三十", "san'shi") => 8.30,
        ("四十", "si'shi") => 8.65,
        ("十四", "shi'si") => 11.25,
        ("尾款", "wei'kuan") => 10.65,
        ("欠款", "qian'kuan") => 9.15,
        ("首付", "shou'fu") => 11.95,
        ("人设", "ren'she") => 10.55,
        ("女频", "nv'pin") => 10.75,
        ("男频", "nan'pin") => 10.85,
        ("受众", "shou'zhong") => 10.85,
        ("短剧", "duan'ju") => 12.15,
        ("内卷", "nei'juan") => 10.95,
        ("复盘", "fu'pan") => 10.95,
        ("霸总", "ba'zong") => 11.35,
        ("团宠", "tuan'chong") => 11.55,
        ("二创", "er'chuang") => 11.55,
        ("拉新", "la'xin") => 11.75,
        ("投流", "tou'liu") => 11.75,
        ("出圈", "chu'quan") => 11.65,
        ("痛点", "tong'dian") => 11.35,
        ("甜宠", "tian'chong") => 11.45,
        ("塌房", "ta'fang") => 11.85,
        _ => return raw_cost,
    };
    raw_cost.min(cap)
}

/// 判断小写输入串是否能由合法非简拼拼音音节（含模糊音/手误纠错）与词典英文词组合覆盖，允许末尾残留尚未敲完的声母前缀
fn can_cover_with_non_abbrev_pinyin(
    lower_input: &str,
    spans_at: &[Vec<&SyllableSpan>],
    edges_from: &[Vec<LatticeEdge>],
    lexicon: &CompiledLexicon,
) -> bool {
    let n = lower_input.len();
    if n == 0 {
        return false;
    }
    let mut reachable = vec![false; n + 1];
    reachable[0] = true;
    for pos in 0..n {
        if !reachable[pos] {
            continue;
        }
        for sp in &spans_at[pos] {
            if !sp.is_abbrev && sp.end <= n {
                reachable[sp.end] = true;
            }
        }
        for edge in &edges_from[pos] {
            if edge.kind == EdgeKind::EnglishWord && edge.end >= pos + 2 && edge.end <= n {
                reachable[edge.end] = true;
            }
        }
    }
    if reachable[n] {
        return true;
    }
    // 允许长句末尾残留 1~4 字节尚未敲完的合法拼音前缀（如 `keyiyouhuadedif` 的 `f`、`nihaozh` 的 `zh`）
    for pos in 1..n {
        if !reachable[pos] {
            continue;
        }
        let tail = &lower_input[pos..n];
        if tail.len() <= 4
            && tail.bytes().all(|b| b.is_ascii_lowercase())
            && lexicon.syllable_trie().advance_node(0, tail).is_some()
        {
            return true;
        }
    }
    false
}
