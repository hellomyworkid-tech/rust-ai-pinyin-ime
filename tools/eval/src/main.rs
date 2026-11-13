//! 端到端评测工具。
//!
//! ```text
//! cargo run --release --manifest-path tools/eval/Cargo.toml -- <仓库根目录> <模型.gguf>
//!     [--cases data/eval/context_cases.tsv] [--threads N] [--quiet]
//!     [--typing]                  额外模拟逐键输入：每个用例的拼音一个字母一个字母地敲（中间不停顿），
//!                                 统计被取消 / 跑完的打分次数与最后一键之后的等待时间
//!     [--explore]                 额外输出「候选池策略 × 决策规则」对照表（直接调用打分器）
//!     [--pools]                   只打印每个用例规则重排后的候选（文本、编码、标签）以及哪些进了候选池，
//!                                 不加载模型（模型路径仍要给，但不会被读取）；编写用例文件时用它看解码器实际给出什么
//!     [--audit <输入.tsv> [--audit-limit N]]   对一批输入统计大模型推翻规则首选的情况
//! ```
//!
//! 默认流程（每个用例）：解码 → 规则重排（大模型关闭的调度器）→ 开启大模型并安装了进程内打分器的调度器
//! `rerank` → 等待刷新回调 → 再次 `rerank` 读取重排后的列表。输出解码器 / 规则 / 规则 + 大模型三者的
//! 首选准确率、每个用例的耗时，以及打分器的 `phys_footprint`。
//!
//! 用例文件：`上文 \t 目标文本 \t 拼音`，`#` 开头为注释。`data/eval/` 下有四份：
//! - `context_cases.tsv`：50 个需要上文才能选对的用例（默认）
//! - `phrase_split_cases.tsv`：400 个对照用例，词库词组在词边界拆成「上文 + 目标」，解码器大多已经正确
//! - `phrase_nocontext_cases.tsv`：400 个无上文的对照用例（词库词组）
//! - `english_cases.tsv`：68 个英文词、缩写与中英混输的对照用例，规则重排全部正确，大模型应当一个都不改
//!
//! 中间两份由 `tools/eval/gen_control_cases.py` 从 rime-ice 基础词库生成。
//! （另有 `alternating_cases*.tsv`，由 `crates/ime-ffi/examples/ffi_session` 使用。）

use ime_ai::{
    choose_by_exact_scores, select_scoring_pool, CandidateScorer, DualModeAiOrchestrator, OpenAiCompatibleClient,
    OpenAiCompatibleConfig, ScoreError, SCORING_POOL_MAX,
};
use ime_core::{AppScene, Candidate, ImeCoreEngine};
use ime_lexicon::CompiledLexicon;
use ime_llm::{phys_footprint_bytes, LlmOptions, LlmScorer};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// 等待刷新回调的上限：超过即视为该用例没有得到大模型结果
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(60);

struct Case {
    context: String,
    target: String,
    pinyin: String,
}

struct Args {
    root: PathBuf,
    model: PathBuf,
    cases: Option<PathBuf>,
    threads: u32,
    /// 不逐个打印用例（只输出汇总）
    quiet: bool,
    typing: bool,
    explore: bool,
    /// 只打印候选池，不加载模型
    pools: bool,
    audit: Option<PathBuf>,
    audit_limit: usize,
}

fn usage() -> ! {
    eprintln!(
        "用法: eval <仓库根目录> <模型.gguf> [--cases 用例.tsv] [--threads N] [--quiet] [--typing] [--explore] [--pools] [--audit 输入.tsv [--audit-limit N]]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut args = Args {
        root: PathBuf::new(),
        model: PathBuf::new(),
        cases: None,
        threads: 0,
        quiet: false,
        typing: false,
        explore: false,
        pools: false,
        audit: None,
        audit_limit: 300,
    };
    let mut it = argv.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().unwrap_or_else(|| {
            eprintln!("参数 {name} 缺少取值");
            usage()
        });
        match arg.as_str() {
            "--cases" => args.cases = Some(PathBuf::from(value("--cases"))),
            "--threads" => args.threads = value("--threads").parse().unwrap_or_else(|_| usage()),
            "--audit" => args.audit = Some(PathBuf::from(value("--audit"))),
            "--audit-limit" => args.audit_limit = value("--audit-limit").parse().unwrap_or_else(|_| usage()),
            "--quiet" => args.quiet = true,
            "--typing" => args.typing = true,
            "--explore" => args.explore = true,
            "--pools" => args.pools = true,
            other if other.starts_with("--") => {
                eprintln!("未知参数 {other}");
                usage()
            }
            _ => positional.push(arg),
        }
    }
    if positional.len() != 2 {
        usage();
    }
    args.root = PathBuf::from(&positional[0]);
    args.model = PathBuf::from(&positional[1]);
    args
}

fn load_cases(path: &Path) -> Vec<Case> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("读取 {} 失败: {e}", path.display());
        std::process::exit(1);
    });
    let mut cases = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() != 3 {
            eprintln!("用例格式应为「上文\\t目标\\t拼音」: {line}");
            std::process::exit(1);
        }
        cases.push(Case { context: cols[0].to_string(), target: cols[1].to_string(), pinyin: cols[2].to_string() });
    }
    cases
}

/// 比较文本时忽略空白（中英混排候选带显示用空格）
fn norm(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
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

fn time_summary(label: &str, values_ms: &[f64]) {
    let mut sorted = values_ms.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let mean = sorted.iter().sum::<f64>() / sorted.len().max(1) as f64;
    println!(
        "[time] {label:<34} n={:<3} mean {mean:8.2} ms | p50 {:8.2} | p90 {:8.2} | max {:8.2}",
        sorted.len(),
        percentile(&sorted, 0.5),
        percentile(&sorted, 0.9),
        percentile(&sorted, 1.0),
    );
}

fn top_text(candidates: &[Candidate]) -> String {
    candidates.first().map(|c| c.text.clone()).unwrap_or_default()
}

/// 一个用例的端到端结果
struct Outcome {
    decoder_top: String,
    rule_top: String,
    llm_top: String,
    llm_tag: String,
    /// 是否向打分器发出了请求（否则规则 + 大模型 的结果就是规则的结果）
    llm_asked: bool,
    /// 目标是否在交给打分器的候选池内
    target_in_pool: bool,
    pool_len: usize,
    decode_ms: f64,
    rule_ms: f64,
    /// 从 `rerank` 返回到刷新回调触发
    wait_ms: f64,
    /// 打分器自己统计的本次打分耗时
    score_ms: f64,
}

fn main() {
    let args = parse_args();
    let cases_path = args.cases.clone().unwrap_or_else(|| args.root.join("data/eval/context_cases.tsv"));
    let cases = load_cases(&cases_path);

    let t = Instant::now();
    let lexicon_path = args.root.join("data/compiled/merged_lexicon.bin");
    let lexicon = CompiledLexicon::load_mmap(&lexicon_path).unwrap_or_else(|e| {
        eprintln!("加载词库 {} 失败: {e}", lexicon_path.display());
        std::process::exit(1);
    });
    let engine = ImeCoreEngine::new(lexicon);
    println!("[load] 词库 {:.0} ms", t.elapsed().as_secs_f64() * 1e3);

    if args.pools {
        dump_pools(&engine, &cases);
        return;
    }

    let footprint_start = phys_footprint_bytes();
    let options = LlmOptions { threads: args.threads, ..LlmOptions::default() };
    let t = Instant::now();
    let scorer = match LlmScorer::load(&args.model, options.clone()) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let model_load_ms = t.elapsed().as_secs_f64() * 1e3;
    let footprint_loaded = phys_footprint_bytes();
    println!(
        "[load] 模型 {} {:.0} ms，线程 {}，limits {:?}",
        scorer.model_id(),
        model_load_ms,
        if options.threads == 0 { "自动".to_string() } else { options.threads.to_string() },
        scorer.limits()
    );

    // 规则重排：大模型关闭的调度器（与 Windows 默认配置相同）
    let mut rules = DualModeAiOrchestrator::new(OpenAiCompatibleConfig::default());
    assert!(!rules.cloud_client.config.enabled);

    // 规则 + 大模型：开启大模型重排并安装进程内打分器
    let mut config = OpenAiCompatibleConfig::default();
    config.enabled = true;
    let mut ai = DualModeAiOrchestrator::new(config);
    ai.set_candidate_scorer(Some(Arc::clone(&scorer) as Arc<dyn CandidateScorer>));
    let (notify_tx, notify_rx) = mpsc::channel::<Instant>();
    let notify_tx = std::sync::Mutex::new(notify_tx);
    ai.set_async_notify_callback(Arc::new(move || {
        if let Ok(tx) = notify_tx.lock() {
            let _ = tx.send(Instant::now());
        }
    }));

    let mut outcomes = Vec::with_capacity(cases.len());
    let mut footprint_peak = footprint_loaded;
    for (id, case) in cases.iter().enumerate() {
        let t = Instant::now();
        let decoded = engine.decode(&case.pinyin, AppScene::General, &case.context);
        let decode_ms = t.elapsed().as_secs_f64() * 1e3;
        let decoder_top = top_text(&decoded.candidates);

        let t = Instant::now();
        let rule_report = rules.rerank(&case.pinyin, &case.context, AppScene::General, decoded.candidates.clone());
        let rule_ms = t.elapsed().as_secs_f64() * 1e3;
        let rule_top = top_text(&rule_report.candidates);

        // 候选池只用于报告（调度器内部用同一个函数、同一份规则重排结果选池）
        let local = rules
            .local_engine
            .rerank_candidates(&case.context, AppScene::General, decoded.candidates.clone());
        let pool = select_scoring_pool(&local.candidates);
        let target_in_pool = pool.iter().any(|c| norm(&c.text) == norm(&case.target));

        let asked_at = Instant::now();
        let first = ai.rerank(&case.pinyin, &case.context, AppScene::General, decoded.candidates.clone());
        let llm_asked = ai.inprocess_result_pending();
        let (mut wait_ms, mut score_ms) = (0.0, 0.0);
        let report = if llm_asked {
            match notify_rx.recv_timeout(NOTIFY_TIMEOUT) {
                Ok(at) => {
                    wait_ms = at.duration_since(asked_at).as_secs_f64() * 1e3;
                    score_ms = scorer.last_stats().total_us as f64 / 1e3;
                }
                Err(_) => eprintln!("[warn] 用例 {id} 等待大模型结果超时"),
            }
            ai.rerank(&case.pinyin, &case.context, AppScene::General, decoded.candidates.clone())
        } else {
            first
        };
        footprint_peak = footprint_peak.max(phys_footprint_bytes());

        let outcome = Outcome {
            decoder_top,
            rule_top,
            llm_top: top_text(&report.candidates),
            llm_tag: report.candidates.first().map(|c| c.tag.clone()).unwrap_or_default(),
            llm_asked,
            target_in_pool,
            pool_len: pool.len(),
            decode_ms,
            rule_ms,
            wait_ms,
            score_ms,
        };
        if !args.quiet {
            println!(
                "[case {id:>2}] {}|{} ({})  解码={}  规则={}  大模型={} {}  池={}{}  解码 {:.2} ms 规则 {:.3} ms 等待 {:.1} ms 打分 {:.1} ms",
                case.context,
                case.target,
                case.pinyin,
                outcome.decoder_top,
                outcome.rule_top,
                outcome.llm_top,
                outcome.llm_tag,
                outcome.pool_len,
                if outcome.target_in_pool { "" } else { "(目标不在池内)" },
                outcome.decode_ms,
                outcome.rule_ms,
                outcome.wait_ms,
                outcome.score_ms,
            );
        }
        outcomes.push(outcome);
    }
    let footprint_end = phys_footprint_bytes();

    // ---- 准确率
    let n = cases.len();
    let correct = |pick: &dyn Fn(&Outcome) -> &str| -> usize {
        cases.iter().zip(&outcomes).filter(|(c, o)| norm(pick(o)) == norm(&c.target)).count()
    };
    let decoder_ok = correct(&|o| o.decoder_top.as_str());
    let rule_ok = correct(&|o| o.rule_top.as_str());
    let llm_ok = correct(&|o| o.llm_top.as_str());
    let in_pool = outcomes.iter().filter(|o| o.target_in_pool).count();
    let asked = outcomes.iter().filter(|o| o.llm_asked).count();
    println!();
    println!("用例文件: {}（{n} 个用例）  模型: {}", cases_path.display(), scorer.model_id());
    println!("[acc] 解码器首选            {decoder_ok:>2}/{n}  {:.0}%", 100.0 * decoder_ok as f64 / n.max(1) as f64);
    println!("[acc] 规则重排首选          {rule_ok:>2}/{n}  {:.0}%", 100.0 * rule_ok as f64 / n.max(1) as f64);
    println!("[acc] 规则 + 大模型首选     {llm_ok:>2}/{n}  {:.0}%", 100.0 * llm_ok as f64 / n.max(1) as f64);
    println!("[acc] 目标在候选池内（上限） {in_pool:>2}/{n}；向打分器发出请求的用例 {asked}/{n}");

    let regressions: Vec<usize> = (0..n)
        .filter(|&i| {
            let t = norm(&cases[i].target);
            norm(&outcomes[i].rule_top) == t && norm(&outcomes[i].llm_top) != t
        })
        .collect();
    let fixes = (0..n)
        .filter(|&i| {
            let t = norm(&cases[i].target);
            norm(&outcomes[i].rule_top) != t && norm(&outcomes[i].llm_top) == t
        })
        .count();
    println!("[acc] 大模型相对规则：修正 {fixes} 个，改错 {} 个 {:?}", regressions.len(), regressions);

    println!();
    println!("规则 + 大模型 仍然错误的用例：");
    for (id, (case, o)) in cases.iter().zip(&outcomes).enumerate() {
        if norm(&o.llm_top) != norm(&case.target) {
            println!(
                "  [{id:>2}] {}|{} ({})  解码={}  规则={}  大模型={}{}",
                case.context,
                case.target,
                case.pinyin,
                o.decoder_top,
                o.rule_top,
                o.llm_top,
                if o.target_in_pool { "" } else { "  (目标不在候选池内)" },
            );
        }
    }

    // ---- 耗时与内存
    println!();
    time_summary("解码", &outcomes.iter().map(|o| o.decode_ms).collect::<Vec<_>>());
    time_summary("规则重排", &outcomes.iter().map(|o| o.rule_ms).collect::<Vec<_>>());
    let asked_outcomes: Vec<&Outcome> = outcomes.iter().filter(|o| o.llm_asked).collect();
    time_summary("大模型：rerank 返回 → 刷新回调", &asked_outcomes.iter().map(|o| o.wait_ms).collect::<Vec<_>>());
    time_summary("大模型：打分器内部耗时", &asked_outcomes.iter().map(|o| o.score_ms).collect::<Vec<_>>());
    println!(
        "[mem] 进程 phys_footprint（含词库与引擎）: 加载模型前 {:.1} MiB | 加载后 {:.1} MiB | 评测中峰值 {:.1} MiB | 结束时 {:.1} MiB",
        mib(footprint_start),
        mib(footprint_loaded),
        mib(footprint_peak),
        mib(footprint_end),
    );
    println!(
        "[mem] 打分器占用（相对加载模型前的增量）: 加载后 +{:.1} MiB | 峰值 +{:.1} MiB（模型加载 {:.0} ms）",
        mib(footprint_loaded) - mib(footprint_start),
        mib(footprint_peak) - mib(footprint_start),
        model_load_ms,
    );

    if args.typing {
        let expected: Vec<String> = outcomes.iter().map(|o| o.llm_top.clone()).collect();
        typing(&engine, &scorer, &cases, &expected);
    }
    if args.explore {
        explore(&engine, &scorer, &cases);
    }
    if let Some(path) = &args.audit {
        audit(&engine, &scorer, path, args.audit_limit);
    }
}

// ---------------------------------------------------------------------------------------------
// 候选池一览：不需要模型
// ---------------------------------------------------------------------------------------------

/// 每个用例打印规则重排后覆盖整段输入的前 16 个候选：`文本<编码>标签/段数/代价差`，进了候选池的前面带 `+`，
/// 目标前面带 `*`。最后汇总规则首选的正确数、调度器会不会为它请求大模型。
fn dump_pools(engine: &ImeCoreEngine, cases: &[Case]) {
    let mut rules = DualModeAiOrchestrator::new(OpenAiCompatibleConfig::default());
    let (mut rule_ok, mut asked) = (0usize, 0usize);
    for (id, case) in cases.iter().enumerate() {
        let decoded = engine.decode(&case.pinyin, AppScene::General, &case.context);
        let local = rules
            .local_engine
            .rerank_candidates(&case.context, AppScene::General, decoded.candidates.clone());
        let pool = select_scoring_pool(&local.candidates);
        let target = norm(&case.target);
        let top = top_text(&local.candidates);
        let ok = norm(&top) == target;
        rule_ok += usize::from(ok);
        // 与 rerank() 的前置条件相同（大模型已开启时）
        let wanted = local.candidates.first().is_some_and(|c| {
            !c.tag.starts_with("[记忆") && !(c.text.chars().count() <= 1 && !c.text.is_ascii())
        }) && (!case.context.trim().is_empty() || case.pinyin.len() > 8)
            && case.pinyin.len() >= 2
            && local.candidates.len() >= 2
            && pool.len() >= 2;
        asked += usize::from(wanted);
        let max_len = local.candidates.iter().map(|c| c.consumed_len).max().unwrap_or(0);
        let best_cost = local.candidates.first().map(|c| c.l1_cost).unwrap_or(0.0);
        let shown: Vec<String> = local
            .candidates
            .iter()
            .filter(|c| c.consumed_len == max_len)
            .take(16)
            .map(|c| {
                format!(
                    "{}{}{}<{}>{}/{}段/{:+.1}",
                    if pool.iter().any(|p| p.text == c.text) { "+" } else { "" },
                    if norm(&c.text) == target { "*" } else { "" },
                    c.text,
                    c.preedit,
                    c.tag,
                    c.segments.len(),
                    c.l1_cost - best_cost
                )
            })
            .collect();
        println!(
            "[{id:>2}] {}|{} ({})  规则首选={top}{}  池 {} 个{}",
            case.context,
            case.target,
            case.pinyin,
            if ok { "" } else { " ✗" },
            pool.len(),
            if wanted { "" } else { "（不请求大模型）" },
        );
        println!("       {}", shown.join("  "));
    }
    println!();
    println!("规则重排首选正确 {rule_ok}/{}；会请求大模型的用例 {asked}/{}", cases.len(), cases.len());
}

// ---------------------------------------------------------------------------------------------
// 逐键输入：验证更新的击键会取消在途的打分、任务不会堆积
// ---------------------------------------------------------------------------------------------

/// 给打分器套一层计数
struct CountingScorer {
    inner: Arc<LlmScorer>,
    calls: AtomicUsize,
    completed: AtomicUsize,
    cancelled: AtomicUsize,
    cancel_requests: AtomicUsize,
}

impl CandidateScorer for CountingScorer {
    fn model_id(&self) -> String {
        self.inner.model_id().to_string()
    }

    fn score(&self, context: &str, candidates: &[String]) -> Result<Vec<f64>, ScoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = CandidateScorer::score(self.inner.as_ref(), context, candidates);
        match &result {
            Ok(_) => self.completed.fetch_add(1, Ordering::SeqCst),
            Err(ScoreError::Cancelled) => self.cancelled.fetch_add(1, Ordering::SeqCst),
            Err(_) => 0,
        };
        result
    }

    fn cancel(&self) {
        self.cancel_requests.fetch_add(1, Ordering::SeqCst);
        self.inner.cancel();
    }
}

/// 每个用例的拼音逐字母输入，每敲一个字母就解码 + `rerank` 一次，中间不停顿（比真人打字快得多，
/// 是任务堆积的最坏情况）。敲完后等待刷新回调，再读取首选，应与一次性输入整串拼音的结果 `expected` 相同。
fn typing(engine: &ImeCoreEngine, scorer: &Arc<LlmScorer>, cases: &[Case], expected: &[String]) {
    println!();
    println!("==== 逐键输入（每个字母一次 rerank，中间不停顿）====");
    let counting = Arc::new(CountingScorer {
        inner: Arc::clone(scorer),
        calls: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
        cancelled: AtomicUsize::new(0),
        cancel_requests: AtomicUsize::new(0),
    });
    let mut config = OpenAiCompatibleConfig::default();
    config.enabled = true;
    let mut ai = DualModeAiOrchestrator::new(config);
    ai.set_candidate_scorer(Some(Arc::clone(&counting) as Arc<dyn CandidateScorer>));
    let (notify_tx, notify_rx) = mpsc::channel::<Instant>();
    let notify_tx = std::sync::Mutex::new(notify_tx);
    ai.set_async_notify_callback(Arc::new(move || {
        if let Ok(tx) = notify_tx.lock() {
            let _ = tx.send(Instant::now());
        }
    }));

    let mut keystrokes = 0usize;
    let mut requests = 0usize;
    let mut early_notifies = 0usize;
    let mut mismatches = Vec::new();
    let mut settle_ms = Vec::new();
    for (id, case) in cases.iter().enumerate() {
        let mut last_key = Instant::now();
        for end in 1..=case.pinyin.len() {
            let prefix = &case.pinyin[..end];
            let decoded = engine.decode(prefix, AppScene::General, &case.context);
            last_key = Instant::now();
            ai.rerank(prefix, &case.context, AppScene::General, decoded.candidates);
            keystrokes += 1;
            if ai.inprocess_result_pending() {
                requests += 1;
            }
        }
        // 敲完之前就到达的回调属于中间前缀（它们在下一键到来之前就算完了）
        let mut final_notify = None;
        if ai.inprocess_result_pending() {
            match notify_rx.recv_timeout(NOTIFY_TIMEOUT) {
                Ok(at) => final_notify = Some(at),
                Err(_) => eprintln!("[warn] 用例 {id} 等待大模型结果超时"),
            }
        }
        while let Ok(at) = notify_rx.try_recv() {
            if let Some(previous) = final_notify.replace(at) {
                let _ = previous;
                early_notifies += 1;
            }
        }
        if let Some(at) = final_notify {
            settle_ms.push(at.saturating_duration_since(last_key).as_secs_f64() * 1e3);
        }
        let decoded = engine.decode(&case.pinyin, AppScene::General, &case.context);
        let report = ai.rerank(&case.pinyin, &case.context, AppScene::General, decoded.candidates);
        let top = top_text(&report.candidates);
        if top != expected[id] {
            mismatches.push(format!("[{id}] 逐键={top} 一次性={}", expected[id]));
        }
        ai.cancel_pending_llm_work();
    }
    println!(
        "击键 {keystrokes} 次，其中需要大模型结果的 {requests} 次；打分器被调用 {} 次：跑完 {}，中途取消 {}；调度器发出取消 {} 次",
        counting.calls.load(Ordering::SeqCst),
        counting.completed.load(Ordering::SeqCst),
        counting.cancelled.load(Ordering::SeqCst),
        counting.cancel_requests.load(Ordering::SeqCst),
    );
    println!("中间前缀触发的刷新回调 {early_notifies} 次");
    time_summary("最后一键 → 刷新回调", &settle_ms);
    if mismatches.is_empty() {
        println!("逐键输入后的首选与一次性输入完全一致（{} 个用例）", cases.len());
    } else {
        println!("逐键输入后的首选与一次性输入不一致的用例 {} 个：", mismatches.len());
        for line in &mismatches {
            println!("  {line}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 探索：候选池策略 × 决策规则
// ---------------------------------------------------------------------------------------------

/// 与 `filter_plausible_candidates` 相同的代价过滤，但上限放宽到 `cap` 个
fn plausible_capped(candidates: &[Candidate], cap: usize) -> Vec<Candidate> {
    let Some(first) = candidates.first() else {
        return Vec::new();
    };
    let max_len = candidates.iter().map(|c| c.consumed_len).max().unwrap_or(0);
    let best_cost = first.l1_cost;
    let first_is_exact_word =
        first.consumed_len == max_len && first.segments.len() == 1 && !first.tag.starts_with("[纠错");
    let mut seen = HashSet::new();
    let mut out: Vec<Candidate> = Vec::new();
    for c in candidates {
        if c.consumed_len != max_len || !seen.insert(norm(&c.text)) {
            continue;
        }
        let keep = if out.is_empty() {
            true
        } else if first_is_exact_word {
            c.segments.len() == 1 && c.l1_cost <= best_cost + 4.2
        } else {
            c.l1_cost <= best_cost + 3.5
        };
        if keep {
            out.push(c.clone());
        }
        if out.len() >= cap {
            break;
        }
    }
    out
}

/// 前 `cap` 个覆盖整段输入的候选，可选剔除模糊音纠错候选（第 0 项除外）
fn full_cover(candidates: &[Candidate], cap: usize, drop_fuzzy: bool) -> Vec<Candidate> {
    let max_len = candidates.iter().map(|c| c.consumed_len).max().unwrap_or(0);
    let mut seen = HashSet::new();
    let mut out: Vec<Candidate> = Vec::new();
    for c in candidates {
        if c.consumed_len != max_len || !seen.insert(norm(&c.text)) {
            continue;
        }
        if drop_fuzzy && !out.is_empty() && c.tag.starts_with("[纠错") {
            continue;
        }
        out.push(c.clone());
        if out.len() >= cap {
            break;
        }
    }
    out
}

fn pick_with_margin(scores: &[f64], margin: f64) -> usize {
    let mut best = 0usize;
    for (i, &s) in scores.iter().enumerate() {
        if s > scores[best] {
            best = i;
        }
    }
    if best != 0 && scores[best] - scores[0] >= margin {
        best
    } else {
        0
    }
}

fn score_pool(scorer: &LlmScorer, context: &str, pool: &[Candidate]) -> Vec<f64> {
    if pool.len() < 2 {
        return vec![0.0; pool.len()];
    }
    let texts: Vec<String> = pool.iter().map(|c| c.text.clone()).collect();
    scorer.score(context, &texts).unwrap_or_else(|e| {
        eprintln!("[warn] 打分失败: {e}");
        vec![0.0; pool.len()]
    })
}

fn explore(engine: &ImeCoreEngine, scorer: &LlmScorer, cases: &[Case]) {
    println!();
    println!("==== 探索：候选池策略 × 替换首选所需的似然优势（直接调用打分器，索引 0 = 规则重排首选）====");
    let mut rules = DualModeAiOrchestrator::new(OpenAiCompatibleConfig::default());
    let margins = [0.0, 0.5, 1.0, 1.2, 2.0, 2.3, 3.0];
    type PoolFn = Box<dyn Fn(&[Candidate]) -> Vec<Candidate>>;
    let policies: Vec<(&str, PoolFn)> = vec![
        ("A 调度器实际使用的池（select_scoring_pool）", Box::new(|c| select_scoring_pool(c))),
        ("B 整句前8，不做其它过滤", Box::new(|c| full_cover(c, SCORING_POOL_MAX, false))),
        ("C 整句前8，剔除纠错候选", Box::new(|c| full_cover(c, SCORING_POOL_MAX, true))),
        ("D 按解码代价过滤，上限8", Box::new(|c| plausible_capped(c, SCORING_POOL_MAX))),
        ("E 按解码代价过滤，上限4（HTTP 路径的池）", Box::new(|c| {
            OpenAiCompatibleClient::filter_plausible_candidates(c).into_iter().take(4).collect()
        })),
        ("F 整句前4", Box::new(|c| full_cover(c, 4, false))),
        ("G 整句前16", Box::new(|c| full_cover(c, 16, false))),
    ];

    for (name, policy) in &policies {
        let mut correct = vec![0usize; margins.len()];
        let mut regress = vec![Vec::new(); margins.len()];
        let mut wrong = vec![Vec::new(); margins.len()];
        let mut in_pool = 0usize;
        let mut pool_sizes = 0usize;
        let mut detail = Vec::new();
        for (id, case) in cases.iter().enumerate() {
            let decoded = engine.decode(&case.pinyin, AppScene::General, &case.context);
            let local = rules
                .local_engine
                .rerank_candidates(&case.context, AppScene::General, decoded.candidates.clone());
            let rule_top = top_text(&local.candidates);
            let pool = policy(&local.candidates);
            let target = norm(&case.target);
            if pool.iter().any(|c| norm(&c.text) == target) {
                in_pool += 1;
            }
            pool_sizes += pool.len();
            let scores = score_pool(scorer, &case.context, &pool);
            for (m, &margin) in margins.iter().enumerate() {
                let pick = if pool.len() >= 2 {
                    pool[pick_with_margin(&scores, margin)].text.clone()
                } else {
                    rule_top.clone()
                };
                if norm(&pick) == target {
                    correct[m] += 1;
                } else {
                    wrong[m].push(format!("{id}:{pick}"));
                    if norm(&rule_top) == target {
                        regress[m].push(id);
                    }
                }
            }
            detail.push(format!(
                "    [{id:>2}] {}|{}: {}",
                case.context,
                case.target,
                pool.iter()
                    .zip(&scores)
                    .map(|(c, s)| format!(
                        "{}{}{}/{}段/代价{:+.1} {:.2}",
                        if norm(&c.text) == target { "*" } else { "" },
                        c.text,
                        if c.tag.starts_with("[纠错") { "(纠错)" } else { "" },
                        c.segments.len(),
                        c.l1_cost - pool[0].l1_cost,
                        s
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        println!();
        println!(
            "策略 {name}: 目标在池内 {in_pool}/{}，平均池大小 {:.1}",
            cases.len(),
            pool_sizes as f64 / cases.len().max(1) as f64
        );
        for (m, margin) in margins.iter().enumerate() {
            println!(
                "  阈值 {margin:>3.1} nat: {:>2}/{}  改错 {:?}  错误 [{}]",
                correct[m],
                cases.len(),
                regress[m],
                wrong[m].join(", ")
            );
        }
        if name.starts_with('A') {
            println!("  各用例得分（* 为目标）:");
            for line in detail {
                println!("{line}");
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 审计：没有标准答案的输入上，大模型推翻规则首选的情况
// ---------------------------------------------------------------------------------------------

/// 输入文件：`拼音 \t 上文`（tools/golden 的输入格式）。只统计调度器会向大模型发出请求的输入
/// （有上文，或拼音超过 8 个字节；首选不是单字；候选池不少于 2 个）。
fn audit(engine: &ImeCoreEngine, scorer: &LlmScorer, path: &Path, limit: usize) {
    println!();
    println!("==== 审计：{}（至多 {limit} 个会触发大模型的输入）====", path.display());
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("读取 {} 失败: {e}", path.display());
        std::process::exit(1);
    });
    let mut rules = DualModeAiOrchestrator::new(OpenAiCompatibleConfig::default());
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    // 均匀抽样，避免只看到文件开头的某一类输入
    let eligible: Vec<(&str, &str)> = lines
        .iter()
        .map(|l| {
            let mut parts = l.splitn(2, '\t');
            (parts.next().unwrap_or(""), parts.next().unwrap_or(""))
        })
        .filter(|(pinyin, context)| pinyin.len() >= 2 && (!context.trim().is_empty() || pinyin.len() > 8))
        .collect();
    let step = (eligible.len() / limit.max(1)).max(1);
    let mut scored = 0usize;
    let mut flips: Vec<(f64, String)> = Vec::new();
    let mut total_ms = 0.0f64;
    for (pinyin, context) in eligible.iter().step_by(step).take(limit) {
        let decoded = engine.decode(pinyin, AppScene::General, context);
        let local = rules
            .local_engine
            .rerank_candidates(context, AppScene::General, decoded.candidates.clone());
        let Some(top) = local.candidates.first() else { continue };
        if top.tag.starts_with("[记忆") || (top.text.chars().count() <= 1 && !top.text.is_ascii()) {
            continue;
        }
        let pool = select_scoring_pool(&local.candidates);
        if pool.len() < 2 {
            continue;
        }
        let t = Instant::now();
        let scores = score_pool(scorer, context, &pool);
        total_ms += t.elapsed().as_secs_f64() * 1e3;
        scored += 1;
        let best = pick_with_margin(&scores, 0.0);
        if best != 0 {
            let gain = scores[best] - scores[0];
            let applied = choose_by_exact_scores(&scores, !context.trim().is_empty()) != 0;
            flips.push((
                gain,
                format!(
                    "  +{gain:5.2} nat {} {pinyin} | 上文「{context}」: {} → {}",
                    if applied { "[采纳]" } else { "[保持]" },
                    pool[0].text,
                    pool[best].text
                ),
            ));
        }
    }
    flips.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!(
        "打分 {scored} 个输入（平均 {:.1} ms），其中最高分不是规则首选的有 {} 个：",
        total_ms / scored.max(1) as f64,
        flips.len()
    );
    for (_, line) in &flips {
        println!("{line}");
    }
    for threshold in [0.0, 0.5, 1.0, 1.2, 2.0, 2.3, 3.0, 4.0] {
        let count = flips.iter().filter(|(g, _)| *g >= threshold).count();
        println!("  阈值 {threshold:>3.1} nat 时会推翻首选: {count}/{scored}");
    }
}
