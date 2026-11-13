// ime-llm 垫片层实现，见 ime_llm_shim.h。
//
// 内存配置依据（iPhone 键盘扩展实测）：
//   - mmap + use_extra_bufts=false + 纯 CPU 是唯一能让权重不计入 phys_footprint 的组合
//   - 不向 llama.cpp 传任何 GPU 设备（devices = {NULL}），避免初始化 Metal
//   - n_outputs_max 要小：logits 输出缓冲 = n_outputs_max * n_vocab * 4 字节
//     （n_outputs_max 取收敛后的值，不小于并行序列数，见 resolve_limits）
#include "ime_llm_shim.h"

#include "llama.h"

#include <atomic>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <mutex>
#include <new>
#include <string>
#include <thread>
#include <vector>

#if defined(__APPLE__)
#include <mach/mach.h>
#include <sys/sysctl.h>
#if !defined(IME_LLM_NO_ACCELERATE)
#include <Accelerate/Accelerate.h>
#define IME_LLM_USE_ACCELERATE 1
#endif
#endif

struct ime_llm {
    llama_model *model = nullptr;
    llama_context *ctx = nullptr;
    const llama_vocab *vocab = nullptr;
    llama_memory_t mem = nullptr;
    llama_batch batch{};
    bool batch_ready = false;
    int32_t n_vocab = 0;
    int32_t n_ctx = 0;
    int32_t n_batch = 0;
    int32_t n_seq_max = 0;
    int32_t n_outputs_max = 0;
    int32_t n_threads = 0;
    // 最近一次成功 decode 的 want 标记，用于校验 ime_llm_logprobs 的行号
    std::vector<uint8_t> last_want;
    std::vector<float> scratch;
    // 中止回调：decode 期间只读；设置发生在两次 decode 之间（同一把 Rust 锁内）
    std::atomic<ime_llm_abort_fn> abort_fn{nullptr};
    std::atomic<void *> abort_user{nullptr};
};

namespace {

std::mutex g_log_mutex;
std::string g_last_error; // 最近的 ERROR 级日志，用于拼装加载失败原因
std::atomic<bool> g_verbose{false};
std::once_flag g_backend_once;

void log_callback(enum ggml_log_level level, const char *text, void *) {
    if (text == nullptr) {
        return;
    }
    if (g_verbose.load(std::memory_order_relaxed)) {
        fputs(text, stderr);
    }
    if (level == GGML_LOG_LEVEL_ERROR) {
        std::lock_guard<std::mutex> lock(g_log_mutex);
        if (g_last_error.size() < 600) {
            g_last_error.append(text);
        }
    }
}

void write_err(char *err, int32_t cap, const std::string &msg) {
    if (err == nullptr || cap <= 0) {
        return;
    }
    size_t n = msg.size();
    if (n > (size_t)cap - 1) {
        n = (size_t)cap - 1;
    }
    memcpy(err, msg.data(), n);
    err[n] = '\0';
}

std::string take_last_error() {
    std::lock_guard<std::mutex> lock(g_log_mutex);
    std::string s = g_last_error;
    g_last_error.clear();
    while (!s.empty() && (s.back() == '\n' || s.back() == ' ')) {
        s.pop_back();
    }
    return s;
}

int32_t auto_threads() {
    int32_t n = 0;
#if defined(__APPLE__)
    int32_t perf = 0;
    size_t len = sizeof(perf);
    if (sysctlbyname("hw.perflevel0.physicalcpu", &perf, &len, nullptr, 0) == 0 && perf > 0) {
        n = perf;
    }
#endif
    if (n <= 0) {
        n = (int32_t)(std::thread::hardware_concurrency() / 2);
    }
    if (n < 1) {
        n = 1;
    }
    if (n > 4) {
        n = 4;
    }
    return n;
}

bool abort_trampoline(void *data) {
    ime_llm *h = (ime_llm *)data;
    ime_llm_abort_fn fn = h->abort_fn.load(std::memory_order_acquire);
    if (fn == nullptr) {
        return false;
    }
    return fn(h->abort_user.load(std::memory_order_acquire));
}

// 对一行 logits 求 (max, log(sum(exp(x - max))))，于是 log p(t) = x[t] - max - lse
void row_norm(const float *row, int32_t n, std::vector<float> &scratch, float &mx_out, double &lse_out) {
#if defined(IME_LLM_USE_ACCELERATE)
    float mx = 0.0f;
    vDSP_maxv(row, 1, &mx, (vDSP_Length)n);
    float neg = -mx;
    scratch.resize((size_t)n);
    vDSP_vsadd(row, 1, &neg, scratch.data(), 1, (vDSP_Length)n);
    int cnt = n;
    vvexpf(scratch.data(), scratch.data(), &cnt);
    // 分段累加到 double，避免 15 万项单精度求和的舍入误差
    double sum = 0.0;
    const int32_t block = 4096;
    for (int32_t i = 0; i < n; i += block) {
        float part = 0.0f;
        vDSP_sve(scratch.data() + i, 1, &part, (vDSP_Length)((n - i) < block ? (n - i) : block));
        sum += (double)part;
    }
    mx_out = mx;
    lse_out = std::log(sum);
#else
    (void)scratch;
    float mx = row[0];
    for (int32_t i = 1; i < n; i++) {
        if (row[i] > mx) {
            mx = row[i];
        }
    }
    double sum = 0.0;
    for (int32_t i = 0; i < n; i++) {
        sum += std::exp((double)(row[i] - mx));
    }
    mx_out = mx;
    lse_out = std::log(sum);
#endif
}

// 把请求的并行序列数与 logits 行数收敛到 llama.cpp 能接受的组合：
//   n_seq_max <= min(llama.cpp 的序列数上限, n_ctx)，且 n_seq_max <= n_outputs_max <= n_ctx。
// llama.cpp 创建上下文时按 max(n_outputs_max, n_seq_max) 行预留输出缓冲，并断言它不超过 n_outputs_max；
// 所以必须先把 n_seq_max 压到 n_ctx 以内，再抬高 n_outputs_max，否则 n_ctx < n_seq_max 时
// n_outputs_max 被压回 n_ctx 后小于 n_seq_max，断言失败会直接 abort 整个进程。
void resolve_limits(int32_t n_ctx, int32_t &n_seq_max, int32_t &n_outputs_max) {
    const int32_t seq_cap = (int32_t)llama_max_parallel_sequences();
    if (n_seq_max > seq_cap) {
        n_seq_max = seq_cap;
    }
    if (n_seq_max > n_ctx) {
        n_seq_max = n_ctx;
    }
    if (n_outputs_max < n_seq_max) {
        n_outputs_max = n_seq_max;
    }
    if (n_outputs_max > n_ctx) {
        n_outputs_max = n_ctx;
    }
}

void destroy(ime_llm *h) {
    if (h == nullptr) {
        return;
    }
    if (h->batch_ready) {
        llama_batch_free(h->batch);
    }
    if (h->ctx != nullptr) {
        llama_free(h->ctx);
    }
    if (h->model != nullptr) {
        llama_model_free(h->model);
    }
    delete h;
}

} // namespace

extern "C" {

int32_t ime_llm_abi_version(void) { return IME_LLM_ABI_VERSION; }

ime_llm *ime_llm_load(const char *model_path, int32_t n_ctx, int32_t n_threads, int32_t n_gpu_layers,
                      int32_t n_seq_max, int32_t n_outputs_max, uint32_t flags, char *err, int32_t err_cap) {
    if (model_path == nullptr || n_ctx <= 0 || n_seq_max <= 0 || n_outputs_max <= 0) {
        write_err(err, err_cap, "invalid arguments");
        return nullptr;
    }
    ime_llm *h = nullptr;
    try {
        g_verbose.store((flags & IME_LLM_FLAG_VERBOSE) != 0, std::memory_order_relaxed);
        std::call_once(g_backend_once, [] {
            llama_log_set(log_callback, nullptr);
            llama_backend_init();
        });
        take_last_error();

        resolve_limits(n_ctx, n_seq_max, n_outputs_max);
        if (n_threads <= 0) {
            n_threads = auto_threads();
        }

        h = new ime_llm();

        llama_model_params mp = llama_model_default_params();
        mp.n_gpu_layers = n_gpu_layers;
        mp.load_mode = (flags & IME_LLM_FLAG_NO_MMAP) ? LLAMA_LOAD_MODE_NONE : LLAMA_LOAD_MODE_MMAP;
        mp.use_extra_bufts = (flags & IME_LLM_FLAG_REPACK) != 0;
        static ggml_backend_dev_t no_devices[1] = {nullptr};
        if (n_gpu_layers == 0) {
            mp.devices = no_devices; // 纯 CPU：连 GPU 设备都不初始化
        }
        h->model = llama_model_load_from_file(model_path, mp);
        if (h->model == nullptr) {
            std::string why = take_last_error();
            write_err(err, err_cap, why.empty() ? std::string("llama_model_load_from_file failed") : why);
            destroy(h);
            return nullptr;
        }

        llama_context_params cp = llama_context_default_params();
        cp.n_ctx = (uint32_t)n_ctx;
        cp.n_batch = (uint32_t)n_ctx;
        cp.n_ubatch = (uint32_t)n_ctx;
        cp.n_seq_max = (uint32_t)n_seq_max;
        cp.kv_unified = true; // 所有候选分支共用 n_ctx 个 KV 单元，共享前缀只存一份
        cp.n_outputs_max = (uint32_t)n_outputs_max;
        cp.n_threads = n_threads;
        cp.n_threads_batch = n_threads;
        cp.no_perf = true;
        cp.abort_callback = abort_trampoline;
        cp.abort_callback_data = h;
        h->ctx = llama_init_from_model(h->model, cp);
        if (h->ctx == nullptr) {
            std::string why = take_last_error();
            write_err(err, err_cap, why.empty() ? std::string("llama_init_from_model failed") : why);
            destroy(h);
            return nullptr;
        }

        h->vocab = llama_model_get_vocab(h->model);
        h->mem = llama_get_memory(h->ctx);
        h->n_vocab = llama_vocab_n_tokens(h->vocab);
        h->n_ctx = (int32_t)llama_n_ctx(h->ctx);
        h->n_batch = (int32_t)llama_n_batch(h->ctx);
        h->n_seq_max = (int32_t)llama_n_seq_max(h->ctx);
        h->n_outputs_max = n_outputs_max;
        h->n_threads = n_threads;
        if (h->mem == nullptr || h->n_vocab <= 0 || h->n_batch <= 0) {
            write_err(err, err_cap, "model has no usable memory/vocab");
            destroy(h);
            return nullptr;
        }
        h->batch = llama_batch_init(h->n_batch, 0, 1);
        h->batch_ready = true;
        h->scratch.reserve((size_t)h->n_vocab);
        return h;
    } catch (const std::exception &e) {
        write_err(err, err_cap, std::string("exception: ") + e.what());
    } catch (...) {
        write_err(err, err_cap, "unknown exception");
    }
    try {
        destroy(h);
    } catch (...) {
    }
    return nullptr;
}

int32_t ime_llm_resolve_limits(int32_t n_ctx, int32_t *n_seq_max, int32_t *n_outputs_max) {
    if (n_ctx <= 0 || n_seq_max == nullptr || n_outputs_max == nullptr || *n_seq_max <= 0 || *n_outputs_max <= 0) {
        return 0;
    }
    try {
        resolve_limits(n_ctx, *n_seq_max, *n_outputs_max);
        return 1;
    } catch (...) {
        return 0;
    }
}

void ime_llm_free(ime_llm *h) {
    try {
        destroy(h);
    } catch (...) {
    }
}

int32_t ime_llm_n_vocab(const ime_llm *h) { return h ? h->n_vocab : 0; }
int32_t ime_llm_n_ctx(const ime_llm *h) { return h ? h->n_ctx : 0; }
int32_t ime_llm_n_batch(const ime_llm *h) { return h ? h->n_batch : 0; }
int32_t ime_llm_n_seq_max(const ime_llm *h) { return h ? h->n_seq_max : 0; }
int32_t ime_llm_n_outputs_max(const ime_llm *h) { return h ? h->n_outputs_max : 0; }
int32_t ime_llm_n_threads(const ime_llm *h) { return h ? h->n_threads : 0; }

void ime_llm_set_abort_callback(ime_llm *h, ime_llm_abort_fn cb, void *user) {
    if (h == nullptr) {
        return;
    }
    // 先撤掉旧回调再换 user，避免旧回调读到新 user
    h->abort_fn.store(nullptr, std::memory_order_release);
    h->abort_user.store(user, std::memory_order_release);
    h->abort_fn.store(cb, std::memory_order_release);
}

int32_t ime_llm_tokenize(const ime_llm *h, const char *text, int32_t text_len, int32_t *out, int32_t cap) {
    if (h == nullptr || text == nullptr || text_len < 0 || cap < 0 || (out == nullptr && cap > 0)) {
        return INT32_MIN;
    }
    try {
        return llama_tokenize(h->vocab, text, text_len, out, cap, /*add_special*/ false, /*parse_special*/ false);
    } catch (...) {
        return INT32_MIN;
    }
}

void ime_llm_kv_clear(ime_llm *h) {
    if (h == nullptr) {
        return;
    }
    try {
        // data = false：只清元数据，不必把 KV 数据区清零
        llama_memory_clear(h->mem, false);
        h->last_want.clear();
    } catch (...) {
    }
}

int32_t ime_llm_kv_seq_rm(ime_llm *h, int32_t seq, int32_t p0, int32_t p1) {
    if (h == nullptr || seq < 0 || seq >= h->n_seq_max) {
        return 0;
    }
    try {
        return llama_memory_seq_rm(h->mem, seq, p0, p1) ? 1 : 0;
    } catch (...) {
        return 0;
    }
}

void ime_llm_kv_seq_cp(ime_llm *h, int32_t src, int32_t dst, int32_t p0, int32_t p1) {
    if (h == nullptr || src < 0 || dst < 0 || src >= h->n_seq_max || dst >= h->n_seq_max || src == dst) {
        return;
    }
    try {
        llama_memory_seq_cp(h->mem, src, dst, p0, p1);
    } catch (...) {
    }
}

int32_t ime_llm_decode(ime_llm *h, const int32_t *tokens, const int32_t *pos, const int32_t *seq,
                       const uint8_t *want, int32_t n) {
    if (h == nullptr || tokens == nullptr || pos == nullptr || seq == nullptr || want == nullptr) {
        return IME_LLM_ERR_ARGS;
    }
    // 下列条件在 llama.cpp 内部是断言（失败即 abort 整个进程），必须在这里拦住
    if (n <= 0 || n > h->n_batch) {
        return IME_LLM_ERR_ARGS;
    }
    int32_t n_want = 0;
    for (int32_t i = 0; i < n; i++) {
        if (tokens[i] < 0 || tokens[i] >= h->n_vocab || pos[i] < 0 || seq[i] < 0 || seq[i] >= h->n_seq_max) {
            return IME_LLM_ERR_ARGS;
        }
        n_want += want[i] != 0;
    }
    if (n_want > h->n_outputs_max) {
        return IME_LLM_ERR_ARGS;
    }
    try {
        h->last_want.clear();
        llama_batch &b = h->batch;
        b.n_tokens = n;
        for (int32_t i = 0; i < n; i++) {
            b.token[i] = tokens[i];
            b.pos[i] = pos[i];
            b.n_seq_id[i] = 1;
            b.seq_id[i][0] = seq[i];
            b.logits[i] = want[i] != 0;
        }
        int32_t r = llama_decode(h->ctx, b);
        if (r == 0) {
            h->last_want.assign(want, want + n);
        }
        return r;
    } catch (...) {
        return IME_LLM_ERR_EXCEPTION;
    }
}

int32_t ime_llm_logprobs(ime_llm *h, int32_t batch_index, const int32_t *targets, int32_t n_targets, double *out) {
    if (h == nullptr || targets == nullptr || out == nullptr || n_targets < 0) {
        return IME_LLM_ERR_ARGS;
    }
    if (batch_index < 0 || (size_t)batch_index >= h->last_want.size() || h->last_want[(size_t)batch_index] == 0) {
        return IME_LLM_ERR_ARGS;
    }
    for (int32_t k = 0; k < n_targets; k++) {
        if (targets[k] < 0 || targets[k] >= h->n_vocab) {
            return IME_LLM_ERR_ARGS;
        }
    }
    try {
        const float *row = llama_get_logits_ith(h->ctx, batch_index);
        if (row == nullptr) {
            return IME_LLM_ERR_ARGS;
        }
        float mx = 0.0f;
        double lse = 0.0;
        row_norm(row, h->n_vocab, h->scratch, mx, lse);
        for (int32_t k = 0; k < n_targets; k++) {
            out[k] = (double)row[targets[k]] - (double)mx - lse;
        }
        return 0;
    } catch (...) {
        return IME_LLM_ERR_EXCEPTION;
    }
}

uint64_t ime_llm_phys_footprint(void) {
#if defined(__APPLE__)
    task_vm_info_data_t info;
    mach_msg_type_number_t count = TASK_VM_INFO_COUNT;
    if (task_info(mach_task_self(), TASK_VM_INFO, (task_info_t)&info, &count) == KERN_SUCCESS) {
        return (uint64_t)info.phys_footprint;
    }
#endif
    return 0;
}

} // extern "C"
