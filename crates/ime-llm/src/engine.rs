//! 与后端无关的打分引擎：把「上文 + 若干候选」变成 token 序列，规划共享前缀与分支的解码批次，
//! 并在多次调用之间复用 KV 缓存。
//!
//! 记号（与 `ime-ai` 的 `score_candidates_by_lm` 一致）：
//! - `common`：所有候选 token 序列的最长公共前缀长度
//! - `shared = max(1, common)`：每个候选只累加下标 `>= shared` 的 token 的对数概率
//!   （公共前缀部分对所有候选完全相同，比较时可以略去）
//!
//! 一次调用的解码过程：
//! 1. 序列 0 的 KV 缓存里保留着上一次调用的 token；与本次公共前缀求最长公共 token 前缀，只移除其后的部分
//! 2. 在序列 0 上补齐公共前缀（至少重算最后一个 token，以取得「公共前缀之后」的概率分布）。
//!    每 [`PREFIX_CHUNK_TOKENS`] 个 token 解码一次，每一块都不可中止（[`CancelFlag::set_shielded`]）：
//!    取消在块与块之间生效，算完的块留在缓存里。前缀只取决于上文，同一上文下的下一次击键要的是同一段前缀，
//!    所以正在算的那一块不扔掉；否则击键间隔短于一块的解码时间时，快速打字会让前缀永远算不完
//!    （手机上 2 个性能核大约就是这个量级）。代价是取消最多晚一块（≤16 个 token）才生效
//! 3. 每个候选占一个序列号，`kv_seq_cp` 共享前缀，所有候选的剩余 token 合成一批解码
//!    （受并行序列数、KV 单元数、单批 logits 行数限制时自动分组 / 分块）
//! 4. 清掉其它序列，序列 0 留给下一次调用复用
//!
//! 任何一次 `decode` 都不超过 [`MAX_DECODE_TOKENS`] 个 token（参考实现 `score_naive` 也一样）。
//!
//! 没有可打分 token 的候选（空候选，或者它的 token 序列是其余所有候选的公共前缀）：只要别的候选有可打分的
//! token，它的得分就是 `-inf`，见 [`ScoringEngine::score_tokens`]。

use crate::backend::{BackendLimits, BatchItem, CancelFlag, DecodeOutcome, LmBackend};
use crate::text;

/// 不可中止区段的守卫：创建时进入，析构时离开
struct Shield<'a>(&'a CancelFlag);

impl<'a> Shield<'a> {
    fn new(flag: &'a CancelFlag) -> Self {
        flag.set_shielded(true);
        Self(flag)
    }
}

impl Drop for Shield<'_> {
    fn drop(&mut self) {
        self.0.set_shielded(false);
    }
}
use crate::LlmError;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

/// 最近一次打分调用的统计信息（用于观测缓存复用效果与耗时构成）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScoreStats {
    /// 候选数
    pub candidates: usize,
    /// 所有候选的公共 token 前缀长度
    pub common_prefix_tokens: usize,
    /// 从上一次调用的 KV 缓存中直接复用的 token 数
    pub reused_tokens: usize,
    /// 本次为公共前缀新解码的 token 数
    pub prefix_tokens_decoded: usize,
    /// 本次为各候选分支解码的 token 数
    pub branch_tokens_decoded: usize,
    /// 后端 `decode` 调用次数
    pub decode_calls: usize,
    /// 分支解码的分组数（候选数或总长度超出后端容量时大于 1）
    pub groups: usize,
    /// 分词耗时（微秒）
    pub tokenize_us: u64,
    /// 公共前缀解码耗时（微秒）
    pub prefix_us: u64,
    /// 候选分支解码耗时（微秒）
    pub branch_us: u64,
    /// 整次调用耗时（微秒）
    pub total_us: u64,
}

/// 公共前缀每次 `decode` 的 token 数上限。
///
/// 每一块都在不可中止区段里解码（见模块文档第 2 步）：取消要等正在算的那一块算完才生效，这一块和之前算完的块
/// 都留在缓存里，下一个请求从那里接着算，算过的不会白算。所以块的大小决定的是前缀阶段的取消最多晚多久生效：
/// 16 个 token 在这台 Mac 上 2 线程约 60 ~ 75 ms。块越大 `decode` 次数越少，但每次击键的取消都可能晚得更多；
/// 整段前缀一次解码的话，取消要等整段算完（25 个新 token 在 2 线程下要 110 ~ 125 ms）。块越小取消越及时，
/// 但 `decode` 次数和每块多算的那一行 logits（见 `run_plan` 里的说明）也越多。还不能超过 [`MAX_DECODE_TOKENS`]。
///
/// 击键比一块的解码时间还快时，被取消的请求也会把正在算的那一块算完，前缀照样一块一块推进
/// （测试 `fast_typing_cannot_starve_the_scorer`、`keystrokes_faster_than_one_prefix_chunk_still_make_progress`）。
/// 实测数据与分块对耗时的影响见 `lib.rs` 的「取消与分块」一节。
pub(crate) const PREFIX_CHUNK_TOKENS: usize = 16;

/// 任何一次 `decode` 的 token 数上限。
///
/// ggml 的注意力算子在一次 `decode` 的 token 数达到 64 时换用另一套（分块）内核，算出的 logits 与 63 个以内时
/// 不同（实测 Qwen3-0.6B：63 个以内与逐 token 解码逐位相同，64 个时同一行 logits 最大相差 0.77 nat）。
/// 把每次 `decode` 都压在 63 个以内，得分才不取决于这段文本是一次算完的还是接着缓存算的。
pub(crate) const MAX_DECODE_TOKENS: usize = 63;

/// 两个 token 序列的最长公共前缀长度
pub(crate) fn common_prefix_len(a: &[i32], b: &[i32]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// 一组 token 序列的最长公共前缀长度（空集合为 0，单个序列为其自身长度）
pub(crate) fn common_prefix_len_all(seqs: &[Vec<i32>]) -> usize {
    let Some(first) = seqs.first() else { return 0 };
    seqs[1..]
        .iter()
        .fold(first.len(), |acc, s| acc.min(common_prefix_len(first, s)))
}

/// 把一批输入切成若干次 `decode`：每块不超过 `max_items` 个 token、不超过 `max_outputs` 个需要 logits 的 token
pub(crate) fn split_chunks(items: &[BatchItem], max_items: usize, max_outputs: usize) -> Vec<Range<usize>> {
    let max_items = max_items.max(1);
    let max_outputs = max_outputs.max(1);
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut outputs = 0usize;
    for (i, item) in items.iter().enumerate() {
        let want = item.want_logits as usize;
        if i > start && (i - start >= max_items || outputs + want > max_outputs) {
            chunks.push(start..i);
            start = i;
            outputs = 0;
        }
        outputs += want;
    }
    if start < items.len() {
        chunks.push(start..items.len());
    }
    chunks
}

/// 一次打分调用的解码规划（纯计算，不触碰后端）
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    pub common: usize,
    pub shared: usize,
    /// 候选下标 → 去重后的序号；`None` 表示该候选没有可打分的 token
    /// （`uniques` 非空时得分为 `-inf`，否则所有候选都是 0）
    pub cand_to_unique: Vec<Option<usize>>,
    /// 去重后的序号 → 代表它的候选下标
    pub uniques: Vec<usize>,
    /// 需要独立分支的去重序号分组；第 0 组最后执行，其首个成员留在序列 0 供下次调用复用
    pub groups: Vec<Vec<usize>>,
}

impl Plan {
    pub fn new(seqs: &[Vec<i32>], limits: &BackendLimits) -> Result<Self, LlmError> {
        if seqs.iter().any(|s| s.is_empty()) {
            return Err(LlmError::Tokenize("分词结果为空序列".to_string()));
        }
        let common = common_prefix_len_all(seqs);
        let shared = common.max(1);

        let mut index: HashMap<&[i32], usize> = HashMap::new();
        let mut uniques = Vec::new();
        let mut cand_to_unique = Vec::with_capacity(seqs.len());
        for (i, s) in seqs.iter().enumerate() {
            if s.len() <= shared {
                cand_to_unique.push(None);
                continue;
            }
            let u = *index.entry(s.as_slice()).or_insert_with(|| {
                uniques.push(i);
                uniques.len() - 1
            });
            cand_to_unique.push(Some(u));
        }

        // 分支长度 = 需要在该候选自己的序列上解码的 token 数（最后一个 token 只被预测，不需要解码）
        let budget = limits.n_ctx.saturating_sub(common);
        let max_members = limits.n_seq_max.max(1);
        let mut groups: Vec<Vec<usize>> = Vec::new();
        let mut current: Vec<usize> = Vec::new();
        let mut current_cost = 0usize;
        for (u, &cand) in uniques.iter().enumerate() {
            let branch = seqs[cand].len() - 1 - common;
            if branch == 0 {
                continue; // 只差最后一个 token：直接用公共前缀那一行的分布
            }
            if branch > budget {
                return Err(LlmError::ContextOverflow {
                    needed: seqs[cand].len() - 1,
                    capacity: limits.n_ctx,
                });
            }
            if !current.is_empty() && (current.len() >= max_members || current_cost + branch > budget) {
                groups.push(std::mem::take(&mut current));
                current_cost = 0;
            }
            current.push(u);
            current_cost += branch;
        }
        if !current.is_empty() {
            groups.push(current);
        }
        if common > limits.n_ctx {
            return Err(LlmError::ContextOverflow { needed: common, capacity: limits.n_ctx });
        }
        Ok(Self { common, shared, cand_to_unique, uniques, groups })
    }
}

/// 引擎内部的失败分类：KV 放不下时可以清空缓存后重试一次
enum RunError {
    NoSlot,
    Fail(LlmError),
}

impl From<LlmError> for RunError {
    fn from(e: LlmError) -> Self {
        RunError::Fail(e)
    }
}

pub(crate) struct ScoringEngine<B: LmBackend> {
    backend: B,
    limits: BackendLimits,
    /// 序列 0 的 KV 缓存中确定有效的 token（位置 `0..cached.len()`）
    cached: Vec<i32>,
    /// KV 状态未知，下次使用前必须整体清空
    dirty: bool,
    cancel: Arc<CancelFlag>,
    stats: ScoreStats,
}

impl<B: LmBackend> ScoringEngine<B> {
    pub fn new(mut backend: B, cancel: Arc<CancelFlag>) -> Self {
        backend.install_cancel(Arc::clone(&cancel));
        let limits = backend.limits();
        Self { backend, limits, cached: Vec::new(), dirty: true, cancel, stats: ScoreStats::default() }
    }

    pub fn limits(&self) -> BackendLimits {
        self.limits
    }

    pub fn stats(&self) -> ScoreStats {
        self.stats
    }

    #[cfg(test)]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    #[cfg(test)]
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    #[cfg(test)]
    pub fn cached_tokens(&self) -> &[i32] {
        &self.cached
    }

    /// 丢弃跨调用的 KV 缓存（下次调用从头解码）
    pub fn invalidate(&mut self) {
        self.cached.clear();
        self.dirty = true;
    }

    /// 按 `text` 模块的规则构造每个候选的打分文本并整体分词。
    /// 前缀用进程内版本 [`text::lm_scoring_prefix_inprocess`]：长上文的窗口起点对齐到分句处
    /// （用户连续上屏时，新前缀以上一次的前缀开头，KV 缓存才接得上），英文上文末尾的空格保留一个。
    pub fn tokenize_candidates<S: AsRef<str>>(
        &mut self,
        context: &str,
        candidates: &[S],
    ) -> Result<Vec<Vec<i32>>, LlmError> {
        let prefix = text::lm_scoring_prefix_inprocess(context);
        let mut seqs = Vec::with_capacity(candidates.len());
        for c in candidates {
            let tokens = self.backend.tokenize(&text::lm_scoring_text(&prefix, c.as_ref()))?;
            if tokens.is_empty() {
                return Err(LlmError::Tokenize("分词结果为空序列".to_string()));
            }
            seqs.push(tokens);
        }
        Ok(seqs)
    }

    /// 批量打分：共享前缀只解码一次，并复用上一次调用留下的 KV 缓存
    pub fn score<S: AsRef<str>>(&mut self, context: &str, candidates: &[S]) -> Result<Vec<f64>, LlmError> {
        let started = Instant::now();
        self.stats = ScoreStats { candidates: candidates.len(), ..ScoreStats::default() };
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        self.check_cancel()?;
        let seqs = self.tokenize_candidates(context, candidates)?;
        self.stats.tokenize_us = started.elapsed().as_micros() as u64;
        let result = self.score_tokens(&seqs);
        self.stats.total_us = started.elapsed().as_micros() as u64;
        result
    }

    /// 参考实现：每个候选都清空 KV、从头单独解码。结果应与 [`Self::score`] 在数值误差内一致，仅用于校验与对比
    /// （实测的误差范围见 `lib.rs` 的「数值一致性」一节）。
    pub fn score_naive<S: AsRef<str>>(&mut self, context: &str, candidates: &[S]) -> Result<Vec<f64>, LlmError> {
        let started = Instant::now();
        self.stats = ScoreStats { candidates: candidates.len(), ..ScoreStats::default() };
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        self.check_cancel()?;
        let seqs = self.tokenize_candidates(context, candidates)?;
        self.stats.tokenize_us = started.elapsed().as_micros() as u64;
        let result = self.score_tokens_naive(&seqs);
        self.stats.total_us = started.elapsed().as_micros() as u64;
        result
    }

    /// 对已分词的候选打分。
    ///
    /// 没有可打分 token 的候选（长度不超过 `shared`：空候选，或者它的 token 序列是其余所有候选的公共前缀）
    /// 得 `-inf`，前提是至少有一个候选有可打分的 token。这样的候选没有任何 token 参与比较，若记 0 分，
    /// 它会高于所有其它候选（对数概率之和恒不大于 0）而必然胜出，与上文无关；`-inf` 表示「不能选它」。
    /// 只有一个候选、或所有候选完全相同时没有可比较的 token，全部返回 0。
    pub fn score_tokens(&mut self, seqs: &[Vec<i32>]) -> Result<Vec<f64>, LlmError> {
        let plan = Plan::new(seqs, &self.limits)?;
        self.stats.common_prefix_tokens = plan.common;
        if plan.uniques.is_empty() {
            // 只有一个候选，或所有候选完全相同：没有可比较的 token
            return Ok(vec![0.0; seqs.len()]);
        }
        let mut retried = false;
        let unique_scores = loop {
            let had_state = !self.dirty && !self.cached.is_empty();
            match self.run_plan(seqs, &plan) {
                Ok(scores) => break scores,
                Err(RunError::NoSlot) if had_state && !retried => {
                    // 残留的缓存占着位置：清空后从头再来一次
                    retried = true;
                    self.invalidate();
                }
                Err(RunError::NoSlot) => {
                    self.invalidate();
                    let needed = seqs.iter().map(|s| s.len() - 1).max().unwrap_or(0);
                    return Err(LlmError::ContextOverflow { needed, capacity: self.limits.n_ctx });
                }
                Err(RunError::Fail(e)) => return Err(e),
            }
        };
        Ok(plan
            .cand_to_unique
            .iter()
            .map(|u| u.map_or(f64::NEG_INFINITY, |u| unique_scores[u]))
            .collect())
    }

    pub fn score_tokens_naive(&mut self, seqs: &[Vec<i32>]) -> Result<Vec<f64>, LlmError> {
        if seqs.iter().any(|s| s.is_empty()) {
            return Err(LlmError::Tokenize("分词结果为空序列".to_string()));
        }
        let common = common_prefix_len_all(seqs);
        let shared = common.max(1);
        self.stats.common_prefix_tokens = common;
        if seqs.iter().all(|s| s.len() <= shared) {
            // 只有一个候选，或所有候选完全相同：没有可比较的 token
            return Ok(vec![0.0; seqs.len()]);
        }
        self.invalidate();
        let t_branch = Instant::now();
        let chunk_items = self.limits.n_batch.min(MAX_DECODE_TOKENS);
        let mut scores = Vec::with_capacity(seqs.len());
        let mut out = [0.0f64; 1];
        for seq in seqs {
            if seq.len() <= shared {
                // 没有可打分的 token，而别的候选有：不能选它（与 `score_tokens` 相同）
                scores.push(f64::NEG_INFINITY);
                continue;
            }
            if seq.len() - 1 > self.limits.n_ctx {
                return Err(LlmError::ContextOverflow { needed: seq.len() - 1, capacity: self.limits.n_ctx });
            }
            self.backend.kv_clear();
            let items: Vec<BatchItem> = (0..seq.len() - 1)
                .map(|i| BatchItem { token: seq[i], pos: i as i32, seq: 0, want_logits: i + 1 >= shared })
                .collect();
            let mut total = 0.0f64;
            for range in split_chunks(&items, chunk_items, self.limits.n_outputs_max) {
                self.check_cancel()?;
                let chunk = &items[range.clone()];
                match self.decode(chunk)? {
                    DecodeOutcome::Ok => {}
                    DecodeOutcome::Aborted => return Err(LlmError::Cancelled),
                    DecodeOutcome::NoKvSlot => {
                        return Err(LlmError::ContextOverflow { needed: seq.len() - 1, capacity: self.limits.n_ctx })
                    }
                }
                self.stats.branch_tokens_decoded += chunk.len();
                for (row, item) in chunk.iter().enumerate() {
                    if item.want_logits {
                        let target = seq[item.pos as usize + 1];
                        self.logprobs(row, &[target], &mut out)?;
                        total += out[0];
                    }
                }
            }
            scores.push(total);
        }
        self.stats.branch_us = t_branch.elapsed().as_micros() as u64;
        self.stats.groups = seqs.len();
        Ok(scores)
    }

    fn check_cancel(&self) -> Result<(), LlmError> {
        if self.cancel.is_cancelled() {
            Err(LlmError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// 后端报错意味着 KV 状态未知：标记为脏后再把错误交给调用方
    fn decode(&mut self, batch: &[BatchItem]) -> Result<DecodeOutcome, LlmError> {
        self.stats.decode_calls += 1;
        let result = self.backend.decode(batch);
        if result.is_err() {
            self.invalidate();
        }
        result
    }

    fn logprobs(&mut self, row: usize, targets: &[i32], out: &mut [f64]) -> Result<(), LlmError> {
        let result = self.backend.logprobs(row, targets, out);
        if result.is_err() {
            self.invalidate();
        }
        result
    }

    /// 中止 / 放不下之后的收尾：移除本次调用占用的其它序列，把序列 0 截回到确定有效的 `cached`
    fn recover(&mut self, seqs_in_use: usize) {
        for s in 1..seqs_in_use {
            self.backend.kv_seq_rm(s as i32, -1, -1);
        }
        if !self.backend.kv_seq_rm(0, self.cached.len() as i32, -1) {
            self.invalidate();
        }
    }

    fn run_plan(&mut self, seqs: &[Vec<i32>], plan: &Plan) -> Result<Vec<f64>, RunError> {
        let common = plan.common;
        let prefix = &seqs[plan.uniques[0]][..common];
        let mut scores = vec![0.0f64; plan.uniques.len()];

        // ---- 1. 与上一次调用留下的缓存对齐
        if self.dirty {
            self.backend.kv_clear();
            self.cached.clear();
            self.dirty = false;
        }
        let mut keep = common_prefix_len(&self.cached, prefix);
        if common > 0 {
            // 公共前缀的最后一个 token 必须重新解码，才能拿到它之后的概率分布
            keep = keep.min(common - 1);
        }
        if keep < self.cached.len() {
            if !self.backend.kv_seq_rm(0, keep as i32, -1) {
                self.backend.kv_clear();
                keep = 0;
            }
            self.cached.truncate(keep);
        }
        self.stats.reused_tokens = keep;

        // ---- 2. 补齐公共前缀，读取「公共前缀之后」那一行
        if common > 0 {
            let t_prefix = Instant::now();
            let mut items: Vec<BatchItem> = (keep..common)
                .map(|i| BatchItem { token: prefix[i], pos: i as i32, seq: 0, want_logits: i + 1 == common })
                .collect();
            // 分块解码：每一块都在不可中止区段里算完才记入 `cached`，取消在块与块之间（循环开头的检查）生效，
            // 算完的块都留着。放不下（`NoKvSlot`）或后端返回 `Aborted`（只会是测试注入的 `abort_on_decode`，
            // 真后端在区段里不中止）时，后端已经撤掉了这一块写入的位置（`ime_llm_shim.h` 的约定；假后端在写入之前
            // 就返回），所以这几处 `recover` 都没有要截掉的位置，只是兜底把序列 0 截回 `cached`
            let chunks = split_chunks(&items, self.limits.n_batch.min(PREFIX_CHUNK_TOKENS), self.limits.n_outputs_max);
            // 每一块的最后一个 token 都请求 logits，尽管只有最后一块的那一行会被读取。
            // llama.cpp 为「没有任何输出行」的一批构建的计算图布局不同，会多写脏约 1 MiB 的计算缓冲
            // （tools/eval 默认用例集的峰值 phys_footprint 实测：Qwen2.5-0.5B 70.5 → 71.4 MiB，
            // Qwen3-0.6B 94.9 → 96.0 MiB）；每块多算一行 logits 之后峰值回到不分块时的水平（70.3 / 94.8 MiB），
            // 冷缓存耗时比不请求时多 1% ~ 2%，与不分块时持平。
            for range in &chunks {
                items[range.end - 1].want_logits = true;
            }
            let mut last_len = 0usize;
            let cancel = Arc::clone(&self.cancel);
            for range in chunks {
                if cancel.is_cancelled() {
                    self.recover(1);
                    return Err(LlmError::Cancelled.into());
                }
                let chunk = &items[range.clone()];
                // 这一块算完才响应取消（见模块文档第 2 步）；离开作用域时（包括出错返回）解除
                let outcome = {
                    let _shield = Shield::new(&cancel);
                    self.decode(chunk)?
                };
                match outcome {
                    DecodeOutcome::Ok => {}
                    DecodeOutcome::Aborted => {
                        self.recover(1);
                        return Err(LlmError::Cancelled.into());
                    }
                    DecodeOutcome::NoKvSlot => {
                        self.recover(1);
                        return Err(RunError::NoSlot);
                    }
                }
                self.cached.extend(chunk.iter().map(|it| it.token));
                self.stats.prefix_tokens_decoded += chunk.len();
                last_len = chunk.len();
            }
            let targets: Vec<i32> = plan.uniques.iter().map(|&c| seqs[c][common]).collect();
            let mut out = vec![0.0f64; targets.len()];
            self.logprobs(last_len - 1, &targets, &mut out)?;
            for (score, lp) in scores.iter_mut().zip(&out) {
                *score += lp;
            }
            self.stats.prefix_us = t_prefix.elapsed().as_micros() as u64;
        }

        // ---- 3. 各候选分支：每个候选一个序列号，同组合批解码
        let t_branch = Instant::now();
        self.stats.groups = plan.groups.len();
        let chunk_items = self.limits.n_batch.min(MAX_DECODE_TOKENS);
        let chunk_outputs = self.limits.n_outputs_max;
        let mut out = [0.0f64; 1];
        for (gi, group) in plan.groups.iter().enumerate().rev() {
            let is_last = gi == 0;
            if common > 0 {
                for s in 1..group.len() {
                    self.backend.kv_seq_cp(0, s as i32, -1, -1);
                }
            }
            // 按序列号排列：序列 0 的分支紧跟在公共前缀之后
            let mut items: Vec<BatchItem> = Vec::new();
            let mut owners: Vec<(usize, i32)> = Vec::new(); // (去重序号, 该行要读取的目标 token)
            for (s, &u) in group.iter().enumerate() {
                let seq = &seqs[plan.uniques[u]];
                for i in common..seq.len() - 1 {
                    items.push(BatchItem { token: seq[i], pos: i as i32, seq: s as i32, want_logits: true });
                    owners.push((u, seq[i + 1]));
                }
            }
            for range in split_chunks(&items, chunk_items, chunk_outputs) {
                if self.cancel.is_cancelled() {
                    self.recover(group.len());
                    return Err(LlmError::Cancelled.into());
                }
                let chunk = &items[range.clone()];
                match self.decode(chunk)? {
                    DecodeOutcome::Ok => {}
                    DecodeOutcome::Aborted => {
                        self.recover(group.len());
                        return Err(LlmError::Cancelled.into());
                    }
                    DecodeOutcome::NoKvSlot => {
                        self.recover(group.len());
                        return Err(RunError::NoSlot);
                    }
                }
                self.stats.branch_tokens_decoded += chunk.len();
                for (row, &(u, target)) in owners[range].iter().enumerate() {
                    self.logprobs(row, &[target], &mut out)?;
                    scores[u] += out[0];
                }
            }
            // ---- 4. 收尾：只留序列 0
            for s in 1..group.len() {
                self.backend.kv_seq_rm(s as i32, -1, -1);
            }
            if is_last {
                let seq = &seqs[plan.uniques[group[0]]];
                self.cached.extend_from_slice(&seq[common..seq.len() - 1]);
            } else if !self.backend.kv_seq_rm(0, common as i32, -1) {
                self.invalidate();
                return Err(LlmError::Backend("KV 缓存不支持部分移除".to_string()).into());
            }
        }
        self.stats.branch_us = t_branch.elapsed().as_micros() as u64;
        Ok(scores)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeBackend;
    use crate::text::LM_SCORE_PROMPT_PREFIX;

    fn limits(n_ctx: usize, n_seq_max: usize, n_outputs_max: usize) -> BackendLimits {
        BackendLimits { n_ctx, n_batch: n_ctx, n_seq_max, n_outputs_max }
    }

    fn engine_with(backend: FakeBackend) -> ScoringEngine<FakeBackend> {
        ScoringEngine::new(backend, Arc::new(CancelFlag::new()))
    }

    fn default_engine() -> ScoringEngine<FakeBackend> {
        engine_with(FakeBackend::with_default_limits())
    }

    /// 不经过引擎、不经过任何缓存的期望得分
    fn reference(engine: &ScoringEngine<FakeBackend>, context: &str, candidates: &[&str]) -> Vec<f64> {
        let prefix = text::lm_scoring_prefix_inprocess(context);
        let seqs: Vec<Vec<i32>> = candidates
            .iter()
            .map(|c| engine.backend().tokenize_text(&text::lm_scoring_text(&prefix, c)))
            .collect();
        let shared = common_prefix_len_all(&seqs).max(1);
        // 没有可打分 token 的候选：别的候选有可打分的 token 时是 -inf，否则大家都是 0
        let empty = if seqs.iter().any(|s| s.len() > shared) { f64::NEG_INFINITY } else { 0.0 };
        seqs.iter()
            .map(|s| if s.len() <= shared { empty } else { FakeBackend::reference_score(s, shared) })
            .collect()
    }

    fn assert_scores_eq(actual: &[f64], expected: &[f64]) {
        assert_eq!(actual.len(), expected.len());
        for (i, (a, e)) in actual.iter().zip(expected).enumerate() {
            // `a == e` 覆盖两边都是 -inf 的情况（相减得 NaN）
            assert!(a == e || (a - e).abs() < 1e-9, "候选 {i}: 实际 {a}，期望 {e}\n实际 {actual:?}\n期望 {expected:?}");
        }
    }

    /// 调用结束后只应剩下序列 0，且内容与引擎记录的缓存一致
    fn assert_clean_state(engine: &ScoringEngine<FakeBackend>) {
        let backend = engine.backend();
        assert_eq!(backend.seq_tokens(0), engine.cached_tokens());
        for s in 1..backend.limits().n_seq_max {
            assert!(backend.seq_tokens(s).is_empty(), "序列 {s} 未清理");
        }
        assert_eq!(backend.cells_used(), engine.cached_tokens().len());
    }

    const PROMPT_TOKENS: usize = 19; // 假分词器按字符切分，引导语共 19 个字符

    #[test]
    fn prompt_prefix_length_assumption_holds() {
        assert_eq!(LM_SCORE_PROMPT_PREFIX.chars().count(), PROMPT_TOKENS);
    }

    #[test]
    fn common_prefix_helpers() {
        assert_eq!(common_prefix_len(&[1, 2, 3], &[1, 2, 4]), 2);
        assert_eq!(common_prefix_len(&[1, 2], &[1, 2, 4]), 2);
        assert_eq!(common_prefix_len(&[], &[1]), 0);
        assert_eq!(common_prefix_len_all(&[]), 0);
        assert_eq!(common_prefix_len_all(&[vec![7, 8, 9]]), 3);
        assert_eq!(common_prefix_len_all(&[vec![7, 8, 9], vec![7, 8], vec![7, 8, 1]]), 2);
        assert_eq!(common_prefix_len_all(&[vec![1], vec![2]]), 0);
    }

    #[test]
    fn split_chunks_respects_both_caps() {
        let item = |want| BatchItem { token: 1, pos: 0, seq: 0, want_logits: want };
        // 只有最后一个需要 logits：只受 token 数限制
        let mut items = vec![item(false); 9];
        items.push(item(true));
        assert_eq!(split_chunks(&items, 4, 1), vec![0..4, 4..8, 8..10]);
        // 全部需要 logits：受行数限制
        let items = vec![item(true); 7];
        assert_eq!(split_chunks(&items, 100, 3), vec![0..3, 3..6, 6..7]);
        assert_eq!(split_chunks(&items, 2, 3), vec![0..2, 2..4, 4..6, 6..7]);
        assert!(split_chunks(&[], 4, 4).is_empty());
        // 上限为 0 时按 1 处理，不会死循环
        assert_eq!(split_chunks(&items[..2], 0, 0), vec![0..1, 1..2]);
    }

    #[test]
    fn plan_dedups_and_skips_candidates_without_scored_tokens() {
        let seqs = vec![
            vec![1, 2, 3, 4],    // 分支长度 1
            vec![1, 2, 5],       // 只差最后一个 token：不需要分支
            vec![1, 2, 3, 4],    // 与第 0 个重复
            vec![1, 2],          // 等于公共前缀：没有可打分的 token
            vec![1, 2, 6, 7, 8], // 分支长度 2
        ];
        let plan = Plan::new(&seqs, &limits(64, 8, 8)).unwrap();
        assert_eq!(plan.common, 2);
        assert_eq!(plan.shared, 2);
        assert_eq!(plan.uniques, vec![0, 1, 4]);
        assert_eq!(plan.cand_to_unique, vec![Some(0), Some(1), Some(0), None, Some(2)]);
        assert_eq!(plan.groups, vec![vec![0, 2]]);
    }

    #[test]
    fn plan_shared_is_at_least_one() {
        // 首个 token 就不同：仍然只从下标 1 开始计分（与 HTTP 路径一致）
        let seqs = vec![vec![1, 2, 3], vec![9, 2, 3], vec![5]];
        let plan = Plan::new(&seqs, &limits(64, 8, 8)).unwrap();
        assert_eq!((plan.common, plan.shared), (0, 1));
        assert_eq!(plan.cand_to_unique, vec![Some(0), Some(1), None]);
        assert_eq!(plan.groups, vec![vec![0, 1]]);
    }

    #[test]
    fn plan_groups_respect_sequence_and_cell_limits() {
        let seqs: Vec<Vec<i32>> = (0..5).map(|k| vec![1, 2, 10 + k, 20 + k, 30 + k]).collect(); // 分支长度均为 2
        let plan = Plan::new(&seqs, &limits(64, 2, 8)).unwrap();
        assert_eq!(plan.groups, vec![vec![0, 1], vec![2, 3], vec![4]]);
        // 公共前缀占 2 个单元，剩余 5 个：每组最多放两条长度 2 的分支
        let plan = Plan::new(&seqs, &limits(7, 8, 8)).unwrap();
        assert_eq!(plan.groups, vec![vec![0, 1], vec![2, 3], vec![4]]);
        // 单条分支都放不下
        let err = Plan::new(&seqs, &limits(3, 8, 8)).unwrap_err();
        assert_eq!(err, LlmError::ContextOverflow { needed: 4, capacity: 3 });
        // 空序列
        assert!(matches!(Plan::new(&[vec![1], vec![]], &limits(8, 8, 8)), Err(LlmError::Tokenize(_))));
    }

    #[test]
    fn batched_scores_match_reference() {
        let mut engine = default_engine();
        let cases: [(&str, &[&str]); 5] = [
            ("周末天气不错，我们", &["去公园完", "去公园万", "去公园晚", "去公园玩"]),
            ("他在婚礼上许下了", &["实验", "试验", "式样", "饰演", "食盐", "誓言", "试样", "失言"]),
            ("", &["你好", "拟好", "尼好"]),
            ("mixed 上文 ", &["AI 选词", "AI 炫词", "ai xuanci"]),
            ("这篇论文的", &["实验结果", "试验结果"]),
        ];
        for (context, candidates) in cases {
            let expected = reference(&engine, context, candidates);
            let actual = engine.score(context, candidates).unwrap();
            assert_scores_eq(&actual, &expected);
            assert_clean_state(&engine);
            // 参考实现走的是完全不同的解码路径，结果也必须一致
            let naive = engine.score_naive(context, candidates).unwrap();
            assert_scores_eq(&naive, &expected);
        }
    }

    #[test]
    fn degenerate_inputs() {
        let mut engine = default_engine();
        let none: [&str; 0] = [];
        assert!(engine.score("上文", &none).unwrap().is_empty());
        // 单个候选：没有可比较的 token，不触发任何解码
        assert_eq!(engine.score("上文", &["候选"]).unwrap(), vec![0.0]);
        assert_eq!(engine.backend().counters.decode_calls, 0);
        // 完全相同的候选
        assert_eq!(engine.score("上文", &["候选", "候选"]).unwrap(), vec![0.0, 0.0]);
        assert_eq!(engine.backend().counters.decode_calls, 0);
    }

    /// 没有可打分 token 的候选不能得 0 分：0 高于任何对数概率之和，它会不看上文就胜出
    #[test]
    fn candidate_without_scored_tokens_gets_negative_infinity() {
        let mut engine = default_engine();
        // 一个候选是其余所有候选的前缀
        let cands = ["去公园", "去公园玩", "去公园玩", "去公园完"];
        let expected = reference(&engine, "我们", &cands);
        assert_eq!(expected[0], f64::NEG_INFINITY);
        let actual = engine.score("我们", &cands).unwrap();
        assert_scores_eq(&actual, &expected);
        assert_eq!(actual[0], f64::NEG_INFINITY);
        assert!(actual[1..].iter().all(|s| s.is_finite() && *s < 0.0), "{actual:?}");
        assert_eq!(actual[1], actual[2]);
        assert_clean_state(&engine);
        // 参考实现给出同样的结果
        assert_scores_eq(&engine.score_naive("我们", &cands).unwrap(), &expected);

        // 空候选（分词后只剩前缀）
        let cands = ["", "去公园玩", "去公元万"];
        let actual = engine.score("周末天气不错，我们", &cands).unwrap();
        assert_eq!(actual[0], f64::NEG_INFINITY);
        assert!(actual[1].is_finite() && actual[2].is_finite(), "{actual:?}");
        assert_scores_eq(&actual, &reference(&engine, "周末天气不错，我们", &cands));
        assert_scores_eq(&engine.score_naive("周末天气不错，我们", &cands).unwrap(), &actual);
        // 最高分永远不是它
        let best = actual.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(i, _)| i).unwrap();
        assert_ne!(best, 0);

        // 只是其中一部分候选的前缀：它自己有可打分的 token，照常计分
        let cands = ["去公园", "去公园玩", "去工元"];
        let actual = engine.score("我们", &cands).unwrap();
        assert!(actual.iter().all(|s| s.is_finite()), "{actual:?}");
        assert_scores_eq(&actual, &reference(&engine, "我们", &cands));

        // 没有任何候选有可打分的 token（单个候选、全部相同、全部为空）：仍然全是 0，参考实现也一样
        assert_eq!(engine.score("上文", &["候选"]).unwrap(), vec![0.0]);
        assert_eq!(engine.score("上文", &["", ""]).unwrap(), vec![0.0, 0.0]);
        assert_eq!(engine.score_naive("上文", &["候选"]).unwrap(), vec![0.0]);
        assert_eq!(engine.score_naive("上文", &["候选", "候选"]).unwrap(), vec![0.0, 0.0]);

        // 首个 token 就不同时 shared 取 1：只有一个 token 的候选同样没有可打分的 token
        let scores = engine.score_tokens(&[vec![1, 2, 3], vec![9, 2, 3], vec![5]]).unwrap();
        assert!(scores[0].is_finite() && scores[1].is_finite());
        assert_eq!(scores[2], f64::NEG_INFINITY);
        assert_eq!(engine.score_tokens_naive(&[vec![1, 2, 3], vec![9, 2, 3], vec![5]]).unwrap()[2], f64::NEG_INFINITY);
    }

    #[test]
    fn prefix_is_reused_across_calls() {
        let mut engine = default_engine();
        let context = "周末天气不错，我们";
        let common = PROMPT_TOKENS + context.chars().count() + 2; // 候选都以「去公」开头

        // 第一次：冷缓存，公共前缀全部解码
        let first = ["去公园完", "去公园玩", "去公元万"];
        assert_scores_eq(&engine.score(context, &first).unwrap(), &reference(&engine, context, &first));
        let stats = engine.stats();
        assert_eq!(stats.common_prefix_tokens, common);
        assert_eq!(stats.reused_tokens, 0);
        assert_eq!(stats.prefix_tokens_decoded, common);
        // 每个候选在「去公」之后还有两个 token，其中最后一个只被预测、不需要解码
        assert_eq!(stats.branch_tokens_decoded, 1 + 1 + 1);
        assert_eq!(stats.groups, 1);
        assert_eq!(engine.backend().counters.kv_clears, 1);
        assert_clean_state(&engine);

        // 第二次：同一上文、候选不同（下一次击键）。只重算公共前缀的最后一个 token
        let second = ["去公园玩吧", "去公园晚八", "去公元万把"];
        assert_scores_eq(&engine.score(context, &second).unwrap(), &reference(&engine, context, &second));
        let stats = engine.stats();
        assert_eq!(stats.reused_tokens, common - 1);
        assert_eq!(stats.prefix_tokens_decoded, 1);
        assert_clean_state(&engine);

        // 第三次：首选「去公园玩吧」上屏后上文变长。它的分支还留在序列 0，新的公共前缀能复用其中一部分
        let committed = format!("{context}去公园玩吧");
        let third = ["然后回家", "燃后回家", "然后会家"];
        assert_scores_eq(&engine.score(&committed, &third).unwrap(), &reference(&engine, &committed, &third));
        let stats = engine.stats();
        // 序列 0 中缓存了「…去公园玩」（最后一个 token「吧」只被预测、未解码）
        assert_eq!(stats.reused_tokens, PROMPT_TOKENS + context.chars().count() + 4);
        assert_eq!(stats.prefix_tokens_decoded, stats.common_prefix_tokens - stats.reused_tokens);
        assert_clean_state(&engine);

        // 第四次：换了一个完全不同的上文，只剩引导语可复用
        let other = "他在婚礼上许下了";
        let fourth = ["誓言", "食盐", "试验"];
        assert_scores_eq(&engine.score(other, &fourth).unwrap(), &reference(&engine, other, &fourth));
        assert_eq!(engine.stats().reused_tokens, PROMPT_TOKENS);
        assert_clean_state(&engine);

        // 全程没有再清空过缓存
        assert_eq!(engine.backend().counters.kv_clears, 1);

        // 显式丢弃缓存后从头解码
        engine.invalidate();
        assert_scores_eq(&engine.score(other, &fourth).unwrap(), &reference(&engine, other, &fourth));
        assert_eq!(engine.stats().reused_tokens, 0);
        assert_eq!(engine.backend().counters.kv_clears, 2);
    }

    #[test]
    fn fully_cached_prefix_still_redecodes_its_last_token() {
        let mut engine = default_engine();
        let context = "今天";
        let cands = ["天气好", "天汽好"];
        engine.score(context, &cands).unwrap();
        // 第二次的公共前缀比第一次短，且完全落在缓存之内
        let cands2 = ["天气", "田七", "添砌"];
        let expected = reference(&engine, context, &cands2);
        assert_scores_eq(&engine.score(context, &cands2).unwrap(), &expected);
        let stats = engine.stats();
        assert_eq!(stats.common_prefix_tokens, PROMPT_TOKENS + 2);
        assert_eq!(stats.reused_tokens, PROMPT_TOKENS + 1);
        assert_eq!(stats.prefix_tokens_decoded, 1);
        assert_clean_state(&engine);
    }

    #[test]
    fn small_limits_force_grouping_and_chunking() {
        // 3 条并行序列、单批最多 3 行 logits、KV 单元紧张
        let context = "上文";
        let candidates = [
            "一二三四五", "一二三四六", "一二七", "一八九十", "一二三四五", "一", "一甲乙丙丁戊", "一二", "一子丑寅", "一二三卯",
        ];
        let prefix_tokens = PROMPT_TOKENS + 2 + 1;
        let mut engine = engine_with(FakeBackend::new(limits(prefix_tokens + 9, 3, 3)));
        let expected = reference(&engine, context, &candidates);
        let actual = engine.score(context, &candidates).unwrap();
        assert_scores_eq(&actual, &expected);
        let stats = engine.stats();
        assert!(stats.groups >= 3, "应当分成多组: {stats:?}");
        assert!(stats.decode_calls > stats.groups, "每组还应按 logits 行数分块: {stats:?}");
        assert_clean_state(&engine);
        // 序列 0 留下的是第一个需要分支的候选（最可能上屏的首选）
        let first =
            engine.backend().tokenize_text(&text::lm_scoring_text(&text::lm_scoring_prefix_inprocess(context), candidates[0]));
        assert_eq!(engine.cached_tokens(), &first[..first.len() - 1]);

        // 紧接着的下一次调用照常复用
        let next = ["一二三四五六", "一二三四五七"];
        assert_scores_eq(&engine.score(context, &next).unwrap(), &reference(&engine, context, &next));
        assert_eq!(engine.stats().reused_tokens, first.len() - 1);
        assert_clean_state(&engine);

        // 参考实现在同样的限制下也要分块
        assert_scores_eq(&engine.score_naive(context, &candidates).unwrap(), &expected);
    }

    #[test]
    fn joint_tokenization_across_the_prefix_boundary() {
        // 分词器把上文末尾的「们」与部分候选开头的「去」合并成一个 token，公共前缀因此变短
        let mut backend = FakeBackend::with_default_limits();
        backend.add_merge("们去");
        backend.add_merge("公园");
        let mut engine = engine_with(backend);
        let context = "周末我们";
        let candidates = ["去公园玩", "趣公园玩", "去公元万", "曲工元"];
        let expected = reference(&engine, context, &candidates);
        let actual = engine.score(context, &candidates).unwrap();
        assert_scores_eq(&actual, &expected);
        // 公共前缀 = 引导语 + 「周末我」，不包含被合并的「们」
        assert_eq!(engine.stats().common_prefix_tokens, PROMPT_TOKENS + 3);
        assert_clean_state(&engine);
    }

    #[test]
    fn candidate_too_long_for_context_is_an_error_not_a_panic() {
        let mut engine = engine_with(FakeBackend::new(limits(PROMPT_TOKENS + 6, 4, 4)));
        let err = engine.score("上文", &["短", "这个候选实在是太长了放不下"]).unwrap_err();
        assert!(matches!(err, LlmError::ContextOverflow { capacity, .. } if capacity == PROMPT_TOKENS + 6));
        // 引擎仍可继续使用
        let cands = ["好的", "号的"];
        assert_scores_eq(&engine.score("上文", &cands).unwrap(), &reference(&engine, "上文", &cands));
    }

    #[test]
    fn abort_during_prefix_keeps_previous_cache_valid() {
        let mut engine = default_engine();
        let cands = ["誓言", "食盐", "试验"];
        engine.score("他在婚礼上许下了", &cands).unwrap();

        // 换上文后的第一次 decode（公共前缀）被中止
        engine.backend_mut().abort_on_decode(1);
        let other = "车快没油了，赶紧把";
        let cands2 = ["油箱加满", "邮箱加满", "油箱家满"];
        assert_eq!(engine.score(other, &cands2).unwrap_err(), LlmError::Cancelled);
        // 只剩与旧缓存公共的引导语部分
        assert_eq!(engine.cached_tokens().len(), PROMPT_TOKENS);
        assert_clean_state(&engine);

        assert_scores_eq(&engine.score(other, &cands2).unwrap(), &reference(&engine, other, &cands2));
        assert_eq!(engine.stats().reused_tokens, PROMPT_TOKENS);
        assert_eq!(engine.backend().counters.kv_clears, 1);
    }

    #[test]
    fn cancel_during_branches_keeps_the_shared_prefix() {
        let cancel = Arc::new(CancelFlag::new());
        let mut engine =
            ScoringEngine::new(FakeBackend::new(limits(256, 8, 2)), Arc::clone(&cancel));
        cancel.begin(cancel.ticket());
        let context = "周末天气不错，我们";
        let cands = ["去公园完了", "去公园玩了", "去公元万了"];
        let common = PROMPT_TOKENS + context.chars().count() + 2;

        // 前 2 次 decode 是公共前缀（30 个 token，分成 16 + 14 两块）；
        // 第 4 次 decode（分支的第二块）执行期间，另一线程调用了 cancel
        engine.backend_mut().cancel_on_decode(4);
        assert_eq!(engine.score(context, &cands).unwrap_err(), LlmError::Cancelled);
        assert_eq!(engine.cached_tokens().len(), common);
        assert_clean_state(&engine);

        // 票据未更新之前，后续请求在入口处就被拒绝，不触碰后端
        let calls = engine.backend().counters.decode_calls;
        assert_eq!(engine.score(context, &cands).unwrap_err(), LlmError::Cancelled);
        assert_eq!(engine.backend().counters.decode_calls, calls);

        // 新请求领取新票据后正常执行，并复用被取消请求留下的公共前缀
        cancel.begin(cancel.ticket());
        assert_scores_eq(&engine.score(context, &cands).unwrap(), &reference(&engine, context, &cands));
        assert_eq!(engine.stats().reused_tokens, common - 1);
        assert_clean_state(&engine);
    }

    #[test]
    fn backend_without_partial_removal_falls_back_to_full_clear() {
        let mut backend = FakeBackend::with_default_limits();
        backend.refuse_partial_rm = true;
        let mut engine = engine_with(backend);
        let a = ["誓言", "食盐"];
        let b = ["油箱加满", "邮箱加满"];
        assert_scores_eq(&engine.score("他在婚礼上许下了", &a).unwrap(), &reference(&engine, "他在婚礼上许下了", &a));
        assert_scores_eq(&engine.score("赶紧把", &b).unwrap(), &reference(&engine, "赶紧把", &b));
        assert_eq!(engine.stats().reused_tokens, 0);
        assert_clean_state(&engine);
    }

    #[test]
    fn many_calls_never_leak_cells() {
        let mut engine = engine_with(FakeBackend::new(limits(96, 4, 4)));
        let contexts = ["", "今天", "今天天气", "今天天气不错，", "换个话题", "今天天气不错，我们"];
        let pools: [&[&str]; 4] = [
            &["去公园玩", "去公园完", "趣公园玩", "去公元玩", "取公园玩", "去工园玩"],
            &["好", "号", "耗"],
            &["怎么回事", "怎么会是", "怎么回是", "咋么回事", "怎么回事"],
            &["a", "ab", "abc", "abd"],
        ];
        for round in 0..40 {
            let context = contexts[round % contexts.len()];
            let cands = pools[(round * 7 + 3) % pools.len()];
            let expected = reference(&engine, context, cands);
            assert_scores_eq(&engine.score(context, cands).unwrap(), &expected);
            assert_clean_state(&engine);
        }
        assert_eq!(engine.backend().counters.kv_clears, 1);
    }

    /// 一段较长的新前缀：冷缓存下公共前缀远超过一块
    const LONG_CONTEXT: &str = "今天上午我们在公司开了一个很长的会议，主要讨论下个季度的产品计划和人员安排";

    #[test]
    fn prefix_is_decoded_in_chunks_and_cancel_keeps_finished_chunks() {
        let mut engine = default_engine();
        let cands = ["然后回家", "燃后回家", "然后会家"];
        let common = PROMPT_TOKENS + LONG_CONTEXT.chars().count();
        assert!(common > 3 * PREFIX_CHUNK_TOKENS);

        // 第 3 次 decode（公共前缀的第三块）被中止：前两块留在缓存里
        engine.backend_mut().abort_on_decode(3);
        assert_eq!(engine.score(LONG_CONTEXT, &cands).unwrap_err(), LlmError::Cancelled);
        assert_eq!(engine.cached_tokens().len(), 2 * PREFIX_CHUNK_TOKENS);
        assert_eq!(engine.backend().counters.tokens_decoded, 2 * PREFIX_CHUNK_TOKENS);
        assert_clean_state(&engine);

        // 下一个请求从第三块接着算，不重算前两块
        assert_scores_eq(&engine.score(LONG_CONTEXT, &cands).unwrap(), &reference(&engine, LONG_CONTEXT, &cands));
        let stats = engine.stats();
        assert_eq!(stats.common_prefix_tokens, common);
        assert_eq!(stats.reused_tokens, 2 * PREFIX_CHUNK_TOKENS);
        assert_eq!(stats.prefix_tokens_decoded, common - 2 * PREFIX_CHUNK_TOKENS);
        assert!(engine.backend().counters.max_batch_tokens <= PREFIX_CHUNK_TOKENS);
        // 每一块都带一行输出（没有输出行的批会让 llama.cpp 多占一块计算缓冲，见 run_plan 里的说明）
        assert_eq!(engine.backend().counters.decodes_without_outputs, 0);
        assert_clean_state(&engine);
    }

    /// 取消在一块前缀算到一半时到达：这一块不可中止，照常算完并留在缓存里，取消在它之后、下一块之前生效
    #[test]
    fn cancel_during_a_prefix_chunk_lets_it_finish_and_keeps_it() {
        let cancel = Arc::new(CancelFlag::new());
        let mut engine = ScoringEngine::new(FakeBackend::with_default_limits(), Arc::clone(&cancel));
        let cands = ["然后回家", "燃后回家", "然后会家"];
        let common = PROMPT_TOKENS + LONG_CONTEXT.chars().count();
        assert!(common > 2 * PREFIX_CHUNK_TOKENS);

        // 冷缓存：第 1 次 decode（公共前缀的第一块）执行期间，另一线程调用了 cancel
        cancel.begin(cancel.ticket());
        engine.backend_mut().cancel_on_decode(1);
        assert_eq!(engine.score(LONG_CONTEXT, &cands).unwrap_err(), LlmError::Cancelled);
        let counters = engine.backend().counters;
        assert_eq!(counters.decode_calls, 1, "取消应当在第一块之后、第二块之前生效: {counters:?}");
        assert_eq!(counters.tokens_decoded, PREFIX_CHUNK_TOKENS, "正在算的那一块应当算完: {counters:?}");
        assert_eq!(engine.cached_tokens().len(), PREFIX_CHUNK_TOKENS, "算完的那一块应当留在缓存里");
        assert!(!cancel.is_shielded(), "被取消之后不应仍在不可中止区段里");
        assert_clean_state(&engine);

        // 下一个请求从第二块接着算
        cancel.begin(cancel.ticket());
        assert_scores_eq(&engine.score(LONG_CONTEXT, &cands).unwrap(), &reference(&engine, LONG_CONTEXT, &cands));
        let stats = engine.stats();
        assert_eq!(stats.reused_tokens, PREFIX_CHUNK_TOKENS, "{stats:?}");
        assert_eq!(stats.prefix_tokens_decoded, common - PREFIX_CHUNK_TOKENS, "{stats:?}");
        assert_clean_state(&engine);
    }

    /// 用户敲得比「整段前缀的解码时间」快：每个请求只来得及解码 20 个 token 就被下一次击键取消。
    /// 整段前缀一次解码时，每个请求都在前缀算完之前被取消，永远没有结果；分块之后每个请求至少推进一块。
    #[test]
    fn fast_typing_cannot_starve_the_scorer() {
        // 与 LlmScorer 一样：每次请求前领取票据（时间到时假后端会在标记上记一次取消）
        let cancel = Arc::new(CancelFlag::new());
        let mut engine = ScoringEngine::new(FakeBackend::new(limits(256, 8, 8)), Arc::clone(&cancel));
        // 上一个词的请求：缓存里有引导语和另一段上文
        cancel.begin(cancel.ticket());
        engine.score("他在婚礼上许下了", &["誓言", "食盐"]).unwrap();

        // 同一上文下连续 6 次击键，候选逐次变长
        let pools: [&[&str]; 6] = [
            &["在", "再", "载"],
            &["在看", "再看", "在刊"],
            &["再考", "在考", "在烤"],
            &["再考虑", "在考虑", "再考率"],
            &["再考虑新", "在考虑新", "再考虑心"],
            &["再考虑新的", "在考虑新的", "再考虑心的"],
        ];
        let budget = 20;
        assert!(LONG_CONTEXT.chars().count() > budget, "新前缀必须超过一个请求来得及算的量");
        let mut outcomes = Vec::new();
        for pool in pools {
            engine.backend_mut().abort_after_tokens(Some(budget));
            cancel.begin(cancel.ticket());
            match engine.score(LONG_CONTEXT, pool) {
                Ok(scores) => {
                    assert_scores_eq(&scores, &reference(&engine, LONG_CONTEXT, pool));
                    outcomes.push(true);
                }
                Err(LlmError::Cancelled) => outcomes.push(false),
                Err(other) => panic!("意外错误: {other:?}"),
            }
            assert_clean_state(&engine);
            // 被取消的请求也留下了至少一块
            assert!(engine.cached_tokens().len() >= PROMPT_TOKENS + PREFIX_CHUNK_TOKENS.min(LONG_CONTEXT.chars().count()));
        }
        // 37 个新 token：第一个请求的第二块超出时间，但前缀块不可中止，算完才停（推进 32 个）；
        // 第二个请求算完剩下的 5 个并给出结果，之后每次击键都有结果
        assert_eq!(outcomes, vec![false, true, true, true, true, true]);
    }

    #[test]
    fn keystrokes_faster_than_one_prefix_chunk_still_make_progress() {
        // 击键间隔短于一块前缀的解码时间（这里折算成 10 个 token，一块是 16 个）。
        // 前缀块可以中止时，每次请求的第一块都被打断，缓存一点也不增长，停手之后还得把整段前缀从头算一遍
        let cancel = Arc::new(CancelFlag::new());
        let mut engine = ScoringEngine::new(FakeBackend::new(limits(256, 8, 8)), Arc::clone(&cancel));
        let budget = 10;
        assert!(budget < PREFIX_CHUNK_TOKENS);
        let pool: &[&str] = &["再考虑新的", "在考虑新的", "再考虑心的"];
        let full_prefix = PROMPT_TOKENS + LONG_CONTEXT.chars().count();
        let prefix_chunks = full_prefix.div_ceil(PREFIX_CHUNK_TOKENS);

        // 快速打字期间：每次请求都被下一次击键取消，但前缀每次至少推进一块，直到整段前缀进了缓存
        let mut cached_after = Vec::new();
        for _ in 0..prefix_chunks + 2 {
            engine.backend_mut().abort_after_tokens(Some(budget));
            cancel.begin(cancel.ticket());
            let result = engine.score(LONG_CONTEXT, pool);
            assert!(matches!(result, Err(LlmError::Cancelled)), "分支解码比击键间隔长，应当被取消: {result:?}");
            assert_clean_state(&engine);
            cached_after.push(engine.cached_tokens().len());
        }
        for (i, pair) in cached_after.windows(2).enumerate() {
            if pair[0] < full_prefix {
                assert!(pair[1] > pair[0], "第 {} 次请求没有推进前缀: {cached_after:?}", i + 1);
            }
        }
        assert_eq!(cached_after[prefix_chunks - 1], full_prefix, "前缀 {prefix_chunks} 块应在 {prefix_chunks} 次击键内算完: {cached_after:?}");

        // 停手：最后一次请求只需重算前缀的最后一个 token 和分支
        engine.backend_mut().abort_after_tokens(None);
        cancel.begin(cancel.ticket());
        let scores = engine.score(LONG_CONTEXT, pool).unwrap();
        assert_scores_eq(&scores, &reference(&engine, LONG_CONTEXT, pool));
        let stats = engine.stats();
        assert_eq!(stats.reused_tokens, full_prefix - 1, "{stats:?}");
        assert_eq!(stats.prefix_tokens_decoded, 1, "{stats:?}");
    }

    #[test]
    fn branch_decodes_stay_abortable() {
        // 前缀已在缓存里时，取消要能在计算中途打断分支解码（分支只对这一组候选有用）
        let cancel = Arc::new(CancelFlag::new());
        let mut engine = ScoringEngine::new(FakeBackend::new(limits(256, 8, 8)), Arc::clone(&cancel));
        cancel.begin(cancel.ticket());
        engine.score(LONG_CONTEXT, &["在", "再"]).unwrap();
        let cached = engine.cached_tokens().len();
        let decoded = engine.backend().counters.tokens_decoded;
        // 只够重算前缀的最后一个 token，分支的第一批算到一半就到了下一次击键
        engine.backend_mut().abort_after_tokens(Some(1));
        cancel.begin(cancel.ticket());
        let result = engine.score(LONG_CONTEXT, &["再考虑新的", "在考虑新的", "再考虑心的"]);
        assert!(matches!(result, Err(LlmError::Cancelled)), "{result:?}");
        // 分支有两批（12 个 token、每批最多 8 行 logits）：第一批必须在计算中途中止，
        // 而不是算完之后才由引擎在两批之间发现取消
        let stats = engine.stats();
        assert_eq!(stats.prefix_tokens_decoded, 1, "{stats:?}");
        assert_eq!(
            engine.backend().counters.tokens_decoded,
            decoded + stats.prefix_tokens_decoded,
            "分支的第一批不应算完: {stats:?}"
        );
        assert_clean_state(&engine);
        assert_eq!(engine.cached_tokens().len(), cached, "前缀缓存应当原样保留");
        assert!(!cancel.is_shielded(), "离开前缀解码之后不应仍在不可中止区段里");
    }

    #[test]
    fn no_decode_reaches_the_attention_kernel_switch() {
        // logits 行数上限放得很宽，让「单批 token 数」成为唯一的约束
        let wide = BackendLimits { n_ctx: 1024, n_batch: 1024, n_seq_max: 8, n_outputs_max: 256 };
        let context = "今天上午我们在公司开了一个很长的会议，主要讨论下个季度的产品计划和人员安排，大家都发表了自己的看法";
        assert!(PROMPT_TOKENS + context.chars().count() > MAX_DECODE_TOKENS, "参考实现的前缀要超过一批");
        let candidates = [
            "最后决定先把手头的项目做完，然后再考虑新的方向，下周一之前每个人都要提交一份详细的工作报告",
            "最后决定先把手头的项目做完，然后在考虑新的方向，下周一之前每个人都要提交一份详细的工作报告",
            "最后觉定先把手头的项目做完，然后再考虑新的方向，下周一之前每个人都要提交一份详细的工作抱告",
        ];
        let mut engine = engine_with(FakeBackend::new(wide));
        let expected = reference(&engine, context, &candidates);
        assert_scores_eq(&engine.score(context, &candidates).unwrap(), &expected);
        let stats = engine.stats();
        assert!(stats.branch_tokens_decoded > MAX_DECODE_TOKENS, "分支合起来要超过一批: {stats:?}");
        assert_eq!(stats.groups, 1);
        assert!(engine.backend().counters.max_batch_tokens <= MAX_DECODE_TOKENS, "{:?}", engine.backend().counters);
        assert_clean_state(&engine);

        // 参考实现：每个候选从头解码，前缀 + 候选远超过 63 个 token
        let mut engine = engine_with(FakeBackend::new(wide));
        assert_scores_eq(&engine.score_naive(context, &candidates).unwrap(), &expected);
        assert!(engine.backend().counters.tokens_decoded > 3 * MAX_DECODE_TOKENS);
        assert!(engine.backend().counters.max_batch_tokens <= MAX_DECODE_TOKENS, "{:?}", engine.backend().counters);
    }

    /// 假分词器按字符切分：token 序列可以原样还原成文本
    fn detokenize(tokens: &[i32]) -> String {
        tokens.iter().map(|t| char::from_u32(*t as u32).unwrap()).collect()
    }

    #[test]
    fn latin_context_keeps_one_space_before_the_candidate() {
        let mut engine = default_engine();
        let seqs = engine.tokenize_candidates("I really love ", &["you", "yew"]).unwrap();
        assert_eq!(detokenize(&seqs[0]), format!("{LM_SCORE_PROMPT_PREFIX}I really love you"));
        assert_eq!(detokenize(&seqs[1]), format!("{LM_SCORE_PROMPT_PREFIX}I really love yew"));
        // 中文上文末尾的空白照旧去掉
        let seqs = engine.tokenize_candidates("今天天气 ", &["不错"]).unwrap();
        assert_eq!(detokenize(&seqs[0]), format!("{LM_SCORE_PROMPT_PREFIX}今天天气不错"));
    }

    /// 用户一段一段上屏，上文超过 48 个字符的窗口之后：固定窗口每上屏一次就向后滑，前缀从引导语之后就对不上，
    /// 每次都要把整段上文重算一遍；对齐到分句处的窗口在大多数步骤里只需要算新上屏的那几个字。
    #[test]
    fn long_text_keeps_reusing_the_cache_as_it_grows() {
        let pieces = [
            "今天上午", "我们", "开了", "一个", "很长的", "会，", "讨论", "下个季度", "的", "产品", "计划。", "大家",
            "都觉得", "时间", "有点", "紧张，", "但是", "只要", "安排", "得当，", "应该", "还是", "可以", "按时",
            "完成的。", "会后", "我", "把", "会议", "纪要", "整理", "好了，", "发到", "了", "群里，", "请", "各位",
            "有空", "的时候", "看一下，", "有问题", "随时", "找我。", "另外，", "周五", "下午", "还有", "一次",
            "评审，", "别忘了", "提前", "准备", "材料。",
        ];
        let cands = ["然后回家", "燃后回家", "然后会家"];
        let mut engine = default_engine();
        let mut text = String::new();
        let (mut steps, mut warm_steps, mut decoded) = (0usize, 0usize, 0usize);
        for piece in pieces {
            let before = text.chars().count();
            text.push_str(piece);
            assert_scores_eq(&engine.score(&text, &cands).unwrap(), &reference(&engine, &text, &cands));
            assert_clean_state(&engine);
            let stats = engine.stats();
            // 只统计上文已经超过窗口之后的步骤
            if before > text::LM_SCORE_CONTEXT_TAIL_CHARS {
                steps += 1;
                decoded += stats.prefix_tokens_decoded;
                // 「接上了」：除了新上屏的这几个字（和必须重算的最后一个 token），前缀都来自缓存
                warm_steps += usize::from(stats.prefix_tokens_decoded <= piece.chars().count() + 1);
            }
        }
        assert!(steps >= 30, "用例应覆盖足够多的超窗步骤: {steps}");
        assert!(warm_steps * 10 >= steps * 8, "超窗之后绝大多数步骤应接上缓存: {warm_steps}/{steps}");
        // 固定窗口下每一步都要重算 48 个字符的上文
        assert!(decoded < steps * text::LM_SCORE_CONTEXT_TAIL_CHARS / 3, "超窗 {steps} 步共解码 {decoded} 个前缀 token");
    }
}
