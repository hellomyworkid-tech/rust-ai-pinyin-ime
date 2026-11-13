//! 需要真实模型的测试（默认跳过）。
//!
//! ```text
//! IME_LLM_TEST_MODEL=/path/to/model.gguf \
//!     cargo test --release -p ime-llm --features llama --test model_backed -- --nocapture
//! ```
//!
//! 加载选项的测试在 `model_options.rs`（另一个测试进程：它要额外加载几次模型，放在这里会搅乱本文件的内存报告）。
//!
//! 可选环境变量：
//! - `IME_LLM_TEST_THREADS`：推理线程数（默认自动）
//! - `IME_LLM_TEST_TOLERANCE`：批量 / 复用路径与逐候选参考实现之间允许的最大差值（nat），默认 1.0。
//!   实测（详见 crate 文档的「数值一致性」一节）：
//!   - Qwen3-0.6B Q4_K_M：两条路径逐位一致，可以设为 0
//!   - Qwen2.5-0.5B q4_k_m：ggml 按一次解码的 token 数选不同的矩阵乘法内核，得分随之不同。
//!     macOS 本机构建下，本文件的用例最大相差 0.32 nat，900 次随机调用最大相差 0.81 nat（所以默认值取 1.0）；
//!     按 iOS 的编译选项（armv8.4-a，无 i8mm）构建时同样的 900 次调用最大相差 0.000003 nat
#![cfg(feature = "llama")]

use ime_llm::{phys_footprint_bytes, LlmError, LlmOptions, LlmScorer};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// 上文 + 解码器给出的整句候选（取自 data/eval/context_cases.tsv 的评测用例）
const CASES: &[(&str, &[&str])] = &[
    ("周末天气不错，我们", &["去公园完", "去公园万", "去公园晚", "去公园玩"]),
    ("这篇论文的", &["实验结果", "试验结果"]),
    ("他在婚礼上许下了", &["实验", "试验", "式样", "饰演", "食盐", "誓言", "试样", "失言"]),
    ("请把文件发到我的", &["邮箱", "油箱", "有效", "右向", "游想", "又想", "有想", "有向"]),
    ("车快没油了，赶紧把", &["邮箱加满", "油箱加满", "有效加满", "邮箱家满"]),
    ("我不知道这是", &["怎么回事", "怎么会是", "怎么回是", "怎么会事"]),
    ("公民享有选举的", &["权利", "权力", "全力", "全立", "圈里"]),
    ("", &["你好", "拟好", "尼好"]),
    ("用 Rust 写一个", &["AI 选词", "AI 炫词", "爱选词"]),
    (
        "下周就要",
        &["期中考试了", "其中考试了", "期终考试了", "七种考试了", "期中考试乐", "其中考试乐", "气中考试了", "期中烤柿了"],
    ),
];

fn env_parse<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

fn tolerance() -> f64 {
    env_parse("IME_LLM_TEST_TOLERANCE").unwrap_or(1.0)
}

/// 引擎给公共前缀分块解码时每块的 token 数（`engine::PREFIX_CHUNK_TOKENS`）
const PREFIX_CHUNK_TOKENS: usize = 16;

struct Shared {
    scorer: Arc<LlmScorer>,
    load_ms: f64,
    footprint_before: Option<u64>,
    footprint_loaded: Option<u64>,
}

/// 整个测试进程只加载一次模型；各测试串行使用（它们会读取 `last_stats`、互相取消）
fn shared() -> Option<(MutexGuard<'static, ()>, &'static Shared)> {
    static SHARED: OnceLock<Option<Shared>> = OnceLock::new();
    static SERIAL: Mutex<()> = Mutex::new(());
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let shared = SHARED.get_or_init(|| {
        let Some(path) = std::env::var_os("IME_LLM_TEST_MODEL").filter(|p| !p.is_empty()) else {
            eprintln!("跳过模型测试：未设置 IME_LLM_TEST_MODEL");
            return None;
        };
        let options = LlmOptions { threads: env_parse("IME_LLM_TEST_THREADS").unwrap_or(0), ..LlmOptions::default() };
        let footprint_before = phys_footprint_bytes();
        let t = Instant::now();
        let scorer = LlmScorer::load(&path, options).expect("加载 IME_LLM_TEST_MODEL 失败");
        Some(Shared {
            scorer: Arc::new(scorer),
            load_ms: t.elapsed().as_secs_f64() * 1000.0,
            footprint_before,
            footprint_loaded: phys_footprint_bytes(),
        })
    });
    shared.as_ref().map(|s| (guard, s))
}

fn max_abs_diff(a: &[f64], b: &[f64]) -> f64 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f64::max)
}

fn mib(bytes: Option<u64>) -> f64 {
    bytes.unwrap_or(0) as f64 / 1_048_576.0
}

#[test]
fn batched_and_reused_scores_match_naive() {
    let Some((_guard, shared)) = shared() else { return };
    let scorer = &shared.scorer;
    let tol = tolerance();
    let mut worst_cold = 0.0f64;
    let mut worst_warm = 0.0f64;
    for (context, candidates) in CASES {
        let naive = scorer.score_naive(context, candidates).unwrap();
        assert_eq!(naive.len(), candidates.len());
        assert!(naive.iter().all(|s| s.is_finite() && *s <= 0.0), "对数概率之和应为有限的非正数: {naive:?}");

        // 冷缓存：公共前缀从头解码（16 个 token 一块）+ 各候选分支合批
        scorer.clear_cache();
        let cold = scorer.score(context, candidates).unwrap();
        // 热缓存：上一次调用就是同一上文，公共前缀只重算最后一个 token
        let warm = scorer.score(context, candidates).unwrap();
        let stats = scorer.last_stats();
        assert_eq!(stats.prefix_tokens_decoded, 1, "{stats:?}");
        assert_eq!(stats.reused_tokens + 1, stats.common_prefix_tokens, "{stats:?}");

        let (d_cold, d_warm) = (max_abs_diff(&naive, &cold), max_abs_diff(&naive, &warm));
        worst_cold = worst_cold.max(d_cold);
        worst_warm = worst_warm.max(d_warm);
        assert!(d_cold <= tol, "上文「{context}」冷缓存差值 {d_cold:.4} > {tol}\nnaive {naive:?}\ncold  {cold:?}");
        assert!(d_warm <= tol, "上文「{context}」热缓存差值 {d_warm:.4} > {tol}\nnaive {naive:?}\nwarm  {warm:?}");
    }
    println!("[consistency] max |batched - naive|: cold cache {worst_cold:.5} nats, warm cache {worst_warm:.5} nats (tolerance {tol})");
}

#[test]
fn language_model_prefers_the_contextually_correct_candidate() {
    let Some((_guard, shared)) = shared() else { return };
    let scorer = &shared.scorer;
    // 两个区分度很大的用例：小模型也应当选对
    for (context, candidates, expected) in [
        ("他在婚礼上许下了", &["实验", "试验", "食盐", "誓言"][..], "誓言"),
        ("车快没油了，赶紧把", &["邮箱加满", "油箱加满"][..], "油箱加满"),
    ] {
        let scores = scorer.score(context, candidates).unwrap();
        let best = scores.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(i, _)| i).unwrap();
        assert_eq!(candidates[best], expected, "上文「{context}」得分 {scores:?}");
    }
    // 单个候选不需要推理
    assert_eq!(scorer.score("上文", &["候选"]).unwrap(), vec![0.0]);
    // 显示用空格不影响得分
    let a = scorer.score("用 Rust 写一个", &["AI 选词", "爱选词"]).unwrap();
    let b = scorer.score("用 Rust 写一个", &["AI选词", "爱选词"]).unwrap();
    assert_eq!(a, b);
}

#[test]
fn more_candidates_than_parallel_sequences() {
    let Some((_guard, shared)) = shared() else { return };
    let scorer = &shared.scorer;
    let context = "他在婚礼上许下了";
    let candidates: Vec<String> = ["实验", "试验", "式样", "饰演", "食盐", "誓言", "试样", "失言", "事宜", "适宜", "十亿", "诗意"]
        .iter()
        .map(|w| format!("{w}，全场都很感动"))
        .collect();
    assert!(candidates.len() > scorer.limits().n_seq_max);
    let naive = scorer.score_naive(context, &candidates).unwrap();
    scorer.clear_cache();
    let batched = scorer.score(context, &candidates).unwrap();
    let stats = scorer.last_stats();
    assert!(stats.groups >= 2, "{stats:?}");
    let d = max_abs_diff(&naive, &batched);
    assert!(d <= tolerance(), "差值 {d:.4}\nnaive   {naive:?}\nbatched {batched:?}");
    // 超长候选：报错而不是崩溃，之后仍可使用
    let too_long = "很长的候选".repeat(80);
    let err = scorer.score(context, &["短候选".to_string(), too_long]).unwrap_err();
    assert!(matches!(err, LlmError::ContextOverflow { .. }), "{err:?}");
    let again = scorer.score(context, &candidates).unwrap();
    assert!(max_abs_diff(&batched, &again) <= tolerance());
}

#[test]
fn cancel_from_another_thread_aborts_inference() {
    let Some((_guard, shared)) = shared() else { return };
    let scorer = Arc::clone(&shared.scorer);
    let (context, candidates) = CASES[9];
    let baseline = scorer.score(context, candidates).unwrap();

    let mut cancelled_after = None;
    let mut full_ms = 0.0f64;
    for attempt in 0..20 {
        scorer.clear_cache();
        let t = Instant::now();
        let full = scorer.score(context, candidates).unwrap();
        full_ms = t.elapsed().as_secs_f64() * 1000.0;
        assert!(max_abs_diff(&baseline, &full) <= tolerance());

        scorer.clear_cache();
        let canceller = {
            let scorer = Arc::clone(&scorer);
            // 在一次完整打分的大约 1/3 处取消
            let delay = Duration::from_secs_f64(full_ms / 3000.0);
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                scorer.cancel();
            })
        };
        let t = Instant::now();
        let result = scorer.score(context, candidates);
        let elapsed = t.elapsed().as_secs_f64() * 1000.0;
        canceller.join().unwrap();
        match result {
            Err(LlmError::Cancelled) => {
                cancelled_after = Some((attempt, elapsed));
                break;
            }
            Ok(_) => continue, // 取消线程被调度得太晚，重试
            Err(other) => panic!("意外错误: {other:?}"),
        }
    }
    let (attempt, elapsed) = cancelled_after.expect("20 次尝试都没能在推理中途取消");
    println!("[cancel] aborted after {elapsed:.1} ms (uncancelled call: {full_ms:.1} ms, attempt {attempt})");
    assert!(elapsed < full_ms, "被取消的调用应当提前返回: {elapsed:.1} ms vs {full_ms:.1} ms");

    // 取消之后的请求不受影响，结果正确
    let after = scorer.score(context, candidates).unwrap();
    assert!(max_abs_diff(&baseline, &after) <= tolerance(), "{baseline:?} vs {after:?}");
}

#[test]
fn timing_and_footprint_report() {
    let Some((_guard, shared)) = shared() else { return };
    let scorer = &shared.scorer;
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.total_cmp(b));
        v[v.len() / 2]
    };
    let (mut cold, mut warm, mut naive) = (Vec::new(), Vec::new(), Vec::new());
    for (context, candidates) in CASES {
        let (mut c, mut w, mut n) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..3 {
            scorer.clear_cache();
            let t = Instant::now();
            scorer.score(context, candidates).unwrap();
            c.push(t.elapsed().as_secs_f64() * 1000.0);
            let t = Instant::now();
            scorer.score(context, candidates).unwrap();
            w.push(t.elapsed().as_secs_f64() * 1000.0);
            let t = Instant::now();
            scorer.score_naive(context, candidates).unwrap();
            n.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        cold.push(median(c));
        warm.push(median(w));
        naive.push(median(n));
    }
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let max = |v: &[f64]| v.iter().copied().fold(0.0, f64::max);
    println!("[load] {:.1} ms, limits {:?}", shared.load_ms, scorer.limits());
    println!(
        "[time] {} cases, per-case median of 3: naive mean {:.1} ms (max {:.1}) | batched cold mean {:.1} ms (max {:.1}) | batched same-context mean {:.1} ms (max {:.1})",
        CASES.len(),
        mean(&naive),
        max(&naive),
        mean(&cold),
        max(&cold),
        mean(&warm),
        max(&warm),
    );
    let now = phys_footprint_bytes();
    println!(
        "[mem] phys_footprint: before load {:.1} MiB | after load {:.1} MiB | now {:.1} MiB (test process incl. harness)",
        mib(shared.footprint_before),
        mib(shared.footprint_loaded),
        mib(now),
    );
    assert!(now.is_some_and(|b| b > 0));
    // 批量路径必须明显快于逐候选参考实现
    assert!(mean(&cold) < mean(&naive), "cold {:.1} ms vs naive {:.1} ms", mean(&cold), mean(&naive));
}

/// ggml 在一次解码达到 64 个 token 时换注意力内核。前缀约 62 个 token 时，「每个候选从头解码」的一次解码
/// 会越过这条线，而接着缓存算的不会；所有解码都压在 63 个 token 以内之后，两条路径才用同一套内核。
#[test]
fn long_prefix_scores_do_not_depend_on_how_the_prefix_was_decoded() {
    let Some((_guard, shared)) = shared() else { return };
    let scorer = &shared.scorer;
    let tol = tolerance();
    // 这个模型在短上文上是否逐位一致（Qwen3-0.6B 是；Qwen2.5-0.5B 在 macOS 本机构建下不是）
    let mut d_short = 0.0f64;
    for (context, candidates) in &CASES[..3] {
        let naive = scorer.score_naive(context, candidates).unwrap();
        scorer.clear_cache();
        d_short = d_short.max(max_abs_diff(&naive, &scorer.score(context, candidates).unwrap()));
    }

    // 48 个表情符号（每个至少一个 token）：上文正好填满 48 个字符的窗口
    let context = "😀".repeat(48);
    let candidates = ["然后回家", "燃后回家", "然后会家", "然后回嫁"];
    let naive = scorer.score_naive(&context, &candidates).unwrap();
    scorer.clear_cache();
    let cold = scorer.score(&context, &candidates).unwrap();
    let stats = scorer.last_stats();
    assert!(
        stats.common_prefix_tokens >= 60,
        "这个用例要求前缀接近 64 个 token（换了分词差别很大的模型时需要换一段上文）: {stats:?}"
    );
    let warm = scorer.score(&context, &candidates).unwrap();
    let d_long = max_abs_diff(&naive, &cold).max(max_abs_diff(&naive, &warm));
    println!(
        "[consistency] {}-token prefix: max |batched - naive| {d_long:.5} nats (short contexts: {d_short:.5} nats)",
        stats.common_prefix_tokens
    );
    assert!(d_long <= tol, "长前缀差值 {d_long:.4} > {tol}\nnaive {naive:?}\ncold  {cold:?}\nwarm  {warm:?}");
    if d_short == 0.0 {
        assert_eq!(d_long, 0.0, "短上文逐位一致的模型，长前缀也必须逐位一致\nnaive {naive:?}\ncold  {cold:?}\nwarm  {warm:?}");
    }
}

/// 公共前缀分块解码：在一段较长的新前缀算到一半时取消，算完的块留在缓存里，下一次请求接着算。
/// （整段前缀一次解码时，被取消的请求一个 token 也留不下，用户敲得比前缀的解码时间快就永远没有结果。）
#[test]
fn cancel_during_a_long_cold_prefix_keeps_the_finished_chunks() {
    let Some((_guard, shared)) = shared() else { return };
    let scorer = Arc::clone(&shared.scorer);
    let context = "今天上午我们在公司开了一个很长的会议，主要讨论下个季度的产品计划和人员安排，大家都发表了看法";
    let candidates = ["然后回家", "燃后回家", "然后会家"];
    scorer.clear_cache();
    let baseline = scorer.score(context, &candidates).unwrap();
    let cold = scorer.last_stats();
    let full_prefix = cold.common_prefix_tokens;
    assert_eq!(cold.prefix_tokens_decoded, full_prefix);
    assert!(full_prefix > 2 * PREFIX_CHUNK_TOKENS, "前缀要超过两块: {cold:?}");
    let prefix_ms = cold.prefix_us as f64 / 1000.0;

    let mut kept = None;
    for attempt in 0..40 {
        scorer.clear_cache();
        let canceller = {
            let scorer = Arc::clone(&scorer);
            // 取消时刻分布在前缀解码的 30% ~ 90% 处
            let delay = Duration::from_secs_f64(prefix_ms * (0.3 + 0.6 * (attempt % 8) as f64 / 8.0) / 1000.0);
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                scorer.cancel();
            })
        };
        let result = scorer.score(context, &candidates);
        canceller.join().unwrap();
        let stats = scorer.last_stats();
        match result {
            Err(LlmError::Cancelled) if stats.prefix_tokens_decoded > 0 && stats.prefix_tokens_decoded < full_prefix => {
                kept = Some((attempt, stats.prefix_tokens_decoded));
                break;
            }
            // 取消得太早（第一块还没算完）或太晚（前缀已经算完）：重试
            Ok(_) | Err(LlmError::Cancelled) => continue,
            Err(other) => panic!("意外错误: {other:?}"),
        }
    }
    let (attempt, kept) = kept.expect("40 次尝试都没能在前缀解码的中途取消并留下已完成的块");
    assert_eq!(kept % PREFIX_CHUNK_TOKENS, 0, "留下的应当是整数个块");

    // 下一次请求从留下的地方接着算，结果与完整算一遍相同
    let again = scorer.score(context, &candidates).unwrap();
    let stats = scorer.last_stats();
    println!(
        "[cancel] {full_prefix}-token cold prefix ({prefix_ms:.1} ms) cancelled mid-way: {kept} tokens kept (attempt {attempt}), next call reused {} and decoded {}",
        stats.reused_tokens, stats.prefix_tokens_decoded
    );
    assert_eq!(stats.reused_tokens, kept, "{stats:?}");
    assert_eq!(stats.prefix_tokens_decoded, full_prefix - kept, "{stats:?}");
    assert!(max_abs_diff(&baseline, &again) <= tolerance(), "{baseline:?} vs {again:?}");
}

/// 没有可打分 token 的候选得 `-inf`，而不是会压过所有候选的 0
#[test]
fn candidate_without_scored_tokens_cannot_win() {
    let Some((_guard, shared)) = shared() else { return };
    let scorer = &shared.scorer;
    let context = "周末天气不错，我们";
    for candidates in [["", "去公园玩", "去公元万"], ["去公园玩", "", "去公元万"]] {
        let empty = candidates.iter().position(|c| c.is_empty()).unwrap();
        for scores in [scorer.score(context, &candidates).unwrap(), scorer.score_naive(context, &candidates).unwrap()] {
            assert_eq!(scores[empty], f64::NEG_INFINITY, "{scores:?}");
            let best = scores.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(i, _)| i).unwrap();
            assert_eq!(candidates[best], "去公园玩", "{scores:?}");
            for (i, s) in scores.iter().enumerate() {
                assert!(i == empty || (s.is_finite() && *s < 0.0), "{scores:?}");
            }
        }
    }
    // 没有任何候选可比：全 0
    assert_eq!(scorer.score(context, &["", ""]).unwrap(), vec![0.0, 0.0]);
}

/// 用户连续上屏、上文超过 48 个字符之后，新前缀仍然接得上缓存（窗口起点对齐到分句处，而不是每次向后滑）
#[test]
fn long_text_keeps_reusing_the_prefix_cache() {
    let Some((_guard, shared)) = shared() else { return };
    let scorer = &shared.scorer;
    let candidates = ["然后回家", "燃后回家", "然后会家"];
    let mut text = String::from("今天上午我们在公司开了一个很长的会议，主要讨论下个季度的产品计划和人员安排，大家都发表了自己的看法，");
    assert!(text.chars().count() > ime_llm::LM_SCORE_CONTEXT_TAIL_CHARS);
    scorer.clear_cache();
    scorer.score(&text, &candidates).unwrap();
    let mut previous = scorer.last_stats().common_prefix_tokens;
    for piece in ["最后", "决定", "先把", "手头的", "项目"] {
        text.push_str(piece);
        scorer.score(&text, &candidates).unwrap();
        let stats = scorer.last_stats();
        // 新上屏的几个字之外都来自缓存（BPE 可能把上一次的最后一个 token 与新字合并，留 2 个 token 的余量）
        assert!(
            stats.reused_tokens + 2 >= previous,
            "上屏「{piece}」之后只复用了 {} 个 token（上一次的前缀有 {previous} 个）: {stats:?}",
            stats.reused_tokens
        );
        previous = stats.common_prefix_tokens;
    }
}
