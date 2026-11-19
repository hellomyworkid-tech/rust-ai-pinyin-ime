//! `openai_client`: 纯本地回环大模型客户端（支持内置 llama.cpp 18434 / Ollama 11434 / llama.cpp 8080 / LM Studio 1234）
//!
//! 隐私与架构原则：
//! 1. **仅限本机回环（Local-Only）**：严格校验目标地址仅允许 `http://127.0.0.1` / `http://localhost` / `http://[::1]`，
//!    并在建立 TCP 连接前二次校验解析结果 `SocketAddr::ip().is_loopback()`，彻底杜绝任何击键或上下文离开本机。
//! 2. **本地最优模型接入**：支持内置托管 `Qwen2.5-1.5B-Instruct` / `Qwen2.5-0.5B-Instruct`（Vulkan GPU 加速），
//!    并可自动探测本机 Ollama (`11434`)、`llama.cpp server` (`8080`) 或 LM Studio (`1234`)。
//! 3. **零外部网络库依赖**：完全基于标准库 `TcpStream` 本机回环通信，不再链接 `winhttp.dll`。

use crate::lm_text::{
    normalize_for_lm_scoring, LM_SCORE_CONTEXT_TAIL_CHARS, LM_SCORE_PROMPT_PREFIX,
};
use ime_core::{AppScene, Candidate};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// 本地 AI 模型服务类型（仅支持本机离线模型，保留单枚举变体以兼容历史 `ime_config.json`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
pub enum AiProvider {
    /// 本地大模型（内置 Qwen2.5-1.5B / 0.5B，或本机 Ollama / llama.cpp / LM Studio）
    #[default]
    LocalQwen,
}

impl<'de> Deserialize<'de> for AiProvider {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // 任何历史配置中残留的云端枚举值（如 "OpenAiChatGpt" / "CustomCloud" 等）均平滑迁移到本地模式
        let _ = String::deserialize(deserializer)?;
        Ok(Self::LocalQwen)
    }
}

impl AiProvider {
    pub const ALL: [AiProvider; 1] = [AiProvider::LocalQwen];

    pub fn key_id(self) -> &'static str {
        "local_qwen"
    }

    pub fn display_name(self) -> &'static str {
        "本地模型 · Qwen2.5 (离线 GPU / CPU 加速)"
    }

    pub fn short_tag(self) -> &'static str {
        "本地Qwen"
    }

    pub fn default_base_url(self) -> &'static str {
        "http://127.0.0.1:11434/v1"
    }

    pub fn default_model(self) -> &'static str {
        "qwen2.5:1.5b"
    }

    pub fn default_timeout_ms(self) -> u64 {
        900
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalEndpointStatus {
    pub name: &'static str,
    pub base_url: &'static str,
    pub online: bool,
}

fn default_local_api_base() -> String {
    AiProvider::LocalQwen.default_base_url().to_string()
}

fn default_local_model() -> String {
    AiProvider::LocalQwen.default_model().to_string()
}

fn default_local_timeout_ms() -> u64 {
    AiProvider::LocalQwen.default_timeout_ms()
}

/// 本地大模型连接配置（兼容读取历史 `ime_config.json`，自动忽略旧版云端 `api_key`/`saved_profiles` 字段）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiCompatibleConfig {
    /// 当前 AI 服务类型（固定为本地回环模式）
    #[serde(default)]
    pub provider: AiProvider,
    /// 本机推理服务 Base URL（仅允许 `http://127.0.0.1...` / `http://localhost...` / `http://[::1]...`）
    #[serde(default = "default_local_api_base")]
    pub api_base: String,
    /// 本地模型名称（如 `qwen2.5-1.5b-builtin`、`qwen2.5:1.5b`、`qwen3:1.7b`）
    #[serde(default = "default_local_model")]
    pub model: String,
    /// 请求超时毫秒数
    #[serde(default = "default_local_timeout_ms")]
    pub timeout_ms: u64,
    /// 是否启用本地大模型后台异步重排（端侧轻量语义模型始终开启）
    #[serde(default)]
    pub enabled: bool,
}

impl Default for OpenAiCompatibleConfig {
    fn default() -> Self {
        let env_base = std::env::var("IME_LOCAL_LLM_API_BASE")
            .or_else(|_| std::env::var("IME_OPENAI_API_BASE"))
            .ok()
            .filter(|u| is_loopback_http_url(u))
            .unwrap_or_else(default_local_api_base);
        let env_model = std::env::var("IME_LOCAL_LLM_MODEL")
            .or_else(|_| std::env::var("IME_OPENAI_MODEL"))
            .unwrap_or_else(|_| default_local_model());
        Self {
            provider: AiProvider::LocalQwen,
            api_base: env_base,
            model: env_model,
            timeout_ms: default_local_timeout_ms(),
            enabled: false,
        }
    }
}

impl OpenAiCompatibleConfig {
    /// 清洗历史配置中可能残留的远程云端地址：若 `api_base` 不是合法的本机回环 HTTP 地址，
    /// 则重置为默认本地配置并关闭大模型开关，防止升级后尝试外连或反复报错。
    pub fn sanitize_local_only(&mut self) {
        self.provider = AiProvider::LocalQwen;
        if !is_loopback_http_url(&self.api_base) {
            self.api_base = default_local_api_base();
            self.model = default_local_model();
            self.enabled = false;
        }
        if self.model.trim().is_empty() {
            self.model = default_local_model();
        }
        self.timeout_ms = self.timeout_ms.clamp(200, 5000);
    }
}

/// 判断给定 URL 是否为合法的本机回环 HTTP 地址（严格拦截 `https://`、远程域名、公网/局域网 IP 及 `@` 伪装主机）
pub fn is_loopback_http_url(url: &str) -> bool {
    parse_loopback_http_url(url).is_ok()
}

/// 校验并提取本机回环 HTTP 地址的 `host[:port]/path` 部分（不含 `http://`）
pub fn parse_loopback_http_url(url: &str) -> Result<String, String> {
    let trimmed = url.trim();
    if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("无效的本机服务地址（包含空白或控制字符）: {trimmed}"));
    }
    let rest = trimmed
        .strip_prefix("http://")
        .ok_or_else(|| "仅支持本机回环 HTTP 服务（例如 http://127.0.0.1:18434/v1），已禁止远程或 HTTPS 云端地址".to_string())?;
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') || authority.contains('?') || authority.contains('#') {
        return Err(format!("无效的本机服务地址: {trimmed}"));
    }
    let (host, port_opt) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let Some((ipv6, tail)) = after_bracket.split_once(']') else {
            return Err(format!("无效的 IPv6 地址: {trimmed}"));
        };
        if ipv6 != "::1" {
            return Err(format!("仅允许本机回环地址 (127.0.0.1 / localhost / [::1])，已拒绝: {trimmed}"));
        }
        let port = if tail.is_empty() {
            None
        } else if let Some(p) = tail.strip_prefix(':') {
            Some(p)
        } else {
            return Err(format!("无效的端口格式: {trimmed}"));
        };
        ("[::1]", port)
    } else if let Some((h, p)) = authority.split_once(':') {
        (h, Some(p))
    } else {
        (authority, None)
    };

    if let Some(port_str) = port_opt {
        let parsed_port = port_str
            .parse::<u16>()
            .ok()
            .filter(|&p| p > 0);
        if parsed_port.is_none() {
            return Err(format!("无效的端口号 ({port_str}): {trimmed}"));
        }
    }

    let is_local_host = host == "127.0.0.1"
        || host.eq_ignore_ascii_case("localhost")
        || host == "[::1]";
    if !is_local_host {
        return Err(format!(
            "仅允许连接本机回环地址 (127.0.0.1 / localhost)，已拒绝远程地址: {trimmed}"
        ));
    }
    Ok(rest.to_string())
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
        let (system_prompt, mut user_prompt) =
            Self::build_prompts(pinyin_input, surrounding_context, scene, candidates);
        if self.config.model.to_ascii_lowercase().contains("qwen3")
            && !user_prompt.contains("/no_think")
        {
            user_prompt.push_str("\n/no_think");
        }
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

    /// 测试本机大模型服务的连通性与响应延迟（供设置界面「测试本地连接」按钮使用）
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
                pronunciation_hint: None,
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
                pronunciation_hint: None,
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

    /// 调用已配置的本机回环大模型服务（内置 llama.cpp / Ollama / LM Studio），严格禁止远程云端请求
    pub fn query_best_candidate_http(
        &self,
        pinyin_input: &str,
        surrounding_context: &str,
        scene: AppScene,
        candidates: &[Candidate],
    ) -> Result<String, String> {
        if !self.config.enabled {
            return Err("本地大模型开关当前处于关闭状态".to_string());
        }
        let api_base = self.config.api_base.trim().trim_end_matches('/');
        if api_base.is_empty() {
            return Err("本机服务地址不能为空".to_string());
        }
        // 强制仅允许本机回环地址，杜绝任何远程网络请求
        let _ = parse_loopback_http_url(api_base)?;

        let model = self.config.model.trim();
        if model.is_empty() {
            return Err("本地模型名称不能为空".to_string());
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
                Err(e) if root.contains(":18434") => {
                    // 内置引擎未就绪或打分失败时直接返回错误（调用方自动保留 L1 首选且不污染缓存，
                    // 设置页「测试本地连接」也能准确报告离线而非误报成功）
                    return Err(e);
                }
                Err(_) => {}
            }
        }

        let (system_prompt, user_prompt) =
            Self::build_prompts(pinyin_input, surrounding_context, scene, cand_pool);
        let raw_reply =
            self.call_local_chat_completions(api_base, model, &system_prompt, &user_prompt)?;

        Ok(sanitize_llm_candidate_output(&raw_reply, cand_pool))
    }

    /// 若当前配置指向本机 llama.cpp 兼容服务（内置 18434 / llama-server 8080 / 其他本地端口），
    /// 返回其原生接口根地址（不含 `http://` 与 `/v1`），例如 `127.0.0.1:18434`
    fn llama_native_root(&self) -> Option<String> {
        let base = self.config.api_base.trim().trim_end_matches('/');
        let rest = parse_loopback_http_url(base).ok()?;
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
        let v: serde_json::Value = serde_json::from_str(resp.trim())
            .map_err(|e| format!("completion 响应解析失败: {e}"))?;
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
        let _ = parse_loopback_http_url(&format!("http://{root}"))?;
        let started = Instant::now();
        let budget = Duration::from_millis(timeout_ms.clamp(400, 4000));
        let ctx_tail: String = {
            let chars: Vec<char> = surrounding_context.trim().chars().collect();
            let start = chars.len().saturating_sub(LM_SCORE_CONTEXT_TAIL_CHARS);
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
        let scores = Self::score_candidates_by_lm(
            root,
            surrounding_context,
            &pool,
            self.config.timeout_ms.max(1500),
        )?;
        Ok(pool[choose_by_lm_margin(&scores, !surrounding_context.trim().is_empty())]
            .text
            .clone())
    }

    /// 调用本机 OpenAI 兼容聊天接口（用于本地 Ollama / LM Studio 等不支持 llama.cpp 原生打分的服务）
    fn call_local_chat_completions(
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
        let rest = parse_loopback_http_url(&endpoint)?;

        let m_lower = model.trim().to_ascii_lowercase();
        let base_lower = api_base.to_ascii_lowercase();
        let is_qwen3 = m_lower.contains("qwen3");
        let is_pure_reasoner = m_lower.contains("reasoner")
            || m_lower.contains("deepseek-r1")
            || m_lower.contains("-r1")
            || m_lower.contains("qwq");

        // 本地 Qwen3 系列（Ollama / llama.cpp / LM Studio）支持 `/no_think` 软开关，
        // 让聊天模板直接输出空 `<think>\n\n</think>` 并立即给出候选词，避免慢思考耗时
        let final_user_prompt = if is_qwen3 && !user_prompt.contains("/no_think") {
            format!("{user_prompt}\n/no_think")
        } else {
            user_prompt.to_string()
        };

        let mut body_val = serde_json::json!({
            "model": model,
            "messages": [
                { "role": "system", "content": system_prompt },
                { "role": "user", "content": final_user_prompt }
            ],
            "stream": false,
            "temperature": 0.0,
            "max_tokens": if is_pure_reasoner { 256 } else { 32 }
        });

        if is_qwen3 {
            body_val["enable_thinking"] = serde_json::json!(false);
            if base_lower.contains("11434") {
                body_val["think"] = serde_json::json!(false);
            }
        }

        let body_json = serde_json::to_string(&body_val).map_err(|e| e.to_string())?;
        let headers = vec![("Content-Type".to_string(), "application/json".to_string())];

        let resp_json = http_post_local_tcp(&rest, &headers, &body_json, self.config.timeout_ms)?;
        let parsed: serde_json::Value = serde_json::from_str(resp_json.trim())
            .map_err(|e| format!("JSON 解析失败: {e} (响应: {resp_json})"))?;

        if let Some(err_msg) = parsed["error"]["message"].as_str() {
            return Err(format!("本地服务错误: {err_msg}"));
        }

        parsed["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.trim().to_string())
            .ok_or_else(|| format!("响应未包含 choices[0].message.content: {resp_json}"))
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

/// 清理大模型返回的文本（去除思考标签 `<think>...</think>` 及被截断的未闭合 `<think>...`、编号、首尾引号、句末多余标点，并严格对齐候选列表）
pub fn sanitize_llm_candidate_output(raw: &str, candidates: &[Candidate]) -> String {
    let mut s = raw.trim();
    if let Some((_, after_think)) = s.rsplit_once("</think>") {
        s = after_think.trim();
    } else if let Some((before_think, _)) = s.split_once("<think>") {
        // 若输出包含 `<think>` 但没有闭合的 `</think>`（说明在思考阶段被 max_tokens 截断），
        // 丢弃未完成的思考过程，绝不从半截思考草稿中误匹配候选词
        s = before_think.trim();
    }
    if s.is_empty() {
        return candidates
            .first()
            .map(|c| c.text.clone())
            .unwrap_or_default();
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
            c.is_whitespace()
                || matches!(c, '"' | '\'' | '`' | '“' | '”' | '‘' | '’' | '【' | '】' | '。')
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
    // 若大模型输出了类似 "选：级数" 或包含候选词本身，优先匹配被包含的「最长」候选词
    if let Some(c) = candidates
        .iter()
        .take(4)
        .filter(|c| !c.text.is_empty() && stripped_num.contains(&c.text))
        .max_by_key(|c| c.text.chars().count())
    {
        return c.text.clone();
    }

    candidates
        .first()
        .map(|c| c.text.clone())
        .unwrap_or_default()
}

/// 仅向本机回环地址发起零依赖 HTTP POST 请求（二次校验目标 IP 必须为 loopback）
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

    let has_port = if let Some(after_bracket) = host_port.strip_prefix('[') {
        after_bracket.contains("]:")
    } else {
        host_port.contains(':')
    };
    let dial_target = if has_port {
        host_port.to_string()
    } else {
        format!("{host_port}:80")
    };

    let addrs = dial_target
        .to_socket_addrs()
        .map_err(|e| format!("本地地址解析失败 ({host_port}): {e}"))?;
    let addr = addrs
        .into_iter()
        .find(|a| a.ip().is_loopback())
        .ok_or_else(|| format!("安全拦截：目标地址 ({host_port}) 不是本机回环 IP，已拒绝连接"))?;

    let timeout = Duration::from_millis(timeout_ms.max(150));
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("连接本地推理服务失败 ({host_port}): {e}"))?;
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

    let sep_pos = resp_bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "无效的 HTTP 响应格式".to_string())?;
    let header_str = String::from_utf8_lossy(&resp_bytes[..sep_pos]);
    let body_bytes = &resp_bytes[sep_pos + 4..];

    if header_str
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        Ok(decode_chunked_bytes(body_bytes))
    } else {
        Ok(String::from_utf8_lossy(body_bytes).trim().to_string())
    }
}

fn decode_chunked_bytes(raw: &[u8]) -> String {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(line_end) = rest.windows(2).position(|w| w == b"\r\n") {
        let len_line = String::from_utf8_lossy(&rest[..line_end]);
        let clean_len = len_line.split(';').next().unwrap_or("").trim();
        let Ok(chunk_len) = usize::from_str_radix(clean_len, 16) else {
            break;
        };
        rest = &rest[line_end + 2..];
        if chunk_len == 0 {
            break;
        }
        if rest.len() < chunk_len {
            out.extend_from_slice(rest);
            break;
        }
        out.extend_from_slice(&rest[..chunk_len]);
        rest = &rest[chunk_len..];
        if rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        }
    }
    if out.is_empty() {
        String::from_utf8_lossy(raw).trim().to_string()
    } else {
        String::from_utf8_lossy(&out).trim().to_string()
    }
}
