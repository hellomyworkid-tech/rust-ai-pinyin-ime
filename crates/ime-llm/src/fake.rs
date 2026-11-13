//! 测试用的假后端：不需要任何模型文件，却严格模拟 llama.cpp KV 缓存的约束。
//!
//! - 「模型」是确定性的：下一 token 的对数概率只取决于（该序列到当前位置为止的全部 token，目标 token）的哈希，
//!   因此只要引擎把 KV 操作（复用、复制、移除）做错一步，得分就会与 [`FakeBackend::reference_score`] 不一致
//! - 违反容量上限（单批 token 数、logits 行数、序列号范围、位置不连续）会直接返回错误，
//!   这些情况在真实的 llama.cpp 里是断言失败
//! - KV 单元按「统一缓存」计数：`kv_seq_cp` 共享单元，不额外占用
//! - 取消与 llama.cpp 的中止回调一致：一批是否中止只看 [`CancelFlag::should_abort`]
//!   （不可中止区段里为 `false`，这一批照常算完），被中止的批不进缓存

use crate::backend::{BackendLimits, BatchItem, CancelFlag, DecodeOutcome, LmBackend};
use crate::LlmError;
use std::collections::HashSet;
use std::sync::Arc;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn mix(history: u64, token: i32) -> u64 {
    let mut h = history;
    for b in token.to_le_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

fn logprob_from(history: u64, target: i32) -> f64 {
    // splitmix64 收尾，映射到 (-12, 0]
    let mut x = mix(history, target) ^ 0x9e37_79b9_7f4a_7c15;
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    -((x >> 11) as f64 / (1u64 << 53) as f64) * 12.0
}

#[derive(Debug, Clone, Copy)]
struct Cell {
    token: i32,
    /// 统一缓存中的单元编号；`kv_seq_cp` 之后多个序列共享同一编号
    id: u64,
    /// 到该位置为止（含）的 token 历史哈希
    history: u64,
}

/// 假后端的调用计数，用于断言缓存复用确实发生
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FakeCounters {
    pub decode_calls: usize,
    pub tokens_decoded: usize,
    /// 单次 decode 收到的最大 token 数（含被中止的那些批）
    pub max_batch_tokens: usize,
    /// 没有任何 token 请求 logits 的 decode 次数
    pub decodes_without_outputs: usize,
    pub kv_clears: usize,
    pub tokenize_calls: usize,
}

#[derive(Debug)]
pub struct FakeBackend {
    limits: BackendLimits,
    seqs: Vec<Vec<Cell>>,
    next_cell_id: u64,
    /// 最近一次 decode 各行的历史哈希（未请求 logits 的行为 `None`）
    rows: Vec<Option<u64>>,
    /// 分词时优先合并的多字符片段（模拟 BPE 跨越「前缀 / 候选」边界合并的情况）
    merges: Vec<(String, i32)>,
    cancel: Option<Arc<CancelFlag>>,
    /// 第 N 次（从 1 起）decode 调用返回 `Aborted`（见 [`FakeBackend::abort_on_decode()`]）
    abort_on_decode: Option<usize>,
    /// 第 N 次（从 1 起）decode 调用时触发取消标记（见 [`FakeBackend::cancel_on_decode()`]）
    cancel_on_decode: Option<usize>,
    /// 累计成功解码的 token 数会超过这个值的 decode 在取消标记上记一次取消（见 [`FakeBackend::abort_after_tokens`]）
    abort_token_limit: Option<usize>,
    /// 为 `true` 时部分移除（`p0 > 0`）失败，模拟不支持部分移除的缓存类型
    pub refuse_partial_rm: bool,
    pub counters: FakeCounters,
}

impl FakeBackend {
    pub fn new(limits: BackendLimits) -> Self {
        Self {
            limits,
            seqs: vec![Vec::new(); limits.n_seq_max],
            next_cell_id: 0,
            rows: Vec::new(),
            merges: Vec::new(),
            cancel: None,
            abort_on_decode: None,
            cancel_on_decode: None,
            abort_token_limit: None,
            refuse_partial_rm: false,
            counters: FakeCounters::default(),
        }
    }

    /// 与默认 [`crate::LlmOptions`] 相同的容量：n_ctx 256、8 条并行序列、单批 8 行 logits
    pub fn with_default_limits() -> Self {
        Self::new(BackendLimits { n_ctx: 256, n_batch: 256, n_seq_max: 8, n_outputs_max: 8 })
    }

    /// 让分词器把 `piece` 合并成单个 token
    pub fn add_merge(&mut self, piece: &str) {
        let id = 0x20_0000 + self.merges.len() as i32;
        self.merges.push((piece.to_string(), id));
        // 长片段优先匹配
        self.merges.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    }

    /// 从现在起第 `nth` 次 decode 直接返回 `Aborted`，不看取消标记，不可中止区段里也一样。
    /// 用来检验引擎对 `Aborted` 的收尾；真实后端只在 [`CancelFlag::should_abort`] 为真时中止，
    /// 模拟取消请用 [`Self::cancel_on_decode()`] 或 [`Self::abort_after_tokens()`]
    pub fn abort_on_decode(&mut self, nth: usize) {
        self.abort_on_decode = Some(self.counters.decode_calls + nth);
    }

    /// 从现在起第 `nth` 次 decode 算到一半时，另一线程在取消标记上调用 `cancel`。
    /// 这一批是否中止只看 [`CancelFlag::should_abort`]：不可中止区段里照常算完
    pub fn cancel_on_decode(&mut self, nth: usize) {
        self.cancel_on_decode = Some(self.counters.decode_calls + nth);
    }

    /// 模拟「下一次击键在固定时间后到来」：解码耗时大致与 token 数成正比，所以从现在起再成功解码 `budget` 个
    /// token 就到了时间。会让累计数超过它的那一次 decode（以及之后的每一次）算到一半时，取消标记上记一次取消；
    /// 这一批是否中止只看 [`CancelFlag::should_abort`]，与 llama.cpp 的中止回调相同：不可中止区段外整批作废、
    /// 不进缓存，区段里照常算完，由引擎在这一批之后检查取消。`None` 取消这个限制。
    /// 取消记在标记上，所以用这个功能的测试要像 `LlmScorer` 一样在每次请求前 `begin(ticket())`
    /// （没有安装取消标记时，超出的那一批直接返回 `Aborted`）。
    pub fn abort_after_tokens(&mut self, budget: Option<usize>) {
        self.abort_token_limit = budget.map(|b| self.counters.tokens_decoded + b);
    }

    /// 当前占用的 KV 单元数（共享单元只算一次）
    pub fn cells_used(&self) -> usize {
        let ids: HashSet<u64> = self.seqs.iter().flatten().map(|c| c.id).collect();
        ids.len()
    }

    /// 序列 `seq` 当前缓存的 token
    pub fn seq_tokens(&self, seq: usize) -> Vec<i32> {
        self.seqs[seq].iter().map(|c| c.token).collect()
    }

    pub fn tokenize_text(&self, text: &str) -> Vec<i32> {
        let mut tokens = Vec::new();
        let mut rest = text;
        'outer: while !rest.is_empty() {
            for (piece, id) in &self.merges {
                if rest.starts_with(piece.as_str()) {
                    tokens.push(*id);
                    rest = &rest[piece.len()..];
                    continue 'outer;
                }
            }
            let ch = rest.chars().next().unwrap_or('\u{fffd}');
            tokens.push(ch as i32);
            rest = &rest[ch.len_utf8()..];
        }
        tokens
    }

    /// 不经过任何缓存的参考得分：`sum_{i >= shared} log P(tokens[i] | tokens[..i])`
    pub fn reference_score(tokens: &[i32], shared: usize) -> f64 {
        let mut history = FNV_OFFSET;
        let mut total = 0.0;
        for (i, &t) in tokens.iter().enumerate() {
            if i >= shared {
                total += logprob_from(history, t);
            }
            history = mix(history, t);
        }
        total
    }
}

impl LmBackend for FakeBackend {
    fn limits(&self) -> BackendLimits {
        self.limits
    }

    fn tokenize(&mut self, text: &str) -> Result<Vec<i32>, LlmError> {
        self.counters.tokenize_calls += 1;
        Ok(self.tokenize_text(text))
    }

    fn kv_clear(&mut self) {
        self.counters.kv_clears += 1;
        for s in &mut self.seqs {
            s.clear();
        }
        self.rows.clear();
    }

    fn kv_seq_rm(&mut self, seq: i32, p0: i32, p1: i32) -> bool {
        let Some(cells) = self.seqs.get_mut(seq.max(0) as usize) else { return false };
        if seq < 0 {
            return false;
        }
        let p0 = p0.max(0) as usize;
        let p1 = if p1 < 0 { usize::MAX } else { p1 as usize };
        if p0 > 0 && p0 < cells.len() && self.refuse_partial_rm {
            return false;
        }
        if p1 < cells.len() {
            // 引擎只会移除尾部；中间挖洞在这里视为不支持
            return p0 >= p1;
        }
        cells.truncate(p0.min(cells.len()));
        true
    }

    fn kv_seq_cp(&mut self, src: i32, dst: i32, p0: i32, p1: i32) {
        let (src, dst) = (src as usize, dst as usize);
        if src >= self.seqs.len() || dst >= self.seqs.len() || src == dst {
            return;
        }
        let p0 = p0.max(0) as usize;
        let end = if p1 < 0 { self.seqs[src].len() } else { (p1 as usize).min(self.seqs[src].len()) };
        // 目标序列在该范围内已有的内容被覆盖（引擎只在目标为空时调用）
        let copied: Vec<Cell> = self.seqs[src][p0.min(end)..end].to_vec();
        self.seqs[dst].truncate(p0);
        if self.seqs[dst].len() == p0 {
            self.seqs[dst].extend(copied);
        }
    }

    fn decode(&mut self, batch: &[BatchItem]) -> Result<DecodeOutcome, LlmError> {
        self.counters.decode_calls += 1;
        self.counters.max_batch_tokens = self.counters.max_batch_tokens.max(batch.len());
        let call = self.counters.decode_calls;
        if batch.is_empty() || batch.len() > self.limits.n_batch {
            return Err(LlmError::Backend(format!("批大小 {} 超出 n_batch {}", batch.len(), self.limits.n_batch)));
        }
        let wanted = batch.iter().filter(|b| b.want_logits).count();
        self.counters.decodes_without_outputs += usize::from(wanted == 0 && !batch.is_empty());
        if wanted > self.limits.n_outputs_max {
            return Err(LlmError::Backend(format!(
                "请求 {wanted} 行 logits，超出 n_outputs_max {}",
                self.limits.n_outputs_max
            )));
        }
        // 位置连续性检查（llama.cpp 要求每条序列的起始位置紧接缓存中的最后位置）
        let mut next_pos: Vec<usize> = self.seqs.iter().map(|s| s.len()).collect();
        for item in batch {
            if item.seq < 0 || item.seq as usize >= self.limits.n_seq_max {
                return Err(LlmError::Backend(format!("序列号 {} 越界", item.seq)));
            }
            let s = item.seq as usize;
            if item.pos < 0 || item.pos as usize != next_pos[s] {
                return Err(LlmError::Backend(format!(
                    "序列 {s} 位置不连续：缓存到 {}，本批给出 {}",
                    next_pos[s], item.pos
                )));
            }
            next_pos[s] += 1;
        }
        if self.cells_used() + batch.len() > self.limits.n_ctx {
            return Ok(DecodeOutcome::NoKvSlot);
        }
        // 先模拟「这一批算到一半时另一线程调用了 cancel」，再像 llama.cpp 那样问中止回调
        if self.cancel_on_decode == Some(call) {
            if let Some(flag) = &self.cancel {
                flag.cancel();
            }
        }
        let over_budget = self.abort_token_limit.is_some_and(|limit| self.counters.tokens_decoded + batch.len() > limit);
        if over_budget {
            // 「下一次击键」在这一批算到一半时到达：取消记在标记上
            if let Some(flag) = &self.cancel {
                flag.cancel();
            }
        }
        // 与 llama.cpp 一致：一批是否中止只看中止回调问的 `should_abort`（`llama.rs` 的 `abort_trampoline`）。
        // 不可中止区段里它是 false，这一批照常算完，由引擎在这一批之后检查取消。
        // 没有安装取消标记时没有地方记取消，超出时间的那一批直接中止
        let aborted = match &self.cancel {
            Some(flag) => flag.should_abort(),
            None => over_budget,
        };
        if self.abort_on_decode == Some(call) || aborted {
            // 与 llama.cpp 一致：被中止的批不会留在缓存里
            return Ok(DecodeOutcome::Aborted);
        }
        self.rows.clear();
        for item in batch {
            let cells = &mut self.seqs[item.seq as usize];
            let prev = cells.last().map_or(FNV_OFFSET, |c| c.history);
            let history = mix(prev, item.token);
            cells.push(Cell { token: item.token, id: self.next_cell_id, history });
            self.next_cell_id += 1;
            self.rows.push(item.want_logits.then_some(history));
        }
        self.counters.tokens_decoded += batch.len();
        Ok(DecodeOutcome::Ok)
    }

    fn logprobs(&mut self, batch_index: usize, targets: &[i32], out: &mut [f64]) -> Result<(), LlmError> {
        let Some(Some(history)) = self.rows.get(batch_index).copied() else {
            return Err(LlmError::Backend(format!("第 {batch_index} 行没有 logits")));
        };
        if out.len() < targets.len() {
            return Err(LlmError::Backend("输出缓冲不足".to_string()));
        }
        for (o, &t) in out.iter_mut().zip(targets) {
            *o = logprob_from(history, t);
        }
        Ok(())
    }

    fn install_cancel(&mut self, flag: Arc<CancelFlag>) {
        self.cancel = Some(flag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 序列 0 上位置 `pos` 的一个 token
    fn one_token(pos: i32) -> [BatchItem; 1] {
        [BatchItem { token: 1, pos, seq: 0, want_logits: true }]
    }

    /// 与 llama.cpp 的中止回调一样，一批是否中止只看 `should_abort`：已取消的请求在不可中止区段里照常算完，
    /// 离开区段后才中止。引擎的测试靠这一点检查不可中止区段（`llama.rs` 的中止回调问的也是它）
    #[test]
    fn decode_aborts_only_when_should_abort_says_so() {
        let mut backend = FakeBackend::with_default_limits();
        let flag = Arc::new(CancelFlag::new());
        backend.install_cancel(Arc::clone(&flag));

        // 请求被取消，但在不可中止区段里：这一批照常算完并进缓存
        flag.begin(flag.ticket());
        flag.cancel();
        flag.set_shielded(true);
        assert!(flag.is_cancelled() && !flag.should_abort());
        assert_eq!(backend.decode(&one_token(0)).unwrap(), DecodeOutcome::Ok);
        assert_eq!(backend.seq_tokens(0).len(), 1);
        // 离开区段：同一个取消让下一批中止，不进缓存
        flag.set_shielded(false);
        assert!(flag.should_abort());
        assert_eq!(backend.decode(&one_token(1)).unwrap(), DecodeOutcome::Aborted);
        assert_eq!(backend.seq_tokens(0).len(), 1);

        // 第 N 次 decode 时另一线程取消：同样只看 should_abort
        flag.begin(flag.ticket());
        backend.cancel_on_decode(1);
        flag.set_shielded(true);
        assert_eq!(backend.decode(&one_token(1)).unwrap(), DecodeOutcome::Ok);
        assert!(flag.is_cancelled());
        flag.set_shielded(false);
        assert_eq!(backend.decode(&one_token(2)).unwrap(), DecodeOutcome::Aborted);

        // 时间到（超出 token 预算）：取消记在标记上，是否中止仍然只看 should_abort
        flag.begin(flag.ticket());
        backend.abort_after_tokens(Some(0));
        flag.set_shielded(true);
        assert_eq!(backend.decode(&one_token(2)).unwrap(), DecodeOutcome::Ok);
        assert!(flag.is_cancelled(), "超出预算的那一批应当在取消标记上记一次取消");
        flag.set_shielded(false);
        assert_eq!(backend.decode(&one_token(3)).unwrap(), DecodeOutcome::Aborted);
        assert_eq!(backend.seq_tokens(0).len(), 3);

        // 新请求领取新票据后，超出预算的那一批同样先记下取消，在区段外整批中止
        flag.begin(flag.ticket());
        assert_eq!(backend.decode(&one_token(3)).unwrap(), DecodeOutcome::Aborted);
        assert_eq!(backend.seq_tokens(0).len(), 3);
        // 取消限制之后恢复正常
        backend.abort_after_tokens(None);
        flag.begin(flag.ticket());
        assert_eq!(backend.decode(&one_token(3)).unwrap(), DecodeOutcome::Ok);
        assert_eq!(backend.seq_tokens(0).len(), 4);
    }
}
