//! `ime-core`: 全自研「中英统一晶格图」与混合解码核心引擎

pub mod decoder;
pub mod lattice;
pub mod pinyin;
pub mod user_dict;

pub use decoder::{
    is_classifier_phrase, is_countable_single_noun, is_ungrammatical_after_classifier, AppScene,
    BilingualDecoder, Candidate, DecoderConfig, SegmentSpan,
};
pub use lattice::{BilingualLattice, EdgeKind, LatticeEdge};
pub use pinyin::{FuzzyPinyinConfig, PhonotacticDetector, ShuangpinScheme};
pub use user_dict::{FileStamp, UserDict};

use ime_lexicon::CompiledLexicon;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// 交给个人词库写盘线程的消息
enum PersistMessage {
    /// 一份待写的词库快照。积压多份时只写最新的一份
    Save(UserDict),
    /// 在它之前排队的快照都处理完之后回一声
    Sync(mpsc::Sender<()>),
}

/// 个人词库的持久化状态（见 [`ImeCoreEngine::enable_persistence`]）
struct UserDictPersistence {
    path: PathBuf,
    tx: mpsc::Sender<PersistMessage>,
    /// 本引擎上次读入或写出的那份词库文件的修改时间与长度（`None` = 当时文件不存在）。
    /// 写盘线程每写成功一次就更新；与文件现状不一致说明别的进程改过它
    stamp: Arc<Mutex<Option<FileStamp>>>,
    /// 内存里的词库有还没交给写盘线程的改动
    dirty: bool,
}

impl UserDictPersistence {
    /// 等写盘线程处理完已经排队的快照（最多等 `timeout`）。线程已经退出时立刻返回
    fn wait_for_writer(&self, timeout: Duration) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(PersistMessage::Sync(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(timeout);
        }
    }

    fn known_stamp(&self) -> Option<FileStamp> {
        *self.stamp.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_known_stamp(&self, stamp: Option<FileStamp>) {
        *self.stamp.lock().unwrap_or_else(|e| e.into_inner()) = stamp;
    }
}

/// 写盘线程的主循环：合并积压的快照只写最新的一份；失败只报告一次，直到下一次写成功
fn run_user_dict_writer(
    path: PathBuf,
    rx: mpsc::Receiver<PersistMessage>,
    stamp: Arc<Mutex<Option<FileStamp>>>,
) {
    // 以前写到一半被杀掉的进程留下的临时文件（留 10 分钟余量，不碰别的进程正在写的）
    user_dict::remove_stale_tmp_files(&path, Duration::from_secs(600));
    let mut failure_reported = false;
    while let Ok(first) = rx.recv() {
        let mut latest: Option<UserDict> = None;
        let mut waiters: Vec<mpsc::Sender<()>> = Vec::new();
        let mut next = Some(first);
        while let Some(message) = next {
            match message {
                PersistMessage::Save(snapshot) => latest = Some(snapshot),
                PersistMessage::Sync(ack) => waiters.push(ack),
            }
            next = rx.try_recv().ok();
        }
        if let Some(snapshot) = latest {
            let written = snapshot
                .to_json_bytes()
                .and_then(|json| user_dict::write_file_atomically(&path, &json));
            match written {
                Ok(written) => {
                    *stamp.lock().unwrap_or_else(|e| e.into_inner()) = Some(written);
                    failure_reported = false;
                }
                Err(error) => {
                    if !failure_reported {
                        failure_reported = true;
                        use std::io::Write;
                        // 不用 eprintln!：标准错误不可写时它会 panic，写盘线程就没了
                        let _ = writeln!(
                            std::io::stderr(),
                            "[ime-core] 个人词库写入失败（{}）：{error}。之后每次上屏仍会重试，写成功之前不再重复报告",
                            path.display()
                        );
                    }
                }
            }
        }
        for ack in waiters {
            let _ = ack.send(());
        }
    }
}

/// 单次解码的完整诊断与结果输出
#[derive(Debug, Clone)]
pub struct DecodeOutput {
    pub input: String,
    pub candidates: Vec<Candidate>,
    pub lattice_edge_count: usize,
    pub elapsed: Duration,
}

/// 核心拼音输入法引擎（集成三源融合词库、中英统一晶格图、Beam Search 与时间衰减个人词库）
pub struct ImeCoreEngine {
    pub lexicon: CompiledLexicon,
    pub user_dict: UserDict,
    pub fuzzy_config: FuzzyPinyinConfig,
    pub shuangpin: ShuangpinScheme,
    pub decoder_config: DecoderConfig,
    /// 个人词库的持久化状态（`None` 表示不持久化，单元测试与体验台默认如此）
    persistence: Option<UserDictPersistence>,
    /// 最近一次输入串对应的晶格图缓存（避免 `decode_with_pins` 与 `query_segment_alternatives` 及左右移动光标时重复构图）
    lattice_cache: std::sync::Mutex<Option<(String, u32, BilingualLattice)>>,
}

impl ImeCoreEngine {
    /// 创建引擎（个人词库仅存在于内存中，不读写磁盘）
    pub fn new(lexicon: CompiledLexicon) -> Self {
        Self {
            lexicon,
            user_dict: UserDict::new(),
            fuzzy_config: FuzzyPinyinConfig::default(),
            shuangpin: ShuangpinScheme::None,
            decoder_config: DecoderConfig::default(),
            persistence: None,
            lattice_cache: std::sync::Mutex::new(None),
        }
    }

    fn config_fingerprint(&self) -> u32 {
        let f = &self.fuzzy_config;
        let mut bits = 0u32;
        if f.z_zh { bits |= 1 << 0; }
        if f.c_ch { bits |= 1 << 1; }
        if f.s_sh { bits |= 1 << 2; }
        if f.n_l { bits |= 1 << 3; }
        if f.f_h { bits |= 1 << 4; }
        if f.in_ing { bits |= 1 << 5; }
        if f.en_eng { bits |= 1 << 6; }
        if f.an_ang { bits |= 1 << 7; }
        if f.typo_correction { bits |= 1 << 8; }
        let sp = match self.shuangpin {
            ShuangpinScheme::None => 0,
            ShuangpinScheme::Xiaohe => 1,
        };
        bits | (sp << 16)
    }

    fn get_or_build_lattice(&self, input: &str) -> BilingualLattice {
        let fp = self.config_fingerprint();
        if let Ok(guard) = self.lattice_cache.lock() {
            if let Some((cached_input, cached_fp, cached_lattice)) = guard.as_ref() {
                if cached_input == input && *cached_fp == fp {
                    return cached_lattice.clone();
                }
            }
        }
        let lattice = BilingualLattice::build(
            input,
            &self.lexicon,
            &self.fuzzy_config,
            self.shuangpin,
        );
        if let Ok(mut guard) = self.lattice_cache.lock() {
            *guard = Some((input.to_string(), fp, lattice.clone()));
        }
        lattice
    }

    /// 启用个人词库持久化：从 `path` 加载已有记忆，并启动后台写盘线程。
    ///
    /// 学习（`commit_candidate*` / `learn_*`）只在内存里改词库并记下「有改动」；调用
    /// [`Self::flush_user_dict`] 时才取一份快照交给写盘线程（`KeySession` 在每次输入结束时调用一次，
    /// 所以一次上屏至多取一份快照；引擎销毁时也会补交一次并等它写完）。写盘线程把积压的快照合并为一次写入。
    /// 键盘钩子线程本身绝不做磁盘 I/O，避免触发 Windows 低级钩子超时被系统摘除。
    /// 写入失败在标准错误上报告一次（之后每次上屏仍会重试，写成功后再失败才会再次报告）。
    pub fn enable_persistence(&mut self, path: PathBuf) {
        // 换路径时，旧路径上还没交出去的改动先交出去
        self.flush_user_dict();
        // 先取文件的修改时间与长度再读内容：两步之间文件若被换掉，下次检查会发现不一致并重读，不会漏掉
        let stamp = FileStamp::probe(&path).ok().flatten();
        let (dict, cleaned) = match stamp {
            Some(_) => UserDict::read_from_file(&path).unwrap_or_else(|_| (UserDict::new(), false)),
            None => (UserDict::new(), false),
        };
        self.user_dict = dict;
        let stamp = Arc::new(Mutex::new(stamp));
        let (tx, rx) = mpsc::channel::<PersistMessage>();
        let writer_path = path.clone();
        let writer_stamp = Arc::clone(&stamp);
        std::thread::spawn(move || run_user_dict_writer(writer_path, rx, writer_stamp));
        self.persistence = Some(UserDictPersistence {
            path,
            tx,
            stamp,
            // 读入时清掉了历史残片：把清理后的结果写回去（经写盘线程，不在调用线程上做 I/O）
            dirty: cleaned,
        });
        self.flush_user_dict();
    }

    fn mark_user_dict_dirty(&mut self) {
        if let Some(persistence) = self.persistence.as_mut() {
            persistence.dirty = true;
        }
    }

    /// 把个人词库自上次以来的改动交给写盘线程：取一份快照（克隆整个词库），由写盘线程序列化并写入文件。
    /// 没有改动或没有启用持久化时什么也不做。直接使用本引擎学习接口的调用方在一批学习结束后调用它
    /// （`KeySession` 会在每次输入结束时自动调用）
    pub fn flush_user_dict(&mut self) {
        let Some(persistence) = self.persistence.as_mut() else {
            return;
        };
        if !persistence.dirty {
            return;
        }
        persistence.dirty = false;
        // 快照是整个词库的克隆，序列化留给写盘线程。实测 20,000 条记录的词库克隆一次约 1.8 ms，
        // 在调用线程上直接序列化反而要约 3 ms
        let _ = persistence
            .tx
            .send(PersistMessage::Save(self.user_dict.clone()));
    }

    /// 词库文件若被别的进程改过（修改时间或长度与本引擎上次读入 / 写出时不同）就重新读入，
    /// 返回是否重新读入了；没有启用持久化时返回 `Ok(false)`。读取或解析失败时返回错误，内存里的词库保持不变。
    ///
    /// 已经交给写盘线程的快照会先写完再比较（否则它稍后会盖掉刚读入的内容）。
    /// 文件被删掉（别的进程清空了个人词库）按「变成了空词库」处理。
    /// 重新读入时，内存里还没交出去的改动（只可能是当前这次输入中途学到的）被丢弃
    pub fn reload_user_dict_if_changed(&mut self) -> std::io::Result<bool> {
        let Some(persistence) = self.persistence.as_mut() else {
            return Ok(false);
        };
        persistence.wait_for_writer(Duration::from_secs(2));
        // 先取修改时间与长度再读内容，理由同 `enable_persistence`
        let current = FileStamp::probe(&persistence.path)?;
        if current == persistence.known_stamp() {
            return Ok(false);
        }
        let (dict, cleaned) = match current {
            Some(_) => UserDict::read_from_file(&persistence.path)?,
            None => (UserDict::new(), false),
        };
        self.user_dict = dict;
        persistence.set_known_stamp(current);
        persistence.dirty = cleaned;
        self.flush_user_dict();
        Ok(true)
    }

    /// 等写盘线程把已经交给它的快照写完（最多等 `timeout`）。测试和工具用；正常使用不需要
    pub fn wait_for_user_dict_writes(&self, timeout: Duration) {
        if let Some(persistence) = &self.persistence {
            persistence.wait_for_writer(timeout);
        }
    }

    /// 执行 L1 毫秒级中英混合解码
    pub fn decode(
        &self,
        input: &str,
        scene: AppScene,
        surrounding_context: &str,
    ) -> DecodeOutput {
        self.decode_with_pins(input, scene, surrounding_context, &HashMap::new())
    }

    /// 执行带局部锁定片段 (`pinned_segments: start_pos -> (end_pos, text)`) 的混合解码
    pub fn decode_with_pins(
        &self,
        input: &str,
        scene: AppScene,
        surrounding_context: &str,
        pinned_segments: &HashMap<usize, (usize, String)>,
    ) -> DecodeOutput {
        let start = Instant::now();
        let lattice = self.get_or_build_lattice(input);
        let lattice_edge_count: usize = lattice.edges_from.iter().map(|v| v.len()).sum();
        let candidates = BilingualDecoder::decode_with_pins(
            &lattice,
            &self.lexicon,
            &self.user_dict,
            scene,
            surrounding_context,
            &self.decoder_config,
            pinned_segments,
        );
        DecodeOutput {
            input: input.to_string(),
            candidates,
            lattice_edge_count,
            elapsed: start.elapsed(),
        }
    }

    /// 查询长句中指定起始字节位置 `start_pos` 的所有可选替换词（支持用户点击任意分词块局部改词/改英文）
    pub fn query_segment_alternatives(
        &self,
        input: &str,
        start_pos: usize,
        preferred_end: usize,
    ) -> Vec<Candidate> {
        self.query_segment_alternatives_with_prev(input, start_pos, preferred_end, None)
    }

    /// 查询长句中指定起始字节位置 `start_pos` 的所有可选替换词，并结合前序已锁定/已解码词 `prev_word` 进行二元语法与构词搭配消歧
    pub fn query_segment_alternatives_with_prev(
        &self,
        input: &str,
        start_pos: usize,
        preferred_end: usize,
        prev_word: Option<&str>,
    ) -> Vec<Candidate> {
        if start_pos >= input.len() {
            return Vec::new();
        }
        let lattice = self.get_or_build_lattice(input);
        let mut edges = lattice.edges_from.get(start_pos).cloned().unwrap_or_default();
        // 补齐当前音节在词库中的全部同音单字（或简拼首字母对应的全部单字）
        let end_clamped = preferred_end.min(input.len()).max(start_pos + 1);
        let raw_slice = &input[start_pos..end_clamped];
        let lower_slice = raw_slice.to_ascii_lowercase();
        let exact_ids = self.lexicon.syllable_trie().exact_lookup(&lower_slice);
        let (lookup_ids, is_abbrev_initial) = if exact_ids.is_empty() && lower_slice.len() == 1 {
            (self.lexicon.abbrev_trie().exact_lookup(&lower_slice), true)
        } else {
            (exact_ids, false)
        };
        for id in lookup_ids {
            let entry = self.lexicon.entry(id);
            let entry_text = entry.text();
            if entry.syllable_count() == 1
                && !edges.iter().any(|e| e.end == end_clamped && e.text == entry_text)
            {
                edges.push(LatticeEdge {
                    start: start_pos,
                    end: end_clamped,
                    text: entry_text.to_string(),
                    code_key: if is_abbrev_initial {
                        lower_slice.clone()
                    } else {
                        entry.syllable_key().to_string()
                    },
                    kind: EdgeKind::ChineseChar,
                    base_cost: entry.cost(),
                    domain: entry.domain(),
                    syllable_count: 1,
                    is_fuzzy: false,
                });
            }
        }
        edges.retain(|e| !e.text.is_empty());

        // 将从当前音节出发的候选严格分层，对齐主流输入法体验：
        // 1) 从该音节起始的所有精确匹配多字词组（三字词、双字词排在单字之前，如 `ni'zai` -> "你在", "你再", "拟在"；`shi'yong` -> "使用", "实用"；`z'j` -> "自己", "直接"）
        // 2) 当前音节精确对应的全部同音单字（`e.end == preferred_end`，如 `ni` -> "你", "尼", "泥", "逆", "拟"；或简拼首字母单字）
        // 3) 模糊音/纠错词与单字兜底
        let n = input.len();
        let has_full_pinyin_at_start = edges.iter().any(|e| {
            !e.is_fuzzy
                && (e.kind == EdgeKind::ChineseWord
                    || (e.kind == EdgeKind::ChineseChar && e.end > start_pos + 1))
        });
        let rank_cost = |e: &LatticeEdge| -> f32 {
            let raw = &input[e.start..e.end.min(n)];
            let ctx_bonus = if let Some(pw) = prev_word {
                self.lexicon.bigram_bonus(pw, &e.text)
                    + self.user_dict.bigram_boost(pw, &e.text)
                    + crate::decoder::word_pair_collocation_bonus(pw, e)
            } else {
                0.0
            };
            e.base_cost - self.user_dict.user_boost(raw, &e.text).min(6.0) - ctx_bonus
                + if e.is_fuzzy { 1.5 } else { 0.0 }
        };
        let is_incomplete_tail = |e: &LatticeEdge| -> bool {
            e.kind == EdgeKind::ChineseWord
                && e.code_key.bytes().filter(|&b| b != b'\'').count() > e.end.saturating_sub(e.start)
        };
        let mut multi_syl_words: Vec<LatticeEdge> = edges
            .iter()
            .filter(|e| {
                if e.end <= preferred_end {
                    return false;
                }
                // 过滤把后继完整全拼音节切碎的半音节词（如 `zaijia` 中 `0..5 zai'ji -> 在即` 切碎了 `3..6 jia`）
                if has_full_pinyin_at_start && e.end < n {
                    let cuts_full_syl = (start_pos + 1..e.end).any(|s| {
                        lattice.edges_from.get(s).is_some_and(|es| {
                            es.iter().any(|ce| {
                                ce.kind == EdgeKind::ChineseChar
                                    && !ce.is_fuzzy
                                    && ce.end > e.end
                            })
                        })
                    });
                    if cuts_full_syl {
                        return false;
                    }
                }
                e.kind == EdgeKind::ChineseWord
                    || (!has_full_pinyin_at_start && e.kind == EdgeKind::AbbrevPinyin)
                    || (e.end == n && e.kind == EdgeKind::EnglishWord)
            })
            .cloned()
            .collect();
        multi_syl_words.sort_by(|a, b| {
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

        let mut exact_syl_edges: Vec<LatticeEdge> = edges
            .iter()
            .filter(|e| {
                e.end == preferred_end
                    && (matches!(e.kind, EdgeKind::ChineseChar | EdgeKind::ChineseWord)
                        || (e.end == n && e.kind == EdgeKind::EnglishWord))
            })
            .cloned()
            .collect();
        exact_syl_edges.sort_by(|a, b| {
            a.is_fuzzy.cmp(&b.is_fuzzy).then_with(|| {
                rank_cost(a)
                    .partial_cmp(&rank_cost(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });

        let mut other_edges: Vec<LatticeEdge> = edges
            .into_iter()
            .filter(|e| {
                if e.end < n {
                    if matches!(
                        e.kind,
                        EdgeKind::EnglishWord
                            | EdgeKind::EnglishPrefixCompletion
                            | EdgeKind::EnglishRaw
                    ) {
                        return false;
                    }
                    if e.kind == EdgeKind::AbbrevPinyin && has_full_pinyin_at_start {
                        return false;
                    }
                }
                true
            })
            .collect();
        other_edges.sort_by(|a, b| {
            a.is_fuzzy.cmp(&b.is_fuzzy).then_with(|| {
                rank_cost(a)
                    .partial_cmp(&rank_cost(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });

        let mut ordered_edges = Vec::new();
        let mut full_span_taken = 0usize;
        let mut prefix_word_taken = 0usize;
        let mut deferred_full_span = Vec::new();
        // 1) 精确匹配的多字词组排在单字之前：
        //    - 若词组覆盖到整句末尾 (`e.end >= n`，如 `fan |zhong li` 下的「重力」「中立」「钟离」，或 `|zai jia` 下的「在家」「再加」「宅家」)，
        //      首屏先取前 3 个，紧接着穿插前 2 个最高频单字（保证第 1 页同时有词组与常用单字），随后立即补齐其余同音多字词；
        //    - 若词组是长句中的局部前缀词 (`e.end < n`，如 `nizaifansipaichayixia` 下的「你在」「你再」「拟在」)，最多取 12 个。
        for e in multi_syl_words.iter().filter(|e| !e.is_fuzzy) {
            if e.end >= n {
                if full_span_taken < 3 {
                    ordered_edges.push(e.clone());
                    full_span_taken += 1;
                } else if deferred_full_span.len() < 10 {
                    deferred_full_span.push(e.clone());
                }
            } else if prefix_word_taken < 12 {
                ordered_edges.push(e.clone());
                prefix_word_taken += 1;
            }
        }
        // 2) 当前音节精确匹配的同音单字（在穿插前 2 个常用单字后立即补回延后的同音多字词，避免被几十个生僻单字挤到十几页之后）
        let mut char_taken = 0usize;
        let mut deferred_flushed = false;
        for e in exact_syl_edges.iter().filter(|e| !e.is_fuzzy) {
            if !deferred_flushed && char_taken >= 2 {
                deferred_flushed = true;
                ordered_edges.extend(deferred_full_span.drain(..));
            }
            ordered_edges.push(e.clone());
            char_taken += 1;
        }
        if !deferred_flushed {
            ordered_edges.extend(deferred_full_span.drain(..));
        }
        // 3) 其余生僻多字词与模糊音候选兜底
        for e in multi_syl_words {
            ordered_edges.push(e);
        }
        for e in exact_syl_edges {
            ordered_edges.push(e);
        }
        for e in other_edges {
            ordered_edges.push(e);
        }

        let mut out = Vec::new();
        let mut seen = HashSet::new();

        // 优先注入用户针对当前音节高频记忆的单字或词条（含全拼历史词的简拼联想）
        let end_clamped = preferred_end.min(input.len()).max(start_pos + 1);
        let raw_slice = &input[start_pos..end_clamped];
        let mut user_recs = self.user_dict.top_user_records(raw_slice);
        for jrec in self.user_dict.top_jianpin_records(raw_slice) {
            if !user_recs.iter().any(|r| r.text == jrec.text) {
                user_recs.push(jrec);
            }
        }
        for urec in user_recs.into_iter().take(4) {
            if (urec.dynamic_score >= 4.5
                || urec.commit_count >= 2
                || (urec.code_key != raw_slice && urec.commit_count >= 1))
                && seen.insert(urec.text.clone())
            {
                let span = SegmentSpan {
                    start: start_pos,
                    end: end_clamped,
                    text: urec.text.clone(),
                    code_key: raw_slice.to_string(),
                };
                out.push(Candidate {
                    text: urec.text.clone(),
                    preedit: raw_slice.to_string(),
                    consumed_len: end_clamped.saturating_sub(start_pos),
                    l1_cost: 0.05,
                    final_score: 135.0 + urec.dynamic_score,
                    ai_boosted: false,
                    tag: format!("[记忆·{}次]", urec.commit_count),
                    segments: vec![urec.text],
                    segment_spans: vec![span.clone()],
                    syllable_spans: vec![span],
                });
            }
        }

        for e in ordered_edges {
            if !seen.insert(e.text.clone()) {
                continue;
            }
            let tag = if e.is_fuzzy {
                format!("(纠错 {})", e.code_key)
            } else {
                match e.kind {
                    EdgeKind::EnglishWord | EdgeKind::EnglishPrefixCompletion | EdgeKind::EnglishRaw => {
                        format!("({})", e.code_key)
                    }
                    EdgeKind::ChineseWord => format!("({})", e.code_key),
                    EdgeKind::ChineseChar => format!("({})", e.code_key),
                    EdgeKind::AbbrevPinyin => format!("(简拼 {})", e.code_key),
                }
            };
            let span = SegmentSpan {
                start: e.start,
                end: e.end,
                text: e.text.clone(),
                code_key: e.code_key.clone(),
            };
            out.push(Candidate {
                text: e.text.clone(),
                preedit: e.code_key,
                consumed_len: e.end.saturating_sub(e.start),
                l1_cost: e.base_cost,
                final_score: 100.0 - e.base_cost,
                ai_boosted: false,
                tag,
                segments: vec![e.text],
                segment_spans: vec![span.clone()],
                syllable_spans: vec![span],
            });
            if out.len() >= 200 {
                break;
            }
        }

        // 末尾保底追加该音节区间的英文全大写与原样小写选项（按 Tab 可一键跳选，且不挤占前几页中文候选）
        for (en_variant, label) in [
            (raw_slice.to_ascii_uppercase(), "[英文大写]"),
            (raw_slice.to_ascii_lowercase(), "[英文原词]"),
        ] {
            if seen.insert(en_variant.clone()) {
                let span = SegmentSpan {
                    start: start_pos,
                    end: end_clamped,
                    text: en_variant.clone(),
                    code_key: raw_slice.to_string(),
                };
                out.push(Candidate {
                    text: en_variant.clone(),
                    preedit: raw_slice.to_string(),
                    consumed_len: end_clamped.saturating_sub(start_pos),
                    l1_cost: 2.0,
                    final_score: 96.0,
                    ai_boosted: false,
                    tag: label.to_string(),
                    segments: vec![en_variant],
                    segment_spans: vec![span.clone()],
                    syllable_spans: vec![span],
                });
            }
        }

        out
    }

    /// 记录用户最终选词上屏，更新个人动态词频与二元习惯（写盘见 [`Self::flush_user_dict`]）
    pub fn commit_candidate(&mut self, input_code: &str, text: &str, prev_word: Option<&str>) {
        self.commit_candidate_ex(input_code, text, prev_word, true);
    }

    /// 记录用户选词上屏（支持区分主动改选非首选词/造词 `is_explicit_override=true` 与默认空格首选上屏）
    pub fn commit_candidate_ex(
        &mut self,
        input_code: &str,
        text: &str,
        prev_word: Option<&str>,
        is_explicit_override: bool,
    ) {
        self.user_dict
            .record_commit_ex(input_code, text, prev_word, is_explicit_override);
        self.mark_user_dict_dirty();
    }

    /// 记住用户在这段上文下为 `input_code` 主动改选了 `text`（上文相关的纠正记忆，见 `UserDict::record_context_choice`）
    pub fn learn_context_choice(&mut self, context: &str, input_code: &str, text: &str) {
        self.user_dict.record_context_choice(context, input_code, text);
        self.mark_user_dict_dirty();
    }

    /// 仅学习「上一个词 -> 本词」的个人搭配习惯（句中单字改选时使用）
    pub fn learn_bigram(&mut self, prev_word: &str, text: &str) {
        self.user_dict.record_bigram(prev_word, text);
        self.mark_user_dict_dirty();
    }
}

impl Drop for ImeCoreEngine {
    /// 销毁前把还没交出去的个人词库改动交给写盘线程，并等它写完（最多 2 秒），
    /// 这样紧接着打开同一个词库文件的引擎读到的就是最新内容
    fn drop(&mut self) {
        if self.persistence.is_some() {
            self.flush_user_dict();
            self.wait_for_user_dict_writes(Duration::from_secs(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn build_test_engine() -> ImeCoreEngine {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let lexicon = CompiledLexicon::load_or_build_default(root).expect("load seed lexicon");
        ImeCoreEngine::new(lexicon)
    }

    /// 个人词库持久化测试用的临时目录，结束时删掉
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("ime-core-persist-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const WRITE_WAIT: Duration = Duration::from_secs(5);

    fn persistent_engine(path: &std::path::Path) -> ImeCoreEngine {
        let mut engine = build_test_engine();
        engine.enable_persistence(path.to_path_buf());
        engine
    }

    #[test]
    fn test_learning_is_written_at_flush_not_at_every_learn_call() {
        let dir = TempDir::new("flush");
        let path = dir.0.join("user_dict.json");
        let mut engine = persistent_engine(&path);

        // 学习只改内存：不管学了几条，没有 flush 就不取快照、不写盘
        engine.commit_candidate_ex("shiyan", "誓言", None, true);
        engine.learn_bigram("我的", "誓言");
        engine.learn_context_choice("我对天发誓这是我的", "shiyan", "誓言");
        engine.wait_for_user_dict_writes(WRITE_WAIT);
        assert!(!path.exists(), "nothing is written before the flush");

        // 一次 flush 把全部改动写出去
        engine.flush_user_dict();
        engine.wait_for_user_dict_writes(WRITE_WAIT);
        let saved = UserDict::load_from_file(&path).expect("saved");
        assert!(saved.user_boost("shiyan", "誓言") > 0.0);
        assert!(saved.bigram_boost("我的", "誓言") > 0.0);
        assert_eq!(saved.context_choice("我对天发誓这是我的", "shiyan"), Some("誓言"));

        // 没有新改动时 flush 不再写：文件的修改时间不变
        let before = FileStamp::probe(&path).unwrap();
        engine.flush_user_dict();
        engine.wait_for_user_dict_writes(WRITE_WAIT);
        assert_eq!(FileStamp::probe(&path).unwrap(), before);

        // 没来得及 flush 的改动在引擎销毁时写出，并且销毁返回时已经写完
        engine.commit_candidate_ex("jishu", "级数", None, true);
        drop(engine);
        let saved = UserDict::load_from_file(&path).expect("saved");
        assert!(saved.user_boost("jishu", "级数") > 0.0);
        assert!(saved.user_boost("shiyan", "誓言") > 0.0);
    }

    #[test]
    fn test_reload_picks_up_what_another_engine_wrote() {
        let dir = TempDir::new("reload");
        let path = dir.0.join("user_dict.json");
        // B 先打开（此时词库文件还不存在），A 后学习
        let mut b = persistent_engine(&path);
        let mut a = persistent_engine(&path);
        assert!(!b.reload_user_dict_if_changed().unwrap(), "nothing changed yet");

        a.commit_candidate_ex("shiyan", "誓言", None, true);
        a.flush_user_dict();
        a.wait_for_user_dict_writes(WRITE_WAIT);

        // B 还是旧的；重新读入之后看到 A 学的
        assert_eq!(b.user_dict.user_boost("shiyan", "誓言"), 0.0);
        assert!(b.reload_user_dict_if_changed().unwrap());
        assert!(b.user_dict.user_boost("shiyan", "誓言") > 0.0);
        assert!(!b.reload_user_dict_if_changed().unwrap(), "unchanged since the reload");

        // B 接着学别的并写盘：A 学的还在（以前 B 会用自己的旧快照盖掉它）
        b.commit_candidate_ex("jishu", "级数", None, true);
        b.flush_user_dict();
        b.wait_for_user_dict_writes(WRITE_WAIT);
        assert!(!b.reload_user_dict_if_changed().unwrap(), "own writes are not a change");

        let c = persistent_engine(&path);
        assert!(c.user_dict.user_boost("shiyan", "誓言") > 0.0);
        assert!(c.user_dict.user_boost("jishu", "级数") > 0.0);

        // A 这边也能跟上 B
        assert!(a.reload_user_dict_if_changed().unwrap());
        assert!(a.user_dict.user_boost("jishu", "级数") > 0.0);
    }

    #[test]
    fn test_reload_waits_for_a_queued_snapshot_and_keeps_memory_on_a_bad_file() {
        let dir = TempDir::new("reload-edge");
        let path = dir.0.join("user_dict.json");
        let mut engine = persistent_engine(&path);

        // 刚交给写盘线程的快照先写完再比较：自己写的不算「被别人改过」，内容也不会丢
        engine.commit_candidate_ex("shiyan", "誓言", None, true);
        engine.flush_user_dict();
        assert!(!engine.reload_user_dict_if_changed().unwrap());
        assert!(path.exists(), "the queued snapshot was written before comparing");
        assert!(engine.user_dict.user_boost("shiyan", "誓言") > 0.0);

        // 文件被写坏：报错，内存里的词库保持不变
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(engine.reload_user_dict_if_changed().is_err());
        assert!(engine.user_dict.user_boost("shiyan", "誓言") > 0.0);

        // 文件被删掉（别的进程清空了个人词库）：按空词库重新读入
        std::fs::remove_file(&path).unwrap();
        assert!(engine.reload_user_dict_if_changed().unwrap());
        assert!(engine.user_dict.records.is_empty());
        assert!(!engine.reload_user_dict_if_changed().unwrap());

        // 没有启用持久化的引擎：什么也不做
        let mut plain = build_test_engine();
        plain.commit_candidate_ex("shiyan", "誓言", None, true);
        plain.flush_user_dict();
        assert!(!plain.reload_user_dict_if_changed().unwrap());
        assert!(plain.user_dict.user_boost("shiyan", "誓言") > 0.0);
    }

    #[test]
    fn test_a_failed_write_does_not_stop_later_writes() {
        let dir = TempDir::new("retry");
        // 上级目录还不存在：第一次写失败（在标准错误上报告一次）
        let path = dir.0.join("later").join("user_dict.json");
        let mut engine = persistent_engine(&path);
        engine.commit_candidate_ex("shiyan", "誓言", None, true);
        engine.flush_user_dict();
        engine.wait_for_user_dict_writes(WRITE_WAIT);
        assert!(!path.exists());

        // 目录出现之后，下一次上屏照常写出最新的词库（包括上次没写成的内容）
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        engine.commit_candidate_ex("jishu", "级数", None, true);
        engine.flush_user_dict();
        engine.wait_for_user_dict_writes(WRITE_WAIT);
        let saved = UserDict::load_from_file(&path).expect("saved");
        assert!(saved.user_boost("shiyan", "誓言") > 0.0);
        assert!(saved.user_boost("jishu", "级数") > 0.0);
    }

    #[test]
    fn test_seamless_bilingual_mixed_input() {
        let engine = build_test_engine();

        // 测试无空格中英文混输: "wozaiyongrustxieapi" -> "我在用 Rust 写 API"
        let out = engine.decode("wozaiyongrustxieapi", AppScene::CodeEditor, "");
        assert!(!out.candidates.is_empty());
        assert_eq!(out.candidates[0].text, "我在用 Rust 写 API");

        // 测试无空格中英文混输: "zhegebughennanxiufu" -> "这个 bug 很难修复"
        let out2 = engine.decode("zhegebughennanxiufu", AppScene::CodeEditor, "");
        assert_eq!(out2.candidates[0].text, "这个 bug 很难修复");

        // 测试无空格中英文混输与规范大小写: "qugithubxiazaiopensourcexiangmu"
        let out3 = engine.decode("qugithubxiazaiopensourcexiangmu", AppScene::General, "");
        assert_eq!(out3.candidates[0].text, "去 GitHub 下载 open source 项目");
    }

    #[test]
    fn test_syllable_segmentation_disambiguation() {
        let engine = build_test_engine();

        // 测试 fangan -> 方案 vs 反感
        let out_fangan = engine.decode("fangan", AppScene::General, "设计一套解决");
        assert_eq!(out_fangan.candidates[0].text, "方案");

        let out_fangan2 = engine.decode("fangan", AppScene::General, "让人非常");
        assert_eq!(out_fangan2.candidates[0].text, "反感");
    }

    #[test]
    fn test_fuzzy_correction_never_overrides_correct_input() {
        let engine = build_test_engine();
        let top = |s: &str| engine.decode(s, AppScene::General, "").candidates[0].text.clone();
        let top5 = |s: &str| -> Vec<String> {
            engine
                .decode(s, AppScene::General, "")
                .candidates
                .iter()
                .take(5)
                .map(|c| c.text.clone())
                .collect()
        };

        // 1. 正确输入必须原样优先，不能被平翘舌/前后鼻音纠错项抢走首位
        assert_eq!(top("ni"), "你");
        assert_ne!(top("si"), "是");
        assert_eq!(top("sixiang"), "思想");
        assert_eq!(top("zanshi"), "暂时");
        assert_eq!(top("lvse"), "绿色");
        assert_eq!(top("nvren"), "女人");
        assert!(!top5("nvren").contains(&"路人".to_string()), "lv/nv 不应被改成 lu/nu");
        // 长句中已存在精确双字词+助词（如 `xiazaideanjian` -> `下载的按键`）时，绝不能被词库预组三字模糊词（`xia'zhai'de` -> `狭窄的按键`）反超
        let xzdaj = top5("xiazaideanjian");
        assert_eq!(xzdaj[0], "下载的按键", "xiazaideanjian top5: {:?}", xzdaj);
        assert!(xzdaj.contains(&"下载的".to_string()), "xiazaideanjian top5: {:?}", xzdaj);
        assert!(xzdaj.contains(&"下载".to_string()), "xiazaideanjian top5: {:?}", xzdaj);

        // 2. 输错时纠错项仍能出现在第一页
        assert_eq!(top("zidao"), "知道");
        assert!(top5("zengde").contains(&"真的".to_string()));
        assert!(top5("cenxu").contains(&"程序".to_string()));
        assert!(top5("beijign").contains(&"北京".to_string()));

        // 3. `lue/nue` 是 `lve/nve` 的合法写法
        assert!(top5("hulue").contains(&"忽略".to_string()), "{:?}", top5("hulue"));

        // 4. 长句的第 2、3 候选不应被纠错垃圾句（如 "CPU 三"）占据
        let long = engine.decode(
            "jintiantianqibucuowomenyiqiquchifanbaranhouzaiqukandianying",
            AppScene::General,
            "",
        );
        assert!(long.candidates.iter().take(3).all(|c| !c.tag.starts_with("[纠错")));
    }

    #[test]
    fn test_single_char_memory_does_not_poison_sentences() {
        let mut engine = build_test_engine();
        let before = engine.decode("haodeyang", AppScene::General, "").candidates[0].text.clone();
        // 用户曾单独把 `de` 选成「得」
        engine.commit_candidate_ex("de", "得", None, true);
        // 单独输入 de 时应记住
        assert_eq!(engine.decode("de", AppScene::General, "").candidates[0].text, "得");
        // 但长句里的「的」不应被全部替换
        let after = engine.decode("haodeyang", AppScene::General, "").candidates[0].text.clone();
        assert_eq!(before, after);
        assert_eq!(
            engine.decode("tadepengyou", AppScene::General, "").candidates[0].text,
            "他的朋友"
        );
    }

    #[test]
    fn test_long_hardware_bilingual_sentence_and_segment_pinning() {
        let engine = build_test_engine();
        let input = "woshiyongclaudedeapizuochengxuyunxingzainvidiadexiankayijiamddecpushang";
        let out = engine.decode(input, AppScene::CodeEditor, "");
        println!("Top candidates for {} (elapsed = {:?}, edges = {}):", input, out.elapsed, out.lattice_edge_count);
        for (i, c) in out.candidates.iter().take(5).enumerate() {
            println!("  {}. {} ({})", i + 1, c.text, c.preedit);
        }
        assert!(
            out.candidates[0].text == "我使用 Claude 的 API 做程序运行在 NVIDIA 的显卡以及 AMD 的 CPU 上"
                || out.candidates[0].text == "我是用 Claude 的 API 做程序运行在 NVIDIA 的显卡以及 AMD 的 CPU 上",
            "Got unexpected top candidate: {}",
            out.candidates[0].text
        );

        // 测试分段点击修改 (Pinned Segment)：将首段 woshiyong (0..9) 强制锁定为 "我是用" 或 "我使用"
        let mut pins = HashMap::new();
        pins.insert(0, (9, "我是用".to_string()));
        let out_pinned = engine.decode_with_pins(input, AppScene::CodeEditor, "", &pins);
        assert_eq!(
            out_pinned.candidates[0].text,
            "我是用 Claude 的 API 做程序运行在 NVIDIA 的显卡以及 AMD 的 CPU 上"
        );
    }

    #[test]
    fn test_single_syllable_common_char_frequency_order() {
        let engine = build_test_engine();
        let top5 = |s: &str| -> Vec<String> {
            engine
                .decode(s, AppScene::General, "")
                .candidates
                .iter()
                .take(5)
                .map(|c| c.text.clone())
                .collect()
        };

        // lan 第一页必须是常用高频字（蓝、烂、懒、拦、篮等），绝不能出现生僻字（婪、阑、榄、斓）
        let lan5 = top5("lan");
        assert!(
            lan5.contains(&"蓝".to_string()) || lan5.contains(&"兰".to_string()),
            "lan top5 should contain common chars: {:?}",
            lan5
        );
        for rare in ["婪", "阑", "榄", "斓", "缆"] {
            assert!(
                !lan5.contains(&rare.to_string()),
                "lan top5 should not contain rare char {}: {:?}",
                rare,
                lan5
            );
        }

        // qiu 第一页必须是常用高频字（求、球、秋等），绝不能出现生僻字（蚯、俅、泅、遒、逑）
        let qiu5 = top5("qiu");
        assert!(
            qiu5.contains(&"求".to_string()) || qiu5.contains(&"球".to_string()),
            "qiu top5 should contain common chars: {:?}",
            qiu5
        );
        for rare in ["蚯", "俅", "泅", "遒", "逑"] {
            assert!(
                !qiu5.contains(&rare.to_string()),
                "qiu top5 should not contain rare char {}: {:?}",
                rare,
                qiu5
            );
        }

        // tuo 第一页必须是常用高频字（托、拖、脱、妥等），绝不能出现生僻字（沱、砣、鸵、跎、柁）
        let tuo5 = top5("tuo");
        assert!(
            tuo5.contains(&"托".to_string())
                || tuo5.contains(&"拖".to_string())
                || tuo5.contains(&"脱".to_string()),
            "tuo top5 should contain common chars: {:?}",
            tuo5
        );
        for rare in ["沱", "砣", "鸵", "跎", "柁"] {
            assert!(
                !tuo5.contains(&rare.to_string()),
                "tuo top5 should not contain rare char {}: {:?}",
                rare,
                tuo5
            );
        }

        // ji 必须包含「吉」等全部 8105 常用字（不能在 24 字处被截断）
        let ji_all: Vec<String> = engine
            .decode("ji", AppScene::General, "")
            .candidates
            .into_iter()
            .map(|c| c.text)
            .collect();
        assert!(
            ji_all.contains(&"吉".to_string()),
            "ji candidates should contain 吉, total={}",
            ji_all.len()
        );
        assert!(ji_all.len() >= 100, "ji should have >=100 candidates, got {}", ji_all.len());

        // wzj 简拼必须同时提供：3字词（为自己/我自己）、2字前缀词（我在/文章/位置/完整）、1字首字母单字（我/为/王/问），且音节切分为 w'z'j
        let wzj_out = engine.decode("wzj", AppScene::General, "");
        let wzj_texts: Vec<String> = wzj_out.candidates.iter().map(|c| c.text.clone()).collect();
        println!("wzj total={}, top15={:?}", wzj_texts.len(), &wzj_texts[..wzj_texts.len().min(15)]);
        assert!(wzj_texts.contains(&"为自己".to_string()));
        assert!(wzj_texts.contains(&"我在".to_string()));
        assert!(wzj_texts.contains(&"文章".to_string()));
        assert!(wzj_texts.contains(&"位置".to_string()));
        assert!(wzj_texts.contains(&"完整".to_string()));
        assert!(wzj_texts.contains(&"我".to_string()));
        assert!(wzj_texts.contains(&"王".to_string()));
        let top_syls: Vec<&str> = wzj_out.candidates[0]
            .syllable_spans
            .iter()
            .map(|s| s.code_key.as_str())
            .collect();
        assert_eq!(top_syls, vec!["w", "z", "j"]);

        // 用户曾经用全拼输入过的词（如 wenzhaiji -> 文摘集），之后输入简写 wzj 必须能自动联想到并排在首位
        let mut engine_mem = build_test_engine();
        engine_mem.commit_candidate_ex("wenzhaiji", "文摘集", None, true);
        let wzj_after = engine_mem.decode("wzj", AppScene::General, "");
        assert_eq!(wzj_after.candidates[0].text, "文摘集");

        // 混拼/半拼与递进短语候选（对齐微软拼音体验：输入 keyiyouhuadedif 应依次给出「可以优化的地方」「可以优化的」「可以优化」「可以有」「可以」等，绝不输出「可以优化的地 f」）
        let ky_out = engine.decode("keyiyouhuadedif", AppScene::General, "");
        let ky_top7: Vec<String> = ky_out
            .candidates
            .iter()
            .take(7)
            .map(|c| c.text.clone())
            .collect();
        println!("keyiyouhuadedif top7 = {:?}", ky_top7);
        assert_eq!(ky_top7[0], "可以优化的地方");
        assert!(ky_top7.contains(&"可以优化的".to_string()));
        assert!(ky_top7.contains(&"可以优化".to_string()));
        assert!(ky_top7.contains(&"可以有".to_string()));
        assert!(ky_top7.contains(&"可以".to_string()));

        // 验证个人词库防污染与多音节切分保护：
        // 1) 残片「可以优化的地 f」禁止记入或污染 UserDict；
        // 2) 即使「可以优化的地方」被高频记入 UserDict，仍保持 7 音节空格切分与递进前缀候选（可以优化的/可以优化）；
        // 3) 自造词（如「武增吉」wuzengji）从 UserDict 召回时，音节栏必须切分为 ["wu", "zeng", "ji"] 而非整坨粘连
        for _ in 0..3 {
            engine_mem.commit_candidate_ex("keyiyouhuadedif", "可以优化的地 f", None, true);
            engine_mem.commit_candidate_ex("keyiyouhuadedif", "可以优化的地方", None, true);
        }
        let ky_mem = engine_mem.decode("keyiyouhuadedif", AppScene::General, "");
        assert_eq!(ky_mem.candidates[0].text, "可以优化的地方");
        let ky_syls: Vec<&str> = ky_mem.candidates[0]
            .syllable_spans
            .iter()
            .map(|s| s.code_key.as_str())
            .collect();
        assert_eq!(ky_syls, vec!["ke", "yi", "you", "hua", "de", "di", "f"]);
        let ky_mem_texts: Vec<String> = ky_mem.candidates.iter().take(7).map(|c| c.text.clone()).collect();
        assert!(ky_mem_texts.contains(&"可以优化的".to_string()));
        assert!(ky_mem_texts.contains(&"可以优化".to_string()));

        engine_mem.commit_candidate_ex("wuzengji", "武增吉", None, true);
        let wzj_custom = engine_mem.decode("wuzengji", AppScene::General, "");
        assert_eq!(wzj_custom.candidates[0].text, "武增吉");
        let custom_syls: Vec<&str> = wzj_custom.candidates[0]
            .syllable_spans
            .iter()
            .map(|s| s.code_key.as_str())
            .collect();
        assert_eq!(custom_syls, vec!["wu", "zeng", "ji"]);

        // 常见半拼/混拼与未完音节测试（含句首/句中/句尾孤立声母兜底，绝不退化为原样英文字母如「z 也不对」「切换的 w 时候」）
        assert_eq!(engine.decode("dif", AppScene::General, "").candidates[0].text, "地方");
        assert_eq!(engine.decode("zenm", AppScene::General, "").candidates[0].text, "怎么");
        assert_eq!(engine.decode("shenm", AppScene::General, "").candidates[0].text, "什么");
        assert_eq!(engine.decode("weishenm", AppScene::General, "").candidates[0].text, "为什么");
        assert_eq!(engine.decode("zenmy", AppScene::General, "").candidates[0].text, "怎么样");
        assert_eq!(engine.decode("shurf", AppScene::General, "").candidates[0].text, "输入法");
        assert_eq!(engine.decode("zme", AppScene::General, "").candidates[0].text, "怎么");
        assert_eq!(engine.decode("sme", AppScene::General, "").candidates[0].text, "什么");
        assert_eq!(engine.decode("zmeban", AppScene::General, "").candidates[0].text, "怎么办");
        let zye_top = &engine.decode("zyebudui", AppScene::General, "").candidates[0].text;
        assert!(
            !zye_top.chars().any(|c| c.is_ascii_alphabetic()),
            "zyebudui should decode to pure Chinese sentence without raw ASCII 'z', got {zye_top}"
        );
        let qhw_top = &engine.decode("qiehuandewshihou", AppScene::General, "").candidates[0].text;
        assert!(
            !qhw_top.chars().any(|c| c.is_ascii_alphabetic()),
            "qiehuandewshihou should decode to pure Chinese sentence without raw ASCII 'w', got {qhw_top}"
        );

        // 现代汉语虚词搭配与句末语气助词测试（对齐微软拼音体验：xianzaizaiyanjiuyixia -> 现在再研究一下，shibieba -> 识别吧）
        let xz_out = engine.decode("xianzaizaiyanjiuyixia", AppScene::General, "");
        let xz_top5: Vec<String> = xz_out
            .candidates
            .iter()
            .take(5)
            .map(|c| c.text.clone())
            .collect();
        println!("xianzaizaiyanjiuyixia top5 = {:?}", xz_top5);
        assert_eq!(xz_top5[0], "现在再研究一下");
        assert!(xz_top5.contains(&"现在在".to_string()));
        assert!(xz_top5.contains(&"现在".to_string()));

        let sb_out = engine.decode("shibieba", AppScene::General, "");
        let sb_top5: Vec<String> = sb_out
            .candidates
            .iter()
            .take(5)
            .map(|c| c.text.clone())
            .collect();
        println!("shibieba top5 = {:?}", sb_top5);
        assert_eq!(sb_top5[0], "识别吧");
        assert_eq!(sb_top5[1], "识别");

        // 构词前缀 + 词根搭配测试（fanzhongli -> 反重力；锁定「反」后 zhongli 首选「重力」且包含「中立」，绝不被「重量/肿瘤」跨音节劫持）
        let fzl_out = engine.decode("fanzhongli", AppScene::General, "");
        let fzl_top5: Vec<String> = fzl_out
            .candidates
            .iter()
            .take(5)
            .map(|c| c.text.clone())
            .collect();
        println!("fanzhongli top5 = {:?}", fzl_top5);
        assert_eq!(fzl_top5[0], "反重力");

        let mut pins = HashMap::new();
        pins.insert(0usize, (3usize, "反".to_string()));
        let fzl_pinned = engine.decode_with_pins("fanzhongli", AppScene::General, "", &pins);
        assert_eq!(fzl_pinned.candidates[0].text, "反重力");
        let zl_alts = engine.query_segment_alternatives_with_prev("fanzhongli", 3, 8, Some("反"));
        let zl_top5: Vec<String> = zl_alts.iter().take(5).map(|c| c.text.clone()).collect();
        println!("fan |zhong li top5 = {:?}", zl_top5);
        assert_eq!(zl_top5[0], "重力");
        assert!(zl_top5.contains(&"中立".to_string()));
        assert!(!zl_top5.contains(&"重量".to_string()));
        assert!(!zl_top5.contains(&"肿瘤".to_string()));

        // 量词短语 + 可数名词语法搭配测试（zhegeci -> 这个词，绝不输出「这个此」）
        let zgc_out = engine.decode("zhegeci", AppScene::General, "");
        let zgc_top5: Vec<String> = zgc_out
            .candidates
            .iter()
            .take(5)
            .map(|c| c.text.clone())
            .collect();
        println!("zhegeci top5 = {:?}", zgc_top5);
        assert_eq!(zgc_top5[0], "这个词");
        assert_eq!(engine.decode("nageci", AppScene::General, "").candidates[0].text, "那个词");
        assert_eq!(engine.decode("yigeci", AppScene::General, "").candidates[0].text, "一个词");
        assert_eq!(engine.decode("huangeci", AppScene::General, "").candidates[0].text, "换个词");
        assert_eq!(engine.decode("zhegezi", AppScene::General, "").candidates[0].text, "这个字");
        assert_eq!(engine.decode("zhegeren", AppScene::General, "").candidates[0].text, "这个人");
        assert_eq!(engine.decode("zhegeyue", AppScene::General, "").candidates[0].text, "这个月");
        assert_eq!(engine.decode("zhegequn", AppScene::General, "").candidates[0].text, "这个群");
        assert_eq!(engine.decode("zhegeti", AppScene::General, "").candidates[0].text, "这个题");
        assert_eq!(engine.decode("xiayiye", AppScene::General, "").candidates[0].text, "下一页");
        assert_eq!(engine.decode("zheyihang", AppScene::General, "").candidates[0].text, "这一行");
        assert_eq!(engine.decode("zheyibu", AppScene::General, "").candidates[0].text, "这一步");
        assert_eq!(engine.decode("zheyiceng", AppScene::General, "").candidates[0].text, "这一层");
        assert_eq!(engine.decode("zheyiduan", AppScene::General, "").candidates[0].text, "这一段");

        // 口语疑问习语与「回事 vs 会是」消歧测试（含模拟子词高频记忆污染场景）
        let zmhs_out = engine.decode("zenmehuishi", AppScene::General, "");
        let zmhs_top5: Vec<String> = zmhs_out
            .candidates
            .iter()
            .take(5)
            .map(|c| c.text.clone())
            .collect();
        println!("zenmehuishi top5 = {:?}", zmhs_top5);
        assert_eq!(zmhs_top5[0], "怎么回事");
        assert!(zmhs_top5.contains(&"怎么会是".to_string()));
        assert_eq!(
            engine.decode("daodizenmehuishi", AppScene::General, "").candidates[0].text,
            "到底怎么回事"
        );
        assert_eq!(
            engine.decode("zheshizenmehuishi", AppScene::General, "").candidates[0].text,
            "这是怎么回事"
        );
        assert_eq!(
            engine.decode("shizenmehuishi", AppScene::General, "").candidates[0].text,
            "是怎么回事"
        );
        assert_eq!(
            engine.decode("yihuishi", AppScene::General, "").candidates[0].text,
            "一回事"
        );
        assert_eq!(
            engine.decode("lianghuishi", AppScene::General, "").candidates[0].text,
            "两回事"
        );
        assert_eq!(
            engine.decode("danghuishi", AppScene::General, "").candidates[0].text,
            "当回事"
        );
        assert_eq!(
            engine.decode("zahuishi", AppScene::General, "").candidates[0].text,
            "咋回事"
        );
        assert_eq!(
            engine.decode("zenmehuishizheyang", AppScene::General, "").candidates[0].text,
            "怎么会是这样"
        );

        // 锁定「怎么」后，余下 `huishi` 候选首选必须是「回事」
        let mut zm_pins = HashMap::new();
        zm_pins.insert(0usize, (5usize, "怎么".to_string()));
        let zm_pinned = engine.decode_with_pins("zenmehuishi", AppScene::General, "", &zm_pins);
        assert_eq!(zm_pinned.candidates[0].text, "怎么回事");
        let hs_alts = engine.query_segment_alternatives_with_prev("zenmehuishi", 5, 8, Some("怎么"));
        assert_eq!(hs_alts[0].text, "回事");

        // 极端压力测试：即使个人词库中 `zenme -> 怎么` 和 `huishi -> 会是` 已累积满额词频与二元记忆，
        // 4 字整词 `zenmehuishi` 依然必须稳居第 1 位「怎么回事」，且绝不丢失候选！
        let mut polluted_engine = build_test_engine();
        for _ in 0..10 {
            polluted_engine.commit_candidate_ex("zenme", "怎么", None, false);
        }
        for _ in 0..5 {
            polluted_engine.commit_candidate_ex("huishi", "会是", Some("怎么"), true);
        }
        let polluted_out = polluted_engine.decode("zenmehuishi", AppScene::General, "");
        let polluted_top5: Vec<String> = polluted_out
            .candidates
            .iter()
            .take(5)
            .map(|c| c.text.clone())
            .collect();
        println!("polluted zenmehuishi top5 = {:?}", polluted_top5);
        assert_eq!(polluted_top5[0], "怎么回事");
        assert!(polluted_top5.contains(&"怎么会是".to_string()));
    }

    /// 打印常用输入的前 6 个候选（回归抽查用）：`cargo test -p ime-core dump_top_candidates -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dump_top_candidates() {
        let engine = build_test_engine();
        for py in [
            "xiazaideanjian", "xiazai", "keyiyouhuadedif", "keyiyouhuadedf", "dfang", "wmen", "zh", "zho", "zhonggu", "zhongguo",
            "nihao", "women", "xian", "fangan", "shi", "de", "jintiantianqibucuo", "wozaiyongrustxieapi",
            "api", "bug", "haode", "xiexie", "shima", "zenmeyang", "keyi", "nihaoa", "wobuzhidao",
        ] {
            let out = engine.decode(py, AppScene::General, "");
            let top: Vec<String> = out
                .candidates
                .iter()
                .take(6)
                .map(|c| {
                    let syl: Vec<&str> = c.syllable_spans.iter().map(|s| s.code_key.as_str()).collect();
                    format!("{}[{}]{}", c.text, syl.join("'"), c.tag)
                })
                .collect();
            println!("{py:<28} {}", top.join("  "));
        }
    }
}
