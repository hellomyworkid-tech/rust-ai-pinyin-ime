//! `pinyin_mask`: 端侧 LLM 拼音前缀树掩码约束解码器 (`PinyinLogitsMask`)
//!
//! 核心原理（参考 `xushengfeng/lime` 约束采样机制）：
//! 大语言模型自由生成时可能吐出不符合用户按键的汉字。
//! `PinyinLogitsMask` 预先建立 LLM Tokenizer 词表 `TokenId -> 读音序列` 的映射，
//! 在解码每一步根据用户剩余未匹配的拼音串 `remaining_pinyin` 生成二值掩码，
//! 将所有读音不匹配的 Token Logits 置为 $-\infty$，实现 100% 严格符合拼音约束的语义生成。

use ime_lexicon::CompiledLexicon;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VocabTokenEntry {
    pub token_id: u32,
    pub surface: String,
    /// 该 Token 对应的无分隔拼音串（或英文小写串），多音字可对应多条可能读音
    pub phonetic_keys: Vec<String>,
}

pub struct PinyinLogitsMask {
    pub vocab: Vec<VocabTokenEntry>,
}

impl PinyinLogitsMask {
    /// 从编译好的词库构建模拟或真实 Tokenizer 的拼音掩码映射表
    pub fn from_lexicon(lexicon: &CompiledLexicon) -> Self {
        let mut map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for entry in lexicon.entries() {
            let list = map.entry(entry.text().to_string()).or_default();
            let flat_key = entry.flat_key();
            if !list.iter().any(|k| k.as_str() == flat_key) {
                list.push(flat_key.into_owned());
            }
            let abbrev_key = entry.abbrev_key();
            if !abbrev_key.is_empty() && !list.contains(&abbrev_key) {
                list.push(abbrev_key);
            }
        }

        let mut vocab: Vec<VocabTokenEntry> = map
            .into_iter()
            .enumerate()
            .map(|(idx, (surface, phonetic_keys))| VocabTokenEntry {
                token_id: idx as u32,
                surface,
                phonetic_keys,
            })
            .collect();
        vocab.sort_by_key(|v| v.token_id);
        Self { vocab }
    }

    /// 对模型输出的 `logits` 向量施加拼音强约束掩码：
    /// 仅保留读音是 `remaining_pinyin` 前缀（或 `remaining_pinyin` 是其前缀）的合法 Token，
    /// 其余不匹配 Token 的 Logit 一律置为 `f32::NEG_INFINITY`。
    /// 返回被保留的有效 Token 数量。
    pub fn apply_mask(&self, remaining_pinyin: &str, logits: &mut [f32]) -> usize {
        let clean_rem = remaining_pinyin.replace('\'', "").to_ascii_lowercase();
        if clean_rem.is_empty() {
            return logits.len();
        }

        let mut allowed_count = 0usize;
        for (idx, token) in self.vocab.iter().enumerate() {
            if idx >= logits.len() {
                break;
            }
            let is_match = token.phonetic_keys.iter().any(|key| {
                clean_rem.starts_with(key.as_str()) || key.starts_with(clean_rem.as_str())
            });
            if is_match {
                allowed_count += 1;
            } else {
                logits[idx] = f32::NEG_INFINITY;
            }
        }
        allowed_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ime_lexicon::{DomainTag, MultiSourceBuilder};

    #[test]
    fn test_pinyin_logits_mask_filters_invalid_tokens() {
        let mut builder = MultiSourceBuilder::new();
        builder.load_rime_dict_str(
            "技术\tji shu\t1000\n级数\tji shu\t800\n北京\tbei jing\t1000\n",
            1.0,
            DomainTag::General,
        );
        let lex = builder.build();
        let mask = PinyinLogitsMask::from_lexicon(&lex);

        let mut logits = vec![1.0f32; mask.vocab.len()];
        let kept = mask.apply_mask("jishu", &mut logits);
        assert_eq!(kept, 2); // 仅保留“技术”和“级数”，“北京”被置为 -inf
        for (i, v) in mask.vocab.iter().enumerate() {
            if v.surface == "北京" {
                assert!(logits[i].is_infinite() && logits[i].is_sign_negative());
            } else {
                assert_eq!(logits[i], 1.0);
            }
        }
    }
}
