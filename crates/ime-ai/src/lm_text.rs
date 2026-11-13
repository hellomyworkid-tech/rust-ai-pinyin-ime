//! 语言模型似然打分的文本构造规则。
//!
//! 这些常量与函数是「打分文本长什么样」的唯一出处：通过 HTTP 调用 llama-server 的
//! `score_candidates_by_lm` 与进程内打分器（`ime-llm`，实现 [`crate::CandidateScorer`]）都引用这里。
//!
//! 引导语、候选的规范化、48 个字符的上限两条路径共用。前缀的构造有两个版本：
//! - [`lm_scoring_prefix`]：HTTP 路径使用，窗口固定取上文的最后 48 个字符；
//! - [`lm_scoring_prefix_inprocess`]：给进程内打分器准备。上文不超过 48 个字符、且不以「英文 / 数字 + 空白」结尾时，
//!   两者逐字节相同；区别只在长上文的窗口起点（对齐到分句处，见 [`lm_context_tail_anchored`]）
//!   和英文上文之后的那个空格。

/// 语言模型似然打分时置于上文之前的引导语（让模型处于「书写规范中文」的分布中）
pub const LM_SCORE_PROMPT_PREFIX: &str = "下面是一句通顺自然、没有错别字的中文：";

/// 打分时保留的上文长度上限（按字符计，取上文末尾）
pub const LM_SCORE_CONTEXT_TAIL_CHARS: usize = 48;

/// 打分前的文本规范化：中英混输候选（如 `AI 选词`）去掉显示用空格，使其与自然书写形式 `AI选词` 一致
pub fn normalize_for_lm_scoring(text: &str) -> String {
    let has_cjk = text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
    if has_cjk {
        text.chars().filter(|c| *c != ' ').collect()
    } else {
        text.to_string()
    }
}

/// 取上文（去除首尾空白后）的最后 [`LM_SCORE_CONTEXT_TAIL_CHARS`] 个字符
pub fn lm_context_tail(surrounding_context: &str) -> &str {
    let trimmed = surrounding_context.trim();
    let total = trimmed.chars().count();
    if total <= LM_SCORE_CONTEXT_TAIL_CHARS {
        return trimmed;
    }
    let skip = total - LM_SCORE_CONTEXT_TAIL_CHARS;
    match trimmed.char_indices().nth(skip) {
        Some((byte_idx, _)) => &trimmed[byte_idx..],
        None => trimmed,
    }
}

/// 所有候选共享的打分前缀：引导语 + 上文末尾
pub fn lm_scoring_prefix(surrounding_context: &str) -> String {
    format!("{LM_SCORE_PROMPT_PREFIX}{}", lm_context_tail(surrounding_context))
}

/// 长上文的窗口起点至少要留下的字符数：起点只在距末尾 [`LM_SCORE_CONTEXT_TAIL_CHARS`] 到这个数之间找分句处
pub const LM_SCORE_CONTEXT_ANCHOR_MIN_CHARS: usize = 16;

/// 分句处：中英文的句读与任何空白字符
fn is_clause_boundary(c: char) -> bool {
    matches!(c, '，' | '。' | '！' | '？' | '；' | '：' | '、' | ',' | '.' | '!' | '?' | ';' | ':') || c.is_whitespace()
}

/// 与 [`lm_context_tail`] 相同，但上文超过 [`LM_SCORE_CONTEXT_TAIL_CHARS`] 个字符时，窗口不是从「倒数第 48 个字符」
/// 开始，而是对齐到分句处：在倒数第 48 到倒数第 16 个字符之间找第一个分句符（句读或空白），从它后面开始
/// （开头的空白一并去掉）；这个范围内没有分句符时才从倒数第 48 个字符开始。
///
/// 用途：进程内打分器会复用上一次请求已经算过的前缀（KV 缓存）。用户不断上屏时，固定长度的窗口每次都向后
/// 滑几个字符，前缀从引导语之后就不一样了，整段上文都要重算；对齐之后，窗口起点在接下来的三十来个字符里
/// 都停在同一个分句处，上一次的前缀是这一次的前缀的开头。起点由内容（分句符）决定，与宿主传进来多长的
/// 上文无关——iOS 键盘只传光标前最后 64 个字符，照样对得上。
///
/// 不超过 48 个字符的上文原样返回，与 [`lm_context_tail`] 完全相同。
pub fn lm_context_tail_anchored(surrounding_context: &str) -> &str {
    let trimmed = surrounding_context.trim();
    let total = trimmed.chars().count();
    if total <= LM_SCORE_CONTEXT_TAIL_CHARS {
        return trimmed;
    }
    let first = total - LM_SCORE_CONTEXT_TAIL_CHARS;
    let last = total - LM_SCORE_CONTEXT_ANCHOR_MIN_CHARS;
    let mut start = None;
    for (index, (byte_idx, c)) in trimmed.char_indices().enumerate().skip(first) {
        if index > last {
            break;
        }
        if start.is_none() {
            start = Some(byte_idx); // 兜底：倒数第 48 个字符
        }
        if is_clause_boundary(c) {
            start = Some(byte_idx + c.len_utf8());
            break;
        }
    }
    match start {
        // 分句符之后可能还跟着空白（「。\n」「, 」）；末尾至少还有 15 个字符且不以空白结尾，不会被去空
        Some(byte_idx) => trimmed[byte_idx..].trim_start(),
        None => trimmed,
    }
}

/// 进程内打分器使用的打分前缀：引导语 + [`lm_context_tail_anchored`] 的上文末尾。
///
/// 另外，原始上文以空白结尾、且上文末尾最后一个字符是 ASCII 字母或数字时，补回**一个**空格：
/// `"I really love "` 之后的候选 `you` 应当按 `…I really love you` 打分，而不是粘在一起的 `…loveyou`。
/// 以汉字或标点结尾的上文不补（中文之间本来就没有空格），所以中文上文下与 [`lm_scoring_prefix`] 的差别只有窗口起点。
pub fn lm_scoring_prefix_inprocess(surrounding_context: &str) -> String {
    let tail = lm_context_tail_anchored(surrounding_context);
    let ends_in_whitespace = surrounding_context.chars().next_back().is_some_and(char::is_whitespace);
    let tail_ends_in_latin = tail.chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric());
    let separator = if ends_in_whitespace && tail_ends_in_latin { " " } else { "" };
    format!("{LM_SCORE_PROMPT_PREFIX}{tail}{separator}")
}

/// 某个候选的完整打分文本：前缀 + 规范化后的候选（整体送去分词，与 llama-server 的 `/tokenize` 行为一致）
pub fn lm_scoring_text(prefix: &str, candidate: &str) -> String {
    format!("{prefix}{}", normalize_for_lm_scoring(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_keeps_last_48_chars_of_trimmed_context() {
        assert_eq!(lm_scoring_prefix(""), LM_SCORE_PROMPT_PREFIX);
        assert_eq!(lm_scoring_prefix("  今天天气 \n"), format!("{LM_SCORE_PROMPT_PREFIX}今天天气"));

        let long: String = (0..100).map(|i| char::from_u32(0x4e00 + i).unwrap()).collect();
        let tail = lm_context_tail(&long);
        assert_eq!(tail.chars().count(), LM_SCORE_CONTEXT_TAIL_CHARS);
        assert!(long.ends_with(tail));

        // 恰好 48 个字符时不截断；混合 ASCII 与多字节字符时按字符而非字节计数
        let exact: String = "a好".repeat(24);
        assert_eq!(lm_context_tail(&exact), exact);
        let over = format!("头{exact}");
        assert_eq!(lm_context_tail(&over), exact);
    }

    #[test]
    fn tail_matches_reference_construction() {
        // 与 openai_client::score_candidates_by_lm 原先的写法逐字符对照
        for ctx in ["", " a ", "周末天气不错，我们", &"长上文，".repeat(40), &"mixed 中英 text ".repeat(9)] {
            let chars: Vec<char> = ctx.trim().chars().collect();
            let start = chars.len().saturating_sub(48);
            let expected: String = chars[start..].iter().collect();
            assert_eq!(lm_context_tail(ctx), expected);
        }
    }

    #[test]
    fn anchored_tail_is_identical_for_short_contexts() {
        let exact: String = "a好".repeat(24);
        for ctx in ["", " a", "周末天气不错，我们", "  今天天气 \n", "他说：好。然后就走了", exact.as_str()] {
            assert_eq!(lm_context_tail_anchored(ctx), lm_context_tail(ctx));
            assert_eq!(lm_scoring_prefix_inprocess(ctx), lm_scoring_prefix(ctx), "{ctx:?}");
        }
    }

    #[test]
    fn anchored_tail_starts_after_the_first_boundary_in_range() {
        // 60 个字符，分句符在下标 5、20、40；窗口范围是下标 [12, 44]
        let mut chars: Vec<char> = (0..60).map(|i| char::from_u32(0x4e00 + i).unwrap()).collect();
        chars[5] = '。';
        chars[20] = '，';
        chars[40] = '！';
        let text: String = chars.iter().collect();
        let expected: String = chars[21..].iter().collect();
        assert_eq!(lm_context_tail_anchored(&text), expected, "范围内第一个分句符之后");
        assert!(lm_context_tail_anchored(&text).chars().count() <= LM_SCORE_CONTEXT_TAIL_CHARS);

        // 范围的两个端点都算在内
        let mut at_first = chars.clone();
        at_first[20] = '甲';
        at_first[40] = '乙';
        at_first[12] = '；';
        let text: String = at_first.iter().collect();
        assert_eq!(lm_context_tail_anchored(&text).chars().count(), 47);
        let mut at_last = at_first.clone();
        at_last[12] = '丙';
        at_last[44] = '\n';
        let text: String = at_last.iter().collect();
        assert_eq!(lm_context_tail_anchored(&text).chars().count(), 15);

        // 分句符只在范围之外（太靠前，或离末尾不足 16 个字符）：退回固定的 48 个字符
        let mut outside = at_last.clone();
        outside[44] = '丁';
        outside[11] = '。';
        outside[45] = '。';
        let text: String = outside.iter().collect();
        assert_eq!(lm_context_tail_anchored(&text), lm_context_tail(&text));
        assert_eq!(lm_context_tail_anchored(&text).chars().count(), LM_SCORE_CONTEXT_TAIL_CHARS);

        // 分句符后面紧跟的空白不留在开头；英文按空格对齐到词的开头
        let english = "The quick brown fox jumps over the lazy dog. Then it sat down and looked at the moon";
        let tail = lm_context_tail_anchored(english);
        assert!(english.ends_with(tail));
        assert!(!tail.starts_with(char::is_whitespace));
        let cut = english.len() - tail.len();
        assert!(english[..cut].ends_with(char::is_whitespace), "从词的开头开始: {tail:?}");
        assert!(tail.chars().count() <= LM_SCORE_CONTEXT_TAIL_CHARS && tail.chars().count() >= 15);
    }

    /// 用户一段一段上屏时，新的前缀应当以上一次的前缀开头（打分器才能复用已经算过的部分）。
    /// 固定 48 个字符的旧窗口在上文超过窗口之后每一步都对不上。
    #[test]
    fn inprocess_prefix_stays_a_string_prefix_while_text_grows() {
        let pieces = [
            "今天上午", "我们", "开了", "一个", "很长的", "会，", "讨论", "下个季度", "的", "产品", "计划。", "大家",
            "都觉得", "时间", "有点", "紧张，", "但是", "只要", "安排", "得当，", "应该", "还是", "可以", "按时",
            "完成的。", "会后", "我", "把", "会议", "纪要", "整理", "好了，", "发到", "了", "群里，", "请", "各位",
            "有空", "的时候", "看一下，", "有问题", "随时", "找我。", "另外，", "周五", "下午", "还有", "一次",
            "评审，", "别忘了", "提前", "准备", "材料。",
        ];
        let mut text = String::new();
        let (mut steps, mut kept_new, mut kept_old) = (0usize, 0usize, 0usize);
        let mut previous: Option<(String, String)> = None;
        for piece in pieces {
            text.push_str(piece);
            let new = lm_scoring_prefix_inprocess(&text);
            let old = lm_scoring_prefix(&text);
            if let Some((prev_new, prev_old)) = &previous {
                // 只统计上文已经超过窗口之后的步骤（之前两种前缀都是整段上文，自然保持）
                if text.chars().count() - piece.chars().count() > LM_SCORE_CONTEXT_TAIL_CHARS {
                    steps += 1;
                    kept_new += usize::from(new.starts_with(prev_new.as_str()));
                    kept_old += usize::from(old.starts_with(prev_old.as_str()));
                }
            }
            assert!(new.chars().count() <= LM_SCORE_PROMPT_PREFIX.chars().count() + LM_SCORE_CONTEXT_TAIL_CHARS);
            assert!(text.ends_with(lm_context_tail_anchored(&text)));
            // 宿主只传光标前最后 64 个字符（iOS 键盘的做法）时结果相同：起点由内容决定，与传进来多长无关
            let total = text.chars().count();
            let last_64: String = text.chars().skip(total.saturating_sub(64)).collect();
            assert_eq!(lm_scoring_prefix_inprocess(&last_64), new);
            previous = Some((new, old));
        }
        assert!(steps >= 30, "用例应覆盖足够多的超窗步骤: {steps}");
        assert_eq!(kept_old, 0, "固定窗口每一步都在滑动");
        println!("保持前缀的步骤：对齐 {kept_new}/{steps}，固定窗口 {kept_old}/{steps}");
        assert!(kept_new * 10 >= steps * 8, "对齐到分句处之后，绝大多数步骤应保持前缀: {kept_new}/{steps}");
    }

    #[test]
    fn inprocess_prefix_keeps_one_space_after_latin_context() {
        let p = LM_SCORE_PROMPT_PREFIX;
        // 英文 / 数字结尾 + 空白：补回一个空格（多个空白也只补一个）
        assert_eq!(lm_scoring_prefix_inprocess("I really love "), format!("{p}I really love "));
        assert_eq!(lm_scoring_prefix_inprocess("I really love  \n"), format!("{p}I really love "));
        assert_eq!(lm_scoring_prefix_inprocess("版本 2 "), format!("{p}版本 2 "));
        assert_eq!(lm_scoring_text(&lm_scoring_prefix_inprocess("I really love "), "you"), format!("{p}I really love you"));
        // 没有以空白结尾：不补
        assert_eq!(lm_scoring_prefix_inprocess("I really love"), format!("{p}I really love"));
        // 汉字或标点结尾：有空白也不补
        assert_eq!(lm_scoring_prefix_inprocess("今天天气 "), format!("{p}今天天气"));
        assert_eq!(lm_scoring_prefix_inprocess("Hello, "), format!("{p}Hello,"));
        assert_eq!(lm_scoring_prefix_inprocess("  \n"), p);
        // 旧的前缀函数不变
        assert_eq!(lm_scoring_prefix("I really love "), format!("{p}I really love"));
        // 长上文：对齐与补空格同时生效
        let long = format!("{}。and then I said I really love ", "很长的上文".repeat(12));
        let prefix = lm_scoring_prefix_inprocess(&long);
        assert!(prefix.ends_with("I really love "));
        assert!(prefix.starts_with(&format!("{p}and then")), "{prefix}");
    }

    #[test]
    fn normalization_strips_display_spaces_only_for_cjk() {
        assert_eq!(normalize_for_lm_scoring("AI 选词"), "AI选词");
        assert_eq!(normalize_for_lm_scoring("hello world"), "hello world");
        assert_eq!(normalize_for_lm_scoring("去公园玩"), "去公园玩");
        assert_eq!(lm_scoring_text("前缀", "用 Rust 写"), "前缀用Rust写");
    }
}
