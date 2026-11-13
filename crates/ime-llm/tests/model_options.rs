//! 模型加载选项的测试，需要真实模型（默认跳过）。
//!
//! ```text
//! IME_LLM_TEST_MODEL=/path/to/model.gguf \
//!     cargo test --release -p ime-llm --features llama --test model_options -- --nocapture
//! ```
//!
//! `IME_LLM_TEST_TOLERANCE` 的含义与默认值同 `model_backed.rs`。
#![cfg(feature = "llama")]

use ime_llm::{LlmError, LlmOptions, LlmScorer};

fn tolerance() -> f64 {
    std::env::var("IME_LLM_TEST_TOLERANCE").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0)
}

fn max_abs_diff(a: &[f64], b: &[f64]) -> f64 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f64::max)
}

/// 上下文比并行序列数还小的选项不能让进程中止（llama.cpp 创建上下文时的断言失败是 abort，不是错误返回）
#[test]
fn tiny_context_options_do_not_abort_the_process() {
    let Some(path) = std::env::var_os("IME_LLM_TEST_MODEL").filter(|p| !p.is_empty()) else {
        eprintln!("跳过模型测试：未设置 IME_LLM_TEST_MODEL");
        return;
    };
    let reference = LlmScorer::load(&path, LlmOptions::default())
        .expect("加载 IME_LLM_TEST_MODEL 失败")
        .score("我们", &["去", "趣"])
        .unwrap();

    let tiny = LlmOptions { context_size: 4, max_parallel: 8, max_outputs: 8, ..LlmOptions::default() };
    let scorer = LlmScorer::load(&path, tiny).expect("context_size 4 应当能加载");
    let limits = scorer.limits();
    println!("[limits] context_size 4, max_parallel 8 -> {limits:?}");
    assert!(limits.n_seq_max >= 1 && limits.n_seq_max <= 4, "{limits:?}");
    assert!(limits.n_seq_max <= limits.n_outputs_max, "{limits:?}");
    // 照常能用（llama.cpp 会把上下文长度向上取整；真的放不下时是 ContextOverflow，不是崩溃）
    match scorer.score("我们", &["去", "趣"]) {
        Ok(scores) => assert!(max_abs_diff(&scores, &reference) <= tolerance(), "{scores:?} vs {reference:?}"),
        Err(LlmError::ContextOverflow { .. }) => {}
        Err(other) => panic!("意外错误: {other:?}"),
    }
    drop(scorer);

    // 并行序列数大于 logits 行数：行数被抬高到并行序列数
    let wide = LlmOptions { max_parallel: 12, max_outputs: 2, ..LlmOptions::default() };
    let scorer = LlmScorer::load(&path, wide).expect("max_parallel 12 / max_outputs 2 应当能加载");
    let limits = scorer.limits();
    println!("[limits] max_parallel 12, max_outputs 2 -> {limits:?}");
    assert_eq!((limits.n_seq_max, limits.n_outputs_max), (12, 12));
    let scores = scorer.score("我们", &["去", "趣"]).unwrap();
    assert!(max_abs_diff(&scores, &reference) <= tolerance(), "{scores:?} vs {reference:?}");
}
