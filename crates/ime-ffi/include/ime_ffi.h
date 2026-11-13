// ime-ffi：输入法引擎（ime-session）的 C 接口，给 iOS 键盘扩展等非 Rust 宿主用。
//
// 约定：
// - 除 ime_engine_free 之外，所有函数都可以在任意线程调用，引擎内部自己加锁。
//   ime_engine_free 不能与同一个引擎上的任何其他调用并发，调用之后也不能再用这个引擎。
//   回调可能发生在引擎的后台线程上，回调里不要再调用本接口（会死锁），把事情转到自己的线程去做。
// - 返回 char * 的函数返回一段 UTF-8 JSON，用完必须交给 ime_string_free。
//   出错时也返回合法 JSON（{"action":"pass"}、空闲视图或空的候选列表），不会返回 NULL，除非 engine 本身是 NULL。
// - 按键类函数返回「信封」：
//       {"action":"update","view":{…}}                        引擎吃掉了按键，按新视图刷新
//       {"action":"commit","text":"你好","ghost":null,"view":{…}}  把 text 写进输入框，再按 view 刷新
//       {"action":"pass"}                                     引擎不处理，按键交还给键盘
//   view 的字段见 crates/ime-ffi/src/view.rs（CandidateWindowViewModel 的精简版）。
// - 分页：触屏键盘的候选栏是一条可以一直滚动的列表，高亮的始终是全部候选里的第 0 个，所以让引擎停在第 0 页：
//   「加载更多」用 ime_engine_candidates（不改引擎状态），不要用 IME_KEY_PAGE_DOWN。
//   IME_KEY_PAGE_UP / IME_KEY_PAGE_DOWN 只移动视图里 visible_candidates 所在的那一页；
//   IME_KEY_SPACE 和 ime_engine_refresh_after_ai 都会先回到第 0 页。
// - 正在输入（ime_engine_is_composing）时，IME_KEY_SPACE、IME_KEY_ENTER、ime_engine_commit_full_sentence
//   都不会返回 pass，其中后两个一定返回 commit。
#ifndef IME_FFI_H
#define IME_FFI_H

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// 2：新增 ime_engine_candidates、ime_engine_reload_user_dict；空格和 refresh_after_ai 先回到第 0 页；
//    正在输入时整句上屏一定上屏、光标键不再 pass；load_llm 先卸载已有模型；拼音串有长度上限
#define IME_FFI_ABI_VERSION 2

typedef struct ime_engine ime_engine;

// ime_engine_key 的 kind
enum {
    // value 是 Unicode 码点。中文模式下：小写字母进拼音串；大写字母在输入中途进拼音串（中英混输的词边界），
    // 空闲时 pass；单引号 ' 在输入中途是分词符（返回 update），空闲时 pass；其他字符（数字、标点、非 ASCII、
    // 非法码点）一律 pass。英文模式下全部 pass。
    // 拼音串最长 100 字节：到了上限，再来的字母和分词符被吃掉（返回 update，视图不变）。
    IME_KEY_CHAR = 0,
    // 提交全部候选里的第 0 个（先回到第 0 页，与之前有没有翻页无关）。第 0 个若只覆盖前一部分拼音，
    // 返回 update，剩下的继续选；没有任何候选时与 IME_KEY_ENTER 相同。空闲时 pass。
    IME_KEY_SPACE = 1,
    IME_KEY_ENTER = 2,          // 已选定的词 + 剩余字母原样上屏；空闲时 pass
    IME_KEY_BACKSPACE = 3,
    IME_KEY_TOGGLE_ENGLISH = 4, // 切换中 / 英文；输入中途切换会先把原样字母上屏
    IME_KEY_ESCAPE = 5,
    IME_KEY_PAGE_UP = 6,        // 这两个只移动视图里返回的那一页（见上面「分页」）
    IME_KEY_PAGE_DOWN = 7,
    IME_KEY_CURSOR_LEFT = 8,    // 拼音光标按音节左移。正在输入时这两个键一定返回 update，空闲时 pass
    IME_KEY_CURSOR_RIGHT = 9,   // 拼音光标按音节右移，移过最后一个音节回到整句
};

int32_t ime_ffi_abi_version(void);

// 创建引擎。lexicon_path 是 AIMELX03 词库文件（只读 mmap，不占进程内存配额）。
// user_dict_path 可为 NULL（个人词频只留在内存里）；否则从该文件读取，每次输入结束后由后台线程写回
// （先写同目录下的临时文件再改名；写入失败只在标准错误上报告一次，输入不受影响）。
// 两个路径都必须是 UTF-8；user_dict_path 非 NULL 却不是 UTF-8 时创建失败，不会悄悄当成 NULL。
// 失败返回 NULL，*error_out（可为 NULL）得到一段说明，用 ime_string_free 释放。
ime_engine *ime_engine_new(const char *lexicon_path, const char *user_dict_path, char **error_out);
// 销毁引擎（会等个人词库写完，最多 2 秒）。不能与这个引擎上的任何其他调用并发；
// 调用时不得有别的线程正在使用它，之后也不能再用。返回后两个回调都不会再被调用。
void ime_engine_free(ime_engine *engine);

char *ime_engine_key(ime_engine *engine, int32_t kind, uint32_t value);
// 点选候选，global_index 是全部候选里的下标（与 ime_engine_candidates 的下标一致）
char *ime_engine_select_candidate(ime_engine *engine, uint32_t global_index);
// 点击预编辑栏的第 index 个音节；再点一次同一个音节回到整句。
// 在点的音节上选字之后回到整句（选到句尾且整句都已选定时直接上屏）；点着已选定的音节按退格，撤销那里的选字
char *ime_engine_tap_syllable(ime_engine *engine, uint32_t index);
// 把当前整句预览上屏；没有输入时返回 pass。正在输入时一定返回 commit：解码不出整句时
// （在已选定的片段里再次改选之后可能没有任何候选），上屏的是「已选定的词 + 剩余字母原样」，与 IME_KEY_ENTER 相同
char *ime_engine_commit_full_sentence(ime_engine *engine);

// 当前视图（只有 view 对象，没有信封）
char *ime_engine_view(ime_engine *engine);
// 不改变引擎分页状态地取全部候选里的 [offset, offset + count)。返回
//   {"total":86,"offset":30,"candidates":[{"text":…,"preedit":…,"consumed_len":…,"tag":…,"ai_boosted":…},…]}
// 没有输入时 total 为 0、candidates 为空。键盘的候选栏用它「加载更多」，引擎始终停在第 0 页。
// candidates 里的对象与视图 visible_candidates 里的相同；offset 原样返回，超出 total 时 candidates 为空；
// count 超过 200 按 200 算。
char *ime_engine_candidates(ime_engine *engine, uint32_t offset, uint32_t count);
// 异步 AI 结果到达后调用：重排候选，返回新视图。引擎先回到第 0 页再重排，所以返回的总是第 0 页，
// 宿主此前用 PAGE_DOWN 翻到后面也照常应用结果。光标停在某个音节上时列表是该音节的候选，不受影响
char *ime_engine_refresh_after_ai(ime_engine *engine);

// 光标前的文本，作为解码和 AI 选词的上文。NULL 表示拿不到（退回引擎自己的上屏历史）
void ime_engine_set_context(ime_engine *engine, const char *text_before_cursor);
// 放弃当前输入，不上屏
void ime_engine_reset(ime_engine *engine);
bool ime_engine_is_composing(const ime_engine *engine);
bool ime_engine_is_english(const ime_engine *engine);
// 最近一次按键是否还在等本地模型 / 宿主选词的结果。以下情况变回 false：结果被采纳（随即 ai_notify 回调）、
// 本地模型打分失败、宿主答复 NULL 或答复不在候选里、请求作废（新的按键不需要结果、ime_engine_reset、上屏、
// 加载 / 卸载模型；等的是宿主选词时，安装 / 卸载选词器也会作废它）；宿主选词请求发出后超过 4 秒还没有答复，
// 也变回 false。同一输入之后再重排（ime_engine_refresh_after_ai、移动光标）会重新发出请求，又变回 true。
// 超过 4 秒才送达的答复一律丢弃：这次请求还登记着时得到 IME_CHOICE_TIMED_OUT；同一输入重新发出了请求、
// 输入变了 / 结束了（或者上面其他作废的情况）之后，旧请求作废，宿主收到它的作废通知（request_json 为 NULL），
// 此后再送达的答复得到 IME_CHOICE_STALE。engine 为 NULL 返回 false。
// 界面用不着它（等 ai_notify 回调即可）；评测和测试用它判断什么时候读结果
bool ime_engine_ai_pending(const ime_engine *engine);

// 应用设置。JSON 里的字段都可省略：
//   {"fuzzy":{"z_zh":true,"c_ch":true,"s_sh":true,"n_l":false,"f_h":false,
//             "in_ing":true,"en_eng":true,"an_ang":true,"typo_correction":true},
//    "page_size":30,
//    "scene":"General",          General | CodeEditor | Chat | Academic | Document
//    "context_memory":true,      用户在某段上文下主动改选过的词，下次同样的上文直接排首位（默认开）
//    "emoji":true,               常用词候选（中文词，和英文词库里的 ok、lol 等）后面跟上对应的表情
//                                （默认开；表情候选的 tag 是 "[表情]"，选中只上屏表情）
//    "memory_blocks_model":false, 个人记忆首选是否连本地模型也不问（默认否：记忆与模型分数一起裁决）
//    "chooser_memory":"taught"   个人记忆首选遇到宿主选词器时：blocked = 不问，contested = 候选里至少两个词
//                                  都有近期个人记忆才问，always = 有上文就问，taught = 首选曾被主动改选过、
//                                  记忆强度仍在门槛以上、而且没有争议时不问，其余有上文就问（默认）
//   }
// 成功返回 0，JSON 解析失败返回 -1（设置保持不变）。
int32_t ime_engine_configure(ime_engine *engine, const char *settings_json);

// 个人词库文件若被别的键盘进程改过（修改时间或大小与本引擎上次读写时不同）就重新读入。
// 返回 1 = 重新读入了，0 = 没有变化或没有启用持久化，-1 = 读取失败（保留内存里的词库）。键盘每次出现时调用。
// 在调用线程上做文件 I/O：通常只是一次 stat；真要重新读入时，20,000 条记录的词库约 15 ms。
// 本引擎已经交给后台线程的写入会先写完再比较；文件被删掉按「变成空词库」重新读入（返回 1）；
// 正在输入时按新词库重算候选（之后用 ime_engine_view 取视图）。engine 为 NULL 返回 -1。
int32_t ime_engine_reload_user_dict(ime_engine *engine);

// ---------------------------------------------------------------------------
// AI 选词。规则重排始终开启、同步完成；下面两种「大模型」后端的结果异步到达，二选一：
//   本地模型：llama.cpp 在本进程内给候选打分（ime_engine_load_llm）
//   宿主选词：引擎把上文、拼音、候选交给宿主去选（ime_engine_set_chooser），iOS 上接 Apple 的端侧模型
// 两个都装上时只用本地模型。
// ---------------------------------------------------------------------------

// 异步 AI 结果就绪时回调。本地模型的结果在引擎的后台线程上触发，宿主选词的结果在调用
// ime_engine_deliver_choice 的线程上触发；不要依赖具体是哪个线程，收到后转到自己的线程调用
// ime_engine_refresh_after_ai。只有当前输入还在等这个结果时才回调（见 IME_CHOICE_ACCEPTED）。
// callback 传 NULL 取消。本函数返回后，旧回调保证不会再被调用。
typedef void (*ime_notify_callback)(void *user);
void ime_engine_set_ai_notify(ime_engine *engine, ime_notify_callback callback, void *user);

// 这个构建是否包含本地模型支持
bool ime_llm_supported(void);
// 加载 GGUF 模型（只读 mmap，权重不占进程内存配额）。在调用线程上完成，耗时几十到几百毫秒，
// 不要在主线程调用；加载期间不占引擎的锁，按键不受影响。threads <= 0 表示自动。
// 已经装有模型时，先卸载并释放旧的再加载新的，两份模型不会同时驻留（Mac 上实测：单个模型约 66 MiB，
// 以前先加载后替换时切换瞬间约 134 MiB）。所以加载失败之后引擎里没有模型（ime_engine_llm_loaded 为 false），
// 不会留着旧的。旧模型若正在打分，最多等 2 秒让它放手。并发的加载排队执行。
// 成功返回 0；失败返回 -1，*error_out（可为 NULL）得到说明，用 ime_string_free 释放。
int32_t ime_engine_load_llm(ime_engine *engine, const char *model_path, int32_t threads, char **error_out);
// 卸载本地模型（正在进行的打分会被取消）。可以在主线程调用：释放在引擎的锁之外进行，约几到十几毫秒。
// 释放的是模型的上下文和缓冲区；若此刻正有一次打分在进行，要等它注意到取消之后才在后台线程上释放
// （Mac 上实测：卸载返回时 84–88 MiB，0.5–2 秒后回落）。
// phys_footprint 不会回到加载前：Mac 上实测基线 3–6 MiB，加载后约 63 MiB，第一次卸载后约 17 MiB，
// 反复加载 / 卸载几轮后稳定在约 41–46 MiB；留下的内存下次加载会复用，不会无限增长。
void ime_engine_unload_llm(ime_engine *engine);
bool ime_engine_llm_loaded(const ime_engine *engine);

// 宿主选词器。request_json 非 NULL 时是一次请求：
//     {"id":7,"context":"上文","pinyin":"shiyan","candidates":["实验","誓言"]}
// 候选的第 0 项是当前首选。宿主选好后调用 ime_engine_deliver_choice(engine, id, 选中的文本)，
// 选不出来就传 NULL。request_json 为 NULL 表示编号 id 的请求已经作废，可以中止对应的计算。
// 回调在引擎的后台线程上执行；作废通知（request_json 为 NULL）也可能在调用 ime_engine_set_chooser、
// ime_engine_load_llm 的线程上、在那次调用返回之前执行。request_json 只在回调期间有效。
// 作废通知可能先于同一编号的请求到达（请求发出的瞬间恰好被作废）：那个请求的答复只会得到 IME_CHOICE_STALE。
// callback 传 NULL 卸载；传新的回调是替换：等待中的请求作废，作废通知发给旧回调，新回调只收到之后的请求。
// 本函数返回后，旧回调保证不会再被调用。
typedef void (*ime_chooser_callback)(void *user, uint64_t id, const char *request_json);
void ime_engine_set_chooser(ime_engine *engine, ime_chooser_callback callback, void *user);

enum {
    // 采纳：结果已记下。当前输入若还在等它，触发 ai_notify 回调，候选在随后的 ime_engine_refresh_after_ai 里重排；
    // 输入已经变了或结束了则不回调，结果留在缓存里，下次同样的上文和拼音直接用上
    IME_CHOICE_ACCEPTED = 0,
    IME_CHOICE_NO_ANSWER = 1,   // 宿主没有结果
    IME_CHOICE_IMPLAUSIBLE = 2, // 文本不在这次请求的候选里，丢弃
    IME_CHOICE_STALE = 3,       // 不是当前等待中的请求（已被新的请求取代、已经作废或答复过），丢弃
    IME_CHOICE_TIMED_OUT = 4,   // 超过 4 秒才送达、这次请求还登记着，丢弃（已经作废的请求得到 STALE）
};
int32_t ime_engine_deliver_choice(ime_engine *engine, uint64_t id, const char *text);

// 当前进程的 phys_footprint（字节），也就是 iOS 用来判定键盘扩展是否超限的那个数。非 Apple 平台返回 0
uint64_t ime_phys_footprint(void);

void ime_string_free(char *string);

#ifdef __cplusplus
}
#endif

#endif // IME_FFI_H
