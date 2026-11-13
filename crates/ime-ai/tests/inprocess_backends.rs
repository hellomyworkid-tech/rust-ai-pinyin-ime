//! 调度器的进程内打分器 / 宿主选词器路径测试。全部使用假的打分器与选词器，不需要模型文件与词库。

use ime_ai::{
    CandidateScorer, ChooserDelivery, ChooserEvent, ChooserRequest, DualModeAiOrchestrator, HostChooser,
    OpenAiCompatibleConfig, ScoreError, UserMemory,
};
use ime_core::{AppScene, Candidate};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(10);
/// 用来断言「没有事件发生」的短等待
const QUIET: Duration = Duration::from_millis(150);

fn cand(text: &str, consumed_len: usize, score: f32) -> Candidate {
    Candidate {
        text: text.to_string(),
        preedit: String::new(),
        consumed_len,
        l1_cost: 100.0 - score,
        final_score: score,
        ai_boosted: false,
        tag: String::new(),
        segments: vec![text.to_string()],
        segment_spans: Vec::new(),
        syllable_spans: Vec::new(),
    }
}

/// `shiyan` 的候选：四个整词、一个模糊音纠错候选、一个只覆盖部分输入的前缀候选
fn shiyan_candidates() -> Vec<Candidate> {
    let mut fuzzy = cand("式样", 6, 95.0);
    fuzzy.tag = "[纠错 shi'yang]".to_string();
    vec![
        cand("实验", 6, 99.0),
        cand("试验", 6, 98.0),
        fuzzy,
        cand("食盐", 6, 97.0),
        cand("誓言", 6, 96.0),
        cand("是", 3, 90.0),
    ]
}

const SHIYAN_POOL: [&str; 4] = ["实验", "试验", "食盐", "誓言"];
const CONTEXT: &str = "他在婚礼上许下了";

fn enabled_orchestrator() -> (DualModeAiOrchestrator, Receiver<()>) {
    let mut config = OpenAiCompatibleConfig::default();
    config.enabled = true;
    // 指向一个不可能连通的地址：这些测试里任何 HTTP 请求都不应该发出
    config.api_base = "http://127.0.0.1:9/v1".to_string();
    let ai = DualModeAiOrchestrator::new(config);
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    ai.set_async_notify_callback(Arc::new(move || {
        let _ = tx.lock().unwrap().send(());
    }));
    (ai, rx)
}

fn rerank(ai: &mut DualModeAiOrchestrator, pinyin: &str, context: &str) -> Vec<Candidate> {
    ai.rerank(pinyin, context, AppScene::General, shiyan_candidates()).candidates
}

fn texts(candidates: &[Candidate]) -> Vec<&str> {
    candidates.iter().map(|c| c.text.as_str()).collect()
}

/// 等到调度器不再等待进程内结果（任务失败、作废或被接住的 panic 都不触发刷新回调，只能轮询）
fn wait_until_idle(ai: &DualModeAiOrchestrator) {
    let deadline = std::time::Instant::now() + WAIT;
    while ai.inprocess_result_pending() {
        assert!(std::time::Instant::now() < deadline, "调度器一直在等待进程内结果");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 同一串拼音在用户固定了前缀之后的候选（没有「誓言」）：上文与拼音不变，候选池变了
fn shiyan_candidates_other_pool() -> Vec<Candidate> {
    vec![cand("食盐", 6, 99.0), cand("试验", 6, 98.0), cand("实验", 6, 97.0), cand("是", 3, 90.0)]
}

const OTHER_POOL: [&str; 3] = ["食盐", "试验", "实验"];

// ---------------------------------------------------------------------------------------------
// 假打分器
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct GateState {
    /// 取消世代：`cancel()` 让它加一
    epoch: u64,
    /// 允许通过闸门的调用次数
    permits: usize,
}

/// 假打分器：给 `favourite` 最高分。可选地在每次打分时停在闸门前，直到测试放行或被取消。
struct FakeScorer {
    favourite: Mutex<String>,
    gated: bool,
    /// 是否响应 `cancel()`（否则模拟一个不配合取消的慢打分器）
    cooperative: bool,
    gate: Mutex<GateState>,
    gate_cv: Condvar,
    /// 每次打分开始时发送 (上文, 候选)
    started: Mutex<Sender<(String, Vec<String>)>>,
    calls: AtomicUsize,
    cancels: AtomicUsize,
    cancelled_calls: AtomicUsize,
    fail: bool,
}

impl FakeScorer {
    fn new(favourite: &str, gated: bool, cooperative: bool) -> (Arc<Self>, Receiver<(String, Vec<String>)>) {
        let (tx, rx) = mpsc::channel();
        let scorer = Arc::new(Self {
            favourite: Mutex::new(favourite.to_string()),
            gated,
            cooperative,
            gate: Mutex::new(GateState::default()),
            gate_cv: Condvar::new(),
            started: Mutex::new(tx),
            calls: AtomicUsize::new(0),
            cancels: AtomicUsize::new(0),
            cancelled_calls: AtomicUsize::new(0),
            fail: false,
        });
        (scorer, rx)
    }

    fn release(&self, n: usize) {
        self.gate.lock().unwrap().permits += n;
        self.gate_cv.notify_all();
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl CandidateScorer for FakeScorer {
    fn model_id(&self) -> String {
        "fake-model".to_string()
    }

    fn score(&self, context: &str, candidates: &[String]) -> Result<Vec<f64>, ScoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let ticket = self.gate.lock().unwrap().epoch;
        let _ = self.started.lock().unwrap().send((context.to_string(), candidates.to_vec()));
        if self.gated {
            let mut gate = self.gate.lock().unwrap();
            loop {
                if self.cooperative && gate.epoch != ticket {
                    self.cancelled_calls.fetch_add(1, Ordering::SeqCst);
                    return Err(ScoreError::Cancelled);
                }
                if gate.permits > 0 {
                    gate.permits -= 1;
                    break;
                }
                gate = self.gate_cv.wait(gate).unwrap();
            }
        }
        if self.fail {
            return Err(ScoreError::Failed("假打分器故障".to_string()));
        }
        let favourite = self.favourite.lock().unwrap().clone();
        Ok(candidates.iter().map(|c| if *c == favourite { -1.0 } else { -9.0 }).collect())
    }

    fn cancel(&self) {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        self.gate.lock().unwrap().epoch += 1;
        self.gate_cv.notify_all();
    }
}

#[test]
fn scorer_result_is_applied_cached_and_notified() {
    let (mut ai, notified) = enabled_orchestrator();
    let (scorer, started) = FakeScorer::new("誓言", false, true);
    ai.set_candidate_scorer(Some(scorer.clone()));

    // 第一次：立即返回规则重排的结果，打分在后台进行
    let first = rerank(&mut ai, "shiyan", CONTEXT);
    assert_eq!(first[0].text, "实验");
    assert!(!first[0].ai_boosted);

    // 候选池：覆盖整段输入的整词，按当前顺序；纠错候选与前缀候选不进池
    let (context, pool) = started.recv_timeout(WAIT).expect("打分器应被调用");
    assert_eq!(context, CONTEXT);
    assert_eq!(pool, SHIYAN_POOL);

    notified.recv_timeout(WAIT).expect("结果到达后应触发刷新回调");
    assert!(!ai.inprocess_result_pending());

    // 刷新：命中缓存，打分器选中的候选被提到首位
    let second = rerank(&mut ai, "shiyan", CONTEXT);
    assert_eq!(second[0].text, "誓言");
    assert!(second[0].ai_boosted);
    assert_eq!(second[0].tag, "[端侧AI 优选]");
    assert_eq!(second[1].text, "实验", "被换下的原首选紧随其后");
    assert_eq!(texts(&second).len(), texts(&first).len());

    // 再次重排仍是缓存命中：不再调用打分器，也不再触发回调
    let third = rerank(&mut ai, "shiyan", CONTEXT);
    assert_eq!(third[0].text, "誓言");
    assert!(!ai.inprocess_result_pending());
    assert!(notified.recv_timeout(QUIET).is_err());
    assert_eq!(scorer.calls(), 1);

    // 用户主动选词后清除该拼音的缓存：下次重排重新打分
    ai.invalidate_cache_for_pinyin("shiyan");
    let fourth = rerank(&mut ai, "shiyan", CONTEXT);
    assert_eq!(fourth[0].text, "实验");
    notified.recv_timeout(WAIT).expect("重新打分后再次触发回调");
    assert_eq!(scorer.calls(), 2);
}

#[test]
fn scorer_choosing_current_top_only_tags_it() {
    let (mut ai, notified) = enabled_orchestrator();
    let (scorer, _started) = FakeScorer::new("实验", false, true);
    ai.set_candidate_scorer(Some(scorer));
    rerank(&mut ai, "shiyan", CONTEXT);
    notified.recv_timeout(WAIT).unwrap();
    let after = ai.rerank("shiyan", CONTEXT, AppScene::General, shiyan_candidates());
    assert_eq!(after.candidates[0].text, "实验");
    assert!(after.candidates[0].ai_boosted);
    assert_eq!(after.candidates[0].tag, "[端侧AI 优选]");
}

#[test]
fn guards_keep_the_scorer_out() {
    let (mut ai, notified) = enabled_orchestrator();
    let (scorer, _started) = FakeScorer::new("誓言", false, true);
    ai.set_candidate_scorer(Some(scorer.clone()));

    // 无上文的短输入
    rerank(&mut ai, "shiyan", "");
    assert!(!ai.inprocess_result_pending());

    // 首选是用户记忆词、没有上文：记忆说了算
    let mut memory = shiyan_candidates();
    memory[0].tag = "[记忆·3次]".to_string();
    ai.rerank("shiyanshiyan", "", AppScene::General, memory.clone());
    assert!(!ai.inprocess_result_pending());

    // 首选是用户在同样上文下主动改选的词：有上文也不问模型
    let mut corrected = shiyan_candidates();
    corrected[0].tag = "[记忆·上文]".to_string();
    ai.rerank("shiyan", CONTEXT, AppScene::General, corrected);
    assert!(!ai.inprocess_result_pending());

    // 旧行为开关：个人记忆首选一律挡住打分器（调用方给了记忆强度也一样）
    ai.memory_blocks_scorer = true;
    ai.rerank_with_user_priors("shiyan", CONTEXT, AppScene::General, memory, &|_| plain(8.0));
    assert!(!ai.inprocess_result_pending());
    ai.memory_blocks_scorer = false;

    // 单字查询
    let singles = vec![cand("是", 3, 99.0), cand("时", 3, 98.0), cand("事", 3, 97.0)];
    ai.rerank("shi", CONTEXT, AppScene::General, singles);
    assert!(!ai.inprocess_result_pending());

    // 候选池不足 2 个（其余候选都只覆盖部分输入）
    let lonely = vec![cand("实验", 6, 99.0), cand("是", 3, 90.0), cand("时", 3, 89.0)];
    ai.rerank("shiyan", CONTEXT, AppScene::General, lonely);
    assert!(!ai.inprocess_result_pending());

    // 大模型重排关闭
    ai.cloud_client.config.enabled = false;
    rerank(&mut ai, "shiyan", CONTEXT);
    assert!(!ai.inprocess_result_pending());

    assert!(notified.recv_timeout(QUIET).is_err());
    assert_eq!(scorer.calls(), 0);
}

/// 按给定的分数表打分的假打分器（没列出的候选给 -30）
struct TableScorer {
    scores: Vec<(&'static str, f64)>,
    calls: AtomicUsize,
}

impl CandidateScorer for TableScorer {
    fn model_id(&self) -> String {
        "table-model".to_string()
    }

    fn score(&self, _context: &str, candidates: &[String]) -> Result<Vec<f64>, ScoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(candidates
            .iter()
            .map(|c| self.scores.iter().find(|(t, _)| t == c).map(|(_, s)| *s).unwrap_or(-30.0))
            .collect())
    }

    fn cancel(&self) {}
}

fn memory_top_candidates() -> Vec<Candidate> {
    let mut candidates = shiyan_candidates();
    candidates[0].tag = "[记忆·4次]".to_string();
    candidates[0].final_score = 143.0;
    candidates
}

/// 只用空格上屏形成的个人记忆（没有主动改选过）
fn plain(boost: f32) -> UserMemory {
    UserMemory { boost, explicit: false }
}

#[test]
fn memory_top_with_context_is_arbitrated_by_the_scorer_with_the_memory_as_a_prior() {
    // 模型认为「誓言」比「实验」高 1 nat
    let table = || Arc::new(TableScorer { scores: vec![("实验", -5.0), ("誓言", -4.0)], calls: AtomicUsize::new(0) });

    // 「实验」的个人记忆很强（8 → 2 nat）：保持首选，但确实问过模型
    let (mut ai, notified) = enabled_orchestrator();
    let scorer = table();
    ai.set_candidate_scorer(Some(scorer.clone()));
    let strong = |text: &str| if text == "实验" { plain(8.0) } else { UserMemory::default() };
    let first = ai
        .rerank_with_user_priors("shiyan", CONTEXT, AppScene::General, memory_top_candidates(), &strong)
        .candidates;
    assert_eq!(first[0].text, "实验");
    notified.recv_timeout(WAIT).expect("有上文时个人记忆不再挡住打分器");
    assert_eq!(scorer.calls.load(Ordering::SeqCst), 1);
    let after = ai
        .rerank_with_user_priors("shiyan", CONTEXT, AppScene::General, memory_top_candidates(), &strong)
        .candidates;
    assert_eq!(after[0].text, "实验");
    assert!(after[0].ai_boosted, "模型确认了首选");

    // 个人记忆较弱（2 → 0.5 nat）：模型的 1 nat 优势胜出，记忆词退到第二位、仍然可选
    let (mut ai, notified) = enabled_orchestrator();
    ai.set_candidate_scorer(Some(table()));
    let weak = |text: &str| if text == "实验" { plain(2.0) } else { UserMemory::default() };
    ai.rerank_with_user_priors("shiyan", CONTEXT, AppScene::General, memory_top_candidates(), &weak);
    notified.recv_timeout(WAIT).unwrap();
    let after = ai
        .rerank_with_user_priors("shiyan", CONTEXT, AppScene::General, memory_top_candidates(), &weak)
        .candidates;
    assert_eq!(after[0].text, "誓言");
    assert_eq!(after[0].tag, "[端侧AI 优选]");
    assert_eq!(after[1].text, "实验");
    assert!(after[1].tag.starts_with("[记忆"));

    // 同样弱的记忆，主动改选过：先验乘上倍数（2 × 3 → 1.5 nat），超过模型的 1 nat 优势
    let (mut ai, notified) = enabled_orchestrator();
    ai.set_candidate_scorer(Some(table()));
    let taught = |text: &str| UserMemory { boost: if text == "实验" { 2.0 } else { 0.0 }, explicit: text == "实验" };
    ai.rerank_with_user_priors("shiyan", CONTEXT, AppScene::General, memory_top_candidates(), &taught);
    notified.recv_timeout(WAIT).unwrap();
    let after = ai
        .rerank_with_user_priors("shiyan", CONTEXT, AppScene::General, memory_top_candidates(), &taught)
        .candidates;
    assert_eq!(after[0].text, "实验");
    assert!(after[0].ai_boosted);
}

#[test]
fn memory_top_whose_strength_the_caller_cannot_see_keeps_blocking_the_model() {
    use ime_ai::ChooserMemoryPolicy;
    // 首选带个人记忆标签，调用方给出的首选记忆强度却是 0（旧入口 rerank() 不提供记忆，或者这条记忆存在调用方
    // 查不到的编码下）：没有先验可用，放行就等于让模型无视用户的记忆，照旧挡住（原先的做法）
    let unseen_top = |text: &str| if text == "誓言" { plain(3.0) } else { UserMemory::default() };
    let (mut ai, notified) = enabled_orchestrator();
    let scorer = Arc::new(TableScorer { scores: vec![("实验", -5.0), ("誓言", -4.0)], calls: AtomicUsize::new(0) });
    ai.set_candidate_scorer(Some(scorer.clone()));
    let after = ai
        .rerank_with_user_priors("shiyan", CONTEXT, AppScene::General, memory_top_candidates(), &unseen_top)
        .candidates;
    assert_eq!(after[0].text, "实验");
    assert!(!ai.inprocess_result_pending());
    assert_eq!(ai.rerank("shiyan", CONTEXT, AppScene::General, memory_top_candidates()).candidates[0].text, "实验");
    assert!(!ai.inprocess_result_pending());
    // 负数、NaN 的记忆强度同样当作看不到
    for odd in [-1.0, f32::NAN, f32::INFINITY] {
        let odd_top = move |text: &str| if text == "实验" { plain(odd) } else { plain(3.0) };
        ai.rerank_with_user_priors("shiyan", CONTEXT, AppScene::General, memory_top_candidates(), &odd_top);
        assert!(!ai.inprocess_result_pending(), "{odd}");
    }
    assert!(notified.recv_timeout(QUIET).is_err());
    assert_eq!(scorer.calls.load(Ordering::SeqCst), 0, "打分器一次都没被问过");

    // 首选不是记忆词时不受影响：照常请求
    rerank(&mut ai, "shiyan", CONTEXT);
    notified.recv_timeout(WAIT).expect("非记忆首选照常交给打分器");
    assert_eq!(scorer.calls.load(Ordering::SeqCst), 1);

    // 宿主选词器同样不问，即使池里另有两个词都有记忆（`Contested` 本来会问）、或者策略是 `Always` / `Taught`
    let others_remembered = |text: &str| match text {
        "试验" | "誓言" => plain(3.0),
        _ => UserMemory::default(),
    };
    for policy in [ChooserMemoryPolicy::Contested, ChooserMemoryPolicy::Always, ChooserMemoryPolicy::Taught] {
        let (mut ai, _notified) = enabled_orchestrator();
        let (chooser, events) = fake_chooser(Duration::from_secs(30));
        ai.set_host_chooser(Some(chooser));
        ai.chooser_memory_policy = policy;
        ai.rerank_with_user_priors("shiyan", CONTEXT, AppScene::General, memory_top_candidates(), &others_remembered);
        assert!(events.recv_timeout(QUIET).is_err(), "{policy:?}");
        // 首选的记忆看得到时照常请求
        let all_remembered = |text: &str| match text {
            "实验" | "试验" | "誓言" => plain(3.0),
            _ => UserMemory::default(),
        };
        ai.rerank_with_user_priors("shiyan", CONTEXT, AppScene::General, memory_top_candidates(), &all_remembered);
        assert!(matches!(events.recv_timeout(WAIT), Ok(ChooserEvent::Request(_))), "{policy:?}");
    }
}

#[test]
fn memory_top_and_the_host_chooser() {
    use ime_ai::{ChooserMemoryPolicy, CONTESTED_MEMORY_MIN};
    // 宿主选词器给不出分数，没法和记忆强度一起裁决，所以按策略决定问不问它
    let only_top = |text: &str| if text == "实验" { plain(6.0) } else { UserMemory::default() };
    let two_words = |text: &str| if text == "实验" || text == "誓言" { plain(3.0) } else { UserMemory::default() };
    let asked = |policy: ChooserMemoryPolicy, memory: &dyn Fn(&str) -> UserMemory, context: &str, candidates: Vec<Candidate>| {
        let (mut ai, _notified) = enabled_orchestrator();
        let (chooser, events) = fake_chooser(Duration::from_secs(30));
        ai.set_host_chooser(Some(chooser));
        ai.chooser_memory_policy = policy;
        ai.rerank_with_user_priors("shiyan", context, AppScene::General, candidates, memory);
        matches!(events.recv_timeout(QUIET), Ok(ChooserEvent::Request(_)))
    };

    // 默认（Blocked）：个人记忆首选照旧不发请求
    assert_eq!(DualModeAiOrchestrator::default().chooser_memory_policy, ChooserMemoryPolicy::Blocked);
    assert!(!asked(ChooserMemoryPolicy::Blocked, &two_words, CONTEXT, memory_top_candidates()));

    // Contested：只有首选有记忆（用户一直只用这一个词）不问；候选池里有两个词都有记忆才问
    assert!(!asked(ChooserMemoryPolicy::Contested, &only_top, CONTEXT, memory_top_candidates()));
    assert!(asked(ChooserMemoryPolicy::Contested, &two_words, CONTEXT, memory_top_candidates()));
    // 第二个词的记忆太旧（低于门槛）不算
    let stale = |text: &str| match text {
        "实验" => plain(6.0),
        "誓言" => plain(CONTESTED_MEMORY_MIN * 0.5),
        _ => UserMemory::default(),
    };
    assert!(!asked(ChooserMemoryPolicy::Contested, &stale, CONTEXT, memory_top_candidates()));
    // 门槛比较的是记忆强度本身，不乘主动改选的倍数：一个月前主动改选过的词衰减到 0.6，乘上倍数虽然超过门槛，
    // 也不算近期用过
    let old_but_taught = UserMemory { boost: 0.6, explicit: true };
    assert!(old_but_taught.prior() > CONTESTED_MEMORY_MIN);
    let taught_long_ago = |text: &str| match text {
        "实验" => plain(6.0),
        "誓言" => old_but_taught,
        _ => UserMemory::default(),
    };
    assert!(!asked(ChooserMemoryPolicy::Contested, &taught_long_ago, CONTEXT, memory_top_candidates()));
    // 同一个主动改选过的词，记忆强度在门槛以上就算
    let taught_recently = |text: &str| match text {
        "实验" => plain(6.0),
        "誓言" => UserMemory { boost: CONTESTED_MEMORY_MIN, explicit: true },
        _ => UserMemory::default(),
    };
    assert!(asked(ChooserMemoryPolicy::Contested, &taught_recently, CONTEXT, memory_top_candidates()));
    // 第二个有记忆的词若不在候选池里（只覆盖部分输入的前缀候选）不算
    let prefix_only = |text: &str| if text == "实验" || text == "是" { plain(3.0) } else { UserMemory::default() };
    assert!(!asked(ChooserMemoryPolicy::Contested, &prefix_only, CONTEXT, memory_top_candidates()));

    // Always：有上文就问
    assert!(asked(ChooserMemoryPolicy::Always, &only_top, CONTEXT, memory_top_candidates()));

    // Taught：只靠空格养成的记忆首选照样问（和 Always 一样）；近期主动改选过的首选不问（和 Contested 一样）
    assert!(asked(ChooserMemoryPolicy::Taught, &only_top, CONTEXT, memory_top_candidates()));
    let taught_top = |text: &str| match text {
        "实验" => UserMemory { boost: 6.5, explicit: true },
        _ => UserMemory::default(),
    };
    assert!(!asked(ChooserMemoryPolicy::Taught, &taught_top, CONTEXT, memory_top_candidates()));
    // 教过的首选，但这串拼音有争议（另一个词也近期用过）：照样问
    let taught_but_contested = |text: &str| match text {
        "实验" => UserMemory { boost: 6.5, explicit: true },
        "誓言" => plain(3.0),
        _ => UserMemory::default(),
    };
    assert!(asked(ChooserMemoryPolicy::Taught, &taught_but_contested, CONTEXT, memory_top_candidates()));
    // 很久以前教过（没乘倍数的记忆强度低于门槛）：不再挡
    let taught_top_long_ago = |text: &str| match text {
        "实验" => old_but_taught,
        _ => UserMemory::default(),
    };
    assert!(asked(ChooserMemoryPolicy::Taught, &taught_top_long_ago, CONTEXT, memory_top_candidates()));

    // 任何策略下：没有上文不问；首选是同样上文下的主动改选（[记忆·上文]）不问
    let mut corrected = memory_top_candidates();
    corrected[0].tag = "[记忆·上文]".to_string();
    for policy in [ChooserMemoryPolicy::Contested, ChooserMemoryPolicy::Always, ChooserMemoryPolicy::Taught] {
        assert!(!asked(policy, &two_words, "", memory_top_candidates()));
        assert!(!asked(policy, &two_words, CONTEXT, corrected.clone()));
    }
    // 首选不是记忆词时与策略无关，照常请求
    assert!(asked(ChooserMemoryPolicy::Blocked, &|_| UserMemory::default(), CONTEXT, shiyan_candidates()));
}

#[test]
fn newer_keystroke_cancels_the_running_score() {
    let (mut ai, notified) = enabled_orchestrator();
    let (scorer, started) = FakeScorer::new("誓言", true, true);
    ai.set_candidate_scorer(Some(scorer.clone()));

    rerank(&mut ai, "shiyan", "上文甲");
    assert_eq!(started.recv_timeout(WAIT).unwrap().0, "上文甲");

    // 同一输入再次重排（例如候选窗刷新）：不重复投递，也不取消在途的打分
    rerank(&mut ai, "shiyan", "上文甲");
    assert_eq!(scorer.cancels.load(Ordering::SeqCst), 0);
    assert!(ai.inprocess_result_pending());

    // 新的击键：在途的打分被协作式取消，后台线程转去处理最新请求
    rerank(&mut ai, "shiyan", "上文乙");
    assert_eq!(scorer.cancels.load(Ordering::SeqCst), 1);
    assert_eq!(started.recv_timeout(WAIT).unwrap().0, "上文乙");
    assert_eq!(scorer.cancelled_calls.load(Ordering::SeqCst), 1);
    assert!(notified.recv_timeout(QUIET).is_err(), "被取消的请求不产生结果");

    scorer.release(1);
    notified.recv_timeout(WAIT).unwrap();
    assert_eq!(rerank(&mut ai, "shiyan", "上文乙")[0].text, "誓言");
    assert_eq!(scorer.calls(), 2);

    // 被取消的请求没有写入缓存：回到原输入时重新打分
    assert_eq!(rerank(&mut ai, "shiyan", "上文甲")[0].text, "实验");
    assert_eq!(started.recv_timeout(WAIT).unwrap().0, "上文甲");
    scorer.release(1);
    notified.recv_timeout(WAIT).unwrap();
    assert_eq!(rerank(&mut ai, "shiyan", "上文甲")[0].text, "誓言");
    assert_eq!(scorer.calls(), 3);
}

#[test]
fn slow_scorer_does_not_pile_up_jobs() {
    let (mut ai, notified) = enabled_orchestrator();
    // 不配合取消的慢打分器：第一次打分会一直占着后台线程
    let (scorer, started) = FakeScorer::new("誓言", true, false);
    ai.set_candidate_scorer(Some(scorer.clone()));

    rerank(&mut ai, "shiyan", "上文0");
    assert_eq!(started.recv_timeout(WAIT).unwrap().0, "上文0");

    // 打分期间用户又敲了 20 次键
    for i in 1..=20 {
        rerank(&mut ai, "shiyan", &format!("上文{i}"));
    }
    assert_eq!(scorer.cancels.load(Ordering::SeqCst), 20, "每次新的击键都会请求取消在途的打分");
    assert_eq!(scorer.calls(), 1);

    // 放行：第一次打分结束后，后台线程直接跳到最新的请求，中间 19 个都不执行
    scorer.release(2);
    assert_eq!(started.recv_timeout(WAIT).unwrap().0, "上文20");
    notified.recv_timeout(WAIT).unwrap();
    assert_eq!(scorer.calls(), 2);
    assert!(notified.recv_timeout(QUIET).is_err(), "过期的结果不触发刷新回调");
    assert_eq!(rerank(&mut ai, "shiyan", "上文20")[0].text, "誓言");

    // 第一次打分来不及取消，但结果对它自己的输入仍然正确，已经进了缓存
    assert_eq!(rerank(&mut ai, "shiyan", "上文0")[0].text, "誓言");
    assert_eq!(scorer.calls(), 2);
    assert!(!ai.inprocess_result_pending());
}

#[test]
fn cancel_pending_llm_work_stops_the_scorer() {
    let (mut ai, notified) = enabled_orchestrator();
    let (scorer, started) = FakeScorer::new("誓言", true, true);
    ai.set_candidate_scorer(Some(scorer.clone()));
    rerank(&mut ai, "shiyan", CONTEXT);
    started.recv_timeout(WAIT).unwrap();

    ai.cancel_pending_llm_work();
    assert_eq!(scorer.cancels.load(Ordering::SeqCst), 1);
    assert!(!ai.inprocess_result_pending());
    assert!(notified.recv_timeout(QUIET).is_err());
    assert_eq!(scorer.cancelled_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn failing_scorer_leaves_rule_result_in_place() {
    let (mut ai, notified) = enabled_orchestrator();
    let (tx, started) = mpsc::channel();
    let scorer = Arc::new(FakeScorer {
        favourite: Mutex::new("誓言".to_string()),
        gated: false,
        cooperative: true,
        gate: Mutex::new(GateState::default()),
        gate_cv: Condvar::new(),
        started: Mutex::new(tx),
        calls: AtomicUsize::new(0),
        cancels: AtomicUsize::new(0),
        cancelled_calls: AtomicUsize::new(0),
        fail: true,
    });
    ai.set_candidate_scorer(Some(scorer.clone()));
    rerank(&mut ai, "shiyan", CONTEXT);
    started.recv_timeout(WAIT).unwrap();
    assert!(notified.recv_timeout(QUIET).is_err());
    assert!(!ai.inprocess_result_pending(), "打分失败后不再等待结果");
    assert_eq!(rerank(&mut ai, "shiyan", CONTEXT)[0].text, "实验");
}

/// 没有安装打分器 / 选词器时仍走 HTTP：后台线程会去连接配置的本机 llama.cpp 服务；
/// 安装打分器之后不再发起任何 HTTP 请求。
#[test]
fn http_path_is_used_only_when_nothing_is_installed() {
    use std::io::Read;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen_tx, seen_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let _ = seen_tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
            // 不作答直接断开：客户端按失败处理，不写缓存
        }
    });

    let mut config = OpenAiCompatibleConfig::default();
    config.enabled = true;
    config.api_base = format!("http://127.0.0.1:{port}/v1");
    let mut ai = DualModeAiOrchestrator::new(config);

    assert_eq!(rerank(&mut ai, "shiyan", "上文甲")[0].text, "实验");
    let request = seen_rx.recv_timeout(WAIT).expect("未安装打分器时应发起 HTTP 请求");
    assert!(request.starts_with("POST /tokenize"), "{request}");
    assert!(!ai.inprocess_result_pending(), "HTTP 路径不参与进程内等待统计");
    // 等这一轮 HTTP 尝试结束（失败后可能还有若干次重连）
    while seen_rx.recv_timeout(Duration::from_millis(600)).is_ok() {}

    let (scorer, started) = FakeScorer::new("誓言", false, true);
    ai.set_candidate_scorer(Some(scorer));
    rerank(&mut ai, "shiyan", "上文乙");
    started.recv_timeout(WAIT).expect("安装后改用打分器");
    assert!(seen_rx.recv_timeout(Duration::from_millis(400)).is_err(), "安装打分器后不应再有 HTTP 请求");
    assert_eq!(rerank(&mut ai, "shiyan", "上文乙")[0].text, "誓言");
}

// ---------------------------------------------------------------------------------------------
// 假宿主选词器
// ---------------------------------------------------------------------------------------------

fn fake_chooser(timeout: Duration) -> (HostChooser, Receiver<ChooserEvent>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let chooser = HostChooser::new(
        "fake_host",
        "系统AI",
        Arc::new(move |event| {
            let _ = tx.lock().unwrap().send(event);
        }),
    )
    .with_timeout(timeout);
    (chooser, rx)
}

fn expect_request(events: &Receiver<ChooserEvent>) -> ChooserRequest {
    match events.recv_timeout(WAIT).expect("应收到宿主选词请求") {
        ChooserEvent::Request(request) => request,
        other => panic!("期望 Request，收到 {other:?}"),
    }
}

#[test]
fn chooser_request_delivery_cache_and_notify() {
    let (mut ai, notified) = enabled_orchestrator();
    let (chooser, events) = fake_chooser(WAIT);
    ai.set_host_chooser(Some(chooser));
    let handle = ai.chooser_handle();

    let first = rerank(&mut ai, "shiyan", CONTEXT);
    assert_eq!(first[0].text, "实验");
    let request = expect_request(&events);
    assert_eq!(request.context, CONTEXT);
    assert_eq!(request.pinyin, "shiyan");
    assert_eq!(request.candidates, SHIYAN_POOL);
    assert!(request.cache_key.starts_with("chooser:fake_host:"));
    assert!(request.cache_key.ends_with("\tshiyan"));
    assert!(ai.inprocess_result_pending());

    // 同一输入再次重排：请求仍在等待，不重复发出
    rerank(&mut ai, "shiyan", CONTEXT);
    assert!(events.recv_timeout(QUIET).is_err());

    // 宿主在自己的线程里答复
    let id = request.id;
    let delivery = std::thread::spawn(move || handle.deliver(id, Some(" 誓言\n"))).join().unwrap();
    assert_eq!(delivery, ChooserDelivery::Accepted);
    notified.recv_timeout(WAIT).expect("采纳答复后应触发刷新回调");

    let second = rerank(&mut ai, "shiyan", CONTEXT);
    assert_eq!(second[0].text, "誓言");
    assert_eq!(second[0].tag, "[系统AI 优选]");
    assert!(second[0].ai_boosted);

    // 缓存命中：不再向宿主发请求；同一编号重复答复被忽略
    assert!(events.recv_timeout(QUIET).is_err());
    assert_eq!(ai.deliver_chooser_result(id, Some("食盐")), ChooserDelivery::Stale);
    assert_eq!(rerank(&mut ai, "shiyan", CONTEXT)[0].text, "誓言");
    assert!(notified.recv_timeout(QUIET).is_err());
}

#[test]
fn chooser_stale_and_unknown_ids_are_ignored() {
    let (mut ai, notified) = enabled_orchestrator();
    let (chooser, events) = fake_chooser(WAIT);
    ai.set_host_chooser(Some(chooser));

    rerank(&mut ai, "shiyan", "上文甲");
    let old = expect_request(&events);

    // 新的击键：旧请求作废（宿主收到 Cancel），新请求发出
    rerank(&mut ai, "shiyan", "上文乙");
    assert_eq!(events.recv_timeout(WAIT).unwrap(), ChooserEvent::Cancel { id: old.id });
    let new = expect_request(&events);
    assert_eq!(new.context, "上文乙");
    assert!(new.id > old.id);

    // 旧编号、从未发出的编号：一律忽略
    assert_eq!(ai.deliver_chooser_result(old.id, Some("誓言")), ChooserDelivery::Stale);
    assert_eq!(ai.deliver_chooser_result(987_654, Some("誓言")), ChooserDelivery::Stale);
    assert!(notified.recv_timeout(QUIET).is_err());
    assert!(ai.inprocess_result_pending(), "新请求仍在等待");

    assert_eq!(ai.deliver_chooser_result(new.id, Some("食盐")), ChooserDelivery::Accepted);
    notified.recv_timeout(WAIT).unwrap();
    assert_eq!(rerank(&mut ai, "shiyan", "上文乙")[0].text, "食盐");

    // 旧输入没有被过期的答复污染：回到旧输入时重新发请求
    assert_eq!(rerank(&mut ai, "shiyan", "上文甲")[0].text, "实验");
    assert_eq!(expect_request(&events).context, "上文甲");
}

#[test]
fn chooser_implausible_and_empty_answers_are_dropped() {
    let (mut ai, notified) = enabled_orchestrator();
    let (chooser, events) = fake_chooser(WAIT);
    ai.set_host_chooser(Some(chooser));

    // 不在候选池内的文本（包括被剔除的纠错候选与前缀候选）
    rerank(&mut ai, "shiyan", "上文甲");
    let request = expect_request(&events);
    assert_eq!(ai.deliver_chooser_result(request.id, Some("式样")), ChooserDelivery::Implausible);
    assert_eq!(ai.deliver_chooser_result(request.id, Some("誓言")), ChooserDelivery::Stale, "请求已经结束");

    // 宿主没有结果
    rerank(&mut ai, "shiyan", "上文乙");
    let request = expect_request(&events);
    assert_eq!(ai.deliver_chooser_result(request.id, None), ChooserDelivery::NoAnswer);
    rerank(&mut ai, "shiyan", "上文丙");
    let request = expect_request(&events);
    assert_eq!(ai.deliver_chooser_result(request.id, Some("  ")), ChooserDelivery::NoAnswer);

    assert!(notified.recv_timeout(QUIET).is_err());
    assert!(!ai.inprocess_result_pending(), "宿主没有结果时不再等待");
    assert_eq!(rerank(&mut ai, "shiyan", "上文甲")[0].text, "实验");
}

#[test]
fn chooser_timeout_voids_the_request() {
    let (mut ai, notified) = enabled_orchestrator();
    let (chooser, events) = fake_chooser(Duration::from_millis(40));
    ai.set_host_chooser(Some(chooser));

    rerank(&mut ai, "shiyan", CONTEXT);
    let first = expect_request(&events);
    std::thread::sleep(Duration::from_millis(120));

    // 超时后同一输入再次重排：旧请求作废并重新发出
    rerank(&mut ai, "shiyan", CONTEXT);
    assert_eq!(events.recv_timeout(WAIT).unwrap(), ChooserEvent::Cancel { id: first.id });
    let second = expect_request(&events);
    assert!(second.id > first.id);
    assert_eq!(ai.deliver_chooser_result(first.id, Some("誓言")), ChooserDelivery::Stale);

    // 迟到的答复被丢弃
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(ai.deliver_chooser_result(second.id, Some("誓言")), ChooserDelivery::TimedOut);
    assert!(notified.recv_timeout(QUIET).is_err());
    assert_eq!(rerank(&mut ai, "shiyan", CONTEXT)[0].text, "实验");

    // 超时之后输入变了：旧请求作废（宿主收到 Cancel），迟到的答复是 Stale 而不是 TimedOut
    let third = expect_request(&events);
    std::thread::sleep(Duration::from_millis(120));
    rerank(&mut ai, "shiyan", "另一段上文");
    assert_eq!(events.recv_timeout(WAIT).unwrap(), ChooserEvent::Cancel { id: third.id });
    let fourth = expect_request(&events);
    assert_eq!(ai.deliver_chooser_result(third.id, Some("誓言")), ChooserDelivery::Stale);
    // 超时之后输入结束：同样
    std::thread::sleep(Duration::from_millis(120));
    ai.cancel_pending_llm_work();
    assert_eq!(events.recv_timeout(WAIT).unwrap(), ChooserEvent::Cancel { id: fourth.id });
    assert_eq!(ai.deliver_chooser_result(fourth.id, Some("誓言")), ChooserDelivery::Stale);
    assert!(notified.recv_timeout(QUIET).is_err());
}

#[test]
fn chooser_timeout_ends_the_wait() {
    let (mut ai, notified) = enabled_orchestrator();
    let (chooser, events) = fake_chooser(Duration::from_millis(40));
    ai.set_host_chooser(Some(chooser));

    // 宿主一直不答复：超过等待时限后不再算「在等」（迟到的答复只会被丢弃），等结果的循环不会一直等下去
    rerank(&mut ai, "shiyan", CONTEXT);
    let first = expect_request(&events);
    std::thread::sleep(Duration::from_millis(150));
    assert!(!ai.inprocess_result_pending(), "超过等待时限后不再等");
    assert!(!ai.inprocess_result_pending(), "查询本身不改变状态");

    // 同一输入再次重排：重新发出请求，它也超时之后同样不再等
    rerank(&mut ai, "shiyan", CONTEXT);
    assert_eq!(events.recv_timeout(WAIT).unwrap(), ChooserEvent::Cancel { id: first.id });
    let second = expect_request(&events);
    assert!(second.id > first.id);
    std::thread::sleep(Duration::from_millis(150));
    assert!(!ai.inprocess_result_pending());

    // 迟到的答复被丢弃，不触发刷新
    assert_eq!(ai.deliver_chooser_result(second.id, Some("誓言")), ChooserDelivery::TimedOut);
    assert!(!ai.inprocess_result_pending());
    assert!(notified.recv_timeout(QUIET).is_err());

    // 换一个输入：新请求照常等待（时限从发出时算起），超时后同样不再等
    rerank(&mut ai, "shiyan", "另一段上文");
    expect_request(&events);
    std::thread::sleep(Duration::from_millis(150));
    assert!(!ai.inprocess_result_pending());
}

#[test]
fn chooser_request_is_cancelled_when_no_longer_needed() {
    let (mut ai, _notified) = enabled_orchestrator();
    let (chooser, events) = fake_chooser(WAIT);
    ai.set_host_chooser(Some(chooser));

    // 下一次击键不需要大模型（无上文短输入）：等待中的请求作废
    rerank(&mut ai, "shiyan", CONTEXT);
    let first = expect_request(&events);
    rerank(&mut ai, "shiyan", "");
    assert_eq!(events.recv_timeout(WAIT).unwrap(), ChooserEvent::Cancel { id: first.id });
    assert_eq!(ai.deliver_chooser_result(first.id, Some("誓言")), ChooserDelivery::Stale);

    // 输入结束
    rerank(&mut ai, "shiyan", CONTEXT);
    let second = expect_request(&events);
    ai.cancel_pending_llm_work();
    assert_eq!(events.recv_timeout(WAIT).unwrap(), ChooserEvent::Cancel { id: second.id });

    // 卸载选词器
    rerank(&mut ai, "shiyan", CONTEXT);
    let third = expect_request(&events);
    ai.set_host_chooser(None);
    assert_eq!(events.recv_timeout(WAIT).unwrap(), ChooserEvent::Cancel { id: third.id });
    assert_eq!(ai.deliver_chooser_result(third.id, Some("誓言")), ChooserDelivery::Stale);
}

#[test]
fn scorer_takes_priority_over_chooser() {
    let (mut ai, notified) = enabled_orchestrator();
    let (chooser, events) = fake_chooser(WAIT);
    ai.set_host_chooser(Some(chooser));

    // 只有选词器时请求发给宿主
    rerank(&mut ai, "shiyan", "上文甲");
    let pending = expect_request(&events);

    // 安装打分器：等待中的宿主请求作废，之后只用打分器
    let (scorer, started) = FakeScorer::new("誓言", false, true);
    ai.set_candidate_scorer(Some(scorer.clone()));
    assert_eq!(events.recv_timeout(WAIT).unwrap(), ChooserEvent::Cancel { id: pending.id });

    rerank(&mut ai, "shiyan", "上文乙");
    started.recv_timeout(WAIT).unwrap();
    notified.recv_timeout(WAIT).unwrap();
    let after = rerank(&mut ai, "shiyan", "上文乙");
    assert_eq!(after[0].text, "誓言");
    assert_eq!(after[0].tag, "[端侧AI 优选]");
    assert!(events.recv_timeout(QUIET).is_err(), "安装了打分器就不再请求宿主");

    // 卸载打分器后回到宿主选词器（缓存键不同，不会误用打分器的结果）
    ai.set_candidate_scorer(None);
    assert_eq!(rerank(&mut ai, "shiyan", "上文乙")[0].text, "实验");
    let request = expect_request(&events);
    assert_eq!(request.context, "上文乙");
    assert_eq!(scorer.calls(), 1);
}

// ---------------------------------------------------------------------------------------------
// 候选池是缓存键的一部分
// ---------------------------------------------------------------------------------------------

#[test]
fn cached_pick_for_another_pool_is_not_a_hit() {
    let (mut ai, notified) = enabled_orchestrator();
    let (scorer, started) = FakeScorer::new("誓言", false, true);
    ai.set_candidate_scorer(Some(scorer.clone()));

    rerank(&mut ai, "shiyan", CONTEXT);
    assert_eq!(started.recv_timeout(WAIT).unwrap().1, SHIYAN_POOL);
    notified.recv_timeout(WAIT).unwrap();
    assert_eq!(rerank(&mut ai, "shiyan", CONTEXT)[0].text, "誓言");

    // 同样的上文 + 拼音，候选池变了（用户固定了一段）：原先的结果不在新池里，必须为新池重新请求，
    // 而不是「命中缓存、但结果用不上、也不再问模型」
    *scorer.favourite.lock().unwrap() = "试验".to_string();
    let other = ai.rerank("shiyan", CONTEXT, AppScene::General, shiyan_candidates_other_pool()).candidates;
    assert_eq!(other[0].text, "食盐");
    assert!(ai.inprocess_result_pending(), "新的候选池应当重新请求");
    assert_eq!(started.recv_timeout(WAIT).unwrap().1, OTHER_POOL);
    notified.recv_timeout(WAIT).unwrap();
    let other = ai.rerank("shiyan", CONTEXT, AppScene::General, shiyan_candidates_other_pool()).candidates;
    assert_eq!(other[0].text, "试验");
    assert_eq!(scorer.calls(), 2);

    // 撤销固定、回到原来的池：原来的结果仍在缓存里，直接命中
    assert_eq!(rerank(&mut ai, "shiyan", CONTEXT)[0].text, "誓言");
    assert!(!ai.inprocess_result_pending());
    assert_eq!(scorer.calls(), 2);

    // 按拼音清理时，两个池的结果一起清掉
    ai.invalidate_cache_for_pinyin("shiyan");
    assert_eq!(rerank(&mut ai, "shiyan", CONTEXT)[0].text, "实验");
    notified.recv_timeout(WAIT).unwrap();
    assert_eq!(scorer.calls(), 3);
    let other = ai.rerank("shiyan", CONTEXT, AppScene::General, shiyan_candidates_other_pool()).candidates;
    assert_eq!(other[0].text, "食盐");
    notified.recv_timeout(WAIT).unwrap();
    assert_eq!(scorer.calls(), 4);
}

#[test]
fn in_flight_score_for_another_pool_is_replaced() {
    let (mut ai, notified) = enabled_orchestrator();
    let (scorer, started) = FakeScorer::new("试验", true, true);
    ai.set_candidate_scorer(Some(scorer.clone()));

    rerank(&mut ai, "shiyan", CONTEXT);
    assert_eq!(started.recv_timeout(WAIT).unwrap().1, SHIYAN_POOL);

    // 打分还在途时候选池变了：旧池的任务不算「正在算这一个」，取消并为新池重新投递
    ai.rerank("shiyan", CONTEXT, AppScene::General, shiyan_candidates_other_pool());
    assert_eq!(scorer.cancels.load(Ordering::SeqCst), 1);
    assert_eq!(started.recv_timeout(WAIT).unwrap().1, OTHER_POOL);
    scorer.release(1);
    notified.recv_timeout(WAIT).unwrap();
    let after = ai.rerank("shiyan", CONTEXT, AppScene::General, shiyan_candidates_other_pool()).candidates;
    assert_eq!(after[0].text, "试验");
    assert_eq!(after[0].tag, "[端侧AI 优选]");
}

#[test]
fn chooser_request_for_another_pool_replaces_the_pending_one() {
    let (mut ai, notified) = enabled_orchestrator();
    let (chooser, events) = fake_chooser(WAIT);
    ai.set_host_chooser(Some(chooser));

    rerank(&mut ai, "shiyan", CONTEXT);
    let first = expect_request(&events);
    assert_eq!(first.candidates, SHIYAN_POOL);

    ai.rerank("shiyan", CONTEXT, AppScene::General, shiyan_candidates_other_pool());
    assert_eq!(events.recv_timeout(WAIT).unwrap(), ChooserEvent::Cancel { id: first.id });
    let second = expect_request(&events);
    assert_eq!(second.candidates, OTHER_POOL);
    assert_ne!(second.cache_key, first.cache_key);
    // 键的形状不变：后端前缀在最前，「\t拼音」在最后（按拼音清理缓存靠这个后缀）
    for key in [&first.cache_key, &second.cache_key] {
        assert!(key.starts_with("chooser:fake_host:"), "{key}");
        assert!(key.ends_with(&format!("{CONTEXT}\tshiyan")), "{key}");
    }

    assert_eq!(ai.deliver_chooser_result(first.id, Some("誓言")), ChooserDelivery::Stale);
    assert_eq!(ai.deliver_chooser_result(second.id, Some("试验")), ChooserDelivery::Accepted);
    notified.recv_timeout(WAIT).unwrap();
    let after = ai.rerank("shiyan", CONTEXT, AppScene::General, shiyan_candidates_other_pool()).candidates;
    assert_eq!(after[0].text, "试验");
}

// ---------------------------------------------------------------------------------------------
// panic 不拖垮后台线程
// ---------------------------------------------------------------------------------------------

/// 第一次打分时 panic，之后正常
struct PanickingScorer {
    calls: AtomicUsize,
}

impl CandidateScorer for PanickingScorer {
    fn model_id(&self) -> String {
        "panicking-model".to_string()
    }

    fn score(&self, _context: &str, candidates: &[String]) -> Result<Vec<f64>, ScoreError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("假打分器故意 panic（测试用）");
        }
        Ok(candidates.iter().map(|c| if c == "誓言" { -1.0 } else { -9.0 }).collect())
    }

    fn cancel(&self) {}
}

#[test]
fn panicking_scorer_does_not_kill_the_worker() {
    let (mut ai, notified) = enabled_orchestrator();
    let scorer = Arc::new(PanickingScorer { calls: AtomicUsize::new(0) });
    ai.set_candidate_scorer(Some(scorer.clone()));

    // 第一次打分 panic：没有结果、没有回调，但也不再「一直在等」
    assert_eq!(rerank(&mut ai, "shiyan", CONTEXT)[0].text, "实验");
    wait_until_idle(&ai);
    assert_eq!(scorer.calls.load(Ordering::SeqCst), 1);
    assert!(notified.recv_timeout(QUIET).is_err());

    // 同一输入再来一次：重新投递，后台线程还活着
    assert_eq!(rerank(&mut ai, "shiyan", CONTEXT)[0].text, "实验");
    notified.recv_timeout(WAIT).expect("panic 之后后台线程应继续处理请求");
    assert_eq!(scorer.calls.load(Ordering::SeqCst), 2);
    let after = rerank(&mut ai, "shiyan", CONTEXT);
    assert_eq!(after[0].text, "誓言");
    assert_eq!(after[0].tag, "[端侧AI 优选]");

    // 别的输入、换一个打分器也都正常
    rerank(&mut ai, "shiyan", "上文乙");
    notified.recv_timeout(WAIT).unwrap();
    let (fresh, started) = FakeScorer::new("食盐", false, true);
    ai.set_candidate_scorer(Some(fresh));
    rerank(&mut ai, "shiyan", "上文丙");
    started.recv_timeout(WAIT).unwrap();
    notified.recv_timeout(WAIT).unwrap();
    assert_eq!(rerank(&mut ai, "shiyan", "上文丙")[0].text, "食盐");
}

#[test]
fn panicking_chooser_callback_does_not_kill_the_worker() {
    let (mut ai, notified) = enabled_orchestrator();
    let (tx, events) = mpsc::channel();
    let tx = Mutex::new(tx);
    let seen = AtomicUsize::new(0);
    let chooser = HostChooser::new(
        "fake_host",
        "系统AI",
        Arc::new(move |event| {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("假宿主回调故意 panic（测试用）");
            }
            let _ = tx.lock().unwrap().send(event);
        }),
    )
    .with_timeout(Duration::from_secs(30));
    ai.set_host_chooser(Some(chooser));

    // 第一次请求在回调里 panic：这次请求作废，不占着等待位直到 30 秒超时
    rerank(&mut ai, "shiyan", CONTEXT);
    wait_until_idle(&ai);
    assert!(events.recv_timeout(QUIET).is_err());

    // 同一输入再来一次：重新发出请求（不需要为那个宿主没收到的请求补发 Cancel）
    rerank(&mut ai, "shiyan", CONTEXT);
    let request = expect_request(&events);
    assert_eq!(request.candidates, SHIYAN_POOL);
    assert_eq!(ai.deliver_chooser_result(request.id, Some("誓言")), ChooserDelivery::Accepted);
    notified.recv_timeout(WAIT).unwrap();
    assert_eq!(rerank(&mut ai, "shiyan", CONTEXT)[0].text, "誓言");
}

// ---------------------------------------------------------------------------------------------
// 上屏之后才算完的结果不写缓存
// ---------------------------------------------------------------------------------------------

#[test]
fn score_finishing_after_a_commit_is_not_cached() {
    let (mut ai, notified) = enabled_orchestrator();
    // 不配合取消的打分器：模拟「已经过了最后一次取消检查」的那段时间
    let (scorer, started) = FakeScorer::new("誓言", true, false);
    ai.set_candidate_scorer(Some(scorer.clone()));

    rerank(&mut ai, "shiyan", CONTEXT);
    started.recv_timeout(WAIT).unwrap();

    // 用户自己选了词上屏：清缓存，然后输入结束
    ai.invalidate_cache_for_pinyin("shiyan");
    ai.cancel_pending_llm_work();

    // 那次打分此后才算完。借另一个输入的回调确认后台线程已经处理完它（任务是串行执行的）
    scorer.release(2);
    rerank(&mut ai, "shiyan", "上文乙");
    assert_eq!(started.recv_timeout(WAIT).unwrap().0, "上文乙");
    notified.recv_timeout(WAIT).unwrap();
    assert!(notified.recv_timeout(QUIET).is_err());
    assert_eq!(scorer.calls(), 2);

    // 同样的上文 + 拼音再次出现：被用户推翻的结果不在缓存里，重新问模型
    let again = rerank(&mut ai, "shiyan", CONTEXT);
    assert_eq!(again[0].text, "实验");
    assert!(!again[0].ai_boosted, "上屏之后才算完的结果不应被采纳");
    assert!(ai.inprocess_result_pending());
    assert_eq!(started.recv_timeout(WAIT).unwrap().0, CONTEXT);
    scorer.release(1);
    notified.recv_timeout(WAIT).unwrap();
    assert_eq!(scorer.calls(), 3);
    assert_eq!(rerank(&mut ai, "shiyan", CONTEXT)[0].text, "誓言");
}

#[test]
fn same_input_after_a_commit_gets_a_fresh_score() {
    let (mut ai, notified) = enabled_orchestrator();
    let (scorer, started) = FakeScorer::new("誓言", true, false);
    ai.set_candidate_scorer(Some(scorer.clone()));

    rerank(&mut ai, "shiyan", CONTEXT);
    started.recv_timeout(WAIT).unwrap();

    // 清缓存之后同一个键马上又被请求（例如固定一段之后候选池恰好没变）：在途的任务已经作废，
    // 不能再当作「正在算这一个」，否则它算完不写缓存，这个输入就再也等不到结果
    ai.invalidate_cache_for_pinyin("shiyan");
    rerank(&mut ai, "shiyan", CONTEXT);
    assert!(ai.inprocess_result_pending());
    assert_eq!(scorer.cancels.load(Ordering::SeqCst), 1, "作废的在途任务被取消");

    scorer.release(2);
    assert_eq!(started.recv_timeout(WAIT).unwrap().0, CONTEXT);
    notified.recv_timeout(WAIT).expect("重新投递的任务照常给出结果");
    assert!(notified.recv_timeout(QUIET).is_err(), "作废的那一次不触发回调");
    assert_eq!(scorer.calls(), 2);
    assert_eq!(rerank(&mut ai, "shiyan", CONTEXT)[0].text, "誓言");
}

// ---------------------------------------------------------------------------------------------
// 不可用的分数
// ---------------------------------------------------------------------------------------------

fn table_scorer(scores: Vec<(&'static str, f64)>) -> Arc<TableScorer> {
    Arc::new(TableScorer { scores, calls: AtomicUsize::new(0) })
}

#[test]
fn nan_or_all_negative_infinity_scores_count_as_a_failed_scoring() {
    let ninf = f64::NEG_INFINITY;
    let unusable = [
        vec![("实验", f64::NAN), ("试验", -3.0), ("食盐", -4.0), ("誓言", -1.0)],
        vec![("实验", -2.0), ("试验", -3.0), ("食盐", f64::NAN), ("誓言", -1.0)],
        vec![("实验", ninf), ("试验", ninf), ("食盐", ninf), ("誓言", ninf)],
    ];
    for scores in unusable {
        let (mut ai, notified) = enabled_orchestrator();
        let scorer = table_scorer(scores);
        ai.set_candidate_scorer(Some(scorer.clone()));
        rerank(&mut ai, "shiyan", CONTEXT);
        wait_until_idle(&ai);
        assert!(notified.recv_timeout(QUIET).is_err(), "不可用的分数不触发刷新");
        assert_eq!(scorer.calls.load(Ordering::SeqCst), 1);

        // 没有写缓存：首选不带 AI 标签，同一输入会重新请求
        let again = rerank(&mut ai, "shiyan", CONTEXT);
        assert_eq!(again[0].text, "实验");
        assert!(!again[0].ai_boosted);
        assert_eq!(again[0].tag, "");
        wait_until_idle(&ai);
        assert_eq!(scorer.calls.load(Ordering::SeqCst), 2, "失败的结果没有进缓存，下次重排重新打分");
    }
}

#[test]
fn single_negative_infinity_candidate_never_wins() {
    let ninf = f64::NEG_INFINITY;

    // 非首选打不出分：其余候选照常比较
    let (mut ai, notified) = enabled_orchestrator();
    ai.set_candidate_scorer(Some(table_scorer(vec![("实验", -5.0), ("试验", ninf), ("食盐", -9.0), ("誓言", -4.0)])));
    rerank(&mut ai, "shiyan", CONTEXT);
    notified.recv_timeout(WAIT).unwrap();
    assert_eq!(rerank(&mut ai, "shiyan", CONTEXT)[0].text, "誓言");

    // 当前首选打不出分：换成有分数的候选里最高的
    let (mut ai, notified) = enabled_orchestrator();
    ai.set_candidate_scorer(Some(table_scorer(vec![("实验", ninf), ("试验", -6.0), ("食盐", -9.0), ("誓言", -7.0)])));
    rerank(&mut ai, "shiyan", CONTEXT);
    notified.recv_timeout(WAIT).unwrap();
    let after = rerank(&mut ai, "shiyan", CONTEXT);
    assert_eq!(after[0].text, "试验");
    assert_eq!(after[1].text, "实验", "被换下的原首选仍然可选");

    // 只有首选打得出分：确认首选
    let (mut ai, notified) = enabled_orchestrator();
    ai.set_candidate_scorer(Some(table_scorer(vec![("实验", -5.0), ("试验", ninf), ("食盐", ninf), ("誓言", ninf)])));
    rerank(&mut ai, "shiyan", CONTEXT);
    notified.recv_timeout(WAIT).unwrap();
    let after = rerank(&mut ai, "shiyan", CONTEXT);
    assert_eq!(after[0].text, "实验");
    assert!(after[0].ai_boosted);
}
