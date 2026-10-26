//! `user_dict`: 带指数时间衰减（Time-Decay）的个人记忆与自造词动态词库
//!
//! 核心公式：
//! $$W_{\text{user}}(w, t) = W_{\text{user}}(w, t_0) \cdot e^{-\lambda (t - t_0)} + \Delta w$$
//! 既能保证刚选过的词或自造词在下次输入时“秒上首位”，又能让久远偶发的错选随时间自然衰减。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
}

impl UserDict {
    pub fn new() -> Self {
        Self {
            records: HashMap::new(),
            custom_words: HashMap::new(),
            user_bigrams: HashMap::new(),
            decay_lambda: 0.000_001_1, // 约 7 天半衰期
            next_seq: 1,
        }
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

    /// 记录一次用户选词上屏，实时提升该词及前驱 Bigram 的动态权重
    pub fn record_commit(&mut self, code_key: &str, text: &str, prev_word: Option<&str>) {
        self.record_commit_ex(code_key, text, prev_word, true);
    }

    /// 记录用户选词上屏（区分主动选非首选词/造词 `is_explicit_override=true` 与默认空格首选上屏）
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
        let normalized_key = Self::normalize_key(code_key);
        if normalized_key.is_empty() || text.trim().is_empty() {
            return;
        }
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
        });

        let elapsed = now_secs.saturating_sub(rec.last_updated_secs) as f32;
        let decayed = rec.dynamic_score * (-self.decay_lambda * elapsed).exp();
        // 主动选择非首选词或分段造词给予更高单次权重（+6.5），使常用/主动选定的字词句立即晋升首选
        let delta = if is_explicit_override { 6.5 } else { 2.8 };
        rec.dynamic_score = (decayed + delta).min(12.0);
        rec.last_updated_secs = now_secs;
        rec.commit_count += 1;
        rec.last_seq = cur_seq;

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

    /// 获取某词在当前时刻经过时间衰减后的动态奖励分（用于抵扣解码 cost）
    #[inline]
    pub fn user_boost(&self, code_key: &str, text: &str) -> f32 {
        if self.records.is_empty() {
            return 0.0;
        }
        self.user_boost_at(code_key, text, Self::now_secs())
    }

    pub fn user_boost_at(&self, code_key: &str, text: &str, now_secs: u64) -> f32 {
        if self.records.is_empty() {
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

    /// 原子写入：先写临时文件再重命名，避免进程被杀/断电时把词库写坏
    pub fn save_to_file(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref();
        let json = serde_json::to_string(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    pub fn load_from_file(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let dict: Self = serde_json::from_str(&content)?;
        Ok(dict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
