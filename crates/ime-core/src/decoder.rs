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
                        let char_cnt = urec.text.chars().count();
                        // 晶格图内部仅注入 <= 4 字的词汇级用户记忆（如人名「武增吉」、专有名词「文摘集」），
                        // 5 字及以上的长短语/整句交由末尾 User Memory Top Promotion 处理，避免将多段整句坍缩成单段而丢失递进前缀候选
                        if char_cnt > 4 && !urec.text.is_ascii() {
                            continue;
                        }
                        if urec.dynamic_score >= 4.5
                            || urec.commit_count >= 2
                            || (is_whole_input && urec.code_key != raw_slice && urec.commit_count >= 1)
                        {
                            // 若晶格图中该区间已存在同名非纠错边（如常用词「怎么」「时候」「选词」已在词库中），
                            // 绝不重复塞入 `base_cost = 0.15` 的幽灵边（否则再叠加下方 `user_boost` 会产生负数代价，
                            // 导致两个被用过的 2 字词拼出负数代价而压垮 4 字成语/词组如「怎么回事」）。
                            if edges_at_i
                                .iter()
                                .any(|e| !e.is_fuzzy && e.end == end_pos && e.text == urec.text)
                            {
                                continue;
                            }
                            let is_ascii = urec.text.is_ascii();
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
                                base_cost: if is_whole_input { 3.2 } else { 4.8 },
                                domain: DomainTag::General,
                                syllable_count: if is_ascii { 1 } else { char_cnt.max(1) },
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

                // 个人动态记忆词频加成（Time-Decay User Boost）：
                // 1) 整串完全匹配的词条（`edge_is_whole`）享受完整个人记忆提权；
                // 2) 若整串本身已是词库中的精确词组（`has_exact_whole_word`，如 `zenmehuishi` -> `怎么回事`），
                //    严格限制内部局部子词块（如 `怎么`、`会是`）的历史词频加成，杜绝两个常用 2 字子词拼凑反超完整的 4 字词条；
                // 3) 任何局部子词块扣减 `user_boost` 后的基础代价均设有正数下界（`>= 1.2`），严禁步长代价变负。
                let raw_input_slice = &lattice.input[edge.start..edge.end];
                let raw_boost = user_dict.user_boost(raw_input_slice, &edge.text);
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

            let tag = if has_fuzzy {
                format!("[纠错 {}]", preedit)
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
        const MAX_BACKUP_SENTENCES: usize = 2;
        const SENTENCE_COST_MARGIN: f32 = 3.2;
        let mut alt_sentences: Vec<Candidate> = Vec::new();
        if let Some((best_cost, best_seg_cnt)) = candidates
            .first()
            .map(|c| (c.l1_cost, c.segments.len()))
        {
            let mut idx = 0usize;
            candidates.retain(|c| {
                let is_first = idx == 0;
                idx += 1;
                if is_first || c.segments.len() <= 1 {
                    return true;
                }
                if alt_sentences.len() < MAX_BACKUP_SENTENCES
                    && c.segments.len() <= best_seg_cnt + 1
                    && c.l1_cost <= best_cost + SENTENCE_COST_MARGIN
                    && !c.tag.starts_with("[纠错")
                {
                    let mut backup = c.clone();
                    backup.tag = "[整句备选]".to_string();
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
            e.base_cost - user_dict.user_boost(raw, &e.text).min(6.0) - ctx_bonus
                + if e.is_fuzzy { 1.5 } else { 0.0 }
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
                    && edge.syllable_count >= best_unpinned_syl_cnt
                    && best_syl_spans
                        .iter()
                        .any(|s| s.start < edge.end && edge.end < s.end)
                {
                    continue;
                }
            }
            let is_single_cjk = edge.text.chars().count() == 1 && !edge.text.is_ascii();
            if is_single_cjk {
                if let Some(syl_end) = first_syl_end {
                    if has_full_pinyin_at_start && edge.end != syl_end {
                        continue;
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
        let initial_full_count = candidates.iter().filter(|c| c.consumed_len >= n).count();
        let mut full_word_count = initial_full_count;
        for &edge in &word_edges {
            if edge.is_fuzzy {
                continue;
            }
            if added_words >= MAX_PREFIX_WORDS {
                break;
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
        // 2) 首音节精确匹配的单字候选（若存在延后的同音整词，则在穿插前 2 个最高频单字后立即补回同音整词）
        let mut added_chars = 0usize;
        let mut deferred_flushed = false;
        let mut deferred_beam_iter = deferred_beam_full.into_iter();
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
        // 2b) 备选整句保留原始 l1_cost 供 L2 端侧 AI / 大模型语境重排，未触发 AI 逆转时排在单字之后，绝不挤占前排候选
        for mut alt in alt_sentences {
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

    // 3. 结构助词/时态助词（的/了/着/过/地/得）紧接多字实词后的黏着补偿：
    // 词库中部分形容词自带预组三字词（如「狭窄的」作为单条词只计 1 次步长），而大多数动词/名词（如「下载」「修改」「安装」）
    // 需通过「双字词 + 的」两步组合（额外承担单字 cost 与 1.75 步长惩罚）。对此给予语法黏着补偿，使「下载+的」与预组三字词处于同一量级。
    if prev_word.chars().count() >= 2 {
        if w == "的" {
            bonus += 2.25;
        } else if matches!(w, "了" | "着" | "过" | "地" | "得") {
            bonus += 1.35;
        }
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
            crate::pinyin::is_valid_syllable(clean)
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
        if crate::pinyin::is_valid_syllable(piece) {
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
            // 若存在混拼/半拼（如 `dif` -> `di'fang`、`zme` -> `zen'me`）或模糊音/纠错导致长度不等，
            // 为后续每个音节至少预留 1 字节，依次尝试：原音节前缀 -> 模糊音/纠错音节 -> 字典音节首字母前缀 -> 任意合法音节
            let rem = &input[cur..edge.end];
            let rem_syls_after = syls.len().saturating_sub(idx + 1);
            let max_try = rem.len().saturating_sub(rem_syls_after).min(6);
            let mut matched_len = 1.min(rem.len());
            if syl.len() <= max_try && rem[..max_try].starts_with(syl) {
                matched_len = syl.len();
            } else {
                let fuzzy = crate::pinyin::FuzzyPinyinConfig::default();
                let mut found = false;
                for try_len in (2..=max_try).rev() {
                    let piece = &rem[..try_len];
                    if fuzzy.expand_fuzzy(piece).iter().any(|(s, _)| s == syl) {
                        matched_len = try_len;
                        found = true;
                        break;
                    }
                }
                if !found {
                    for try_len in (1..=syl.len().min(max_try)).rev() {
                        if syl.starts_with(&rem[..try_len])
                            && (try_len == 1 || crate::pinyin::is_valid_syllable(&rem[..try_len]))
                        {
                            matched_len = try_len;
                            found = true;
                            break;
                        }
                    }
                }
                if !found {
                    for try_len in (1..=max_try).rev() {
                        if crate::pinyin::is_valid_syllable(&rem[..try_len]) {
                            matched_len = try_len;
                            break;
                        }
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

