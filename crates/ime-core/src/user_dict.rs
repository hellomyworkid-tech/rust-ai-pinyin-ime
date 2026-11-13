//! `user_dict`: 带指数时间衰减（Time-Decay）的个人记忆与自造词动态词库
//!
//! 核心公式：
//! $$W_{\text{user}}(w, t) = W_{\text{user}}(w, t_0) \cdot e^{-\lambda (t - t_0)} + \Delta w$$
//! 既能保证刚选过的词或自造词在下次输入时“秒上首位”，又能让久远偶发的错选随时间自然衰减。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 文件的修改时间与长度。个人词库用它判断词库文件是否被别的进程改写过
/// （iOS 上每个宿主 App 各有一个键盘进程，它们共用同一个词库文件）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl FileStamp {
    fn from_metadata(meta: &std::fs::Metadata) -> Self {
        Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        }
    }

    /// `path` 现在的修改时间与长度；文件不存在时为 `Ok(None)`
    pub fn probe(path: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::metadata(path) {
            Ok(meta) => Ok(Some(Self::from_metadata(&meta))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// 临时文件名里的计数器：同一进程里的几个写入方（几个引擎、测试里的几个线程）也各用各的临时文件
static TMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `path` 旁边的临时文件名：`<文件名>.<进程号>.<计数>.tmp`
fn unique_tmp_path(path: &Path) -> PathBuf {
    let n = TMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().map(|s| s.to_os_string()).unwrap_or_default();
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// 原子写入：先写 `path` 旁边的临时文件再改名，进程被杀 / 断电时不会把原文件写坏。
/// 临时文件名对每次写入都是唯一的，几个进程同时写同一个文件也不会互相覆盖临时文件。
/// 返回写出的那份文件的修改时间与长度（改名之前从临时文件上取，所以一定是本次写出的这一份，
/// 即使改名之后别的进程立刻又换掉了它）
pub(crate) fn write_file_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<FileStamp> {
    let tmp = unique_tmp_path(path);
    let written = (|| {
        std::fs::write(&tmp, bytes)?;
        let stamp = FileStamp::from_metadata(&std::fs::metadata(&tmp)?);
        std::fs::rename(&tmp, path)?;
        Ok(stamp)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// 删掉 `path` 旁边遗留的临时文件（写到一半进程被杀时留下的），只删修改时间早于 `min_age` 之前的，
/// 免得碰到别的进程正在写的那一个。返回删掉的个数
pub(crate) fn remove_stale_tmp_files(path: &Path, min_age: Duration) -> usize {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return 0;
    };
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let prefix = format!("{name}.");
    let mut removed = 0;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        // 只认 `<文件名>.<数字>.<数字>.tmp`
        let is_ours = file_name
            .to_str()
            .and_then(|n| n.strip_prefix(&prefix))
            .and_then(|n| n.strip_suffix(".tmp"))
            .and_then(|middle| middle.split_once('.'))
            .is_some_and(|(pid, n)| {
                !pid.is_empty()
                    && !n.is_empty()
                    && pid.bytes().all(|b| b.is_ascii_digit())
                    && n.bytes().all(|b| b.is_ascii_digit())
            });
        if !is_ours {
            continue;
        }
        let old_enough = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= min_age);
        if old_enough && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserWordRecord {
    pub text: String,
    pub code_key: String,
    /// 动态累积权重
    pub dynamic_score: f32,
    /// 最近一次上屏的秒级时间戳
    pub last_updated_secs: u64,
    /// 累计上屏次数
    pub commit_count: u32,
    /// 单调递增使用序号（用于同秒内精确判断最近一次选词）
    #[serde(default)]
    pub last_seq: u64,
    /// 其中主动改选的次数：用户选的与输入法当时显示的不同（没要首选、点了别的候选；句中选定的词与整句里
    /// 那一段原来的字不同；含锁定词块的整句与锁定之前显示的整句不同——锁定之后又改了拼音、不知道原句时照旧算）。
    /// 句中逐段选定、但只是接受了整句里原来那几个字的，按空格上屏算，不计在内。
    /// 旧版本写出的词库没有这个字段，按 0 读入
    #[serde(default)]
    pub explicit_count: u32,
}

/// 用户在某段上文下主动改选的结果（上文相关的纠正记忆）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextChoice {
    pub text: String,
    /// 记录时的全局序号，越大越新（淘汰时先丢旧的）
    pub seq: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserDict {
    /// 键：`code_key \t text` -> 个人词频记录
    pub records: HashMap<String, UserWordRecord>,
    /// 键：`code_key` -> 用户自造词/常用词列表
    pub custom_words: HashMap<String, Vec<String>>,
    /// 键：`prev_word \t next_word` -> 个人二元搭配习惯加成
    pub user_bigrams: HashMap<String, f32>,
    /// 指数衰减率（默认半衰期约 7 天）
    pub decay_lambda: f32,
    /// 全局递增序号
    #[serde(default)]
    pub next_seq: u64,
    /// 键：`上文末尾 \t code_key` -> 用户在这段上文下主动改选的词。
    /// 与 `records` 的区别：只在同样的上文末尾 + 同样的编码下生效，不影响别的上文
    #[serde(default)]
    pub context_choices: HashMap<String, ContextChoice>,
}

impl UserDict {
    pub fn new() -> Self {
        Self {
            records: HashMap::new(),
            custom_words: HashMap::new(),
            user_bigrams: HashMap::new(),
            decay_lambda: 0.000_001_1, // 约 7 天半衰期
            next_seq: 1,
            context_choices: HashMap::new(),
        }
    }

    /// 上文相关纠正记忆取上文末尾几个字符作键
    pub const CONTEXT_CHOICE_TAIL_CHARS: usize = 3;
    /// 上文相关纠正记忆的条数上限，超出时丢掉较旧的一半
    pub const MAX_CONTEXT_CHOICES: usize = 4000;

    /// 上文相关纠正记忆的键：上文去掉末尾的空白与标点后，最后 3 个字符 + 规范化的编码。
    /// 上文不足 2 个字符时不够区分语境，返回 `None`
    pub fn context_choice_key(context: &str, code_key: &str) -> Option<String> {
        let trimmed = context.trim_end_matches(|c: char| {
            c.is_whitespace() || "，。！？、：；,.!?:;\"'“”‘’（）()".contains(c)
        });
        let chars: Vec<char> = trimmed.chars().collect();
        if chars.len() < 2 {
            return None;
        }
        let code = Self::normalize_key(code_key);
        if code.is_empty() {
            return None;
        }
        let tail: String = chars[chars.len().saturating_sub(Self::CONTEXT_CHOICE_TAIL_CHARS)..]
            .iter()
            .collect();
        Some(format!("{tail}\t{code}"))
    }

    /// 记住「在这段上文下输入这个编码时，用户没要首选、改选了 `text`」。同一上文再次改选会覆盖旧记录
    pub fn record_context_choice(&mut self, context: &str, code_key: &str, text: &str) {
        if Self::is_polluted_entry(code_key, text) {
            return;
        }
        let Some(key) = Self::context_choice_key(context, code_key) else {
            return;
        };
        self.next_seq = self.next_seq.wrapping_add(1);
        self.context_choices.insert(
            key,
            ContextChoice {
                text: text.to_string(),
                seq: self.next_seq,
            },
        );
        if self.context_choices.len() > Self::MAX_CONTEXT_CHOICES {
            let mut seqs: Vec<u64> = self.context_choices.values().map(|c| c.seq).collect();
            seqs.sort_unstable();
            let cutoff = seqs[seqs.len() / 2];
            self.context_choices.retain(|_, c| c.seq >= cutoff);
        }
    }

    /// 用户曾在这段上文下为这个编码改选的词
    pub fn context_choice(&self, context: &str, code_key: &str) -> Option<&str> {
        if self.context_choices.is_empty() {
            return None;
        }
        let key = Self::context_choice_key(context, code_key)?;
        self.context_choices.get(&key).map(|c| c.text.as_str())
    }

    /// 默认持久化路径（`%LOCALAPPDATA%\RustAiPinyinIME\user_dict.json`）
    pub fn default_storage_path() -> PathBuf {
        if let Some(local_app) = std::env::var_os("LOCALAPPDATA") {
            let dir = PathBuf::from(local_app).join("RustAiPinyinIME");
            let _ = std::fs::create_dir_all(&dir);
            return dir.join("user_dict.json");
        }
        PathBuf::from("user_dict.json")
    }

    /// 从指定路径加载用户记忆词库（文件不存在或损坏时返回空词库）
    pub fn load_or_new(path: &Path) -> Self {
        if path.exists() {
            if let Ok(dict) = Self::load_from_file(path) {
                return dict;
            }
        }
        Self::new()
    }

    /// 词库条目上限，超出时淘汰衰减后权重最低的记录
    pub const MAX_RECORDS: usize = 20_000;

    /// 淘汰衰减后权重最低的记录，防止词库无限膨胀
    pub fn prune(&mut self) {
        if self.records.len() <= Self::MAX_RECORDS {
            return;
        }
        let now = Self::now_secs();
        let lambda = self.decay_lambda;
        let mut scored: Vec<(String, f32)> = self
            .records
            .iter()
            .map(|(k, r)| {
                let s = r.dynamic_score
                    * (-lambda * (now.saturating_sub(r.last_updated_secs) as f32)).exp();
                (k.clone(), s)
            })
            .collect();
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let remove_n = self.records.len() - Self::MAX_RECORDS * 9 / 10;
        for (k, _) in scored.into_iter().take(remove_n) {
            if let Some(rec) = self.records.remove(&k) {
                if let Some(list) = self.custom_words.get_mut(&rec.code_key) {
                    list.retain(|w| *w != rec.text);
                }
            }
        }
        self.custom_words.retain(|_, v| !v.is_empty());
    }

    #[inline]
    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    #[inline]
    pub fn normalize_key(code_key: &str) -> String {
        code_key
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase()
    }

    /// 判断候选词条是否为未完成拼音兜底碎片（如 `"可以优化的地 f"`、`"切换的 w 时候"`、`"z 也不对"`）
    /// 或原样小写回显（如 `"p" -> "p"`），此类碎片绝不能写入或召回自个人记忆词库
    pub fn is_polluted_entry(code_key: &str, text: &str) -> bool {
        let norm = Self::normalize_key(code_key);
        let trimmed = text.trim();
        if norm.is_empty() || trimmed.is_empty() {
            return true;
        }
        // 原样纯小写字母（如敲 p 直接上屏 p）不应记入用户词频，防止压过正常汉字候选
        if trimmed.bytes().all(|b| b.is_ascii_lowercase()) && trimmed == norm {
            return true;
        }
        // 若包含中文（非 ASCII 字符），同时夹杂全小写 ASCII 字母片段（如「可以优化的地 f」「切换的 w 时候」），
        // 属于未完成音节或误触产生的兜底残片（合法中英词如「AI 选词」「UI 设计」「同步到 GitHub」均含大写字母）
        let has_non_ascii = trimmed.chars().any(|c| !c.is_ascii());
        if has_non_ascii {
            let bytes = trimmed.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i].is_ascii_alphabetic() {
                    let start = i;
                    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                    let token = &bytes[start..i];
                    if token.len() <= 2 && token.iter().all(|b| b.is_ascii_lowercase()) {
                        return true;
                    }
                } else {
                    i += 1;
                }
            }
        }
        false
    }

    /// 清理词库中历史遗留的残片/污染词条，返回清理的条目数
    pub fn sanitize(&mut self) -> usize {
        let before = self.records.len();
        self.records
            .retain(|_, rec| !Self::is_polluted_entry(&rec.code_key, &rec.text));
        for (key, words) in self.custom_words.iter_mut() {
            words.retain(|w| {
                !Self::is_polluted_entry(key, w)
                    && self.records.contains_key(&format!("{key}\t{w}"))
            });
        }
        self.custom_words.retain(|_, v| !v.is_empty());
        before.saturating_sub(self.records.len())
    }

    /// 记录一次用户选词上屏，实时提升该词及前驱 Bigram 的动态权重
    pub fn record_commit(&mut self, code_key: &str, text: &str, prev_word: Option<&str>) {
        self.record_commit_ex(code_key, text, prev_word, true);
    }

    /// 记录用户选词上屏（区分主动改选 `is_explicit_override=true`——选了与输入法显示的不同的词，
    /// 见 [`UserWordRecord::explicit_count`]——与空格首选上屏、只是接受了显示的结果）
    pub fn record_commit_ex(
        &mut self,
        code_key: &str,
        text: &str,
        prev_word: Option<&str>,
        is_explicit_override: bool,
    ) {
        self.record_commit_at_ex(
            code_key,
            text,
            prev_word,
            Self::now_secs(),
            is_explicit_override,
        );
    }

    pub fn record_commit_at(
        &mut self,
        code_key: &str,
        text: &str,
        prev_word: Option<&str>,
        now_secs: u64,
    ) {
        self.record_commit_at_ex(code_key, text, prev_word, now_secs, true);
    }

    pub fn record_commit_at_ex(
        &mut self,
        code_key: &str,
        text: &str,
        prev_word: Option<&str>,
        now_secs: u64,
        is_explicit_override: bool,
    ) {
        if Self::is_polluted_entry(code_key, text) {
            return;
        }
        let normalized_key = Self::normalize_key(code_key);
        self.next_seq = self.next_seq.wrapping_add(1);
        let cur_seq = self.next_seq;
        let map_key = format!("{normalized_key}\t{text}");

        let rec = self.records.entry(map_key).or_insert_with(|| UserWordRecord {
            text: text.to_string(),
            code_key: normalized_key.clone(),
            dynamic_score: 0.0,
            last_updated_secs: now_secs,
            commit_count: 0,
            last_seq: cur_seq,
            explicit_count: 0,
        });

        let elapsed = now_secs.saturating_sub(rec.last_updated_secs) as f32;
        let decayed = rec.dynamic_score * (-self.decay_lambda * elapsed).exp();
        // 主动改选（没要首选、或分段选出与原句不同的词）给予更高单次权重（+6.5），使用户改选的字词句立即晋升首选
        let delta = if is_explicit_override { 6.5 } else { 2.8 };
        rec.dynamic_score = (decayed + delta).min(12.0);
        rec.last_updated_secs = now_secs;
        rec.commit_count += 1;
        rec.last_seq = cur_seq;
        if is_explicit_override {
            rec.explicit_count = rec.explicit_count.saturating_add(1);
        }

        let list = self.custom_words.entry(normalized_key).or_default();
        list.retain(|w| w != text);
        list.insert(0, text.to_string());
        if list.len() > 16 {
            list.truncate(16);
        }

        if let Some(prev) = prev_word {
            self.record_bigram(prev, text);
        }
        self.prune();
    }

    /// 仅记录个人二元搭配习惯（用于句中单字改选：学习「上下文 + 字」而非全局提升该字）
    pub fn record_bigram(&mut self, prev_word: &str, next_word: &str) {
        if prev_word.is_empty() || next_word.is_empty() {
            return;
        }
        let bg_key = format!("{prev_word}\t{next_word}");
        let entry = self.user_bigrams.entry(bg_key).or_insert(0.0);
        *entry = (*entry + 1.6).min(4.0);
        if self.user_bigrams.len() > Self::MAX_RECORDS * 2 {
            // 极端情况下的简单保护：丢弃最弱的一半搭配
            let mut v: Vec<(String, f32)> = self.user_bigrams.drain().collect();
            v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            v.truncate(Self::MAX_RECORDS);
            self.user_bigrams = v.into_iter().collect();
        }
    }

    /// 从全拼编码与上屏中文词推导首字母简拼键（例如 `("woziji", "我自己") -> Some("wzj")`）
    pub fn derive_jianpin_key(code_key: &str, text: &str) -> Option<String> {
        let chars: Vec<char> = text.chars().collect();
        let char_count = chars.len();
        if !(2..=12).contains(&char_count) || !chars.iter().all(|c| !c.is_ascii()) {
            return None;
        }
        if code_key.contains('\'') || code_key.contains(' ') {
            let parts: Vec<&str> = code_key
                .split(|c: char| c == '\'' || c.is_whitespace())
                .filter(|s| !s.is_empty())
                .collect();
            if parts.len() == char_count {
                let jp: String = parts
                    .iter()
                    .filter_map(|s| s.chars().next())
                    .map(|c| c.to_ascii_lowercase())
                    .collect();
                if jp.len() == char_count && jp.bytes().all(|b| b.is_ascii_lowercase()) {
                    return Some(jp);
                }
            }
        }
        let norm = Self::normalize_key(code_key);
        if norm.len() < char_count || !norm.bytes().all(|b| b.is_ascii_lowercase()) {
            return None;
        }
        if norm.len() == char_count {
            return Some(norm);
        }
        let fuzzy = crate::pinyin::FuzzyPinyinConfig::default();
        let mut initials = Vec::with_capacity(char_count);
        if split_into_k_syllables(&norm, char_count, &fuzzy, false, &mut initials)
            || split_into_k_syllables(&norm, char_count, &fuzzy, true, &mut initials)
        {
            return String::from_utf8(initials).ok();
        }
        None
    }

    /// 根据简拼编码（如 `"wzj"`）检索用户曾经输入过的全拼词条（支持精确简拼匹配与 3+ 字母简拼前缀联想）。
    /// 即 [`Self::jianpin_matches`] 的结果，同一个词只留排在最前的那条
    pub fn top_jianpin_records(&self, jp_query: &str) -> Vec<UserWordRecord> {
        let mut seen = std::collections::HashSet::new();
        self.jianpin_matches(jp_query)
            .into_iter()
            .filter(|r| seen.insert(r.text.clone()))
            .collect()
    }

    /// 简拼编码 `jp_query` 匹配到的全部记录。同一个词的几条记录都在、不去重（如简拼 `wzj` 下空格上屏过的「文摘集」
    /// 与全拼 `wenzhaiji` 下主动改选出的「文摘集」，两条推导出的简拼都是 `wzj`）。
    ///
    /// 匹配：记录推导出的简拼（[`Self::derive_jianpin_key`]）与查询相同；或者查询至少 3 个字母，是它的前缀，
    /// 且至多比它短 2 个字母。查询不足 2 个字母、含数字，或者能切成完整音节（是全拼而不是简拼）时没有结果。
    /// 返回的 `dynamic_score` 已按时间衰减，前缀匹配再乘 0.85；`explicit_count` 等其余字段原样保留。
    /// 排序：精确匹配在前，其次按衰减后的记忆强度、上屏次数、最近使用从高到低
    pub fn jianpin_matches(&self, jp_query: &str) -> Vec<UserWordRecord> {
        let norm_jp = Self::normalize_key(jp_query);
        if norm_jp.len() < 2
            || !norm_jp.bytes().all(|b| b.is_ascii_lowercase())
            || can_segment_as_full_syllables(&norm_jp)
        {
            return Vec::new();
        }
        let now_secs = Self::now_secs();
        let mut matches: Vec<(f32, bool, UserWordRecord)> = Vec::new();
        for rec in self.records.values() {
            let Some(jp) = Self::derive_jianpin_key(&rec.code_key, &rec.text) else {
                continue;
            };
            let is_exact = jp == norm_jp;
            let is_prefix = !is_exact
                && norm_jp.len() >= 3
                && jp.starts_with(&norm_jp)
                && jp.len() <= norm_jp.len() + 2;
            if !is_exact && !is_prefix {
                continue;
            }
            let elapsed = now_secs.saturating_sub(rec.last_updated_secs) as f32;
            let mut decayed = rec.dynamic_score * (-self.decay_lambda * elapsed).exp();
            if !is_exact {
                decayed *= 0.85;
            }
            let mut cloned = rec.clone();
            cloned.dynamic_score = decayed;
            matches.push((decayed, is_exact, cloned));
        }
        matches.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal))
                .then_with(|| b.2.commit_count.cmp(&a.2.commit_count))
                .then_with(|| b.2.last_seq.cmp(&a.2.last_seq))
        });
        matches.into_iter().map(|(_, _, r)| r).collect()
    }

    /// 获取某编码下按用户记忆强度排序的所有历史候选记录（最高频/最近使用的排最前）
    pub fn top_user_records(&self, code_key: &str) -> Vec<UserWordRecord> {
        let normalized_key = Self::normalize_key(code_key);
        if normalized_key.is_empty() {
            return Vec::new();
        }
        let now_secs = Self::now_secs();
        let Some(words) = self.custom_words.get(&normalized_key) else {
            return Vec::new();
        };
        let mut out: Vec<UserWordRecord> = words
            .iter()
            .filter_map(|w| {
                if Self::is_polluted_entry(&normalized_key, w) {
                    return None;
                }
                let map_key = format!("{normalized_key}\t{w}");
                self.records.get(&map_key).cloned()
            })
            .collect();
        out.sort_by(|a, b| {
            let sa = a.dynamic_score * (-self.decay_lambda * (now_secs.saturating_sub(a.last_updated_secs) as f32)).exp();
            let sb = b.dynamic_score * (-self.decay_lambda * (now_secs.saturating_sub(b.last_updated_secs) as f32)).exp();
            sb.partial_cmp(&sa)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.commit_count.cmp(&a.commit_count))
                .then_with(|| b.last_seq.cmp(&a.last_seq))
        });
        out
    }

    /// 这个词在这个编码下被主动改选过几次（见 [`UserWordRecord::explicit_count`]）
    pub fn explicit_choice_count(&self, code_key: &str, text: &str) -> u32 {
        if self.records.is_empty() {
            return 0;
        }
        let normalized_key = Self::normalize_key(code_key);
        self.records
            .get(&format!("{normalized_key}\t{text}"))
            .map_or(0, |r| r.explicit_count)
    }

    /// 获取某词在当前时刻经过时间衰减后的动态奖励分（用于抵扣解码 cost）
    #[inline]
    pub fn user_boost(&self, code_key: &str, text: &str) -> f32 {
        if self.records.is_empty() {
            return 0.0;
        }
        self.user_boost_at(code_key, text, Self::now_secs())
    }

    pub fn user_boost_at(&self, code_key: &str, text: &str, now_secs: u64) -> f32 {
        if self.records.is_empty() || Self::is_polluted_entry(code_key, text) {
            return 0.0;
        }
        let normalized_key = Self::normalize_key(code_key);
        let map_key = format!("{normalized_key}\t{text}");
        if let Some(rec) = self.records.get(&map_key) {
            let elapsed = now_secs.saturating_sub(rec.last_updated_secs) as f32;
            rec.dynamic_score * (-self.decay_lambda * elapsed).exp()
        } else {
            0.0
        }
    }

    /// 获取个人二元搭配习惯奖励分
    #[inline]
    pub fn bigram_boost(&self, prev_word: &str, next_word: &str) -> f32 {
        if self.user_bigrams.is_empty() || prev_word.is_empty() {
            return 0.0;
        }
        let bg_key = format!("{prev_word}\t{next_word}");
        self.user_bigrams.get(&bg_key).copied().unwrap_or(0.0)
    }

    /// 原子写入：先写临时文件再重命名，避免进程被杀/断电时把词库写坏。
    /// 临时文件名带进程号和计数器（见 [`write_file_atomically`]），几个进程同时保存也不会互相覆盖临时文件
    pub fn save_to_file(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        write_file_atomically(path.as_ref(), &self.to_json_bytes()?).map(|_| ())
    }

    /// 写盘用的 JSON（与 [`Self::save_to_file`] 写出的内容相同）
    pub(crate) fn to_json_bytes(&self) -> std::io::Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    /// 读入词库并清理历史遗留的残片条目，不写回文件。第二个返回值表示是否清掉了条目
    pub(crate) fn read_from_file(path: &Path) -> std::io::Result<(Self, bool)> {
        let content = std::fs::read_to_string(path)?;
        let mut dict: Self = serde_json::from_str(&content)?;
        let cleaned = dict.sanitize() > 0;
        Ok((dict, cleaned))
    }

    pub fn load_from_file(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref();
        let (dict, cleaned) = Self::read_from_file(path)?;
        if cleaned {
            let _ = dict.save_to_file(path);
        }
        Ok(dict)
    }
}

fn can_segment_as_full_syllables(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    let max_len = s.len().min(6);
    for len in (2..=max_len).rev() {
        if crate::pinyin::is_valid_syllable(&s[..len]) && can_segment_as_full_syllables(&s[len..]) {
            return true;
        }
    }
    false
}

fn split_into_k_syllables(
    rem: &str,
    k: usize,
    fuzzy: &crate::pinyin::FuzzyPinyinConfig,
    allow_single_consonant: bool,
    out_initials: &mut Vec<u8>,
) -> bool {
    if k == 0 {
        return rem.is_empty();
    }
    if rem.len() < k || rem.len() > k * 6 {
        return false;
    }
    let max_len = (rem.len() - (k - 1)).min(6);
    for len in (1..=max_len).rev() {
        let piece = &rem[..len];
        let is_valid = crate::pinyin::is_valid_syllable(piece)
            || (len >= 2 && !fuzzy.expand_fuzzy(piece).is_empty())
            || (allow_single_consonant && len == 1);
        if !is_valid {
            continue;
        }
        out_initials.push(piece.as_bytes()[0]);
        if split_into_k_syllables(&rem[len..], k - 1, fuzzy, allow_single_consonant, out_initials) {
            return true;
        }
        out_initials.pop();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_explicit_choices_are_counted_separately() {
        let mut udict = UserDict::new();
        let t0 = 1_700_000_000;
        // 空格接受首选：只记上屏次数
        udict.record_commit_at_ex("shiyan", "实验", None, t0, false);
        udict.record_commit_at_ex("shiyan", "实验", None, t0 + 1, false);
        assert_eq!(udict.explicit_choice_count("shiyan", "实验"), 0);
        // 主动改选
        udict.record_commit_at_ex("shiyan", "誓言", None, t0 + 2, true);
        udict.record_commit_at_ex("shi'yan", "誓言", None, t0 + 3, true);
        assert_eq!(udict.explicit_choice_count("shiyan", "誓言"), 2, "编码按规范化之后的键计");
        assert_eq!(udict.records["shiyan\t誓言"].commit_count, 2);
        assert_eq!(udict.explicit_choice_count("shiyan", "食盐"), 0);
        assert_eq!(UserDict::new().explicit_choice_count("shiyan", "誓言"), 0);

        // 旧版本的词库文件没有 explicit_count：按 0 读入，其余照旧
        let json = serde_json::to_string(&udict).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        for record in value["records"].as_object_mut().unwrap().values_mut() {
            record.as_object_mut().unwrap().remove("explicit_count");
        }
        let old: UserDict = serde_json::from_value(value).unwrap();
        assert_eq!(old.explicit_choice_count("shiyan", "誓言"), 0);
        assert_eq!(old.records["shiyan\t誓言"].commit_count, 2);
        let reloaded: UserDict = serde_json::from_str(&json).unwrap();
        assert_eq!(reloaded.explicit_choice_count("shiyan", "誓言"), 2);
    }

    /// 每个测试一个独立的临时目录，结束时删掉
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("ime-core-{tag}-{}", std::process::id()));
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

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn test_each_save_uses_its_own_temporary_file() {
        let a = unique_tmp_path(Path::new("/some/dir/user_dict.json"));
        let b = unique_tmp_path(Path::new("/some/dir/user_dict.json"));
        assert_ne!(a, b);
        assert_eq!(a.parent(), Some(Path::new("/some/dir")));
        let name = a.file_name().unwrap().to_str().unwrap();
        let pid = std::process::id().to_string();
        assert!(name.starts_with(&format!("user_dict.json.{pid}.")), "{name}");
        assert!(name.ends_with(".tmp"), "{name}");

        // 存完之后目录里只有词库文件本身；返回的修改时间与长度就是文件现在的样子
        let dir = TempDir::new("tmpname");
        let path = dir.0.join("user_dict.json");
        let mut udict = UserDict::new();
        udict.record_commit_at("shiyan", "誓言", None, 1_700_000_000);
        let stamp = write_file_atomically(&path, &udict.to_json_bytes().unwrap()).unwrap();
        assert_eq!(FileStamp::probe(&path).unwrap(), Some(stamp));
        assert_eq!(file_names(&dir.0), ["user_dict.json"]);
        assert_eq!(FileStamp::probe(&dir.0.join("missing.json")).unwrap(), None);

        // 写不进去（上级目录不存在）时报错，也不留下临时文件
        let nowhere = dir.0.join("no-such-dir").join("user_dict.json");
        assert!(udict.save_to_file(&nowhere).is_err());
        assert_eq!(file_names(&dir.0), ["user_dict.json"]);
    }

    #[test]
    fn test_concurrent_saves_to_one_file_never_fail_or_corrupt_it() {
        // 以前所有写入方共用 user_dict.json.tmp：一方改名之后另一方的改名就找不到文件，
        // 或者把对方写了一半的内容改名成正式文件
        let dir = TempDir::new("concurrent");
        let path = dir.0.join("user_dict.json");
        let writers: Vec<_> = (0..8usize)
            .map(|w| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut udict = UserDict::new();
                    // 各个写入方的内容长短不一
                    for i in 0..(w + 1) * 40 {
                        udict.record_commit_at(&format!("ci{w}x{i}"), "词", None, 1_700_000_000);
                    }
                    for _ in 0..40 {
                        udict.save_to_file(&path).expect("save");
                        let loaded = UserDict::load_from_file(&path).expect("file is always complete");
                        assert!(loaded.records.len() % 40 == 0 && !loaded.records.is_empty());
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("writer thread");
        }
        assert_eq!(file_names(&dir.0), ["user_dict.json"]);
    }

    #[test]
    fn test_stale_temporary_files_are_removed_but_fresh_and_foreign_ones_are_kept() {
        let dir = TempDir::new("stale");
        let path = dir.0.join("user_dict.json");
        for name in [
            "user_dict.json",
            "user_dict.json.123.4.tmp",
            "user_dict.json.tmp",
            "user_dict.json.abc.1.tmp",
            "other.json.123.4.tmp",
        ] {
            std::fs::write(dir.0.join(name), b"{}").unwrap();
        }
        // 刚写的不删：可能是别的进程正在写的
        assert_eq!(remove_stale_tmp_files(&path, Duration::from_secs(600)), 0);
        assert_eq!(file_names(&dir.0).len(), 5);
        // 够旧的只删符合 `<文件名>.<进程号>.<计数>.tmp` 的
        assert_eq!(remove_stale_tmp_files(&path, Duration::ZERO), 1);
        assert_eq!(
            file_names(&dir.0),
            ["other.json.123.4.tmp", "user_dict.json", "user_dict.json.abc.1.tmp", "user_dict.json.tmp"]
        );
    }

    #[test]
    fn test_context_choice_key_uses_the_last_three_characters() {
        // 末尾 3 个字符 + 规范化后的编码
        assert_eq!(
            UserDict::context_choice_key("野外露营要先", "shenghuo").as_deref(),
            Some("营要先\tshenghuo")
        );
        // 上文只有 2 个字符也可以；1 个或没有就不够区分语境
        assert_eq!(UserDict::context_choice_key("各位", "tongzhi").as_deref(), Some("各位\ttongzhi"));
        assert_eq!(UserDict::context_choice_key("我", "shiyan"), None);
        assert_eq!(UserDict::context_choice_key("", "shiyan"), None);
        // 末尾的空白与标点不算
        assert_eq!(
            UserDict::context_choice_key("理想很美好， ", "xianshi"),
            UserDict::context_choice_key("理想很美好", "xianshi")
        );
        assert_eq!(UserDict::context_choice_key("，。", "xianshi"), None);
        // 编码按个人词库的规则规范化（大小写、分隔符）
        assert_eq!(
            UserDict::context_choice_key("本周的", "Gong'Shi"),
            UserDict::context_choice_key("本周的", "gongshi")
        );
        assert_eq!(UserDict::context_choice_key("本周的", ""), None);
    }

    #[test]
    fn test_context_choice_is_scoped_to_context_and_code() {
        let mut udict = UserDict::new();
        assert_eq!(udict.context_choice("野外露营要先", "shenghuo"), None);
        udict.record_context_choice("野外露营要先", "shenghuo", "生火");
        assert_eq!(udict.context_choice("野外露营要先", "shenghuo"), Some("生火"));
        // 末尾 3 个字符相同就算同样的上文
        assert_eq!(udict.context_choice("明天去露营要先", "shenghuo"), Some("生火"));
        // 换了上文、换了编码都不生效
        assert_eq!(udict.context_choice("这就是我想要的", "shenghuo"), None);
        assert_eq!(udict.context_choice("野外露营要先", "shenghuo2"), None);
        // 不影响上文无关的个人词频
        assert_eq!(udict.user_boost("shenghuo", "生火"), 0.0);
        assert!(udict.records.is_empty());
        // 同一上文再次改选：覆盖
        udict.record_context_choice("野外露营要先", "shenghuo", "生活");
        assert_eq!(udict.context_choice("野外露营要先", "shenghuo"), Some("生活"));
        assert_eq!(udict.context_choices.len(), 1);
        // 上文太短：不记
        udict.record_context_choice("我", "shiyan", "誓言");
        assert_eq!(udict.context_choices.len(), 1);
    }

    #[test]
    fn test_context_choices_are_bounded_and_keep_the_newest() {
        // 每条的上文末尾 3 个字符都不同（把序号写成三位「百进制」汉字）
        let context = |i: usize| -> String {
            [i / 10_000 % 100, i / 100 % 100, i % 100]
                .iter()
                .map(|d| char::from_u32(0x4E00 + *d as u32).unwrap())
                .collect()
        };
        let mut udict = UserDict::new();
        let total = UserDict::MAX_CONTEXT_CHOICES + 10;
        for i in 0..total {
            udict.record_context_choice(&context(i), "shiyan", "誓言");
        }
        assert!(udict.context_choices.len() <= UserDict::MAX_CONTEXT_CHOICES);
        assert!(udict.context_choices.len() >= UserDict::MAX_CONTEXT_CHOICES / 2);
        // 最新的还在，最旧的被丢掉
        assert_eq!(udict.context_choice(&context(total - 1), "shiyan"), Some("誓言"));
        assert_eq!(udict.context_choice(&context(0), "shiyan"), None);
    }

    #[test]
    fn test_context_choices_survive_a_json_round_trip_and_old_files_still_load() {
        let mut udict = UserDict::new();
        udict.record_commit_at("shiyan", "实验", None, 1_700_000_000);
        udict.record_context_choice("我对天发誓这是我的", "shiyan", "誓言");
        let json = serde_json::to_string(&udict).unwrap();
        let loaded: UserDict = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.context_choice("我对天发誓这是我的", "shiyan"), Some("誓言"));
        assert_eq!(loaded.records.len(), 1);

        // 旧版本写出的文件里没有 context_choices 字段
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value.as_object_mut().unwrap().remove("context_choices");
        let old: UserDict = serde_json::from_value(value).unwrap();
        assert!(old.context_choices.is_empty());
        assert_eq!(old.records.len(), 1);
    }

    #[test]
    fn test_jianpin_matches_keep_every_record_of_a_word() {
        let mut udict = UserDict::new();
        // 全拼下主动改选一次，简拼下空格上屏三次：两条记录推导出的简拼都是 wzj
        udict.record_commit_ex("wenzhaiji", "文摘集", None, true);
        for _ in 0..3 {
            udict.record_commit_ex("wzj", "文摘集", None, false);
        }
        // 简拼 wzjh 以查询 wzj 开头：前缀匹配
        udict.record_commit_ex("wenzhaijihe", "文摘集合", None, false);
        // 推导不出 wzj 的不算
        udict.record_commit_ex("shiyan", "实验", None, false);

        let found: Vec<(String, String, f32, u32)> = udict
            .jianpin_matches("wzj")
            .into_iter()
            .map(|r| (r.code_key, r.text, r.dynamic_score, r.explicit_count))
            .collect();
        let keys: Vec<(&str, &str)> = found.iter().map(|(c, t, _, _)| (c.as_str(), t.as_str())).collect();
        assert_eq!(
            keys,
            [("wzj", "文摘集"), ("wenzhaiji", "文摘集"), ("wenzhaijihe", "文摘集合")],
            "同一个词的两条都在；精确匹配在前，再按记忆强度"
        );
        let close = |a: f32, b: f32| (a - b).abs() < 0.01;
        assert!(close(found[0].2, 8.4) && found[0].3 == 0, "{found:?}");
        assert!(close(found[1].2, 6.5) && found[1].3 == 1, "主动改选次数原样保留: {found:?}");
        assert!(close(found[2].2, 2.8 * 0.85), "前缀匹配乘 0.85: {found:?}");

        // top_jianpin_records 是同一份结果，每个词只留排在最前的那条
        let top: Vec<(String, String)> = udict
            .top_jianpin_records("wzj")
            .into_iter()
            .map(|r| (r.code_key, r.text))
            .collect();
        assert_eq!(
            top,
            [("wzj".to_string(), "文摘集".to_string()), ("wenzhaijihe".to_string(), "文摘集合".to_string())]
        );

        // 全拼查询、太短的查询：没有结果
        assert!(udict.jianpin_matches("wenzhaiji").is_empty());
        assert!(udict.jianpin_matches("w").is_empty());
    }

    #[test]
    fn test_time_decay_user_dict() {
        let mut udict = UserDict::new();
        let t0 = 1_700_000_000;
        udict.record_commit_at("jishu", "级数", Some("傅里叶"), t0);

        let boost_immediate = udict.user_boost_at("jishu", "级数", t0);
        assert!(boost_immediate >= 2.5);

        // 30 天后权重应显著衰减
        let t_later = t0 + 30 * 86400;
        let boost_later = udict.user_boost_at("jishu", "级数", t_later);
        assert!(boost_later < boost_immediate * 0.15);
    }
}
