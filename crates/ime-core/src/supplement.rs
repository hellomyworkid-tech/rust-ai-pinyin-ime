//! `supplement`: 现代高频口语、金融对公/私行、育儿日常与社会高频构词补充词表
//!
//! 背景：基础开源词库（`rime-ice`）主要源自书面语料，在部分高频日常口语（如「咋呀」「接娃」「遛娃」）、
//! 金融实务简称（如「公户」「私户」「私行」「私行客户」）及现代高频复合词（如「富人区」「公家饭」「纯躺平」）
//! 上存在词条空白。本模块在进程内构建零拷贝紧凑静态 `FlatTrie` 索引（毫秒内初始化一次），
//! 在不改动远端发布词库镜像（保持 SHA-256 完全兼容）的前提下，补齐词组检索、简拼与末尾半音节联想。

use crate::lattice::{EdgeKind, LatticeEdge};
use crate::pinyin::{is_unapostrophed_single_syllable, SyllableSpan};
use ime_lexicon::{DomainTag, FlatTrie};
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy)]
pub struct SupplementEntry {
    pub syllable_key: &'static str,
    pub text: &'static str,
    pub cost: f32,
    pub domain: DomainTag,
}

/// 现代高频口语、金融、育儿、职场及常见熟语补充表（`AIMELX03` 代价标尺：`10.0 ~ 12.2`）
pub static SUPPLEMENT_ENTRIES: &[SupplementEntry] = &[
    // 1. 金融财税 / 银行对公与私行实务
    SupplementEntry { syllable_key: "gong'hu", text: "公户", cost: 10.75, domain: DomainTag::General },
    SupplementEntry { syllable_key: "si'hu", text: "私户", cost: 10.95, domain: DomainTag::General },
    SupplementEntry { syllable_key: "dui'gong'hu", text: "对公户", cost: 11.05, domain: DomainTag::General },
    SupplementEntry { syllable_key: "dui'si'hu", text: "对私户", cost: 11.35, domain: DomainTag::General },
    SupplementEntry { syllable_key: "dui'gong'zhang'hu", text: "对公账户", cost: 10.65, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ji'ben'hu", text: "基本户", cost: 11.25, domain: DomainTag::General },
    SupplementEntry { syllable_key: "yi'ban'hu", text: "一般户", cost: 11.45, domain: DomainTag::General },
    SupplementEntry { syllable_key: "kai'gong'hu", text: "开公户", cost: 11.35, domain: DomainTag::General },
    SupplementEntry { syllable_key: "zou'gong'hu", text: "走公户", cost: 11.35, domain: DomainTag::General },
    SupplementEntry { syllable_key: "zhuan'gong'hu", text: "转公户", cost: 11.45, domain: DomainTag::General },
    SupplementEntry { syllable_key: "si'hang", text: "私行", cost: 11.05, domain: DomainTag::General },
    SupplementEntry { syllable_key: "si'hang'ke'hu", text: "私行客户", cost: 10.65, domain: DomainTag::General },
    SupplementEntry { syllable_key: "si'ren'yin'hang", text: "私人银行", cost: 10.65, domain: DomainTag::General },
    SupplementEntry { syllable_key: "cheng'shang'hang", text: "城商行", cost: 11.15, domain: DomainTag::General },
    SupplementEntry { syllable_key: "nong'shang'hang", text: "农商行", cost: 11.15, domain: DomainTag::General },
    SupplementEntry { syllable_key: "guo'you'hang", text: "国有行", cost: 11.25, domain: DomainTag::General },
    SupplementEntry { syllable_key: "gu'fen'hang", text: "股份行", cost: 11.25, domain: DomainTag::General },
    SupplementEntry { syllable_key: "san'qi'fen'zhang", text: "三七分账", cost: 10.95, domain: DomainTag::General },
    SupplementEntry { syllable_key: "san'qi'fen'cheng", text: "三七分成", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "wu'wu'fen'zhang", text: "五五分账", cost: 11.15, domain: DomainTag::General },
    SupplementEntry { syllable_key: "si'liu'fen'zhang", text: "四六分账", cost: 11.25, domain: DomainTag::General },
    SupplementEntry { syllable_key: "er'ba'fen'zhang", text: "二八分账", cost: 11.25, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ru'shu'feng'huan", text: "如数奉还", cost: 10.75, domain: DomainTag::General },

    // 2. 家庭育儿与生活日常（`-娃` 系列等）
    SupplementEntry { syllable_key: "jie'wa", text: "接娃", cost: 10.85, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "song'wa", text: "送娃", cost: 11.05, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "dai'wa", text: "带娃", cost: 10.65, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "liu'wa", text: "遛娃", cost: 10.85, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "shai'wa", text: "晒娃", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "ji'wa", text: "鸡娃", cost: 11.05, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "hong'wa", text: "哄娃", cost: 11.05, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "sheng'wa", text: "生娃", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "yang'wa", text: "养娃", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "kan'wa", text: "看娃", cost: 11.15, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "bao'wa", text: "抱娃", cost: 11.25, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "pei'wa", text: "陪娃", cost: 11.15, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "meng'wa", text: "萌娃", cost: 11.05, domain: DomainTag::Chat },

    // 3. 高频日常口语与语气助词短语
    SupplementEntry { syllable_key: "za'ya", text: "咋呀", cost: 10.85, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "za'la", text: "咋啦", cost: 10.65, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "za'le", text: "咋了", cost: 10.55, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "za'zheng", text: "咋整", cost: 10.85, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "za'ban", text: "咋办", cost: 10.75, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "za'shuo", text: "咋说", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "za'xiang", text: "咋想", cost: 11.05, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "za'nong", text: "咋弄", cost: 11.05, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "za'gao", text: "咋搞", cost: 11.05, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "hao'da", text: "好哒", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "shi'da", text: "是哒", cost: 11.25, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "hao'ya", text: "好呀", cost: 10.65, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "dui'ya", text: "对呀", cost: 10.75, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "xing'ya", text: "行呀", cost: 10.85, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "shi'ya", text: "是呀", cost: 10.85, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "lai'ya", text: "来呀", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "qu'ya", text: "去呀", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "zou'ya", text: "走呀", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "gan'ma'ya", text: "干嘛呀", cost: 10.85, domain: DomainTag::Chat },

    // 4. 区域场所、职场生态与现代社会常用词
    SupplementEntry { syllable_key: "fu'ren'qu", text: "富人区", cost: 10.65, domain: DomainTag::General },
    SupplementEntry { syllable_key: "qiong'ren'qu", text: "穷人区", cost: 11.25, domain: DomainTag::General },
    SupplementEntry { syllable_key: "hua'ren'qu", text: "华人区", cost: 10.95, domain: DomainTag::General },
    SupplementEntry { syllable_key: "wu'ren'qu", text: "无人区", cost: 10.65, domain: DomainTag::General },
    SupplementEntry { syllable_key: "bie'shu'qu", text: "别墅区", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ping'lun'qu", text: "评论区", cost: 10.45, domain: DomainTag::General },
    SupplementEntry { syllable_key: "huan'chong'qu", text: "缓冲区", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "shu'shi'qu", text: "舒适区", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "shen'shui'qu", text: "深水区", cost: 11.05, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ban'gong'qu", text: "办公区", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ju'min'qu", text: "居民区", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "shang'ye'qu", text: "商业区", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "he'xin'qu", text: "核心区", cost: 10.95, domain: DomainTag::General },
    SupplementEntry { syllable_key: "fu'wu'qu", text: "服务区", cost: 10.75, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ge'li'qu", text: "隔离区", cost: 11.05, domain: DomainTag::General },
    SupplementEntry { syllable_key: "gong'jia'fan", text: "公家饭", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "chi'gong'jia'fan", text: "吃公家饭", cost: 10.65, domain: DomainTag::General },
    SupplementEntry { syllable_key: "chi'gong'jia'fan'de", text: "吃公家饭的", cost: 10.55, domain: DomainTag::General },
    SupplementEntry { syllable_key: "tie'fan'wan", text: "铁饭碗", cost: 10.75, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ti'zhi'nei", text: "体制内", cost: 10.65, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ti'zhi'wai", text: "体制外", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "chun'tang'ping", text: "纯躺平", cost: 10.75, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "ban'tang'ping", text: "半躺平", cost: 11.05, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "song'chi'gan", text: "松弛感", cost: 10.75, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "fen'wei'gan", text: "氛围感", cost: 10.65, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "bian'jie'gan", text: "边界感", cost: 10.75, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "po'sui'gan", text: "破碎感", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "gao'ji'gan", text: "高级感", cost: 10.75, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "ti'yan'gan", text: "体验感", cost: 10.85, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "yi'shi'gan", text: "仪式感", cost: 10.65, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "an'quan'gan", text: "安全感", cost: 10.45, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "cun'zai'gan", text: "存在感", cost: 10.55, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "qing'xu'jia'zhi", text: "情绪价值", cost: 10.55, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "jing'shen'nei'hao", text: "精神内耗", cost: 10.65, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "cao'tai'ban'zi", text: "草台班子", cost: 10.85, domain: DomainTag::Chat },
];

struct SupplementIndex {
    syllable_trie: FlatTrie,
    abbrev_trie: FlatTrie,
}

static SUPPLEMENT_INDEX: LazyLock<SupplementIndex> = LazyLock::new(|| {
    let mut order: Vec<usize> = (0..SUPPLEMENT_ENTRIES.len()).collect();
    order.sort_by(|&a, &b| {
        SUPPLEMENT_ENTRIES[a]
            .cost
            .total_cmp(&SUPPLEMENT_ENTRIES[b].cost)
            .then_with(|| SUPPLEMENT_ENTRIES[a].text.cmp(SUPPLEMENT_ENTRIES[b].text))
    });
    let mut syllable_trie = FlatTrie::new();
    let mut abbrev_trie = FlatTrie::new();
    for idx in order {
        let entry = &SUPPLEMENT_ENTRIES[idx];
        syllable_trie.insert_with_limit(entry.syllable_key, idx as u32, true, 6, 16);
        let abbrev: String = entry
            .syllable_key
            .split('\'')
            .filter_map(|s| s.chars().next())
            .collect();
        if abbrev.len() >= 2 {
            abbrev_trie.insert_with_limit(&abbrev, idx as u32, false, 0, 16);
        }
    }
    SupplementIndex {
        syllable_trie,
        abbrev_trie,
    }
});

/// 收集补充词表的全拼/模糊音及末尾不完整音节边
pub fn collect_supplement_edges(
    origin_start: usize,
    input: &str,
    spans_at: &[Vec<&SyllableSpan>],
    out_edges: &mut Vec<LatticeEdge>,
) {
    collect_dfs(
        origin_start,
        origin_start,
        0,
        0,
        0.0,
        input,
        spans_at,
        out_edges,
    );
}

#[allow(clippy::too_many_arguments)]
fn collect_dfs(
    origin_start: usize,
    pos: usize,
    curr_node: u32,
    depth: usize,
    accum_fuzzy: f32,
    input: &str,
    spans_at: &[Vec<&SyllableSpan>],
    out_edges: &mut Vec<LatticeEdge>,
) {
    let n = input.len();
    if depth >= 6 || pos >= n {
        return;
    }
    let mut effective_pos = pos;
    if depth > 0 {
        while effective_pos < n && input.as_bytes()[effective_pos] == b'\'' {
            effective_pos += 1;
        }
        if effective_pos >= n {
            return;
        }
    }
    let trie = &SUPPLEMENT_INDEX.syllable_trie;
    let base_node = if depth == 0 {
        curr_node
    } else {
        match trie.advance_node(curr_node, "'") {
            Some(nd) => nd,
            None => return,
        }
    };
    let syl_count = depth + 1;
    for &sp in &spans_at[effective_pos] {
        let new_fuzzy = accum_fuzzy + sp.fuzzy_penalty;
        if new_fuzzy > 3.6 {
            continue;
        }
        let Some(next_node) = trie.advance_node(base_node, &sp.syllable) else {
            continue;
        };
        let hit_ids = &trie.nodes[next_node as usize].exact_entries;
        if !hit_ids.is_empty() {
            let raw_span = &input[origin_start..sp.end];
            let splits_single_syl = syl_count >= 2 && is_unapostrophed_single_syllable(raw_span);
            if !(splits_single_syl && new_fuzzy > 0.0) {
                let split_pen = if splits_single_syl { 0.70 } else { 0.0 };
                let fuzzy_pen = if new_fuzzy > 0.0 { new_fuzzy * 1.10 } else { 0.0 };
                for &id in hit_ids {
                    let entry = &SUPPLEMENT_ENTRIES[id as usize];
                    let target_cost = entry.cost + fuzzy_pen + split_pen;
                    if let Some(existing) = out_edges.iter_mut().find(|e| {
                        e.start == origin_start
                            && e.end == sp.end
                            && e.text == entry.text
                            && e.is_fuzzy == (new_fuzzy > 0.0)
                    }) {
                        if target_cost < existing.base_cost {
                            existing.base_cost = target_cost;
                        }
                        continue;
                    }
                    out_edges.push(LatticeEdge {
                        start: origin_start,
                        end: sp.end,
                        text: entry.text.to_string(),
                        code_key: entry.syllable_key.to_string(),
                        kind: EdgeKind::ChineseWord,
                        base_cost: target_cost,
                        domain: entry.domain,
                        syllable_count: syl_count,
                        is_fuzzy: new_fuzzy > 0.0,
                        pronunciation_hint: None,
                    });
                }
            }
        }
        collect_dfs(
            origin_start,
            sp.end,
            next_node,
            syl_count,
            new_fuzzy,
            input,
            spans_at,
            out_edges,
        );
    }

    // 末尾不完整音节补全（如 `gongh` -> `公户`、`jiew` -> `接娃`、`furenq` -> `富人区`）
    let whole_slice_lower = input[origin_start..n].to_ascii_lowercase();
    if depth >= 1
        && accum_fuzzy == 0.0
        && effective_pos < n
        && !crate::pinyin::is_valid_or_alias_syllable(&whole_slice_lower)
    {
        let rem_tail = &input[effective_pos..n];
        if rem_tail.len() <= 4
            && rem_tail.bytes().all(|b| b.is_ascii_lowercase())
            && !crate::pinyin::is_valid_or_alias_syllable(rem_tail)
        {
            if let Some(p_node) = trie.advance_node(base_node, rem_tail) {
                for &id in trie.nodes[p_node as usize].top_prefix_entries.iter().take(4) {
                    let entry = &SUPPLEMENT_ENTRIES[id as usize];
                    if entry.syllable_key.split('\'').count() != syl_count {
                        continue;
                    }
                    if out_edges
                        .iter()
                        .any(|e| e.start == origin_start && e.end == n && e.text == entry.text)
                    {
                        continue;
                    }
                    out_edges.push(LatticeEdge {
                        start: origin_start,
                        end: n,
                        text: entry.text.to_string(),
                        code_key: entry.syllable_key.to_string(),
                        kind: EdgeKind::ChineseWord,
                        base_cost: entry.cost + 0.95,
                        domain: entry.domain,
                        syllable_count: syl_count,
                        is_fuzzy: false,
                        pronunciation_hint: None,
                    });
                }
            }
        }
    }
}

/// 补充词表简拼精确匹配（仅用于整串非全拼的纯简拼查询，如 `gh -> 公户`、`shkh -> 私行客户`）
pub fn lookup_supplement_abbrev(abbrev: &str) -> Vec<&'static SupplementEntry> {
    SUPPLEMENT_INDEX
        .abbrev_trie
        .exact_lookup(abbrev)
        .iter()
        .map(|&id| &SUPPLEMENT_ENTRIES[id as usize])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supplement_entries_are_valid_bmp_and_syllables() {
        for e in SUPPLEMENT_ENTRIES {
            assert!(!e.text.is_empty());
            assert!(
                e.text.chars().all(|c| (c as u32) <= 0xFFFF),
                "non-BMP char in {}",
                e.text
            );
            let syls: Vec<&str> = e.syllable_key.split('\'').collect();
            assert_eq!(
                syls.len(),
                e.text.chars().count(),
                "syllable count mismatch in {}",
                e.text
            );
            for s in syls {
                assert!(
                    crate::pinyin::is_valid_syllable(s),
                    "invalid syllable {s} in {}",
                    e.text
                );
            }
        }
        assert!(lookup_supplement_abbrev("shkh")
            .iter()
            .any(|e| e.text == "私行客户"));
    }
}
