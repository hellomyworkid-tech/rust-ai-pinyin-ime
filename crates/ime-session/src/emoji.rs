//! 候选里的表情：像系统键盘那样，在常用词后面跟上对应的表情（打 mao，「猫」后面是 🐈 🐱；打 ok，「ok」后面是 👌 🆗）。
//!
//! 词表是 `data/emoji/zh_emoji.tsv`：由 `tools/emoji/gen_emoji_table.swift` 从 Unicode CLDR 的中文表情注释生成
//! （Unicode License v3，见 `data/emoji/LICENSE-CLDR`），编进程序里，第一次用到时才解析成表（约 5,700 个词）。
//! 表里除了中文词，还有注释里本来就有的英文关键词（ok、lol、sos、cool……，小写）。
//!
//! 表情候选只在 AI 重排之后插入，不进打分池，也不进个人词库：选中它只上屏这个表情。

use ime_core::Candidate;
use std::collections::HashMap;
use std::sync::OnceLock;

/// 表情候选的标签
pub const EMOJI_TAG: &str = "[表情]";
/// 英文词库里的词的标签（ime_core 的解码器给纯英文候选打的标签）。英文只跟在这种候选后面：
/// 原样的字母串、中英混输不跟
const ENGLISH_TAG: &str = "[英文]";
/// 只看前几个词候选（第一页靠前的位置）
const SCAN_WORDS: usize = 6;
/// 每个词后面最多跟几个表情
const PER_WORD: usize = 2;
/// 一次最多插几个表情
const TOTAL: usize = 3;

static TABLE_TEXT: &str = include_str!("../../../data/emoji/zh_emoji.tsv");

fn table() -> &'static HashMap<&'static str, Vec<&'static str>> {
    static TABLE: OnceLock<HashMap<&'static str, Vec<&'static str>>> = OnceLock::new();
    TABLE.get_or_init(|| {
        TABLE_TEXT
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(|line| line.split_once('\t'))
            .map(|(word, emojis)| (word, emojis.split(' ').filter(|e| !e.is_empty()).collect()))
            .collect()
    })
}

/// 这个词对应的表情（最相关的在前），没有就是空
pub fn emoji_for(word: &str) -> &'static [&'static str] {
    table().get(word).map(Vec::as_slice).unwrap_or(&[])
}

/// 在覆盖全部输入的中文词候选（和英文词库里的英文词，按小写查）后面插入它对应的表情。只看前 [`SCAN_WORDS`] 个词，
/// 每个词最多 [`PER_WORD`] 个，一共最多 [`TOTAL`] 个，同一个表情只出现一次。首选永远是词，所以空格上屏不受影响
pub fn insert_emoji_candidates(candidates: &mut Vec<Candidate>, input_len: usize) {
    let mut inserted: Vec<&'static str> = Vec::new();
    let mut words_seen = 0;
    let mut i = 0;
    while i < candidates.len() && words_seen < SCAN_WORDS && inserted.len() < TOTAL {
        let candidate = &candidates[i];
        if candidate.tag == EMOJI_TAG {
            i += 1;
            continue;
        }
        words_seen += 1;
        let english = candidate.text.is_ascii();
        if candidate.consumed_len != input_len || (english && candidate.tag != ENGLISH_TAG) {
            i += 1;
            continue;
        }
        let word: String = if english {
            candidate.text.to_ascii_lowercase()
        } else {
            candidate.text.chars().filter(|c| !c.is_whitespace()).collect()
        };
        let preedit = candidate.preedit.clone();
        let room = TOTAL - inserted.len();
        let new: Vec<&'static str> = emoji_for(&word)
            .iter()
            .copied()
            .filter(|e| !inserted.contains(e))
            .take(PER_WORD.min(room))
            .collect();
        let mut at = i + 1;
        for emoji in new {
            candidates.insert(
                at,
                Candidate {
                    text: emoji.to_string(),
                    preedit: preedit.clone(),
                    consumed_len: input_len,
                    l1_cost: 0.0,
                    final_score: 0.0,
                    ai_boosted: false,
                    tag: EMOJI_TAG.to_string(),
                    segments: vec![emoji.to_string()],
                    segment_spans: Vec::new(),
                    syllable_spans: Vec::new(),
                },
            );
            inserted.push(emoji);
            at += 1;
        }
        i = at;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, consumed_len: usize) -> Candidate {
        Candidate {
            text: text.to_string(),
            preedit: "mao".to_string(),
            consumed_len,
            l1_cost: 0.0,
            final_score: 0.0,
            ai_boosted: false,
            tag: String::new(),
            segments: vec![text.to_string()],
            segment_spans: Vec::new(),
            syllable_spans: Vec::new(),
        }
    }

    #[test]
    fn the_table_maps_common_words() {
        assert_eq!(emoji_for("猫").first(), Some(&"🐈"));
        assert!(emoji_for("猫").contains(&"🐱"));
        assert_eq!(emoji_for("加油").first(), Some(&"💪"));
        assert!(emoji_for("没有这个词").is_empty());
        // 默认按文字显示的字符都补了 U+FE0F，按表情显示
        assert_eq!(emoji_for("爱").first(), Some(&"❤\u{FE0F}"));
    }

    #[test]
    fn emoji_follow_the_word_and_never_take_the_top() {
        let mut list = vec![word("猫", 3), word("毛", 3), word("冒", 3), word("m", 1)];
        insert_emoji_candidates(&mut list, 3);
        let texts: Vec<&str> = list.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts[0], "猫");
        assert_eq!(&texts[1..3], &["🐈", "🐱"]);
        assert!(list[1].tag == EMOJI_TAG && list[1].consumed_len == 3);
        assert_eq!(texts[3], "毛");
        // 一共最多 3 个；前缀候选（没覆盖全部输入）后面不插
        assert!(list.iter().filter(|c| c.tag == EMOJI_TAG).count() <= TOTAL);
        let prefix_pos = texts.iter().position(|t| *t == "m").unwrap();
        assert!(list.get(prefix_pos + 1).is_none_or(|c| c.tag != EMOJI_TAG));
    }

    fn english(text: &str, consumed_len: usize) -> Candidate {
        Candidate { tag: ENGLISH_TAG.to_string(), ..word(text, consumed_len) }
    }

    #[test]
    fn unknown_words_and_raw_letters_get_nothing() {
        let mut list = vec![english("cpu", 3), word("没有这个词", 3)];
        insert_emoji_candidates(&mut list, 3);
        assert!(list.iter().all(|c| c.tag != EMOJI_TAG));
        // 不是英文词库里的词（原样的字母串、中英混输）：就算正好是 ok 也不跟
        let mut raw = vec![word("ok", 2)];
        insert_emoji_candidates(&mut raw, 2);
        assert!(raw.iter().all(|c| c.tag != EMOJI_TAG));
    }

    #[test]
    fn english_words_get_emoji_case_insensitively() {
        assert_eq!(emoji_for("ok").first(), Some(&"👌"));
        assert!(emoji_for("ok").contains(&"🆗"));
        let mut list = vec![english("OK", 2), word("偶看", 2)];
        insert_emoji_candidates(&mut list, 2);
        let texts: Vec<&str> = list.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(&texts[..4], &["OK", "👌", "🆗", "偶看"]);
        let mut lol = vec![english("LOL", 3)];
        insert_emoji_candidates(&mut lol, 3);
        assert_eq!(lol[1].text, "😂");
    }
}
