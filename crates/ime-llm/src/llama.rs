//! llama.cpp 后端：通过 `shim/ime_llm_shim.h` 定义的窄 C ABI 调用，Rust 一侧只持有一个不透明句柄。

use crate::backend::{BackendLimits, BatchItem, CancelFlag, DecodeOutcome, LmBackend};
use crate::scorer::LlmOptions;
use crate::LlmError;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Arc;

/// 必须与 `ime_llm_shim.h` 中的 `IME_LLM_ABI_VERSION` 一致
const SHIM_ABI_VERSION: i32 = 1;

const FLAG_VERBOSE: u32 = 1;
const FLAG_NO_MMAP: u32 = 2;
const FLAG_REPACK: u32 = 4;

const DECODE_OK: i32 = 0;
const DECODE_NO_SLOT: i32 = 1;
const DECODE_ABORTED: i32 = 2;
const DECODE_BAD_INPUT: i32 = -1;
const ERR_ARGS: i32 = -100;

#[repr(C)]
struct RawHandle {
    _opaque: [u8; 0],
}

type AbortFn = unsafe extern "C" fn(user: *mut c_void) -> bool;

extern "C" {
    fn ime_llm_abi_version() -> i32;
    fn ime_llm_load(
        model_path: *const c_char,
        n_ctx: i32,
        n_threads: i32,
        n_gpu_layers: i32,
        n_seq_max: i32,
        n_outputs_max: i32,
        flags: u32,
        err: *mut c_char,
        err_cap: i32,
    ) -> *mut RawHandle;
    #[cfg(test)]
    fn ime_llm_resolve_limits(n_ctx: i32, n_seq_max: *mut i32, n_outputs_max: *mut i32) -> i32;
    fn ime_llm_free(h: *mut RawHandle);
    fn ime_llm_n_ctx(h: *const RawHandle) -> i32;
    fn ime_llm_n_batch(h: *const RawHandle) -> i32;
    fn ime_llm_n_seq_max(h: *const RawHandle) -> i32;
    fn ime_llm_n_outputs_max(h: *const RawHandle) -> i32;
    fn ime_llm_set_abort_callback(h: *mut RawHandle, cb: Option<AbortFn>, user: *mut c_void);
    fn ime_llm_tokenize(h: *const RawHandle, text: *const c_char, text_len: i32, out: *mut i32, cap: i32) -> i32;
    fn ime_llm_kv_clear(h: *mut RawHandle);
    fn ime_llm_kv_seq_rm(h: *mut RawHandle, seq: i32, p0: i32, p1: i32) -> i32;
    fn ime_llm_kv_seq_cp(h: *mut RawHandle, src: i32, dst: i32, p0: i32, p1: i32);
    fn ime_llm_decode(
        h: *mut RawHandle,
        tokens: *const i32,
        pos: *const i32,
        seq: *const i32,
        want: *const u8,
        n: i32,
    ) -> i32;
    fn ime_llm_logprobs(h: *mut RawHandle, batch_index: i32, targets: *const i32, n_targets: i32, out: *mut f64)
        -> i32;
    fn ime_llm_phys_footprint() -> u64;
}

pub(crate) fn phys_footprint_bytes() -> Option<u64> {
    // SAFETY: 无参数、无前置条件的查询函数
    let bytes = unsafe { ime_llm_phys_footprint() };
    (bytes > 0).then_some(bytes)
}

/// llama.cpp 计算过程中反复调用的中止回调。只做原子读取，不会恐慌。
unsafe extern "C" fn abort_trampoline(user: *mut c_void) -> bool {
    if user.is_null() {
        return false;
    }
    // SAFETY: `user` 是 `LlamaBackend::cancel` 持有的 `Arc<CancelFlag>` 的地址，
    // 在回调被撤销（`Drop` / 重新安装）之前一直有效
    let flag = unsafe { &*(user as *const CancelFlag) };
    flag.should_abort()
}

pub(crate) struct LlamaBackend {
    handle: NonNull<RawHandle>,
    limits: BackendLimits,
    /// 中止回调读取的取消标记；必须活得比回调的注册更久
    cancel: Option<Arc<CancelFlag>>,
    buf_tokens: Vec<i32>,
    buf_pos: Vec<i32>,
    buf_seq: Vec<i32>,
    buf_want: Vec<u8>,
}

// SAFETY: 句柄只通过 `&mut self` 访问（外层有互斥锁），llama.cpp 的上下文可以在线程间移动；
// 中止回调读取的是原子变量
unsafe impl Send for LlamaBackend {}

impl LlamaBackend {
    pub fn load(model_path: &Path, options: &LlmOptions) -> Result<Self, LlmError> {
        // SAFETY: 无参数的版本查询
        let abi = unsafe { ime_llm_abi_version() };
        if abi != SHIM_ABI_VERSION {
            return Err(LlmError::Load(format!("垫片 ABI 版本不匹配：期望 {SHIM_ABI_VERSION}，实际 {abi}")));
        }
        let path_str = model_path
            .to_str()
            .ok_or_else(|| LlmError::Load(format!("模型路径不是合法的 UTF-8: {}", model_path.display())))?;
        if !model_path.is_file() {
            return Err(LlmError::Load(format!("模型文件不存在: {path_str}")));
        }
        let c_path = CString::new(path_str).map_err(|_| LlmError::Load("模型路径包含 NUL 字符".to_string()))?;

        let clamp = |v: u32| v.clamp(1, i32::MAX as u32) as i32;
        let mut flags = 0u32;
        if options.verbose {
            flags |= FLAG_VERBOSE;
        }
        if !options.use_mmap {
            flags |= FLAG_NO_MMAP;
        }
        if options.repack_weights {
            flags |= FLAG_REPACK;
        }
        let mut err = [0 as c_char; 512];
        // SAFETY: `c_path` 以 NUL 结尾；`err` 的容量如实传入，垫片保证写入以 NUL 结尾的字符串
        let raw = unsafe {
            ime_llm_load(
                c_path.as_ptr(),
                clamp(options.context_size),
                options.threads.min(i32::MAX as u32) as i32,
                options.gpu_layers,
                clamp(options.max_parallel),
                clamp(options.max_outputs),
                flags,
                err.as_mut_ptr(),
                err.len() as i32,
            )
        };
        let Some(handle) = NonNull::new(raw) else {
            // SAFETY: 垫片写入的内容以 NUL 结尾；数组初始全 0，即使未写入也是合法的空串
            let msg = unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned();
            return Err(LlmError::Load(if msg.is_empty() { "未知错误".to_string() } else { msg }));
        };
        // SAFETY: `handle` 刚由 `ime_llm_load` 返回，非空且有效
        let limits = unsafe {
            BackendLimits {
                n_ctx: ime_llm_n_ctx(handle.as_ptr()).max(0) as usize,
                n_batch: ime_llm_n_batch(handle.as_ptr()).max(0) as usize,
                n_seq_max: ime_llm_n_seq_max(handle.as_ptr()).max(0) as usize,
                n_outputs_max: ime_llm_n_outputs_max(handle.as_ptr()).max(0) as usize,
            }
        };
        Ok(Self {
            handle,
            limits,
            cancel: None,
            buf_tokens: Vec::new(),
            buf_pos: Vec::new(),
            buf_seq: Vec::new(),
            buf_want: Vec::new(),
        })
    }
}

impl Drop for LlamaBackend {
    fn drop(&mut self) {
        // SAFETY: 句柄有效且此后不再使用；先撤销回调，再释放
        unsafe {
            ime_llm_set_abort_callback(self.handle.as_ptr(), None, std::ptr::null_mut());
            ime_llm_free(self.handle.as_ptr());
        }
    }
}

impl LmBackend for LlamaBackend {
    fn limits(&self) -> BackendLimits {
        self.limits
    }

    fn tokenize(&mut self, text: &str) -> Result<Vec<i32>, LlmError> {
        if text.len() > i32::MAX as usize / 2 {
            return Err(LlmError::Tokenize("文本过长".to_string()));
        }
        // token 数不会超过字节数；留一点余量，不够时按返回的所需数量重试一次
        let mut out = vec![0i32; text.len() + 8];
        for _ in 0..2 {
            // SAFETY: 指针与长度来自同一个切片 / Vec，垫片不会越界写
            let n = unsafe {
                ime_llm_tokenize(
                    self.handle.as_ptr(),
                    text.as_ptr() as *const c_char,
                    text.len() as i32,
                    out.as_mut_ptr(),
                    out.len() as i32,
                )
            };
            if n >= 0 {
                out.truncate(n as usize);
                return Ok(out);
            }
            if n == i32::MIN {
                break;
            }
            out.resize(n.unsigned_abs() as usize, 0);
        }
        Err(LlmError::Tokenize("llama_tokenize 失败".to_string()))
    }

    fn kv_clear(&mut self) {
        // SAFETY: 句柄有效，独占访问
        unsafe { ime_llm_kv_clear(self.handle.as_ptr()) }
    }

    fn kv_seq_rm(&mut self, seq: i32, p0: i32, p1: i32) -> bool {
        // SAFETY: 句柄有效，独占访问；越界的序列号由垫片拒绝
        unsafe { ime_llm_kv_seq_rm(self.handle.as_ptr(), seq, p0, p1) != 0 }
    }

    fn kv_seq_cp(&mut self, src: i32, dst: i32, p0: i32, p1: i32) {
        // SAFETY: 同上
        unsafe { ime_llm_kv_seq_cp(self.handle.as_ptr(), src, dst, p0, p1) }
    }

    fn decode(&mut self, batch: &[BatchItem]) -> Result<DecodeOutcome, LlmError> {
        self.buf_tokens.clear();
        self.buf_pos.clear();
        self.buf_seq.clear();
        self.buf_want.clear();
        for item in batch {
            self.buf_tokens.push(item.token);
            self.buf_pos.push(item.pos);
            self.buf_seq.push(item.seq);
            self.buf_want.push(item.want_logits as u8);
        }
        // SAFETY: 四个数组长度都等于 `batch.len()`；垫片先做范围检查再交给 llama.cpp
        let r = unsafe {
            ime_llm_decode(
                self.handle.as_ptr(),
                self.buf_tokens.as_ptr(),
                self.buf_pos.as_ptr(),
                self.buf_seq.as_ptr(),
                self.buf_want.as_ptr(),
                batch.len().min(i32::MAX as usize) as i32,
            )
        };
        match r {
            DECODE_OK => Ok(DecodeOutcome::Ok),
            DECODE_NO_SLOT => Ok(DecodeOutcome::NoKvSlot),
            DECODE_ABORTED => Ok(DecodeOutcome::Aborted),
            DECODE_BAD_INPUT => Err(LlmError::Backend("llama_decode 拒绝了输入批（位置或序列号不一致）".to_string())),
            ERR_ARGS => Err(LlmError::Backend("解码批超出垫片层的容量检查".to_string())),
            other => Err(LlmError::Backend(format!("llama_decode 失败，返回值 {other}"))),
        }
    }

    fn logprobs(&mut self, batch_index: usize, targets: &[i32], out: &mut [f64]) -> Result<(), LlmError> {
        if out.len() < targets.len() || batch_index > i32::MAX as usize || targets.len() > i32::MAX as usize {
            return Err(LlmError::Backend("logprobs 参数不合法".to_string()));
        }
        // SAFETY: `out` 至少能容纳 `targets.len()` 个元素；行号与 token 范围由垫片检查
        let r = unsafe {
            ime_llm_logprobs(
                self.handle.as_ptr(),
                batch_index as i32,
                targets.as_ptr(),
                targets.len() as i32,
                out.as_mut_ptr(),
            )
        };
        if r == 0 {
            Ok(())
        } else {
            Err(LlmError::Backend(format!("读取第 {batch_index} 行 logits 失败，返回值 {r}")))
        }
    }

    fn install_cancel(&mut self, flag: Arc<CancelFlag>) {
        let user = Arc::as_ptr(&flag) as *mut c_void;
        // SAFETY: 句柄有效；`flag` 随后存入 `self.cancel`，生命周期覆盖回调的整个注册期
        unsafe { ime_llm_set_abort_callback(self.handle.as_ptr(), Some(abort_trampoline), user) };
        // 旧标记在新回调生效之后才释放
        self.cancel = Some(flag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 垫片对（上下文长度，并行序列数，logits 行数）的收敛结果
    fn resolve(n_ctx: i32, n_seq_max: i32, n_outputs_max: i32) -> Option<(i32, i32)> {
        let (mut seq, mut outputs) = (n_seq_max, n_outputs_max);
        // SAFETY: 两个指针指向本函数栈上的有效整数
        let ok = unsafe { ime_llm_resolve_limits(n_ctx, &mut seq, &mut outputs) };
        (ok == 1).then_some((seq, outputs))
    }

    /// llama.cpp 创建上下文时断言 `max(n_outputs_max, n_seq_max) <= n_outputs_max`，失败即 abort 整个进程：
    /// 垫片收敛后的参数必须对任何输入都满足 `n_seq_max <= n_outputs_max <= n_ctx`
    #[test]
    fn shim_limits_never_ask_for_more_sequences_than_outputs() {
        // 默认配置原样通过
        assert_eq!(resolve(256, 8, 8), Some((8, 8)));
        // 上下文比并行序列数还小：序列数跟着降下来（原先得到 n_seq_max 8、n_outputs_max 4，加载时进程中止）
        assert_eq!(resolve(4, 8, 8), Some((4, 4)));
        assert_eq!(resolve(1, 8, 1), Some((1, 1)));
        // logits 行数不小于并行序列数
        assert_eq!(resolve(256, 8, 1), Some((8, 8)));
        assert_eq!(resolve(256, 2, 5), Some((2, 5)));
        // 也不超过上下文长度
        assert_eq!(resolve(16, 4, 100), Some((4, 16)));
        for n_ctx in [1, 2, 3, 4, 7, 8, 9, 63, 64, 256, 4096] {
            for n_seq in [1, 2, 7, 8, 9, 64, 1000, i32::MAX] {
                for n_out in [1, 2, 8, 9, 64, 1000, i32::MAX] {
                    let (seq, outputs) = resolve(n_ctx, n_seq, n_out).expect("参数合法");
                    assert!(1 <= seq && seq <= outputs && outputs <= n_ctx, "({n_ctx}, {n_seq}, {n_out}) -> ({seq}, {outputs})");
                    assert!(seq <= n_seq, "序列数只会被调小");
                }
            }
        }
        // 非正数参数被拒绝
        assert_eq!(resolve(0, 8, 8), None);
        assert_eq!(resolve(256, 0, 8), None);
        assert_eq!(resolve(256, 8, -1), None);
    }

    /// 像 llama.cpp 那样问一次中止回调；传的地址与 `install_cancel` 注册回调时相同
    fn ask_abort(flag: &Arc<CancelFlag>) -> bool {
        // SAFETY: 指针指向 `flag` 持有的 `CancelFlag`，调用期间一直有效
        unsafe { abort_trampoline(Arc::as_ptr(flag) as *mut c_void) }
    }

    /// llama.cpp 计算中途反复调用的中止回调只看 `should_abort`，不看 `is_cancelled`：已取消的请求在不可中止区段
    /// （公共前缀的每一块）里照常算完，离开区段后才中止。引擎与假后端的测试都不经过这个回调，
    /// 所以在这里直接调用它（不需要模型）
    #[test]
    fn abort_trampoline_aborts_only_when_should_abort_says_so() {
        let flag = Arc::new(CancelFlag::new());
        flag.begin(flag.ticket());
        assert!(!ask_abort(&flag), "没有被取消的请求不中止");

        // 请求被取消，但在不可中止区段里：这一块照常算完
        flag.cancel();
        flag.set_shielded(true);
        assert!(flag.is_cancelled());
        assert!(!ask_abort(&flag), "不可中止区段里即使已被取消也不中止");
        // 离开区段：同一个取消让计算中止
        flag.set_shielded(false);
        assert!(ask_abort(&flag), "不可中止区段外，已被取消的请求应当中止");

        // `user` 是空指针（`install_cancel` 不会这样注册，回调里是防御性的判空）：不中止
        // SAFETY: 回调先判空，空指针不会被解引用
        assert!(!unsafe { abort_trampoline(std::ptr::null_mut()) }, "空指针不中止");
    }
}
