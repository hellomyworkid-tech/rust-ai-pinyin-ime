// ime-llm 垫片层：把 llama.cpp 的 C API 收窄成一组稳定的 C ABI。
//
// 约定：
//   - 只出现定宽整数、float、指针和一个不透明句柄；Rust 一侧不声明 llama.cpp 的任何结构体，
//     因此升级 llama.cpp（其参数结构体经常变动）只需重新编译本垫片
//   - 所有函数都不会抛出 C++ 异常；会触发 llama.cpp 内部断言的输入在垫片里提前拦截并返回错误码
//   - 同一个句柄不可并发调用（由 Rust 一侧加锁保证）；中止回调可在任意线程被读取
//   - 修改任何已有函数的签名或语义时必须递增 IME_LLM_ABI_VERSION
#ifndef IME_LLM_SHIM_H
#define IME_LLM_SHIM_H

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define IME_LLM_ABI_VERSION 1

// ime_llm_load 的 flags 位
#define IME_LLM_FLAG_VERBOSE 1u // 输出 llama.cpp 日志到 stderr（默认静默）
#define IME_LLM_FLAG_NO_MMAP 2u // 不使用 mmap（默认 mmap，权重页不计入 phys_footprint）
#define IME_LLM_FLAG_REPACK  4u // 允许权重重排 use_extra_bufts（默认关闭；开启会把权重复制进匿名内存）

// ime_llm_decode 的返回值
#define IME_LLM_DECODE_OK        0
#define IME_LLM_DECODE_NO_SLOT   1    // KV 缓存放不下本批（状态已恢复到调用前）
#define IME_LLM_DECODE_ABORTED   2    // 被中止回调打断（本批写入的位置已被移除）
#define IME_LLM_DECODE_BAD_INPUT (-1) // llama.cpp 判定输入批非法（状态未变）
#define IME_LLM_ERR_ARGS         (-100) // 垫片层参数检查未通过（状态未变）
#define IME_LLM_ERR_EXCEPTION    (-101) // 捕获到 C++ 异常（状态未知，调用方应清空 KV）
// 其余负值为 llama.cpp 的致命错误（状态未知，调用方应清空 KV）

typedef struct ime_llm ime_llm;

// 返回 true 表示请求中止当前的 ime_llm_decode
typedef bool (*ime_llm_abort_fn)(void *user);

int32_t ime_llm_abi_version(void);

// 加载模型并创建上下文。失败返回 NULL，并把以 NUL 结尾的错误说明写入 err（最多 err_cap 字节）。
//   n_ctx          上下文长度（所有并行序列共用，kv_unified）
//   n_threads      线程数；<= 0 表示自动（性能核数量，上限 4）
//   n_gpu_layers   放到 GPU 的层数；0 = 纯 CPU（此时不初始化任何 GPU 设备）
//   n_seq_max      并行序列数上限
//   n_outputs_max  单次 decode 可请求 logits 的 token 数上限（每行占 n_vocab * 4 字节）
// n_seq_max 与 n_outputs_max 先按 ime_llm_resolve_limits 的规则收敛，实际生效的值用下面的查询函数读取。
ime_llm *ime_llm_load(const char *model_path, int32_t n_ctx, int32_t n_threads, int32_t n_gpu_layers,
                      int32_t n_seq_max, int32_t n_outputs_max, uint32_t flags, char *err, int32_t err_cap);

// ime_llm_load 对并行序列数与 logits 行数的收敛规则（不需要模型，可单独调用）：
//   *n_seq_max     不超过 llama.cpp 的并行序列数上限，也不超过 n_ctx
//   *n_outputs_max 不小于收敛后的 *n_seq_max，也不超过 n_ctx
// 于是总有 *n_seq_max <= *n_outputs_max <= n_ctx；输出缓冲为 *n_outputs_max * n_vocab * 4 字节。
// 三个值都必须为正；参数不合法返回 0（不修改输出），成功返回 1。
int32_t ime_llm_resolve_limits(int32_t n_ctx, int32_t *n_seq_max, int32_t *n_outputs_max);
void ime_llm_free(ime_llm *h);

// 实际生效的参数
int32_t ime_llm_n_vocab(const ime_llm *h);
int32_t ime_llm_n_ctx(const ime_llm *h);
int32_t ime_llm_n_batch(const ime_llm *h);
int32_t ime_llm_n_seq_max(const ime_llm *h);
int32_t ime_llm_n_outputs_max(const ime_llm *h);
int32_t ime_llm_n_threads(const ime_llm *h);

// 设置中止回调（NULL 取消）。回调会在 decode 的计算过程中被反复调用，必须足够轻且不得抛出异常/恐慌。
void ime_llm_set_abort_callback(ime_llm *h, ime_llm_abort_fn cb, void *user);

// 分词（add_special = false, parse_special = false）。
// 返回 token 数；若 cap 不足返回所需数量的相反数（此时 out 内容未定义）；INT32_MIN 表示失败。
int32_t ime_llm_tokenize(const ime_llm *h, const char *text, int32_t text_len, int32_t *out, int32_t cap);

// KV 缓存操作，语义同 llama_memory_clear / llama_memory_seq_rm / llama_memory_seq_cp
// （p0 < 0 表示从 0 开始，p1 < 0 表示到末尾）。seq_rm 返回 1 成功、0 失败。
void ime_llm_kv_clear(ime_llm *h);
int32_t ime_llm_kv_seq_rm(ime_llm *h, int32_t seq, int32_t p0, int32_t p1);
void ime_llm_kv_seq_cp(ime_llm *h, int32_t src, int32_t dst, int32_t p0, int32_t p1);

// 解码一批 token：第 i 个 token 为 tokens[i]，位置 pos[i]，属于序列 seq[i]，want[i] != 0 表示需要它的 logits。
// 返回值见 IME_LLM_DECODE_*。
int32_t ime_llm_decode(ime_llm *h, const int32_t *tokens, const int32_t *pos, const int32_t *seq,
                       const uint8_t *want, int32_t n);

// 读取最近一次 decode 中第 batch_index 个 token（必须 want != 0）之后的下一 token 对数概率：
// 对整个词表做精确 log-softmax，out[k] = log P(targets[k])。成功返回 0。
int32_t ime_llm_logprobs(ime_llm *h, int32_t batch_index, const int32_t *targets, int32_t n_targets, double *out);

// 当前进程的 phys_footprint（字节）；非 Apple 平台返回 0。
uint64_t ime_llm_phys_footprint(void);

#ifdef __cplusplus
}
#endif

#endif // IME_LLM_SHIM_H
