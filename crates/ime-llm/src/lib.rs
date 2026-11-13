//! `ime-llm`: 进程内的语言模型候选打分器（llama.cpp）
//!
//! Windows 版通过 HTTP 与 `llama-server.exe` 子进程通信来给候选打分（见 `ime-ai` 的
//! `score_candidates_by_lm`）。iOS 键盘扩展不能创建子进程，打分必须在键盘进程内完成，
//! 本 crate 提供的就是这条路径：
//!
//! - [`LlmScorer::load`]：mmap 加载 GGUF 模型（纯 CPU、不做权重重排，权重页不计入 `phys_footprint`）
//! - [`LlmScorer::score`]：返回每个候选在「引导语 + 上文末尾」之后的联合对数似然。文本构造规则与
//!   `score_candidates_by_lm` 共用同一份（见 [`text`]）；前缀用进程内版本 [`lm_scoring_prefix_inprocess`]，
//!   上文不超过 48 个字符、且不以「英文 / 数字 + 空白」结尾时与 HTTP 路径的文本逐字节相同。
//!   概率为整个词表上的精确 log-softmax（HTTP 路径只能拿到 Top-256 近似）
//! - 公共前缀只解码一次，所有候选分支合成一批；跨调用保留与上一次调用的最长公共 token 前缀
//! - [`LlmScorer::cancel`]：协作式取消。正在解码候选分支的过期请求在 llama.cpp 的计算中途即可退出；
//!   公共前缀按 16 个 token 一块解码，每一块都不可中止，取消在块与块之间生效（最多晚一块），
//!   正在算的那一块和之前算完的块都留在缓存里（见下文「取消与分块」）
//!
//! [`LlmScorer`] 实现了 `ime_ai::CandidateScorer`，可以直接装到调度器上
//! （`DualModeAiOrchestrator::set_candidate_scorer`），由调度器的后台线程在每次击键后调用。
//! 依赖方向是 `ime-llm → ime-ai`：`ime-ai` 只定义接口与打分文本规则，不依赖 llama.cpp。
//!
//! # 特性开关
//!
//! 默认不启用 `llama` 特性：此时本 crate 是不含任何 C/C++ 代码的纯 Rust（Windows 等所有平台都能编译），
//! 类型与接口保持不变，[`LlmScorer::load`] 返回 [`LlmError::Unsupported`]。
//!
//! 启用 `llama` 特性后，`build.rs` 用 `cc` 编译 `shim/ime_llm_shim.cpp`（把 llama.cpp 的 C API 收窄为一组稳定的
//! C ABI，Rust 一侧不声明 llama.cpp 的任何结构体）并链接 `scripts/build_llama.sh` 产出的静态库。
//! 相关环境变量见 `build.rs` 顶部说明。
//!
//! ```text
//! # macOS：构建静态库后直接测试
//! LLAMA_CPP_SRC=/path/to/llama.cpp scripts/build_llama.sh macos
//! cargo test -p ime-llm --features llama
//! IME_LLM_TEST_MODEL=/path/to/model.gguf cargo test --release -p ime-llm --features llama --test model_backed -- --nocapture
//! IME_LLM_TEST_MODEL=/path/to/model.gguf cargo test --release -p ime-llm --features llama --test model_options -- --nocapture
//!
//! # iOS：Rust 只编译垫片，llama.cpp 由 Xcode 链接 third_party/llama/ImeLlama.xcframework
//! scripts/build_llama.sh ios ios-sim xcframework
//! IME_LLM_NO_LINK=1 IPHONEOS_DEPLOYMENT_TARGET=16.4 cargo build --release --features llama --target aarch64-apple-ios ...
//! ```
//!
//! # 取消与分块
//!
//! 公共前缀按 16 个 token 一块解码，每一块都在不可中止区段里算（[`CancelFlag::set_shielded`]；区段里
//! llama.cpp 的中止回调问的 [`CancelFlag::should_abort`] 是 `false`）。前缀只取决于上文，同一上文下的下一次击键
//! 要的是同一段前缀，所以取消到达时正在算的那一块照常算完，和之前算完的块一起留在序列 0 的缓存里，
//! 下一个请求从那里接着算。代价是前缀阶段的取消要等这一块算完才生效，最多晚一块（≤16 个 token，
//! 这台 Mac 上 2 线程约 60 ~ 75 ms）。候选分支只对这一组候选有用，仍然可以在 llama.cpp 的计算中途中止，
//! 被中止的一批整批作废（llama.cpp 会把这一批写入的位置全部移除）。
//!
//! 所以击键间隔比一块的解码时间还短时，被取消的请求也会把正在算的那一块算完，前缀照样一块一块推进
//! （`engine.rs` 的测试 `keystrokes_faster_than_one_prefix_chunk_still_make_progress`）。
//!
//! 实测（这台 Mac，2 线程；缓存里只有引导语，新上文带来 25 个新的前缀 token；每次击键取消上一个请求并发起新请求，
//! 共 8 次击键，候选逐次变长）。「16 个 token 一块，可中止」一列是加不可中止区段之前的做法：每一块都可以中止，
//! 被取消时正在算的那一块整块作废。
//!
//! | 击键间隔 | 模型 | 整段前缀一次解码 | 16 个 token 一块，可中止 | 16 个 token 一块，不可中止（现在） |
//! |---|---|---|---|---|
//! | 100 ms | Qwen2.5-0.5B | 前 7 个请求全部被取消，最后一次击键之后 185 ms 才有第一个结果 | 第 2 次击键的请求就有结果，8 个请求完成 7 个 | 同左 |
//! | 100 ms | Qwen3-0.6B | 同上，最后一次击键之后 165 ms | 同上 | 同左 |
//! | 60 ms | Qwen2.5-0.5B | 前 7 个请求全部被取消 | 前 7 个请求全部被取消 | 第 3 次击键的请求就有结果，8 个请求完成 5 个 |
//! | 60 ms | Qwen3-0.6B | 前 7 个请求全部被取消 | 8 个请求完成 3 个 | 同上 |
//!
//! 60 ms 比一块的解码时间还短：可以中止时每个请求大多连第一块都算不完（Qwen3-0.6B 的一块稍快，有时能算完）；
//! 不可中止之后前两个请求就把前缀算完了，之后的请求只需重算最后一个 token 和候选分支。现在被取消的 3 个是
//! 前两个请求和第 7 次击键的请求，后者的候选更长，不被打断时要算约 80 ms，比击键间隔长（见下文「仍然存在的限制」）。
//! 最后一列是加了不可中止区段之后另测的，每一格测 3 次，结果都相同。
//!
//! 分块没有让解码变慢。同一组 14 个用例（上文不超过 48 个字符，打分文本相同）的冷缓存平均耗时，
//! 原先 → 现在：Qwen2.5-0.5B 4 线程 73.0 → 73.3 ms、2 线程 139.5 → 139.2 ms；
//! Qwen3-0.6B 4 线程 65.0 → 64.8 ms、2 线程 124.1 → 122.4 ms；同一上文的连续请求（热缓存）不变。
//!
//! 内存：加载后的 `phys_footprint` 不变（55.3 / 80.4 MiB）。`tools/eval` 四个用例集的峰值与原先相差
//! -0.1 ~ +0.4 MiB（同一个二进制多次运行之间也有约 ±0.2 MiB 的波动）；长短上文混合的 1500 次随机调用之后，
//! 占用从 65.5 降到 63.7 MiB（Qwen2.5-0.5B）、从 89.2 降到 88.3 MiB（Qwen3-0.6B）。
//! 每一块的最后一个 token 都请求一行 logits 是为了内存：没有输出行的一批在 llama.cpp 里是另一种计算图布局，
//! 会多写脏约 1 MiB 的计算缓冲。
//!
//! 仍然存在的限制：候选分支仍然可以中止。击键间隔比「重算前缀的最后一个 token 加上解码候选分支」的时间还短时，
//! 每个请求都在分支阶段被取消，结果还是要等停手才出来（上表 60 ms 两行里第 7 次击键的请求就是这样；
//! `keystrokes_faster_than_one_prefix_chunk_still_make_progress` 模拟的也是这种情况：前缀算完之后的请求也都被取消）；
//! 不过那时前缀已经在缓存里，停手之后只剩这一步要算。手机上的这些时间都没有测过。
//!
//! # 长上文与缓存复用
//!
//! 上文超过 48 个字符之后，固定取「最后 48 个字符」的窗口每上屏一次就向后滑，前缀从引导语之后就与缓存
//! 对不上，每次都要重算整段上文。进程内前缀把窗口起点对齐到分句处（`ime_ai::lm_text::lm_context_tail_anchored`），
//! 起点在接下来的三十来个字符里不动。实测（2 线程，上文每步增加 3 个字符，超过 48 个字符之后的 16 步）：
//! 原先每一步都重算 25 ~ 27 个前缀 token，平均 174 ms（Qwen2.5-0.5B）/ 163 ms（Qwen3-0.6B）；
//! 现在 13 步只需解码 1 ~ 3 个 token（53 ~ 66 ms），窗口起点跳到下一个分句处的 3 步解码 15 ~ 21 个 token
//! （115 ~ 146 ms），16 步平均 72 ms。
//!
//! # 数值一致性
//!
//! 批量 / 复用路径与「每个候选从头单独解码」（[`LlmScorer::score_naive`]）在数学上等价，数值上是否一致取决于
//! ggml 为每次解码选用的内核，而内核的选择与一次解码的 token 数有关：
//!
//! - 注意力算子在一次解码达到 64 个 token 时换用分块内核（`ggml-cpu/ops.cpp` 的 `Q_TILE_SZ`）。实测 Qwen3-0.6B
//!   同一行 logits 在 62、63 个 token 的解码与逐 token 解码之间逐位相同，64 个 token 时最大相差 0.77 nat。
//!   所以本 crate 的任何一次解码（包括 `score_naive`）都不超过 63 个 token。
//! - 启用 ARM i8mm 指令的构建（macOS 本机构建）里，Q4_0 / Q8_0 / Q4_K 权重的矩阵乘法在 token 数为偶数时
//!   走一次算两行两列的内核，奇数时走逐行内核（`ggml-cpu.c` 的 `num_rows_per_vec_dot`）。
//!
//! 在 63 个 token 以内，实测结果（每个模型 900 次随机的上文 / 候选池调用，其中约三分之一在推理中途被取消，
//! 与另一个打分器实例的 `score_naive` 对比）：
//!
//! | 模型 | macOS 本机构建（含 i8mm） | 按 iOS 的编译选项构建（armv8.4-a，无 i8mm，在 Mac 上运行） |
//! |---|---|---|
//! | Qwen3-0.6B Q4_K_M | 逐位一致（最大差值 0） | 逐位一致（600 次调用） |
//! | Qwen2.5-0.5B q4_k_m | 最大相差 0.81 nat，约八分之一的调用超过 0.3 nat | 最大相差 0.000003 nat |
//!
//! Qwen2.5-0.5B 的 q4_k_m 含 Q8_0 张量，两行内核与逐行内核的舍入不同，于是 macOS 本机构建下同一个请求的得分
//! 随「前缀是一次算完的还是接着缓存算的」（每次解码的 token 数的奇偶）而变。900 次调用里首选与 `score_naive`
//! 不同的有 6 次，都是同一个池里得分相差 0.12 nat 以内的两个候选（权利 / 全力）；`tests/model_backed.rs`
//! 的 10 个用例最大相差 0.32 nat，首选都没有变。真机上的数值没有测过（上表右列是在 Mac 上用相同的编译选项测的）。

pub mod backend;
mod engine;
#[doc(hidden)]
pub mod fake;
#[cfg(feature = "llama")]
mod llama;
mod scorer;
pub mod text;

pub use backend::{BackendLimits, BatchItem, CancelFlag, DecodeOutcome, LmBackend};
pub use engine::ScoreStats;
pub use scorer::{LlmOptions, LlmScorer};
pub use text::{
    lm_context_tail, lm_context_tail_anchored, lm_scoring_prefix, lm_scoring_prefix_inprocess, lm_scoring_text,
    normalize_for_lm_scoring, LM_SCORE_CONTEXT_ANCHOR_MIN_CHARS, LM_SCORE_CONTEXT_TAIL_CHARS, LM_SCORE_PROMPT_PREFIX,
};

/// 打分器的错误类型
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LlmError {
    /// 编译时未启用 `llama` 特性
    #[error("当前构建未包含进程内 LLM 支持（编译时未启用 ime-llm 的 `llama` 特性）")]
    Unsupported,
    #[error("模型加载失败: {0}")]
    Load(String),
    #[error("分词失败: {0}")]
    Tokenize(String),
    /// 某个候选（连同前缀）长到放不进上下文
    #[error("超出上下文容量：需要 {needed} 个 token 位置，上限 {capacity}")]
    ContextOverflow { needed: usize, capacity: usize },
    /// 请求被 [`LlmScorer::cancel`] 取消
    #[error("打分请求已取消")]
    Cancelled,
    #[error("推理后端错误: {0}")]
    Backend(String),
}

/// 当前进程的 `phys_footprint`（字节），即 iOS 对键盘扩展计内存上限时使用的口径。
/// 仅在启用 `llama` 特性的 Apple 平台上可用，其它情况返回 `None`。
pub fn phys_footprint_bytes() -> Option<u64> {
    #[cfg(feature = "llama")]
    {
        llama::phys_footprint_bytes()
    }
    #[cfg(not(feature = "llama"))]
    {
        None
    }
}
