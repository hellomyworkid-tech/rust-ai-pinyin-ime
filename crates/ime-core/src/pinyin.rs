//! `pinyin`: 汉语拼音全拼/简拼/模糊音/双拼切分器与 26×26 音系学合法性位图探测器

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::OnceLock;

/// 标准现代汉语无调音节全表（涵盖《汉语拼音方案》全部 410+ 合法音节，使用 `v` 代表 `ü`）
pub const VALID_PINYIN_SYLLABLES: &[&str] = &[
    "a", "ai", "an", "ang", "ao",
    "ba", "bai", "ban", "bang", "bao", "bei", "ben", "beng", "bi", "bian", "biao", "bie", "bin", "bing", "bo", "bu",
    "ca", "cai", "can", "cang", "cao", "ce", "cen", "ceng", "cha", "chai", "chan", "chang", "chao", "che", "chen", "cheng", "chi", "chong", "chou", "chu", "chua", "chuai", "chuan", "chuang", "chui", "chun", "chuo", "ci", "cong", "cou", "cu", "cuan", "cui", "cun", "cuo",
    "da", "dai", "dan", "dang", "dao", "de", "dei", "den", "deng", "di", "dia", "dian", "diao", "die", "ding", "diu", "dong", "dou", "du", "duan", "dui", "dun", "duo",
    "e", "ei", "en", "eng", "er",
    "fa", "fan", "fang", "fei", "fen", "feng", "fo", "fou", "fu",
    "ga", "gai", "gan", "gang", "gao", "ge", "gei", "gen", "geng", "gong", "gou", "gu", "gua", "guai", "guan", "guang", "gui", "gun", "guo",
    "ha", "hai", "han", "hang", "hao", "he", "hei", "hen", "heng", "hong", "hou", "hu", "hua", "huai", "huan", "huang", "hui", "hun", "huo",
    "ji", "jia", "jian", "jiang", "jiao", "jie", "jin", "jing", "jiong", "jiu", "ju", "juan", "jue", "jun",
    "ka", "kai", "kan", "kang", "kao", "ke", "kei", "ken", "keng", "kong", "kou", "ku", "kua", "kuai", "kuan", "kuang", "kui", "kun", "kuo",
    "la", "lai", "lan", "lang", "lao", "le", "lei", "leng", "li", "lia", "lian", "liang", "liao", "lie", "lin", "ling", "liu", "lo", "long", "lou", "lu", "luan", "lun", "luo", "lv", "lve",
    "ma", "mai", "man", "mang", "mao", "me", "mei", "men", "meng", "mi", "mian", "miao", "mie", "min", "ming", "miu", "mo", "mou", "mu",
    "na", "nai", "nan", "nang", "nao", "ne", "nei", "nen", "neng", "ni", "nian", "niang", "niao", "nie", "nin", "ning", "niu", "nong", "nou", "nu", "nuan", "nun", "nuo", "nv", "nve",
    "o", "ou",
    "pa", "pai", "pan", "pang", "pao", "pei", "pen", "peng", "pi", "pian", "piao", "pie", "pin", "ping", "po", "pou", "pu",
    "qi", "qia", "qian", "qiang", "qiao", "qie", "qin", "qing", "qiong", "qiu", "qu", "quan", "que", "qun",
    "ran", "rang", "rao", "re", "ren", "reng", "ri", "rong", "rou", "ru", "rua", "ruan", "rui", "run", "ruo",
    "sa", "sai", "san", "sang", "sao", "se", "sen", "seng", "sha", "shai", "shan", "shang", "shao", "she", "shei", "shen", "sheng", "shi", "shou", "shu", "shua", "shuai", "shuan", "shuang", "shui", "shun", "shuo", "si", "song", "sou", "su", "suan", "sui", "sun", "suo",
    "ta", "tai", "tan", "tang", "tao", "te", "tei", "teng", "ti", "tian", "tiao", "tie", "ting", "tong", "tou", "tu", "tuan", "tui", "tun", "tuo",
    "wa", "wai", "wan", "wang", "wei", "wen", "weng", "wo", "wu",
    "xi", "xia", "xian", "xiang", "xiao", "xie", "xin", "xing", "xiong", "xiu", "xu", "xuan", "xue", "xun",
    "ya", "yan", "yang", "yao", "ye", "yi", "yin", "ying", "yo", "yong", "you", "yu", "yuan", "yue", "yun",
    "za", "zai", "zan", "zang", "zao", "ze", "zei", "zen", "zeng", "zha", "zhai", "zhan", "zhang", "zhao", "zhe", "zhei", "zhen", "zheng", "zhi", "zhong", "zhou", "zhu", "zhua", "zhuai", "zhuan", "zhuang", "zhui", "zhun", "zhuo", "zi", "zong", "zou", "zu", "zuan", "zui", "zun", "zuo",
];

fn syllable_set() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| VALID_PINYIN_SYLLABLES.iter().copied().collect())
}

#[inline]
pub fn is_valid_syllable(s: &str) -> bool {
    syllable_set().contains(s)
}

/// 模糊音与拼写纠错配置（支持平翘舌、前后鼻音、鼻边音、f/h 模糊及常见按键顺序倒置纠错）
///
/// 代价设计原则：「正确输入永远优先」。
/// 每做一次声母或韵母替换就累加一次惩罚（如 `zeng -> zhen` 同时改了声母和韵母，惩罚为两次之和），
/// 只有当用户原样输入的拼音组不成高频词时，纠错候选才会浮到前面。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuzzyPinyinConfig {
    pub z_zh: bool,
    pub c_ch: bool,
    pub s_sh: bool,
    /// 鼻边音 n/l（方言相关，主流输入法默认关闭）
    pub n_l: bool,
    /// 唇齿/舌根 f/h（方言相关，主流输入法默认关闭）
    pub f_h: bool,
    pub in_ing: bool,
    pub en_eng: bool,
    pub an_ang: bool,
    pub typo_correction: bool,
}

impl Default for FuzzyPinyinConfig {
    fn default() -> Self {
        Self {
            z_zh: true,
            c_ch: true,
            s_sh: true,
            n_l: false,
            f_h: false,
            in_ing: true,
            en_eng: true,
            an_ang: true,
            typo_correction: true,
        }
    }
}

/// 平翘舌 / 前后鼻音单次替换惩罚
const FUZZY_STEP_PENALTY: f32 = 1.6;
/// 方言类（n/l, f/h）单次替换惩罚
const DIALECT_STEP_PENALTY: f32 = 2.0;
/// 键盘手误（字母顺序倒置）纠错惩罚：原输入本身不是合法音节，不与正确输入竞争
const TYPO_PENALTY: f32 = 0.8;

impl FuzzyPinyinConfig {
    /// 生成给定合法或近似音节的所有模糊音与拼写纠错候选及额外代价
    pub fn expand_fuzzy(&self, syl: &str) -> Vec<(String, f32)> {
        let mut results: Vec<(String, f32)> = Vec::new();
        let push = |results: &mut Vec<(String, f32)>, v: String, cost: f32| {
            if !is_valid_syllable(&v) {
                return;
            }
            if let Some(existing) = results.iter_mut().find(|(x, _)| *x == v) {
                if cost < existing.1 {
                    existing.1 = cost;
                }
            } else {
                results.push((v, cost));
            }
        };

        // 0. 原样输入（零代价）+ 等价拼写别名（`lue/nue` 是 `lve/nve` 的常见合法写法，零代价）
        push(&mut results, syl.to_string(), 0.0);
        for (alias, canon) in [("lue", "lve"), ("nue", "nve")] {
            if syl == alias {
                push(&mut results, canon.to_string(), 0.0);
            }
        }

        // 1. 键盘手误 / 韵母字母顺序倒置纠错
        let mut seeds: Vec<(String, f32)> = vec![(syl.to_string(), 0.0)];
        if self.typo_correction && syl.len() >= 2 && !is_valid_syllable(syl) {
            const TYPO_SUFFIXES: &[(&str, &str)] = &[
                ("ign", "ing"),
                ("agn", "ang"),
                ("egn", "eng"),
                ("ogn", "ong"),
                ("ain", "ian"),
                ("aun", "uan"),
                ("uei", "ui"),
                ("uen", "un"),
                ("iou", "iu"),
                ("ung", "ong"),
            ];
            for &(bad, good) in TYPO_SUFFIXES {
                if let Some(prefix) = syl.strip_suffix(bad) {
                    let fixed = format!("{prefix}{good}");
                    if is_valid_syllable(&fixed) {
                        push(&mut results, fixed.clone(), TYPO_PENALTY);
                        seeds.push((fixed, TYPO_PENALTY));
                    }
                }
            }
            // `jv/qv/xv/yv` -> `ju/qu/xu/yu`（仅限 j/q/x/y；`lv/nv` 本身就是正确写法，绝不能改成 `lu/nu`）
            if let Some(rest) = syl.strip_suffix('v') {
                if matches!(rest, "j" | "q" | "x" | "y") {
                    let fixed = format!("{rest}u");
                    push(&mut results, fixed.clone(), TYPO_PENALTY);
                    seeds.push((fixed, TYPO_PENALTY));
                }
            }
        }

        // 2. 声母模糊（平翘舌 + 可选的方言类），每次替换累加惩罚
        let mut after_initial: Vec<(String, f32)> = seeds.clone();
        for (seed, base) in &seeds {
            let mut add = |v: String, p: f32| after_initial.push((v, base + p));
            for (on, short, long) in [
                (self.z_zh, "z", "zh"),
                (self.c_ch, "c", "ch"),
                (self.s_sh, "s", "sh"),
            ] {
                if !on {
                    continue;
                }
                if let Some(rest) = seed.strip_prefix(long) {
                    add(format!("{short}{rest}"), FUZZY_STEP_PENALTY);
                } else if let Some(rest) = seed.strip_prefix(short) {
                    if !rest.starts_with('h') {
                        add(format!("{long}{rest}"), FUZZY_STEP_PENALTY);
                    }
                }
            }
            if self.n_l {
                if let Some(rest) = seed.strip_prefix('n') {
                    add(format!("l{rest}"), DIALECT_STEP_PENALTY);
                } else if let Some(rest) = seed.strip_prefix('l') {
                    add(format!("n{rest}"), DIALECT_STEP_PENALTY);
                }
            }
            if self.f_h {
                if let Some(rest) = seed.strip_prefix('f') {
                    add(format!("h{rest}"), DIALECT_STEP_PENALTY);
                } else if let Some(rest) = seed.strip_prefix('h') {
                    add(format!("f{rest}"), DIALECT_STEP_PENALTY);
                }
            }
        }

        // 3. 韵母模糊（前后鼻音），每次替换累加惩罚
        let mut all = after_initial.clone();
        for (item, base) in &after_initial {
            for (on, short, long) in [
                (self.in_ing, "in", "ing"),
                (self.en_eng, "en", "eng"),
                (self.an_ang, "an", "ang"),
            ] {
                if !on {
                    continue;
                }
                if let Some(prefix) = item.strip_suffix(long) {
                    all.push((format!("{prefix}{short}"), base + FUZZY_STEP_PENALTY));
                } else if let Some(prefix) = item.strip_suffix(short) {
                    all.push((format!("{prefix}{long}"), base + FUZZY_STEP_PENALTY));
                }
            }
        }

        for (v, cost) in all {
            if v != syl {
                push(&mut results, v, cost);
            }
        }
        results
    }
}

/// 双拼方案支持
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShuangpinScheme {
    None,
    Xiaohe,
}

impl ShuangpinScheme {
    /// 将小鹤双拼 2 字母键码展开为全拼音节（如 `"xh"` -> `"xiao"`, `"he"`）
    pub fn decode_pair(self, pair: &str) -> Option<&'static str> {
        if self == Self::None || pair.len() != 2 {
            return None;
        }
        let bytes = pair.as_bytes();
        let initial = match bytes[0] {
            b'v' => "zh",
            b'i' => "ch",
            b'u' => "sh",
            b'a'..=b'z' => std::str::from_utf8(&bytes[0..1]).ok()?,
            _ => return None,
        };
        let final_char = bytes[1] as char;
        // 小鹤双拼常见韵母表映射
        let candidates: &[&str] = match final_char {
            'a' => &["a"],
            'b' => &["in"],
            'c' => &["ao"],
            'd' => &["ai"],
            'e' => &["e"],
            'f' => &["en"],
            'g' => &["eng"],
            'h' => &["ang"],
            'i' => &["i"],
            'j' => &["an"],
            'k' => &["iao", "ing"],
            'l' => &["iang", "uang"],
            'm' => &["ian"],
            'n' => &["iao"],
            'o' => &["uo", "o"],
            'p' => &["ie"],
            'q' => &["iu"],
            'r' => &["uan"],
            's' => &["iong", "ong"],
            't' => &["ue", "ve"],
            'u' => &["u"],
            'v' => &["ui", "v"],
            'w' => &["ei"],
            'x' => &["ia", "ua"],
            'y' => &["un"],
            'z' => &["ou"],
            _ => return None,
        };
        for &fin in candidates {
            let combined = format!("{initial}{fin}");
            if let Some(&static_syl) = syllable_set().get(combined.as_str()) {
                return Some(static_syl);
            }
        }
        None
    }
}

/// 26×26 汉语拼音音系学合法性探测器（Phonotactic Violation Detector）
///
/// 作用：在無空格中英文混输（如 `wozaiyongpythonxiecode`）中，
/// 瞬间探测出违反汉语拼音拼写规则的字母组合（如 `th`, `mp`, `pt`, `ct`, `str`, `ck`, `fl`），
/// 从而将该区段零延迟锁定为英文单词。
pub struct PhonotacticDetector {
    /// 音节内部合法的 2 字母组合位图
    internal_bigram: [[bool; 26]; 26],
}

impl PhonotacticDetector {
    pub fn global() -> &'static Self {
        static DETECTOR: OnceLock<PhonotacticDetector> = OnceLock::new();
        DETECTOR.get_or_init(Self::build)
    }

    fn build() -> Self {
        let mut internal_bigram = [[false; 26]; 26];
        let mut can_start = [false; 26];
        let mut can_end = [false; 26];

        for &syl in VALID_PINYIN_SYLLABLES {
            let bytes = syl.as_bytes();
            if let (Some(&first), Some(&last)) = (bytes.first(), bytes.last()) {
                can_start[(first - b'a') as usize] = true;
                can_end[(last - b'a') as usize] = true;
            }
            for w in bytes.windows(2) {
                if w[0].is_ascii_lowercase() && w[1].is_ascii_lowercase() {
                    let i = (w[0] - b'a') as usize;
                    let j = (w[1] - b'a') as usize;
                    internal_bigram[i][j] = true;
                }
            }
        }
        // 允许合法的跨音节边界（前音节韵尾 -> 后音节声母）
        for i in 0..26 {
            for j in 0..26 {
                if can_end[i] && can_start[j] {
                    internal_bigram[i][j] = true;
                }
            }
        }
        Self { internal_bigram }
    }

    /// 检查一段连续 ASCII 字符串是否包含典型的英语辅音簇（汉语拼音音节内绝不可能出现的二元组）
    pub fn english_cluster_score(&self, span: &str) -> f32 {
        if span.len() < 2 {
            return 0.0;
        }
        let lower = span.to_ascii_lowercase();
        let bytes = lower.as_bytes();
        let mut violations = 0usize;

        // 强英文特征辅音连缀（即使跨拼音简拼也极少自然连打的典型英语词根特征）
        const HARD_CLUSTERS: &[&str] = &[
            "th", "mp", "pt", "ct", "ck", "str", "spr", "spl", "scr", "fl", "fr", "pl", "pr",
            "bl", "br", "cl", "cr", "gl", "gr", "dr", "tr", "tw", "sw", "sp", "sk", "sm", "sn",
            "ld", "lf", "lk", "lm", "lp", "lt", "nd", "nk", "nt", "rd", "rf", "rk", "rl", "rm",
            "rn", "rp", "rt", "rv", "rz", "st", "xp", "xt", "vr", "vl", "gh", "ph", "wh",
            "md", "vd", "au", "tx", "gp", "cp", "np", "nv",
        ];
        for &cluster in HARD_CLUSTERS {
            if lower.contains(cluster) {
                violations += 2;
            }
        }

        for w in bytes.windows(2) {
            if w[0].is_ascii_lowercase() && w[1].is_ascii_lowercase() {
                let i = (w[0] - b'a') as usize;
                let j = (w[1] - b'a') as usize;
                if !self.internal_bigram[i][j] {
                    violations += 1;
                }
            }
        }
        violations as f32
    }
}

/// 切分出的单个拼音音节步
#[derive(Debug, Clone, PartialEq)]
pub struct SyllableSpan {
    pub start: usize,
    pub end: usize,
    pub syllable: String,
    pub is_abbrev: bool,
    pub fuzzy_penalty: f32,
}

/// 对输入字符串在区间 `[0..N]` 上枚举所有合法的拼音音节边（支持全拼歧义切分与首字母简拼）
pub fn extract_syllable_spans(
    input: &str,
    fuzzy: &FuzzyPinyinConfig,
    shuangpin: ShuangpinScheme,
) -> Vec<SyllableSpan> {
    let n = input.len();
    let bytes = input.as_bytes();
    let mut spans = Vec::new();

    for i in 0..n {
        if !bytes[i].is_ascii_alphabetic() {
            continue;
        }
        // 仅对小写字母起始的片段尝试拼音切分（大写字母开头直接交由英文切分器处理）
        if bytes[i].is_ascii_uppercase() {
            continue;
        }

        let max_len = (n - i).min(6);
        for len in (1..=max_len).rev() {
            let sub = &input[i..i + len];
            if !sub.bytes().all(|b| b.is_ascii_lowercase()) {
                continue;
            }

            if len == 2 && shuangpin != ShuangpinScheme::None {
                if let Some(decoded) = shuangpin.decode_pair(sub) {
                    spans.push(SyllableSpan {
                        start: i,
                        end: i + 2,
                        syllable: decoded.to_string(),
                        is_abbrev: false,
                        fuzzy_penalty: 0.0,
                    });
                }
            }

            let expanded = fuzzy.expand_fuzzy(sub);
            for (syl, penalty) in expanded {
                // 若存在元音尾与后继元音粘连（如 `xian` 切出 `xi` + `an` vs `xian`），由 DAG 自然保留两条路径
                spans.push(SyllableSpan {
                    start: i,
                    end: i + len,
                    syllable: syl,
                    is_abbrev: false,
                    fuzzy_penalty: penalty,
                });
            }

            // 单字母简拼边（如 `j`, `s`, `j`），排除 `i`, `u`（除非 `v`）
            if len == 1 && !matches!(bytes[i], b'i' | b'u') && !is_valid_syllable(sub) {
                spans.push(SyllableSpan {
                    start: i,
                    end: i + 1,
                    syllable: sub.to_string(),
                    is_abbrev: true,
                    fuzzy_penalty: 0.0,
                });
            }
        }
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ambiguous_pinyin_segmentation() {
        let fuzzy = FuzzyPinyinConfig::default();
        let spans = extract_syllable_spans("fangan", &fuzzy, ShuangpinScheme::None);
        // 必须同时包含 ["fang", "an"] 与 ["fan", "gan"] 所需的所有音节边
        assert!(spans.iter().any(|s| s.start == 0 && s.end == 4 && s.syllable == "fang"));
        assert!(spans.iter().any(|s| s.start == 4 && s.end == 6 && s.syllable == "an"));
        assert!(spans.iter().any(|s| s.start == 0 && s.end == 3 && s.syllable == "fan"));
        assert!(spans.iter().any(|s| s.start == 3 && s.end == 6 && s.syllable == "gan"));
    }

    #[test]
    fn test_phonotactic_detector() {
        let det = PhonotacticDetector::global();
        assert!(det.english_cluster_score("python") >= 3.0);
        assert!(det.english_cluster_score("typescript") >= 4.0);
        assert_eq!(det.english_cluster_score("nihao"), 0.0);
    }
}
