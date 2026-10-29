//! `ime-ai`: 双模「AI 理解选词」与异步无阻塞并发调度引擎
//!
//! 同时支持：
//! 1. **端侧极速模式（默认，`<0.15ms`）**：基于 KV-Cache 上下文状态预热 + 单次前向 N-Best 重打分 + 句内同音搭配矩阵 + 拼音 Trie 掩码约束 (`PinyinLogitsMask`)
//! 2. **本地/云端大模型模式（可选，后台异步无阻塞）**：
//!    - 本地最优解：`Qwen2.5-1.5B-Instruct` / `Qwen2.5-0.5B-Instruct`（Ollama / llama.cpp）
//!    - 主流云端大模型：**OpenAI (ChatGPT)**、**Anthropic (Claude)**、**Google (Gemini)**
//!    - **自定义云端 (Custom Cloud)**：DeepSeek、硅基流动、阿里云百炼、OpenRouter 等

pub mod local_engine;
pub mod managed_llm;
pub mod openai_client;
pub mod pinyin_mask;

pub use local_engine::{AiRerankReport, CachedContextState, OnDeviceAiEngine};
pub use managed_llm::{BuiltinModelTier, ManagedLocalLlm, MANAGED_LLM_BASE_URL, MANAGED_LLM_PORT};
pub use openai_client::{
    AiProvider, ChatCompletionRequest, LocalEndpointStatus, OpenAiCompatibleClient,
    OpenAiCompatibleConfig, ProviderProfile,
};
pub use pinyin_mask::{PinyinLogitsMask, VocabTokenEntry};

use ime_core::{AppScene, Candidate};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

struct AsyncRerankJob {
    gen_id: u64,
    cache_key: String,
    pinyin_input: String,
    surrounding_context: String,
    scene: AppScene,
    candidates: Vec<Candidate>,
    config: OpenAiCompatibleConfig,
}

/// 带版本号防抖、后台异步推理与结果缓存的双模 AI 选词调度器
///
/// 关键设计：
/// Windows `WH_KEYBOARD_LL` 低级键盘钩子严禁在回调线程执行耗时网络 I/O。
/// 因此当开启本地 Ollama 或云端 ChatGPT/Claude/Gemini 时，击键线程仅做 `<0.15ms` 的端侧重排与缓存查表；
/// 未命中的大模型请求通过 `mpsc` 投递给后台常驻线程，推理完成后写入 `llm_cache` 并触发 UI 刷新回调。
pub struct DualModeAiOrchestrator {
    pub local_engine: OnDeviceAiEngine,
    pub cloud_client: OpenAiCompatibleClient,
    generation_counter: Arc<AtomicU64>,
    llm_cache: Arc<Mutex<HashMap<String, (String, AiProvider)>>>,
    async_tx: mpsc::Sender<AsyncRerankJob>,
    notify_cb: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
}

impl Default for DualModeAiOrchestrator {
    fn default() -> Self {
        Self::new(OpenAiCompatibleConfig::default())
    }
}

impl DualModeAiOrchestrator {
    pub fn new(cloud_config: OpenAiCompatibleConfig) -> Self {
        let generation_counter = Arc::new(AtomicU64::new(0));
        let llm_cache: Arc<Mutex<HashMap<String, (String, AiProvider)>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let notify_cb: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>> =
            Arc::new(Mutex::new(None));

        let (async_tx, rx) = mpsc::channel::<AsyncRerankJob>();
        let gen_ref = Arc::clone(&generation_counter);
        let cache_ref = Arc::clone(&llm_cache);
        let cb_ref = Arc::clone(&notify_cb);

        std::thread::spawn(move || {
            while let Ok(mut job) = rx.recv() {
                // 若用户在模型思考期间继续快速打字，直接跳过积压的旧拼音，只推理最新一帧
                while let Ok(newer) = rx.try_recv() {
                    job = newer;
                }
                if gen_ref.load(Ordering::SeqCst) != job.gen_id {
                    continue;
                }
                let client = OpenAiCompatibleClient::new(job.config.clone());
                if let Ok(best_word) = client.query_best_candidate_http(
                    &job.pinyin_input,
                    &job.surrounding_context,
                    job.scene,
                    &job.candidates,
                ) {
                    if !best_word.is_empty() {
                        if let Ok(mut guard) = cache_ref.lock() {
                            if guard.len() > 512 {
                                guard.clear();
                            }
                            guard.insert(job.cache_key, (best_word, job.config.provider));
                        }
                        if gen_ref.load(Ordering::SeqCst) == job.gen_id {
                            if let Ok(cb_guard) = cb_ref.lock() {
                                if let Some(cb) = cb_guard.as_ref() {
                                    cb();
                                }
                            }
                        }
                    }
                }
            }
        });

        Self {
            local_engine: OnDeviceAiEngine::new(),
            cloud_client: OpenAiCompatibleClient::new(cloud_config),
            generation_counter,
            llm_cache,
            async_tx,
            notify_cb,
        }
    }

    /// 注册当后台大模型完成选词仲裁时的 UI 唤醒回调（例如向候选窗发送 `PostMessageW` 触发重绘）
    pub fn set_async_notify_callback(&self, cb: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut guard) = self.notify_cb.lock() {
            *guard = Some(cb);
        }
    }

    /// 递增并返回最新击键版本号（用于取消未完成的旧请求）
    pub fn next_generation(&self) -> u64 {
        self.generation_counter.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn current_generation(&self) -> u64 {
        self.generation_counter.load(Ordering::SeqCst)
    }

    /// 当用户主动选词或确认词条时，清除该拼音编码对应的旧大模型缓存，确保用户个人高频记忆立即生效
    pub fn invalidate_cache_for_pinyin(&self, pinyin_input: &str) {
        let suffix = format!("\t{pinyin_input}");
        if let Ok(mut guard) = self.llm_cache.lock() {
            guard.retain(|k, _| !k.ends_with(&suffix));
        }
    }

    /// 执行上下文感知选词重排：
    /// 1. 立即执行端侧微模型毫秒级重排（`<0.15ms`）
    /// 2. 若开启了本地 Qwen / 云端大模型，先查 `llm_cache` 是否已有该上下文+拼音的仲裁结果；
    ///    若未命中，则异步投递给后台推理线程，绝不阻塞当前击键！
    pub fn rerank(
        &mut self,
        pinyin_input: &str,
        surrounding_context: &str,
        scene: AppScene,
        l1_candidates: Vec<Candidate>,
    ) -> AiRerankReport {
        let gen_id = self.next_generation();
        let mut report = self
            .local_engine
            .rerank_candidates(surrounding_context, scene, l1_candidates);

        // 若首选已经是用户个人记忆词（`[记忆·X次]`），始终尊重用户个人词频记忆，不让大模型覆盖
        let top_is_user_memory = report
            .candidates
            .first()
            .is_some_and(|c| c.tag.starts_with("[记忆"));
        // 若输入仅为单音节单字（如单独敲 `lan`、`qiu`、`tuo`），严格保持常用汉字字频排序，不调用异步大模型把生僻字置顶
        let is_single_char_lookup = report
            .candidates
            .first()
            .is_some_and(|c| c.text.chars().count() <= 1 && !c.text.is_ascii());
        let has_context_or_clause =
            !surrounding_context.trim().is_empty() || pinyin_input.len() > 8;

        if self.cloud_client.config.enabled
            && !top_is_user_memory
            && !is_single_char_lookup
            && has_context_or_clause
            && pinyin_input.len() >= 2
            && report.candidates.len() >= 2
        {
            let plausible = OpenAiCompatibleClient::filter_plausible_candidates(&report.candidates);
            if plausible.len() >= 2 {
                let cache_key = format!(
                    "{}:{}:{}\t{}",
                    self.cloud_client.config.provider.key_id(),
                    self.cloud_client.config.model,
                    surrounding_context,
                    pinyin_input
                );

                let cached_hit = self
                    .llm_cache
                    .lock()
                    .ok()
                    .and_then(|g| g.get(&cache_key).cloned());

                if let Some((best_word, provider)) = cached_hit {
                    // 仅当缓存词属于合理候选池时才采纳，防止历史脏缓存把生造拼凑词置顶
                    let is_plausible_pick = plausible.iter().any(|c| c.text == best_word);
                    if is_plausible_pick {
                        if let Some(pos) = report.candidates.iter().position(|c| c.text == best_word)
                        {
                            let tag = format!("[{} 优选]", provider.short_tag());
                            if pos > 0 {
                                let mut picked = report.candidates.remove(pos);
                                picked.ai_boosted = true;
                                picked.tag = tag;
                                report.candidates.insert(0, picked);
                                report.top_changed = true;
                            } else {
                                report.candidates[0].ai_boosted = true;
                                report.candidates[0].tag = tag;
                            }
                        }
                    }
                } else {
                    // 异步投递给后台线程推理（仅传递过滤后的合理候选池）
                    let _ = self.async_tx.send(AsyncRerankJob {
                        gen_id,
                        cache_key,
                        pinyin_input: pinyin_input.to_string(),
                        surrounding_context: surrounding_context.to_string(),
                        scene,
                        candidates: plausible,
                        config: self.cloud_client.config.clone(),
                    });
                }
            }
        }

        // 在完成端侧 AI 与大模型候选池提取后，剔除未夺冠的 `[整句备选]`，
        // 确保候选框第 2~5 项始终是干净的递进短语与前缀词组（如「现在再研究」「现在在」「现在」「先在」）。
        // 注意只按标签剔除：若 AI 推翻了 L1 首选整句，被换下来的原首选整句必须保留在第 2 位，
        // 防止 AI 判断失误时用户再也选不到原本的整句。
        if report.candidates.len() > 1 {
            let mut idx = 0usize;
            report.candidates.retain(|c| {
                let is_first = idx == 0;
                idx += 1;
                is_first || c.tag != "[整句备选]"
            });
        }

        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ime_core::ImeCoreEngine;
    use ime_lexicon::CompiledLexicon;
    use std::path::PathBuf;

    #[test]
    fn test_ai_contextual_disambiguation() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let lexicon = CompiledLexicon::load_or_build_default(root).unwrap();
        let core = ImeCoreEngine::new(lexicon);
        let mut ai = DualModeAiOrchestrator::default();

        // 场景1：微积分语境下输入 "jishu" -> AI 应将 "级数" 排在第一位
        let l1_math = core.decode("jishu", AppScene::Academic, "这道微积分题需要展开成傅里叶");
        let l2_math = ai.rerank(
            "jishu",
            "这道微积分题需要展开成傅里叶",
            AppScene::Academic,
            l1_math.candidates,
        );
        assert_eq!(l2_math.candidates[0].text, "级数");

        // 场景2：软件工程语境下输入 "jishu" -> AI 应将 "技术" 排在第一位
        let l1_code = core.decode("jishu", AppScene::CodeEditor, "我们团队正在攻关核心计算机");
        let l2_code = ai.rerank(
            "jishu",
            "我们团队正在攻关核心计算机",
            AppScene::CodeEditor,
            l1_code.candidates,
        );
        assert_eq!(l2_code.candidates[0].text, "技术");

        // 场景3：释怀语境 vs 喜剧语境下输入 "buzaiyile" -> "不在意了" vs "不再一乐"
        let l1_sad = core.decode("buzaiyile", AppScene::Chat, "算了，过去的事我已经");
        let l2_sad = ai.rerank(
            "buzaiyile",
            "算了，过去的事我已经",
            AppScene::Chat,
            l1_sad.candidates,
        );
        assert_eq!(l2_sad.candidates[0].text, "不在意了");

        let l1_happy = core.decode("buzaiyile", AppScene::Chat, "听完这段搞笑相声怎么");
        let l2_happy = ai.rerank(
            "buzaiyile",
            "听完这段搞笑相声怎么",
            AppScene::Chat,
            l1_happy.candidates,
        );
        assert_eq!(l2_happy.candidates[0].text, "不再一乐");

        // 场景4：扩展同音词搭配矩阵测试（权利 vs 权力，登录 vs 登陆）
        let l1_rights = core.decode("quanli", AppScene::Document, "宪法保障公民的合法");
        let l2_rights = ai.rerank(
            "quanli",
            "宪法保障公民的合法",
            AppScene::Document,
            l1_rights.candidates,
        );
        assert_eq!(l2_rights.candidates[0].text, "权利");

        let l1_power = core.decode("quanli", AppScene::Document, "坚决防止国家机关滥用");
        let l2_power = ai.rerank(
            "quanli",
            "坚决防止国家机关滥用",
            AppScene::Document,
            l1_power.candidates,
        );
        assert_eq!(l2_power.candidates[0].text, "权力");

        let l1_login = core.decode("denglu", AppScene::CodeEditor, "请输入账号密码扫码");
        let l2_login = ai.rerank(
            "denglu",
            "请输入账号密码扫码",
            AppScene::CodeEditor,
            l1_login.candidates,
        );
        assert_eq!(l2_login.candidates[0].text, "登录");

        let l1_typhoon = core.decode("denglu", AppScene::General, "超强台风即将在东南沿海");
        let l2_typhoon = ai.rerank(
            "denglu",
            "超强台风即将在东南沿海",
            AppScene::General,
            l1_typhoon.candidates,
        );
        assert_eq!(l2_typhoon.candidates[0].text, "登陆");
    }

    fn synthetic_cand(text: &str, score: f32, segments: &[&str]) -> Candidate {
        Candidate {
            text: text.to_string(),
            preedit: String::new(),
            consumed_len: 10,
            l1_cost: 100.0 - score,
            final_score: score,
            ai_boosted: false,
            tag: String::new(),
            segments: segments.iter().map(|s| s.to_string()).collect(),
            segment_spans: Vec::new(),
            syllable_spans: Vec::new(),
        }
    }

    #[test]
    fn test_collocation_rules_do_not_misfire_on_single_char_triggers() {
        let mut ai = DualModeAiOrchestrator::default();
        // 「侦查机关」含「机」字，不应被误判为军事侦察语境而变成「侦察机关」
        let r = ai.rerank(
            "zhenchajiguan",
            "",
            AppScene::General,
            vec![
                synthetic_cand("侦查机关", 99.0, &["侦查", "机关"]),
                synthetic_cand("侦察机关", 98.5, &["侦察", "机关"]),
            ],
        );
        assert_eq!(r.candidates[0].text, "侦查机关");
        // 上文含「产品」时，「必须」不应被误改为「必需」
        let r2 = ai.rerank(
            "bixu",
            "这款产品",
            AppScene::General,
            vec![
                synthetic_cand("必须", 99.0, &["必须"]),
                synthetic_cand("必需", 98.5, &["必需"]),
            ],
        );
        assert_eq!(r2.candidates[0].text, "必须");
        // 上文含「用户」时，「爆发」不应被误改为「暴发」
        let r3 = ai.rerank(
            "baofa",
            "很多用户的不满终于",
            AppScene::General,
            vec![
                synthetic_cand("爆发", 99.0, &["爆发"]),
                synthetic_cand("暴发", 98.5, &["暴发"]),
            ],
        );
        assert_eq!(r3.candidates[0].text, "爆发");
    }

    #[test]
    fn test_ai_flip_keeps_original_top_sentence_as_alternative() {
        let mut ai = DualModeAiOrchestrator::default();
        // L1 首选「现在在研究一下」被端侧语法 AI 推翻为「现在再研究一下」后，原首选整句仍应保留可选
        let mut backup = synthetic_cand("现在再研究一下", 90.0, &["现在", "再", "研究", "一下"]);
        backup.tag = "[整句备选]".to_string();
        let mut noise = synthetic_cand("现在载研究一下", 89.0, &["现在", "载", "研究", "一下"]);
        noise.tag = "[整句备选]".to_string();
        let mut prefix = synthetic_cand("现在", 80.0, &["现在"]);
        prefix.consumed_len = 6;
        let r = ai.rerank(
            "xianzaizaiyanjiuyixia",
            "",
            AppScene::General,
            vec![
                synthetic_cand("现在在研究一下", 99.0, &["现在", "在", "研究", "一下"]),
                backup,
                noise,
                prefix,
            ],
        );
        let texts: Vec<&str> = r.candidates.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts[0], "现在再研究一下");
        assert!(texts.contains(&"现在在研究一下"), "被 AI 换下的原首选整句必须保留: {texts:?}");
        assert!(!texts.contains(&"现在载研究一下"), "未夺冠的整句备选应被剔除: {texts:?}");
        assert!(texts.contains(&"现在"));
    }

    #[test]
    fn test_multi_provider_config_switching() {
        let mut cfg = OpenAiCompatibleConfig::default();
        assert_eq!(cfg.provider, AiProvider::LocalQwen);
        assert_eq!(cfg.model, "qwen2.5:1.5b");

        cfg.switch_provider(AiProvider::OpenAiChatGpt);
        assert_eq!(cfg.api_base, "https://api.openai.com/v1");
        assert_eq!(cfg.model, "gpt-4o-mini");
        cfg.api_key = "sk-test-openai".to_string();

        cfg.switch_provider(AiProvider::AnthropicClaude);
        assert_eq!(cfg.api_base, "https://api.anthropic.com/v1");
        assert_eq!(cfg.model, "claude-3-5-haiku-latest");
        cfg.api_key = "sk-ant-test".to_string();

        cfg.switch_provider(AiProvider::GoogleGemini);
        assert_eq!(cfg.api_base, "https://generativelanguage.googleapis.com/v1beta");
        assert_eq!(cfg.model, "gemini-2.5-flash");

        cfg.switch_provider(AiProvider::CustomCloud);
        assert_eq!(cfg.api_base, "https://api.deepseek.com/v1");
        assert_eq!(cfg.model, "deepseek-chat");

        // 切回 OpenAI 时应完整保留之前填写的 Key
        cfg.switch_provider(AiProvider::OpenAiChatGpt);
        assert_eq!(cfg.api_key, "sk-test-openai");
    }

    #[test]
    fn test_short_phrase_plausibility_and_user_memory_priority() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let lexicon = CompiledLexicon::load_or_build_default(root).unwrap();
        let mut core = ImeCoreEngine::new(lexicon);

        // 1. 短句/短词合理性过滤：拦截生造散字拼凑词（如 "在乳突"、"可以乐"、"玄此"）
        let l1_zairutu = core.decode("zairutu", AppScene::General, "但是在");
        assert_eq!(l1_zairutu.candidates[0].text, "在如图");
        let plausible_zairutu =
            OpenAiCompatibleClient::filter_plausible_candidates(&l1_zairutu.candidates);
        assert!(
            !plausible_zairutu.iter().any(|c| c.text == "在乳突"),
            "生造散字组合 '在乳突' 不应进入大模型候选池"
        );

        let l1_keyile = core.decode("keyile", AppScene::General, "");
        assert_eq!(l1_keyile.candidates[0].text, "可以了");
        let plausible_keyile =
            OpenAiCompatibleClient::filter_plausible_candidates(&l1_keyile.candidates);
        assert_eq!(plausible_keyile.len(), 1);
        assert_eq!(plausible_keyile[0].text, "可以了");

        let l1_xuanci = core.decode("xuanci", AppScene::General, "还有大模型");
        assert_eq!(l1_xuanci.candidates[0].text, "选词");
        let plausible_xuanci =
            OpenAiCompatibleClient::filter_plausible_candidates(&l1_xuanci.candidates);
        assert_eq!(plausible_xuanci.len(), 1);
        assert_eq!(plausible_xuanci[0].text, "选词");

        // 2. 长句中避免散字拼凑 "急一" 压过双字词 "记忆"
        let l1_long = core.decode(
            "haiyoudamoxingxuancisihubunengjiyigaopingci",
            AppScene::General,
            "",
        );
        let pos_good = l1_long
            .candidates
            .iter()
            .position(|c| c.text == "还有大模型选词似乎不能记忆高频词")
            .expect("应包含 '还有大模型选词似乎不能记忆高频词'");
        assert!(pos_good <= 1);

        // 3. 用户高频记忆置顶保护：确认一次后即带 [记忆·1次] 标签，不被大模型覆盖
        core.commit_candidate_ex("xuanci", "选词", None, true);
        let l1_after_commit = core.decode("xuanci", AppScene::General, "");
        assert_eq!(l1_after_commit.candidates[0].text, "选词");
        assert!(l1_after_commit.candidates[0].tag.starts_with("[记忆"));
    }

    #[test]
    fn test_lm_margin_decision_rule() {
        // L1 首选略差但差距不够：保持首选（防抖）
        assert_eq!(openai_client::choose_by_lm_margin(&[-5.0, -4.5, -9.0], false), 0);
        // 无上文时需要 ≥2.3 nats 才替换
        assert_eq!(openai_client::choose_by_lm_margin(&[-8.0, -5.0], false), 1);
        // 有上文时 ≥1.2 nats 即可替换
        assert_eq!(openai_client::choose_by_lm_margin(&[-6.0, -4.6], true), 1);
        assert_eq!(openai_client::choose_by_lm_margin(&[-6.0, -5.0], true), 0);
    }

    /// 打印候选分页结构：`cargo test -p ime-ai dump_candidate_pages -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dump_candidate_pages() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let lexicon = CompiledLexicon::load_or_build_default(root).unwrap();
        let core = ImeCoreEngine::new(lexicon);
        let mut ai = DualModeAiOrchestrator::default();
        for py in ["zairenwulanshang", "haiyoujiushiguoduzhengjucaici", "zhileide", "shi"] {
            let l1 = core.decode(py, AppScene::General, "");
            let l2 = ai.rerank(py, "", AppScene::General, l1.candidates);
            println!("== {py} ({} 个候选)", l2.candidates.len());
            for (p, page) in l2.candidates.chunks(5).take(4).enumerate() {
                let items: Vec<String> = page
                    .iter()
                    .map(|c| format!("{}{}", c.text, if c.consumed_len < py.len() { "…" } else { "" }))
                    .collect();
                println!("  第{}页: {}", p + 1, items.join(" | "));
            }
        }
    }

    /// 需本机内置引擎在线：`cargo test -p ime-ai live_lm_scoring -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_lm_scoring_on_user_reported_cases() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let lexicon = CompiledLexicon::load_or_build_default(root).unwrap();
        let core = ImeCoreEngine::new(lexicon);
        let mut cfg = OpenAiCompatibleConfig::default();
        cfg.enabled = true;
        cfg.api_base = "http://127.0.0.1:18434/v1".to_string();
        cfg.model = "qwen2.5-1.5b-builtin".to_string();
        let client = OpenAiCompatibleClient::new(cfg);

        let cases: [(&str, &str, &str); 12] = [
            ("zairenwulanshang", "", "在任务栏上"),
            ("zairenwulanshang", "设置的图标没有显示", "在任务栏上"),
            ("zhileide", "", "之类的"),
            ("zhileide", "比如改变颜色", "之类的"),
            ("erbendimoxingbufen", "", "而本地模型部分"),
            ("aixuanci", "", ""),
            ("aixuanci", "还有在我简短的测试中，", ""),
            ("zairutu", "但是", "在如图"),
            ("keyile", "", "可以了"),
            ("xuanci", "大模型", "选词"),
            ("jishu", "这道微积分题需要展开成傅里叶", "级数"),
            ("jishu", "我们团队正在攻关核心计算机", "技术"),
        ];
        let mut failures = Vec::new();
        for (py, ctx, expect) in cases {
            let l1 = core.decode(py, AppScene::General, ctx);
            let pool = OpenAiCompatibleClient::filter_plausible_candidates(&l1.candidates);
            let t = std::time::Instant::now();
            let scores = OpenAiCompatibleClient::score_candidates_by_lm(
                "127.0.0.1:18434",
                ctx,
                &pool.iter().take(4).cloned().collect::<Vec<_>>(),
                2000,
            );
            let pick = client.query_best_candidate_http(py, ctx, AppScene::General, &l1.candidates);
            println!(
                "PY={py:<22} CTX={ctx:<14} L1={:?}\n    pool={:?}\n    scores={:?}\n    pick={pick:?} ({} ms)\n",
                l1.candidates.iter().take(4).map(|c| c.text.as_str()).collect::<Vec<_>>(),
                pool.iter().take(4).map(|c| c.text.as_str()).collect::<Vec<_>>(),
                scores.map(|v| v.iter().map(|s| (s * 100.0).round() / 100.0).collect::<Vec<_>>()),
                t.elapsed().as_millis()
            );
            if !expect.is_empty() && pick.as_deref().ok() != Some(expect) {
                failures.push(format!("{py}|{ctx} -> {pick:?}, expected {expect}"));
            }
        }
        assert!(failures.is_empty(), "failures: {failures:#?}");
    }
}
