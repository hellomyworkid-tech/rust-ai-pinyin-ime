//! 进程内打分器的耗时 / 内存 / 一致性测量工具。
//!
//! ```text
//! cargo run --release -p ime-llm --features llama --example lmscore_bench -- \
//!     --model <模型.gguf> --input <用例.tsv> [--threads 4] [--ctx 256] [--outputs 8] [--parallel 8]
//!     [--reps 5] [--repack] [--no-mmap] [--verbose] [--skip-naive]
//! ```
//!
//! 用例文件每行一个用例，制表符分隔，两种格式都接受：
//! - `上文 \t 候选1 \t 候选2 ...`
//! - `编号 \t 引导语+上文 \t 候选1 \t 候选2 ...`（评测脚本导出的 lm_input.tsv）

use ime_llm::{phys_footprint_bytes, LlmOptions, LlmScorer, LM_SCORE_PROMPT_PREFIX};
use std::time::Instant;

struct Case {
    context: String,
    candidates: Vec<String>,
}

fn mib(bytes: Option<u64>) -> f64 {
    bytes.unwrap_or(0) as f64 / 1_048_576.0
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let x = p * (sorted.len() - 1) as f64;
    let a = x.floor() as usize;
    let b = (a + 1).min(sorted.len() - 1);
    sorted[a] + (sorted[b] - sorted[a]) * (x - a as f64)
}

fn summarize(label: &str, per_case_ms: &[f64]) {
    let mut sorted = per_case_ms.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let mean = sorted.iter().sum::<f64>() / sorted.len().max(1) as f64;
    println!(
        "[time] {label:<22} cases={} mean {mean:7.2} ms | p50 {:7.2} | p90 {:7.2} | max {:7.2}",
        sorted.len(),
        percentile(&sorted, 0.5),
        percentile(&sorted, 0.9),
        percentile(&sorted, 1.0),
    );
}

fn argmax(v: &[f64]) -> usize {
    let mut best = 0;
    for (i, x) in v.iter().enumerate() {
        if *x > v[best] {
            best = i;
        }
    }
    best
}

fn load_cases(path: &str) -> Vec<Case> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("读取 {path} 失败: {e}");
        std::process::exit(1);
    });
    let mut cases = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let numbered = fields.len() >= 2 && fields[0].parse::<u64>().is_ok() && fields[1].starts_with(LM_SCORE_PROMPT_PREFIX);
        let (context, rest) = if numbered {
            (fields[1][LM_SCORE_PROMPT_PREFIX.len()..].to_string(), &fields[2..])
        } else {
            (fields[0].to_string(), &fields[1..])
        };
        cases.push(Case { context, candidates: rest.iter().map(|s| s.to_string()).collect() });
    }
    cases
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut model = String::new();
    let mut input = String::new();
    let mut options = LlmOptions::default();
    let mut reps = 5usize;
    let mut skip_naive = false;
    let mut it = args.iter().skip(1);
    while let Some(arg) = it.next() {
        let mut next = || {
            it.next().cloned().unwrap_or_else(|| {
                eprintln!("参数 {arg} 缺少取值");
                std::process::exit(2);
            })
        };
        match arg.as_str() {
            "--model" => model = next(),
            "--input" => input = next(),
            "--threads" => options.threads = next().parse().unwrap_or(0),
            "--ctx" => options.context_size = next().parse().unwrap_or(256),
            "--outputs" => options.max_outputs = next().parse().unwrap_or(8),
            "--parallel" => options.max_parallel = next().parse().unwrap_or(8),
            "--gpu-layers" => options.gpu_layers = next().parse().unwrap_or(0),
            "--reps" => reps = next().parse().unwrap_or(5),
            "--repack" => options.repack_weights = true,
            "--no-mmap" => options.use_mmap = false,
            "--verbose" => options.verbose = true,
            "--skip-naive" => skip_naive = true,
            other => {
                eprintln!("未知参数 {other}");
                std::process::exit(2);
            }
        }
    }
    if model.is_empty() || input.is_empty() {
        eprintln!("用法: lmscore_bench --model M.gguf --input cases.tsv [--threads N] [--ctx 256] [--outputs 8] [--parallel 8] [--reps 5] [--repack] [--no-mmap] [--verbose] [--skip-naive]");
        std::process::exit(2);
    }

    let cases: Vec<Case> = load_cases(&input).into_iter().filter(|c| c.candidates.len() >= 2).collect();
    println!("[mem] start                 footprint {:7.1} MiB", mib(phys_footprint_bytes()));

    let t = Instant::now();
    let scorer = match LlmScorer::load(&model, options.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("[load] {load_ms:.1} ms, options {options:?}, limits {:?}", scorer.limits());
    println!("[mem] model+context loaded  footprint {:7.1} MiB", mib(phys_footprint_bytes()));
    if cases.is_empty() {
        eprintln!("没有候选数 >= 2 的用例");
        std::process::exit(1);
    }

    // ---- 首次打分
    let t = Instant::now();
    let first = scorer.score(&cases[0].context, &cases[0].candidates);
    println!(
        "[first] {:.1} ms, {:?}",
        t.elapsed().as_secs_f64() * 1000.0,
        first.as_ref().map(|v| v.len()).map_err(|e| e.to_string())
    );
    println!("[mem] after first scoring   footprint {:7.1} MiB", mib(phys_footprint_bytes()));

    // ---- 一致性：批量（冷缓存 / 热缓存）对比逐候选参考实现
    if !skip_naive {
        let mut max_cold = 0.0f64;
        let mut max_warm = 0.0f64;
        let mut flips = 0usize;
        for case in &cases {
            let naive = scorer.score_naive(&case.context, &case.candidates).expect("naive");
            scorer.clear_cache();
            let cold = scorer.score(&case.context, &case.candidates).expect("cold");
            let warm = scorer.score(&case.context, &case.candidates).expect("warm");
            for k in 0..naive.len() {
                max_cold = max_cold.max((naive[k] - cold[k]).abs());
                max_warm = max_warm.max((naive[k] - warm[k]).abs());
            }
            if argmax(&naive) != argmax(&cold) || argmax(&naive) != argmax(&warm) {
                flips += 1;
            }
        }
        println!(
            "[consistency] max |batched - naive|: cold cache {max_cold:.4} nats, warm cache {max_warm:.4} nats; top-1 flips {flips}/{}",
            cases.len()
        );
    }

    // ---- 耗时：每个用例取 reps 次的中位数
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.total_cmp(b));
        percentile(&v, 0.5)
    };
    let mut t_naive = Vec::new();
    let mut t_cold = Vec::new();
    let mut t_prompt = Vec::new();
    let mut t_repeat = Vec::new();
    let mut prefix_cold = Vec::new();
    let mut branch_cold = Vec::new();
    let mut tokenize_us = Vec::new();
    let mut prev = cases.len() - 1;
    for (ci, case) in cases.iter().enumerate() {
        let (mut a, mut b, mut c, mut d) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut p, mut br) = (Vec::new(), Vec::new());
        for _ in 0..reps.max(1) {
            if !skip_naive {
                let t = Instant::now();
                scorer.score_naive(&case.context, &case.candidates).expect("naive");
                a.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            // 冷缓存：公共前缀（引导语 + 上文）全部重新解码
            scorer.clear_cache();
            let t = Instant::now();
            scorer.score(&case.context, &case.candidates).expect("cold");
            b.push(t.elapsed().as_secs_f64() * 1000.0);
            let st = scorer.last_stats();
            p.push(st.prefix_us as f64 / 1000.0);
            br.push(st.branch_us as f64 / 1000.0);
            tokenize_us.push(st.tokenize_us as f64);
            // 上一次调用是另一个用例：只有引导语部分能复用
            scorer.score(&cases[prev].context, &cases[prev].candidates).expect("other");
            let t = Instant::now();
            scorer.score(&case.context, &case.candidates).expect("prompt-cached");
            c.push(t.elapsed().as_secs_f64() * 1000.0);
            // 上一次调用是同一上文（连续击键的常见情形）：整个前缀都在缓存里
            let t = Instant::now();
            scorer.score(&case.context, &case.candidates).expect("repeat");
            d.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        if !skip_naive {
            t_naive.push(median(a));
        }
        t_cold.push(median(b));
        t_prompt.push(median(c));
        t_repeat.push(median(d));
        prefix_cold.push(median(p));
        branch_cold.push(median(br));
        prev = ci;
    }
    if !skip_naive {
        summarize("naive", &t_naive);
    }
    summarize("batched, cold cache", &t_cold);
    summarize("  prefix part", &prefix_cold);
    summarize("  branch part", &branch_cold);
    summarize("batched, prompt cached", &t_prompt);
    summarize("batched, same context", &t_repeat);
    println!(
        "[time] tokenize mean {:.0} us per call",
        tokenize_us.iter().sum::<f64>() / tokenize_us.len().max(1) as f64
    );
    println!("[mem] after all scoring     footprint {:7.1} MiB", mib(phys_footprint_bytes()));
}
