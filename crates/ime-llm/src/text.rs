//! 语言模型似然打分的文本构造规则。
//!
//! 定义在 `ime_ai::lm_text`，这里原样转出。引导语、候选的规范化、48 个字符的上文上限与通过 HTTP 调用
//! llama-server 的 `score_candidates_by_lm` 共用同一份。
//!
//! 本 crate 的打分器用 [`lm_scoring_prefix_inprocess`] 构造前缀（HTTP 路径用 [`lm_scoring_prefix`]）。
//! 上文不超过 48 个字符、且不以「英文字母 / 数字 + 空白」结尾时，两者逐字节相同；区别只有两处：
//! - 上文超过 48 个字符时，窗口起点对齐到分句处（[`lm_context_tail_anchored`]），而不是固定从倒数第 48 个字符开始。
//!   用户连续上屏时，新前缀因此以上一次的前缀开头，跨调用的 KV 缓存才接得上
//! - 上文以英文字母或数字加空白结尾时保留一个空格，`I really love ` 之后的候选 `you` 按 `…love you` 打分

pub use ime_ai::lm_text::{
    lm_context_tail, lm_context_tail_anchored, lm_scoring_prefix, lm_scoring_prefix_inprocess, lm_scoring_text,
    normalize_for_lm_scoring, LM_SCORE_CONTEXT_ANCHOR_MIN_CHARS, LM_SCORE_CONTEXT_TAIL_CHARS, LM_SCORE_PROMPT_PREFIX,
};
