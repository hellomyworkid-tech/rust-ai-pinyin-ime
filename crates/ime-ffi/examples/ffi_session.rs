//! 会话模拟：像用户一样连续输入、看候选、上屏，让个人记忆不断积累，统计首选正确率随轮次的变化。
//! 走的是 iOS 键盘用的同一层封装（`ImeEngine`），所以学习路径与真实键盘一致。
//!
//! 用法：
//!   cargo run --release -q -p ime-ffi [--features llama] --example ffi_session -- \
//!       [--lexicon <文件>] [--model <gguf>] [--rounds N] [--fresh] [--skew N] [--teach N] [--verbose] \
//!       [--old-policy] [--no-context-memory] \
//!       [--chooser-model <gguf> [--chooser-noise P] [--chooser-seed N] [--chooser-policy blocked|contested|always|taught]] \
//!       <用例.tsv> [<用例2.tsv> ...]
//!
//! 用例文件每行 `上文<TAB>目标<TAB>拼音`（`#` 开头为注释），如 data/eval/alternating_cases.tsv。
//! 给出多个文件时依次处理，引擎（连同个人记忆）不换：先在第一组上形成记忆，再看换了上文的第二组。
//! `--skew N` 换一种输入方式：同一拼音的各个用例里，先把第一个连续输入 N 遍（养成习惯），
//! 再把其余的各输入一遍，最后回到第一个，分别统计「不常用的词」和「回到常用词」的正确率。
//! `--teach N` 按文件顺序输入一遍，每 N 行一组（用于 data/eval/habit_neutral_cases.tsv），按每组第 1 行的结果分开统计：
//!   第 1 行首选不对、用户点选了目标的组才算教过，看换了上文之后它还在不在首位；第 1 行本来就对（空格上屏，
//!   没有主动改选）的组没教过，单列；目标不在候选里的组教不了。组里目标和第 1 行不同的行是反向行
//!   （data/eval/habit_reversal_cases.tsv 用 `--teach 3`：教过之后，上文明确要另一个词的地方还选不选它），
//!   单独统计。结果只有一行（含「教会」二字），见 `run_teach`。
//! 默认方式下每一轮按不同的顺序把全部用例输入一遍。对每个用例：
//!   1. 把上文设为光标前文本，逐个字母输入拼音；装了模型就等它的结果；
//!   2. 首选等于目标记为正确，按空格上屏（默认首选上屏）；
//!   3. 首选不对：在候选里找到目标并点选（用户主动改选）；候选里没有目标就取消输入。
//! `--fresh` 每个用例都换一个新引擎（没有任何个人记忆），作为对照。只用于默认方式：
//!   `--teach`、`--skew` 量的就是个人记忆，和 `--fresh` 一起用会报错。
//! `--old-policy` 用旧的做法作对照：个人记忆首选挡住模型，也没有上文相关的纠正记忆。
//!   用了 `--chooser-model` 而没给 `--chooser-policy` 时，选词器也按 blocked 处理（个人记忆首选不请求选词器）。
//! `--no-context-memory` 只关掉上文相关的纠正记忆。
//! `--chooser-model <gguf>` 不把模型装进引擎，而是用它扮演「宿主选词器」（Apple 端侧模型那条路）：
//!   只回答选哪个候选，不给分数。不能和 `--model` 一起用（引擎里装了本地模型就只用它，不会问选词器）。
//!   `--chooser-noise P` 让它以概率 P 答错（模拟准确率较低的模型）。答不答错、错成哪个由请求本身
//!   （种子、上文、拼音、有哪些候选）的哈希决定，同样的请求总是同样的答案，见 `noise_draws`；
//!   `--chooser-seed N` 换种子（默认 0）。
//!   `--chooser-policy blocked|contested|always|taught` 选择个人记忆首选遇到选词器时的处理方式（默认 taught）。

use ime_ffi::{ImeEngine, IME_KEY_CHAR, IME_KEY_SPACE};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Case {
    context: String,
    target: String,
    pinyin: String,
}

struct Options {
    lexicon: PathBuf,
    model: Option<PathBuf>,
    rounds: usize,
    fresh: bool,
    verbose: bool,
    skew: usize,
    teach: usize,
    old_policy: bool,
    no_context_memory: bool,
    chooser_model: Option<PathBuf>,
    chooser_noise: f64,
    chooser_seed: u64,
    chooser_policy: Option<String>,
    cases: Vec<PathBuf>,
}

fn parse_args() -> Options {
    let mut options = Options {
        lexicon: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/compiled/merged_lexicon.bin"),
        model: None,
        rounds: 4,
        fresh: false,
        verbose: false,
        skew: 0,
        teach: 0,
        old_policy: false,
        no_context_memory: false,
        chooser_model: None,
        chooser_noise: 0.0,
        chooser_seed: 0,
        chooser_policy: None,
        cases: Vec::new(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--lexicon" => options.lexicon = PathBuf::from(args.next().expect("--lexicon 需要路径")),
            "--model" => options.model = Some(PathBuf::from(args.next().expect("--model 需要路径"))),
            "--rounds" => options.rounds = args.next().and_then(|v| v.parse().ok()).expect("--rounds 需要数字"),
            "--fresh" => options.fresh = true,
            "--verbose" => options.verbose = true,
            "--old-policy" => options.old_policy = true,
            "--no-context-memory" => options.no_context_memory = true,
            "--chooser-model" => options.chooser_model = Some(PathBuf::from(args.next().expect("--chooser-model 需要路径"))),
            "--chooser-noise" => options.chooser_noise = args.next().and_then(|v| v.parse().ok()).expect("--chooser-noise 需要 0~1 的数"),
            "--chooser-seed" => options.chooser_seed = args.next().and_then(|v| v.parse().ok()).expect("--chooser-seed 需要非负整数"),
            "--chooser-policy" => options.chooser_policy = Some(args.next().expect("--chooser-policy 需要取值")),
            "--teach" => options.teach = args.next().and_then(|v| v.parse().ok()).expect("--teach 需要数字"),
            "--skew" => options.skew = args.next().and_then(|v| v.parse().ok()).expect("--skew 需要数字"),
            _ => options.cases.push(PathBuf::from(arg)),
        }
    }
    if options.cases.is_empty() {
        eprintln!("缺少用例文件");
        std::process::exit(2);
    }
    // 不支持的组合直接报错，免得结果和开头打印的说明对不上
    if options.fresh && (options.teach > 0 || options.skew > 0) {
        eprintln!("--fresh 只用于默认的多轮方式：--teach 和 --skew 量的就是个人记忆，始终用同一个引擎");
        std::process::exit(2);
    }
    if options.model.is_some() && options.chooser_model.is_some() {
        eprintln!("--model 和 --chooser-model 不能一起用：引擎里装了本地模型就只用它打分，不会问宿主选词器");
        std::process::exit(2);
    }
    options
}

fn load_cases(path: &Path) -> Vec<Case> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("读不了 {}: {e}", path.display());
        std::process::exit(1);
    });
    text.lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut parts = line.split('\t');
            Some(Case {
                context: parts.next()?.to_string(),
                target: parts.next()?.trim().to_string(),
                pinyin: parts.next()?.trim().to_string(),
            })
        })
        .collect()
}

/// 用一个模型扮演宿主选词器：收到请求后在自己的线程上给候选打分，只把最高分的那个（或按噪声答错）送回去
fn install_simulated_chooser(engine: &std::sync::Arc<ImeEngine>, options: &Options, model: &Path) {
    use ime_ffi::ChoiceEvent;
    use ime_llm::{LlmOptions, LlmScorer};
    let scorer = LlmScorer::load(model, LlmOptions::default()).unwrap_or_else(|e| {
        eprintln!("加载选词器模型失败: {e}");
        std::process::exit(1);
    });
    let (tx, rx) = std::sync::mpsc::channel::<(u64, String, String, Vec<String>)>();
    let tx = std::sync::Mutex::new(tx);
    engine.set_chooser(Some(std::sync::Arc::new(move |event: ChoiceEvent<'_>| {
        if let ChoiceEvent::Request { id, context, pinyin, candidates } = event {
            let _ = tx
                .lock()
                .unwrap()
                .send((id, context.to_string(), pinyin.to_string(), candidates.to_vec()));
        }
    })));
    let weak = std::sync::Arc::downgrade(engine);
    let (noise, seed) = (options.chooser_noise, options.chooser_seed);
    std::thread::spawn(move || {
        while let Ok((id, context, pinyin, candidates)) = rx.recv() {
            let Some(engine) = weak.upgrade() else { break };
            let answer = match scorer.score(&context, &candidates) {
                Ok(scores) => {
                    let mut best = 0usize;
                    for (i, s) in scores.iter().enumerate() {
                        if *s > scores[best] {
                            best = i;
                        }
                    }
                    if candidates.len() > 1 {
                        // 噪声只看候选有哪些，不看顺序：个人记忆的时间只记到秒，强度相同的两个词谁排在前面，
                        // 要看它们是不是在同一秒里上屏的，候选的顺序会随运行的快慢变
                        let mut sorted: Vec<&str> = candidates.iter().map(String::as_str).collect();
                        sorted.sort_unstable();
                        let (wrong, which) = noise_draws(seed, &context, &pinyin, &sorted);
                        let others: Vec<&str> = sorted.into_iter().filter(|c| *c != candidates[best]).collect();
                        if wrong < noise && !others.is_empty() {
                            // 答错：在其余候选里挑一个
                            let pick = others[((which * others.len() as f64) as usize).min(others.len() - 1)];
                            best = candidates.iter().position(|c| c == pick).unwrap_or(best);
                        }
                    }
                    Some(candidates[best].clone())
                }
                Err(_) => None,
            };
            engine.deliver_choice(id, answer.as_deref());
        }
    });
}

/// 选词器噪声用的两个 [0, 1) 均匀数（第一个定答不答错，第二个定错成哪个），由「种子 + 上文 + 拼音 + 候选」的哈希算出。
///
/// 不用一条共享的伪随机数序列：每个请求都会抽签，包括被后来的按键作废的半截拼音请求，而这样的请求有几个能到达
/// 选词器取决于线程调度，共享序列会被它们错开，之后每一次抽签都跟着变，同样的输入两次运行结果不同。
/// 现在每个请求的抽签只看它自己：同样的请求不管什么时候到、前面来过多少别的请求，答案都一样。
/// 这也意味着同一个请求再出现时（下一轮输入同一条用例、候选也没变）答案不变：模拟的是一个在固定的那部分请求上
/// 答错的模型，而不是每次重新抽签
fn noise_draws(seed: u64, context: &str, pinyin: &str, candidates: &[&str]) -> (f64, f64) {
    // 64 位 FNV-1a：实现固定，不随 Rust 版本变化（std 的 DefaultHasher 不保证这一点）。
    // 每段后面跟一个 UTF-8 里不会出现的 0xFF 字节，不同的切分不会得到同样的字节串
    let mut hash = 0xCBF2_9CE4_8422_2325u64 ^ splitmix64(seed);
    for part in [context, pinyin].into_iter().chain(candidates.iter().copied()) {
        for &byte in part.as_bytes().iter().chain(std::iter::once(&0xFFu8)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    let first = splitmix64(hash);
    let second = splitmix64(first);
    let unit = |bits: u64| (bits >> 11) as f64 / (1u64 << 53) as f64;
    (unit(first), unit(second))
}

/// splitmix64 的一步：把相近的输入打散成互不相关的 64 位数
fn splitmix64(value: u64) -> u64 {
    let mut z = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// 要设给引擎的 chooser_memory 和它的来由；`None` 表示不设，用引擎的默认值（taught）。
/// 旧做法（`--old-policy`）里个人记忆首选不请求任何模型，所以有选词器、又没有明说策略时按 blocked
fn chooser_policy_override(options: &Options) -> Option<(&str, &'static str)> {
    match &options.chooser_policy {
        Some(policy) => Some((policy.as_str(), "--chooser-policy")),
        None if options.old_policy && options.chooser_model.is_some() => Some(("blocked", "--old-policy")),
        None => None,
    }
}

fn open_engine(options: &Options) -> std::sync::Arc<ImeEngine> {
    let engine = ImeEngine::open(&options.lexicon, None).unwrap_or_else(|message| {
        eprintln!("{message}");
        std::process::exit(1);
    });
    let engine = std::sync::Arc::new(engine);
    if let Some(model) = &options.chooser_model {
        install_simulated_chooser(&engine, options, model);
    }
    if let Some(model) = &options.model {
        if let Err(message) = engine.load_llm(model, 0) {
            eprintln!("加载模型失败: {message}");
            std::process::exit(1);
        }
    }
    let settings = format!(
        r#"{{"context_memory":{},"memory_blocks_model":{}}}"#,
        !(options.old_policy || options.no_context_memory),
        options.old_policy
    );
    engine.configure(&settings).expect("configure");
    if let Some((policy, _)) = chooser_policy_override(options) {
        engine
            .configure(&format!(r#"{{"chooser_memory":"{policy}"}}"#))
            .unwrap_or_else(|e| {
                eprintln!("{e}");
                std::process::exit(2);
            });
    }
    engine
}

fn strip(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 第 `round` 轮的输入顺序：每轮不同，但可重复（不依赖随机数）
fn order(count: usize, round: usize) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..count).collect();
    // 乘一个与 count 互素的步长再平移，得到一个确定的排列
    let step = [1usize, 37, 53, 71, 89, 101, 113, 127][round % 8];
    let step = (0..).map(|k| step + k).find(|s| gcd(*s, count.max(1)) == 1).unwrap_or(1);
    for (position, slot) in indices.iter_mut().enumerate() {
        *slot = (position * step + round * 17) % count.max(1);
    }
    indices
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

struct Outcome {
    correct: bool,
    top: String,
    tag: String,
    /// 目标在候选里的位置（全部候选里的下标）；`None` 表示候选里没有
    target_index: Option<usize>,
    waited: Duration,
}

fn run_case(engine: &ImeEngine, case: &Case, has_model: bool) -> Outcome {
    engine.reset();
    engine.set_context(Some(&case.context));
    for ch in case.pinyin.chars() {
        engine.key(IME_KEY_CHAR, ch as u32);
    }
    let start = Instant::now();
    if has_model {
        while engine.ai_result_pending() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_micros(500));
        }
    }
    let waited = start.elapsed();
    let view: Value = serde_json::from_str(&engine.refresh_after_ai()).unwrap_or_default();
    let target = strip(&case.target);
    let first = &view["visible_candidates"][0];
    let top = first["text"].as_str().unwrap_or("").to_string();
    let tag = first["tag"].as_str().unwrap_or("").to_string();
    let correct = strip(&top) == target;

    // 找目标的位置：像键盘的候选栏那样用 candidates 往后取，引擎一直停在第 0 页
    let total = view["total_candidates"].as_u64().unwrap_or(0) as usize;
    let page_size = view["page_size"].as_u64().unwrap_or(30).max(1) as usize;
    let mut target_index = view["visible_candidates"].as_array().and_then(|candidates| {
        candidates
            .iter()
            .position(|c| strip(c["text"].as_str().unwrap_or("")) == target)
    });
    let mut offset = page_size;
    while target_index.is_none() && offset < total {
        let more: Value = serde_json::from_str(&engine.candidates(offset, page_size)).unwrap_or_default();
        let Some(candidates) = more["candidates"].as_array().filter(|c| !c.is_empty()) else {
            break;
        };
        target_index = candidates
            .iter()
            .position(|c| strip(c["text"].as_str().unwrap_or("")) == target)
            .map(|found| offset + found);
        offset += candidates.len();
    }

    // 像用户一样上屏
    match target_index {
        Some(0) => {
            engine.key(IME_KEY_SPACE, 0);
        }
        Some(index) => {
            engine.select_candidate(index);
            // 选中的若只是前缀（不应发生：目标覆盖整串），把剩下的整句上屏，避免残留输入
            if engine.is_composing() {
                engine.commit_full_sentence();
            }
        }
        None => engine.reset(),
    }
    Outcome {
        correct,
        top,
        tag,
        target_index,
        waited,
    }
}

fn describe_wrong(case: &Case, outcome: &Outcome) -> String {
    format!(
        "    {}|{} ({})  首选={} {}  目标位置={}",
        case.context,
        case.target,
        case.pinyin,
        outcome.top,
        outcome.tag,
        outcome
            .target_index
            .map(|i| (i + 1).to_string())
            .unwrap_or_else(|| "不在候选里".to_string())
    )
}

/// 默认方式：每轮把全部用例按不同顺序输入一遍
fn run_rounds(options: &Options, engine: &mut std::sync::Arc<ImeEngine>, cases: &[Case], has_model: bool) {
    let mut summary = Vec::new();
    for round in 0..options.rounds {
        let mut correct = 0usize;
        let mut memory_top_wrong = 0usize;
        let mut not_in_candidates = 0usize;
        let mut waited_total = Duration::ZERO;
        let mut wrong_lines = Vec::new();
        for index in order(cases.len(), round) {
            let case = &cases[index];
            if options.fresh {
                *engine = open_engine(options);
            }
            let outcome = run_case(engine, case, has_model);
            waited_total += outcome.waited;
            if outcome.correct {
                correct += 1;
            } else {
                if outcome.tag.starts_with("[记忆") {
                    memory_top_wrong += 1;
                }
                if outcome.target_index.is_none() {
                    not_in_candidates += 1;
                }
                wrong_lines.push(describe_wrong(case, &outcome));
            }
        }
        println!(
            "第 {} 轮：首选正确 {}/{} ({:.1}%)；错误里首选是个人记忆的 {} 个，目标不在候选里的 {} 个{}",
            round + 1,
            correct,
            cases.len(),
            100.0 * correct as f64 / cases.len().max(1) as f64,
            memory_top_wrong,
            not_in_candidates,
            if has_model {
                format!("；平均等模型 {:.1} ms", waited_total.as_secs_f64() * 1000.0 / cases.len().max(1) as f64)
            } else {
                String::new()
            }
        );
        if options.verbose {
            for line in &wrong_lines {
                println!("{line}");
            }
        }
        summary.push(correct);
    }
    println!(
        "各轮正确数：{}",
        summary.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(" → ")
    );
}

/// `--teach` 里一组按第 1 行的结果归的类
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TeachClass {
    /// 首选不对、目标在候选里，用户点选目标（主动改选一次）
    Taught,
    /// 首选本来就是目标，用户按空格上屏，没有主动改选：这一组量的是模型自己的偏好，不是教会的记忆
    Untaught,
    /// 目标不在候选里
    Unteachable,
}

impl TeachClass {
    fn of(first: &Outcome) -> Self {
        match first.target_index {
            Some(0) => Self::Untaught,
            Some(_) => Self::Taught,
            None => Self::Unteachable,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Taught => "教过",
            Self::Untaught => "没教过",
            Self::Unteachable => "教不了",
        }
    }
}

/// `--teach` 的计数，和输入、打印分开（单元测试直接喂结果）。
/// 其余行分两种：目标和第 1 行相同的是保持行，量教会的词换了上文还在不在首位；目标不同的是反向行，
/// 量上文明确指向另一个词时还选不选它（记忆太强就会错），按所在组的归类单独计数，不算进任何保持的数
#[derive(Default)]
struct TeachTally {
    groups: usize,
    before_ok: usize,
    taught: usize,
    taught_first_ok: usize,
    taught_all_ok: usize,
    untaught: usize,
    untaught_ok: usize,
    untaught_n: usize,
    unteachable: usize,
    /// 旧口径：所有组的保持行
    later_ok: usize,
    later_n: usize,
    /// 一组里最多有几个保持行（「其余 N 行全在首位」里的 N）
    kept_rows: usize,
    /// 反向行的（首选正确数，行数），下标是所在组的归类：教过、没教过、教不了
    reversal: [(usize, usize); 3],
}

/// `--teach` 的一组里，目标和第 1 行不同的行是反向行
fn is_reversal(first_target: &str, target: &str) -> bool {
    target != first_target
}

impl TeachTally {
    /// 记下一组。`first_target` 是第 1 行的目标，`later` 是其余各行的（目标，首选对不对），按输入顺序
    fn add(&mut self, class: TeachClass, first_correct: bool, first_target: &str, later: &[(&str, bool)]) {
        self.groups += 1;
        self.before_ok += first_correct as usize;
        let kept: Vec<bool> = later
            .iter()
            .filter(|(target, _)| !is_reversal(first_target, target))
            .map(|&(_, ok)| ok)
            .collect();
        let kept_ok = kept.iter().filter(|ok| **ok).count();
        self.kept_rows = self.kept_rows.max(kept.len());
        self.later_n += kept.len();
        self.later_ok += kept_ok;
        let slot = &mut self.reversal[class as usize];
        for &(_, ok) in later.iter().filter(|(target, _)| is_reversal(first_target, target)) {
            slot.0 += ok as usize;
            slot.1 += 1;
        }
        match class {
            TeachClass::Taught => {
                self.taught += 1;
                self.taught_first_ok += kept.first().copied().unwrap_or(false) as usize;
                self.taught_all_ok += (!kept.is_empty() && kept_ok == kept.len()) as usize;
            }
            TeachClass::Untaught => {
                self.untaught += 1;
                self.untaught_n += kept.len();
                self.untaught_ok += kept_ok;
            }
            TeachClass::Unteachable => self.unteachable += 1,
        }
    }

    /// 结果的那一行（含「教会」二字）。没有反向行时格式与以前完全相同
    fn line(&self) -> String {
        let mut line = format!(
            "教会之前（每组第 1 行）{}/{}；第 1 行主动改选、教过一次的 {} 组：换了上文第 1 次仍在首位 {}/{}",
            self.before_ok, self.groups, self.taught, self.taught_first_ok, self.taught
        );
        // 只有一个保持行时「全在首位」就是上一个数，不再重复
        if self.kept_rows > 1 {
            line += &format!(
                "，其余 {} 行全在首位（中途没再纠正）{}/{}",
                self.kept_rows, self.taught_all_ok, self.taught
            );
        }
        line += &format!(
            "；第 1 行本来就对、没教过的 {} 组：其余行 {}/{}；目标不在候选里、教不了的 {} 组；\
             旧口径（所有组的其余行合计）{}/{}",
            self.untaught, self.untaught_ok, self.untaught_n, self.unteachable, self.later_ok, self.later_n
        );
        let [taught, untaught, unteachable] = self.reversal;
        if taught.1 + untaught.1 + unteachable.1 > 0 {
            line += &format!(
                "；反向行（目标换成同音的另一个词，上文明确指向它）：教过的组 {}/{}，没教过的组 {}/{}",
                taught.0, taught.1, untaught.0, untaught.1
            );
            if unteachable.1 > 0 {
                line += &format!("，教不了的组 {}/{}", unteachable.0, unteachable.1);
            }
        }
        line
    }
}

/// `--teach N`：按文件顺序输入一遍并像用户一样上屏，每 N 行一组（同一拼音、同一个目标词、N 个不同的中性上文，
/// 见 tools/eval/gen_habit_cases.py）。每组按第 1 行的结果归类（见 [`TeachClass`]）：教过、没教过、教不了。
///
/// 其余行首选不对时照样点选目标（用户再纠正一次），会话和真实使用一样；但再纠正过之后，后面几行量的就是
/// 教了两三次之后的保持。所以教过的组只报两个数：换了上文第 1 次仍在首位的组数，和其余行全在首位
/// （中途没再纠正，只教了一次）的组数。没教过的组单列其余行；所有组其余行的合计是旧口径，只作对照。
///
/// 组里目标和第 1 行不同的行是反向行（data/eval/habit_reversal_cases.tsv：每组第 3 行换成同音的另一个词，
/// 上文明确指向它）：不算进上面任何一个数，按所在组的归类另报首选正确数。和保持行一样，首选不对时照样点选目标。
/// 反向行最好放在组的最后：放在中间的话，它后面的保持行是在用户选过另一个词之后量的。
/// 结果打印成一行（含「教会」二字，在日志里按它就能找到这一行）
fn run_teach(options: &Options, engine: &ImeEngine, cases: &[Case], has_model: bool) {
    let size = options.teach.max(1);
    let mut tally = TeachTally::default();
    let mut untaught_words = Vec::new();
    let mut unteachable_words = Vec::new();
    let mut wrong_lines = Vec::new();
    for group in cases.chunks(size) {
        if group.iter().any(|c| c.pinyin != group[0].pinyin) {
            eprintln!("第 {} 组里拼音不一致：--teach {size} 和用例文件的分组对不上", tally.groups + 1);
        }
        let first = run_case(engine, &group[0], has_model);
        let class = TeachClass::of(&first);
        let mut later = Vec::with_capacity(group.len() - 1);
        for case in &group[1..] {
            let outcome = run_case(engine, case, has_model);
            if !outcome.correct {
                let kind = if is_reversal(&group[0].target, &case.target) { "·反向" } else { "" };
                wrong_lines.push(format!("  [{}{kind}]{}", class.label(), describe_wrong(case, &outcome)));
            }
            later.push((case.target.as_str(), outcome.correct));
        }
        match class {
            TeachClass::Untaught => untaught_words.push(group[0].target.as_str()),
            TeachClass::Unteachable => unteachable_words.push(group[0].target.as_str()),
            TeachClass::Taught => {}
        }
        tally.add(class, first.correct, &group[0].target, &later);
    }
    println!("{}", tally.line());
    if options.verbose {
        if !untaught_words.is_empty() {
            println!("  第 1 行本来就对的组：{}", untaught_words.join("、"));
        }
        if !unteachable_words.is_empty() {
            println!("  目标不在候选里的组：{}", unteachable_words.join("、"));
        }
        for line in &wrong_lines {
            println!("{line}");
        }
    }
}

/// `--skew N`：同一拼音里先把第一个用例输入 N 遍，再输入其余的，最后回到第一个
fn run_skewed(options: &Options, engine: &ImeEngine, cases: &[Case], has_model: bool) {
    let mut groups: Vec<Vec<&Case>> = Vec::new();
    for case in cases {
        match groups.iter_mut().find(|g| g[0].pinyin == case.pinyin) {
            Some(group) => group.push(case),
            None => groups.push(vec![case]),
        }
    }
    let (mut habit_ok, mut habit_n) = (0usize, 0usize);
    let (mut rare_ok, mut rare_n) = (0usize, 0usize);
    let (mut back_ok, mut back_n) = (0usize, 0usize);
    let mut wrong_lines = Vec::new();
    for group in groups.iter().filter(|g| g.len() >= 2) {
        for repeat in 0..options.skew {
            let outcome = run_case(engine, group[0], has_model);
            // 第一遍可能要靠用户改选，之后的才算「习惯」
            if repeat > 0 {
                habit_n += 1;
                habit_ok += outcome.correct as usize;
            }
        }
        for case in &group[1..] {
            let outcome = run_case(engine, case, has_model);
            rare_n += 1;
            rare_ok += outcome.correct as usize;
            if !outcome.correct {
                wrong_lines.push(format!("  [不常用]{}", describe_wrong(case, &outcome)));
            }
        }
        let outcome = run_case(engine, group[0], has_model);
        back_n += 1;
        back_ok += outcome.correct as usize;
        if !outcome.correct {
            wrong_lines.push(format!("  [回到常用]{}", describe_wrong(group[0], &outcome)));
        }
    }
    println!(
        "习惯词连续输入 {} 遍：重复时正确 {}/{}；随后不常用的词 {}/{}；再回到习惯词 {}/{}",
        options.skew, habit_ok, habit_n, rare_ok, rare_n, back_ok, back_n
    );
    if options.verbose {
        for line in &wrong_lines {
            println!("{line}");
        }
    }
}

fn main() {
    let options = parse_args();
    let has_model = options.model.is_some() || options.chooser_model.is_some();
    let stem = |path: &Path| path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    println!(
        "模型：{}，{}",
        match (&options.model, &options.chooser_model) {
            (Some(model), _) => stem(model),
            (None, Some(_)) => "引擎里不装（由下面的宿主选词器选词）".to_string(),
            (None, None) => "无（只有规则重排）".to_string(),
        },
        if options.fresh { "每个用例换新引擎（无个人记忆）" } else { "同一个引擎连续输入（个人记忆累积）" }
    );
    if let Some(model) = &options.chooser_model {
        let noise = if options.chooser_noise > 0.0 {
            format!("{}（种子 {}）", options.chooser_noise, options.chooser_seed)
        } else {
            "0".to_string()
        };
        let policy = match chooser_policy_override(&options) {
            Some((policy, source)) => format!("{policy}（{source}）"),
            None => "taught（引擎默认）".to_string(),
        };
        println!("宿主选词器由 {} 扮演，答错概率 {noise}，个人记忆首选的处理：{policy}", stem(model));
    }
    if options.old_policy {
        match chooser_policy_override(&options) {
            Some((policy, _)) if options.chooser_model.is_some() && policy != "blocked" => println!(
                "旧做法只用了一半：没有上文相关的纠正记忆，但个人记忆首选遇到宿主选词器时按 {policy} 处理，没有挡住它"
            ),
            _ => println!("旧做法：个人记忆首选挡住模型，没有上文相关的纠正记忆"),
        }
    } else if options.no_context_memory {
        println!("没有上文相关的纠正记忆");
    }
    let mut engine = open_engine(&options);
    for path in &options.cases {
        let cases = load_cases(path);
        println!("== {}（{} 个用例）", path.display(), cases.len());
        if options.teach > 0 {
            run_teach(&options, &engine, &cases, has_model);
        } else if options.skew > 0 {
            run_skewed(&options, &engine, &cases, has_model);
        } else {
            run_rounds(&options, &mut engine, &cases, has_model);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 没有反向行（habit_neutral_cases.tsv）时那一行和以前一字不差
    #[test]
    fn teach_line_without_reversal_rows_keeps_its_format() {
        let mut tally = TeachTally::default();
        let w = "誓言";
        tally.add(TeachClass::Taught, false, w, &[(w, true), (w, true), (w, false), (w, true)]);
        tally.add(TeachClass::Taught, false, w, &[(w, true); 4]);
        tally.add(TeachClass::Untaught, true, w, &[(w, true), (w, false), (w, true), (w, true)]);
        tally.add(TeachClass::Unteachable, false, w, &[(w, false); 4]);
        assert_eq!(
            tally.line(),
            "教会之前（每组第 1 行）1/4；第 1 行主动改选、教过一次的 2 组：换了上文第 1 次仍在首位 2/2，\
             其余 4 行全在首位（中途没再纠正）1/2；第 1 行本来就对、没教过的 1 组：其余行 3/4；\
             目标不在候选里、教不了的 1 组；旧口径（所有组的其余行合计）10/16"
        );
    }

    /// 目标和第 1 行不同的行（反向行）不算进保持的数，按所在组的归类另计
    #[test]
    fn reversal_rows_are_counted_apart_from_the_kept_rows() {
        let mut tally = TeachTally::default();
        let (w, c) = ("誓言", "实验");
        // 教过、第 2 行保持住了，但上文明确要另一个词时还是给了教过的词（记忆太强）
        tally.add(TeachClass::Taught, false, w, &[(w, true), (c, false)]);
        // 教过、第 2 行没保持住，另一个词选对了
        tally.add(TeachClass::Taught, false, w, &[(w, false), (c, true)]);
        tally.add(TeachClass::Taught, false, w, &[(w, true), (c, true)]);
        // 没教过：第 1 行本来就对
        tally.add(TeachClass::Untaught, true, w, &[(w, true), (c, false)]);
        assert_eq!(
            tally.line(),
            "教会之前（每组第 1 行）1/4；第 1 行主动改选、教过一次的 3 组：换了上文第 1 次仍在首位 2/3；\
             第 1 行本来就对、没教过的 1 组：其余行 1/1；目标不在候选里、教不了的 0 组；\
             旧口径（所有组的其余行合计）3/4；反向行（目标换成同音的另一个词，上文明确指向它）：教过的组 2/3，没教过的组 0/1"
        );
        // 教不了的组有反向行时也报出来
        tally.add(TeachClass::Unteachable, false, w, &[(w, false), (c, true)]);
        assert!(tally.line().ends_with("，教不了的组 1/1"), "{}", tally.line());
    }

    /// 反向行放在保持行前面时，「换了上文第 1 次」看的仍是第一个保持行
    #[test]
    fn the_first_kept_row_is_the_first_row_with_the_taught_word() {
        let mut tally = TeachTally::default();
        let (w, c) = ("誓言", "实验");
        tally.add(TeachClass::Taught, false, w, &[(c, false), (w, true), (w, false)]);
        assert_eq!((tally.taught_first_ok, tally.taught_all_ok, tally.kept_rows), (1, 0, 2));
        assert_eq!(tally.reversal, [(0, 1), (0, 0), (0, 0)]);
        assert_eq!((tally.later_ok, tally.later_n), (1, 2));
    }
}
