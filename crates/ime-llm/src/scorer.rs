//! 对外的打分器类型：在引擎外面加一把锁，使其 `Send + Sync`，并提供不经过锁的取消入口。

use crate::backend::{BackendLimits, CancelFlag, LmBackend};
use crate::engine::{ScoreStats, ScoringEngine};
use crate::LlmError;
use ime_ai::{CandidateScorer, ScoreError};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

/// 模型加载选项
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmOptions {
    /// 上下文长度（token）。所有并行候选分支共用这些 KV 单元。默认 256。
    /// 比 `max_parallel` 还小时，并行序列数随之降到这个值。llama.cpp 会把实际的上下文长度向上取整
    /// （实测请求 1 ~ 16 时得到的都是 256），生效的值见 [`LlmScorer::limits`]
    pub context_size: u32,
    /// 推理线程数；0 表示自动（性能核数量，上限 4）
    pub threads: u32,
    /// 放到 GPU 的层数。默认 0（纯 CPU：键盘扩展里加载 Metal 后端本身就要多占约 90 MiB）
    pub gpu_layers: i32,
    /// 并行序列数上限（一批里最多同时展开多少个候选分支）。默认 8。
    /// 实际生效的值不超过 llama.cpp 的上限，也不超过 `context_size`。它同时是 logits 行数的下限（见 `max_outputs`）
    pub max_parallel: u32,
    /// 单次解码最多读取多少行 logits，超出时引擎自动把一批拆成多次解码。默认 8。
    ///
    /// llama.cpp 要求行数不少于并行序列数，所以实际生效的行数是 `max(max_outputs, 生效的并行序列数)`，
    /// 再以 `context_size` 为上限；输出缓冲为「生效的行数 × 词表大小 × 4」字节
    /// （词表 15 万的模型每行约 0.58 MiB：把 `max_parallel` 调大，即使 `max_outputs` 不变缓冲也会变大）。
    /// 生效的值见 [`LlmScorer::limits`]
    pub max_outputs: u32,
    /// 是否用 mmap 加载权重（默认开启；关闭后权重全部计入进程内存）
    pub use_mmap: bool,
    /// 是否允许 llama.cpp 重排权重（默认关闭；开启会把权重复制进匿名内存，内存占用成倍增加）
    pub repack_weights: bool,
    /// 是否把 llama.cpp 的日志输出到 stderr
    pub verbose: bool,
}

impl Default for LlmOptions {
    fn default() -> Self {
        Self {
            context_size: 256,
            threads: 0,
            gpu_layers: 0,
            max_parallel: 8,
            max_outputs: 8,
            use_mmap: true,
            repack_weights: false,
            verbose: false,
        }
    }
}

/// 进程内语言模型打分器。`Send + Sync`：内部加锁，同一时刻只有一个打分请求在执行。
pub struct LlmScorer {
    engine: Mutex<ScoringEngine<Box<dyn LmBackend>>>,
    cancel: Arc<CancelFlag>,
    /// 模型标识（默认取模型文件名，不含扩展名）
    model_id: String,
}

impl LlmScorer {
    /// 当前构建是否包含进程内 LLM 支持（即是否启用了 `llama` 特性）
    pub fn is_supported() -> bool {
        cfg!(feature = "llama")
    }

    /// 加载 GGUF 模型。未启用 `llama` 特性的构建始终返回 [`LlmError::Unsupported`]。
    pub fn load(model_path: impl AsRef<Path>, options: LlmOptions) -> Result<Self, LlmError> {
        #[cfg(feature = "llama")]
        {
            let backend = crate::llama::LlamaBackend::load(model_path.as_ref(), &options)?;
            let model_id = model_path
                .as_ref()
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            Ok(Self::with_backend(Box::new(backend)).with_model_id(model_id))
        }
        #[cfg(not(feature = "llama"))]
        {
            let _ = (model_path.as_ref(), options);
            Err(LlmError::Unsupported)
        }
    }

    /// 用任意后端构造打分器（测试时注入假后端）
    pub fn with_backend(backend: Box<dyn LmBackend>) -> Self {
        let cancel = Arc::new(CancelFlag::new());
        Self {
            engine: Mutex::new(ScoringEngine::new(backend, Arc::clone(&cancel))),
            cancel,
            model_id: "custom".to_string(),
        }
    }

    /// 指定模型标识（作为 [`CandidateScorer::model_id`] 参与调度器的结果缓存键）
    pub fn with_model_id(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = model_id.into();
        self
    }

    /// 模型标识：[`Self::load`] 取模型文件名（不含扩展名），[`Self::with_backend`] 默认为 `custom`
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// 计算每个候选在「引导语 + 上文末尾」之后的联合对数似然，返回顺序与 `candidates` 一致。
    ///
    /// 只累加各候选在首个分歧 token 及其之后的部分（公共前缀对所有候选相同），因此分数只在同一次调用的
    /// 候选之间可比。被 [`Self::cancel`] 取消时返回 [`LlmError::Cancelled`]。
    ///
    /// 没有可打分 token 的候选——空候选，或者打分文本的 token 序列是其余所有候选的公共前缀（如 `去` 之于
    /// `去公园`、`去公元`）——返回 [`f64::NEG_INFINITY`]，表示「不能选它」：它没有任何 token 参与比较，
    /// 记 0 分的话会高于所有其它候选（对数似然恒不大于 0），不看上文就胜出。
    /// 例外是没有任何候选有可打分 token 的情况（只有一个候选、所有候选的文本相同）：此时全部返回 `0.0`。
    /// 其余候选的得分都是有限的非正数。
    pub fn score<S: AsRef<str>>(&self, context: &str, candidates: &[S]) -> Result<Vec<f64>, LlmError> {
        let ticket = self.cancel.ticket();
        let mut engine = self.lock();
        self.cancel.begin(ticket);
        engine.score(context, candidates)
    }

    /// 参考实现：每个候选都清空缓存、从头单独解码（慢，约为 [`Self::score`] 的三倍耗时）。
    /// 用于校验批量 / 复用路径的数值一致性（实测的误差范围见 crate 文档的「数值一致性」一节）。
    /// 对没有可打分 token 的候选的处理与 [`Self::score`] 相同。
    pub fn score_naive<S: AsRef<str>>(&self, context: &str, candidates: &[S]) -> Result<Vec<f64>, LlmError> {
        let ticket = self.cancel.ticket();
        let mut engine = self.lock();
        self.cancel.begin(ticket);
        engine.score_naive(context, candidates)
    }

    /// 取消所有在此之前发起、尚未完成的打分请求（排队中的直接返回；正在执行的若在解码候选分支，推理中途退出，
    /// 若在解码公共前缀，先把正在算的那一块——最多 16 个 token——算完再退出，这一块留在缓存里给下一次请求用）。
    /// 不获取内部锁，可在任意线程随时调用；之后新发起的请求不受影响。
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// 丢弃跨调用保留的 KV 缓存（例如切换输入框之后）
    pub fn clear_cache(&self) {
        self.lock().invalidate();
    }

    /// 最近一次打分调用的统计信息
    pub fn last_stats(&self) -> ScoreStats {
        self.lock().stats()
    }

    /// 后端实际生效的容量上限
    pub fn limits(&self) -> BackendLimits {
        self.lock().limits()
    }

    fn lock(&self) -> MutexGuard<'_, ScoringEngine<Box<dyn LmBackend>>> {
        match self.engine.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                // 持锁线程曾经恐慌：KV 状态不可信，丢弃缓存后继续使用
                let mut guard = poisoned.into_inner();
                guard.invalidate();
                self.engine.clear_poison();
                guard
            }
        }
    }
}

/// 接入 `ime-ai` 调度器的后台重排流程：`DualModeAiOrchestrator::set_candidate_scorer(Some(Arc::new(scorer)))`
///
/// 分数的约定与 [`LlmScorer::score`] 相同，对应 `CandidateScorer::score` 的契约：
/// - 没有可打分 token 的候选（空候选、其余所有候选的公共前缀）是 `-inf`，调度器永远不会选中它
///   （`ime_ai::choose_by_exact_scores`）；当前首选是 `-inf` 而别的候选有分数时，调度器会换成其中最高的
/// - 只有一个候选或所有候选相同时返回全 0
/// - 不会返回 NaN，也不会返回全是 `-inf` 的结果（调度器把这两种都当作打分失败，见 `ime_ai::scores_are_usable`）
impl CandidateScorer for LlmScorer {
    fn model_id(&self) -> String {
        self.model_id.clone()
    }

    fn score(&self, context: &str, candidates: &[String]) -> Result<Vec<f64>, ScoreError> {
        LlmScorer::score(self, context, candidates).map_err(|e| match e {
            LlmError::Cancelled => ScoreError::Cancelled,
            other => ScoreError::Failed(other.to_string()),
        })
    }

    fn cancel(&self) {
        LlmScorer::cancel(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeBackend;

    #[test]
    fn scorer_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LlmScorer>();
    }

    #[test]
    fn default_options_match_the_measured_configuration() {
        let o = LlmOptions::default();
        assert_eq!((o.context_size, o.gpu_layers, o.max_parallel), (256, 0, 8));
        assert!(o.use_mmap && !o.repack_weights);
        assert!(o.max_outputs >= o.max_parallel);
    }

    #[cfg(not(feature = "llama"))]
    #[test]
    fn load_reports_missing_llm_support() {
        assert!(!LlmScorer::is_supported());
        let err = LlmScorer::load("/nonexistent/model.gguf", LlmOptions::default()).err().unwrap();
        assert_eq!(err, LlmError::Unsupported);
        assert!(err.to_string().contains("未包含进程内 LLM 支持"));
        assert_eq!(crate::phys_footprint_bytes(), None);
    }

    #[cfg(feature = "llama")]
    #[test]
    fn load_reports_missing_model_file() {
        assert!(LlmScorer::is_supported());
        let err = LlmScorer::load("/nonexistent/model.gguf", LlmOptions::default()).err().unwrap();
        assert!(matches!(err, LlmError::Load(_)), "{err:?}");
    }

    #[test]
    fn cancel_only_affects_requests_issued_before_it() {
        let scorer = LlmScorer::with_backend(Box::new(FakeBackend::with_default_limits()));
        let cands = ["誓言", "食盐", "试验"];
        let first = scorer.score("他在婚礼上许下了", &cands).unwrap();
        // 没有请求在执行时调用 cancel：不影响之后的请求
        scorer.cancel();
        scorer.cancel();
        assert_eq!(scorer.score("他在婚礼上许下了", &cands).unwrap(), first);
        assert_eq!(scorer.score_naive("他在婚礼上许下了", &cands).unwrap().len(), 3);
        scorer.clear_cache();
        assert_eq!(scorer.score("他在婚礼上许下了", &cands).unwrap(), first);
        assert_eq!(scorer.last_stats().reused_tokens, 0);
        assert_eq!(scorer.limits().n_seq_max, 8);
    }

    #[test]
    fn implements_the_orchestrator_scorer_trait() {
        let scorer: Arc<dyn CandidateScorer> =
            Arc::new(LlmScorer::with_backend(Box::new(FakeBackend::with_default_limits())).with_model_id("fake-model"));
        assert_eq!(scorer.model_id(), "fake-model");
        let cands = vec!["誓言".to_string(), "食盐".to_string(), "试验".to_string()];
        let via_trait = scorer.score("他在婚礼上许下了", &cands).unwrap();
        let direct = LlmScorer::with_backend(Box::new(FakeBackend::with_default_limits()))
            .score("他在婚礼上许下了", &cands)
            .unwrap();
        assert_eq!(via_trait, direct);
        // 没有请求在执行时取消：之后的请求不受影响
        scorer.cancel();
        assert_eq!(scorer.score("他在婚礼上许下了", &cands).unwrap(), direct);
    }

    /// 「去」是其余候选的公共前缀：它没有可打分的 token。记 0 分时它在任何上文下都是最高分
    #[test]
    fn prefix_candidate_is_never_the_orchestrators_pick() {
        let scorer: Arc<dyn CandidateScorer> = Arc::new(LlmScorer::with_backend(Box::new(FakeBackend::with_default_limits())));
        let pool = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        for context in ["周末天气不错，我们", "", "他说"] {
            let has_context = !context.is_empty();
            // 前缀候选排在后面：不会被选中
            let scores = scorer.score(context, &pool(&["去公园玩", "去", "去公元万"])).unwrap();
            assert_eq!(scores[1], f64::NEG_INFINITY);
            assert!(scores[0].is_finite() && scores[2].is_finite(), "{scores:?}");
            assert!(ime_ai::scores_are_usable(&scores));
            assert_ne!(ime_ai::choose_by_exact_scores(&scores, has_context), 1);
            assert_ne!(ime_ai::choose_with_user_priors(&scores, &[0.0, 12.0, 0.0], has_context), 1);
            // 空候选同理
            let scores = scorer.score(context, &pool(&["去公园玩", "", "去公元万"])).unwrap();
            assert_eq!(scores[1], f64::NEG_INFINITY);
            assert_ne!(ime_ai::choose_by_exact_scores(&scores, has_context), 1);
        }
        // 没有任何候选可比时全 0：调度器保持当前首选
        let scores = scorer.score("上文", &pool(&["候选"])).unwrap();
        assert_eq!(scores, vec![0.0]);
        assert_eq!(ime_ai::choose_by_exact_scores(&scores, true), 0);
    }

    #[test]
    fn shared_across_threads() {
        let scorer = Arc::new(LlmScorer::with_backend(Box::new(FakeBackend::with_default_limits())));
        let expected = scorer.score("上文", &["候选一", "候选二", "后选一"]).unwrap();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let scorer = Arc::clone(&scorer);
                let expected = expected.clone();
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        assert_eq!(scorer.score("上文", &["候选一", "候选二", "后选一"]).unwrap(), expected);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }
}
