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

        // 2. 基于 Trie 节点增量游走的零拷贝 DFS：枚举从位置 i 出发的 1~6 个连续中文音节路径，无前缀时立即剪枝
        for start_pos in 0..n {
            if !spans_at[start_pos].is_empty() {
                collect_chinese_edges(
                    start_pos,
                    start_pos,
                    0,
                    0,
                    0.0,
                    input,
                    &spans_at,
                    lexicon,
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
            for (matched_len, entry_ids) in lexicon.abbrev_trie.common_prefix_search(suffix) {
                let end_pos = start_pos + matched_len;
                let span_str = &input[start_pos..end_pos];
                let is_whole_input = start_pos == 0 && end_pos == n;

                // 3.1 单字母声母简拼单字（如输入 `w`、`wzj` 中的 `w`/`z`/`j`、或句首/句中/句尾未成音节的孤立声母 `z`/`w`/`f`）：
                // 当该位置无完整全拼音节（`no_full_syl_at_start`）且句中不构成词典英文词前缀时，允许匹配高频声母单字，杜绝退化为原样英文字母
                if matched_len == 1 {
                    let has_eng_word_at_start = !lexicon
                        .english_trie
                        .common_prefix_search(&lower_input[start_pos..])
                        .is_empty();
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
                    for &id in entry_ids.iter().take(take_chars) {
                        let entry = &lexicon.entries[id as usize];
                        if entry.pinyin_syllables.len() == 1 {
                            edges_from[start_pos].push(LatticeEdge {
                                start: start_pos,
                                end: end_pos,
                                text: entry.text.clone(),
                                code_key: span_str.to_string(),
                                kind: EdgeKind::ChineseChar,
                                base_cost: entry.cost + char_penalty,
                                domain: entry.domain,
                                syllable_count: 1,
                                is_fuzzy: false,
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
                let is_single_full_syl = crate::pinyin::is_valid_syllable(span_str);
                let is_sentence_tail_abbrev = end_pos == n
                    && no_full_syl_at_start
                    && matched_len <= 4
                    && lexicon
                        .english_trie
                        .exact_lookup(&lower_input[start_pos..end_pos])
                        .is_empty();

                // 关键规则：若在长句中间（n > 6 且不在句尾纯声母处），严禁局部简拼抢夺全拼与英文词（如 `amd + de`）
                if !is_whole_input && n > 6 && !is_sentence_tail_abbrev {
                    continue;
                }

                let (abbrev_penalty, take_limit) = if is_single_full_syl {
                    (4.2, 6)
                } else if is_whole_input {
                    (1.6 + (matched_len as f32) * 0.25, 24)
                } else if no_full_syl_at_start {
                    // 纯简拼串的前缀词组（如 `wzj` 中的 `wz`）或长句末尾简拼词（如 `keyiyouhuadedf` 中的 `df` -> 地方）
                    (2.2 + (matched_len as f32) * 0.35, 20)
                } else {
                    (6.8 + (matched_len as f32) * 1.8, 6)
                };

                for &id in entry_ids.iter().take(take_limit) {
                    let entry = &lexicon.entries[id as usize];
                    if entry.pinyin_syllables.len() < 2 {
                        continue;
                    }
                    edges_from[start_pos].push(LatticeEdge {
                        start: start_pos,
                        end: end_pos,
                        text: entry.text.clone(),
                        code_key: entry.syllable_key.clone(),
                        kind: EdgeKind::AbbrevPinyin,
                        base_cost: entry.cost + abbrev_penalty,
                        domain: entry.domain,
                        syllable_count: entry.pinyin_syllables.len(),
                        is_fuzzy: false,
                    });
                }
            }

            // 3.3 句尾不完整单音节前缀补全（如输入 `zh`/`ch`/`sh`/`zho`/`zhon`/`sho`/`xio`/`dua` 等尚未敲完韵母的单字）
            let tail_len = n - start_pos;
            if (2..=5).contains(&tail_len)
                && suffix.bytes().all(|b| b.is_ascii_lowercase())
                && !crate::pinyin::is_valid_syllable(suffix)
                && lexicon
                    .english_trie
                    .exact_lookup(&lower_input[start_pos..n])
                    .is_empty()
            {
                let take_inc = if start_pos == 0 { 24 } else { 6 };
                let inc_char_penalty = if start_pos == 0 { 1.25 } else { 2.85 };
                for id in collect_syllable_prefix_entries(lexicon, 0, suffix, false, take_inc) {
                    let entry = &lexicon.entries[id as usize];
                    if entry.pinyin_syllables.len() == 1 && entry.text.chars().count() == 1 {
                        edges_from[start_pos].push(LatticeEdge {
                            start: start_pos,
                            end: n,
                            text: entry.text.clone(),
                            code_key: suffix.to_string(),
                            kind: EdgeKind::ChineseChar,
                            base_cost: entry.cost + inc_char_penalty,
                            domain: entry.domain,
                            syllable_count: 1,
                            is_fuzzy: false,
                        });
                    }
                }
            }
        }

        // 4. 扫描英文与程序员规范词库 (`english_trie`)，实现无空格中英混输
        let detector = PhonotacticDetector::global();

        for start_pos in 0..n {
            let lower_suffix = &lower_input[start_pos..];
            let orig_suffix = &input[start_pos..];
            if !orig_suffix.as_bytes()[0].is_ascii_alphabetic() {
                continue;
            }

            let starts_with_upper = orig_suffix.as_bytes()[0].is_ascii_uppercase();

            // 4.1 精确前缀匹配词典中的英文词（如 `rust`, `api`, `github`, `open`, `source`, `bug`）
            for (matched_len, entry_ids) in lexicon.english_trie.common_prefix_search(lower_suffix) {
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

                for &id in entry_ids.iter().take(4) {
                    let entry = &lexicon.entries[id as usize];
                    edges_from[start_pos].push(LatticeEdge {
                        start: start_pos,
                        end: end_pos,
                        text: entry.text.clone(),
                        code_key: entry.flat_key.clone(),
                        kind: EdgeKind::EnglishWord,
                        base_cost: (entry.cost - phonotactic_bonus + short_overlap_penalty).max(0.2),
                        domain: entry.domain,
                        syllable_count: 1,
                        is_fuzzy: false,
                    });
                }
            }

            // 4.2 句尾长英文前缀自动补全（如输入 `typesc` 自动补全 `TypeScript`）
            if start_pos == 0 && n >= 4 {
                let prefix_ids = lexicon.english_trie.prefix_lookup(lower_suffix);
                for &id in prefix_ids.iter().take(3) {
                    let entry = &lexicon.entries[id as usize];
                    if entry.flat_key.len() > n {
                        edges_from[0].push(LatticeEdge {
                            start: 0,
                            end: n,
                            text: entry.text.clone(),
                            code_key: entry.flat_key.clone(),
                            kind: EdgeKind::EnglishPrefixCompletion,
                            base_cost: entry.cost + 1.5,
                            domain: entry.domain,
                            syllable_count: 1,
                            is_fuzzy: false,
                        });
                    }
                }
            }
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
        match lexicon.syllable_trie.advance_node(curr_trie_node, "'") {
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

        // 在 Trie 上直接推进一个音节，若词库中根本不存在以该音节序列为前缀的词条，立即剪枝！
        let Some(next_node) = lexicon.syllable_trie.advance_node(base_node, &sp.syllable) else {
            continue;
        };

        let hit_ids = &lexicon.syllable_trie.nodes[next_node as usize].exact_entries;
        if !hit_ids.is_empty() {
            let take_limit = if syl_count == 1 {
                if new_fuzzy > 0.0 {
                    // 长句中绝不用模糊音单字去拼凑碎片；短输入时最多保留前 3 个高频模糊单字
                    if n > 6 { 0 } else { 3 }
                } else if n > 6 {
                    // 长句中每个单音节保留前 4 个最高频单字供整句 Beam Search（逐音节选词时会从 Trie 补齐全部同音字）
                    4
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
            // 多音节词条的纠错惩罚略打折（完整词比碎片更可信）；
            // 单音节短输入则适当提高模糊音门槛，让 `si` 的精确字「死/四/思」排在模糊音「是/时」前面
            let has_common_exact_word_at_origin = new_fuzzy > 0.0
                && out_edges.iter().any(|e| !e.is_fuzzy && e.syllable_count >= 2 && e.base_cost < 9.2);
            let effective_fuzzy = if syl_count >= 2 {
                if has_common_exact_word_at_origin {
                    new_fuzzy * 1.15
                } else {
                    new_fuzzy * 0.75
                }
            } else if is_short_single_input {
                new_fuzzy * 1.45
            } else {
                new_fuzzy
            };

            for &id in hit_ids.iter().take(take_limit) {
                let entry = &lexicon.entries[id as usize];
                out_edges.push(LatticeEdge {
                    start: origin_start,
                    end: sp.end,
                    text: entry.text.clone(),
                    code_key: entry.syllable_key.clone(),
                    kind: if syl_count >= 2 {
                        EdgeKind::ChineseWord
                    } else {
                        EdgeKind::ChineseChar
                    },
                    base_cost: entry.cost + effective_fuzzy,
                    domain: entry.domain,
                    syllable_count: syl_count,
                    is_fuzzy: new_fuzzy > 0.0,
                });
            }
        }

        collect_chinese_edges(
            origin_start,
            sp.end,
            next_node,
            syl_count,
            new_fuzzy,
            input,
            spans_at,
            lexicon,
            out_edges,
        );
    }

    // 多音节词末尾不完整音节/声母补全（如 `keyiyouhuadedif` 中的 `di` + `f` -> `地方`，`zenm` -> `怎么`，`weishenm` -> `为什么`，`zenmy` -> `怎么样`）
    // 注意：若 `input[origin_start..n]` 本身已是单个合法完整音节（如 `qun`、`fan`、`zhuang`），绝不能将其韵尾（如 `qun` 中的 `n`）拆成第 2 个半音节去补全出「去年」等双字词！
    let whole_slice_lower = input[origin_start..n].to_ascii_lowercase();
    let whole_slice_is_single_syl = crate::pinyin::is_valid_syllable(&whole_slice_lower);
    if depth >= 1 && accum_fuzzy == 0.0 && effective_pos < n && !whole_slice_is_single_syl {
        let rem_tail = &input[effective_pos..n];
        if rem_tail.len() <= 5 && rem_tail.bytes().all(|b| b.is_ascii_lowercase()) {
            let tail_is_valid_syl = crate::pinyin::is_valid_syllable(rem_tail);
            let whole_slice_is_en = !lexicon
                .english_trie
                .exact_lookup(&whole_slice_lower)
                .is_empty();
            let inc_penalty = if whole_slice_is_en {
                2.8
            } else if !tail_is_valid_syl {
                0.95
            } else {
                1.55
            };
            // 句尾已是完整音节（如 `zhongli` 的 `li`、`shima` 的 `ma`、`api` 的 `pi`）时：
            // 1) 若该完整音节本身已能与前文构成常用词（`exact_best_cost < 10.2`，如 `zhongli` -> 中立/重力、`shima` -> 是吗），
            //    绝不再将其当作“未输完的半截音节”去扩展出 `zhong'liang`（重量）、`zhong'liu`（肿瘤）等跨音节干扰项；
            // 2) 仅当原样完整音节无常用词（如 `zhonggu` 仅有低频词「中古」）且补全音节最多只多 1 个尾字母（如 `gu` -> `guo` 补全「中国」）时才允许补全。
            let exact_best_cost = if tail_is_valid_syl {
                lexicon
                    .syllable_trie
                    .advance_node(base_node, rem_tail)
                    .and_then(|nd| lexicon.syllable_trie.nodes[nd as usize].exact_entries.first().copied())
                    .map(|id| lexicon.entries[id as usize].cost)
            } else {
                None
            };
            let allow_tail_completion = if !tail_is_valid_syl {
                true
            } else {
                exact_best_cost.is_none_or(|best| best >= 10.2)
            };
            if allow_tail_completion {
                for id in collect_syllable_prefix_entries(lexicon, base_node, rem_tail, false, 8) {
                    let entry = &lexicon.entries[id as usize];
                    if tail_is_valid_syl {
                        let last_syl_len = entry
                            .pinyin_syllables
                            .last()
                            .map(|s| s.len())
                            .unwrap_or(0);
                        if last_syl_len > rem_tail.len() + 1 {
                            continue;
                        }
                        if exact_best_cost.is_some_and(|best| entry.cost + 0.8 >= best) {
                            continue;
                        }
                    }
                    out_edges.push(LatticeEdge {
                        start: origin_start,
                        end: n,
                        text: entry.text.clone(),
                        code_key: entry.syllable_key.clone(),
                        kind: EdgeKind::ChineseWord,
                        base_cost: entry.cost + inc_penalty,
                        domain: entry.domain,
                        syllable_count: syl_count,
                        is_fuzzy: false,
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
                    let entry = &lexicon.entries[id as usize];
                    out_edges.push(LatticeEdge {
                        start: origin_start,
                        end: n,
                        text: entry.text.clone(),
                        code_key: entry.syllable_key.clone(),
                        kind: EdgeKind::ChineseWord,
                        base_cost: entry.cost + 1.35,
                        domain: entry.domain,
                        syllable_count: syl_count + 1,
                        is_fuzzy: false,
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
    let Some(p_node) = lexicon.syllable_trie.advance_node(start_node, prefix_slice) else {
        return Vec::new();
    };
    let mut stack = vec![(p_node, 0u8)];
    let mut cands: Vec<(f32, u32)> = Vec::new();
    while let Some((node_idx, depth)) = stack.pop() {
        let node = &lexicon.syllable_trie.nodes[node_idx as usize];
        if depth > 0 || include_self {
            for &id in &node.exact_entries {
                let cost = lexicon.entries[id as usize].cost;
                cands.push((cost, id));
            }
        }
        if depth < 5 {
            for &(ch, child_idx) in &node.transitions {
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
    let Some(p1) = lexicon.syllable_trie.advance_node(start_node, c1) else {
        return Vec::new();
    };
    let mut stack1 = vec![(p1, 0u8)];
    let mut after_sep_nodes = Vec::new();
    while let Some((node_idx, d)) = stack1.pop() {
        let node = &lexicon.syllable_trie.nodes[node_idx as usize];
        if let Some(sep_child) = node.find_child(b'\'') {
            after_sep_nodes.push(sep_child);
        }
        if d < 5 {
            for &(ch, child_idx) in &node.transitions {
                if ch.is_ascii_lowercase() {
                    stack1.push((child_idx, d + 1));
                }
            }
        }
    }
    let mut cands: Vec<(f32, u32)> = Vec::new();
    for sep_node in after_sep_nodes {
        for id in collect_syllable_prefix_entries(lexicon, sep_node, c2, true, limit) {
            cands.push((lexicon.entries[id as usize].cost, id));
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
    let Some(p0) = lexicon.syllable_trie.advance_node(start_node, first_c) else {
        return Vec::new();
    };
    let mut stack = vec![(p0, 0u8)];
    let mut sep_nodes = Vec::new();
    while let Some((node_idx, d)) = stack.pop() {
        let node = &lexicon.syllable_trie.nodes[node_idx as usize];
        if let Some(sep_child) = node.find_child(b'\'') {
            sep_nodes.push(sep_child);
        }
        if d < 5 {
            for &(ch, child_idx) in &node.transitions {
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
        let Some(next_node) = lexicon.syllable_trie.advance_node(sep_node, &sp.syllable) else {
            continue;
        };
        let hit_ids = &lexicon.syllable_trie.nodes[next_node as usize].exact_entries;
        for &id in hit_ids.iter().take(6) {
            let entry = &lexicon.entries[id as usize];
            out_edges.push(LatticeEdge {
                start: origin_start,
                end: sp.end,
                text: entry.text.clone(),
                code_key: entry.syllable_key.clone(),
                kind: EdgeKind::ChineseWord,
                base_cost: entry.cost + base_penalty,
                domain: entry.domain,
                syllable_count: syl_count,
                is_fuzzy: false,
            });
        }
        if syl_count < 6 && sp.end < input.len() {
            collect_chinese_edges(
                origin_start,
                sp.end,
                next_node,
                syl_count,
                0.0,
                input,
                spans_at,
                lexicon,
                out_edges,
            );
        }
    }
}
