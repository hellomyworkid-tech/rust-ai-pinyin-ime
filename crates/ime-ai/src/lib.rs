//! `ime-ai`: 双模「AI 理解选词」与异步无阻塞并发调度引擎
//!
//! 同时支持：
//! 1. **端侧极速模式（默认，`<0.15ms`）**：基于 KV-Cache 上下文状态预热 + 单次前向 N-Best 重打分 + 句内同音搭配矩阵 + 拼音 Trie 掩码约束 (`PinyinLogitsMask`)
//! 2. **本地/云端大模型模式（可选，后台异步无阻塞）**：
//!    - 本地最优解：`Qwen2.5-1.5B-Instruct` / `Qwen2.5-0.5B-Instruct`（Ollama / llama.cpp）
//!    - 主流云端大模型：**OpenAI (ChatGPT)**、**Anthropic (Claude)**、**Google (Gemini)**
//!    - **自定义云端 (Custom Cloud)**：DeepSeek、硅基流动、阿里云百炼、OpenRouter 等
//! 3. **进程内打分器 / 宿主选词器（可选，见 [`inprocess`]）**：不经过 HTTP 的大模型选词后端，
//!    供不能创建子进程的平台（iOS 键盘扩展）使用；未安装时行为与上面两种模式完全相同

pub mod inprocess;
pub mod lm_text;
pub mod local_engine;
pub mod managed_llm;
pub mod openai_client;
pub mod pinyin_mask;

pub use inprocess::{
    choose_by_exact_scores, select_scoring_pool, CandidateScorer, ChooserCallback, ChooserDelivery, ChooserEvent,
    ChooserHandle, ChooserRequest, HostChooser, ScoreError, SCORING_POOL_MAX, choose_with_user_priors, USER_PRIOR_WEIGHT,
    EXPLICIT_CHOICE_PRIOR_FACTOR, UserMemory,
    scores_are_usable,
};
pub use local_engine::{AiRerankReport, CachedContextState, OnDeviceAiEngine};
pub use managed_llm::{BuiltinModelTier, ManagedLocalLlm, MANAGED_LLM_BASE_URL, MANAGED_LLM_PORT};
pub use openai_client::{
    AiProvider, ChatCompletionRequest, LocalEndpointStatus, OpenAiCompatibleClient,
    OpenAiCompatibleConfig, ProviderProfile,
};
pub use pinyin_mask::{PinyinLogitsMask, VocabTokenEntry};

use ime_core::{AppScene, Candidate};
use inprocess::{InProcessDispatch, InProcessJob, InProcessRequest, InProcessShared};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

struct AsyncRerankJob {
    gen_id: u64,
    cache_key: String,
    pinyin_input: String,
    surrounding_context: String,
    scene: AppScene,
    candidates: Vec<Candidate>,
    config: OpenAiCompatibleConfig,
}

/// 投递给后台线程的任务
enum AsyncJob {
    /// HTTP 路径（本机 llama-server / 云端接口）
    Http(AsyncRerankJob),
    /// 进程内打分器
    Score(InProcessJob),
    /// 宿主选词器（`None` 表示只清理已作废的等待项）
    Choose(Option<InProcessJob>),
}

/// 带版本号防抖、后台异步推理与结果缓存的双模 AI 选词调度器
///
/// 关键设计：
/// Windows `WH_KEYBOARD_LL` 低级键盘钩子严禁在回调线程执行耗时网络 I/O。
/// 因此当开启本地 Ollama 或云端 ChatGPT/Claude/Gemini 时，击键线程仅做 `<0.15ms` 的端侧重排与缓存查表；
/// 未命中的大模型请求通过 `mpsc` 投递给后台常驻线程，推理完成后写入 `llm_cache` 并触发 UI 刷新回调。
pub struct DualModeAiOrchestrator {
    pub local_engine: OnDeviceAiEngine,
    pub cloud_client: OpenAiCompatibleClient,
    generation_counter: Arc<AtomicU64>,
    /// 缓存键 → (被选中的候选文本, 来源简称)
    llm_cache: Arc<Mutex<HashMap<String, (String, String)>>>,
    async_tx: mpsc::Sender<AsyncJob>,
    notify_cb: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
    /// 进程内打分器 / 宿主选词器的共享状态（两者都未安装时不参与任何流程）
    inprocess: Arc<InProcessShared>,
    /// 首选是个人记忆词（`[记忆·N次]`）时，是否连进程内打分器也不请求。
    /// 默认 `false`：打分器给的是精确分数，有上文时把记忆强度当作先验与它一起裁决
    /// （见 [`inprocess::choose_with_user_priors`]）。设为 `true` 回到「记忆一律挡住大模型」的旧行为，
    /// 评测对照用。调用方给不出首选的记忆强度（为 0）时不管这个开关，照旧挡住，见 [`Self::rerank_with_user_priors`]。
    /// HTTP 路径与宿主选词器不受这个开关影响，宿主选词器另见 `chooser_memory_policy`
    pub memory_blocks_scorer: bool,
    /// 首选是个人记忆词、又有上文时，要不要请宿主选词器裁决，见 [`ChooserMemoryPolicy`]。
    /// 默认 [`ChooserMemoryPolicy::Blocked`]（原先的行为）
    pub chooser_memory_policy: ChooserMemoryPolicy,
}

/// 首选是个人记忆词（`[记忆·N次]`）且有上文时，要不要请宿主选词器裁决。
///
/// 进程内打分器给的是精确分数，可以把记忆强度当作先验一起比较（见 [`inprocess::choose_with_user_priors`]）；
/// 宿主选词器（iOS 上是 Apple 的端侧模型）只回答「选哪一个」，没法和记忆强度相加，只能二选一：要么记忆说了算，
/// 要么模型说了算。同一串拼音在不同上文里要选不同的词（「做实验」/「立下誓言」），记忆说了算时，
/// 用户用过其中一个之后，这串拼音就再也轮不到模型；模型说了算时，用户一直只用的那个词也会被模型的错误答案换掉。
///
/// 用 `crates/ime-ffi/examples/ffi_session` 模拟：`--chooser-model` 让本地模型（Qwen2.5-0.5B）扮演只答选项的选词器，
/// `--chooser-noise 0.3` 让它答错三成（答不答错由请求本身的哈希决定，同样的请求总是同样的答案；
/// 30% 的几行是种子 0–4 五次的范围）。测试与 [`inprocess::choose_with_user_priors`] 相同：
/// 同音异境会话（`alternating_cases.tsv` 连续 4 轮，再换一批上文 `alternating_cases_b.tsv`）、
/// 习惯词（同一拼音先把一个词用空格连打 8 遍，再在各自上文里打其余的词，共 63 个）、
/// 教会的词（只算第 1 行经过主动改选的组）、反过来（教会之后，另一个词在它自己明确的上文里），
/// 另加只空格接受过的词（`habit_space_cases.tsv`：一个词在它明确的上文里被接受，再看它到了 4 个中性上文还在不在首位）。2026-11-12：
///
/// | 策略 | 答错率 | 同一批上文第 1~4 轮（共 107） | 换一批上文第 1 轮 | 习惯词之后的其余词（共 63） | 教会的词仍在首位 | 反过来仍能选对 | 只空格接受过的词，到了中性上文 |
/// |---|---|---|---|---|---|---|---|
/// | `Blocked` | 0 | 46 → 62 → 107 → 107 | 47 | 0 | 37/37 | 0/37 | 176/176 |
/// | `Contested` | 0 | 65 → 107 → 107 → 107 | 103 | 19 | 37/37 | 0/37 | 176/176 |
/// | `Always` | 0 | 106 → 107 → 107 → 107 | 103 | 62 | 10/37 | 37/37 | 107/176 |
/// | `Taught` | 0 | 106 → 107 → 107 → 107 | 103 | 62 | 37/37 | 0/37 | 145/176 |
/// | `Blocked` | 30% | 28–35 → 73–79 → 107 → 107 | 47 | 0 | 全部 | 0 | 全部 |
/// | `Contested` | 30% | 40–47 → 105–107 → 106–107 → 107 | 66–75 | 12–15 | 全部 | 0 | 全部 |
/// | `Always` | 30% | 71–76 → 105–107 → 106–107 → 107 | 66–75 | 41–45 | 约 23% | 约 68% | 42%–56% |
/// | `Taught` | 30% | 60–70 → 105–107 → 107 → 107 | 66–75 | 32–37 | 全部 | 0 | 75%–81% |
///
/// （30% 时教过的组随种子是 36–39 组。）习惯词连打 8 遍时，四种做法都没有换错过一次。但这有一部分是模拟的结果：
/// 同样的请求总给同样的答案，模拟的选词器也不看候选的顺序；只空格接受过的词没有上文相关的纠正记忆护着，
/// 每一遍其实都在问选词器。真机上 Apple 模型看到的候选顺序会随个人记忆变，不一定如此。
///
/// - `Blocked` 换一批上文时几乎完全不用模型；`Contested` 好一些，但用户只用过一个词的拼音照样不问，
///   上文明确要另一个词时，习惯词大多错、反过来全错。
/// - `Always` 让模型说了算，用户主动纠正过的词到了中性上文也保不住：答错率 0 时只剩 27%，答错三成时约 23%。
/// - `Taught` 按用户做了什么区分（和打分器的两档先验同一个道理）：空格接受养成的记忆照样交给模型，
///   主动改选过的词不让模型换掉。答错率 0 时会话和习惯词与 `Always` 相同，答错三成时比 `Always` 少对 5–13 个；
///   教会的词和 `Contested` 一样全保住。代价有两处，各要用户纠正一次：只空格接受过的词到了中性上文，
///   约五分之一会被模型换掉（纠正之后就算教过）；教过的词遇到上文明确要另一个词时不会自己换过去
///   （纠正之后这串拼音有了争议，又交给模型）。
///
/// # 真机上的 Apple 端侧模型（iPhone 16 Pro Max，iOS 27.2，2026-11-12，`platforms/ios/DeviceEval` 的 `testD`）
///
/// 第一次见到同音异境用例时 Apple 模型对 70 / 107，正好在上表「答错三成」那一档；但它对同样的请求不总给同样的答案
/// （候选顺序随个人记忆变），这一点模拟没有：
///
/// | 策略 | 同一批上文第 1~3 轮 | 换一批上文第 1~2 轮 | 习惯词之后的其余词（共 63） | 教会的词仍在首位 | 反过来仍能选对 | 只空格接受过的词，到了中性上文 |
/// |---|---|---|---|---|---|---|
/// | `Blocked` | 39 → 69 → 107 | 47 → 67 | 0 | 37/37 | 0/37 | 148/148 |
/// | `Contested` | 48 → 96 → 105 | 67 → 99 | 8 | 37/37 | 0/37 | 148/148 |
/// | `Always` | 71 → 96 → 103 | 71 → 96 | 32 | 23/37 | 23/37 | 109/148 |
/// | `Taught` | 68 → 96 → 103 | 71 → 95 | 29 | 37/37 | 0/37 | 134/148 |
///
/// `Taught` 在上文该说了算的地方比 `Contested` 多对（会话第 1 轮 +20、换一批上文第 1 轮 +4、习惯词 +21），
/// 在记忆本来就对的地方因为又去问 Apple 模型而少对（会话后几轮 2–4 条、只空格接受过的词 14 条）。
/// 各项加起来 `Taught` 658、`Always` 645、`Contested` 635、`Blocked` 541，所以 iOS 用 `Taught`。
/// 每问一次 Apple 模型要等约 450ms；答复晚到时用户可能已经按空格上屏，这一点评测没有量（评测总等到答复）。
/// 没有模拟、也没有量的：记忆随时间衰减（`Taught` 的保护在约 20 天不用之后失效）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChooserMemoryPolicy {
    /// 不请求：个人记忆说了算（原先的行为）
    #[default]
    Blocked,
    /// 只在「有争议」时请求：候选池里至少有两个词各自有近期的个人记忆
    /// （没乘主动改选倍数的记忆强度 [`UserMemory::boost`] 不低于 [`CONTESTED_MEMORY_MIN`]）
    Contested,
    /// 有上文就请求
    Always,
    /// 只挡用户教过的词：首选曾经被主动改选过（[`UserMemory::explicit`]，不随时间消失），而且它的记忆强度
    /// （[`UserMemory::boost`]：空格上屏与主动改选合计、按时间衰减、不乘倍数）不低于 [`CONTESTED_MEMORY_MIN`]，
    /// 这串拼音也没有争议（见 `Contested`）时不请求；其余有上文就请求。
    /// 与进程内打分器的两档先验同一个道理：空格上屏只说明用户不反对，上文明确时让模型改；
    /// 主动改选说明模型对这个用户选错了，不让它再换回去
    Taught,
}

/// [`ChooserMemoryPolicy::Contested`] 里「有个人记忆」的门槛，比较的是调用方给的 [`UserMemory::boost`]
/// （`UserDict::user_boost`，不乘主动改选的倍数 [`EXPLICIT_CHOICE_PRIOR_FACTOR`]——那个倍数只用在打分器的先验上）。
/// 空格上屏一次记 2.8、主动改选一次记 6.5、上限 12，按每秒 1.1e-6 指数衰减（半衰期约 7.3 天）：
/// 只空格上屏过一次的词约 11 天后低于 1.0，主动改选过一次的约 20 天，记满 12 的约 26 天。
/// 很久以前偶然用过一次的词不算，免得陈旧的记忆把模型叫醒
pub const CONTESTED_MEMORY_MIN: f32 = 1.0;

impl Default for DualModeAiOrchestrator {
    fn default() -> Self {
        Self::new(OpenAiCompatibleConfig::default())
    }
}

impl DualModeAiOrchestrator {
    pub fn new(cloud_config: OpenAiCompatibleConfig) -> Self {
        let generation_counter = Arc::new(AtomicU64::new(0));
        let llm_cache: Arc<Mutex<HashMap<String, (String, String)>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let notify_cb: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>> =
            Arc::new(Mutex::new(None));
        let inprocess = Arc::new(InProcessShared::new(Arc::clone(&llm_cache), Arc::clone(&notify_cb)));

        let (async_tx, rx) = mpsc::channel::<AsyncJob>();
        let gen_ref = Arc::clone(&generation_counter);
        let cache_ref = Arc::clone(&llm_cache);
        let cb_ref = Arc::clone(&notify_cb);
        let inprocess_ref = Arc::clone(&inprocess);

        std::thread::spawn(move || {
            while let Ok(mut job) = rx.recv() {
                // 若用户在模型思考期间继续快速打字，直接跳过积压的旧拼音，只推理最新一帧
                while let Ok(newer) = rx.try_recv() {
                    job = newer;
                }
                let job = match job {
                    AsyncJob::Http(job) => job,
                    // 进程内的两种任务各自接住打分器 / 宿主回调的 panic（见 `InProcessShared`），
                    // 这条线程不会因此退出——它一旦退出，之后所有的大模型请求都会被静默丢弃
                    AsyncJob::Score(job) => {
                        inprocess_ref.run_score_job(job);
                        continue;
                    }
                    AsyncJob::Choose(job) => {
                        inprocess_ref.run_choose_job(job);
                        continue;
                    }
                };
                if gen_ref.load(Ordering::SeqCst) != job.gen_id {
                    continue;
                }
                let client = OpenAiCompatibleClient::new(job.config.clone());
                if let Ok(best_word) = client.query_best_candidate_http(
                    &job.pinyin_input,
                    &job.surrounding_context,
                    job.scene,
                    &job.candidates,
                ) {
                    if !best_word.is_empty() {
                        if let Ok(mut guard) = cache_ref.lock() {
                            if guard.len() > 512 {
                                guard.clear();
                            }
                            guard.insert(
                                job.cache_key,
                                (best_word, job.config.provider.short_tag().to_string()),
                            );
                        }
                        if gen_ref.load(Ordering::SeqCst) == job.gen_id {
                            if let Ok(cb_guard) = cb_ref.lock() {
                                if let Some(cb) = cb_guard.as_ref() {
                                    cb();
                                }
                            }
                        }
                    }
                }
            }
        });

        Self {
            local_engine: OnDeviceAiEngine::new(),
            cloud_client: OpenAiCompatibleClient::new(cloud_config),
            generation_counter,
            llm_cache,
            async_tx,
            notify_cb,
            inprocess,
            memory_blocks_scorer: false,
            chooser_memory_policy: ChooserMemoryPolicy::default(),
        }
    }

    /// 安装（`Some`）或卸载（`None`）进程内打分器。
    ///
    /// 安装后，只要大模型重排处于开启状态（`cloud_client.config.enabled`），后台任务就改用打分器而不再发起
    /// HTTP 请求：候选池为 [`select_scoring_pool`]（至多 8 个覆盖整段输入的候选），按 [`choose_by_exact_scores`]
    /// 决定首选，结果同样写入缓存并触发刷新回调。更新的击键会调用 [`CandidateScorer::cancel`] 让在途的打分中途退出，
    /// 同一时刻至多有一个打分任务在执行、一个在排队。
    pub fn set_candidate_scorer(&self, scorer: Option<Arc<dyn CandidateScorer>>) {
        self.inprocess.set_scorer(scorer);
    }

    /// 安装（`Some`）或卸载（`None`）宿主选词器（Swift 一侧的系统模型或云端调用）。
    ///
    /// 只有在没有安装打分器时才会使用：调度器在后台线程上通过 [`HostChooser::callback`] 发出
    /// [`ChooserEvent::Request`]，宿主算完后调用 [`ChooserHandle::deliver`]（或
    /// [`Self::deliver_chooser_result`]）送回选中的文本。同一时刻至多有一个等待中的请求：
    /// - 用户继续输入（缓存键变化）或不再需要结果时，旧请求作废并收到 [`ChooserEvent::Cancel`]；
    /// - 超过 [`HostChooser::timeout`] 仍未答复的请求不再等，迟到的答复被丢弃：请求还登记着时返回
    ///   [`ChooserDelivery::TimedOut`]；同一输入下次重排（重新发出请求，并为旧编号补发 `Cancel`）或输入变了之后，
    ///   旧请求作废，此后的答复返回 [`ChooserDelivery::Stale`]；
    /// - 为已作废或未知编号送达的答复返回 [`ChooserDelivery::Stale`]，不产生任何效果。
    ///
    /// 调用本方法（以及在有等待中的请求时调用 [`Self::set_candidate_scorer`] 安装打分器）会作废等待中的请求，
    /// 对应的 `Cancel` 在**调用线程**上同步发给原先的选词器，而不是后台线程；回调里不得再调用会加锁的接口。
    /// 线程与先后顺序的完整约定见 [`ChooserCallback`]。
    pub fn set_host_chooser(&self, chooser: Option<HostChooser>) {
        self.inprocess.set_chooser(chooser);
    }

    /// 宿主送回选词结果用的句柄（可克隆、可跨线程）
    pub fn chooser_handle(&self) -> ChooserHandle {
        ChooserHandle { shared: Arc::clone(&self.inprocess) }
    }

    /// 等价于 `self.chooser_handle().deliver(id, choice)`
    pub fn deliver_chooser_result(&self, id: u64, choice: Option<&str>) -> ChooserDelivery {
        self.inprocess.deliver(id, choice)
    }

    /// 最新一次 [`Self::rerank`] 是否还在等进程内打分器 / 宿主选词器的结果。
    /// 结果写入缓存（随即触发刷新回调）、打分失败或被取消、宿主答复没有结果或答复不在候选池内、
    /// 请求因新的击键或 [`Self::cancel_pending_llm_work`] 作废、安装或卸载打分器（没有打分器时，安装或卸载选词器）
    /// 之后变回 `false`；宿主选词请求超过 [`HostChooser::timeout`] 仍未答复、同一输入也没有新请求排着队等着发出时
    /// 同样返回 `false`（迟到的答复只会被丢弃：请求还登记着时是 [`ChooserDelivery::TimedOut`]，已经作废时是
    /// [`ChooserDelivery::Stale`]；同一输入再次重排会重新发出请求，又变回 `true`）。
    /// HTTP 路径不参与统计，始终返回 `false`。
    pub fn inprocess_result_pending(&self) -> bool {
        self.inprocess.result_pending()
    }

    /// 放弃所有尚未完成的后台大模型工作（例如输入结束、键盘收起时调用）：
    /// 版本号加一使在途的 HTTP 结果不再触发刷新，取消在途的打分，并作废等待中的宿主请求。
    /// 已经写入缓存的结果不受影响。
    pub fn cancel_pending_llm_work(&self) {
        self.next_generation();
        let tx = &self.async_tx;
        self.inprocess.lookup_or_request(None, &mut |d| Self::dispatch_inprocess(tx, d));
    }

    fn dispatch_inprocess(tx: &mpsc::Sender<AsyncJob>, dispatch: InProcessDispatch) {
        let _ = tx.send(match dispatch {
            InProcessDispatch::Score(job) => AsyncJob::Score(job),
            InProcessDispatch::Choose(job) => AsyncJob::Choose(job),
        });
    }

    /// 把大模型选中的候选提到首位并打上 `[{来源} 优选]` 标签（候选已在首位时只打标签）
    fn promote_llm_pick(report: &mut AiRerankReport, best_word: &str, short_tag: &str) {
        if let Some(pos) = report.candidates.iter().position(|c| c.text == best_word) {
            let tag = format!("[{short_tag} 优选]");
            if pos > 0 {
                let mut picked = report.candidates.remove(pos);
                picked.ai_boosted = true;
                picked.tag = tag;
                report.candidates.insert(0, picked);
                report.top_changed = true;
            } else {
                report.candidates[0].ai_boosted = true;
                report.candidates[0].tag = tag;
            }
        }
    }

    /// 注册当后台大模型完成选词仲裁时的 UI 唤醒回调（例如向候选窗发送 `PostMessageW` 触发重绘）
    pub fn set_async_notify_callback(&self, cb: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut guard) = self.notify_cb.lock() {
            *guard = Some(cb);
        }
    }

    /// 递增并返回最新击键版本号（用于取消未完成的旧请求）
    pub fn next_generation(&self) -> u64 {
        self.generation_counter.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn current_generation(&self) -> u64 {
        self.generation_counter.load(Ordering::SeqCst)
    }

    /// 当用户主动选词或确认词条时，清除该拼音编码对应的旧大模型缓存，确保用户个人高频记忆立即生效。
    ///
    /// 此前投递给进程内打分器、尚未算完的任务同时作废：它们算完也不会再写缓存
    /// （否则一个刚好在清理之后算完的结果会把被清掉的条目写回去）。HTTP 路径的行为不变。
    pub fn invalidate_cache_for_pinyin(&self, pinyin_input: &str) {
        self.inprocess.invalidate_for_pinyin(pinyin_input);
    }

    /// 执行上下文感知选词重排：
    /// 1. 立即执行端侧微模型毫秒级重排（`<0.15ms`）
    /// 2. 若开启了本地 Qwen / 云端大模型，先查 `llm_cache` 是否已有该上下文+拼音的仲裁结果；
    ///    若未命中，则异步投递给后台推理线程，绝不阻塞当前击键！
    ///
    /// 不提供个人记忆：首选是个人记忆词（`[记忆·N次]`）时照旧不请求大模型，见 [`Self::rerank_with_user_priors`]
    pub fn rerank(
        &mut self,
        pinyin_input: &str,
        surrounding_context: &str,
        scene: AppScene,
        l1_candidates: Vec<Candidate>,
    ) -> AiRerankReport {
        self.rerank_with_user_priors(pinyin_input, surrounding_context, scene, l1_candidates, &|_| UserMemory::default())
    }

    /// 与 [`Self::rerank`] 相同，另外由调用方提供「某个候选文本的个人记忆」（[`UserMemory`]：
    /// `UserDict::user_boost` 的记忆强度，以及这个词有没有主动改选过）。首选是个人记忆词、又有上文时：
    /// - 装了进程内打分器：个人记忆不再直接挡住大模型，而是以 [`UserMemory::prior`]（主动改选过的词乘
    ///   [`EXPLICIT_CHOICE_PRIOR_FACTOR`]）作为先验与模型分数一起裁决（`memory_blocks_scorer` 关掉这个例外）；
    /// - 只有宿主选词器：按 [`ChooserMemoryPolicy`] 决定问不问，`Contested` 数的是没乘倍数的
    ///   [`UserMemory::boost`] 不低于 [`CONTESTED_MEMORY_MIN`] 的候选；
    /// - 首选的记忆强度在这里是 0（调用方没有提供，或者首选的记忆来自调用方查不到的记录）：既没有先验可用，
    ///   也数不出争议，照旧挡住大模型（与原先的做法相同）。
    pub fn rerank_with_user_priors(
        &mut self,
        pinyin_input: &str,
        surrounding_context: &str,
        scene: AppScene,
        l1_candidates: Vec<Candidate>,
        user_memory: &dyn Fn(&str) -> UserMemory,
    ) -> AiRerankReport {
        let gen_id = self.next_generation();
        let mut report = self
            .local_engine
            .rerank_candidates(surrounding_context, scene, l1_candidates);

        // 若首选已经是用户个人记忆词（`[记忆·X次]`），始终尊重用户个人词频记忆，不让大模型覆盖
        let top_is_user_memory = report
            .candidates
            .first()
            .is_some_and(|c| c.tag.starts_with("[记忆"));
        // 若输入仅为单音节单字（如单独敲 `lan`、`qiu`、`tuo`），严格保持常用汉字字频排序，不调用异步大模型把生僻字置顶
        let is_single_char_lookup = report
            .candidates
            .first()
            .is_some_and(|c| c.text.chars().count() <= 1 && !c.text.is_ascii());
        let has_context_or_clause =
            !surrounding_context.trim().is_empty() || pinyin_input.len() > 8;

        // 个人记忆首选默认不让大模型覆盖。例外（都要求有上文，且调用方给得出首选的记忆强度）：
        // - 进程内打分器给的是精确分数，把记忆强度当先验一起裁决（`memory_blocks_scorer` 关掉这个例外）；
        // - 宿主选词器按 `chooser_memory_policy` 决定。
        // `[记忆·上文]` 是用户在同样上文下的主动改选，任何情况下都不让大模型改动
        let top_is_context_choice = report
            .candidates
            .first()
            .is_some_and(|c| c.tag.starts_with("[记忆·上文"));
        let has_context = !surrounding_context.trim().is_empty();
        // 首选带着个人记忆标签，调用方给的记忆强度却是 0：旧入口 `rerank` 不提供记忆，或者这条记忆来自调用方
        // 查不到的记录。这时先验是 0、也数不出争议，放行就等于让模型无视用户的记忆，所以照旧挡住
        let top_memory_unseen = || {
            report
                .candidates
                .first()
                .is_some_and(|c| user_memory(&c.text).prior() == 0.0)
        };
        let memory_blocks_llm = if top_is_context_choice {
            true
        } else if !top_is_user_memory {
            false
        } else if !has_context {
            true
        } else if self.inprocess.has_scorer() {
            self.memory_blocks_scorer || top_memory_unseen()
        } else if self.inprocess.chooser_is_active() {
            // 比较的是记忆强度本身（不乘主动改选的倍数），门槛的含义见 `CONTESTED_MEMORY_MIN`
            let recent = |memory: UserMemory| memory.boost.is_finite() && memory.boost >= CONTESTED_MEMORY_MIN;
            let contested = || {
                select_scoring_pool(&report.candidates)
                    .iter()
                    .filter(|c| recent(user_memory(&c.text)))
                    .count()
                    >= 2
            };
            top_memory_unseen()
                || match self.chooser_memory_policy {
                    ChooserMemoryPolicy::Blocked => true,
                    ChooserMemoryPolicy::Always => false,
                    ChooserMemoryPolicy::Contested => !contested(),
                    ChooserMemoryPolicy::Taught => {
                        let top_taught = report.candidates.first().is_some_and(|c| {
                            let memory = user_memory(&c.text);
                            memory.explicit && recent(memory)
                        });
                        top_taught && !contested()
                    }
                }
        } else {
            true
        };
        let llm_wanted = self.cloud_client.config.enabled
            && !memory_blocks_llm
            && !is_single_char_lookup
            && has_context_or_clause
            && pinyin_input.len() >= 2
            && report.candidates.len() >= 2;

        if let Some(cache_prefix) = self.inprocess.active_cache_prefix() {
            // 已安装进程内打分器 / 宿主选词器：不走 HTTP。
            // 即使本次击键不需要大模型结果也要走一遍，上一次击键留下的在途工作才会被取消。
            let mut request = None;
            let mut pool = Vec::new();
            if llm_wanted {
                pool = select_scoring_pool(&report.candidates);
                if pool.len() >= 2 {
                    let pool_texts: Vec<String> = pool.iter().map(|c| c.text.clone()).collect();
                    // 候选池的指纹也是键的一部分：同一上文 + 拼音下，池会随固定分段（选了前缀候选、再撤销）而变，
                    // 为另一个池算出的结果不算命中，在途的旧池任务也要让位。
                    // 指纹放在制表符之前，`invalidate_cache_for_pinyin` 按「\t拼音」后缀清理时照样匹配
                    let pool_hash = inprocess::pool_fingerprint(&pool_texts);
                    request = Some(InProcessRequest {
                        cache_key: format!("{cache_prefix}:{pool_hash:016x}:{surrounding_context}\t{pinyin_input}"),
                        pinyin_input: pinyin_input.to_string(),
                        surrounding_context: surrounding_context.to_string(),
                        priors: pool.iter().map(|c| user_memory(&c.text).prior()).collect(),
                        pool: pool_texts,
                    });
                }
            }
            let tx = &self.async_tx;
            let cached_hit = self
                .inprocess
                .lookup_or_request(request, &mut |d| Self::dispatch_inprocess(tx, d));
            if let Some((best_word, short_tag)) = cached_hit {
                // 与 HTTP 路径相同：缓存词必须仍在本次的候选池内才采纳（键里已有候选池指纹，这里是第二道保险）
                if pool.iter().any(|c| c.text == best_word) {
                    Self::promote_llm_pick(&mut report, &best_word, &short_tag);
                }
            }
        } else if llm_wanted {
            let plausible = OpenAiCompatibleClient::filter_plausible_candidates(&report.candidates);
            if plausible.len() >= 2 {
                let cache_key = format!(
                    "{}:{}:{}\t{}",
                    self.cloud_client.config.provider.key_id(),
                    self.cloud_client.config.model,
                    surrounding_context,
                    pinyin_input
                );

                let cached_hit = self
                    .llm_cache
                    .lock()
                    .ok()
                    .and_then(|g| g.get(&cache_key).cloned());

                if let Some((best_word, short_tag)) = cached_hit {
                    // 仅当缓存词属于合理候选池时才采纳，防止历史脏缓存把生造拼凑词置顶
                    let is_plausible_pick = plausible.iter().any(|c| c.text == best_word);
                    if is_plausible_pick {
                        Self::promote_llm_pick(&mut report, &best_word, &short_tag);
                    }
                } else {
                    // 异步投递给后台线程推理（仅传递过滤后的合理候选池）
                    let _ = self.async_tx.send(AsyncJob::Http(AsyncRerankJob {
                        gen_id,
                        cache_key,
                        pinyin_input: pinyin_input.to_string(),
                        surrounding_context: surrounding_context.to_string(),
                        scene,
                        candidates: plausible,
                        config: self.cloud_client.config.clone(),
                    }));
                }
            }
        }

        // 在完成端侧 AI 与大模型候选池提取后，剔除未夺冠的 `[整句备选]`，
        // 确保候选框第 2~5 项始终是干净的递进短语与前缀词组（如「现在再研究」「现在在」「现在」「先在」）。
        // 注意只按标签剔除：若 AI 推翻了 L1 首选整句，被换下来的原首选整句必须保留在第 2 位，
        // 防止 AI 判断失误时用户再也选不到原本的整句。
        if report.candidates.len() > 1 {
            let mut idx = 0usize;
            report.candidates.retain(|c| {
                let is_first = idx == 0;
                idx += 1;
                is_first || c.tag != "[整句备选]"
            });
        }

        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ime_core::ImeCoreEngine;
    use ime_lexicon::CompiledLexicon;
    use std::path::PathBuf;

    #[test]
    fn test_ai_contextual_disambiguation() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let lexicon = CompiledLexicon::load_or_build_default(root).unwrap();
        let core = ImeCoreEngine::new(lexicon);
        let mut ai = DualModeAiOrchestrator::default();

        // 场景1：微积分语境下输入 "jishu" -> AI 应将 "级数" 排在第一位
        let l1_math = core.decode("jishu", AppScene::Academic, "这道微积分题需要展开成傅里叶");
        let l2_math = ai.rerank(
            "jishu",
            "这道微积分题需要展开成傅里叶",
            AppScene::Academic,
            l1_math.candidates,
        );
        assert_eq!(l2_math.candidates[0].text, "级数");

        // 场景2：软件工程语境下输入 "jishu" -> AI 应将 "技术" 排在第一位
        let l1_code = core.decode("jishu", AppScene::CodeEditor, "我们团队正在攻关核心计算机");
        let l2_code = ai.rerank(
            "jishu",
            "我们团队正在攻关核心计算机",
            AppScene::CodeEditor,
            l1_code.candidates,
        );
        assert_eq!(l2_code.candidates[0].text, "技术");

        // 场景3：释怀语境 vs 喜剧语境下输入 "buzaiyile" -> "不在意了" vs "不再一乐"
        let l1_sad = core.decode("buzaiyile", AppScene::Chat, "算了，过去的事我已经");
        let l2_sad = ai.rerank(
            "buzaiyile",
            "算了，过去的事我已经",
            AppScene::Chat,
            l1_sad.candidates,
        );
        assert_eq!(l2_sad.candidates[0].text, "不在意了");

        let l1_happy = core.decode("buzaiyile", AppScene::Chat, "听完这段搞笑相声怎么");
        let l2_happy = ai.rerank(
            "buzaiyile",
            "听完这段搞笑相声怎么",
            AppScene::Chat,
            l1_happy.candidates,
        );
        assert_eq!(l2_happy.candidates[0].text, "不再一乐");

        // 场景4：扩展同音词搭配矩阵测试（权利 vs 权力，登录 vs 登陆）
        let l1_rights = core.decode("quanli", AppScene::Document, "宪法保障公民的合法");
        let l2_rights = ai.rerank(
            "quanli",
            "宪法保障公民的合法",
            AppScene::Document,
            l1_rights.candidates,
        );
        assert_eq!(l2_rights.candidates[0].text, "权利");

        let l1_power = core.decode("quanli", AppScene::Document, "坚决防止国家机关滥用");
        let l2_power = ai.rerank(
            "quanli",
            "坚决防止国家机关滥用",
            AppScene::Document,
            l1_power.candidates,
        );
        assert_eq!(l2_power.candidates[0].text, "权力");

        let l1_login = core.decode("denglu", AppScene::CodeEditor, "请输入账号密码扫码");
        let l2_login = ai.rerank(
            "denglu",
            "请输入账号密码扫码",
            AppScene::CodeEditor,
            l1_login.candidates,
        );
        assert_eq!(l2_login.candidates[0].text, "登录");

        let l1_typhoon = core.decode("denglu", AppScene::General, "超强台风即将在东南沿海");
        let l2_typhoon = ai.rerank(
            "denglu",
            "超强台风即将在东南沿海",
            AppScene::General,
            l1_typhoon.candidates,
        );
        assert_eq!(l2_typhoon.candidates[0].text, "登陆");
    }

    fn synthetic_cand(text: &str, score: f32, segments: &[&str]) -> Candidate {
        Candidate {
            text: text.to_string(),
            preedit: String::new(),
            consumed_len: 10,
            l1_cost: 100.0 - score,
            final_score: score,
            ai_boosted: false,
            tag: String::new(),
            segments: segments.iter().map(|s| s.to_string()).collect(),
            segment_spans: Vec::new(),
            syllable_spans: Vec::new(),
        }
    }

    #[test]
    fn test_collocation_rules_do_not_misfire_on_single_char_triggers() {
        let mut ai = DualModeAiOrchestrator::default();
        // 「侦查机关」含「机」字，不应被误判为军事侦察语境而变成「侦察机关」
        let r = ai.rerank(
            "zhenchajiguan",
            "",
            AppScene::General,
            vec![
                synthetic_cand("侦查机关", 99.0, &["侦查", "机关"]),
                synthetic_cand("侦察机关", 98.5, &["侦察", "机关"]),
            ],
        );
        assert_eq!(r.candidates[0].text, "侦查机关");
        // 上文含「产品」时，「必须」不应被误改为「必需」
        let r2 = ai.rerank(
            "bixu",
            "这款产品",
            AppScene::General,
            vec![
                synthetic_cand("必须", 99.0, &["必须"]),
                synthetic_cand("必需", 98.5, &["必需"]),
            ],
        );
        assert_eq!(r2.candidates[0].text, "必须");
        // 上文含「用户」时，「爆发」不应被误改为「暴发」
        let r3 = ai.rerank(
            "baofa",
            "很多用户的不满终于",
            AppScene::General,
            vec![
                synthetic_cand("爆发", 99.0, &["爆发"]),
                synthetic_cand("暴发", 98.5, &["暴发"]),
            ],
        );
        assert_eq!(r3.candidates[0].text, "爆发");
    }

    #[test]
    fn test_ai_flip_keeps_original_top_sentence_as_alternative() {
        let mut ai = DualModeAiOrchestrator::default();
        // L1 首选「现在在研究一下」被端侧语法 AI 推翻为「现在再研究一下」后，原首选整句仍应保留可选
        let mut backup = synthetic_cand("现在再研究一下", 90.0, &["现在", "再", "研究", "一下"]);
        backup.tag = "[整句备选]".to_string();
        let mut noise = synthetic_cand("现在载研究一下", 89.0, &["现在", "载", "研究", "一下"]);
        noise.tag = "[整句备选]".to_string();
        let mut prefix = synthetic_cand("现在", 80.0, &["现在"]);
        prefix.consumed_len = 6;
        let r = ai.rerank(
            "xianzaizaiyanjiuyixia",
            "",
            AppScene::General,
            vec![
                synthetic_cand("现在在研究一下", 99.0, &["现在", "在", "研究", "一下"]),
                backup,
                noise,
                prefix,
            ],
        );
        let texts: Vec<&str> = r.candidates.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts[0], "现在再研究一下");
        assert!(texts.contains(&"现在在研究一下"), "被 AI 换下的原首选整句必须保留: {texts:?}");
        assert!(!texts.contains(&"现在载研究一下"), "未夺冠的整句备选应被剔除: {texts:?}");
        assert!(texts.contains(&"现在"));
    }

    #[test]
    fn test_multi_provider_config_switching() {
        let mut cfg = OpenAiCompatibleConfig::default();
        assert_eq!(cfg.provider, AiProvider::LocalQwen);
        assert_eq!(cfg.model, "qwen2.5:1.5b");

        cfg.switch_provider(AiProvider::OpenAiChatGpt);
        assert_eq!(cfg.api_base, "https://api.openai.com/v1");
        assert_eq!(cfg.model, "gpt-4o-mini");
        cfg.api_key = "sk-test-openai".to_string();

        cfg.switch_provider(AiProvider::AnthropicClaude);
        assert_eq!(cfg.api_base, "https://api.anthropic.com/v1");
        assert_eq!(cfg.model, "claude-3-5-haiku-latest");
        cfg.api_key = "sk-ant-test".to_string();

        cfg.switch_provider(AiProvider::GoogleGemini);
        assert_eq!(cfg.api_base, "https://generativelanguage.googleapis.com/v1beta");
        assert_eq!(cfg.model, "gemini-2.5-flash");

        cfg.switch_provider(AiProvider::CustomCloud);
        assert_eq!(cfg.api_base, "https://api.deepseek.com/v1");
        assert_eq!(cfg.model, "deepseek-chat");

        // 切回 OpenAI 时应完整保留之前填写的 Key
        cfg.switch_provider(AiProvider::OpenAiChatGpt);
        assert_eq!(cfg.api_key, "sk-test-openai");
    }

    #[test]
    fn test_short_phrase_plausibility_and_user_memory_priority() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let lexicon = CompiledLexicon::load_or_build_default(root).unwrap();
        let mut core = ImeCoreEngine::new(lexicon);

        // 1. 短句/短词合理性过滤：拦截生造散字拼凑词（如 "在乳突"、"可以乐"、"玄此"）
        let l1_zairutu = core.decode("zairutu", AppScene::General, "但是在");
        assert_eq!(l1_zairutu.candidates[0].text, "在如图");
        let plausible_zairutu =
            OpenAiCompatibleClient::filter_plausible_candidates(&l1_zairutu.candidates);
        assert!(
            !plausible_zairutu.iter().any(|c| c.text == "在乳突"),
            "生造散字组合 '在乳突' 不应进入大模型候选池"
        );

        let l1_keyile = core.decode("keyile", AppScene::General, "");
        assert_eq!(l1_keyile.candidates[0].text, "可以了");
        let plausible_keyile =
            OpenAiCompatibleClient::filter_plausible_candidates(&l1_keyile.candidates);
        assert_eq!(plausible_keyile.len(), 1);
        assert_eq!(plausible_keyile[0].text, "可以了");

        let l1_xuanci = core.decode("xuanci", AppScene::General, "还有大模型");
        assert_eq!(l1_xuanci.candidates[0].text, "选词");
        let plausible_xuanci =
            OpenAiCompatibleClient::filter_plausible_candidates(&l1_xuanci.candidates);
        assert_eq!(plausible_xuanci.len(), 1);
        assert_eq!(plausible_xuanci[0].text, "选词");

        // 2. 长句中避免散字拼凑 "急一" 压过双字词 "记忆"
        let l1_long = core.decode(
            "haiyoudamoxingxuancisihubunengjiyigaopingci",
            AppScene::General,
            "",
        );
        let pos_good = l1_long
            .candidates
            .iter()
            .position(|c| c.text == "还有大模型选词似乎不能记忆高频词")
            .expect("应包含 '还有大模型选词似乎不能记忆高频词'");
        assert!(pos_good <= 1);

        // 3. 用户高频记忆置顶保护：确认一次后即带 [记忆·1次] 标签，不被大模型覆盖
        core.commit_candidate_ex("xuanci", "选词", None, true);
        let l1_after_commit = core.decode("xuanci", AppScene::General, "");
        assert_eq!(l1_after_commit.candidates[0].text, "选词");
        assert!(l1_after_commit.candidates[0].tag.starts_with("[记忆"));
    }

    #[test]
    fn test_lm_margin_decision_rule() {
        // L1 首选略差但差距不够：保持首选（防抖）
        assert_eq!(openai_client::choose_by_lm_margin(&[-5.0, -4.5, -9.0], false), 0);
        // 无上文时需要 ≥2.3 nats 才替换
        assert_eq!(openai_client::choose_by_lm_margin(&[-8.0, -5.0], false), 1);
        // 有上文时 ≥1.2 nats 即可替换
        assert_eq!(openai_client::choose_by_lm_margin(&[-6.0, -4.6], true), 1);
        assert_eq!(openai_client::choose_by_lm_margin(&[-6.0, -5.0], true), 0);
    }

    #[test]
    fn test_thinking_model_handling_and_truncated_think_guard() {
        let cands = vec![
            Candidate {
                text: "技术".to_string(),
                preedit: "ji'shu".to_string(),
                consumed_len: 5,
                l1_cost: 1.0,
                final_score: 99.0,
                ai_boosted: false,
                tag: String::new(),
                segments: vec!["技术".to_string()],
                segment_spans: Vec::new(),
                syllable_spans: Vec::new(),
            },
            Candidate {
                text: "级数".to_string(),
                preedit: "ji'shu".to_string(),
                consumed_len: 5,
                l1_cost: 1.2,
                final_score: 98.0,
                ai_boosted: false,
                tag: String::new(),
                segments: vec!["级数".to_string()],
                segment_spans: Vec::new(),
                syllable_spans: Vec::new(),
            },
        ];

        // 1. Qwen3 模型自动注入 `/no_think` 软开关
        let mut cfg = OpenAiCompatibleConfig::default();
        cfg.model = "qwen3:1.7b".to_string();
        let client = OpenAiCompatibleClient::new(cfg);
        let req = client.build_rerank_request("jishu", "微积分傅里叶", AppScene::Academic, &cands);
        assert!(req.messages[1].content.ends_with("/no_think"));

        // 2. 完整闭合 `<think>...</think>`：正常提取思考后的候选词
        let closed_think = "<think>\n上文是微积分傅里叶，第1项是技术，第2项是级数，应选级数。\n</think>\n级数";
        assert_eq!(
            openai_client::sanitize_llm_candidate_output(closed_think, &cands),
            "级数"
        );

        // 3. 未闭合的截断 `<think>`：绝不能从思考草稿中误抓候选词，应安全回退至第 1 项首选
        let truncated_think = "<think>\n用户让我从 1. 技术 和 2. 级数 中选择一个，我们来看看级数是否";
        assert_eq!(
            openai_client::sanitize_llm_candidate_output(truncated_think, &cands),
            "技术"
        );
    }

    /// 打印候选分页结构：`cargo test -p ime-ai dump_candidate_pages -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dump_candidate_pages() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let lexicon = CompiledLexicon::load_or_build_default(root).unwrap();
        let core = ImeCoreEngine::new(lexicon);
        let mut ai = DualModeAiOrchestrator::default();
        for py in ["zairenwulanshang", "haiyoujiushiguoduzhengjucaici", "zhileide", "shi"] {
            let l1 = core.decode(py, AppScene::General, "");
            let l2 = ai.rerank(py, "", AppScene::General, l1.candidates);
            println!("== {py} ({} 个候选)", l2.candidates.len());
            for (p, page) in l2.candidates.chunks(5).take(4).enumerate() {
                let items: Vec<String> = page
                    .iter()
                    .map(|c| format!("{}{}", c.text, if c.consumed_len < py.len() { "…" } else { "" }))
                    .collect();
                println!("  第{}页: {}", p + 1, items.join(" | "));
            }
        }
    }

    /// 需本机内置引擎在线：`cargo test -p ime-ai live_lm_scoring -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_lm_scoring_on_user_reported_cases() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let lexicon = CompiledLexicon::load_or_build_default(root).unwrap();
        let core = ImeCoreEngine::new(lexicon);
        let mut cfg = OpenAiCompatibleConfig::default();
        cfg.enabled = true;
        cfg.api_base = "http://127.0.0.1:18434/v1".to_string();
        cfg.model = "qwen2.5-1.5b-builtin".to_string();
        let client = OpenAiCompatibleClient::new(cfg);

        let cases: [(&str, &str, &str); 12] = [
            ("zairenwulanshang", "", "在任务栏上"),
            ("zairenwulanshang", "设置的图标没有显示", "在任务栏上"),
            ("zhileide", "", "之类的"),
            ("zhileide", "比如改变颜色", "之类的"),
            ("erbendimoxingbufen", "", "而本地模型部分"),
            ("aixuanci", "", ""),
            ("aixuanci", "还有在我简短的测试中，", ""),
            ("zairutu", "但是", "在如图"),
            ("keyile", "", "可以了"),
            ("xuanci", "大模型", "选词"),
            ("jishu", "这道微积分题需要展开成傅里叶", "级数"),
            ("jishu", "我们团队正在攻关核心计算机", "技术"),
        ];
        let mut failures = Vec::new();
        for (py, ctx, expect) in cases {
            let l1 = core.decode(py, AppScene::General, ctx);
            let pool = OpenAiCompatibleClient::filter_plausible_candidates(&l1.candidates);
            let t = std::time::Instant::now();
            let scores = OpenAiCompatibleClient::score_candidates_by_lm(
                "127.0.0.1:18434",
                ctx,
                &pool.iter().take(4).cloned().collect::<Vec<_>>(),
                2000,
            );
            let pick = client.query_best_candidate_http(py, ctx, AppScene::General, &l1.candidates);
            println!(
                "PY={py:<22} CTX={ctx:<14} L1={:?}\n    pool={:?}\n    scores={:?}\n    pick={pick:?} ({} ms)\n",
                l1.candidates.iter().take(4).map(|c| c.text.as_str()).collect::<Vec<_>>(),
                pool.iter().take(4).map(|c| c.text.as_str()).collect::<Vec<_>>(),
                scores.map(|v| v.iter().map(|s| (s * 100.0).round() / 100.0).collect::<Vec<_>>()),
                t.elapsed().as_millis()
            );
            if !expect.is_empty() && pick.as_deref().ok() != Some(expect) {
                failures.push(format!("{py}|{ctx} -> {pick:?}, expected {expect}"));
            }
        }
        assert!(failures.is_empty(), "failures: {failures:#?}");
    }
}
