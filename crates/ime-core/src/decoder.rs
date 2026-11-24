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
    /// 常见易读错字词的正确读音提示（如输 `hundun` 出「馄饨」时提示 `"馄 (hun) 饨 (tun)"`）；常规词为 `None`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pronunciation_hint: Option<String>,
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
        let has_exact_whole_word = pinned_segments.is_empty()
            && lattice.edges_from.first().is_some_and(|edges| {
                edges
                    .iter()
                    .any(|e| e.end == n && !e.is_fuzzy && e.kind == EdgeKind::ChineseWord)
            });
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
                    // 优先复用晶格图中同起止区间的带音节分隔符 code_key（如 "shi'yong"）；
                    // 单字锁定（如在长句 `dongyue` 中锁定单字「岳」）绝不能借用双音节强拆边 `yu'e` 的 code_key
                    let pin_char_cnt = pin_text.chars().count();
                    let matched_code_key = lattice.edges_from[i]
                        .iter()
                        .find(|e| e.end == pin_end && e.text == *pin_text)
                        .or_else(|| {
                            (!pin_text.is_ascii() && pin_char_cnt >= 2).then(|| {
                                lattice.edges_from[i].iter().find(|e| {
                                    e.end == pin_end
                                        && e.code_key.contains('\'')
                                        && e.syllable_count == pin_char_cnt
                                })
                            })?
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
                        pronunciation_hint: None,
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
                        for jrec in user_dict
                            .top_jianpin_records(raw_slice)
                            .into_iter()
                            .chain(user_dict.top_prefix_records(raw_slice))
                        {
                            if !user_recs.iter().any(|r| r.text == jrec.text) {
                                user_recs.push(jrec);
                            }
                        }
                    }
                    if user_recs.is_empty() {
                        continue;
                    }
                    for urec in user_recs.into_iter().take(4) {
                        let char_cnt = urec.text.chars().count();
                        let matched_code_key = (!urec.text.is_ascii() && char_cnt >= 2)
                            .then(|| {
                                edges_at_i
                                    .iter()
                                    .find(|e| {
                                        e.end == end_pos
                                            && e.code_key.contains('\'')
                                            && e.syllable_count == char_cnt
                                    })
                                    .map(|e| e.code_key.clone())
                            })
                            .flatten()
                            .unwrap_or_else(|| raw_slice.to_string());
                        // 晶格图内部仅注入 <= 4 字的词汇级用户记忆（如人名「武增吉」、专有名词「文摘集」），
                        // 5 字及以上的长短语/整句交由末尾 User Memory Top Promotion 处理，避免将多段整句坍缩成单段而丢失递进前缀候选
                        if char_cnt > 4 && !urec.text.is_ascii() {
                            continue;
                        }
                        if urec.dynamic_score >= 4.5
                            || (urec.commit_count >= 2 && urec.dynamic_score >= 2.2)
                            || (is_whole_input
                                && urec.code_key != raw_slice
                                && urec.commit_count >= 1
                                && urec.dynamic_score >= 1.5)
                        {
                            // 若晶格图中该区间已存在同名非纠错边（如常用词「怎么」「时候」「选词」「右下角」已在词库中），
                            // 绝不重复塞入额外边（其词频记忆统一由下方 `user_boost` 扣减）。
                            if edges_at_i
                                .iter()
                                .any(|e| !e.is_fuzzy && e.end == end_pos && e.text == urec.text)
                            {
                                continue;
                            }
                            // 对词库外的个人自造词（或超出词库截断的冷门同音词），以该跨度上词库最优精确词的 `base_cost + 0.55`
                            // 作为统一起跑线（无词库词时按词库规模取标准基准），使其完全依靠真实衰减后的 `user_boost` 公平竞争，
                            // 彻底杜绝陈旧误输词（如多天前误输 2 次的「右下叫」）凭固定低 `base_cost` 压过词库正词「右下角」！
                            let best_exact_span_cost = lattice.edges_from[i]
                                .iter()
                                .filter(|e| {
                                    !e.is_fuzzy && e.end == end_pos && e.kind == EdgeKind::ChineseWord
                                })
                                .map(|e| e.base_cost)
                                .fold(None, |acc: Option<f32>, c| {
                                    Some(acc.map_or(c, |a| a.min(c)))
                                });
                            let default_custom_cost = if is_whole_input {
                                4.2
                            } else if lexicon.entry_count() > 50_000 {
                                9.2
                            } else {
                                5.2
                            };
                            let injected_base_cost = best_exact_span_cost
                                .map(|c| c + 0.55)
                                .unwrap_or(default_custom_cost);
                            let is_ascii = urec.text.is_ascii();
                            let hint = crate::corrector::lookup_hint_flat(raw_slice, &urec.text);
                            edges_at_i.push(LatticeEdge {
                                start: i,
                                end: end_pos,
                                text: urec.text,
                                code_key: matched_code_key.clone(),
                                kind: if is_ascii {
                                    EdgeKind::EnglishWord
                                } else if char_cnt >= 2 || urec.code_key.len() > 3 {
                                    EdgeKind::ChineseWord
                                } else {
                                    EdgeKind::ChineseChar
                                },
                                base_cost: injected_base_cost,
                                domain: DomainTag::General,
                                syllable_count: if is_ascii { 1 } else { char_cnt.max(1) },
                                is_fuzzy: false,
                                pronunciation_hint: hint,
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

                // 个人动态记忆词频加成（Time-Decay User Boost）：
                // 1) 整串完全匹配的词条（`edge_is_whole`）享受完整个人记忆提权；
                // 2) 若整串本身已是词库中的精确词组（`has_exact_whole_word`，如 `zenmehuishi` -> `怎么回事`），
                //    严格限制内部局部子词块（如 `怎么`、`会是`）的历史词频加成，杜绝两个常用 2 字子词拼凑反超完整的 4 字词条；
                // 3) 任何局部子词块扣减 `user_boost` 后的基础代价均设有正数下界（`>= 1.2`），严禁步长代价变负。
                let raw_input_slice = &lattice.input[edge.start..edge.end];
                let code_boost = if edge.code_key.bytes().filter(|&b| b != b'\'').count()
                    > raw_input_slice.len()
                {
                    user_dict.user_boost(&edge.code_key, &edge.text)
                } else {
                    0.0
                };
                let raw_boost = user_dict
                    .user_boost(raw_input_slice, &edge.text)
                    .max(code_boost);
                let edge_is_whole = edge.start == 0 && edge.end == n;
                if edge_is_whole {
                    let user_boost = raw_boost.min(6.0);
                    edge_step_cost = (edge_step_cost - user_boost).max(0.15);
                } else if edge.syllable_count <= 1 && edge.kind == EdgeKind::ChineseChar {
                    let user_boost = (raw_boost * 0.1).min(0.6);
                    edge_step_cost = (edge_step_cost - user_boost).max(1.8);
                } else if has_exact_whole_word {
                    let user_boost = (raw_boost * 0.2).min(0.9);
                    edge_step_cost = (edge_step_cost - user_boost).max(2.2);
                } else {
                    let user_boost = raw_boost.min(4.5);
                    edge_step_cost = (edge_step_cost - user_boost).max(1.2);
                }

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
                // 注意：若多音节词条实为无隔音符强拆单个合法音节（如 `jie -> ji'e 饥饿`、`yue -> yu'e 余额`），
                // 其在输入串上与单音节汉字跨越完全相同的区间，不能享受多音节跨步折扣（否则在 `sijie`/`dongyue` 中会反超单字 2.70 分）。
                let edge_splits_single_syl = edge.kind == EdgeKind::ChineseWord
                    && edge.syllable_count >= 2
                    && crate::pinyin::is_unapostrophed_single_syllable(raw_input_slice);
                let step_transition_penalty = match edge.kind {
                    EdgeKind::ChineseWord if !edge_splits_single_syl => 0.55,
                    EdgeKind::EnglishWord => 0.65,
                    EdgeKind::ChineseChar | EdgeKind::ChineseWord => {
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
                    let user_bg = if has_exact_whole_word && !edge_is_whole {
                        (user_dict.bigram_boost(prev_word, &edge.text) * 0.25).min(0.8)
                    } else {
                        user_dict.bigram_boost(prev_word, &edge.text)
                    };
                    let mut bg_bonus = lexicon.bigram_bonus(prev_word, &edge.text) + user_bg;
                    if state.edges.is_empty() {
                        for ctx_w in &context_tail {
                            bg_bonus = bg_bonus.max(lexicon.bigram_bonus(ctx_w, &edge.text));
                        }
                    }
                    bg_bonus += grammar_transition_bonus(state, prev_word, edge, n);

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
            // 若多段拼接路径的表面文本恰好就是词库中覆盖整串 [0..n] 的单个精确词条（如「怎么回事」「不在意了」），
            // 则归一为单段词库词条，防止后续 `candidates.retain` 将其误判为多段整句而剔除。
            let whole_lex_edge = if st.edges.len() > 1 && pinned_segments.is_empty() {
                lattice.edges_from.first().and_then(|edges| {
                    edges.iter().find(|e| {
                        e.end == n
                            && !e.is_fuzzy
                            && e.kind == EdgeKind::ChineseWord
                            && e.text == formatted
                    })
                })
            } else {
                None
            };
            let effective_edges: Vec<LatticeEdge> = if let Some(lex_e) = whole_lex_edge {
                vec![lex_e.clone()]
            } else {
                st.edges
            };

            let preedit = effective_edges
                .iter()
                .filter(|e| !e.text.is_empty())
                .map(|e| e.code_key.as_str())
                .collect::<Vec<_>>()
                .join(" ");

            let has_en = effective_edges.iter().any(|e| {
                matches!(
                    e.kind,
                    EdgeKind::EnglishWord
                        | EdgeKind::EnglishPrefixCompletion
                        | EdgeKind::EnglishRaw
                )
            });
            let has_cn = effective_edges
                .iter()
                .any(|e| matches!(e.kind, EdgeKind::ChineseWord | EdgeKind::ChineseChar | EdgeKind::AbbrevPinyin));
            let has_fuzzy = effective_edges.iter().any(|e| e.is_fuzzy);

            let non_empty_edges: Vec<&LatticeEdge> = effective_edges
                .iter()
                .filter(|e| !e.text.is_empty())
                .collect();
            let pronunciation_hint = if non_empty_edges.len() == 1 {
                non_empty_edges[0].pronunciation_hint.map(str::to_string)
            } else if formatted.chars().count() <= 6 {
                let mut hints = non_empty_edges.iter().filter_map(|e| e.pronunciation_hint);
                match (hints.next(), hints.next()) {
                    (Some(h), None) => Some(h.to_string()),
                    _ => None,
                }
            } else {
                None
            };

            let tag = if has_fuzzy {
                format!("[纠错 {}]", preedit)
            } else if non_empty_edges.len() == 1 && non_empty_edges[0].pronunciation_hint.is_some() {
                if crate::corrector::is_near_synonym_misread(&non_empty_edges[0].code_key, &formatted) {
                    "[错音·近义]".to_string()
                } else {
                    "[错音]".to_string()
                }
            } else if has_cn && has_en {
                "[中英混输]".to_string()
            } else if has_en {
                "[英文]".to_string()
            } else if effective_edges.iter().any(|e| e.kind == EdgeKind::AbbrevPinyin) {
                "[简拼]".to_string()
            } else if effective_edges.len() > 1 {
                "[整句]".to_string()
            } else {
                "[词库]".to_string()
            };

            let segments = effective_edges
                .iter()
                .filter(|e| !e.text.is_empty())
                .map(|e| e.text.clone())
                .collect();

            let segment_spans = effective_edges
                .iter()
                .filter(|e| !e.text.is_empty())
                .map(|e| SegmentSpan {
                    start: e.start,
                    end: e.end,
                    text: e.text.clone(),
                    code_key: e.code_key.clone(),
                })
                .collect();

            let syllable_spans = effective_edges
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
                pronunciation_hint,
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
        // - 覆盖全部输入的单一词条（如 `xuanci` → 选词/选此，`zenmehuishi` → 怎么回事/怎么会是）都是合法同音词，全部保留在最前；
        // - 多段拼接的整句：除第 1 名首选整句外，将代价接近的前 2 条备选整句暂存至 `alt_sentences`：
        //   一方面用于提取句首第二词块同音对比前缀（如首选「现在再研究一下」时提取对比前缀「现在在」），
        //   另一方面保留在候选池供 L2 端侧 AI / 大模型做语境与语法逆转重排，同时不挤占第一页前缀词位置。
        const MAX_BACKUP_SENTENCES: usize = 3;
        const SENTENCE_COST_MARGIN: f32 = 3.5;
        let mut alt_sentences: Vec<Candidate> = Vec::new();
        if let Some((best_cost, best_seg_cnt, best_syl_cnt, best_is_ascii)) =
            candidates.first().map(|c| {
                (
                    c.l1_cost,
                    c.segments.len(),
                    c.syllable_spans.len(),
                    c.text.is_ascii(),
                )
            })
        {
            let first_exact_pos = if candidates.first().is_some_and(|c| c.tag.starts_with("[纠错")) {
                candidates
                    .iter()
                    .position(|c| !c.tag.starts_with("[纠错") && !c.text.is_ascii())
            } else {
                None
            };
            let best_exact = candidates
                .iter()
                .find(|c| !c.tag.starts_with("[纠错") && !c.text.is_ascii())
                .map(|c| (c.l1_cost, c.segments.len(), c.syllable_spans.len()));
            let mut has_diff_syl_backup = false;
            let mut idx = 0usize;
            candidates.retain(|c| {
                let cur_idx = idx;
                let is_first = cur_idx == 0;
                idx += 1;
                if is_first || Some(cur_idx) == first_exact_pos || c.segments.len() <= 1 {
                    return true;
                }
                if c.tag.starts_with("[纠错") {
                    return false;
                }
                let (ref_cost, ref_seg_cnt, ref_syl_cnt) =
                    best_exact.unwrap_or((best_cost, best_seg_cnt, best_syl_cnt));
                let is_pure_full_pinyin_cn = !best_is_ascii
                    && c.tag == "[整句]"
                    && c.segment_spans.iter().all(|sp| {
                        sp.code_key.bytes().filter(|&b| b != b'\'').count()
                            == sp.end.saturating_sub(sp.start)
                    })
                    && c.syllable_spans
                        .iter()
                        .all(|sy| crate::pinyin::is_valid_or_alias_syllable(&sy.code_key));
                let is_diff_syl_split = is_pure_full_pinyin_cn
                    && (c.syllable_spans.len() > best_syl_cnt
                        || c.syllable_spans.len() > ref_syl_cnt);
                let effective_margin = if is_pure_full_pinyin_cn && c.segments.len() <= best_seg_cnt {
                    4.2
                } else {
                    SENTENCE_COST_MARGIN
                };
                let standard_ok = c.segments.len() <= best_seg_cnt + 1
                    && c.l1_cost <= best_cost + effective_margin;
                // 跨音节元音拆分短语召回（如 `sijie -> 司机饿`、`sijiele -> 司机饿了`、`laosijie -> 老司机饿`）：
                // 1) 若同段数非纠错路径因首选是纠错词而被拉大差距（如 `sijiele` 首选纠错「世界了」，
                //    精确路径「司机饿了」与「四届了」代价接近），按非纠错基准 `ref_cost` 放行；
                // 2) 若高频多字词跨元音切分比单段整词多出 1 段（如 `sijie` 中「司机+饿」对比单段「四届」），
                //    多 1 段天然多计一次字频与步长惩罚；当其包含真实多字词、末段非语气叹词且单段均价足够低时，保留 1 个 `[分词备选]`。
                let split_phrase_ok = is_diff_syl_split
                    && !has_diff_syl_backup
                    && c.l1_cost <= best_cost + 13.5
                    && if c.segments.len() <= ref_seg_cnt {
                        c.l1_cost <= ref_cost + SENTENCE_COST_MARGIN
                    } else if c.segments.len() == ref_seg_cnt + 1 {
                        let has_multi_char_word = c
                            .segments
                            .iter()
                            .any(|w| w.chars().count() >= 2 && !w.is_ascii());
                        let ends_with_filler = c.segments.last().is_some_and(|w| {
                            matches!(
                                w.as_str(),
                                "啊" | "哦" | "喔" | "噢" | "呃" | "嗯" | "阿" | "呵"
                            )
                        });
                        has_multi_char_word
                            && !ends_with_filler
                            && c.l1_cost <= ref_cost + 10.5
                            && c.l1_cost / (c.segments.len() as f32) <= 11.3
                    } else {
                        false
                    };
                if (alt_sentences.len() < MAX_BACKUP_SENTENCES && standard_ok) || split_phrase_ok {
                    let mut backup = c.clone();
                    if is_diff_syl_split && !has_diff_syl_backup {
                        has_diff_syl_backup = true;
                        backup.tag = "[分词备选]".to_string();
                    } else {
                        backup.tag = "[整句备选]".to_string();
                    }
                    alt_sentences.push(backup);
                }
                false
            });
            // 同步清理 `seen_surface`：被 `candidates.retain` 剔除的多段整句不能继续占用 `seen_surface`，
            // 否则会阻塞后续 `word_edges` 推入同名词库词条。
            seen_surface.retain(|txt| candidates.iter().any(|c| &c.text == txt));
        }

        // 整句候选之后，严格按照主流输入法层次依次给出从 `unpinned_start` 起算的：
        //   1) 所有精确匹配的多字前缀词组（三字词/双字词排在单字之前，如 `nizaifansipaichayixia` → 你在 / 你再 / 拟在；`wzj` → 文章 / 网站 / 位置 / 我在 / 完整）
        //   2) 首音节全部同音单字（按字频排序，如 你 / 尼 / 泥 / 逆 / 拟 …；或简拼首字母单字 我 / 为 / 无 / 问 / 外 / 王 …）
        //   3) 模糊音/纠错词组与单字
        const MAX_PREFIX_WORDS: usize = 20;
        const MAX_PREFIX_CHARS: usize = 200;
        /// 拆短前缀音节（如 `jie` 里的 `ji`）的单字另算上限，只在单音节或句末仅剩一个音节时出现
        const MAX_SPLIT_PREFIX_CHARS: usize = 200;
        let prev_pinned_word: Option<&str> = if unpinned_start > 0 {
            pinned_segments
                .iter()
                .filter(|(_, (end, _))| *end <= unpinned_start)
                .max_by_key(|(_, (end, _))| *end)
                .map(|(_, (_, txt))| txt.as_str())
        } else {
            context_tail.last().map(|s| s.as_str())
        };
        let rank_cost = |e: &LatticeEdge| -> f32 {
            let raw = &lattice.input[e.start..e.end];
            let ctx_bonus = if let Some(pw) = prev_pinned_word {
                lexicon.bigram_bonus(pw, &e.text)
                    + user_dict.bigram_boost(pw, &e.text)
                    + word_pair_collocation_bonus(pw, e)
            } else {
                0.0
            };
            let code_boost = if e.code_key.bytes().filter(|&b| b != b'\'').count() > raw.len() {
                user_dict.user_boost(&e.code_key, &e.text)
            } else {
                0.0
            };
            let u_boost = user_dict.user_boost(raw, &e.text).max(code_boost).min(6.0);
            e.base_cost - u_boost - ctx_bonus + if e.is_fuzzy { 1.5 } else { 0.0 }
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
        let mut split_char_edges: Vec<LatticeEdge> = Vec::new();
        if let Some(syl_end) = first_syl_end {
            if syl_end > unpinned_start && syl_end <= n {
                let lower_syl = lattice.input[unpinned_start..syl_end].to_ascii_lowercase();
                let canon_syl = crate::pinyin::canonical_syllable(&lower_syl);
                let exact = lexicon.syllable_trie().exact_lookup(canon_syl);
                let (lookup_ids, is_abbrev_initial) = if exact.is_empty() && lower_syl.len() == 1 {
                    (lexicon.abbrev_trie().exact_lookup(&lower_syl), true)
                } else {
                    (exact, false)
                };
                for id in lookup_ids {
                    let entry = lexicon.entry(id);
                    let entry_text = entry.text();
                    if entry.syllable_count() == 1
                        && !prefix_edges
                            .iter()
                            .any(|e| e.end == syl_end && e.text == entry_text)
                    {
                        extra_char_edges.push(LatticeEdge {
                            start: unpinned_start,
                            end: syl_end,
                            text: entry_text.to_string(),
                            code_key: if is_abbrev_initial {
                                lower_syl.clone()
                            } else {
                                entry.syllable_key().to_string()
                            },
                            kind: EdgeKind::ChineseChar,
                            base_cost: entry.cost(),
                            domain: entry.domain(),
                            syllable_count: 1,
                            is_fuzzy: false,
                            pronunciation_hint: None,
                        });
                    }
                }
                // 单输单个可元音拆分的音节（如 `jie`/`yue`/`xian`），或前面都已选定、只剩这一个音节（如 `sijie` 选「司」后的 `jie`），
                // 在完整单音节全部同音字之后补齐拆短前缀音节（如 `ji`/`yu`/`xi`）的全部同音单字兜底；
                // 长句中间的音节不补（几百个单字每次按键都要过一遍 AI 重排，且用户点该音节时由逐音节选词补齐）
                let gap_end = pinned_segments
                    .keys()
                    .copied()
                    .find(|&ps| ps > unpinned_start)
                    .unwrap_or(n);
                if syl_end == gap_end
                    && syl_end > unpinned_start + 1
                    && crate::pinyin::is_unapostrophed_single_syllable(
                        &lattice.input[unpinned_start..syl_end],
                    )
                {
                    for short_end in ((unpinned_start + 1)..syl_end).rev() {
                        let short_raw = &lattice.input[unpinned_start..short_end];
                        let rem_raw = &lattice.input[short_end..syl_end];
                        if !crate::pinyin::is_unapostrophed_single_syllable(short_raw)
                            || !crate::pinyin::is_unapostrophed_single_syllable(rem_raw)
                        {
                            continue;
                        }
                        for e in prefix_edges {
                            if e.kind == EdgeKind::ChineseChar
                                && !e.is_fuzzy
                                && e.end == short_end
                                && !e.text.is_empty()
                            {
                                split_char_edges.push(e.clone());
                            }
                        }
                        let short_canon = crate::pinyin::canonical_syllable(short_raw);
                        for id in lexicon.syllable_trie().exact_lookup(short_canon) {
                            let entry = lexicon.entry(id);
                            let entry_text = entry.text();
                            if entry.syllable_count() == 1
                                && !split_char_edges
                                    .iter()
                                    .any(|e| e.end == short_end && e.text == entry_text)
                            {
                                split_char_edges.push(LatticeEdge {
                                    start: unpinned_start,
                                    end: short_end,
                                    text: entry_text.to_string(),
                                    code_key: entry.syllable_key().to_string(),
                                    kind: EdgeKind::ChineseChar,
                                    base_cost: entry.cost(),
                                    domain: entry.domain(),
                                    syllable_count: 1,
                                    is_fuzzy: false,
                                    pronunciation_hint: None,
                                });
                            }
                        }
                    }
                }
            }
        }

        let next_pin_start = pinned_segments
            .keys()
            .copied()
            .find(|&ps| ps > unpinned_start)
            .unwrap_or(n);
        let best_syl_spans: Vec<SegmentSpan> = candidates
            .first()
            .map(|c| c.syllable_spans.clone())
            .unwrap_or_default();
        let best_unpinned_syl_cnt = best_syl_spans
            .iter()
            .filter(|s| s.start >= unpinned_start && s.end <= next_pin_start)
            .count();

        let split_backup_lead: Option<(usize, usize, String)> = alt_sentences
            .iter()
            .find(|c| c.tag == "[分词备选]")
            .and_then(|c| c.segment_spans.iter().find(|s| s.start >= unpinned_start))
            .map(|s| (s.start, s.end, s.text.clone()));
        let mut word_edges: Vec<&LatticeEdge> = Vec::new();
        let mut char_edges: Vec<&LatticeEdge> = Vec::new();
        for edge in prefix_edges.iter().chain(extra_char_edges.iter()) {
            if edge.text.is_empty() {
                continue;
            }
            if !pinned_segments.is_empty()
                && pinned_segments
                    .iter()
                    .any(|(&p_start, &(p_end, _))| edge.start < p_end && edge.end > p_start)
            {
                continue;
            }
            // 在全拼句子中过滤局部前缀简拼（`shi` 里的 s'h → 上海）、英文片段（`zairen…` 里的 Zaire）、
            // 以及把首选音节切碎的半音节词（如 `zaijia` 里的 `0..5 zai'ji -> 在即` 切碎了 `3..6 jia`）；
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
                if has_full_pinyin_at_start
                    && first_syl_end.is_some_and(|se| edge.end < se)
                {
                    // 禁止把首音节切碎的半音节元音强拆词（如 `mian` 里的 `0..3 mi'a -> 米啊`、`kuai` 里的 `0..3 ku'a -> 苦啊`）进入前缀词
                    continue;
                }
                if has_full_pinyin_at_start
                    && edge.syllable_count >= best_unpinned_syl_cnt
                    && best_syl_spans
                        .iter()
                        .any(|s| s.start < edge.end && edge.end < s.end)
                {
                    // 仅当该高频词恰好是已保留的跨音节 `[分词备选]` 整句的首个词块时放行（如 `sijie` 的「司机饿」→「司机」），
                    // 余下元音拼不成通顺短语的切分（如 `dalie` 里的「搭理+e」、`zaiguojia` 里的「在国际+a」）仍然过滤
                    let allows_common_vowel_split_prefix = !edge.is_fuzzy
                        && edge.kind == EdgeKind::ChineseWord
                        && edge.syllable_count >= 2
                        && edge.base_cost < 9.5
                        && split_backup_lead
                            .as_ref()
                            .is_some_and(|(s, e, t)| *s == edge.start && *e == edge.end && *t == edge.text);
                    if !allows_common_vowel_split_prefix {
                        continue;
                    }
                }
            }
            let is_single_cjk = edge.text.chars().count() == 1 && !edge.text.is_ascii();
            let splits_first_single_syl = edge.kind == EdgeKind::ChineseWord
                && edge.syllable_count >= 2
                && first_syl_end == Some(edge.end)
                && crate::pinyin::is_unapostrophed_single_syllable(
                    &lattice.input[edge.start..edge.end],
                );
            if is_single_cjk || splits_first_single_syl {
                if is_single_cjk {
                    if let Some(syl_end) = first_syl_end {
                        if has_full_pinyin_at_start && edge.end != syl_end {
                            continue;
                        }
                    }
                }
                char_edges.push(edge);
            } else {
                word_edges.push(edge);
            }
        }
        let is_incomplete_tail = |e: &LatticeEdge| -> bool {
            e.kind == EdgeKind::ChineseWord
                && e.code_key.bytes().filter(|&b| b != b'\'').count() > e.end.saturating_sub(e.start)
        };
        // 词组：精确拼音优先于纠错词与半音节补全词；覆盖音节数越多越靠前；同音节数按覆盖长度与词频（含个人记忆与上文搭配）排序
        word_edges.sort_by(|a, b| {
            a.is_fuzzy
                .cmp(&b.is_fuzzy)
                .then_with(|| is_incomplete_tail(a).cmp(&is_incomplete_tail(b)))
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
        split_char_edges.sort_by(|a, b| {
            rank_cost(a)
                .partial_cmp(&rank_cost(b))
                .unwrap_or(std::cmp::Ordering::Equal)
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
                    .take_while(|s| {
                        !pinned_segments
                            .iter()
                            .any(|(&ps, &(pe, _))| s.start < pe && s.end > ps)
                    })
                    .cloned()
                    .collect();
                if unpinned_segs.len() >= 3 {
                    let max_prog = 2usize;
                    let mut added_prog = 0usize;
                    let max_k = unpinned_segs.len().min(5);
                    for k in (2..max_k).rev() {
                        if added_prog >= max_prog {
                            break;
                        }
                        let mut slice_segs = unpinned_segs[..k].to_vec();
                        // 当 `k == 2` 且第 2 个词块为单字「再」（如首选「现在再研究一下」的前两段 `["现在", "再"]`）时，
                        // 首选整句本身已包含「现在再」，此时将第 2 段替换为同音对比字「在」生成「现在在」，对齐微软拼音体验！
                        if k == 2 && slice_segs.get(1).is_some_and(|s| s.text == "再") {
                            slice_segs[1].text = "在".to_string();
                        }
                        let prog_start = slice_segs.first().unwrap().start;
                        let prog_end = slice_segs.last().unwrap().end;
                        let mut prog_text = String::new();
                        let mut prev_ascii: Option<bool> = None;
                        for sp in &slice_segs {
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
                            pronunciation_hint: None,
                            segments: slice_segs.iter().map(|s| s.text.clone()).collect(),
                            segment_spans: slice_segs.clone(),
                            syllable_spans: prog_syls.clone(),
                        });
                        rank += 1;
                        added_prog += 1;

                        // 若该递进短语末尾词块含助词后缀（如「优化的」以「的/了/着/过」结尾），继续派生去除末尾助词的主干短语（如「可以优化」）；
                        // 若末尾词块为「再+双字动词」（如「现在 + 再研究」），同步派生前 3 音节同音对比前缀「现在在」，对齐微软拼音体验！
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
                                            pronunciation_hint: None,
                                            segments: stem_segs.iter().map(|s| s.text.clone()).collect(),
                                            segment_spans: stem_segs,
                                            syllable_spans: stem_syls,
                                        });
                                        rank += 1;
                                        added_prog += 1;
                                    }
                                } else if (last_seg.text.chars().count() == 3
                                    && last_seg.text.starts_with(['再', '在']))
                                    || (slice_segs.first().is_some_and(|s0| {
                                        s0.text.chars().count() == 3
                                            && s0.text.ends_with(['再', '在'])
                                    }))
                                {
                                    let stem_syls = prog_syls[..3.min(prog_syls.len())].to_vec();
                                    let stem_end = stem_syls.last().unwrap().end;
                                    let first_two_chars: String =
                                        prog_text.chars().take(2).collect();
                                    let third_char = prog_text.chars().nth(2).unwrap_or('再');
                                    let alt_particle = if third_char == '再' { "在" } else { "再" };
                                    let stem_text = format!("{first_two_chars}{alt_particle}");
                                    let first_two_end = stem_syls
                                        .get(1)
                                        .map(|s| s.end)
                                        .unwrap_or(prog_start);
                                    let stem_segs = vec![
                                        SegmentSpan {
                                            start: prog_start,
                                            end: first_two_end,
                                            text: first_two_chars,
                                            code_key: stem_syls[..2.min(stem_syls.len())]
                                                .iter()
                                                .map(|s| s.code_key.as_str())
                                                .collect::<Vec<_>>()
                                                .join("'"),
                                        },
                                        SegmentSpan {
                                            start: first_two_end,
                                            end: stem_end,
                                            text: alt_particle.to_string(),
                                            code_key: stem_syls.last().unwrap().code_key.clone(),
                                        },
                                    ];
                                    if !stem_text.is_empty()
                                        && stem_end > prog_start
                                        && seen_surface.insert(stem_text.clone())
                                    {
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
                                            pronunciation_hint: None,
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
            } else if is_full_input && edge.pronunciation_hint.is_some() {
                if crate::corrector::is_near_synonym_misread(&edge.code_key, &edge.text) {
                    "[错音·近义]".to_string()
                } else {
                    "[错音]".to_string()
                }
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
                pronunciation_hint: edge.pronunciation_hint.map(str::to_string),
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
        // 随后立即穿插高频 2 字简拼前缀词（如「文章」「网站」「位置」「文字」「我在」「完整」），对齐微软拼音简拼体验；
        // 若当前为多音节全拼整词且无更短的多字前缀词（如双音节 `zaijia` 命中多个同音二字词），首屏保留前 3 个最高频同音整词（如「在家」「再加」「宅家」），
        // 随后穿插首音节最常用的前 2 个单字（如「在」「再」），确保第一页就能直接选前面的单字！
        let deferred_abbrev_full: Vec<Candidate> = if !has_full_pinyin_at_start && n >= 3 && candidates.len() > 2 {
            candidates.drain(2..).collect()
        } else {
            Vec::new()
        };
        let has_strict_prefix_words = best_unpinned_syl_cnt >= 3
            && word_edges
                .iter()
                .any(|e| !e.is_fuzzy && e.end < n && e.syllable_count < best_unpinned_syl_cnt);
        let deferred_beam_full: Vec<Candidate> = if has_full_pinyin_at_start
            && !has_strict_prefix_words
            && !char_edges.is_empty()
            && candidates.len() > 3
            && best_unpinned_syl_cnt >= 2
        {
            candidates.drain(3..).collect()
        } else {
            Vec::new()
        };

        // 1) 所有精确匹配的多字前缀词组（递进短语、三字词、双字词）排在单字之前；
        //    但若当前词组是覆盖整句末尾的同音整词（`edge.end >= n` 或双音节输入下的二字词），
        //    在首屏最多保留 3 个整词后先穿插首音节最常用的单字（如「在」「再」），确保第一页就能直接选前面的单字！
        let mut deferred_full_words: Vec<&LatticeEdge> = Vec::new();
        // 跨音节元音切分的高频前缀词（如 `sijie` 里的「司机」），与对应的 `[分词备选]` 整句一起放
        let mut split_prefix_words: Vec<&LatticeEdge> = Vec::new();
        let initial_full_count = candidates.iter().filter(|c| c.consumed_len >= n).count();
        let mut full_word_count = initial_full_count;
        for &edge in &word_edges {
            if edge.is_fuzzy {
                continue;
            }
            if added_words >= MAX_PREFIX_WORDS {
                break;
            }
            let cuts_best_syl = edge.end < n
                && edge.syllable_count >= best_unpinned_syl_cnt
                && best_syl_spans
                    .iter()
                    .any(|s| s.start < edge.end && edge.end < s.end);
            if has_full_pinyin_at_start && cuts_best_syl {
                split_prefix_words.push(edge);
                continue;
            }
            let is_full_syl_word = edge.end >= n || !has_strict_prefix_words;
            if has_full_pinyin_at_start && is_full_syl_word && full_word_count >= 3 {
                deferred_full_words.push(edge);
                continue;
            }
            if push_prefix(edge, rank, &mut candidates) {
                rank += 1;
                added_words += 1;
                if is_full_syl_word {
                    full_word_count += 1;
                }
            }
        }
        // 1b) 追加其余完整简拼词条
        for mut cand in deferred_abbrev_full {
            cand.final_score = (min_beam_score - 1.0) - rank as f32 * 0.01;
            rank += 1;
            candidates.push(cand);
        }
        let (split_backups, same_syl_backups): (Vec<Candidate>, Vec<Candidate>) = alt_sentences
            .into_iter()
            .partition(|c| c.tag == "[分词备选]");
        // 2) 首音节精确匹配的单字候选（若存在延后的同音整词，则在穿插前 2 个最高频单字后立即补回同音整词；
        //    跨音节元音拆分的整句备选与前缀词（如「司机饿」「司机」「那余额」）在前 6 个常用单字之后呈现：
        //    不占候选栏前几位，展开候选的第一屏就能看到，无需翻过几十个生僻单字）
        const SPLIT_BACKUP_AFTER_CHARS: usize = 6;
        let mut added_chars = 0usize;
        let mut deferred_flushed = false;
        let mut split_backups_flushed = false;
        let mut split_backups_iter = split_backups.into_iter();
        let mut deferred_beam_iter = deferred_beam_full.into_iter();
        let flush_split = |rank: &mut usize, candidates: &mut Vec<Candidate>,
                               push_prefix: &mut dyn FnMut(&LatticeEdge, usize, &mut Vec<Candidate>) -> bool,
                               iter: &mut std::vec::IntoIter<Candidate>| {
            for mut alt in iter.by_ref() {
                alt.final_score = (min_beam_score - 1.0) - *rank as f32 * 0.01;
                *rank += 1;
                candidates.push(alt);
            }
            for &w_edge in &split_prefix_words {
                if push_prefix(w_edge, *rank, candidates) {
                    *rank += 1;
                }
            }
        };
        for &edge in &char_edges {
            if edge.is_fuzzy {
                continue;
            }
            if !deferred_flushed && added_chars >= 2 {
                deferred_flushed = true;
                for mut cand in deferred_beam_iter.by_ref() {
                    cand.final_score = (min_beam_score - 1.0) - rank as f32 * 0.01;
                    rank += 1;
                    candidates.push(cand);
                }
                for &w_edge in &deferred_full_words {
                    if added_words >= MAX_PREFIX_WORDS {
                        break;
                    }
                    if push_prefix(w_edge, rank, &mut candidates) {
                        rank += 1;
                        added_words += 1;
                    }
                }
            }
            if !split_backups_flushed && added_chars >= SPLIT_BACKUP_AFTER_CHARS {
                split_backups_flushed = true;
                flush_split(&mut rank, &mut candidates, &mut push_prefix, &mut split_backups_iter);
            }
            if added_chars >= MAX_PREFIX_CHARS {
                break;
            }
            if push_prefix(edge, rank, &mut candidates) {
                rank += 1;
                added_chars += 1;
            }
        }
        if !deferred_flushed {
            for mut cand in deferred_beam_iter {
                cand.final_score = (min_beam_score - 1.0) - rank as f32 * 0.01;
                rank += 1;
                candidates.push(cand);
            }
            for &w_edge in &deferred_full_words {
                if added_words >= MAX_PREFIX_WORDS {
                    break;
                }
                if push_prefix(w_edge, rank, &mut candidates) {
                    rank += 1;
                    added_words += 1;
                }
            }
        }
        if !split_backups_flushed {
            flush_split(&mut rank, &mut candidates, &mut push_prefix, &mut split_backups_iter);
        }
        // 2b) 其余同音节数备选整句保留原始 l1_cost 供 L2 端侧 AI / 大模型语境重排，未触发 AI 逆转时排在单字之后
        for mut alt in same_syl_backups {
            alt.final_score = (min_beam_score - 1.0) - rank as f32 * 0.01;
            rank += 1;
            candidates.push(alt);
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
        // 4) 单音节可合法拆短的前缀单字（如 `jie -> ji`、`yue -> yu`、`xian -> xi` 的全部单字）放在最末，
        //    不挤占原样音节单字与模糊音纠错（如 `xian` 的纠错「想/向」）的位置
        for edge in split_char_edges.iter().take(MAX_SPLIT_PREFIX_CHARS) {
            if push_prefix(edge, rank, &mut candidates) {
                rank += 1;
            }
        }

        let is_fuzzy_cand = |c: &Candidate| c.tag.starts_with("[纠错");

        // 正确输入优先保护（Exact-Input Protection）：
        // 若首选是纠错项，但存在用户原样拼音对应的整词或同段数通顺精确整句（且词频/代价差距不悬殊），则把原样精确结果放回首位。
        // 例：`sixiang` 必须是「思想」而不是纠错后的「实现」；`chenxianzaiai` 必须是「趁现在 AI」而不是纠错后的「呈现再爱」；
        // `jiangquanyunxing` 应优先原样「奖券运行」而非模糊音「健全运行」。
        if candidates.first().is_some_and(|c| is_fuzzy_cand(c)) {
            let top_cost = candidates[0].l1_cost;
            let top_segs = candidates[0].segments.len();
            let clean_full = crate::pinyin::can_segment_as_clean_full_syllables(
                &lattice.input.to_ascii_lowercase(),
            );
            if let Some(pos) = candidates.iter().position(|c| {
                c.consumed_len == n
                    && !is_fuzzy_cand(c)
                    && ((c.segments.len() == 1 && c.l1_cost <= top_cost + 3.0)
                        || (clean_full
                            && c.segments.len() <= top_segs
                            && c.l1_cost <= top_cost + 2.2)
                        || (clean_full
                            && c.segments.len() == top_segs + 1
                            && c.l1_cost <= top_cost + 0.8))
            }) {
                let top_score = candidates[0].final_score;
                let mut item = candidates.remove(pos);
                item.final_score = top_score + 0.05;
                candidates.insert(0, item);
            }
        }

        // 拼音纠错备选位（Fuzzy Error-Correction Slot）：
        // 只为「整串对应一个完整词语（≥2 字）」的纠错项保留 1 个第一页位置（第 3 位），
        // 不为长句或单字（含单个合法完整音节）纠错占位，避免把正确输入的同音字词挤出第一页。
        let whole_input_is_single_syl =
            crate::pinyin::is_unapostrophed_single_syllable(&lattice.input);
        let has_fuzzy_in_top3 = candidates
            .iter()
            .take(3)
            .any(|c| is_fuzzy_cand(c) || c.pronunciation_hint.is_some());
        if !whole_input_is_single_syl && !has_fuzzy_in_top3 && candidates.len() > 3 {
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
                // 若插入第 3 位后导致前 4 位全是整词（`consumed_len == n`），
                // 而紧随其后的是单字前缀候选（`consumed_len < n`），
                // 则将顺延到第 4 位的同音整词移到前 2 个单字候选之后（第 2 页首位），
                // 保证第 1 页同时包含前 3 个整词与前 2 个首音节高频单字（如 `zaijia` 第 1 页同时有「在」「再」）。
                if candidates.len() > 4
                    && candidates[3].consumed_len == n
                    && candidates[4].consumed_len < n
                {
                    let mut fourth_full = candidates.remove(3);
                    let insert_after_chars = candidates
                        .iter()
                        .enumerate()
                        .skip(3)
                        .take_while(|(_, c)| c.consumed_len < n)
                        .take(2)
                        .last()
                        .map(|(idx, _)| idx + 1)
                        .unwrap_or(3);
                    let ref_score = candidates[insert_after_chars - 1].final_score;
                    fourth_full.final_score = (ref_score - 0.02).min(89.8);
                    candidates.insert(insert_after_chars, fourth_full);
                }
            }
        }

        // 黄金第二候选位（Guaranteed 2nd Slot）：
        // 若整个输入串（3~10字母）恰好命中高频英文词表（如 `api`, `data`, `main`, `nice`, `bug`, `app`）
        // 且第一候选为中文，则强制将该英文词提升至至多第 2 候选位（下标 1）
        if config.golden_second_slot_english && (3..=10).contains(&n) && candidates.len() >= 2 {
            let lower_key = lattice.input.to_ascii_lowercase();
            let eng_ids = lexicon.english_trie().exact_lookup(&lower_key);
            if let Some(top_eng_id) = eng_ids.first() {
                let eng_word = lexicon.entry(top_eng_id).text();
                if candidates[0].text != eng_word {
                    if let Some(pos) = candidates.iter().position(|c| c.text == eng_word) {
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
            for jrec in user_dict
                .top_jianpin_records(&lattice.input)
                .into_iter()
                .chain(user_dict.top_prefix_records(&lattice.input))
            {
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
                        let split_syls =
                            split_raw_pinyin_to_char_spans(&lattice.input, 0, n, &urec.text);
                        let fallback_syls = split_syls.unwrap_or_else(|| {
                            candidates
                                .first()
                                .map(|c| c.syllable_spans.clone())
                                .unwrap_or_else(|| {
                                    vec![SegmentSpan {
                                        start: 0,
                                        end: n,
                                        text: urec.text.clone(),
                                        code_key: lattice.input.clone(),
                                    }]
                                })
                        });
                        let mem_hint = crate::corrector::lookup_hint_flat(&lattice.input, &urec.text)
                            .map(str::to_string);
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
                                pronunciation_hint: mem_hint,
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

/// 判断前词是否为现代汉语「指代/数词/动词 + 量词」短语（如「这个」「那个」「一个」「每个」「哪个」「几个」「这些」「这种」「换个」「加个」）
#[inline]
pub fn is_classifier_phrase(w: &str) -> bool {
    if matches!(
        w,
        "这个" | "那个" | "一个" | "每个" | "哪个" | "几个" | "两个" | "三个" | "四个" | "五个"
            | "半个" | "整个" | "下个" | "上个" | "前个" | "后个" | "同个" | "单个" | "首个"
            | "另一个" | "多半个" | "这些" | "那些" | "哪些" | "一些" | "某些" | "好些"
            | "这种" | "那种" | "哪种" | "一种" | "某种" | "各种" | "这类" | "那类" | "哪类"
            | "一类" | "这件" | "那件" | "一件" | "这条" | "那条" | "一条" | "这段" | "那段"
            | "一段" | "这句" | "那句" | "一句" | "这篇" | "那篇" | "一篇" | "这页" | "那页"
            | "一页" | "这行" | "那行" | "一行" | "这项" | "那项" | "一项" | "这次" | "那次"
            | "一次" | "这场" | "那场" | "一场" | "这套" | "那套" | "一套" | "这批" | "那批"
            | "一批" | "这群" | "那群" | "一群" | "这位" | "那位" | "一位" | "这家" | "那家"
            | "一家" | "这版" | "那版" | "一版" | "这张" | "那张" | "一张" | "这本" | "那本"
            | "一本" | "这块" | "那块" | "一块" | "这台" | "那台" | "一台" | "这部" | "那部"
            | "一部" | "这首" | "那首" | "一首" | "这座" | "那座" | "一座" | "这层" | "那层"
            | "一层" | "这间" | "那间" | "一间"
    ) {
        return true;
    }
    w.chars().count() == 2
        && w.ends_with('个')
        && !matches!(w, "各个" | "挨个" | "逐个" | "真个" | "高个" | "大个" | "小个" | "矮个")
}

/// 判断单字是否为不能跟在「这个/一个/那个」等量词短语后面的文言代词、语气词、连词、介词、粘着语素或纯动量词/度量词（如 `*这个此`、`*这个次`、`*这个子`、`*一个之`、`*那个着`）
#[inline]
pub fn is_ungrammatical_after_classifier(ch: &str) -> bool {
    matches!(
        ch,
        "此" | "之" | "乎" | "者" | "也" | "矣" | "焉" | "哉" | "着" | "了" | "过" | "吗"
            | "呢" | "吧" | "啊" | "呀" | "和" | "与" | "或" | "及" | "而" | "且" | "但"
            | "却" | "虽" | "若" | "如" | "因" | "以" | "于" | "由" | "从" | "自" | "向"
            | "往" | "在" | "再" | "又" | "都" | "就" | "才" | "还" | "更" | "最" | "很"
            | "太" | "不" | "没" | "未" | "非" | "勿" | "莫" | "是" | "有" | "被" | "把"
            | "给" | "让" | "我" | "你" | "他" | "她" | "它" | "咱" | "谁" | "这" | "那"
            | "哪" | "次" | "回" | "趟" | "遍" | "番" | "顿" | "阵" | "元" | "斤" | "吨"
            | "米" | "尺" | "寸" | "亩" | "升" | "岁" | "倍" | "成" | "只" | "匹" | "辆"
            | "艘" | "栋" | "扇" | "双" | "子" | "办" | "半" | "越" | "约" | "动" | "懂"
            | "曾" | "据" | "断" | "接" | "提" | "体"
    )
}

/// 判断单字是否为现代汉语常见可受量词修饰的单音节可数名词（如「这个词」「这个人」「这件事」「加个群」「换个字」）
#[inline]
pub fn is_countable_single_noun(ch: &str) -> bool {
    matches!(
        ch,
        "词" | "字" | "人" | "事" | "点" | "月" | "年" | "周" | "天" | "夜" | "国" | "家"
            | "店" | "厂" | "校" | "院" | "馆" | "站" | "局" | "处" | "科" | "室" | "部"
            | "群" | "班" | "队" | "组" | "团" | "会" | "社" | "党" | "派" | "省" | "市"
            | "县" | "区" | "镇" | "乡" | "村" | "州" | "岛" | "山" | "河" | "湖" | "海"
            | "版" | "包" | "表" | "库" | "键" | "页" | "行" | "列" | "项" | "段" | "句"
            | "章" | "节" | "篇" | "册" | "卷" | "本" | "书" | "题" | "课" | "图" | "画"
            | "歌" | "曲" | "片" | "剧" | "戏" | "号" | "码" | "名" | "姓" | "数" | "值"
            | "率" | "度" | "量" | "步" | "招" | "法" | "方" | "式" | "面" | "边" | "角"
            | "端" | "侧" | "头" | "尾" | "底" | "顶" | "心" | "口" | "眼" | "手" | "脚"
            | "腿" | "脸" | "嘴" | "耳" | "鼻" | "位" | "座" | "间" | "层" | "楼" | "门"
            | "窗" | "墙" | "房" | "屋" | "洞" | "坑" | "沟" | "槽" | "孔" | "盒" | "箱"
            | "袋" | "瓶" | "杯" | "盘" | "碗" | "锅" | "盆" | "桶" | "罐" | "球" | "车"
            | "船" | "机" | "灯" | "钟" | "镜" | "床" | "桌" | "椅" | "柜" | "架" | "菜"
            | "饭" | "汤" | "果" | "瓜" | "蛋" | "肉" | "鱼" | "鸟" | "猫" | "狗" | "马"
            | "牛" | "羊" | "猪" | "鸡" | "虫" | "花" | "草" | "树" | "木" | "叶" | "根"
            | "星" | "云" | "风" | "雨" | "雷" | "电" | "火" | "水" | "土" | "石" | "金"
            | "银" | "铜" | "铁" | "钢" | "网" | "线" | "绳" | "带" | "链" | "环" | "圈"
            | "弯" | "折" | "扣" | "结" | "印" | "痕" | "疤" | "病" | "症" | "癌" | "瘤"
            | "药" | "针" | "刀" | "枪" | "剑" | "炮" | "弹" | "箭" | "盾" | "牌" | "旗"
            | "徽" | "票" | "券" | "卡" | "证" | "照" | "信" | "件" | "帖" | "榜" | "单"
            | "档" | "案" | "例" | "谜" | "梦" | "愿" | "福" | "祸" | "灾" | "难" | "劫"
            | "关" | "坎" | "鬼" | "神" | "仙" | "妖" | "魔" | "怪" | "佛" | "王" | "帝"
            | "皇" | "官" | "将" | "兵" | "贼" | "敌" | "友" | "客" | "主" | "徒" | "师"
            | "匠" | "医" | "生" | "员"
    )
}

/// 计算相邻两词块（`prev_word` -> `edge`）的构词法与量词-名词搭配奖励，供整句 Beam Search 与逐段分步选词共同复用
pub(crate) fn word_pair_collocation_bonus(prev_word: &str, edge: &LatticeEdge) -> f32 {
    if prev_word.is_empty() {
        return 0.0;
    }
    let w = edge.text.as_str();
    let mut bonus = 0.0f32;

    // 1. 现代汉语高产单字前缀（反/超/非/无/抗/微/半/零/总/副/准/亚/伪/全/跨/免/防/耐）+ 双字词根构词（如「反+重力」「超+导体」「微+服务」）
    if prev_word == "反" {
        if matches!(
            w,
            "重力" | "引力" | "物质" | "粒子" | "作用" | "方向" | "比例" | "函数" | "矩阵"
                | "对称" | "垄断" | "腐败" | "欺诈" | "洗钱" | "间谍" | "倾销" | "补贴"
                | "制裁" | "人类" | "社会" | "常规" | "直觉" | "常识" | "传统" | "主流"
                | "教材" | "兴奋剂" | "恐"
        ) {
            bonus += 3.95;
        } else if edge.kind == EdgeKind::ChineseWord && edge.syllable_count == 2 && !edge.is_fuzzy {
            bonus += 1.65;
        }
    } else if matches!(prev_word, "超" | "微" | "零" | "无" | "失" | "抗" | "低" | "高")
        && matches!(
            w,
            "重力" | "引力" | "导体" | "流体" | "临界" | "自然" | "负荷" | "声速" | "音速"
                | "光速" | "清晰" | "广角" | "算力" | "延迟" | "并发" | "可用" | "性能"
                | "精度" | "强度" | "密度" | "维度" | "频段" | "功耗"
        )
    {
        bonus += 3.65;
    } else if matches!(
        prev_word,
        "超" | "非" | "无" | "抗" | "微" | "半" | "零" | "总" | "副" | "准" | "亚" | "伪"
            | "全" | "跨" | "免" | "防" | "耐"
    ) && edge.kind == EdgeKind::ChineseWord
        && edge.syllable_count == 2
        && !edge.is_fuzzy
    {
        bonus += 1.45;
    } else if matches!(prev_word, "纯" | "巨" | "贼" | "特" | "真" | "挺")
        && edge.kind == EdgeKind::ChineseWord
        && edge.syllable_count >= 2
        && !edge.is_fuzzy
    {
        bonus += 2.45;
    } else if matches!(
        prev_word,
        "吃" | "喝" | "买" | "卖" | "办" | "搞" | "开" | "拿" | "收" | "交" | "走" | "穿"
    ) && edge.kind == EdgeKind::ChineseWord
        && edge.syllable_count >= 2
        && !edge.is_fuzzy
    {
        bonus += 1.55;
    }

    // 1b. 现代汉语高产名词性类后缀（[多字实词] + 区/饭/费/群/店/馆/站/卡/券/感/圈/风/版/号/户/娃）构词补偿，
    //     消除单字后缀因未预组进词库而相对双字动词（如「夫人+去」）额外承受的步长与字频差
    if !edge.is_fuzzy && edge.syllable_count == 1 && prev_word.chars().count() >= 2 && !prev_word.is_ascii() {
        if w == "区" && !matches!(prev_word, "夫人" | "妇人" | "本人" | "别人" | "有人" | "没人" | "个人") {
            bonus += if prev_word.ends_with('人') { 3.65 } else { 2.35 };
        } else if matches!(
            w,
            "饭" | "费" | "群" | "店" | "馆" | "站" | "卡" | "券" | "感" | "圈" | "风" | "版" | "户" | "娃"
        ) && !prev_word.ends_with('的')
        {
            bonus += 2.35;
        }
    }
    if !edge.is_fuzzy && edge.syllable_count == 1 {
        if w == "户" && matches!(prev_word, "公" | "私" | "专" | "开" | "销" | "落" | "过" | "大" | "散") {
            bonus += 3.25;
        } else if w == "娃"
            && matches!(
                prev_word,
                "接" | "送" | "带" | "遛" | "溜" | "晒" | "哄" | "生" | "养" | "抱" | "陪" | "看" | "鸡" | "萌"
            )
        {
            bonus += 3.25;
        } else if w == "行" && matches!(prev_word, "私" | "公" | "支" | "分" | "总" | "同" | "外" | "内") {
            bonus += 2.65;
        }
    }
    if matches!(prev_word, "右上" | "右下" | "左上" | "左下") {
        if matches!(w, "角" | "方" | "边" | "侧" | "部") {
            bonus += 3.45;
        } else if w == "叫" {
            bonus -= 2.25;
        }
    }
    if matches!(
        w,
        "右下角" | "左下角" | "右上角" | "左上角" | "右下方" | "左下方" | "右上方" | "左上方"
    ) && matches!(
        prev_word,
        "让" | "把" | "在" | "放" | "放在" | "点" | "点击" | "看" | "拖到" | "移到" | "从"
            | "往" | "向" | "屏幕" | "桌面" | "窗口" | "界面" | "图片" | "页面"
    ) {
        bonus += 2.45;
    }

    // 2. 指代/数量/动词+量词短语（如「这个」「那个」「一个」「每个」「哪个」「几个」「这些」「这种」「换个」「加个」）+ 可数名词 vs 文言代词/虚词
    if is_classifier_phrase(prev_word) {
        if is_ungrammatical_after_classifier(w) {
            bonus -= 2.65;
        } else if is_countable_single_noun(w) {
            bonus += 3.45;
        } else if edge.kind == EdgeKind::ChineseWord && edge.syllable_count >= 2 && !edge.is_fuzzy {
            bonus += 0.65;
        }
    } else if matches!(
        prev_word,
        "这一" | "那一" | "哪一" | "第一" | "第二" | "第三" | "第四" | "第五" | "上一"
            | "下一" | "前一" | "后一" | "另一" | "每一" | "某一" | "同一" | "唯一" | "这"
            | "那" | "哪" | "一" | "两" | "三" | "四" | "五" | "六" | "七" | "八" | "九" | "十"
            | "几" | "每" | "某" | "各" | "首" | "末"
    ) {
        let is_two_char_det = prev_word.chars().count() == 2;
        if matches!(
            w,
            "词" | "字" | "页" | "行" | "列" | "项" | "段" | "句" | "章" | "节" | "篇" | "条"
                | "件" | "次" | "回" | "场" | "遍" | "趟" | "步" | "招" | "层" | "楼" | "门"
                | "间" | "位" | "名" | "家" | "人" | "天" | "月" | "年" | "周" | "岁" | "代"
                | "点" | "分" | "秒" | "倍" | "元" | "块" | "斤" | "米" | "度" | "级" | "等"
                | "类" | "种" | "样" | "款" | "版" | "期" | "届" | "批" | "群" | "组" | "队"
                | "班" | "套" | "份" | "本" | "部" | "台" | "辆" | "座" | "张" | "颗" | "关"
                | "轮" | "波" | "站" | "课" | "题" | "幕" | "集"
        ) {
            bonus += if is_two_char_det { 3.35 } else { 2.25 };
        } else if matches!(
            w,
            "此" | "之" | "乎" | "者" | "也" | "矣" | "焉" | "哉" | "不" | "业" | "曾" | "断"
                | "据" | "接" | "提"
        ) {
            bonus -= 2.45;
        }
    }

    // 3. 现代汉语固定口语/疑问习语「怎么回事/咋回事/一回事/两回事/当回事/这么回事/那么回事」vs 未完系词「会是」
    if matches!(w, "回事" | "一回事") {
        if matches!(
            prev_word,
            "怎么" | "咋" | "什么" | "哪" | "这" | "那" | "这么" | "那么" | "一" | "两" | "三"
                | "几" | "同一" | "另一" | "每" | "当" | "不当" | "没当" | "当成" | "当作"
                | "有" | "没" | "真有" | "确有" | "是" | "不是" | "算"
        ) {
            bonus += 3.85;
        }
    } else if w == "会是" {
        if matches!(
            prev_word,
            "怎么" | "咋" | "这么" | "那么" | "一" | "两" | "三" | "同一" | "另一" | "当"
                | "不当" | "没当" | "当成" | "当作"
        ) {
            bonus -= 1.95;
        }
    } else if matches!(w, "怎么回事" | "是怎么回事") {
        if matches!(
            prev_word,
            "到底" | "究竟" | "是" | "这是" | "那是" | "这" | "那" | "知道" | "不知道"
                | "看看" | "问问" | "说说" | "讲讲" | "搞清楚" | "弄清楚"
        ) {
            bonus += 2.45;
        }
    }
    if matches!(
        w,
        "他" | "她" | "它" | "你" | "我" | "谁" | "这样" | "那样" | "真的" | "假的"
    ) {
        if matches!(prev_word, "怎么会是" | "会是") {
            bonus += 2.65;
        } else if matches!(prev_word, "怎么回事" | "回事" | "一回事" | "两回事") {
            bonus -= 1.85;
        }
    }

    // 4. 常见跨词搭配下保留元音切分双字词的自然召回（如「查询/可用/账户+余额」「忍受/感到/非常/很+饥饿」）
    if w == "余额"
        && matches!(
            prev_word,
            "查询" | "可用" | "账户" | "卡内" | "卡里" | "钱包" | "剩余" | "当前" | "查看" | "显示" | "不足" | "充值"
        )
    {
        bonus += 2.25;
    } else if w == "饥饿"
        && (prev_word.ends_with('很')
            || matches!(
                prev_word,
                "忍受" | "感到" | "陷入" | "消除" | "战胜" | "抗击" | "处于" | "非常" | "十分" | "极度" | "太"
            ))
    {
        bonus += 3.15;
    } else if matches!(w, "饿" | "饿了") && prev_word == "肚子" {
        bonus += 2.25;
    }

    // 5. 现代高频搭配与新媒体常用词同音消歧（受众人群 / 女频短剧 / 人设 / 用户痛点 / 明星塌房 / 吃馄饨）
    if prev_word == "受众"
        && matches!(
            w,
            "人群" | "群体" | "范围" | "画像" | "定位" | "广" | "面" | "层" | "分析" | "规模" | "基础"
        )
    {
        bonus += 4.8;
    } else if w == "受众"
        && matches!(
            prev_word,
            "目标" | "核心" | "年轻" | "广泛" | "主要" | "精准" | "潜在" | "特定" | "主流" | "女性" | "男性"
        )
    {
        bonus += 4.4;
    } else if prev_word == "手中" && matches!(w, "人群" | "群体" | "范围" | "画像") {
        bonus -= 1.8;
    }

    if matches!(prev_word, "女频" | "男频")
        && matches!(
            w,
            "短剧" | "爽文" | "小说" | "网文" | "甜宠" | "虐文" | "赛道" | "作品" | "剧本"
        )
    {
        bonus += 4.2;
    } else if w == "短剧"
        && matches!(
            prev_word,
            "微" | "竖屏" | "爆款" | "热门" | "精品" | "出海" | "古装" | "现代" | "都市" | "甜宠"
                | "逆袭" | "战神" | "霸总" | "重生" | "穿越" | "拍" | "看" | "追" | "刷" | "演"
                | "部" | "这部"
        )
    {
        bonus += 3.8;
    } else if prev_word == "短剧"
        && matches!(
            w,
            "市场" | "平台" | "赛道" | "出海" | "演员" | "剧本" | "制作" | "投流" | "行业" | "热播"
                | "爆火" | "女主" | "男主"
        )
    {
        bonus += 3.8;
    } else if w == "短句" && matches!(prev_word, "女频" | "男频" | "甜宠" | "霸总" | "竖屏" | "爆款")
    {
        bonus -= 1.5;
    }

    if w == "人设"
        && matches!(
            prev_word,
            "立" | "立好" | "凹" | "打造" | "维持" | "保持" | "符合" | "贴合" | "反差" | "高冷"
                | "完美" | "霸总" | "女主" | "男主" | "角色"
        )
    {
        bonus += 3.8;
    } else if prev_word == "人设"
        && matches!(w, "崩塌" | "翻车" | "崩了" | "不倒" | "讨喜" | "鲜明" | "立住")
    {
        bonus += 3.8;
    }

    if w == "痛点"
        && matches!(
            prev_word,
            "用户" | "行业" | "核心" | "解决" | "直击" | "抓住" | "市场" | "消费" | "业务" | "产品"
        )
    {
        bonus += 3.8;
    } else if prev_word == "甜宠"
        && matches!(w, "剧" | "短剧" | "文" | "小说" | "风格" | "路线" | "日常")
    {
        bonus += 3.8;
    } else if w == "塌房" && matches!(prev_word, "明星" | "偶像" | "爱豆" | "网红" | "顶流" | "艺人")
    {
        bonus += 4.0;
    } else if w == "出圈"
        && matches!(
            prev_word,
            "成功" | "火爆" | "火速" | "文化" | "强势" | "破圈" | "再次" | "彻底"
        )
    {
        bonus += 3.8;
    } else if w == "馄饨"
        && (prev_word.ends_with('吃')
            || prev_word.ends_with('碗')
            || matches!(
                prev_word,
                "煮" | "包" | "一碗" | "小" | "鲜肉" | "虾仁" | "荠菜" | "菜肉" | "买" | "点"
            ))
    {
        bonus += 4.2;
    } else if (prev_word == "是真" && w == "是假") || (prev_word == "半真" && w == "半假") {
        bonus += 3.5;
    }

    bonus
}

/// 现代汉语语法结构与虚词同音搭配奖励（在 Beam Search 转移时实时消歧「再/在」「吧/把」「的/地/得」「了/乐」）
fn grammar_transition_bonus(
    state: &BeamState,
    prev_word: &str,
    edge: &LatticeEdge,
    n: usize,
) -> f32 {
    let mut bonus = word_pair_collocation_bonus(prev_word, edge);
    let w = edge.text.as_str();
    let is_end = edge.end == n && edge.start > 0;

    // 1. 句末语气助词（吧/吗/呢/啊/呀/啦/呗/嘛） vs 句末未完介词结构（把/别把/先把/再把）
    if matches!(w, "吧" | "吗" | "呢" | "啊" | "呀" | "啦" | "呗" | "嘛") {
        if is_end {
            if prev_word.chars().count() >= 2 {
                bonus += 1.45;
            } else if !prev_word.is_empty() {
                bonus += 0.95;
            }
        } else if edge.end < n {
            bonus -= 0.55;
        }
    }
    if is_end {
        if w == "把"
            && !matches!(
                prev_word,
                "一" | "两" | "三" | "四" | "五" | "几" | "这" | "那" | "每" | "大" | "小" | "火" | "刀" | "手"
            )
        {
            bonus -= 1.65;
        } else if matches!(
            w,
            "别把" | "先把" | "再把" | "就把" | "要把" | "快把" | "请把" | "没把" | "都把" | "还把" | "又把" | "才把" | "却把" | "总把"
        ) {
            bonus -= 2.35;
        }
        if w == "了" && !prev_word.is_empty() {
            bonus += 0.55;
        }
        if prev_word == "怎么会" && w == "是" {
            bonus -= 1.65;
        }
    }
    if edge.end == n {
        if matches!(w, "怎么回事" | "是怎么回事" | "这是怎么回事" | "回事") {
            bonus += 0.85;
        } else if w == "怎么会是" {
            bonus -= 1.25;
        }
    }
    if prev_word == "是" && w == "别把" {
        bonus -= 1.20;
    }

    // 2. 「再」vs「在」语法功能与句式呼应（如「现在再研究一下」「你再反思排查一下」vs「现在在哪里」「现在在开会」）
    let is_lexical_zai_word = matches!(
        w,
        "在家" | "在场" | "在线" | "在内" | "在世" | "在职" | "在座" | "在逃" | "在即"
            | "在望" | "在乎" | "在意" | "在行" | "在理" | "在野" | "在朝" | "在案" | "在册"
            | "在编" | "在籍" | "在押" | "在位" | "在于" | "在先" | "在后"
    );
    let is_zai_verb_phrase =
        w.starts_with('在') && w.chars().count() >= 2 && !is_lexical_zai_word && w != "你在" && w != "我在";

    if w == "再" {
        if matches!(
            prev_word,
            "现在" | "先" | "稍后" | "以后" | "回头" | "明天" | "下次" | "改天" | "等会"
                | "一会儿" | "之后" | "后来" | "然后" | "接着" | "不妨" | "不如" | "还要"
                | "还得" | "需要" | "可以" | "应该" | "决定" | "准备" | "重新" | "继续"
        ) || state
            .edges
            .iter()
            .any(|e| matches!(e.text.as_str(), "先" | "首先" | "稍后" | "回头" | "等会"))
        {
            bonus += 1.45;
        } else if matches!(prev_word, "你" | "我" | "他" | "她" | "我们" | "你们" | "咱们" | "大家")
            && edge.end < n
        {
            bonus += 0.45;
        }
    } else if w == "在" || is_zai_verb_phrase {
        if prev_word.ends_with('在') {
            // 抑制「现在 + 在」「现在 + 在研究」这类叠字进行体粘连
            bonus -= if is_zai_verb_phrase { 2.65 } else { 0.95 };
        }
    }

    if prev_word == "在" || prev_word == "你在" || prev_word == "我在" {
        if matches!(
            w,
            "哪" | "哪里" | "哪儿" | "这" | "这里" | "这儿" | "那" | "那里" | "那儿" | "家"
                | "家里" | "公司" | "学校" | "医院" | "路上" | "车上" | "网上" | "线上"
                | "现场" | "外地" | "国外" | "国内" | "外面" | "里面" | "上面" | "下面"
                | "旁边" | "一起" | "一块" | "干嘛" | "干什么" | "做什么" | "忙什么" | "上班"
                | "上课" | "开会" | "吃饭" | "睡觉" | "休息" | "忙" | "看书" | "学习" | "工作"
                | "如图"
        ) || (w.chars().count() >= 2
            && w.ends_with(['里', '上', '下', '内', '外', '中', '旁', '边', '处']))
        {
            bonus += 1.45;
        }
    } else if prev_word == "再" || prev_word == "你再" || prev_word == "我再" {
        let is_action_verb = matches!(
            w,
            "研究" | "优化" | "排查" | "检查" | "测试" | "修改" | "确认" | "考虑" | "讨论"
                | "分析" | "处理" | "解决" | "尝试" | "核对" | "完善" | "调整" | "沟通"
                | "联系" | "发" | "看" | "试" | "想" | "说" | "问" | "找" | "改" | "加"
                | "买" | "做" | "来" | "去"
        );
        if is_action_verb || (edge.kind == EdgeKind::ChineseWord && (state.edges.len() >= 2 || prev_word != "再")) {
            bonus += 1.35;
        }
    }

    // 尝试/反复动量补语（一下/一遍/一次/一会儿/一点/一番/一趟/看看/试试/说说/想想）在现代汉语中与「再」强呼应，与进行体「在」语法互斥
    if matches!(
        w,
        "一下" | "一遍" | "一次" | "一会儿" | "一点" | "一番" | "一趟" | "看看" | "试试" | "说说" | "想想"
    ) {
        let has_zai_again = state
            .edges
            .iter()
            .rev()
            .take(4)
            .any(|e| matches!(e.text.as_str(), "再" | "你再" | "我再" | "先再") || e.text.starts_with("再"));
        let has_zai_prog = state.edges.iter().rev().take(4).any(|e| {
            let t = e.text.as_str();
            matches!(t, "在" | "你在" | "我在")
                || (t.starts_with('在')
                    && t.chars().count() >= 2
                    && !matches!(
                        t,
                        "在家" | "在场" | "在线" | "在内" | "在世" | "在职" | "在座" | "在乎" | "在意" | "在于"
                    ))
        });
        if has_zai_again {
            bonus += 1.85;
        } else if has_zai_prog {
            bonus -= 2.45;
        }
    }

    // 3. 结构助词/时态助词（的/了/着/过/地/得）紧接多字实词或多词短语后的黏着补偿：
    // 词库中部分形容词自带预组三字词（如「狭窄的」作为单条词只计 1 次步长），而大多数动词/名词短语（如「下载」「吃+公家+饭」）
    // 需多步组合（额外承担单字 cost 与 1.75 步长惩罚）。对此给予语法黏着补偿，使其与预组多字词处于同一量级。
    if prev_word.chars().count() >= 2 || state.edges.len() >= 2 {
        if w == "的" {
            bonus += 2.25;
        } else if matches!(w, "了" | "着" | "过" | "地" | "得") {
            bonus += 1.35;
        }
    }

    // 3b. 领属/定语助词「的 / *的」后接单音节实体名词（如「百姓的+钱」「公司的+车」）与句末方位词抑制（杜绝 `*百姓的前`）
    let after_possessive_de = prev_word == "的"
        || (prev_word.chars().count() >= 2
            && prev_word.ends_with('的')
            && !matches!(
                prev_word,
                "目的" | "标的" | "有的" | "真的" | "假的" | "是的" | "好的" | "对的"
            ));
    if after_possessive_de {
        if !edge.is_fuzzy
            && edge.syllable_count == 1
            && (matches!(
                w,
                "钱" | "水" | "肉" | "酒" | "茶" | "血" | "汗" | "力" | "气" | "命" | "权" | "利" | "债" | "账"
            ) || (is_countable_single_noun(w) && !matches!(w, "前" | "后" | "内" | "外" | "上" | "下" | "中" | "里")))
        {
            bonus += 3.25;
        } else if w == "前" && is_end {
            bonus -= 2.65;
        }
    }

    // 3c. 常见动体结构「出了/花了/交了/捐了 + 钱/事/力」及能愿助动词「才会/才能/才敢 + 单字动词」句法补偿
    if matches!(prev_word, "毫升" | "厘米" | "毫米" | "分米" | "公斤" | "千克")
        && matches!(w, "出了" | "出钱" | "花了" | "交了" | "捐了")
    {
        bonus -= 2.55;
    } else if prev_word == "豪绅" && matches!(w, "出了" | "出钱" | "的钱" | "的" | "捐钱" | "先出") {
        bonus += 2.65;
    }
    if matches!(
        prev_word,
        "出了" | "花了" | "交了" | "付了" | "捐了" | "退了" | "借了" | "欠了" | "赚了" | "挣了" | "亏了" | "拿了" | "收了" | "给了" | "赔了"
    ) && matches!(
        w,
        "钱" | "事" | "力" | "门" | "国" | "错" | "血" | "汗" | "名" | "声" | "问题" | "故障" | "事故"
    ) {
        bonus += 3.95;
    } else if prev_word == "除了" && matches!(w, "千百" | "百千") {
        bonus -= 1.85;
    }
    if matches!(prev_word, "钱" | "的钱" | "出钱" | "花钱" | "交钱" | "捐钱")
        && matches!(
            w,
            "百姓" | "大家" | "人们" | "别人" | "客户" | "用户" | "群众" | "三七" | "五五" | "四六" | "二八" | "如数"
        )
    {
        bonus += 2.65;
    }
    if matches!(
        prev_word,
        "才会" | "才能" | "才敢" | "才肯" | "就会" | "就能" | "不会" | "不能" | "可以" | "愿意" | "应该"
    ) && !edge.is_fuzzy
        && edge.syllable_count == 1
        && matches!(
            w,
            "出" | "来" | "去" | "买" | "卖" | "给" | "跟" | "做" | "干" | "信" | "投" | "交" | "付" | "退" | "走" | "吃" | "看" | "说" | "用"
        )
    {
        bonus += 2.85;
    }

    // 3d. 中文词组后接常见英文技术缩写（如「趁现在 AI」「调用 API」「设计 UI」）加分，防止被冷门单字如「趁现在艾」干扰
    if prev_word.chars().count() >= 2
        && !prev_word.is_ascii()
        && edge.kind == EdgeKind::EnglishWord
        && matches!(
            w,
            "AI" | "API" | "UI" | "App" | "VIP" | "PPT" | "PDF" | "GPU" | "CPU" | "SDK" | "LLM" | "GPT" | "iOS" | "Mac" | "Web"
        )
    {
        bonus += 1.85;
    }

    // 4. 「得」后接程度/结果补语搭配
    if prev_word == "得"
        && matches!(
            w,
            "很" | "好" | "快" | "慢" | "多" | "少" | "远" | "近" | "早" | "晚" | "高" | "低"
                | "清楚" | "明白" | "出来" | "下去" | "不了" | "不起" | "不得" | "完" | "到" | "住"
        )
    {
        bonus += 1.15;
    }

    bonus
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

/// 将无 `'` 分隔符的用户词条原始拼音区间 `[start..end]` 按汉字个数拆分为逐字对应的 `SegmentSpan`
fn split_raw_pinyin_to_char_spans(
    input: &str,
    start: usize,
    end: usize,
    text: &str,
) -> Option<Vec<SegmentSpan>> {
    let chars: Vec<char> = text.chars().collect();
    let k = chars.len();
    if k < 2 || text.is_ascii() || start >= end || end > input.len() {
        return None;
    }
    let raw = &input[start..end];
    let mut cuts = Vec::with_capacity(k);
    if raw.len() == k {
        cuts.resize(k, 1);
    } else if !find_k_syllable_cuts(raw, k, false, &mut cuts) {
        cuts.clear();
        if !find_k_syllable_cuts(raw, k, true, &mut cuts) {
            return None;
        }
    }
    if cuts.len() != k {
        return None;
    }
    let mut out = Vec::with_capacity(k);
    let mut cur = start;
    for (idx, step) in cuts.into_iter().enumerate() {
        let next_end = (cur + step).min(end);
        if next_end > cur {
            let code_piece = input[cur..next_end].trim_matches('\'').to_string();
            out.push(SegmentSpan {
                start: cur,
                end: next_end,
                text: chars[idx].to_string(),
                code_key: if code_piece.is_empty() {
                    input[cur..next_end].to_string()
                } else {
                    code_piece
                },
            });
            cur = next_end;
        }
    }
    (out.len() == k).then_some(out)
}

fn find_k_syllable_cuts(
    rem: &str,
    k: usize,
    allow_fuzzy_or_abbrev: bool,
    out_cuts: &mut Vec<usize>,
) -> bool {
    if k == 0 {
        return rem.is_empty();
    }
    if rem.len() < k || rem.len() > k * 7 {
        return false;
    }
    let lead_apos = rem.bytes().take_while(|&b| b == b'\'').count();
    let clean = &rem[lead_apos..];
    if clean.len() < k {
        return false;
    }
    if k == 1 {
        let ok = if !allow_fuzzy_or_abbrev {
            crate::pinyin::is_valid_or_alias_syllable(clean)
        } else {
            clean.len() <= 6
        };
        if ok {
            out_cuts.push(rem.len());
            return true;
        }
        return false;
    }
    let max_len = (clean.len() - (k - 1)).min(6);
    // 第一轮优先匹配完整合法全拼音节（长音节优先）
    for len in (2..=max_len).rev() {
        let piece = &clean[..len];
        if crate::pinyin::is_valid_or_alias_syllable(piece) {
            out_cuts.push(lead_apos + len);
            if find_k_syllable_cuts(&clean[len..], k - 1, allow_fuzzy_or_abbrev, out_cuts) {
                return true;
            }
            out_cuts.pop();
        }
    }
    // 单字母合法韵母音节（a / e / o）
    if matches!(clean.as_bytes()[0], b'a' | b'e' | b'o') {
        out_cuts.push(lead_apos + 1);
        if find_k_syllable_cuts(&clean[1..], k - 1, allow_fuzzy_or_abbrev, out_cuts) {
            return true;
        }
        out_cuts.pop();
    }
    // 宽松模式：允许模糊音或单字母声母简拼
    if allow_fuzzy_or_abbrev {
        let fuzzy = crate::pinyin::FuzzyPinyinConfig::default();
        for len in (2..=max_len).rev() {
            let piece = &clean[..len];
            if !crate::pinyin::is_valid_syllable(piece) && !fuzzy.expand_fuzzy(piece).is_empty() {
                out_cuts.push(lead_apos + len);
                if find_k_syllable_cuts(&clean[len..], k - 1, true, out_cuts) {
                    return true;
                }
                out_cuts.pop();
            }
        }
        out_cuts.push(lead_apos + 1);
        if find_k_syllable_cuts(&clean[1..], k - 1, true, out_cuts) {
            return true;
        }
        out_cuts.pop();
    }
    false
}

/// 使用全局最优回溯对齐将词条规范音节序列 `syls` 映射到原始输入片段 `input[start..end]`。
///
/// 之所以不能做单步贪心 `starts_with`：对前后鼻音模糊音（如原输入 `jiangquan` 模糊匹配词库 `jian'quan`「健全」、
/// `zhengshi` 匹配 `zhen'shi`「真实」），规范音节 `jian` 恰好是原始音节 `jiang` 的真前缀；
/// 贪心 `starts_with("jian")` 会把 `g` 推给下一个音节形成非法片段 `jian | gquan`！
/// 全局最小代价对齐会综合考虑所有音节：`["jiang", "quan"]`（总代价 1）远优于 `["jian", "gquan"]`（总代价 100）。
fn align_syllables_global(
    input: &str,
    start: usize,
    end: usize,
    syls: &[&str],
) -> Option<Vec<(usize, usize)>> {
    let all_fuzzy = crate::pinyin::FuzzyPinyinConfig {
        z_zh: true,
        c_ch: true,
        s_sh: true,
        n_l: true,
        f_h: true,
        in_ing: true,
        en_eng: true,
        an_ang: true,
        typo_correction: true,
    };

    fn piece_cost(piece_raw: &str, syl: &str, fuzzy: &crate::pinyin::FuzzyPinyinConfig) -> u32 {
        if piece_raw.is_empty() {
            return 200;
        }
        let lower = piece_raw.to_ascii_lowercase();
        if lower == syl || crate::pinyin::canonical_syllable(&lower) == syl {
            return 0;
        }
        if lower.len() >= 2 && fuzzy.expand_fuzzy(&lower).iter().any(|(s, _)| s == syl) {
            return 1;
        }
        if syl.starts_with(&lower) {
            if lower.len() > 1 && crate::pinyin::is_valid_or_alias_syllable(&lower) {
                return 3;
            }
            return 4;
        }
        if crate::pinyin::is_valid_or_alias_syllable(&lower) {
            return 8;
        }
        100
    }

    fn dfs(
        input: &str,
        cur: usize,
        end: usize,
        syls: &[&str],
        idx: usize,
        fuzzy: &crate::pinyin::FuzzyPinyinConfig,
        cur_cost: u32,
        best_cost: &mut u32,
        path: &mut Vec<(usize, usize)>,
        best_path: &mut Option<Vec<(usize, usize)>>,
    ) {
        if cur_cost >= *best_cost {
            return;
        }
        let bytes = input.as_bytes();
        let mut pos = cur;
        while pos < end && bytes[pos] == b'\'' {
            pos += 1;
        }
        if idx + 1 == syls.len() {
            let mut tail_end = end;
            while tail_end > pos && bytes[tail_end - 1] == b'\'' {
                tail_end -= 1;
            }
            if tail_end <= pos {
                return;
            }
            let c = piece_cost(&input[pos..tail_end], syls[idx], fuzzy);
            let total = cur_cost + c;
            if total < *best_cost {
                *best_cost = total;
                let mut done = path.clone();
                done.push((pos, end));
                *best_path = Some(done);
            }
            return;
        }

        let rem_syls = syls.len() - idx - 1;
        // 为后续每个音节至少保留 1 字节；单音节最长 6 字节（如 zhuang / chuang）
        let max_end = end.saturating_sub(rem_syls).min(pos + 6);
        if max_end <= pos {
            return;
        }
        // 收集当前步所有非含分隔符的候选切点并按局部代价升序枚举，实现极速剪枝
        let mut cands: [(usize, u32); 6] = [(0, 0); 6];
        let mut cand_len = 0usize;
        for next_end in (pos + 1)..=max_end {
            if bytes[next_end - 1] == b'\'' {
                break;
            }
            let c = piece_cost(&input[pos..next_end], syls[idx], fuzzy);
            cands[cand_len] = (next_end, c);
            cand_len += 1;
        }
        cands[..cand_len].sort_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)));
        for &(next_end, c) in &cands[..cand_len] {
            if cur_cost + c >= *best_cost {
                break;
            }
            path.push((pos, next_end));
            dfs(
                input,
                next_end,
                end,
                syls,
                idx + 1,
                fuzzy,
                cur_cost + c,
                best_cost,
                path,
                best_path,
            );
            path.pop();
        }
    }

    let mut best_cost = u32::MAX;
    let mut best_path = None;
    let mut path = Vec::with_capacity(syls.len());
    dfs(
        input,
        start,
        end,
        syls,
        0,
        &all_fuzzy,
        0,
        &mut best_cost,
        &mut path,
        &mut best_path,
    );
    best_path
}

/// 将单条晶格图边（如多字中文词 `0..9 "wo'shi'yong" -> "我使用"`）拆解为逐个独立音节区间
/// 这样预编辑音节栏中的每一个拼音音节都有精确对应的 `[start..end]` 字节范围与单字
fn split_edge_into_syllables(input: &str, edge: &LatticeEdge) -> Vec<SegmentSpan> {
    if !edge.code_key.contains('\'') {
        if let Some(spans) =
            split_raw_pinyin_to_char_spans(input, edge.start, edge.end, &edge.text)
        {
            return spans;
        }
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
    let has_apostrophe = input
        .get(edge.start..edge.end)
        .is_some_and(|s| s.contains('\''));

    let mut out = Vec::with_capacity(syls.len());
    let mut push_span = |idx: usize, s: usize, e: usize| {
        if e > s {
            let syl = syls.get(idx).copied().unwrap_or("");
            let actual_code = input
                .get(s..e)
                .unwrap_or(syl)
                .trim_matches('\'')
                .to_string();
            let ch_text = chars
                .get(idx)
                .map(|c| c.to_string())
                .unwrap_or_else(|| edge.text.clone());
            out.push(SegmentSpan {
                start: s,
                end: e,
                text: ch_text,
                code_key: if actual_code.is_empty() {
                    syl.to_string()
                } else {
                    actual_code
                },
            });
        }
    };

    if edge.kind == EdgeKind::AbbrevPinyin || span_len == syls.len() {
        // 首字母简拼词条（如 `wzj` -> `wei'zi'ji`）：每个音节固定对应 1 个输入字母
        let mut cur = edge.start;
        for idx in 0..syls.len() {
            while cur < edge.end && input.as_bytes().get(cur) == Some(&b'\'') {
                cur += 1;
            }
            let next_end = if idx + 1 == syls.len() {
                edge.end
            } else {
                (cur + 1).min(edge.end)
            };
            push_span(idx, cur, next_end);
            cur = next_end;
        }
    } else if !edge.is_fuzzy && !has_apostrophe && total_syl_len == span_len {
        let mut cur = edge.start;
        for (idx, &syl) in syls.iter().enumerate() {
            let next_end = if idx + 1 == syls.len() {
                edge.end
            } else {
                (cur + syl.len()).min(edge.end)
            };
            push_span(idx, cur, next_end);
            cur = next_end;
        }
    } else if let Some(aligned) = align_syllables_global(input, edge.start, edge.end, &syls) {
        for (idx, (s, e)) in aligned.into_iter().enumerate() {
            push_span(idx, s, e);
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

