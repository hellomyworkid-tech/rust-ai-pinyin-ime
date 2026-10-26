//! `openai_client`: 多模态本地与云端大模型客户端（支持本地 Qwen2.5 + ChatGPT + Claude + Gemini + 自定义云端）
//!
//! 职责：
//! 1. **本地最优模型接入**：支持 `Qwen2.5-1.5B-Instruct` (`qwen2.5:1.5b`) 与 `Qwen2.5-0.5B-Instruct` (`qwen2.5:0.5b`)，
//!    自动探测本机 Ollama (`11434`)、`llama.cpp server` (`8080`) 或 LM Studio (`1234`)。
//! 2. **主流云端大模型原生协议支持**：
//!    - **OpenAI (ChatGPT)**：`https://api.openai.com/v1/chat/completions` (`gpt-4o-mini` / `gpt-4.1-mini`)
//!    - **Anthropic (Claude)**：`https://api.anthropic.com/v1/messages` (`claude-3-5-haiku-latest` / `claude-sonnet-4-20250514`)
//!    - **Google (Gemini)**：`https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent` (`gemini-2.5-flash`)
//!    - **自定义云端 (Custom Cloud)**：支持任意 OpenAI 兼容接口（如 DeepSeek、硅基流动 SiliconFlow、阿里云百炼、OpenRouter 等）
//! 3. **零外部依赖原生 HTTPS (`winhttp.dll`) + 本地回环极速 TCP**：
//!    在 Windows 下直接调用系统级 `WinHTTP` TLS 1.3 栈访问云端 HTTPS API，无需额外部署代理。

use ime_core::{AppScene, Candidate};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// AI 模型服务商类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AiProvider {
    /// 本地大模型（推荐 Qwen2.5-1.5B-Instruct / Qwen2.5-0.5B-Instruct，通过 Ollama / llama.cpp 运行）
    LocalQwen,
    /// OpenAI 官方或兼容接口 (ChatGPT: gpt-4o-mini / gpt-4.1-mini)
    OpenAiChatGpt,
    /// Anthropic 官方或兼容接口 (Claude: claude-3-5-haiku-latest)
    AnthropicClaude,
    /// Google 官方或兼容接口 (Gemini: gemini-2.5-flash)
    GoogleGemini,
    /// 自定义云端大模型 (DeepSeek / 硅基流动 / OpenRouter / 自建 vLLM 等)
    CustomCloud,
}

impl Default for AiProvider {
    fn default() -> Self {
        Self::LocalQwen
    }
}

impl AiProvider {
    pub const ALL: [AiProvider; 5] = [
        AiProvider::LocalQwen,
        AiProvider::OpenAiChatGpt,
        AiProvider::AnthropicClaude,
        AiProvider::GoogleGemini,
        AiProvider::CustomCloud,
    ];

    pub fn key_id(self) -> &'static str {
        match self {
            Self::LocalQwen => "local_qwen",
            Self::OpenAiChatGpt => "openai_chatgpt",
            Self::AnthropicClaude => "anthropic_claude",
            Self::GoogleGemini => "google_gemini",
            Self::CustomCloud => "custom_cloud",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::LocalQwen => "本地模型 · Qwen2.5 (推荐 RTX4070 用 qwen2.5:1.5b)",
            Self::OpenAiChatGpt => "云端大模型 · OpenAI (ChatGPT)",
            Self::AnthropicClaude => "云端大模型 · Anthropic (Claude)",
            Self::GoogleGemini => "云端大模型 · Google (Gemini)",
            Self::CustomCloud => "自定义云端 (DeepSeek / 硅基流动 / OpenRouter 等)",
        }
    }

    pub fn short_tag(self) -> &'static str {
        match self {
            Self::LocalQwen => "本地Qwen",
            Self::OpenAiChatGpt => "ChatGPT",
            Self::AnthropicClaude => "Claude",
            Self::GoogleGemini => "Gemini",
            Self::CustomCloud => "云端AI",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::LocalQwen => "http://127.0.0.1:11434/v1",
            Self::OpenAiChatGpt => "https://api.openai.com/v1",
            Self::AnthropicClaude => "https://api.anthropic.com/v1",
            Self::GoogleGemini => "https://generativelanguage.googleapis.com/v1beta",
            Self::CustomCloud => "https://api.deepseek.com/v1",
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Self::LocalQwen => "qwen2.5:1.5b",
            Self::OpenAiChatGpt => "gpt-4o-mini",
            Self::AnthropicClaude => "claude-3-5-haiku-latest",
            Self::GoogleGemini => "gemini-2.5-flash",
            Self::CustomCloud => "deepseek-chat",
        }
    }

    pub fn default_timeout_ms(self) -> u64 {
        match self {
            Self::LocalQwen => 900,
            Self::OpenAiChatGpt | Self::AnthropicClaude | Self::GoogleGemini | Self::CustomCloud => 2500,
        }
    }
}

/// 单个服务商的独立配置档案（方便用户在 GUI 中自由切换不同服务商而不丢失已填的 Key 与地址）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub api_base: String,
    pub api_key: String,
    pub model: String,
}

impl ProviderProfile {
    pub fn default_for(provider: AiProvider) -> Self {
        Self {
            api_base: provider.default_base_url().to_string(),
            api_key: String::new(),
            model: provider.default_model().to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalEndpointStatus {
    pub name: &'static str,
    pub base_url: &'static str,
    pub online: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiCompatibleConfig {
    /// 当前选中的 AI 服务商
    #[serde(default)]
    pub provider: AiProvider,
    /// 当前生效的 API Base URL
    pub api_base: String,
    /// 当前生效的 API Key（本地 Ollama 可留空）
    pub api_key: String,
    /// 当前生效的模型名称
    pub model: String,
    /// 请求超时毫秒数
    pub timeout_ms: u64,
    /// 是否启用大模型联网/本地推理重排（端侧微模型特征层始终开启）
    pub enabled: bool,
    /// 各服务商独立保存的参数档案
    #[serde(default)]
    pub saved_profiles: HashMap<String, ProviderProfile>,
}

impl Default for OpenAiCompatibleConfig {
    fn default() -> Self {
        let mut saved_profiles = HashMap::new();
        for p in AiProvider::ALL {
            saved_profiles.insert(p.key_id().to_string(), ProviderProfile::default_for(p));
        }
        Self {
            provider: AiProvider::LocalQwen,
            api_base: std::env::var("IME_OPENAI_API_BASE")
                .unwrap_or_else(|_| AiProvider::LocalQwen.default_base_url().to_string()),
            api_key: std::env::var("IME_OPENAI_API_KEY").unwrap_or_default(),
            model: std::env::var("IME_OPENAI_MODEL")
                .unwrap_or_else(|_| AiProvider::LocalQwen.default_model().to_string()),
            timeout_ms: AiProvider::LocalQwen.default_timeout_ms(),
            enabled: false,
            saved_profiles,
        }
    }
}

impl OpenAiCompatibleConfig {
    /// 将当前输入框内容同步至当前 provider 的档案，并切换到新的 provider
    pub fn switch_provider(&mut self, next_provider: AiProvider) {
        self.save_current_to_profile();
        self.provider = next_provider;
        let profile = self
            .saved_profiles
            .entry(next_provider.key_id().to_string())
            .or_insert_with(|| ProviderProfile::default_for(next_provider))
            .clone();
        self.api_base = if profile.api_base.trim().is_empty() {
            next_provider.default_base_url().to_string()
        } else {
            profile.api_base
        };
        self.api_key = profile.api_key;
        self.model = if profile.model.trim().is_empty() {
            next_provider.default_model().to_string()
        } else {
            profile.model
        };
        self.timeout_ms = next_provider.default_timeout_ms();
    }

    pub fn save_current_to_profile(&mut self) {
        self.saved_profiles.insert(
            self.provider.key_id().to_string(),
            ProviderProfile {
                api_base: self.api_base.trim().to_string(),
                api_key: self.api_key.trim().to_string(),
                model: self.model.trim().to_string(),
            },
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub temperature: f32,
    pub max_tokens: u32,
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleClient {
    pub config: OpenAiCompatibleConfig,
}

impl OpenAiCompatibleClient {
    pub fn new(config: OpenAiCompatibleConfig) -> Self {
        Self { config }
    }

    /// 自动探测本机常见的本地大模型推理端点（含输入法内置引擎 18434 / Ollama 11434 / llama.cpp 8080 / LM Studio 1234）
    pub fn probe_local_endpoints() -> Vec<LocalEndpointStatus> {
        let targets = [
            ("内置本地模型引擎 (免装Ollama)", "http://127.0.0.1:18434/v1", [127, 0, 0, 1], 18434u16),
            ("Ollama (推荐 qwen2.5:1.5b / 0.5b)", "http://127.0.0.1:11434/v1", [127, 0, 0, 1], 11434u16),
            ("llama.cpp Server (GGUF)", "http://127.0.0.1:8080/v1", [127, 0, 0, 1], 8080u16),
            ("LM Studio Local API", "http://127.0.0.1:1234/v1", [127, 0, 0, 1], 1234u16),
        ];

        targets
            .into_iter()
            .map(|(name, base_url, ip, port)| {
                let addr = SocketAddr::from((ip, port));
                let online = TcpStream::connect_timeout(&addr, Duration::from_millis(45)).is_ok();
                LocalEndpointStatus {
                    name,
                    base_url,
                    online,
                }
            })
            .collect()
    }

    /// 过滤出真正具备竞争力的合理候选词（剔除词频差距悬殊的单字生造拼凑项，防止小模型被凑数乱码词带偏）
    pub fn filter_plausible_candidates(candidates: &[Candidate]) -> Vec<Candidate> {
        let Some(first) = candidates.first() else {
            return Vec::new();
        };
        let max_len = candidates.iter().map(|c| c.consumed_len).max().unwrap_or(0);
        let best_cost = first.l1_cost;
        let first_is_exact_word = first.consumed_len == max_len
            && first.segments.len() == 1
            && !first.tag.starts_with("[纠错");

        let mut out = Vec::with_capacity(4);
        for c in candidates {
            if c.consumed_len != max_len {
                continue;
            }
            if out.is_empty() {
                out.push(c.clone());
                continue;
            }
            if first_is_exact_word {
                // 若第 1 项已是完整词库词条（如「选词」「可以了」），仅允许词频接近的其他完整词条参与同音消歧，
                // 绝不允许「可以+乐」「玄+此」这类生拼硬凑的多段碎片推翻词库整词
                if c.segments.len() == 1 && c.l1_cost <= best_cost + 4.2 {
                    out.push(c.clone());
                }
            } else {
                // 短语或长句：仅保留与第 1 项解码代价差距在合理阈值内的前 4 个候选（剔除「在乳突」「载入突」等高代价噪音）
                if c.l1_cost <= best_cost + 3.5 {
                    out.push(c.clone());
                }
            }
            if out.len() >= 4 {
                break;
            }
        }
        out
    }

    pub fn build_prompts(
        pinyin_input: &str,
        surrounding_context: &str,
        scene: AppScene,
        candidates: &[Candidate],
    ) -> (String, String) {
        let plausible = Self::filter_plausible_candidates(candidates);
        let cand_slice = if plausible.is_empty() {
            candidates
        } else {
            plausible.as_slice()
        };
        let ctx_clean = surrounding_context.trim();
        let options_str = cand_slice
            .iter()
            .take(4)
            .enumerate()
            .map(|(i, c)| {
                let default_mark = if i == 0 { " (默认高频首选)" } else { "" };
                if ctx_clean.is_empty() {
                    format!("{}. {}{}", i + 1, c.text, default_mark)
                } else {
                    format!(
                        "{}. {}{} -> 完整语境：「{}{}」",
                        i + 1,
                        c.text,
                        default_mark,
                        ctx_clean,
                        c.text
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        let system_prompt = "你是中文拼音输入法候选词优选器。用户给出了按词频从高到低排列的候选词（第1项为默认高频首选）。请结合【当前场景】和【光标前上文】选出最符合现代汉语习惯、无错别字的一项。若第1项本身已通顺自然，必须直接输出第1项；只有当第2~4项在语义搭配或语法上明显优于第1项时才选后面的项。只输出最终选中的那一个候选词本身，不要包含编号、引号或解释。".to_string();
        let user_prompt = format!(
            "【当前场景】：{}\n【光标前上文】：{}\n【输入拼音】：{}\n【候选词】：\n{}",
            scene.label(),
            if ctx_clean.is_empty() { "(无上文)" } else { ctx_clean },
            pinyin_input,
            options_str
        );
        (system_prompt, user_prompt)
    }

    /// 构造用于输入法上下文选词消歧的最小化 Prompt 请求体
    pub fn build_rerank_request(
        &self,
        pinyin_input: &str,
        surrounding_context: &str,
        scene: AppScene,
        candidates: &[Candidate],
    ) -> ChatCompletionRequest {
        let (system_prompt, user_prompt) =
            Self::build_prompts(pinyin_input, surrounding_context, scene, candidates);
        ChatCompletionRequest {
            model: self.config.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: system_prompt,
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: user_prompt,
                },
            ],
            temperature: 0.0,
            max_tokens: 32,
        }
    }

    /// 测试当前配置的连通性与响应延迟（供设置界面「测试连接」按钮使用）
    pub fn test_connection(&self) -> Result<(String, Duration), String> {
        let start = Instant::now();
        let dummy_cands = vec![
            Candidate {
                text: "技术".to_string(),
                preedit: "ji'shu".to_string(),
                consumed_len: 5,
                l1_cost: 1.0,
                final_score: 99.0,
                ai_boosted: false,
                tag: String::new(),
                segments: vec!["技术".to_string()],
                segment_spans: Vec::new(),
                syllable_spans: Vec::new(),
            },
            Candidate {
                text: "级数".to_string(),
                preedit: "ji'shu".to_string(),
                consumed_len: 5,
                l1_cost: 1.2,
                final_score: 98.0,
                ai_boosted: false,
                tag: String::new(),
                segments: vec!["级数".to_string()],
                segment_spans: Vec::new(),
                syllable_spans: Vec::new(),
            },
        ];
        let mut temp_client = self.clone();
        temp_client.config.enabled = true;
        let reply = temp_client.query_best_candidate_http(
            "jishu",
            "微积分傅里叶",
            AppScene::Academic,
            &dummy_cands,
        )?;
        Ok((reply, start.elapsed()))
    }

    /// 调用已配置的本地大模型或云端大模型（支持 Local Qwen / OpenAI ChatGPT / Anthropic Claude / Google Gemini / 自定义云端）
    pub fn query_best_candidate_http(
        &self,
        pinyin_input: &str,
        surrounding_context: &str,
        scene: AppScene,
        candidates: &[Candidate],
    ) -> Result<String, String> {
        if !self.config.enabled {
            return Err("AI 大模型开关当前处于关闭状态".to_string());
        }
        let api_base = self.config.api_base.trim().trim_end_matches('/');
        if api_base.is_empty() {
            return Err("API Base URL 不能为空".to_string());
        }
        let model = self.config.model.trim();
        if model.is_empty() {
            return Err("模型名称 (Model) 不能为空".to_string());
        }

        let plausible = Self::filter_plausible_candidates(candidates);
        if plausible.len() == 1 {
            return Ok(plausible[0].text.clone());
        }
        let cand_pool = if plausible.is_empty() {
            candidates
        } else {
            plausible.as_slice()
        };

        // 本地 llama.cpp 引擎（含内置 18434）：使用语言模型「整句联合概率」打分，而非让小模型做选择题。
        // 1.5B 级小模型做多选题时校准性很差（常无理由选第 2 项），而直接比较各候选在上文后的对数似然稳定得多。
        if let Some(root) = self.llama_native_root() {
            match self.pick_by_lm_likelihood(&root, surrounding_context, cand_pool) {
                Ok(best) => return Ok(best),
                Err(_) if self.config.provider == AiProvider::LocalQwen && root.contains(":18434") => {
                    // 内置引擎打分失败（如正在启动），保持 L1 首选，绝不退回到不稳定的生成式选择
                    return Ok(cand_pool[0].text.clone());
                }
                Err(_) => {}
            }
        }

        let (system_prompt, user_prompt) =
            Self::build_prompts(pinyin_input, surrounding_context, scene, cand_pool);

        // 根据服务商或 URL 特征选择原生协议
        let is_native_anthropic = self.config.provider == AiProvider::AnthropicClaude
            && (api_base.contains("anthropic.com") || api_base.ends_with("/messages"));
        let is_native_gemini = self.config.provider == AiProvider::GoogleGemini
            && (api_base.contains("googleapis.com") || api_base.contains("generativelanguage"));

        let raw_reply = if is_native_anthropic {
            self.call_anthropic_messages(api_base, model, &system_prompt, &user_prompt)?
        } else if is_native_gemini {
            self.call_gemini_generate_content(api_base, model, &system_prompt, &user_prompt)?
        } else {
            self.call_openai_chat_completions(api_base, model, &system_prompt, &user_prompt)?
        };

        Ok(sanitize_llm_candidate_output(&raw_reply, cand_pool))
    }

    /// 若当前配置指向本机 llama.cpp 兼容服务（内置 18434 / llama-server 8080 / 其他本地端口），
    /// 返回其原生接口根地址（不含 `http://` 与 `/v1`），例如 `127.0.0.1:18434`
    fn llama_native_root(&self) -> Option<String> {
        if !matches!(self.config.provider, AiProvider::LocalQwen | AiProvider::CustomCloud) {
            return None;
        }
        let base = self.config.api_base.trim().trim_end_matches('/');
        let rest = base.strip_prefix("http://")?;
        if !(rest.starts_with("127.0.0.1") || rest.starts_with("localhost")) {
            return None;
        }
        // Ollama (11434) / LM Studio (1234) 不提供 llama.cpp 原生 /tokenize 与 /completion 概率接口
        if rest.contains(":11434") || rest.contains(":1234") {
            return None;
        }
        let host_port = rest.split('/').next()?.to_string();
        Some(host_port)
    }

    fn llama_tokenize(root: &str, text: &str, timeout_ms: u64) -> Result<Vec<i64>, String> {
        let body = serde_json::json!({ "content": text, "add_special": false }).to_string();
        let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        let resp = http_post_local_tcp(&format!("{root}/tokenize"), &headers, &body, timeout_ms)?;
        let v: serde_json::Value =
            serde_json::from_str(resp.trim()).map_err(|e| format!("tokenize 响应解析失败: {e}"))?;
        let arr = v["tokens"]
            .as_array()
            .ok_or_else(|| format!("tokenize 响应缺少 tokens: {resp}"))?;
        Ok(arr
            .iter()
            .filter_map(|t| t.as_i64().or_else(|| t["id"].as_i64()))
            .collect())
    }

    /// 查询给定 token 前缀之后，下一个 token 为 `target` 的对数概率
    fn llama_next_token_logprob(
        root: &str,
        prefix: &[i64],
        target: i64,
        timeout_ms: u64,
    ) -> Result<f64, String> {
        let body = serde_json::json!({
            "prompt": prefix,
            "n_predict": 1,
            "n_probs": 256,
            "temperature": 0.0,
            "cache_prompt": true,
        })
        .to_string();
        let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        let resp = http_post_local_tcp(&format!("{root}/completion"), &headers, &body, timeout_ms)?;
        let v: serde_json::Value =
            serde_json::from_str(resp.trim()).map_err(|e| format!("completion 响应解析失败: {e}"))?;
        let tops = v["completion_probabilities"][0]["top_logprobs"]
            .as_array()
            .ok_or_else(|| "completion 响应缺少 top_logprobs".to_string())?;
        let mut min_lp = 0.0f64;
        for t in tops {
            let lp = t["logprob"].as_f64().unwrap_or(-30.0);
            if t["id"].as_i64() == Some(target) {
                return Ok(lp);
            }
            min_lp = min_lp.min(lp);
        }
        // 目标 token 不在 Top-256 内：给出低于第 256 名的保守下界
        Ok(min_lp - 1.0)
    }

    /// 计算每个候选在「上文」之后的联合对数似然（仅统计各候选首个分歧 token 之后的部分，数学上等价于比较整句概率）。
    /// 返回与 `candidates` 顺序一致的得分。
    pub fn score_candidates_by_lm(
        root: &str,
        surrounding_context: &str,
        candidates: &[Candidate],
        timeout_ms: u64,
    ) -> Result<Vec<f64>, String> {
        let started = Instant::now();
        let budget = Duration::from_millis(timeout_ms.clamp(400, 4000));
        let ctx_tail: String = {
            let chars: Vec<char> = surrounding_context.trim().chars().collect();
            let start = chars.len().saturating_sub(48);
            chars[start..].iter().collect()
        };
        let prefix_text = format!("{LM_SCORE_PROMPT_PREFIX}{ctx_tail}");

        let mut seqs: Vec<Vec<i64>> = Vec::with_capacity(candidates.len());
        for c in candidates {
            let scoring_text = normalize_for_lm_scoring(&c.text);
            let full = format!("{prefix_text}{scoring_text}");
            seqs.push(Self::llama_tokenize(root, &full, timeout_ms)?);
        }
        if seqs.iter().any(|s| s.is_empty()) {
            return Err("tokenize 返回空序列".to_string());
        }
        // 所有候选共享的最长 token 前缀（这部分概率完全相同，无需计算）
        let mut shared = 0usize;
        'outer: loop {
            let Some(tok) = seqs[0].get(shared) else { break };
            for s in &seqs[1..] {
                if s.get(shared) != Some(tok) {
                    break 'outer;
                }
            }
            shared += 1;
        }
        let shared = shared.max(1);

        let mut memo: HashMap<Vec<i64>, f64> = HashMap::new();
        let mut scores = Vec::with_capacity(seqs.len());
        for seq in &seqs {
            let mut total = 0.0f64;
            for i in shared..seq.len() {
                if started.elapsed() > budget {
                    return Err("本地模型打分超时".to_string());
                }
                let key = seq[..=i].to_vec();
                let lp = if let Some(&cached) = memo.get(&key) {
                    cached
                } else {
                    let v = Self::llama_next_token_logprob(root, &seq[..i], seq[i], timeout_ms)?;
                    memo.insert(key, v);
                    v
                };
                total += lp;
            }
            scores.push(total);
        }
        Ok(scores)
    }

    /// 基于语言模型似然的稳健选词：仅当某候选的整句概率显著高于 L1 首选时才替换首选
    fn pick_by_lm_likelihood(
        &self,
        root: &str,
        surrounding_context: &str,
        candidates: &[Candidate],
    ) -> Result<String, String> {
        let pool: Vec<Candidate> = candidates.iter().take(4).cloned().collect();
        if pool.is_empty() {
            return Err("候选为空".to_string());
        }
        if pool.len() == 1 {
            return Ok(pool[0].text.clone());
        }
        let scores =
            Self::score_candidates_by_lm(root, surrounding_context, &pool, self.config.timeout_ms.max(1500))?;
        Ok(pool[choose_by_lm_margin(&scores, !surrounding_context.trim().is_empty())]
            .text
            .clone())
    }

    fn call_openai_chat_completions(
        &self,
        api_base: &str,
        model: &str,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<String, String> {
        let endpoint = if api_base.ends_with("/chat/completions") {
            api_base.to_string()
        } else {
            format!("{api_base}/chat/completions")
        };

        let body_val = serde_json::json!({
            "model": model,
            "messages": [
                { "role": "system", "content": system_prompt },
                { "role": "user", "content": user_prompt }
            ],
            "temperature": 0.0,
            "max_tokens": 32,
            "stream": false
        });
        let body_json = serde_json::to_string(&body_val).map_err(|e| e.to_string())?;

        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        if !self.config.api_key.trim().is_empty() {
            headers.push((
                "Authorization".to_string(),
                format!("Bearer {}", self.config.api_key.trim()),
            ));
        }

        let resp_json = http_or_https_post(&endpoint, &headers, &body_json, self.config.timeout_ms)?;
        let parsed: serde_json::Value =
            serde_json::from_str(resp_json.trim()).map_err(|e| format!("JSON 解析失败: {e} (响应: {resp_json})"))?;

        if let Some(err_msg) = parsed["error"]["message"].as_str() {
            return Err(format!("API 错误: {err_msg}"));
        }

        parsed["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.trim().to_string())
            .ok_or_else(|| format!("响应未包含 choices[0].message.content: {resp_json}"))
    }

    fn call_anthropic_messages(
        &self,
        api_base: &str,
        model: &str,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<String, String> {
        if self.config.api_key.trim().is_empty() {
            return Err("Claude API 需要填写 API Key".to_string());
        }
        let endpoint = if api_base.ends_with("/messages") {
            api_base.to_string()
        } else if api_base.ends_with("/v1") {
            format!("{api_base}/messages")
        } else {
            format!("{api_base}/v1/messages")
        };

        let body_val = serde_json::json!({
            "model": model,
            "max_tokens": 32,
            "temperature": 0.0,
            "system": system_prompt,
            "messages": [
                { "role": "user", "content": user_prompt }
            ]
        });
        let body_json = serde_json::to_string(&body_val).map_err(|e| e.to_string())?;

        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("x-api-key".to_string(), self.config.api_key.trim().to_string()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
        ];

        let resp_json = http_or_https_post(&endpoint, &headers, &body_json, self.config.timeout_ms)?;
        let parsed: serde_json::Value =
            serde_json::from_str(resp_json.trim()).map_err(|e| format!("Claude JSON 解析失败: {e}"))?;

        if let Some(err_msg) = parsed["error"]["message"].as_str() {
            return Err(format!("Claude API 错误: {err_msg}"));
        }

        if let Some(text) = parsed["content"][0]["text"].as_str() {
            return Ok(text.trim().to_string());
        }
        if let Some(text) = parsed["choices"][0]["message"]["content"].as_str() {
            return Ok(text.trim().to_string());
        }
        Err(format!("Claude 响应未包含 content[0].text: {resp_json}"))
    }

    fn call_gemini_generate_content(
        &self,
        api_base: &str,
        model: &str,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<String, String> {
        let key = self.config.api_key.trim();
        if key.is_empty() {
            return Err("Gemini API 需要填写 API Key".to_string());
        }
        let clean_model = model.strip_prefix("models/").unwrap_or(model);
        let endpoint = format!("{api_base}/models/{clean_model}:generateContent?key={key}");

        let body_val = serde_json::json!({
            "systemInstruction": {
                "parts": [{ "text": system_prompt }]
            },
            "contents": [
                {
                    "role": "user",
                    "parts": [{ "text": user_prompt }]
                }
            ],
            "generationConfig": {
                "temperature": 0.0,
                "maxOutputTokens": 32
            }
        });
        let body_json = serde_json::to_string(&body_val).map_err(|e| e.to_string())?;

        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("x-goog-api-key".to_string(), key.to_string()),
        ];

        let resp_json = http_or_https_post(&endpoint, &headers, &body_json, self.config.timeout_ms)?;
        let parsed: serde_json::Value =
            serde_json::from_str(resp_json.trim()).map_err(|e| format!("Gemini JSON 解析失败: {e}"))?;

        if let Some(err_msg) = parsed["error"]["message"].as_str() {
            return Err(format!("Gemini API 错误: {err_msg}"));
        }

        if let Some(text) = parsed["candidates"][0]["content"]["parts"][0]["text"].as_str() {
            return Ok(text.trim().to_string());
        }
        if let Some(text) = parsed["choices"][0]["message"]["content"].as_str() {
            return Ok(text.trim().to_string());
        }
        Err(format!("Gemini 响应未包含 candidates[0].content.parts[0].text: {resp_json}"))
    }
}

/// 语言模型似然打分时置于上文之前的引导语（让模型处于「书写规范中文」的分布中）
const LM_SCORE_PROMPT_PREFIX: &str = "下面是一句通顺自然、没有错别字的中文：";

/// 打分前的文本规范化：中英混输候选（如 `AI 选词`）去掉显示用空格，使其与自然书写形式 `AI选词` 一致
fn normalize_for_lm_scoring(text: &str) -> String {
    let has_cjk = text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
    if has_cjk {
        text.chars().filter(|c| *c != ' ').collect()
    } else {
        text.to_string()
    }
}

/// 根据各候选对数似然（索引 0 为 L1 首选）决定最终首选：
/// 只有当最佳候选比 L1 首选的概率高出足够倍数时才替换，避免模型在两可情况下来回抖动。
/// - 有上文：需高出 e^1.2 ≈ 3.3 倍
/// - 无上文：需高出 e^2.3 ≈ 10 倍（无语境时词库词频与个人记忆更可靠）
pub fn choose_by_lm_margin(scores: &[f64], has_context: bool) -> usize {
    if scores.is_empty() {
        return 0;
    }
    let mut best = 0usize;
    for (i, &s) in scores.iter().enumerate() {
        if s > scores[best] {
            best = i;
        }
    }
    let margin = if has_context { 1.2 } else { 2.3 };
    if best != 0 && scores[best] - scores[0] >= margin {
        best
    } else {
        0
    }
}

/// 清理大模型返回的文本（去除思考标签 `<think>...</think>`、编号、首尾引号、句末多余标点，并严格对齐候选列表）
fn sanitize_llm_candidate_output(raw: &str, candidates: &[Candidate]) -> String {
    let mut s = raw.trim();
    if let Some((_, after_think)) = s.split_once("</think>") {
        s = after_think.trim();
    }
    let first_line = s.lines().next().unwrap_or(s).trim();
    let without_arrow = first_line
        .split_once("->")
        .map(|(l, _)| l)
        .unwrap_or(first_line)
        .split_once(" (")
        .map(|(l, _)| l)
        .unwrap_or(first_line)
        .trim();

    let cleaned = without_arrow
        .trim_matches(|c: char| {
            c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '“' | '”' | '‘' | '’' | '【' | '】' | '。')
        })
        .trim();

    if cleaned.is_empty() {
        return String::new();
    }

    // 若返回纯编号 "1".."4" 或 "1. xxx"
    let stripped_num = cleaned
        .strip_prefix("1.")
        .or_else(|| cleaned.strip_prefix("2."))
        .or_else(|| cleaned.strip_prefix("3."))
        .or_else(|| cleaned.strip_prefix("4."))
        .or_else(|| cleaned.strip_prefix("1、"))
        .or_else(|| cleaned.strip_prefix("2、"))
        .or_else(|| cleaned.strip_prefix("3、"))
        .or_else(|| cleaned.strip_prefix("4、"))
        .map(str::trim)
        .unwrap_or(cleaned);

    if let Ok(idx) = stripped_num.parse::<usize>() {
        if (1..=candidates.len()).contains(&idx) {
            return candidates[idx - 1].text.clone();
        }
    }

    // 若返回内容与某个候选完全一致，直接返回该候选
    for c in candidates {
        if c.text == stripped_num || c.text == cleaned {
            return c.text.clone();
        }
    }
    // 若大模型输出了类似 "选：级数" 或包含候选词本身，优先匹配候选词
    for c in candidates.iter().take(4) {
        if !c.text.is_empty() && stripped_num.contains(&c.text) {
            return c.text.clone();
        }
    }

    candidates
        .first()
        .map(|c| c.text.clone())
        .unwrap_or_default()
}

/// 统一发送 HTTP 或 HTTPS POST 请求：
/// - 对于本机回环 HTTP (`http://127.0.0.1` / `http://localhost`)，走零开销 `TcpStream` 直连；
/// - 对于云端 HTTPS (`https://...`) 或远程地址，在 Windows 下调用系统原生 `WinHTTP` TLS 1.3 客户端。
fn http_or_https_post(
    url: &str,
    headers: &[(String, String)],
    body_json: &str,
    timeout_ms: u64,
) -> Result<String, String> {
    if let Some(rest) = url.strip_prefix("http://") {
        if rest.starts_with("127.0.0.1") || rest.starts_with("localhost") {
            return http_post_local_tcp(rest, headers, body_json, timeout_ms);
        }
    }

    #[cfg(target_os = "windows")]
    {
        winhttp_post(url, headers, body_json, timeout_ms)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| "非 Windows 平台仅支持 http:// 直连".to_string())?;
        http_post_local_tcp(rest, headers, body_json, timeout_ms)
    }
}

fn http_post_local_tcp(
    url_without_http: &str,
    headers: &[(String, String)],
    body_json: &str,
    timeout_ms: u64,
) -> Result<String, String> {
    let (host_port, path) = match url_without_http.find('/') {
        Some(idx) => (&url_without_http[..idx], &url_without_http[idx..]),
        None => (url_without_http, "/"),
    };

    let mut addrs = host_port
        .to_socket_addrs()
        .map_err(|e| format!("本地地址解析失败 ({host_port}): {e}"))?;
    let addr = addrs
        .next()
        .ok_or_else(|| format!("无法解析地址: {host_port}"))?;

    let timeout = Duration::from_millis(timeout_ms.max(150));
    let mut stream =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("连接本地推理服务失败 ({host_port}): {e}"))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    let mut header_block = String::new();
    for (k, v) in headers {
        header_block.push_str(&format!("{k}: {v}\r\n"));
    }

    let http_req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\n{header_block}Content-Length: {}\r\nConnection: close\r\n\r\n{body_json}",
        body_json.len()
    );
    stream
        .write_all(http_req.as_bytes())
        .map_err(|e| format!("发送请求失败: {e}"))?;

    let mut resp_bytes = Vec::new();
    stream
        .read_to_end(&mut resp_bytes)
        .map_err(|e| format!("读取响应失败: {e}"))?;
    let resp_str = String::from_utf8_lossy(&resp_bytes);

    let (header_part, body_part) = resp_str
        .split_once("\r\n\r\n")
        .ok_or_else(|| "无效的 HTTP 响应格式".to_string())?;

    if header_part.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        Ok(decode_chunked_body(body_part))
    } else {
        Ok(body_part.trim().to_string())
    }
}

fn decode_chunked_body(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    while let Some((len_str, after_len)) = rest.split_once("\r\n") {
        let clean_len = len_str.split(';').next().unwrap_or("").trim();
        let Ok(chunk_len) = usize::from_str_radix(clean_len, 16) else {
            break;
        };
        if chunk_len == 0 {
            break;
        }
        if after_len.len() < chunk_len {
            out.push_str(after_len);
            break;
        }
        out.push_str(&after_len[..chunk_len]);
        rest = &after_len[chunk_len..];
        if let Some(stripped) = rest.strip_prefix("\r\n") {
            rest = stripped;
        }
    }
    if out.is_empty() {
        raw.trim().to_string()
    } else {
        out
    }
}

#[cfg(target_os = "windows")]
fn winhttp_post(
    url: &str,
    headers: &[(String, String)],
    body_json: &str,
    timeout_ms: u64,
) -> Result<String, String> {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    type HInternet = *mut c_void;
    const WINHTTP_ACCESS_TYPE_DEFAULT_PROXY: u32 = 0;
    const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;

    #[link(name = "winhttp")]
    extern "system" {
        fn WinHttpOpen(
            psz_agent_w: *const u16,
            dw_access_type: u32,
            psz_proxy_w: *const u16,
            psz_proxy_bypass_w: *const u16,
            dw_flags: u32,
        ) -> HInternet;
        fn WinHttpSetTimeouts(
            h_internet: HInternet,
            n_resolve_timeout: i32,
            n_connect_timeout: i32,
            n_send_timeout: i32,
            n_receive_timeout: i32,
        ) -> i32;
        fn WinHttpConnect(
            h_session: HInternet,
            pswz_server_name: *const u16,
            n_server_port: u16,
            dw_reserved: u32,
        ) -> HInternet;
        fn WinHttpOpenRequest(
            h_connect: HInternet,
            pwsz_verb: *const u16,
            pwsz_object_name: *const u16,
            pwsz_version: *const u16,
            pwsz_referrer: *const u16,
            ppwsz_accept_types: *const *const u16,
            dw_flags: u32,
        ) -> HInternet;
        fn WinHttpSendRequest(
            h_request: HInternet,
            lpsz_headers: *const u16,
            dw_headers_length: u32,
            lp_optional: *const c_void,
            dw_optional_length: u32,
            dw_total_length: u32,
            dw_context: usize,
        ) -> i32;
        fn WinHttpReceiveResponse(h_request: HInternet, lp_reserved: *mut c_void) -> i32;
        fn WinHttpReadData(
            h_request: HInternet,
            lp_buffer: *mut u8,
            dw_number_of_bytes_to_read: u32,
            lpdw_number_of_bytes_read: *mut u32,
        ) -> i32;
        fn WinHttpCloseHandle(h_internet: HInternet) -> i32;
    }

    struct HandleGuard(HInternet);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    WinHttpCloseHandle(self.0);
                }
            }
        }
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let (is_https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!("URL 必须以 https:// 或 http:// 开头: {url}"));
    };

    let (host_port, path_and_query) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };

    let (host, port) = if let Some((h, p_str)) = host_port.rsplit_once(':') {
        let p = p_str.parse::<u16>().unwrap_or(if is_https { 443 } else { 80 });
        (h, p)
    } else {
        (host_port, if is_https { 443 } else { 80 })
    };

    let t_ms = timeout_ms.clamp(300, 15_000) as i32;

    unsafe {
        let agent = to_wide("RustAiPinyinIME/1.0");
        let h_session = HandleGuard(WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            null(),
            null(),
            0,
        ));
        if h_session.0.is_null() {
            return Err("WinHttpOpen 初始化失败".to_string());
        }
        WinHttpSetTimeouts(h_session.0, t_ms, t_ms, t_ms, t_ms);

        let host_w = to_wide(host);
        let h_connect = HandleGuard(WinHttpConnect(h_session.0, host_w.as_ptr(), port, 0));
        if h_connect.0.is_null() {
            return Err(format!("WinHttpConnect 无法连接主机: {host}:{port}"));
        }

        let verb_w = to_wide("POST");
        let path_w = to_wide(path_and_query);
        let flags = if is_https { WINHTTP_FLAG_SECURE } else { 0 };
        let h_request = HandleGuard(WinHttpOpenRequest(
            h_connect.0,
            verb_w.as_ptr(),
            path_w.as_ptr(),
            null(),
            null(),
            null(),
            flags,
        ));
        if h_request.0.is_null() {
            return Err("WinHttpOpenRequest 创建请求失败".to_string());
        }

        let mut header_str = String::new();
        for (k, v) in headers {
            header_str.push_str(&format!("{k}: {v}\r\n"));
        }
        let header_w = to_wide(&header_str);
        let header_len = (header_w.len().saturating_sub(1)) as u32;
        let body_bytes = body_json.as_bytes();

        let sent = WinHttpSendRequest(
            h_request.0,
            header_w.as_ptr(),
            header_len,
            body_bytes.as_ptr() as *const c_void,
            body_bytes.len() as u32,
            body_bytes.len() as u32,
            0,
        );
        if sent == 0 {
            return Err(format!("HTTPS 请求发送失败 ({host}:{port})，请检查网络或 API 地址"));
        }

        let received = WinHttpReceiveResponse(h_request.0, null_mut());
        if received == 0 {
            return Err(format!("等待 {host} 响应超时或被拒绝"));
        }

        let mut response_data = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let mut bytes_read = 0u32;
            let ok = WinHttpReadData(
                h_request.0,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut bytes_read,
            );
            if ok == 0 || bytes_read == 0 {
                break;
            }
            response_data.extend_from_slice(&buf[..bytes_read as usize]);
        }

        String::from_utf8(response_data).map_err(|e| format!("响应不是合法 UTF-8: {e}"))
    }
}
