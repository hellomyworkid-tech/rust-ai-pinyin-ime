//! `decoder`: 中英混合 Viterbi + Beam Search 极速解码器
//!
//! 特性：
//! 1. 束搜索（Beam Search）多路径并行寻优，耗时 `< 1ms`
//! 2. 结合上文末尾词（Context Tail Word）的跨句 Bigram 转移加成
//! 3. 结合当前应用焦点场景（`AppScene`）的垂直领域权重加成
//! 4. 破碎度惩罚（Fragmentation Penalty）防止英文词被拆成单字母简拼
//! 5. 黄金第二候选位（Guaranteed 2nd Slot）解决中英重叠短词冲突
//! 6. 智能盘古之白（Auto-Spacing）中英文边界自动空格格式化

use crate::lattice::{BilingualLattice, EdgeKind, LatticeEdge};
use crate::user_dict::UserDict;
use ime_lexicon::{CompiledLexicon, DomainTag};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// 当前焦点应用程序所属场景（由 L3 系统感知层或用户设定）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppScene {
    /// 通用日常场景
    General,
    /// 代码编辑器 / IDE / 终端（VS Code, RustRover, Cursor, Terminal）
    CodeEditor,
    /// 即时通讯聊天（微信, 飞书, QQ, Telegram）
    Chat,
    /// 数学/学术论文写作（LaTeX, Overleaf, MATLAB, Mathematica）
    Academic,
    /// 正式公文/办公文档（Word, Notion）
    Document,
}

impl AppScene {
    pub fn label(self) -> &'static str {
        match self {
            Self::General => "通用日常",
            Self::CodeEditor => "代码研发 (IDE/Terminal)",
            Self::Chat => "即时通讯 (聊天)",
            Self::Academic => "学术/数学写作",
            Self::Document => "公文/文档撰写",
        }
    }

    /// 根据当前焦点应用计算词条的领域匹配奖励分
    #[inline]
    pub fn domain_boost(self, domain: DomainTag, is_english: bool) -> f32 {
        match (self, domain) {
            (Self::CodeEditor, DomainTag::Tech) => {
                if is_english {
                    1.65
                } else {
                    1.15
                }
            }
            (Self::Academic, DomainTag::Academic) => 1.85,
            (Self::Chat, DomainTag::Chat) => 1.25,
            _ => 0.0,
        }
    }
}

/// 句子中单个分词片段的起止字节范围与文本（用于支持鼠标点击任意分词块进行局部换词修改）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentSpan {
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub code_key: String,
}

/// 解码输出的单个候选项
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// 最终上屏文本（已根据盘古之白策略格式化）
    pub text: String,
    /// 匹配的拼音/英文编码展示串
    pub preedit: String,
    /// 覆盖原始输入串的字节长度（等于 `input.len()` 代表整句完全匹配候选）
    pub consumed_len: usize,
    /// L1 图解码器综合得分（越小代表基础代价越低；为方便直观展示，同时记录转换后的置信度分）
    pub l1_cost: f32,
    /// L2 AI 语境重打分后的综合得分（越大越优先）
    pub final_score: f32,
    /// 是否由 AI 语境模型提权置顶
    pub ai_boosted: bool,
    /// 候选项说明标签（如 `"[整句]"`, `"[混输]"`, `"[简拼]"`, `"[英文]"`, `"[AI 优选]"`）
    pub tag: String,
    /// 路径由哪些词组成
    pub segments: Vec<String>,
    /// 每个分词块对应的精确输入字节区间 `(start, end)`
    pub segment_spans: Vec<SegmentSpan>,
    /// 每个独立拼音音节/英文词对应的精确输入字节区间 `(start, end)`（用于预编辑栏音节光标移动与逐音节改词/改拼音）
    pub syllable_spans: Vec<SegmentSpan>,
}

#[derive(Debug, Clone)]
struct BeamState {
    edges: Vec<LatticeEdge>,
    cost: f32,
}

impl BeamState {
    fn last_word(&self) -> Option<&str> {
        self.edges
            .iter()
            .rev()
            .find(|e| !e.text.is_empty())
            .map(|e| e.text.as_str())
    }
}

/// 解码配置项
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecoderConfig {
    pub beam_size: usize,
    /// 是否在中英文之间自动插入半角空格（盘古之白）
    pub auto_space_cjk_ascii: bool,
    /// 是否启用中英重叠短词的“黄金第二候选位”保底
    pub golden_second_slot_english: bool,
}

impl Default for DecoderConfig {
    fn default() -> Self {
        Self {
            beam_size: 28,
            auto_space_cjk_ascii: true,
            golden_second_slot_english: true,
        }
    }
}

pub struct BilingualDecoder;

impl BilingualDecoder {
    /// 在中英统一晶格图上执行 Beam Search 并合成多样化候选列表
    pub fn decode(
        lattice: &BilingualLattice,
        lexicon: &CompiledLexicon,
        user_dict: &UserDict,
        scene: AppScene,
        surrounding_context: &str,
        config: &DecoderConfig,
    ) -> Vec<Candidate> {
        Self::decode_with_pins(
            lattice,
            lexicon,
            user_dict,
            scene,
            surrounding_context,
            config,
            &std::collections::HashMap::new(),
        )
    }

    /// 支持用户在长句中手动锁定某几个位置的分词选择 (`pinned_segments: start_pos -> (end_pos, text)`)
    pub fn decode_with_pins(
        lattice: &BilingualLattice,
        lexicon: &CompiledLexicon,
        user_dict: &UserDict,
        scene: AppScene,
        surrounding_context: &str,
        config: &DecoderConfig,
        pinned_segments: &std::collections::HashMap<usize, (usize, String)>,
    ) -> Vec<Candidate> {
        let n = lattice.input.len();
        if n == 0 {
            return Vec::new();
        }

        let context_tail = extract_context_tail_words(surrounding_context);
        let mut beams: Vec<Vec<BeamState>> = vec![Vec::new(); n + 1];
        beams[0].push(BeamState {
            edges: Vec::new(),
            cost: 0.0,
        });

        for i in 0..n {
            if beams[i].is_empty() {
                continue;
            }
            // 在拓扑序到达位置 i 时，仅对 beams[i] 执行一次去重与 Top-K 剪枝（避免 O(n^2) 重复排序与字符串拼接）
            if beams[i].len() > 1 {
                beams[i].sort_by(|a, b| {
                    a.cost
                        .partial_cmp(&b.cost)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                let mut seen_texts = HashSet::with_capacity(beams[i].len());
                beams[i].retain(|st| {
                    let joined: String = st.edges.iter().map(|e| e.text.as_str()).collect();
                    seen_texts.insert(joined)
                });
                if beams[i].len() > config.beam_size {
                    beams[i].truncate(config.beam_size);
                }
            }
            let current_states = std::mem::take(&mut beams[i]);
            let pin_at_i = pinned_segments.get(&i);
            if let Some(&(pin_end, ref pin_text)) = pin_at_i {
                if pin_end > i && pin_end <= n {
                    // 优先复用晶格图中同起止区间的带音节分隔符 code_key（如 "shi'yong"）
                    let matched_code_key = lattice.edges_from[i]
                        .iter()
                        .find(|e| e.end == pin_end && e.text == *pin_text)
                        .or_else(|| {
                            lattice.edges_from[i]
                                .iter()
                                .find(|e| e.end == pin_end && e.code_key.contains('\''))
                        })
                        .map(|e| e.code_key.clone())
                        .unwrap_or_else(|| lattice.input[i..pin_end].to_string());

                    let is_ascii = pin_text.is_ascii();
                    let pinned_edge = LatticeEdge {
                        start: i,
                        end: pin_end,
                        text: pin_text.clone(),
                        code_key: matched_code_key,
                        syllable_count: if is_ascii { 1 } else { ((pin_end - i) / 2).max(1) },
                        kind: if is_ascii {
                            EdgeKind::EnglishWord
                        } else {
                            EdgeKind::ChineseWord
                        },
                        domain: DomainTag::General,
                        base_cost: 0.1,
                        is_fuzzy: false,
                    };
                    for state in &current_states {
                        let mut next_edges = state.edges.clone();
                        next_edges.push(pinned_edge.clone());
                        beams[pin_end].push(BeamState {
                            edges: next_edges,
                            cost: state.cost + 0.1,
                        });
                    }
                    continue;
                }
            }

            // 合并该位置 i 的静态晶格边与用户记忆自造词边
            let mut edges_at_i = lattice.edges_from[i].clone();

            if !user_dict.records.is_empty() {
                for end_pos in (i + 1)..=n.min(i + 24) {
                    let raw_slice = &lattice.input[i..end_pos];
                    // 单音节记忆（如曾在某句中把 `de` 选成「得」）只在用户单独输入该音节时生效，
                    // 绝不注入到长句中间，否则会把所有句子里的「的」都替换成「得」
                    let is_whole_input = i == 0 && end_pos == n;
                    if !is_whole_input && (crate::pinyin::is_valid_syllable(raw_slice) || raw_slice.len() <= 1) {
                        continue;
                    }
                    let mut user_recs = user_dict.top_user_records(raw_slice);
                    if is_whole_input {
                        for jrec in user_dict.top_jianpin_records(raw_slice) {
                            if !user_recs.iter().any(|r| r.text == jrec.text) {
                                user_recs.push(jrec);
                            }
                        }
                    }
                    if user_recs.is_empty() {
                        continue;
                    }
                    let matched_code_key = edges_at_i
                        .iter()
                        .find(|e| e.end == end_pos && e.code_key.contains('\''))
                        .map(|e| e.code_key.clone())
                        .unwrap_or_else(|| raw_slice.to_string());

                    for urec in user_recs.into_iter().take(4) {
                        if urec.dynamic_score >= 4.5
                            || urec.commit_count >= 2
                            || (is_whole_input && urec.code_key != raw_slice && urec.commit_count >= 1)
                        {
                            let is_ascii = urec.text.is_ascii();
                            edges_at_i.push(LatticeEdge {
                                start: i,
                                end: end_pos,
                                text: urec.text,
                                code_key: matched_code_key.clone(),
                                kind: if is_ascii {
                                    EdgeKind::EnglishWord
                                } else if urec.code_key.len() > 3 {
                                    EdgeKind::ChineseWord
                                } else {
                                    EdgeKind::ChineseChar
                                },
                                base_cost: 0.15,
                                domain: DomainTag::General,
                                syllable_count: ((end_pos - i) / 2).max(1),
                                is_fuzzy: false,
                            });
                        }
                    }
                }
            }

            for edge in &edges_at_i {
                // 禁止任何未锁定边跨越或侵入已锁定的区间 [p_start, p_end)
                let overlaps_pin = pinned_segments
                    .iter()
                    .any(|(&p_start, &(p_end, _))| edge.start < p_end && edge.end > p_start);
                if overlaps_pin {
                    continue;
                }

                if edge.text.is_empty() {
                    // 分隔符空边
                    for state in &current_states {
                        let mut next_edges = state.edges.clone();
                        next_edges.push(edge.clone());
                        beams[edge.end].push(BeamState {
                            edges: next_edges,
                            cost: state.cost,
                        });
                    }
                    continue;
                }

                // 预先计算仅依赖 edge 的静态与个人词频代价（无需在 28 个 BeamState 上重复计算 28 次）
                let mut edge_step_cost = edge.base_cost;

                // 个人动态记忆词频加成（Time-Decay User Boost）
                let raw_input_slice = &lattice.input[edge.start..edge.end];
                let raw_boost = user_dict.user_boost(raw_input_slice, &edge.text);
                let edge_is_whole = edge.start == 0 && edge.end == n;
                let user_boost = if edge.syllable_count <= 1
                    && edge.kind == EdgeKind::ChineseChar
                    && !edge_is_whole
                {
                    (raw_boost * 0.1).min(0.6)
                } else {
                    raw_boost.min(6.0)
                };
                edge_step_cost -= user_boost;

                // 应用焦点场景领域加成
                let is_eng = matches!(
                    edge.kind,
                    EdgeKind::EnglishWord
                        | EdgeKind::EnglishPrefixCompletion
                        | EdgeKind::EnglishRaw
                );
                edge_step_cost -= scene.domain_boost(edge.domain, is_eng);

                // 破碎度与跨步惩罚：每多切分一段施加微小的步长惩罚，鼓励长词整体匹配而非单字碎片拼接。
                // 对长句中非虚词/非语法助词的孤立单字（如「不能急+一」「载入+突」「可以+乐」）施加更高碎片惩罚，
                // 避免「3字词+孤立单字」抢占正常的「2字词+2字词」（如「不能+记忆」「在+如图」）。
                let step_transition_penalty = match edge.kind {
                    EdgeKind::ChineseWord => 0.55,
                    EdgeKind::EnglishWord => 0.65,
                    EdgeKind::ChineseChar => {
                        if edge_is_whole || is_common_sentence_particle(&edge.text) {
                            1.75
                        } else {
                            3.25
                        }
                    }
                    EdgeKind::AbbrevPinyin => 1.50,
                    EdgeKind::EnglishPrefixCompletion => 1.20,
                    EdgeKind::EnglishRaw => 1.60,
                };
                edge_step_cost += step_transition_penalty;

                for state in &current_states {
                    let prev_word = state
                        .last_word()
                        .or_else(|| context_tail.last().map(|s| s.as_str()))
                        .unwrap_or("");

                    // 跨词/跨语种二元语法加成（含光标前上文末尾词对当前句首词的转移加成）
                    let mut bg_bonus = lexicon.bigram_bonus(prev_word, &edge.text)
                        + user_dict.bigram_boost(prev_word, &edge.text);
                    if state.edges.is_empty() {
                        for ctx_w in &context_tail {
                            bg_bonus = bg_bonus.max(lexicon.bigram_bonus(ctx_w, &edge.text));
                        }
                    }

                    let mut next_edges = state.edges.clone();
                    next_edges.push(edge.clone());
                    beams[edge.end].push(BeamState {
                        edges: next_edges,
                        cost: state.cost + edge_step_cost - bg_bonus,
                    });
                }
            }
        }

        let mut candidates: Vec<Candidate> = Vec::new();
        let mut seen_surface: HashSet<String> = HashSet::new();

        // 收集到达句尾 N 的完整整句/整词候选路径
        let mut full_states = beams[n].clone();
        full_states.sort_by(|a, b| {
            a.cost
                .partial_cmp(&b.cost)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        for st in full_states {
            let formatted = format_segments(&st.edges, config.auto_space_cjk_ascii);
            if formatted.is_empty() || !seen_surface.insert(formatted.clone()) {
                continue;
            }
            let preedit = st
                .edges
                .iter()
                .filter(|e| !e.text.is_empty())
                .map(|e| e.code_key.as_str())
                .collect::<Vec<_>>()
                .join(" ");

            let has_en = st.edges.iter().any(|e| {
                matches!(
                    e.kind,
                    EdgeKind::EnglishWord
                        | EdgeKind::EnglishPrefixCompletion
                        | EdgeKind::EnglishRaw
                )
            });
            let has_cn = st
                .edges
                .iter()
                .any(|e| matches!(e.kind, EdgeKind::ChineseWord | EdgeKind::ChineseChar | EdgeKind::AbbrevPinyin));
            let has_fuzzy = st.edges.iter().any(|e| e.is_fuzzy);

            let tag = if has_fuzzy {
                format!("[纠错 {}]", preedit)
            } else if has_cn && has_en {
                "[中英混输]".to_string()
            } else if has_en {
                "[英文]".to_string()
            } else if st.edges.iter().any(|e| e.kind == EdgeKind::AbbrevPinyin) {
                "[简拼]".to_string()
            } else if st.edges.len() > 1 {
                "[整句]".to_string()
            } else {
                "[词库]".to_string()
            };

            let segments = st
                .edges
                .iter()
                .filter(|e| !e.text.is_empty())
                .map(|e| e.text.clone())
                .collect();

            let segment_spans = st
                .edges
                .iter()
                .filter(|e| !e.text.is_empty())
                .map(|e| SegmentSpan {
                    start: e.start,
                    end: e.end,
                    text: e.text.clone(),
                    code_key: e.code_key.clone(),
                })
                .collect();

            let syllable_spans = st
                .edges
                .iter()
                .filter(|e| !e.text.is_empty())
                .flat_map(|e| split_edge_into_syllables(&lattice.input, e))
                .collect();

            candidates.push(Candidate {
                text: formatted,
                preedit,
                consumed_len: n,
                l1_cost: st.cost,
                final_score: 100.0 - st.cost,
                ai_boosted: false,
                tag,
                segments,
                segment_spans,
                syllable_spans,
            });
        }

        // 分步选词候选（主流输入法逻辑）：计算从句首 0 起连续已手动锁定前缀之后的第一个未锁定位置 `unpinned_start`。
        // 已手动锁定的前缀（`0..unpinned_start`）绝不再重复出现在分步候选中。
        let mut unpinned_start = 0usize;
        while let Some((&_s, &(e, _))) = pinned_segments
            .iter()
            .find(|(&s, &(e, _))| s <= unpinned_start && unpinned_start < e)
        {
            if e > unpinned_start {
                unpinned_start = e.min(n);
            } else {
                break;
            }
        }

        while unpinned_start < n && lattice.input.as_bytes()[unpinned_start] == b'\'' {
            unpinned_start += 1;
        }

        // 整句候选只保留少量「真正可能」的版本：
        // - 覆盖全部输入的单一词条（如 `xuanci` → 选词/选此）都是合法同音词，全部保留；
        // - 多段拼接的整句：除第 1 名首选整句外，最多再保留 1 条「首个未锁定词块不同且代价极其接近」的备选整句
        //   （如首选「你在反思排查一下」，备选整句保留「你再反思排查一下」，而剔除句首相同、仅后半句换成生僻同音词的
        //   「你在反死排查一下」「你在反四排查一下」）。
        const MAX_DIFF_HEAD_SENTENCE_ALTS: usize = 1;
        const SENTENCE_COST_MARGIN: f32 = 2.2;
        let first_unpinned_seg = |c: &Candidate| -> Option<(usize, usize, String)> {
            c.segment_spans
                .iter()
                .find(|s| s.start >= unpinned_start)
                .map(|s| (s.start, s.end, s.text.clone()))
        };
        if let Some((best_cost, best_seg_cnt, first_head)) = candidates
            .first()
            .map(|c| (c.l1_cost, c.segments.len(), first_unpinned_seg(c)))
        {
            let mut kept_diff_head = 0usize;
            let mut idx = 0usize;
            candidates.retain(|c| {
                let is_first = idx == 0;
                idx += 1;
                if is_first || c.segments.len() <= 1 {
                    return true;
                }
                let cur_head = first_unpinned_seg(c);
                let is_clean_head_alt = match (&cur_head, &first_head) {
                    (Some((cs, ce, ct)), Some((fs, fe, ft))) => {
                        cs == fs && ce == fe && ct != ft && c.segments.len() <= best_seg_cnt
                    }
                    _ => false,
                };
                if is_clean_head_alt
                    && kept_diff_head < MAX_DIFF_HEAD_SENTENCE_ALTS
                    && c.l1_cost <= best_cost + SENTENCE_COST_MARGIN
                {
                    kept_diff_head += 1;
                    return true;
                }
                false
            });
        }

        // 整句候选之后，严格按照主流输入法层次依次给出从 `unpinned_start` 起算的：
        //   1) 所有精确匹配的多字前缀词组（三字词/双字词排在单字之前，如 `nizaifansipaichayixia` → 你在 / 你再 / 拟在；`wzj` → 文章 / 网站 / 位置 / 我在 / 完整）
        //   2) 首音节全部同音单字（按字频排序，如 你 / 尼 / 泥 / 逆 / 拟 …；或简拼首字母单字 我 / 为 / 无 / 问 / 外 / 王 …）
        //   3) 模糊音/纠错词组与单字
        const MAX_PREFIX_WORDS: usize = 20;
        const MAX_PREFIX_CHARS: usize = 200;
        let rank_cost = |e: &LatticeEdge| -> f32 {
            let raw = &lattice.input[e.start..e.end];
            e.base_cost - user_dict.user_boost(raw, &e.text).min(6.0) + if e.is_fuzzy { 1.5 } else { 0.0 }
        };
        let prefix_edges = if unpinned_start < n {
            lattice
                .edges_from
                .get(unpinned_start)
                .map(|v| v.as_slice())
                .unwrap_or(&[])
        } else {
            &[]
        };
        // 判断从 `unpinned_start` 起是否存在全拼多字母音节或全拼词组；
        // 若不存在（例如用户输入 `wzj` 等纯简拼串），则允许局部前缀简拼词（如 `wz` -> 我在/文章/位置/完整）进入前缀候选
        let has_full_pinyin_at_start = prefix_edges.iter().any(|e| {
            !e.is_fuzzy
                && (e.kind == EdgeKind::ChineseWord
                    || (e.kind == EdgeKind::ChineseChar && e.end > unpinned_start + 1))
        });

        // 补齐首音节在词库中的全部同音单字（或简拼首字母对应的全部常用单字）
        let first_syl_end = candidates
            .first()
            .and_then(|c| {
                c.syllable_spans
                    .iter()
                    .find(|s| s.start == unpinned_start)
                    .map(|s| s.end)
            })
            .or_else(|| {
                prefix_edges
                    .iter()
                    .filter(|e| e.kind == EdgeKind::ChineseChar && !e.is_fuzzy)
                    .map(|e| e.end)
                    .max()
            });
        let mut extra_char_edges: Vec<LatticeEdge> = Vec::new();
        if let Some(syl_end) = first_syl_end {
            if syl_end > unpinned_start && syl_end <= n {
                let lower_syl = lattice.input[unpinned_start..syl_end].to_ascii_lowercase();
                let exact = lexicon.syllable_trie.exact_lookup(&lower_syl);
                let (lookup_ids, is_abbrev_initial) = if exact.is_empty() && lower_syl.len() == 1 {
                    (lexicon.abbrev_trie.exact_lookup(&lower_syl), true)
                } else {
                    (exact, false)
                };
                for &id in lookup_ids {
                    let entry = &lexicon.entries[id as usize];
                    if entry.pinyin_syllables.len() == 1
                        && !prefix_edges
                            .iter()
                            .any(|e| e.end == syl_end && e.text == entry.text)
                    {
                        extra_char_edges.push(LatticeEdge {
                            start: unpinned_start,
                            end: syl_end,
                            text: entry.text.clone(),
                            code_key: if is_abbrev_initial {
                                lower_syl.clone()
                            } else {
                                entry.syllable_key.clone()
                            },
                            kind: EdgeKind::ChineseChar,
                            base_cost: entry.cost,
                            domain: entry.domain,
                            syllable_count: 1,
                            is_fuzzy: false,
                        });
                    }
                }
            }
        }

        let mut word_edges: Vec<&LatticeEdge> = Vec::new();
        let mut char_edges: Vec<&LatticeEdge> = Vec::new();
        for edge in prefix_edges.iter().chain(extra_char_edges.iter()) {
            if edge.text.is_empty() {
                continue;
            }
            // 在全拼句子中过滤局部前缀简拼（`shi` 里的 s'h → 上海）与英文片段（`zairen…` 里的 Zaire）；
            // 但在纯简拼输入（`!has_full_pinyin_at_start`，如 `wzj`）中保留前缀简拼词（`wz` → 我在/文章/位置/完整）
            if edge.end < n {
                if matches!(
                    edge.kind,
                    EdgeKind::EnglishWord
                        | EdgeKind::EnglishPrefixCompletion
                        | EdgeKind::EnglishRaw
                ) {
                    continue;
                }
                if edge.kind == EdgeKind::AbbrevPinyin && has_full_pinyin_at_start {
                    continue;
                }
            }
            let is_single_cjk = edge.text.chars().count() == 1 && !edge.text.is_ascii();
            if is_single_cjk {
                char_edges.push(edge);
            } else {
                word_edges.push(edge);
            }
        }
        // 词组：精确拼音优先于纠错词；覆盖音节数越多越靠前；同音节数按覆盖长度与词频（含个人记忆）排序
        word_edges.sort_by(|a, b| {
            a.is_fuzzy
                .cmp(&b.is_fuzzy)
                .then_with(|| b.syllable_count.cmp(&a.syllable_count))
                .then_with(|| b.end.cmp(&a.end))
                .then_with(|| {
                    rank_cost(a)
                        .partial_cmp(&rank_cost(b))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });
        // 单字：精确拼音的单字优先于纠错单字，再按字频排序
        char_edges.sort_by(|a, b| {
            a.is_fuzzy.cmp(&b.is_fuzzy).then_with(|| {
                rank_cost(a)
                    .partial_cmp(&rank_cost(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });

        let min_beam_score = candidates
            .iter()
            .map(|c| c.final_score)
            .fold(90.0_f32, f32::min);
        let max_beam_cost = candidates
            .iter()
            .map(|c| c.l1_cost)
            .fold(10.0_f32, f32::max);

        let mut rank = 0usize;
        let mut added_words = 0usize;

        // 1.0) 对齐微软拼音多段递进短语候选（如输入 `keyiyouhuadedif` 时，在首选整句「可以优化的地方」之后，
        //      依次提供递进前缀短语「可以优化的」「可以优化」，方便一键锁定前 N-1 / N-2 个词块）
        if has_full_pinyin_at_start {
            if let Some(best_cand) = candidates.first().cloned() {
                let unpinned_segs: Vec<SegmentSpan> = best_cand
                    .segment_spans
                    .iter()
                    .filter(|s| s.start >= unpinned_start && !s.text.is_empty())
                    .cloned()
                    .collect();
                if unpinned_segs.len() >= 3 {
                    let max_prog = if candidates.len() <= 1 { 2 } else { 1 };
                    let mut added_prog = 0usize;
                    let max_k = unpinned_segs.len().min(5);
                    for k in (2..max_k).rev() {
                        if added_prog >= max_prog {
                            break;
                        }
                        let slice_segs = &unpinned_segs[..k];
                        let prog_start = slice_segs.first().unwrap().start;
                        let prog_end = slice_segs.last().unwrap().end;
                        let mut prog_text = String::new();
                        let mut prev_ascii: Option<bool> = None;
                        for sp in slice_segs {
                            let cur_ascii = sp.text.is_ascii();
                            if let Some(pa) = prev_ascii {
                                if pa != cur_ascii || (pa && cur_ascii) {
                                    prog_text.push(' ');
                                }
                            }
                            prog_text.push_str(&sp.text);
                            prev_ascii = Some(cur_ascii);
                        }
                        if prog_text.is_empty() || !seen_surface.insert(prog_text.clone()) {
                            continue;
                        }
                        let prog_syls: Vec<SegmentSpan> = best_cand
                            .syllable_spans
                            .iter()
                            .filter(|s| s.start >= prog_start && s.end <= prog_end)
                            .cloned()
                            .collect();
                        let prog_preedit = prog_syls
                            .iter()
                            .map(|s| s.code_key.as_str())
                            .collect::<Vec<_>>()
                            .join("'");
                        candidates.push(Candidate {
                            text: prog_text.clone(),
                            preedit: prog_preedit,
                            consumed_len: prog_end.saturating_sub(prog_start),
                            l1_cost: max_beam_cost + 0.3 + rank as f32 * 0.05,
                            final_score: (min_beam_score - 1.0) - rank as f32 * 0.01,
                            ai_boosted: false,
                            tag: "[前缀选词]".to_string(),
                            segments: slice_segs.iter().map(|s| s.text.clone()).collect(),
                            segment_spans: slice_segs.to_vec(),
                            syllable_spans: prog_syls.clone(),
                        });
                        rank += 1;
                        added_prog += 1;

                        // 若该递进短语末尾词块含助词后缀（如「优化的」以「的/了/着/过」结尾），继续派生去除末尾助词的主干短语（如「可以优化」）
                        if added_prog < max_prog && prog_syls.len() >= 3 {
                            if let Some(last_seg) = slice_segs.last() {
                                if last_seg.text.chars().count() >= 2
                                    && last_seg.text.ends_with(['的', '了', '着', '过'])
                                {
                                    let stem_syls = prog_syls[..prog_syls.len() - 1].to_vec();
                                    let stem_end = stem_syls.last().unwrap().end;
                                    let last_char_len = last_seg.text.chars().last().unwrap().len_utf8();
                                    let stem_text = prog_text[..prog_text.len() - last_char_len].to_string();
                                    if !stem_text.is_empty()
                                        && stem_end > prog_start
                                        && seen_surface.insert(stem_text.clone())
                                    {
                                        let mut stem_segs = slice_segs.to_vec();
                                        if let Some(last_s) = stem_segs.last_mut() {
                                            last_s.end = stem_end;
                                            last_s.text = last_seg.text[..last_seg.text.len() - last_char_len].to_string();
                                        }
                                        let stem_preedit = stem_syls
                                            .iter()
                                            .map(|s| s.code_key.as_str())
                                            .collect::<Vec<_>>()
                                            .join("'");
                                        candidates.push(Candidate {
                                            text: stem_text,
                                            preedit: stem_preedit,
                                            consumed_len: stem_end.saturating_sub(prog_start),
                                            l1_cost: max_beam_cost + 0.3 + rank as f32 * 0.05,
                                            final_score: (min_beam_score - 1.0) - rank as f32 * 0.01,
                                            ai_boosted: false,
                                            tag: "[前缀选词]".to_string(),
                                            segments: stem_segs.iter().map(|s| s.text.clone()).collect(),
                                            segment_spans: stem_segs,
                                            syllable_spans: stem_syls,
                                        });
                                        rank += 1;
                                        added_prog += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let mut push_prefix = |edge: &LatticeEdge, rank: usize, candidates: &mut Vec<Candidate>| -> bool {
            if !seen_surface.insert(edge.text.clone()) {
                return false;
            }
            let is_full_input = edge.start == 0 && edge.end == n;
            let syl_spans = split_edge_into_syllables(&lattice.input, edge);
            let tag = if edge.is_fuzzy {
                format!("[纠错 {}]", edge.code_key)
            } else if is_full_input {
                "[词库]".to_string()
            } else if edge.end == n {
                "[余下整句]".to_string()
            } else {
                "[前缀选词]".to_string()
            };
            candidates.push(Candidate {
                text: edge.text.clone(),
                preedit: edge.code_key.clone(),
                consumed_len: edge.end.saturating_sub(edge.start),
                l1_cost: (edge.base_cost + if is_full_input { 3.5 } else { 8.0 })
                    .max(max_beam_cost + 0.5 + rank as f32 * 0.05),
                // 分步候选与补齐单字的展示顺序由这里决定，且其分数必须严格低于所有整句/整词 Beam 候选，
                // 绝不允许第 13..30 个补齐的生僻单字反超前 12 个高频常用单字！
                final_score: (min_beam_score - 1.0) - rank as f32 * 0.01,
                ai_boosted: false,
                tag,
                segments: vec![edge.text.clone()],
                segment_spans: vec![SegmentSpan {
                    start: edge.start,
                    end: edge.end,
                    text: edge.text.clone(),
                    code_key: edge.code_key.clone(),
                }],
                syllable_spans: syl_spans,
            });
            true
        };

        // 若当前为 3 字母及以上的纯简拼输入（如 `wzj`），将前 2 个最高频整词（如「为自己」「我自己」）保留在最前，
        // 随后立即穿插高频 2 字简拼前缀词（如「文章」「网站」「位置」「文字」「我在」「完整」），对齐微软拼音简拼体验
        let deferred_abbrev_full: Vec<Candidate> = if !has_full_pinyin_at_start && n >= 3 && candidates.len() > 2 {
            candidates.drain(2..).collect()
        } else {
            Vec::new()
        };

        // 1) 所有精确匹配的多字前缀词组（递进短语、三字词、双字词）一律排在单字之前！
        for &edge in &word_edges {
            if edge.is_fuzzy {
                continue;
            }
            if added_words >= MAX_PREFIX_WORDS {
                break;
            }
            if push_prefix(edge, rank, &mut candidates) {
                rank += 1;
                added_words += 1;
            }
        }
        // 1b) 追加其余完整简拼词条
        for mut cand in deferred_abbrev_full {
            cand.final_score = (min_beam_score - 1.0) - rank as f32 * 0.01;
            rank += 1;
            candidates.push(cand);
        }
        // 2) 首音节精确匹配的单字候选
        let mut added_chars = 0usize;
        for &edge in &char_edges {
            if edge.is_fuzzy {
                continue;
            }
            if added_chars >= MAX_PREFIX_CHARS {
                break;
            }
            if push_prefix(edge, rank, &mut candidates) {
                rank += 1;
                added_chars += 1;
            }
        }
        // 3) 模糊音/纠错词组与单字放到最后兜底
        for &edge in word_edges.iter().chain(char_edges.iter()) {
            if !edge.is_fuzzy {
                continue;
            }
            if push_prefix(edge, rank, &mut candidates) {
                rank += 1;
            }
        }

        let is_fuzzy_cand = |c: &Candidate| c.tag.starts_with("[纠错");

        // 正确输入优先保护（Exact-Input Protection）：
        // 若首选是纠错项，但存在用户原样拼音对应的整词（且词频差距不悬殊），则把原样整词放回首位。
        // 例：`sixiang` 必须是「思想」而不是纠错后的「实现」；`si` 必须是「死/四」而不是「是」。
        if candidates.first().is_some_and(|c| is_fuzzy_cand(c)) {
            let top_cost = candidates[0].l1_cost;
            if let Some(pos) = candidates.iter().position(|c| {
                c.consumed_len == n
                    && !is_fuzzy_cand(c)
                    && c.segments.len() == 1
                    && c.l1_cost <= top_cost + 3.0
            }) {
                let top_score = candidates[0].final_score;
                let mut item = candidates.remove(pos);
                item.final_score = top_score + 0.05;
                candidates.insert(0, item);
            }
        }

        // 拼音纠错备选位（Fuzzy Error-Correction Slot）：
        // 只为「整串对应一个完整词语（≥2 字）」的纠错项保留 1 个第一页位置（第 3 位），
        // 不为长句或单字纠错占位，避免把正确输入的同音词挤出第一页。
        let has_fuzzy_in_top3 = candidates.iter().take(3).any(|c| is_fuzzy_cand(c));
        if !has_fuzzy_in_top3 && candidates.len() > 3 {
            if let Some(pos) = candidates.iter().skip(3).position(|c| {
                c.consumed_len == n
                    && is_fuzzy_cand(c)
                    && c.segments.len() == 1
                    && c.text.chars().count() >= 2
            }) {
                let pos = pos + 3;
                let slot_score = candidates[1].final_score.min(candidates[0].final_score) - 0.01;
                let mut item = candidates.remove(pos);
                item.final_score = slot_score;
                candidates.insert(2, item);
            }
        }

        // 黄金第二候选位（Guaranteed 2nd Slot）：
        // 若整个输入串（3~10字母）恰好命中高频英文词表（如 `api`, `data`, `main`, `nice`, `bug`, `app`）
        // 且第一候选为中文，则强制将该英文词提升至至多第 2 候选位（下标 1）
        if config.golden_second_slot_english && (3..=10).contains(&n) && candidates.len() >= 2 {
            let lower_key = lattice.input.to_ascii_lowercase();
            let eng_ids = lexicon.english_trie.exact_lookup(&lower_key);
            if let Some(&top_eng_id) = eng_ids.first() {
                let eng_word = &lexicon.entries[top_eng_id as usize].text;
                if candidates[0].text != *eng_word {
                    if let Some(pos) = candidates.iter().position(|c| c.text == *eng_word) {
                        if pos > 1 {
                            let top_score = candidates[0].final_score;
                            let mut item = candidates.remove(pos);
                            item.tag = "[黄金英文位]".to_string();
                            item.final_score = top_score - 0.08;
                            candidates.insert(1, item);
                        }
                    }
                }
            }
        }

        // 个人输入记忆首选晋升（User Memory Top Promotion）：
        // 若用户针对当前完整编码高频输入或主动选择过某个字词句，或输入简拼匹配到历史全拼输入过的词条，直接将其置顶！
        if pinned_segments.is_empty() {
            let mut user_recs = user_dict.top_user_records(&lattice.input);
            let mut jianpin_set = HashSet::new();
            for jrec in user_dict.top_jianpin_records(&lattice.input) {
                jianpin_set.insert(jrec.text.clone());
                if !user_recs.iter().any(|r| r.text == jrec.text) {
                    user_recs.push(jrec);
                }
            }
            for urec in user_recs.into_iter().take(4).rev() {
                let is_jp_recall = jianpin_set.contains(&urec.text);
                if urec.dynamic_score >= 4.5
                    || urec.commit_count >= 2
                    || (is_jp_recall && urec.commit_count >= 1)
                {
                    let mem_tag = format!("[记忆·{}次]", urec.commit_count);
                    if let Some(pos) = candidates.iter().position(|c| c.text == urec.text) {
                        let mut item = candidates.remove(pos);
                        item.tag = mem_tag;
                        item.final_score = 135.0 + urec.dynamic_score;
                        item.l1_cost = 0.05;
                        candidates.insert(0, item);
                    } else {
                        let fallback_syls = candidates
                            .first()
                            .map(|c| c.syllable_spans.clone())
                            .unwrap_or_else(|| {
                                vec![SegmentSpan {
                                    start: 0,
                                    end: n,
                                    text: urec.text.clone(),
                                    code_key: lattice.input.clone(),
                                }]
                            });
                        candidates.insert(
                            0,
                            Candidate {
                                text: urec.text.clone(),
                                preedit: lattice.input.clone(),
                                consumed_len: n,
                                l1_cost: 0.05,
                                final_score: 135.0 + urec.dynamic_score,
                                ai_boosted: false,
                                tag: mem_tag,
                                segments: vec![urec.text.clone()],
                                segment_spans: vec![SegmentSpan {
                                    start: 0,
                                    end: n,
                                    text: urec.text,
                                    code_key: lattice.input.clone(),
                                }],
                                syllable_spans: fallback_syls,
                            },
                        );
                    }
                } else if urec.commit_count >= 1 && urec.dynamic_score >= 2.0 {
                    // 即使仅用空格确认过 1 次，若该词已排在首位，也打上个人记忆标识，防止被后台大模型误改
                    if let Some(first) = candidates.first_mut() {
                        if first.text == urec.text {
                            first.tag = format!("[记忆·{}次]", urec.commit_count);
                            first.final_score = 130.0 + urec.dynamic_score;
                        }
                    }
                }
            }
        }

        candidates
    }
}

/// 判断单字是否为现代汉语常见语法虚词/代词/介词/助词/方位词（在长句中合法独立成段，不施加孤立单字碎片惩罚）
#[inline]
fn is_common_sentence_particle(ch: &str) -> bool {
    matches!(
        ch,
        "的" | "了" | "在" | "是" | "有" | "和" | "与" | "或" | "但" | "而" | "把" | "被"
            | "给" | "让" | "向" | "从" | "到" | "对" | "于" | "上" | "下" | "中" | "里"
            | "内" | "外" | "前" | "后" | "不" | "没" | "很" | "更" | "最" | "也" | "都"
            | "就" | "才" | "又" | "再" | "还" | "吗" | "呢" | "吧" | "啊" | "呀" | "着"
            | "过" | "得" | "地" | "我" | "你" | "他" | "她" | "它" | "这" | "那" | "哪"
            | "谁" | "个" | "去" | "来" | "看" | "说" | "做" | "用" | "能" | "会" | "要"
            | "想" | "多" | "少" | "大" | "小" | "好" | "新" | "老" | "高" | "低"
    )
}

/// 智能盘古之白格式化：在中英文 Token 交界处自动插入半角空格
fn format_segments(edges: &[LatticeEdge], auto_space: bool) -> String {
    let mut out = String::new();
    let mut prev_is_ascii: Option<bool> = None;

    for edge in edges {
        if edge.text.is_empty() {
            continue;
        }
        let curr_is_ascii = edge.text.is_ascii();
        if auto_space {
            if let Some(prev_ascii) = prev_is_ascii {
                // 中英交界或两个独立英文单词之间自动加空格
                if prev_ascii != curr_is_ascii || (prev_ascii && curr_is_ascii) {
                    out.push(' ');
                }
            }
        } else if prev_is_ascii == Some(true) && curr_is_ascii {
            out.push(' ');
        }
        out.push_str(&edge.text);
        prev_is_ascii = Some(curr_is_ascii);
    }
    out
}

/// 从光标前上文中提取末尾若干个关键词，用于跨句 Bigram 衔接
fn extract_context_tail_words(context: &str) -> Vec<String> {
    let trimmed = context.trim_end_matches(|c: char| c.is_whitespace() || "，。！？、：；,.!?:;\"'“”（）()".contains(c));
    if trimmed.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let len = chars.len();
    let mut tails = Vec::new();
    for window in [4usize, 3, 2, 1] {
        if len >= window {
            let slice: String = chars[len - window..].iter().collect();
            tails.push(slice);
        }
    }
    tails
}

/// 将单条晶格图边（如多字中文词 `0..9 "wo'shi'yong" -> "我使用"`）拆解为逐个独立音节区间
/// 这样预编辑音节栏中的每一个拼音音节都有精确对应的 `[start..end]` 字节范围与单字
fn split_edge_into_syllables(input: &str, edge: &LatticeEdge) -> Vec<SegmentSpan> {
    if !edge.code_key.contains('\'') {
        let raw_code = input
            .get(edge.start..edge.end)
            .unwrap_or(edge.code_key.as_str())
            .trim_matches('\'');
        return vec![SegmentSpan {
            start: edge.start,
            end: edge.end,
            text: edge.text.clone(),
            code_key: if raw_code.is_empty() {
                edge.code_key.clone()
            } else {
                raw_code.to_string()
            },
        }];
    }

    let syls: Vec<&str> = edge
        .code_key
        .split('\'')
        .filter(|s| !s.is_empty())
        .collect();
    let chars: Vec<char> = edge.text.chars().collect();
    let total_syl_len: usize = syls.iter().map(|s| s.len()).sum();
    let span_len = edge.end.saturating_sub(edge.start);

    let mut out = Vec::with_capacity(syls.len());
    let mut cur = edge.start;

    for (idx, &syl) in syls.iter().enumerate() {
        while cur < edge.end && input.as_bytes().get(cur) == Some(&b'\'') {
            cur += 1;
        }
        let next_end = if idx + 1 == syls.len() {
            edge.end
        } else if edge.kind == EdgeKind::AbbrevPinyin || span_len == syls.len() {
            // 首字母简拼词条（如 `wzj` -> `wei'zi'ji`）：每个音节固定对应 1 个输入字母
            (cur + 1).min(edge.end)
        } else if total_syl_len == span_len {
            (cur + syl.len()).min(edge.end)
        } else {
            // 若存在混拼/半拼（如 `dif` -> `di'fang`）或模糊音导致长度不等，为后续每个音节至少预留 1 字节，并优先匹配原音节前缀
            let rem = &input[cur..edge.end];
            let rem_syls_after = syls.len().saturating_sub(idx + 1);
            let max_try = rem.len().saturating_sub(rem_syls_after).min(6);
            let mut matched_len = 1.min(rem.len());
            if syl.len() <= max_try && rem[..max_try].starts_with(syl) {
                matched_len = syl.len();
            } else {
                for try_len in (1..=max_try).rev() {
                    if crate::pinyin::is_valid_syllable(&rem[..try_len]) {
                        matched_len = try_len;
                        break;
                    }
                }
            }
            (cur + matched_len).min(edge.end)
        };
        if next_end > cur {
            let actual_code = input
                .get(cur..next_end)
                .unwrap_or(syl)
                .trim_matches('\'')
                .to_string();
            let ch_text = chars
                .get(idx)
                .map(|c| c.to_string())
                .unwrap_or_else(|| edge.text.clone());
            out.push(SegmentSpan {
                start: cur,
                end: next_end,
                text: ch_text,
                code_key: if actual_code.is_empty() {
                    syl.to_string()
                } else {
                    actual_code
                },
            });
            cur = next_end;
        }
    }

    if out.is_empty() {
        out.push(SegmentSpan {
            start: edge.start,
            end: edge.end,
            text: edge.text.clone(),
            code_key: edge.code_key.clone(),
        });
    }
    out
}

