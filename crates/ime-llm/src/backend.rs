//! 打分引擎与具体推理后端之间的窄接口。
//!
//! 引擎（`engine.rs`）只依赖这里的 [`LmBackend`]：真实实现是 llama.cpp 垫片（`llama.rs`，`llama` 特性），
//! 测试用实现是 [`crate::fake::FakeBackend`]。KV 操作的语义与 llama.cpp 的 `llama_memory_*` 一致。

use crate::LlmError;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// 一批解码输入中的一个 token
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchItem {
    pub token: i32,
    /// 在所属序列中的位置（从 0 起，必须紧接该序列已有的最后一个位置）
    pub pos: i32,
    /// 序列号，`0..n_seq_max`
    pub seq: i32,
    /// 是否需要该 token 之后的下一 token 概率分布
    pub want_logits: bool,
}

/// 一次 `decode` 的非错误结局
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeOutcome {
    Ok,
    /// KV 缓存放不下本批；缓存状态与调用前相同
    NoKvSlot,
    /// 被取消；本批写入的位置已被后端移除，此前的缓存内容仍然有效
    Aborted,
}

/// 后端的容量上限
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendLimits {
    /// KV 单元总数（所有序列共用）
    pub n_ctx: usize,
    /// 单次 `decode` 的 token 数上限
    pub n_batch: usize,
    /// 并行序列数上限
    pub n_seq_max: usize,
    /// 单次 `decode` 中 `want_logits` 的 token 数上限
    pub n_outputs_max: usize,
}

/// 协作式取消标记。
///
/// 采用「世代」而不是单个布尔值：[`CancelFlag::cancel`] 让世代加一，每次打分在进入时领取当时的世代作为票据，
/// 票据过期即视为已取消。这样 `cancel()` 能取消所有在它之前发起的请求（包括还在排队等锁的），
/// 又不会误伤它之后才发起的请求。
///
/// 另有一个「不可中止」区段（[`CancelFlag::set_shielded`]）：区段内推理库的中止回调（[`CancelFlag::should_abort`]）
/// 不响应取消，取消照样记下，由引擎在区段结束后用 [`CancelFlag::is_cancelled`] 检查。
#[derive(Debug, Default)]
pub struct CancelFlag {
    epoch: AtomicU64,
    active: AtomicU64,
    shielded: AtomicBool,
}

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取消所有已发起但尚未完成的请求
    pub fn cancel(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// 领取当前世代作为票据（在请求入口、加锁之前调用）
    pub fn ticket(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// 声明持有 `ticket` 的请求开始执行（持有引擎锁期间调用）
    pub fn begin(&self, ticket: u64) {
        self.active.store(ticket, Ordering::SeqCst);
    }

    /// 当前正在执行的请求是否已被取消。只做两次原子读取，可在推理线程的中止回调里高频调用。
    pub fn is_cancelled(&self) -> bool {
        self.epoch.load(Ordering::SeqCst) != self.active.load(Ordering::SeqCst)
    }

    /// 进入（`true`）/ 离开（`false`）不可中止区段。引擎用它包住公共前缀的每一块解码：同一上文下的
    /// 下一次请求要的是同一段前缀，算到一半扔掉只会在快速打字（击键间隔短于一块的解码时间）时永远算不完
    pub fn set_shielded(&self, shielded: bool) {
        self.shielded.store(shielded, Ordering::SeqCst);
    }

    pub fn is_shielded(&self) -> bool {
        self.shielded.load(Ordering::SeqCst)
    }

    /// 推理库的中止回调用：当前请求已被取消，且不在不可中止区段里
    pub fn should_abort(&self) -> bool {
        !self.is_shielded() && self.is_cancelled()
    }
}

/// 因果语言模型后端：分词、带 KV 缓存的批量解码、读取下一 token 的对数概率。
pub trait LmBackend: Send {
    fn limits(&self) -> BackendLimits;

    /// 把文本整体分词（不加 BOS 等特殊 token，不解析特殊 token 文本）
    fn tokenize(&mut self, text: &str) -> Result<Vec<i32>, LlmError>;

    /// 清空全部 KV 缓存
    fn kv_clear(&mut self);

    /// 从序列 `seq` 移除位置 `[p0, p1)`；`p0 < 0` 表示从头，`p1 < 0` 表示到末尾。
    /// 返回 `false` 表示后端无法做部分移除（移除整条序列不会失败）。
    fn kv_seq_rm(&mut self, seq: i32, p0: i32, p1: i32) -> bool;

    /// 让序列 `dst` 共享序列 `src` 在 `[p0, p1)` 内的缓存内容
    fn kv_seq_cp(&mut self, src: i32, dst: i32, p0: i32, p1: i32);

    /// 解码一批 token。`Err` 表示后端状态未知，调用方必须清空 KV 后才能继续使用。
    fn decode(&mut self, batch: &[BatchItem]) -> Result<DecodeOutcome, LlmError>;

    /// 最近一次成功 `decode` 中，第 `batch_index` 个 token（必须 `want_logits`）之后各 `targets` 的对数概率
    /// （对整个词表做精确 log-softmax）。写入 `out[..targets.len()]`。
    fn logprobs(&mut self, batch_index: usize, targets: &[i32], out: &mut [f64]) -> Result<(), LlmError>;

    /// 安装取消标记，让后端能在一次 `decode` 的计算中途响应取消。计算中途只按 [`CancelFlag::should_abort`]
    /// 决定是否中止，不能用 [`CancelFlag::is_cancelled`]：不可中止区段里的一批即使已被取消也要算完。
    /// 默认忽略（引擎仍会在两次 `decode` 之间检查）。
    fn install_cancel(&mut self, _flag: Arc<CancelFlag>) {}
}

impl<B: LmBackend + ?Sized> LmBackend for Box<B> {
    fn limits(&self) -> BackendLimits {
        (**self).limits()
    }
    fn tokenize(&mut self, text: &str) -> Result<Vec<i32>, LlmError> {
        (**self).tokenize(text)
    }
    fn kv_clear(&mut self) {
        (**self).kv_clear()
    }
    fn kv_seq_rm(&mut self, seq: i32, p0: i32, p1: i32) -> bool {
        (**self).kv_seq_rm(seq, p0, p1)
    }
    fn kv_seq_cp(&mut self, src: i32, dst: i32, p0: i32, p1: i32) {
        (**self).kv_seq_cp(src, dst, p0, p1)
    }
    fn decode(&mut self, batch: &[BatchItem]) -> Result<DecodeOutcome, LlmError> {
        (**self).decode(batch)
    }
    fn logprobs(&mut self, batch_index: usize, targets: &[i32], out: &mut [f64]) -> Result<(), LlmError> {
        (**self).logprobs(batch_index, targets, out)
    }
    fn install_cancel(&mut self, flag: Arc<CancelFlag>) {
        (**self).install_cancel(flag)
    }
}
