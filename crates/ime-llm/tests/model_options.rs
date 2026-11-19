//! 模型加载选项的测试，需要真实模型（默认跳过）。
//!
//! ```text
//! IME_LLM_TEST_MODEL=/path/to/model.gguf \
//!     cargo test --release -p ime-llm --features llama --test model_options -- --nocapture
//! ```
//!
//! `IME_LLM_TEST_TOLERANCE` 的含义与默认值同 `model_backed.rs`。
#![cfg(feature = "llama")]

use ime_llm::{phys_footprint_bytes, LlmError, LlmOptions, LlmScorer};
use std::time::Instant;

/// 释放推理上下文（键盘收起时）之后：分数与释放前逐位相同，内存降到只剩模型本体；
/// 打印各阶段的 phys_footprint 与重建耗时，作为 iOS 上取舍的依据
#[test]
fn release_context_keeps_scores_and_frees_memory() {
    let Some(path) = std::env::var_os("IME_LLM_TEST_MODEL").filter(|p| !p.is_empty()) else {
        eprintln!("跳过模型测试：未设置 IME_LLM_TEST_MODEL");
        return;
    };
    let mib = |b: Option<u64>| b.map(|b| b as f64 / 1048576.0).unwrap_or(f64::NAN);
    let base = phys_footprint_bytes();
    let t = Instant::now();
    let scorer = LlmScorer::load(&path, LlmOptions::default()).expect("加载 IME_LLM_TEST_MODEL 失败");
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;
    let context = "他在婚礼上许下了";
    let cands = ["誓言", "食盐", "试验"];
    let before = scorer.score(context, &cands).unwrap();
    let loaded = phys_footprint_bytes();

    scorer.release_context();
    let released = phys_footprint_bytes();

    let t = Instant::now();
    scorer.restore_context().expect("重建上下文");
    let restore_ms = t.elapsed().as_secs_f64() * 1000.0;
    let after = scorer.score(context, &cands).unwrap();
    let rescored = phys_footprint_bytes();
    assert_eq!(before, after, "释放并重建上下文之后分数应当逐位相同");

    // 不提前重建：打分时自动重建
    scorer.release_context();
    let t = Instant::now();
    let lazily = scorer.score(context, &cands).unwrap();
    let lazy_ms = t.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(before, lazily);

    println!(
        "[release_context] 基线 {:.1} MiB，加载+打分后 {:.1} MiB（加载 {load_ms:.0} ms），释放上下文后 {:.1} MiB，\
         重建后打分 {:.1} MiB；重建上下文 {restore_ms:.1} ms，释放后直接打分（含自动重建）{lazy_ms:.1} ms",
        mib(base),
        mib(loaded),
        mib(released),
        mib(rescored),
    );
    if let (Some(l), Some(r)) = (loaded, released) {
        assert!(r < l, "释放上下文后内存应当下降：{l} -> {r}");
    }
}

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
