//! 不经过 HTTP 的大模型选词后端接口：进程内打分器与宿主选词器。
//!
//! Windows 版的大模型选词通过 HTTP 访问本机 `llama-server` 或云端接口（见 `openai_client`）。
//! iOS 键盘扩展里没有子进程，也不适合在 Rust 一侧发起网络请求，因此 [`crate::DualModeAiOrchestrator`]
//! 另外提供两个挂载点，二者都沿用原有的「后台计算 → 写入 `llm_cache` → 触发刷新回调」流程：
//!
//! 1. [`CandidateScorer`]：进程内打分器（`ime-llm` 用 llama.cpp 实现）。后台线程把上文与候选池交给它，
//!    得到每个候选的精确对数似然，再由 [`choose_by_exact_scores`] 决定首选
//! 2. [`HostChooser`]：宿主选词器。调度器通过回调把请求（编号、缓存键、上文、拼音、候选文本）交给宿主
//!    （Swift 一侧调用 FoundationModels 的系统模型，或用 URLSession 访问云端），宿主算完后调用
//!    [`ChooserHandle::deliver`] 把选中的文本送回来
//!
//! 两者同时安装时只使用打分器；只有在没有安装打分器时才使用宿主选词器。两者都没有安装时，
//! 调度器的行为与之前完全相同（HTTP 路径）。
//!
//! 本模块不依赖任何推理库：`ime-ai` 只定义接口，具体实现由依赖它的 crate 或宿主提供。
//!
//! # 过期与取消
//!
//! 每次 `rerank` 都会重新登记「最新一次击键在等哪个缓存键」（候选池 + 上文 + 拼音；不需要大模型时为空）：
//!
//! - **同一个键再次重排**（候选窗刷新、光标移动）：在途的工作继续，不重复投递；
//! - **键变了或不再需要结果**：在途的打分通过 [`CandidateScorer::cancel`] 中途退出；等待中的宿主请求作废，
//!   宿主收到 [`ChooserEvent::Cancel`]，之后为该编号送达的答复返回 [`ChooserDelivery::Stale`]；
//! - **任务不会堆积**：后台线程每次只取队列里最新的一个任务，较早的直接丢弃。即使打分器完全不响应取消，
//!   同一时刻也至多一个任务在执行、一个在排队；宿主一侧同一时刻至多一个等待中的请求；
//! - **取消来得太晚**（后台线程刚通过过期检查、还没进入 `score`，取消就已发出）：这次打分会跑完，
//!   结果对它自己的键仍然正确，照常写入缓存，但不触发刷新回调；代价只是多算一次；
//! - **用户上屏让结果作废**：`invalidate_cache_for_pinyin`（用户选词 / 上屏时调用）之前投递的打分任务，
//!   即使之后才算完也不写缓存——那次清理本来就是要去掉它。同一输入之后再次重排时会重新投递；
//! - **打分器或宿主回调 panic**：后台线程接住 panic 继续工作，只清理出事的那一个任务（不写缓存、不再等它），
//!   同一输入下次重排时重新请求；
//! - **打分结果不可用**（含 NaN，或全部是 `-inf`）：按打分失败处理，不写缓存、不打标签；
//! - **宿主请求超时**（[`HostChooser::timeout`]）：迟到的答复一律丢弃——候选列表在用户敲完几秒之后突然换首选，
//!   比不换更糟。请求还登记着时，答复返回 [`ChooserDelivery::TimedOut`]；同一输入再次重排（重新发出请求）、
//!   输入变了或结束之后，旧请求随之作废，宿主收到 [`ChooserEvent::Cancel`]，此后的答复返回 [`ChooserDelivery::Stale`]。
//!   超时之后 `inprocess_result_pending` 即返回 `false`；同一输入下次重排时会重新发出请求（又开始等）；
//! - **刷新回调**只在「结果刚写入缓存，且最新一次击键等的正是这个键」时触发，每个结果至多一次。
//!   打分器的结果在后台线程上触发，宿主选词器的结果在调用 [`ChooserHandle::deliver`] 的线程上触发。

use ime_core::Candidate;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// 交给进程内打分器 / 宿主选词器的候选池上限
pub const SCORING_POOL_MAX: usize = 8;

/// 有上文时，替换当前首选所需的最小对数似然优势（nat）。0 即直接取最高分。
/// 取值依据见 [`choose_by_exact_scores`]。
pub const EXACT_SCORE_MARGIN_WITH_CONTEXT: f64 = 0.0;

/// 无上文时，替换当前首选所需的最小对数似然优势（nat）。
/// 没有语境时词库词频与规则重排更可靠，沿用 HTTP 路径的 e^2.3 ≈ 10 倍门槛。
/// 取值依据见 [`choose_by_exact_scores`]。
pub const EXACT_SCORE_MARGIN_NO_CONTEXT: f64 = 2.3;

/// 打分器的错误类型
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScoreError {
    /// 请求被 [`CandidateScorer::cancel`] 取消
    #[error("打分请求已取消")]
    Cancelled,
    #[error("打分失败: {0}")]
    Failed(String),
}

/// 进程内候选打分器。
///
/// 实现必须是 `Send + Sync`：[`CandidateScorer::score`] 在调度器的后台线程上调用，
/// [`CandidateScorer::cancel`] 在击键线程上调用。
pub trait CandidateScorer: Send + Sync {
    /// 模型标识（如 GGUF 文件名）。参与结果缓存键，更换模型后旧缓存自然失效。
    fn model_id(&self) -> String;

    /// 采纳结果时显示在候选标签里的简称（`[{简称} 优选]`）
    fn short_tag(&self) -> String {
        "端侧AI".to_string()
    }

    /// 返回每个候选在上文之后的对数似然（自然对数），顺序与 `candidates` 一致。
    ///
    /// 分数只需在同一次调用的候选之间可比。打分文本应按 [`crate::lm_text`] 的规则构造
    /// （引导语 + 上文末尾 + 规范化后的候选）：前缀用 [`crate::lm_text::lm_scoring_prefix_inprocess`]，
    /// 上文不超过 48 个字符时与 HTTP 路径的文本逐字节相同。
    ///
    /// 某个候选没有可比的分数时返回 `-inf`（它永远不会被选中）；含 NaN 或全部为 `-inf` 的结果按打分失败处理，
    /// 见 [`scores_are_usable`]。
    fn score(&self, context: &str, candidates: &[String]) -> Result<Vec<f64>, ScoreError>;

    /// 协作式取消：让所有在此之前发起、尚未完成的 [`CandidateScorer::score`] 尽快返回
    /// [`ScoreError::Cancelled`]，并且不影响之后发起的请求。在击键线程上调用，不得阻塞。
    fn cancel(&self);
}

/// 发给宿主选词器的一次请求
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChooserRequest {
    /// 请求编号（单调递增）；答复时原样传回 [`ChooserHandle::deliver`]
    pub id: u64,
    /// 结果缓存键（宿主可用来合并重复请求，不需要解析其内容）
    pub cache_key: String,
    /// 光标前的上文
    pub context: String,
    /// 当前的拼音输入串
    pub pinyin: String,
    /// 候选文本，第 0 项是当前首选。宿主只能从中选一个
    pub candidates: Vec<String>,
}

/// 调度器发给宿主选词器的事件
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChooserEvent {
    /// 新的选词请求
    Request(ChooserRequest),
    /// 编号为 `id` 的请求已经作废（用户继续输入、等待超时、安装了打分器，或选词器被卸载 / 替换），
    /// 宿主可以中止对应的任务。之后再为该编号送达的答复会被忽略。
    ///
    /// 不保证在同一编号的 [`ChooserEvent::Request`] 之后到达：请求先登记、后在后台线程上发出，
    /// 这期间别的线程安装打分器或更换选词器，会在它自己的线程上先发出 `Cancel`，同一编号的 `Request` 随后才到。
    /// 宿主要么按编号记住已经作废的请求，要么照常处理迟到的 `Request`——它的答复只会得到
    /// [`ChooserDelivery::Stale`]，不影响正确性。
    Cancel { id: u64 },
}

/// 宿主选词器的回调。应当立刻返回（把实际推理放进宿主自己的异步任务）。
///
/// 调用线程不固定：
/// - [`ChooserEvent::Request`]，以及因用户继续输入、等待超时而发出的 [`ChooserEvent::Cancel`]，
///   在调度器的后台线程上调用；
/// - 安装打分器（`set_candidate_scorer`）或安装 / 卸载 / 替换选词器（`set_host_chooser`）时作废等待中的请求，
///   `Cancel` 在**调用这两个方法的线程**上同步发出，此时调用方往往还持有它自己的锁
///   （经 `ime-ffi` 调用时是会话锁）。
///
/// 因此回调里不能再调用调度器所有者的任何会加锁的接口（否则自己等自己），也不要假设事件之间的线程亲和性。
/// 两个线程上的事件没有先后保证，见 [`ChooserEvent::Cancel`]。回调里 panic 会被后台线程接住，那一次请求作废。
pub type ChooserCallback = Arc<dyn Fn(ChooserEvent) + Send + Sync>;

/// 宿主选词器的注册信息
#[derive(Clone)]
pub struct HostChooser {
    /// 后端标识（如 `apple_fm`、`cloud`）。参与结果缓存键
    pub backend_id: String,
    /// 采纳结果时显示在候选标签里的简称（`[{简称} 优选]`）
    pub short_tag: String,
    /// 单次请求的等待时限：超过后不再等它，迟到的答复被丢弃（见 [`ChooserDelivery::TimedOut`]）
    pub timeout: Duration,
    pub callback: ChooserCallback,
}

impl HostChooser {
    /// 默认等待时限
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(4000);

    pub fn new(backend_id: impl Into<String>, short_tag: impl Into<String>, callback: ChooserCallback) -> Self {
        Self {
            backend_id: backend_id.into(),
            short_tag: short_tag.into(),
            timeout: Self::DEFAULT_TIMEOUT,
            callback,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// [`ChooserHandle::deliver`] 对一次答复的处理结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChooserDelivery {
    /// 已采纳：写入结果缓存；若最新一次击键仍在等这个结果，同时触发刷新回调
    Accepted,
    /// 宿主表示没有结果（`None` 或空串）：请求结束，不写缓存，候选保持规则重排的结果
    NoAnswer,
    /// 答复的文本不在该请求的候选池内：丢弃
    Implausible,
    /// 编号不是当前等待中的请求（已经作废：被更新的击键或超时后重新发出的请求取代、不再需要结果、换了后端；
    /// 已经答复过；或从未发出）：丢弃
    Stale,
    /// 请求还登记着，但超过等待时限才送达：丢弃（超时的请求作废之后才送达的是 [`Self::Stale`]）
    TimedOut,
}

/// 挑选交给进程内打分器 / 宿主选词器的候选池：按当前排序取前 [`SCORING_POOL_MAX`] 个「合理」候选，
/// 第 0 项是当前首选。合理性约束：
///
/// 1. 必须覆盖整段输入——只覆盖部分输入的前缀候选与整句候选长度不同，似然不可比；
/// 2. 文本（忽略显示用空格）去重；纯 ASCII 候选另外忽略大小写，只留排在前面的那个
///    （`Docker` / `docker` 是同一个词的两种写法，哪种写法更「通顺」不该由语言模型决定）；
/// 3. 剔除模糊音纠错候选（`[纠错…]`，当前首选除外）——语言模型只知道哪个词更常见，
///    不知道用户实际敲的是哪个音，放进池里只会让更常见的近音词顶掉用户敲对的词（「自拍」→「指派」）；
/// 4. 当前首选是词库整词（单段、非纠错）时，只允许其它整词参与——与 HTTP 路径相同的保护，
///    防止「可以 + 乐」这类多段拼凑推翻词库整词；
/// 5. 当前首选是用户原样敲出来的英文（文本是纯 ASCII，且编码 [`Candidate::preedit`] 去掉分隔符后的字母数等于
///    [`Candidate::consumed_len`]，如 `CPU<cpu>`、`Intel<intel>`、`bug<bug>`）时，剔除编码比输入长的候选：
///    简拼展开（`菜谱<cai'pu>`、`失去了<shi'qu'le>`）与英文补全（`intelligent<intelligent>`）是在猜用户没敲出来的字母。
///    理由与第 3 条相同：模型只看得到上文之后哪个词更通顺，看不到用户敲的就是 c-p-u。
///    编码与输入等长的候选不受影响（`AI<ai>` 与 `爱<ai>`、`API<api>` 与 `阿批<a'pi>` 是真正的歧义，仍交给模型）。
///
/// 第 5 条只管「英文首选敲全了」这一种情况，其余的候选池与原来完全相同：
/// - 首选本身就是简拼展开或补全时（`pc` → 赔偿、`mac` → 马车、`java` → JavaScript），池里的候选都是同一类猜测，
///   敲全了的候选（`Mac<mac>`、`java<java>`）也照常参与；
/// - 首选是中文时不剔除。全拼输入下绝大多数候选的编码本来就与输入等长（`shiyan` → 实验 / 誓言 / 食盐）；
///   少数更长的（`qigu` 下的 `齐国<qi'guo>`）是解码器对「还没敲完」的预测，中文词之间用似然比较是公平的，
///   实测模型没有因此改错过（`phrase_split_cases.tsv` 里有 15 个这样的池）。而且中文整句的编码不能按字母数判断
///   是否敲全：`卫星城镇有<wei'xing'cheng'zhen y>` 末尾的简拼只显示敲出来的那个字母，
///   与 `卫星城真有<… zhen'you>` 其实是同一类候选；
/// - 没有编码信息的候选（`preedit` 为空）、输入里带用户自己敲的分隔符（`consumed_len` 比字母数多）时同样不剔除。
///
/// 与 HTTP 路径的 [`crate::OpenAiCompatibleClient::filter_plausible_candidates`]（上限 4 个，并按解码代价差剔除）
/// 相比，这里不按代价差剔除、上限放宽到 8 个：那条过滤是为了防止 Top-256 近似打分与生成式选择题被凑数候选带偏；
/// 在中文同音词之间，精确的整词表对数似然能把不通顺的候选压到很低，按代价过滤、只取前 4 个反而会把语境需要的
/// 低频同音词（如「誓言」「边城」）挡在候选池外。各种候选池策略的实测对照见 `tools/eval --explore`。
///
/// 但似然**不能**拿来让中文词与用户原样敲出的英文竞争，这正是第 5 条的来历：英文缩写接在中文上文之后，
/// 似然天然低于常见的中文词。没有第 5 条时，任意不相关的上文之后敲 `cpu`，Qwen2.5-0.5B 会把首选从 CPU 换成
/// 「菜谱」，`sql` → 失去了，`intel` → intelligent，`docker` → docker。`data/eval/english_cases.tsv`（68 个规则重排
/// 本来就对的英文 / 中英混输用例）上，没有第 5 条与大小写去重时 Qwen2.5-0.5B 改错 13 个、Qwen3-0.6B 改错 6 个，
/// 有了之后都是 0 个；其余用例集的候选池一个都没有变。
pub fn select_scoring_pool(candidates: &[Candidate]) -> Vec<Candidate> {
    let max_len = candidates.iter().map(|c| c.consumed_len).max().unwrap_or(0);
    let is_fuzzy = |c: &Candidate| c.tag.starts_with("[纠错");
    let mut top_is_exact_word = false;
    // 首选是否是用户原样敲出来的英文（第 5 条）
    let mut top_is_typed_english = false;
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Candidate> = Vec::with_capacity(SCORING_POOL_MAX);
    for c in candidates {
        if c.consumed_len != max_len || c.text.is_empty() {
            continue;
        }
        if out.is_empty() {
            top_is_exact_word = c.segments.len() == 1 && !is_fuzzy(c);
            top_is_typed_english =
                c.text.is_ascii() && code_letter_count(&c.preedit) == max_len;
        } else if is_fuzzy(c)
            || (top_is_exact_word && c.segments.len() != 1)
            || (top_is_typed_english && code_letter_count(&c.preedit) > max_len)
        {
            continue;
        }
        if !seen.insert(pool_dedupe_key(&c.text)) {
            continue;
        }
        out.push(c.clone());
        if out.len() >= SCORING_POOL_MAX {
            break;
        }
    }
    out
}

/// 候选编码（[`Candidate::preedit`]）的字母数：不计音节分隔符 `'` 与词间空格。没有编码时为 0
fn code_letter_count(preedit: &str) -> usize {
    preedit.chars().filter(|c| *c != '\'' && !c.is_whitespace()).count()
}

/// 候选池去重用的键：去掉显示用空格；纯 ASCII 文本另外忽略大小写
fn pool_dedupe_key(text: &str) -> String {
    let bare = strip_whitespace(text);
    if bare.is_ascii() {
        bare.to_ascii_lowercase()
    } else {
        bare
    }
}

/// 候选池文本的指纹，放进结果缓存键里：同一上文 + 拼音下，候选池会随固定分段（选了前缀候选再撤销）而变，
/// 为旧池算出的结果不能算作新池的缓存命中，在途的旧池任务也不能算作「正在算这一个」。
/// 只要求在进程内稳定（缓存不落盘）。
pub(crate) fn pool_fingerprint(pool: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    pool.hash(&mut hasher);
    hasher.finish()
}

/// 一组分数能不能用来裁决：含 NaN（数值故障），或者没有任何一个高于 `-inf`（没有一个候选打得出分），都不能。
/// 个别候选为 `-inf` 是合法的（打分器表示「这个候选没有可比的分数，不能选它」）。
pub fn scores_are_usable(scores: &[f64]) -> bool {
    !scores.iter().any(|s| s.is_nan()) && scores.iter().any(|s| *s > f64::NEG_INFINITY)
}

/// 根据精确对数似然（索引 0 为当前首选）决定最终首选，返回被选中的下标。
///
/// - 有上文：取最高分（[`EXACT_SCORE_MARGIN_WITH_CONTEXT`] = 0）。
/// - 无上文：最高分须比当前首选高出 [`EXACT_SCORE_MARGIN_NO_CONTEXT`]（2.3 nat）才替换。
///
/// 分数含 NaN 或为空时保持当前首选（调度器在此之前就用 [`scores_are_usable`] 把这类结果当作打分失败，
/// 不写缓存也不打标签；这里的兜底是给直接调用本函数的代码准备的）。
///
/// `-inf` 表示「这个候选不能选」，按普通的数值比较处理：`-inf` 的候选永远不会被选中；
/// 当前首选是 `-inf` 而别的候选有分数时，换成其中分数最高的（有无上文都一样，差值是 `+inf`，必然超过阈值）；
/// 全部是 `-inf` 时保持当前首选。
///
/// # 取值依据
///
/// 用 `tools/eval --explore` 在 [`select_scoring_pool`] 的候选池上实测（Qwen2.5-0.5B / Qwen3-0.6B，Q4_K_M；
/// 「改错」指规则重排本来正确、被大模型换成错误候选的用例数）：
///
/// | 用例集 | 仅规则 | 阈值 0 | 阈值 0.5 | 阈值 1.2 | 阈值 2.0 |
/// |---|---|---|---|---|---|
/// | `context_cases.tsv`（50 个，有上文） | 38 | 45 / 46，改错 0 | 44 / 46 | 43 / 46 | 43 / 46 |
/// | `phrase_split_cases.tsv`（400 个，有上文） | 358 | 386 / 388，改错 4 / 2 | 386 / 388 | 386 / 387 | 386 / 387 |
/// | `phrase_nocontext_cases.tsv`（400 个，无上文） | 397 | 396 / 395，改错 2 / 3 | 396 / 395 | 397 / 396 | 397 / 397，改错 1 |
///
/// 有上文时加阈值没有换来任何好处：对照集上的正确数不随阈值上升（少改错几个的同时也少修正几个），
/// 上下文用例却要丢掉 1~2 个靠微弱优势翻对的用例（「配制药物」只高出 0.14 nat，「权力」高出 0.94 nat），
/// 所以直接取最高分。
/// HTTP 路径的 1.2 nat 阈值是为 Top-256 近似打分的噪声准备的，精确打分不需要。
///
/// 无上文时模型只能判断「哪个词更常见」，直接取最高分是净损失（改错多于修正）；阈值到 2.0 以上才回到不亏，
/// 沿用 HTTP 路径的 2.3 nat。在没有标准答案的 `golden_inputs.tsv` 长输入上抽查（`tools/eval --audit`），
/// 2.3 nat 以上的替换里好坏参半，因此无上文时大模型的收益尚未得到证实，阈值只保证它基本不添乱。
pub fn choose_by_exact_scores(scores: &[f64], has_context: bool) -> usize {
    if scores.is_empty() || scores.iter().any(|s| s.is_nan()) {
        return 0;
    }
    let mut best = 0usize;
    for (i, &s) in scores.iter().enumerate() {
        if s > scores[best] {
            best = i;
        }
    }
    let margin = if has_context {
        EXACT_SCORE_MARGIN_WITH_CONTEXT
    } else {
        EXACT_SCORE_MARGIN_NO_CONTEXT
    };
    if best != 0 && scores[best] - scores[0] >= margin {
        best
    } else {
        0
    }
}

/// 个人记忆强度（`UserDict::user_boost`，每次默认上屏 +2.8、主动改选 +6.5、上限 12，约 7 天半衰期）
/// 折算成对数似然时乘的系数。0.25 意味着空格接受过的词最多相当于 3 nat 的先验。
/// 取值依据见 [`choose_with_user_priors`]。
pub const USER_PRIOR_WEIGHT: f64 = 0.25;

/// 主动改选过（没要首选、点了这个词）的词，个人记忆强度再乘的倍数：最多相当于 9 nat 的先验。
///
/// 空格接受首选只说明用户不反对，主动改选才说明「对我来说模型选错了」。只按记忆强度一视同仁时，
/// 两种情况分不开：系数大了，空格养成的习惯会盖过上文给出的明确判断；系数小了，用户纠正过的词换个上文又被模型换掉。
/// 测量见 [`choose_with_user_priors`] 的表（习惯词、教会的词、反过来三个测试）。由 [`UserMemory::prior`] 乘上它：调用方（`ime-session`）
/// 只需交出记忆强度，并说明这个词有没有主动改选过
pub const EXPLICIT_CHOICE_PRIOR_FACTOR: f32 = 3.0;

/// 调用方交给调度器的某个候选文本的个人记忆（见 [`crate::DualModeAiOrchestrator::rerank_with_user_priors`]）。
///
/// 两处用法取不同的值：
/// - 进程内打分器的先验用 [`Self::prior`]：主动改选过的词乘 [`EXPLICIT_CHOICE_PRIOR_FACTOR`]；
/// - [`crate::ChooserMemoryPolicy::Contested`] / `Taught` 判断「近期有没有用过」时，拿没乘倍数的 [`Self::boost`]
///   与 [`crate::CONTESTED_MEMORY_MIN`] 比较——那个门槛按记忆强度本身定，见它的说明；`Taught` 另看 [`Self::explicit`]。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct UserMemory {
    /// 时间衰减后的记忆强度（`ime-core` 的 `UserDict::user_boost`：空格上屏一次 +2.8、主动改选一次 +6.5、
    /// 上限 12、半衰期约 7.3 天），没有记忆为 0
    pub boost: f32,
    /// 这个词是否被主动改选过（`UserWordRecord::explicit_count > 0`）
    pub explicit: bool,
}

impl UserMemory {
    /// 交给 [`choose_with_user_priors`] 的先验：主动改选过的词乘 [`EXPLICIT_CHOICE_PRIOR_FACTOR`]。
    /// 记忆强度只取有限的正数，负数、NaN、无穷大都按 0 算
    pub fn prior(self) -> f32 {
        if !(self.boost.is_finite() && self.boost > 0.0) {
            return 0.0;
        }
        if self.explicit {
            self.boost * EXPLICIT_CHOICE_PRIOR_FACTOR
        } else {
            self.boost
        }
    }
}

/// 有个人记忆时的裁决：把记忆强度当作先验与模型的对数似然相加，再按 [`choose_by_exact_scores`] 的规则取首选。
/// `priors` 与 `scores` 等长、一一对应；全为 0（没有任何记忆）时与 [`choose_by_exact_scores`] 完全相同。
///
/// # 为什么不让个人记忆直接挡住模型
///
/// 原先的做法（HTTP 路径仍然如此）是：首选一旦带 `[记忆·N次]` 标签就不再请求大模型。问题在于同一串拼音
/// 在不同上文里要选不同的词（「做实验」/「立下誓言」），而个人记忆不分上文——用户上屏过其中一个之后，
/// 这串拼音就再也轮不到模型，剩下的只有「上次用的那个」。用 `crates/ime-ffi/examples/ffi_session`
/// 模拟用户连续输入并上屏（107 个同音异境用例 `data/eval/alternating_cases.tsv`，Qwen2.5-0.5B；
/// 每轮把全部用例换个顺序输入一遍，首选不对就像用户一样改选）：
///
/// | 做法 | 第 1~6 轮首选正确数（共 107） |
/// |---|---|
/// | 没有任何记忆（每个用例换新引擎） | 106 |
/// | 记忆挡住模型（原先的做法） | 46 → 32 → 21 → 10 → 11 → 17 |
/// | 记忆作先验，系数 0.25（另加上文相关的纠正记忆；这一行是加主动改选倍数之前测的） | 103 → 107 → 107 → 107 → 107 → 107 |
/// | 同上，系数 0.5 / 1.0 | 103 → 107 → … / 102 → 106 → 107 → … |
///
/// 换一批上文（`alternating_cases_b.tsv`，拼音与目标相同、上文不同）接着输入：原先的做法 47 → 34 → 19，
/// 记忆作先验 103 → 107 → 107，与没有记忆时模型在这批用例上的成绩（103）相同。加上主动改选的倍数之后的会话结果见下面。
///
/// # 系数怎么定：三个方向不同的测试
///
/// - **习惯词**（`ffi_session --skew 8`）：同一拼音里先把一个词用空格连续上屏 8 遍（记忆强度到上限），
///   再在各自的上文里输入其余的同音词（共 63 个）。上文是对的，记忆不该挡住模型。
/// - **教会的词**（`ffi_session --teach 5`，`data/eval/habit_neutral_cases.tsv`）：44 组拼音，每组在一个中性上文里
///   把模型没选的那个同音词主动改选出来，再看换一个中性上文时它还在不在首位。只算第 1 行确实经过改选的组
///   （Qwen3 34 组、Qwen2.5 37 组；其余的组第一次就已经是首选）。上文给不出判断，该听用户的。
/// - **反过来**（`ffi_session --teach 3`，`data/eval/habit_reversal_cases.tsv`）：同样教会一个词之后，
///   同音的另一个词出现在它自己明确的上文里，看模型还能不能选对。上文是对的，记忆不该挡住模型。
///
/// 结果（Qwen3-0.6B / Qwen2.5-0.5B；习惯词一列是 2026-11-11 测的，后两列 2026-11-12；「教会的词」「反过来」的分母都是 34 / 37）：
///
/// | 做法 | 习惯词（共 63） | 教会的词：换个上文仍在首位 | 反过来：另一个词仍能选对 |
/// |---|---|---|---|
/// | 记忆不影响模型 | 62 / 62 | 1 / 10 | 34 / 37 |
/// | 原先的做法（记忆挡住模型） | 0 / 0 | 34 / 37 | 0 / 0 |
/// | 一律按 0.1 折算 | 58 / 59 | 1 / 11 | 34 / 37 |
/// | 一律按 0.25 折算 | 54 / 58 | 9 / 13 | 34 / 37 |
/// | 一律按 0.5 折算 | — / 48 | 19 / 22 | 33 / 36 |
/// | 空格 0.25，主动改选再乘 2 | 54 / 58 | 19 / 22 | 33 / 36 |
/// | **空格 0.25，主动改选再乘 3（现在的做法）** | **54 / 58** | **25 / 29** | **30 / 33** |
/// | 空格 0.25，主动改选再乘 4 | 54 / 58 | 32 / 34 | 27 / 28 |
/// | 空格 0.25，主动改选再乘 6（这一行另加了「有争议时不放大」，对后两项没有影响） | 54 / 58 | 34 / 36 | 7 / 9 |
///
/// - 只用一个系数顾不全：主动改选过的词在「一律按 0.5」和「改选乘 2」下的先验本来就一样（0.5 × 6.5 = 0.25 × 2 × 6.5），
///   后两项也就一样；但前者把空格接受的习惯也加倍，Qwen2.5 的习惯词少对 10 个（Qwen3 没测）。
///   区别在用户做了什么：空格接受只说明用户不反对，主动改选说明模型对这个用户选错了，所以两种记忆分开折算。
/// - 改选的倍数在「教会的词」和「反过来」之间此消彼长：两项加起来，倍数 3 时 Qwen3 55、Qwen2.5 62，
///   倍数 4 时 59、62，最多；倍数 6 时「反过来」只剩 7 / 9，倍数 10 时全错。
/// - 取 3 不取 4，看同音异境的会话模拟（同一批上文 3 轮 / 换一批上文 3 轮，共 107）：
///   倍数 1 时两个模型都是 103→107→107 / 103→107→107；倍数 3 时 Qwen3 103→107→107 / 105→107→107，
///   Qwen2.5 103→106→107 / 104→107→107；倍数 4 时 Qwen3 103→106→106 / 103→107→107，
///   Qwen2.5 103→106→107 / 104→105→107，又多错几条。差得不多，但两项加起来 4 也没有明显好处（Qwen3 多 4、Qwen2.5 持平），
///   取不那么激进的 3。
/// - 也试过「这串拼音已有两个以上带记忆的词时不放大」：倍数 3 时会话后几轮全对，但换一批上文的第一轮
///   少对 1–2 条（主动改选的记忆本来在帮忙），另外三项不变，所以没有采用。
/// - 「教会的词」原先按每组后 4 行合计（共 176）统计，把第一次就是首选、没经过改选的组也算了进去
///   （倍数 3 时 160 / 161，倍数 4 时 169 / 168），现在只算教过的组。
///
/// 模型判断错误、需要用户纠正、而且上文末尾完全相同的地方，另由上文相关的纠正记忆（`UserDict::record_context_choice`）记住。
pub fn choose_with_user_priors(scores: &[f64], priors: &[f32], has_context: bool) -> usize {
    // 记忆强度只取有限的正数；负数、NaN、无穷大都当作没有（`-inf` 的分数加上 `+inf` 的先验会变成 NaN）
    let usable_prior = |p: f32| if p.is_finite() && p > 0.0 { f64::from(p) } else { 0.0 };
    if priors.len() != scores.len() || priors.iter().all(|p| usable_prior(*p) == 0.0) {
        return choose_by_exact_scores(scores, has_context);
    }
    // `-inf`（不能选的候选）加上有限的先验仍是 `-inf`：记忆再强也救不回一个打不出分的候选
    let adjusted: Vec<f64> = scores
        .iter()
        .zip(priors)
        .map(|(s, p)| s + USER_PRIOR_WEIGHT * usable_prior(*p))
        .collect();
    choose_by_exact_scores(&adjusted, has_context)
}

/// 去掉全部空白字符（比较候选文本时忽略中英混排的显示用空格）
pub(crate) fn strip_whitespace(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 把宿主答复的文本对齐到候选池：先精确匹配，再忽略空白匹配；匹配不到返回 `None`
pub(crate) fn match_choice_in_pool<'a>(choice: &str, pool: &'a [String]) -> Option<&'a str> {
    let trimmed = choice.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(hit) = pool.iter().find(|c| c.as_str() == trimmed) {
        return Some(hit.as_str());
    }
    let bare = strip_whitespace(trimmed);
    pool.iter()
        .find(|c| strip_whitespace(c) == bare)
        .map(|c| c.as_str())
}

/// 交给后台线程的一次进程内请求（打分器与宿主选词器共用）
pub(crate) struct InProcessJob {
    /// 任务编号（宿主选词器用它作为请求编号）
    pub(crate) job_id: u64,
    pub(crate) cache_key: String,
    pub(crate) pinyin_input: String,
    pub(crate) surrounding_context: String,
    /// 候选池文本，第 0 项为当前首选
    pub(crate) pool: Vec<String>,
    /// 与 `pool` 等长：各候选的先验（[`UserMemory::prior`]，即个人记忆强度，主动改选过的词乘
    /// [`EXPLICIT_CHOICE_PRIOR_FACTOR`]），没有记忆为 0
    pub(crate) priors: Vec<f32>,
    /// 投递时的缓存清理代次（`InProcessState::invalidation_epoch`）。打分结束时代次变了，
    /// 说明这期间用户上屏清过缓存，结果作废
    pub(crate) invalidation_epoch: u64,
}

/// 击键线程对后台线程的指示
pub(crate) enum InProcessDispatch {
    /// 交给打分器
    Score(InProcessJob),
    /// 交给宿主选词器；`None` 表示只需清理已作废的等待项
    Choose(Option<InProcessJob>),
}

/// 一次击键想要的大模型仲裁（已通过各项前置条件、候选池不少于 2 个）
pub(crate) struct InProcessRequest {
    pub(crate) cache_key: String,
    pub(crate) pinyin_input: String,
    pub(crate) surrounding_context: String,
    pub(crate) pool: Vec<String>,
    pub(crate) priors: Vec<f32>,
}

/// 正在等待宿主答复的请求
struct PendingChoice {
    id: u64,
    cache_key: String,
    pool: Vec<String>,
    short_tag: String,
    deadline: Instant,
}

#[derive(Default)]
struct InProcessState {
    scorer: Option<Arc<dyn CandidateScorer>>,
    chooser: Option<HostChooser>,
    /// 最新一次击键正在等待的缓存键（已命中缓存或不需要大模型时为 `None`）。结果入缓存时据此决定是否触发刷新
    wanted_key: Option<String>,
    /// 已投递给后台线程、尚未结束的打分任务：(任务编号, 缓存键, 投递时的缓存清理代次)。同一时刻至多一个
    scoring: Option<(u64, String, u64)>,
    /// 已发给宿主、尚未答复的请求。同一时刻至多一个
    pending_choice: Option<PendingChoice>,
    /// 最近一次投递给后台线程、后台线程还没开始处理的宿主选词任务带的请求：(任务编号, 缓存键)。
    /// 后台线程开始处理这个任务时清除；那次投递只是作废等待中的请求（`Choose(None)`）时为 `None`。
    /// `result_pending` 据此判断最新一次击键等的键有没有请求排着队、等后台线程发出
    queued_choice: Option<(u64, String)>,
    next_id: u64,
    /// 缓存清理代次：每次 `invalidate_cache_for_pinyin`（用户选词 / 上屏）加一。
    /// 在此之前投递的打分任务不得再写缓存
    invalidation_epoch: u64,
}

/// 结果缓存：缓存键 → (被选中的候选文本, 来源简称)
pub(crate) type LlmCache = Arc<Mutex<HashMap<String, (String, String)>>>;
pub(crate) type NotifySlot = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

/// 结果缓存的容量上限：超过后整体清空（与 HTTP 路径相同）
pub(crate) const LLM_CACHE_MAX_ENTRIES: usize = 512;

/// 打分器 / 宿主选词器的共享状态。击键线程、后台线程与宿主的答复线程都通过它协作。
///
/// 加锁顺序固定为 `state` → `llm_cache`；回调（刷新通知、宿主选词器回调）一律在释放 `state` 之后调用。
pub(crate) struct InProcessShared {
    llm_cache: LlmCache,
    notify_cb: NotifySlot,
    state: Mutex<InProcessState>,
}

impl InProcessShared {
    pub(crate) fn new(llm_cache: LlmCache, notify_cb: NotifySlot) -> Self {
        Self { llm_cache, notify_cb, state: Mutex::new(InProcessState::default()) }
    }

    fn lock(&self) -> MutexGuard<'_, InProcessState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn cache_get(&self, key: &str) -> Option<(String, String)> {
        self.llm_cache.lock().ok().and_then(|g| g.get(key).cloned())
    }

    fn notify(&self) {
        let cb = self.notify_cb.lock().ok().and_then(|g| g.clone());
        if let Some(cb) = cb {
            cb();
        }
    }

    /// 结果写入缓存。返回最新一次击键是否正在等这个结果（是则由调用方在释放锁之后触发刷新回调）。
    fn store(&self, state: &mut InProcessState, cache_key: &str, text: String, short_tag: String) -> bool {
        if let Ok(mut guard) = self.llm_cache.lock() {
            if guard.len() > LLM_CACHE_MAX_ENTRIES {
                guard.clear();
            }
            guard.insert(cache_key.to_string(), (text, short_tag));
        }
        Self::take_wanted(state, cache_key)
    }

    /// 若最新一次击键等的正是 `cache_key`，清除等待标记并返回 `true`（每个结果至多触发一次刷新）
    fn take_wanted(state: &mut InProcessState, cache_key: &str) -> bool {
        if state.wanted_key.as_deref() == Some(cache_key) {
            state.wanted_key = None;
            true
        } else {
            false
        }
    }

    fn next_id(state: &mut InProcessState) -> u64 {
        state.next_id += 1;
        state.next_id
    }

    /// 当前生效的后端的缓存键前缀；两者都未安装时返回 `None`（走 HTTP 路径）
    pub(crate) fn active_cache_prefix(&self) -> Option<String> {
        let state = self.lock();
        if let Some(scorer) = &state.scorer {
            Some(format!("scorer:{}", scorer.model_id()))
        } else {
            state.chooser.as_ref().map(|c| format!("chooser:{}", c.backend_id))
        }
    }

    /// 是否安装了进程内打分器（它给出精确分数，可以和个人记忆一起裁决）
    pub(crate) fn has_scorer(&self) -> bool {
        self.lock().scorer.is_some()
    }

    /// 当前生效的是不是宿主选词器（没有装打分器、装了选词器）
    pub(crate) fn chooser_is_active(&self) -> bool {
        let state = self.lock();
        state.scorer.is_none() && state.chooser.is_some()
    }

    /// 最新一次击键是否还在等打分器 / 宿主选词器的结果。
    ///
    /// 等的是宿主选词器时，只有两种情况还算在等：为这个键发出的请求还在 [`HostChooser::timeout`] 之内，
    /// 或者这个键有新请求排着队、等后台线程发出。其余情况再等也等不到结果，返回 `false`，例如：
    /// - 请求超时还没有答复，同一输入也没有重新投递请求：迟到的答复只会被丢弃（[`ChooserDelivery::TimedOut`]）；
    /// - 请求已经超时，排着队的只是别的键的任务（用户敲了一个字母又删掉、后台线程还没来得及处理，
    ///   或者中间那次击键不需要大模型），或者那个任务已经处理过：它不会替这个键发出请求。在超时之前处理的什么也不做，
    ///   迟到的答复仍是 [`ChooserDelivery::TimedOut`]；超时之后才处理的会顺手把超时的请求作废
    ///   （宿主收到 [`ChooserEvent::Cancel`]，之后的答复是 [`ChooserDelivery::Stale`]）。
    ///
    /// 查询本身不改变状态：这些情况下等待标记（`wanted_key`）还留着，到下一次重排（同一输入会重新发出请求，
    /// 又变回 `true`），或者请求还登记着时那次迟到的答复，才清除
    pub(crate) fn result_pending(&self) -> bool {
        let state = self.lock();
        let Some(wanted) = state.wanted_key.as_deref() else {
            return false;
        };
        if state.scorer.is_some() {
            return true;
        }
        let asked = state
            .pending_choice
            .as_ref()
            .is_some_and(|p| p.cache_key == wanted && Instant::now() < p.deadline);
        let queued = state.queued_choice.as_ref().is_some_and(|(_, key)| key == wanted);
        asked || queued
    }

    /// 用户选词 / 上屏时清除某串拼音的全部缓存结果，并让此前投递、尚未结束的打分任务作废
    /// （它们算完也不写缓存，否则刚清掉的结果会在清理之后被写回去）。
    pub(crate) fn invalidate_for_pinyin(&self, pinyin_input: &str) {
        let suffix = format!("\t{pinyin_input}");
        // 先加代次、再清缓存，两步都在 `state` 锁内：打分任务写缓存前也要拿这把锁，
        // 所以它要么在清理之前写入（随即被清掉），要么在之后看到代次已变而放弃
        let mut state = self.lock();
        state.invalidation_epoch += 1;
        if let Ok(mut guard) = self.llm_cache.lock() {
            guard.retain(|k, _| !k.ends_with(&suffix));
        }
    }

    /// 击键线程：查缓存；未命中时登记「最新击键在等这个键」并按需投递后台请求。
    ///
    /// 每次击键恰好调用一次。`request` 为 `None` 表示本次击键不需要大模型结果——此时同样要调用，
    /// 这样上一次击键留下的在途工作才会被取消。返回缓存命中的 (候选文本, 来源简称)。
    pub(crate) fn lookup_or_request(
        &self,
        request: Option<InProcessRequest>,
        dispatch: &mut dyn FnMut(InProcessDispatch),
    ) -> Option<(String, String)> {
        let mut state = self.lock();
        let cached = request.as_ref().and_then(|r| self.cache_get(&r.cache_key));
        let request = if cached.is_some() { None } else { request };
        state.wanted_key = request.as_ref().map(|r| r.cache_key.clone());

        if let Some(scorer) = state.scorer.clone() {
            let epoch = state.invalidation_epoch;
            let unchanged = match (&state.scoring, &state.wanted_key) {
                // 在途任务若是在最近一次缓存清理之前投递的，它的结果已经作废：按「键变了」处理，取消后重新投递
                (Some((_, running, job_epoch)), Some(wanted)) => running == wanted && *job_epoch == epoch,
                (None, None) => true,
                _ => false,
            };
            if !unchanged {
                if state.scoring.take().is_some() {
                    // 更新的击键让在途的打分失去意义：让它在推理中途退出，后台线程才能尽快处理最新请求
                    scorer.cancel();
                }
                if let Some(r) = request {
                    let job_id = Self::next_id(&mut state);
                    state.scoring = Some((job_id, r.cache_key.clone(), epoch));
                    dispatch(InProcessDispatch::Score(InProcessJob {
                        job_id,
                        cache_key: r.cache_key,
                        pinyin_input: r.pinyin_input,
                        surrounding_context: r.surrounding_context,
                        pool: r.pool,
                        priors: r.priors,
                        invalidation_epoch: epoch,
                    }));
                }
            }
        } else if state.chooser.is_some() {
            let still_waiting = match (&state.pending_choice, &state.wanted_key) {
                (Some(p), Some(wanted)) => &p.cache_key == wanted && Instant::now() < p.deadline,
                (None, None) => true,
                _ => false,
            };
            if !still_waiting {
                // 发新请求、作废旧请求都要调用宿主回调，放到后台线程去做，击键线程不等宿主
                let job = request.map(|r| {
                    let job_id = Self::next_id(&mut state);
                    InProcessJob {
                        job_id,
                        cache_key: r.cache_key,
                        pinyin_input: r.pinyin_input,
                        surrounding_context: r.surrounding_context,
                        pool: r.pool,
                        priors: r.priors,
                        invalidation_epoch: state.invalidation_epoch,
                    }
                });
                state.queued_choice = job.as_ref().map(|j| (j.job_id, j.cache_key.clone()));
                dispatch(InProcessDispatch::Choose(job));
            }
        }
        cached
    }

    /// 后台线程：执行一次打分任务。打分器 panic 时只清理这一个任务，后台线程继续工作
    pub(crate) fn run_score_job(&self, job: InProcessJob) {
        let (job_id, cache_key) = (job.job_id, job.cache_key.clone());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.score_job(job)));
        if outcome.is_err() {
            // 打分器（或刷新回调）panic：这个任务不会再有结果。只动它自己的登记项——
            // 更新的击键登记的任务与等待标记不受影响，同一输入下次重排时会重新投递
            let mut state = self.lock();
            if state.scoring.as_ref().map(|(id, _, _)| *id) == Some(job_id) {
                state.scoring = None;
                Self::take_wanted(&mut state, &cache_key);
            }
        }
    }

    fn score_job(&self, job: InProcessJob) {
        let scorer = {
            let mut state = self.lock();
            if state.scoring.as_ref().map(|(id, _, _)| *id) != Some(job.job_id) {
                return; // 已被更新的击键取代
            }
            if job.invalidation_epoch != state.invalidation_epoch {
                // 排队期间用户上屏清过缓存，之后也没有新的击键再要这个结果：不必算了
                state.scoring = None;
                Self::take_wanted(&mut state, &job.cache_key);
                return;
            }
            if self.cache_get(&job.cache_key).is_some() {
                // 排队期间已经有了结果
                state.scoring = None;
                let notify = Self::take_wanted(&mut state, &job.cache_key);
                drop(state);
                if notify {
                    self.notify();
                }
                return;
            }
            match state.scorer.clone() {
                Some(scorer) => scorer,
                None => {
                    state.scoring = None;
                    return;
                }
            }
        };

        // 不持锁推理：这期间击键线程可以随时调用 scorer.cancel()
        let result = scorer.score(&job.surrounding_context, &job.pool);

        let mut state = self.lock();
        let still_current = state.scoring.as_ref().map(|(id, _, _)| *id) == Some(job.job_id);
        // 打分期间用户上屏清过缓存：这个结果正是那次清理要去掉的，不能再写回去
        let voided = job.invalidation_epoch != state.invalidation_epoch;
        let mut notify = false;
        match result {
            Ok(scores) if !voided && scores.len() == job.pool.len() && scores_are_usable(&scores) => {
                let has_context = !job.surrounding_context.trim().is_empty();
                let pick = choose_with_user_priors(&scores, &job.priors, has_context);
                // 即使这次任务已经被更新的击键取代（取消来得太晚），结果对它自己的缓存键仍然正确：
                // 照常入缓存，只是不触发刷新
                notify = self.store(&mut state, &job.cache_key, job.pool[pick].clone(), scorer.short_tag());
            }
            _ => {
                // 被取消、打分失败、分数不可用（含 NaN 或全是 -inf），或结果已被上屏作废：
                // 不写缓存，候选保持规则重排的结果。若它仍是最新的任务，说明不会再有结果到来，清除等待标记
                if still_current {
                    Self::take_wanted(&mut state, &job.cache_key);
                }
            }
        }
        if still_current {
            state.scoring = None;
        }
        drop(state);
        if notify {
            self.notify();
        }
    }

    /// 后台线程：作废不再需要的宿主请求，并在需要时发出新的请求。
    /// 宿主回调 panic 时只作废刚登记的这一个请求，后台线程继续工作
    pub(crate) fn run_choose_job(&self, job: Option<InProcessJob>) {
        let registered = job.as_ref().map(|j| (j.job_id, j.cache_key.clone()));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.choose_job(job)));
        if outcome.is_err() {
            // 回调（或刷新回调）panic：宿主多半没有收到这次请求，不能让它一直占着等待位直到超时。
            // 只清理本任务登记的那一项；同一输入下次重排时重新发出
            if let Some((job_id, cache_key)) = registered {
                let mut state = self.lock();
                if state.pending_choice.as_ref().map(|p| p.id) == Some(job_id) {
                    state.pending_choice = None;
                    Self::take_wanted(&mut state, &cache_key);
                }
            }
        }
    }

    fn choose_job(&self, job: Option<InProcessJob>) {
        let mut events: Vec<ChooserEvent> = Vec::new();
        let mut notify = false;
        let callback = {
            let mut state = self.lock();
            // 这个任务不再排着队：下面要么发出它，要么丢弃它
            if let Some(job) = &job {
                if state.queued_choice.as_ref().is_some_and(|(id, _)| *id == job.job_id) {
                    state.queued_choice = None;
                }
            }
            let Some(chooser) = state.chooser.clone() else {
                return;
            };
            let now = Instant::now();
            if let Some(p) = &state.pending_choice {
                if state.wanted_key.as_deref() == Some(p.cache_key.as_str()) && now < p.deadline {
                    return; // 同一输入仍在等宿主答复：不重复发出
                }
                events.push(ChooserEvent::Cancel { id: p.id });
                state.pending_choice = None;
            }
            // 打分器优先：请求排队期间若安装了打分器，就不再发给宿主
            let job = if state.scorer.is_some() { None } else { job };
            if let Some(job) = job {
                if state.wanted_key.as_deref() == Some(job.cache_key.as_str()) {
                    if self.cache_get(&job.cache_key).is_some() {
                        notify = Self::take_wanted(&mut state, &job.cache_key);
                    } else {
                        state.pending_choice = Some(PendingChoice {
                            id: job.job_id,
                            cache_key: job.cache_key.clone(),
                            pool: job.pool.clone(),
                            short_tag: chooser.short_tag.clone(),
                            deadline: now + chooser.timeout,
                        });
                        events.push(ChooserEvent::Request(ChooserRequest {
                            id: job.job_id,
                            cache_key: job.cache_key,
                            context: job.surrounding_context,
                            pinyin: job.pinyin_input,
                            candidates: job.pool,
                        }));
                    }
                }
            }
            chooser.callback
        };
        for event in events {
            callback(event);
        }
        if notify {
            self.notify();
        }
    }

    /// 宿主送回编号为 `id` 的请求的答复（任意线程）
    pub(crate) fn deliver(&self, id: u64, choice: Option<&str>) -> ChooserDelivery {
        let mut state = self.lock();
        if state.pending_choice.as_ref().map(|p| p.id) != Some(id) {
            return ChooserDelivery::Stale;
        }
        let Some(pending) = state.pending_choice.take() else {
            return ChooserDelivery::Stale;
        };
        let rejected = if Instant::now() >= pending.deadline {
            Some(ChooserDelivery::TimedOut)
        } else {
            match choice.map(str::trim).filter(|c| !c.is_empty()) {
                None => Some(ChooserDelivery::NoAnswer),
                Some(choice) if match_choice_in_pool(choice, &pending.pool).is_none() => {
                    Some(ChooserDelivery::Implausible)
                }
                Some(_) => None,
            }
        };
        if let Some(outcome) = rejected {
            // 请求到此结束且没有结果：不写缓存，清除等待标记
            Self::take_wanted(&mut state, &pending.cache_key);
            return outcome;
        }
        let text = choice
            .and_then(|c| match_choice_in_pool(c, &pending.pool))
            .unwrap_or_default()
            .to_string();
        let notify = self.store(&mut state, &pending.cache_key, text, pending.short_tag);
        drop(state);
        if notify {
            self.notify();
        }
        ChooserDelivery::Accepted
    }

    /// 安装或卸载打分器。在途的打分被取消；安装打分器时，等待中的宿主请求同时作废。
    pub(crate) fn set_scorer(&self, scorer: Option<Arc<dyn CandidateScorer>>) {
        let mut state = self.lock();
        if let Some(old) = state.scorer.take() {
            if state.scoring.is_some() {
                old.cancel();
            }
        }
        state.scoring = None;
        state.wanted_key = None;
        let cancelled = if scorer.is_some() { Self::void_pending_choice(&mut state) } else { None };
        state.scorer = scorer;
        drop(state);
        if let Some((callback, id)) = cancelled {
            callback(ChooserEvent::Cancel { id });
        }
    }

    /// 安装或卸载宿主选词器。等待中的请求作废（通知旧的选词器）。
    pub(crate) fn set_chooser(&self, chooser: Option<HostChooser>) {
        let mut state = self.lock();
        let cancelled = Self::void_pending_choice(&mut state);
        if state.scorer.is_none() {
            state.wanted_key = None;
        }
        state.chooser = chooser;
        drop(state);
        if let Some((callback, id)) = cancelled {
            callback(ChooserEvent::Cancel { id });
        }
    }

    fn void_pending_choice(state: &mut InProcessState) -> Option<(ChooserCallback, u64)> {
        let pending = state.pending_choice.take()?;
        state.chooser.as_ref().map(|c| (Arc::clone(&c.callback), pending.id))
    }
}

/// 宿主送回选词结果用的句柄。可克隆、可跨线程，不需要持有调度器本身（宿主的异步任务里直接使用）。
#[derive(Clone)]
pub struct ChooserHandle {
    pub(crate) shared: Arc<InProcessShared>,
}

impl ChooserHandle {
    /// 送回编号为 `id` 的请求的答复：`choice` 为选中的候选文本，`None`（或空串）表示宿主没有结果。
    ///
    /// 只有「当前等待中的请求、未超时、文本属于该请求的候选池」的答复才会被采纳（写入结果缓存，
    /// 并在最新一次击键仍在等它时触发刷新回调——回调在调用本方法的线程上执行）。其余情况见 [`ChooserDelivery`]。
    pub fn deliver(&self, id: u64, choice: Option<&str>) -> ChooserDelivery {
        self.shared.deliver(id, choice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_priors_are_added_to_the_scores_before_choosing() {
        // 没有任何记忆：与 choose_by_exact_scores 完全相同
        let scores = [-5.0, -4.0, -9.0];
        assert_eq!(choose_with_user_priors(&scores, &[0.0, 0.0, 0.0], true), 1);
        assert_eq!(choose_with_user_priors(&scores, &[], true), choose_by_exact_scores(&scores, true));
        // 模型只领先 1 nat：首选有很强的个人记忆（8 × 0.25 = 2 nat）时保持首选
        assert_eq!(choose_with_user_priors(&scores, &[8.0, 0.0, 0.0], true), 0);
        // 记忆较弱（2 × 0.25 = 0.5 nat）时模型说了算
        assert_eq!(choose_with_user_priors(&scores, &[2.0, 0.0, 0.0], true), 1);
        // 模型领先很多：再强的记忆（上限 12 → 3 nat）也挡不住
        assert_eq!(choose_with_user_priors(&[-12.0, -4.0], &[12.0, 0.0], true), 1);
        // 记忆也可以帮非首选的候选
        assert_eq!(choose_with_user_priors(&[-4.0, -5.0], &[0.0, 8.0], true), 1);
        // 两边记忆一样强：相互抵消
        assert_eq!(choose_with_user_priors(&scores, &[6.0, 6.0, 6.0], true), 1);
        // 负数或 NaN 的记忆强度当作没有
        assert_eq!(choose_with_user_priors(&scores, &[-3.0, f32::NAN, 0.0], true), 1);
        // 长度对不上：忽略记忆
        assert_eq!(choose_with_user_priors(&scores, &[12.0], true), 1);
        // 无上文时仍然要过 2.3 nat 的门槛
        assert_eq!(choose_with_user_priors(&[-5.0, -4.0], &[0.0, 1.0], false), 0);
        assert_eq!(choose_with_user_priors(&[-8.0, -4.0], &[0.0, 1.0], false), 1);
        // 分数里有 NaN：保持首选
        assert_eq!(choose_with_user_priors(&[f64::NAN, -4.0], &[0.0, 5.0], true), 0);
    }

    fn cand(text: &str, consumed_len: usize, l1_cost: f32) -> Candidate {
        Candidate {
            text: text.to_string(),
            preedit: String::new(),
            consumed_len,
            l1_cost,
            final_score: 100.0 - l1_cost,
            ai_boosted: false,
            tag: String::new(),
            segments: vec![text.to_string()],
            segment_spans: Vec::new(),
            syllable_spans: Vec::new(),
        }
    }

    #[test]
    fn pool_keeps_full_input_candidates_in_order_up_to_eight() {
        let mut list = vec![cand("实验", 6, 1.0), cand("是", 3, 0.5), cand("试验", 6, 2.0)];
        for i in 0..10 {
            list.push(cand(&format!("同音{i}"), 6, 30.0 + i as f32));
        }
        let pool = select_scoring_pool(&list);
        assert_eq!(pool.len(), SCORING_POOL_MAX);
        assert_eq!(pool[0].text, "实验");
        assert_eq!(pool[1].text, "试验");
        // 代价差距再大也保留（由精确似然来判断），但只覆盖部分输入的前缀候选不进池
        assert_eq!(pool[2].text, "同音0");
        assert!(pool.iter().all(|c| c.consumed_len == 6));
    }

    #[test]
    fn pool_deduplicates_ignoring_display_spaces() {
        let list = vec![cand("AI 选词", 8, 1.0), cand("AI选词", 8, 1.5), cand("爱选词", 8, 2.0)];
        let texts: Vec<String> = select_scoring_pool(&list).into_iter().map(|c| c.text).collect();
        assert_eq!(texts, ["AI 选词", "爱选词"]);
        assert!(select_scoring_pool(&[]).is_empty());
    }

    /// 带编码的候选：`preedit` 是解码器给出的编码展示串（音节之间 `'`，词之间空格）
    fn coded(text: &str, preedit: &str, consumed_len: usize) -> Candidate {
        let mut c = cand(text, consumed_len, 1.0);
        c.preedit = preedit.to_string();
        c
    }

    fn pool_texts(list: &[Candidate]) -> Vec<String> {
        select_scoring_pool(list).into_iter().map(|c| c.text).collect()
    }

    #[test]
    fn pool_drops_expansions_and_completions_when_the_top_is_typed_english() {
        // `cpu`：首选 CPU 的编码就是敲的三个字母，简拼展开不进池（池里只剩一个，调度器不会请求大模型）
        let cpu = [coded("CPU", "cpu", 3), coded("菜谱", "cai'pu", 3), coded("纯朴", "chun'pu", 3)];
        assert_eq!(pool_texts(&cpu), ["CPU"]);
        let sql = [coded("SQL", "sql", 3), coded("失去了", "shi'qu'le", 3), coded("说起来", "shuo'qi'lai", 3)];
        assert_eq!(pool_texts(&sql), ["SQL"]);
        // `intel`：英文补全不进池
        let intel = [coded("Intel", "intel", 5), coded("intellectual", "intellectual", 5), coded("intelligent", "intelligent", 5)];
        assert_eq!(pool_texts(&intel), ["Intel"]);
        // 英文首选之后，编码与输入等长的中文候选仍然保留（真正的歧义），更长的剔除
        let api = [coded("API", "api", 3), coded("阿批", "a'pi", 3), coded("挨批评", "ai'pi'ping", 3)];
        assert_eq!(pool_texts(&api), ["API", "阿批"]);
        let bug = [coded("bug", "bug", 3), coded("不过", "bu'guo", 3), coded("不够", "bu'gou", 3)];
        assert_eq!(pool_texts(&bug), ["bug"]);
    }

    #[test]
    fn pool_keeps_candidates_whose_code_is_as_long_as_the_input() {
        // 全拼同音词：编码都与输入等长，候选池与原来完全相同
        let shiyan = [coded("实验", "shi'yan", 6), coded("誓言", "shi'yan", 6), coded("食盐", "shi'yan", 6)];
        assert_eq!(pool_texts(&shiyan), ["实验", "誓言", "食盐"]);
        // `ai` 既是英文也是完整的拼音：真正的歧义，交给模型
        let ai = [coded("AI", "ai", 2), coded("爱", "ai", 2), coded("艾", "ai", 2)];
        assert_eq!(pool_texts(&ai), ["AI", "爱", "艾"]);
        // 中英混输整句：编码里的空格与 `'` 都不算字母
        let sentence = [coded("用 Rust 写", "yong rust xie", 11), coded("用 Rust 些", "yong rust xie", 11)];
        assert_eq!(pool_texts(&sentence), ["用 Rust 写", "用 Rust 些"]);
    }

    #[test]
    fn pool_is_not_narrowed_when_the_top_itself_is_a_guess() {
        // `mac`：首选「马车」本身就是补全（ma + c…），池里都是同一类猜测，敲全了的 Mac 也照常参与
        let mac = [coded("马车", "ma'che", 3), coded("Mac", "mac", 3), coded("马刺", "ma'ci", 3)];
        assert_eq!(pool_texts(&mac), ["马车", "Mac", "马刺"]);
        // `pc`：全是简拼展开
        let pc = [coded("赔偿", "pei'chang", 2), coded("排除", "pai'chu", 2), coded("平常", "ping'chang", 2)];
        assert_eq!(pool_texts(&pc), ["赔偿", "排除", "平常"]);
        // `java`：首选 JavaScript 是补全，敲全了的 java 留在池里
        let java = [coded("JavaScript", "javascript", 4), coded("java", "java", 4)];
        assert_eq!(pool_texts(&java), ["JavaScript", "java"]);
        // 中文首选：即使编码与输入等长，也不剔除更长的候选（解码器对「还没敲完」的预测、英文补全）
        let qigu = [coded("其故", "qi'gu", 4), coded("齐国", "qi'guo", 4), coded("旗鼓", "qi'gu", 4)];
        assert_eq!(pool_texts(&qigu), ["其故", "齐国", "旗鼓"]);
        let leba = [coded("了吧", "le'ba", 4), coded("乐吧", "le'ba", 4), coded("Lebanon", "lebanon", 4)];
        assert_eq!(pool_texts(&leba), ["了吧", "乐吧", "Lebanon"]);
        // 中文整句末尾的简拼只显示敲出来的字母，字母数与输入相等，但它和展开后的候选是同一类
        let sentence = [coded("卫星城镇有", "wei'xing'cheng'zhen y", 17), coded("卫星城真有", "wei'xing'cheng zhen'you", 17)];
        assert_eq!(pool_texts(&sentence), ["卫星城镇有", "卫星城真有"]);
        // 没有编码信息的英文首选：不知道它是不是敲全了，不剔除
        let unknown = [cand("CPU", 3, 1.0), coded("菜谱", "cai'pu", 3)];
        assert_eq!(pool_texts(&unknown), ["CPU", "菜谱"]);
        // 输入里有用户自己敲的分隔符（consumed_len 比字母数多）：不剔除
        let apostrophe = [coded("it's", "its", 4), coded("items", "items", 4)];
        assert_eq!(pool_texts(&apostrophe), ["it's", "items"]);
    }

    #[test]
    fn pool_deduplicates_ascii_candidates_ignoring_case() {
        // 同一个英文词的两种大小写只留排在前面的
        let docker = [coded("Docker", "docker", 6), coded("docker", "docker", 6)];
        assert_eq!(pool_texts(&docker), ["Docker"]);
        let app = [coded("app", "app", 3), coded("App", "app", 3), coded("APP", "app", 3)];
        assert_eq!(pool_texts(&app), ["app"]);
        // 不是同一个词的不受影响；含汉字的候选仍按原文（忽略空格）去重
        let other = [coded("AI", "ai", 2), coded("ai", "ai", 2), coded("爱", "ai", 2), coded("哎", "ai", 2)];
        assert_eq!(pool_texts(&other), ["AI", "爱", "哎"]);
        let cjk = [coded("AI 选词", "ai xuan'ci", 8), coded("ai 选词", "ai xuan'ci", 8), coded("AI选词", "ai xuan'ci", 8)];
        assert_eq!(pool_texts(&cjk), ["AI 选词", "ai 选词"]);
    }

    #[test]
    fn pool_fingerprint_distinguishes_pools() {
        let pool = |texts: &[&str]| texts.iter().map(|t| t.to_string()).collect::<Vec<_>>();
        let a = pool_fingerprint(&pool(&["怎么回事", "怎么会是"]));
        assert_eq!(a, pool_fingerprint(&pool(&["怎么回事", "怎么会是"])), "同一个池的指纹不变");
        assert_ne!(a, pool_fingerprint(&pool(&["怎么会是", "怎么会时", "怎么会事"])));
        assert_ne!(a, pool_fingerprint(&pool(&["怎么会是", "怎么回事"])), "顺序不同（首选不同）也算不同的池");
        assert_ne!(a, pool_fingerprint(&pool(&["怎么回事怎么会是"])), "不能靠拼接撞在一起");
    }

    #[test]
    fn negative_infinity_marks_a_candidate_that_can_never_win() {
        let ninf = f64::NEG_INFINITY;
        for has_context in [true, false] {
            // 非首选是 -inf：永远不选它
            assert_eq!(choose_by_exact_scores(&[-3.0, ninf], has_context), 0);
            assert_eq!(choose_by_exact_scores(&[-30.0, ninf, ninf], has_context), 0);
            // 当前首选是 -inf、别的候选有分数：换成其中最高的（无上文时差值是 +inf，同样超过 2.3 nat 的门槛）
            assert_eq!(choose_by_exact_scores(&[ninf, -3.0], has_context), 1);
            assert_eq!(choose_by_exact_scores(&[ninf, -5.0, -3.0, ninf], has_context), 2);
            // 全部是 -inf：保持首选
            assert_eq!(choose_by_exact_scores(&[ninf, ninf, ninf], has_context), 0);
            // 记忆再强也救不回 -inf 的候选，也不会让别人输给它
            assert_eq!(choose_with_user_priors(&[ninf, -3.0], &[12.0, 0.0], has_context), 1);
            assert_eq!(choose_with_user_priors(&[-3.0, ninf], &[0.0, 12.0], has_context), 0);
            assert_eq!(choose_with_user_priors(&[ninf, ninf], &[0.0, 12.0], has_context), 0);
            // 无穷大的记忆强度当作没有（否则 -inf + inf = NaN）
            assert_eq!(choose_with_user_priors(&[ninf, -3.0], &[f32::INFINITY, 0.0], has_context), 1);
            assert_eq!(choose_with_user_priors(&[-3.0, ninf], &[1.0, f32::INFINITY], has_context), 0);
        }
        // -inf 之外的候选之间照常比较
        assert_eq!(choose_by_exact_scores(&[-5.0, ninf, -4.0], true), 2);
        assert_eq!(choose_by_exact_scores(&[-5.0, ninf, -4.0], false), 0);
        assert_eq!(choose_with_user_priors(&[-5.0, ninf, -4.0], &[8.0, 0.0, 0.0], true), 0);
    }

    #[test]
    fn unusable_score_vectors_are_recognised() {
        let ninf = f64::NEG_INFINITY;
        assert!(scores_are_usable(&[-3.0, -4.0]));
        assert!(scores_are_usable(&[ninf, -4.0]), "个别候选为 -inf 是合法的");
        assert!(scores_are_usable(&[-4.0, ninf, ninf]));
        assert!(!scores_are_usable(&[-3.0, f64::NAN]));
        assert!(!scores_are_usable(&[f64::NAN, f64::NAN]));
        assert!(!scores_are_usable(&[ninf, ninf]), "没有一个候选打得出分");
        assert!(!scores_are_usable(&[ninf, f64::NAN]));
        assert!(!scores_are_usable(&[]));
    }

    #[test]
    fn exact_score_rule_is_argmax_with_context_and_guarded_without() {
        // 有上文：直接取最高分，哪怕只高一点
        assert_eq!(choose_by_exact_scores(&[-5.0, -4.9, -9.0], true), 1);
        assert_eq!(choose_by_exact_scores(&[-2.0, -4.9, -9.0], true), 0);
        // 并列时保持当前首选
        assert_eq!(choose_by_exact_scores(&[-3.0, -3.0], true), 0);
        // 无上文：须高出 2.3 nat
        assert_eq!(choose_by_exact_scores(&[-6.0, -4.0], false), 0);
        assert_eq!(choose_by_exact_scores(&[-6.0, -3.5], false), 1);
        // 异常输入保持首选
        assert_eq!(choose_by_exact_scores(&[], true), 0);
        assert_eq!(choose_by_exact_scores(&[-1.0, f64::NAN], true), 0);
    }

    #[test]
    fn user_memory_prior_scales_explicit_choices_only() {
        let plain = UserMemory { boost: 2.0, explicit: false };
        let taught = UserMemory { boost: 2.0, explicit: true };
        assert_eq!(plain.prior(), 2.0);
        assert_eq!(taught.prior(), 2.0 * EXPLICIT_CHOICE_PRIOR_FACTOR);
        // 记忆强度本身不变（`Contested` 的门槛比较的是它）
        assert_eq!(taught.boost, 2.0);
        assert_eq!(UserMemory::default().prior(), 0.0);
        assert_eq!(UserMemory { boost: 0.0, explicit: true }.prior(), 0.0);
        // 负数、NaN、无穷大都按 0 算
        for odd in [-1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for explicit in [false, true] {
                assert_eq!(UserMemory { boost: odd, explicit }.prior(), 0.0, "{odd} {explicit}");
            }
        }
    }

    #[test]
    fn a_host_request_past_its_deadline_is_no_longer_pending() {
        let shared = InProcessShared::new(LlmCache::default(), NotifySlot::default());
        let chooser = HostChooser::new("test", "测试", Arc::new(|_| {})).with_timeout(Duration::from_secs(3600));
        shared.set_chooser(Some(chooser));
        let request = || InProcessRequest {
            cache_key: "chooser:test:0:上文\tshiyan".to_string(),
            pinyin_input: "shiyan".to_string(),
            surrounding_context: "上文".to_string(),
            pool: vec!["实验".to_string(), "誓言".to_string()],
            priors: vec![0.0, 0.0],
        };
        // 测试自己扮演后台线程：投递的任务先攒着，什么时候执行由测试决定；超时也由测试直接改时限来模拟
        let mut queued: Vec<InProcessDispatch> = Vec::new();
        let run_queued = |queued: &mut Vec<InProcessDispatch>| {
            for dispatch in queued.drain(..) {
                match dispatch {
                    InProcessDispatch::Choose(job) => shared.run_choose_job(job),
                    InProcessDispatch::Score(_) => panic!("没有装打分器"),
                }
            }
        };
        let expire = || {
            let mut state = shared.lock();
            let pending = state.pending_choice.as_mut().expect("有一个等待中的请求");
            pending.deadline = Instant::now() - Duration::from_millis(1);
        };

        assert!(!shared.result_pending());
        shared.lookup_or_request(Some(request()), &mut |d| queued.push(d));
        assert!(shared.result_pending(), "请求已登记、后台线程还没发出");
        run_queued(&mut queued);
        assert!(shared.result_pending(), "已发给宿主，还在等待时限之内");
        expire();
        assert!(!shared.result_pending(), "超过等待时限：迟到的答复只会被丢弃，再等也没有结果");

        // 同一输入再次重排：重新投递请求。后台线程发出它之前，超时的旧请求还登记着，但已经有新请求排队，仍算在等
        shared.lookup_or_request(Some(request()), &mut |d| queued.push(d));
        assert_eq!(queued.len(), 1);
        assert!(shared.result_pending(), "新请求排着队");
        run_queued(&mut queued);
        assert!(shared.result_pending(), "新请求还在时限之内");
        expire();
        assert!(!shared.result_pending());

        // 迟到的答复按超时丢弃，等待标记随之清除
        let id = shared.lock().pending_choice.as_ref().unwrap().id;
        assert_eq!(shared.deliver(id, Some("誓言")), ChooserDelivery::TimedOut);
        assert!(!shared.result_pending());
        assert!(shared.lock().wanted_key.is_none());
    }

    /// 宿主选词器的测试台：测试自己扮演后台线程（投递的任务先攒着，什么时候处理由测试决定，处理时按投递顺序
    /// 一个不落），超时由测试直接改时限来模拟；宿主收到的事件按顺序记下
    struct ChooserBench {
        shared: InProcessShared,
        queued: Vec<InProcessDispatch>,
        events: Arc<Mutex<Vec<ChooserEvent>>>,
    }

    impl ChooserBench {
        fn new() -> Self {
            let events: Arc<Mutex<Vec<ChooserEvent>>> = Arc::default();
            let sink = Arc::clone(&events);
            let callback: ChooserCallback = Arc::new(move |event| sink.lock().unwrap().push(event));
            let shared = InProcessShared::new(LlmCache::default(), NotifySlot::default());
            shared.set_chooser(Some(HostChooser::new("test", "测试", callback).with_timeout(Duration::from_secs(3600))));
            Self { shared, queued: Vec::new(), events }
        }

        /// 一次击键，等的是缓存键 `key` 的结果；`None` 表示这次击键不需要大模型
        fn keystroke(&mut self, key: Option<&str>) {
            let request = key.map(|key| InProcessRequest {
                cache_key: key.to_string(),
                pinyin_input: "shiyan".to_string(),
                surrounding_context: "上文".to_string(),
                pool: vec!["实验".to_string(), "誓言".to_string()],
                priors: vec![0.0, 0.0],
            });
            let queued = &mut self.queued;
            self.shared.lookup_or_request(request, &mut |d| queued.push(d));
        }

        fn run_queued(&mut self) {
            for dispatch in self.queued.drain(..) {
                match dispatch {
                    InProcessDispatch::Choose(job) => self.shared.run_choose_job(job),
                    InProcessDispatch::Score(_) => panic!("没有装打分器"),
                }
            }
        }

        fn expire(&self) {
            let mut state = self.shared.lock();
            let pending = state.pending_choice.as_mut().expect("有一个等待中的请求");
            pending.deadline = Instant::now() - Duration::from_millis(1);
        }

        fn pending_id(&self) -> Option<u64> {
            self.shared.lock().pending_choice.as_ref().map(|p| p.id)
        }

        /// 宿主收到的事件，`Request(编号)` 记作 `(true, 编号)`，`Cancel { 编号 }` 记作 `(false, 编号)`
        fn events(&self) -> Vec<(bool, u64)> {
            let events = self.events.lock().unwrap();
            events
                .iter()
                .map(|e| match e {
                    ChooserEvent::Request(r) => (true, r.id),
                    ChooserEvent::Cancel { id } => (false, *id),
                })
                .collect()
        }
    }

    #[test]
    fn a_queued_request_for_another_key_does_not_keep_an_expired_wait_pending() {
        let mut bench = ChooserBench::new();
        // 键 B：请求已发给宿主
        bench.keystroke(Some("B"));
        bench.run_queued();
        let first = bench.pending_id().unwrap();
        // 敲一个字母（键 A）：新请求排着队；后台线程处理它之前又删掉（回到键 B）：B 的请求还在等，不再投递
        bench.keystroke(Some("A"));
        bench.keystroke(Some("B"));
        assert_eq!(bench.queued.len(), 1);
        assert!(bench.shared.result_pending());
        // 后台线程这才处理 A 的任务：最新击键等的是 B、B 的请求还在时限之内，A 的任务被丢弃
        bench.run_queued();
        assert_eq!(bench.pending_id(), Some(first));
        assert!(bench.shared.result_pending(), "B 的请求还在时限之内");
        // B 的请求超时：什么也没有排着队，再等也等不到结果
        bench.expire();
        assert!(!bench.shared.result_pending());
        assert_eq!(bench.shared.deliver(first, Some("誓言")), ChooserDelivery::TimedOut);
        assert_eq!(bench.events(), [(true, first)], "宿主只收到 B 的那一次请求");
    }

    #[test]
    fn an_expired_request_voided_without_a_new_one_is_no_longer_pending() {
        // 同上，但后台线程处理 A 的任务时 B 的请求已经超时：B 的请求随之作废，也没有为 B 发出新请求
        let mut bench = ChooserBench::new();
        bench.keystroke(Some("B"));
        bench.run_queued();
        let first = bench.pending_id().unwrap();
        bench.keystroke(Some("A"));
        bench.keystroke(Some("B"));
        bench.expire();
        assert!(!bench.shared.result_pending(), "排着队的是 A 的请求，不会替 B 发出");
        bench.run_queued();
        assert_eq!(bench.pending_id(), None, "超时的请求被作废");
        assert!(!bench.shared.result_pending(), "没有为 B 发出新请求");
        assert_eq!(bench.shared.deliver(first, Some("誓言")), ChooserDelivery::Stale);
        // 同一输入再次重排：重新发出请求，又开始等
        bench.keystroke(Some("B"));
        assert!(bench.shared.result_pending());
        bench.run_queued();
        let second = bench.pending_id().unwrap();
        assert!(second > first);
        assert!(bench.shared.result_pending());
        assert_eq!(bench.events(), [(true, first), (false, first), (true, second)]);

        // 中间那次击键不需要大模型时也一样：作废任务排着队，回到 B 时 B 的请求还没超时、不再投递，
        // 超时之后后台线程才处理作废任务
        let mut bench = ChooserBench::new();
        bench.keystroke(Some("B"));
        bench.run_queued();
        let first = bench.pending_id().unwrap();
        bench.keystroke(None);
        bench.keystroke(Some("B"));
        assert_eq!(bench.queued.len(), 1);
        assert!(bench.shared.result_pending(), "B 的请求还在时限之内");
        bench.expire();
        bench.run_queued();
        assert_eq!(bench.pending_id(), None);
        assert!(!bench.shared.result_pending());
        assert_eq!(bench.shared.deliver(first, Some("誓言")), ChooserDelivery::Stale);
        assert_eq!(bench.events(), [(true, first), (false, first)]);
    }

    #[test]
    fn host_choice_is_aligned_to_pool() {
        let pool = vec!["AI 选词".to_string(), "爱选词".to_string()];
        assert_eq!(match_choice_in_pool(" 爱选词\n", &pool), Some("爱选词"));
        assert_eq!(match_choice_in_pool("AI选词", &pool), Some("AI 选词"));
        assert_eq!(match_choice_in_pool("爱炫词", &pool), None);
        assert_eq!(match_choice_in_pool("   ", &pool), None);
    }
}
