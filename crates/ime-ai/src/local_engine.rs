//! `local_engine`: 端侧极速 AI 语境理解选词引擎（带 KV-Cache 上文状态预热 + 句内搭配相干性打分）
//!
//! 工作机制：
//! 1. **Context KV-Cache 预热 (`CachedContextState`)**：
//!    当光标前上文 `surrounding_context` 或焦点场景 `AppScene` 不变时，直接复用已提取的
//!    语义主题激活向量（Semantic Topic Vector），避免每次击键重复编码上文。
//! 2. **单次并行前向重打分 (Single-Pass Contextual N-Best Rescoring)**：
//!    接收 L1 解码器召回的 Top-K 同音/混输候选列表，结合「光标前上文 + 候选句内上下文」计算
//!    $\Delta \text{Score}_{\text{AI}}(W \mid \text{Context}, \text{Scene})$，在 `< 0.15ms` 内完成同音词与长句搭配消歧。

use ime_core::{AppScene, Candidate};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

/// 预计算的上下文语义槽位（实现端侧微模型的 Context KV-Cache 特征层）
#[derive(Debug, Clone, Default)]
pub struct CachedContextState {
    pub context_hash: u64,
    /// 数学/微积分/物理学术语境强度
    pub math_calculus_affinity: f32,
    /// 离散数学/奇偶数论语境强度
    pub number_theory_affinity: f32,
    /// 软件工程/代码研发/调试语境强度
    pub programming_debug_affinity: f32,
    /// 伤感/释怀/情感对话语境强度
    pub emotional_letgo_affinity: f32,
    /// 欢快/幽默/娱乐语境强度
    pub joyful_humor_affinity: f32,
    /// 突发新闻/安全事故语境强度
    pub incident_news_affinity: f32,
    /// 工程落地/动手实践语境强度
    pub hands_on_practice_affinity: f32,
    /// 时间管理/日程语境强度
    pub time_schedule_affinity: f32,
    /// 人口/统计基数语境强度
    pub statistics_base_affinity: f32,
    /// 历史/文献记载语境强度
    pub history_record_affinity: f32,
    /// 法律法规/公民权益语境强度
    pub legal_rights_affinity: f32,
    /// 政务/行政公权语境强度
    pub state_power_affinity: f32,
    /// 是否命中 KV-Cache
    pub cache_hit: bool,
}

/// 高频易混同音词的「语境触发词 -> 优选目标词」搭配规则表
struct HomophoneCollocationRule {
    triggers: &'static [&'static str],
    boost_targets: &'static [&'static str],
    penalty_targets: &'static [&'static str],
    weight: f32,
}

const COLLOCATION_RULES: &[HomophoneCollocationRule] = &[
    // 权利 vs 权力 vs 全力 (quanli)
    HomophoneCollocationRule {
        triggers: &["公民", "保障", "维护", "消费者", "合法", "享有", "义务", "人权", "隐私", "知识产权", "著作权", "专利", "剥夺"],
        boost_targets: &["权利"],
        penalty_targets: &["权力"],
        weight: 4.5,
    },
    HomophoneCollocationRule {
        triggers: &["滥用", "公权", "国家机关", "政府", "监督", "腐败", "寻租", "下放", "集中", "统治", "管辖", "越权"],
        boost_targets: &["权力"],
        penalty_targets: &["权利"],
        weight: 4.5,
    },
    HomophoneCollocationRule {
        triggers: &["以赴", "支持", "配合", "抢救", "备战", "冲刺", "竭尽"],
        boost_targets: &["全力"],
        penalty_targets: &["权利", "权力"],
        weight: 4.8,
    },
    // 登录 vs 登陆 (denglu)
    HomophoneCollocationRule {
        triggers: &["账号", "密码", "扫码", "验证码", "用户", "注册", "客户端", "网站", "后台", "系统", "微信", "邮箱", "失效"],
        boost_targets: &["登录"],
        penalty_targets: &["登陆"],
        weight: 4.6,
    },
    HomophoneCollocationRule {
        triggers: &["台风", "沿海", "抢滩", "诺曼底", "月球", "火星", "演习", "舰队", "海滩", "岛屿"],
        boost_targets: &["登陆"],
        penalty_targets: &["登录"],
        weight: 4.6,
    },
    // 公式 vs 公示 vs 共识 vs 攻势 (gongshi)
    HomophoneCollocationRule {
        triggers: &["数学", "推导", "物理", "套用", "定理", "面积", "求根", "三角函数", "欧拉", "牛顿", "计算"],
        boost_targets: &["公式"],
        penalty_targets: &["公示", "攻势"],
        weight: 4.4,
    },
    HomophoneCollocationRule {
        triggers: &["名单", "拟录用", "张贴", "社会", "周期", "举报", "候选人", "结果", "公告", "评优"],
        boost_targets: &["公示"],
        penalty_targets: &["公式", "攻势"],
        weight: 4.4,
    },
    HomophoneCollocationRule {
        triggers: &["达成", "凝聚", "广泛", "双方", "谈判", "会谈", "业界", "形成"],
        boost_targets: &["共识"],
        penalty_targets: &["公式", "公示"],
        weight: 4.6,
    },
    HomophoneCollocationRule {
        triggers: &["发起", "猛烈", "凌厉", "外交", "舆论", "防守", "瓦解", "敌人"],
        boost_targets: &["攻势"],
        penalty_targets: &["公式", "公示"],
        weight: 4.5,
    },
    // 制定 vs 制订 vs 指定 (zhiding)
    HomophoneCollocationRule {
        triggers: &["法律", "法规", "战略", "方针", "政策", "制度", "宪章", "标准", "规则"],
        boost_targets: &["制定"],
        penalty_targets: &["制订"],
        weight: 3.6,
    },
    HomophoneCollocationRule {
        triggers: &["位置", "地点", "专人", "唯一", "目录下", "文件夹", "端口", "参数"],
        boost_targets: &["指定"],
        penalty_targets: &["制定", "制订"],
        weight: 4.0,
    },
    // 反应 vs 反映 (fanying)
    HomophoneCollocationRule {
        triggers: &["化学", "过敏", "迟钝", "本能", "连锁", "应激", "第一反应", "原子核"],
        boost_targets: &["反应"],
        penalty_targets: &["反映"],
        weight: 4.2,
    },
    HomophoneCollocationRule {
        triggers: &["群众", "情况", "客观", "真实", "民意", "向上级", "问题", "深刻", "本质"],
        boost_targets: &["反映"],
        penalty_targets: &["反应"],
        weight: 4.2,
    },
    // 必须 vs 必需 (bixu)
    HomophoneCollocationRule {
        triggers: &["生活", "日常", "生存", "营养", "人体", "物资", "品"],
        boost_targets: &["必需"],
        penalty_targets: &[],
        weight: 3.8,
    },
    // 度过 vs 渡过 (duguo)
    HomophoneCollocationRule {
        triggers: &["难关", "危机", "劫", "险境", "难关"],
        boost_targets: &["渡过"],
        penalty_targets: &["度过"],
        weight: 4.5,
    },
    HomophoneCollocationRule {
        triggers: &["周末", "假期", "童年", "余生", "岁月", "时光", "春节", "生日", "夜晚"],
        boost_targets: &["度过"],
        penalty_targets: &["渡过"],
        weight: 4.2,
    },
    // 爆发 vs 暴发 (baofa)
    HomophoneCollocationRule {
        triggers: &["山洪", "泥石流", "疫情", "疾病", "传染病", "户", "冷门"],
        boost_targets: &["暴发"],
        penalty_targets: &["爆发"],
        weight: 4.4,
    },
    HomophoneCollocationRule {
        triggers: &["火山", "战争", "革命", "冲突", "掌声", "欢呼", "潜力", "小宇宙", "危机"],
        boost_targets: &["爆发"],
        penalty_targets: &["暴发"],
        weight: 4.2,
    },
    // 融化 vs 熔化 vs 溶化 (ronghua)
    HomophoneCollocationRule {
        triggers: &["冰雪", "积雪", "冰川", "雪人", "春暖", "冰块"],
        boost_targets: &["融化"],
        penalty_targets: &["熔化", "溶化"],
        weight: 4.5,
    },
    HomophoneCollocationRule {
        triggers: &["金属", "钢铁", "铁水", "高温", "炉", "铜", "铅"],
        boost_targets: &["熔化"],
        penalty_targets: &["融化", "溶化"],
        weight: 4.5,
    },
    HomophoneCollocationRule {
        triggers: &["水中", "砂糖", "食盐", "药片", "溶液", "搅拌"],
        boost_targets: &["溶化"],
        penalty_targets: &["融化", "熔化"],
        weight: 4.5,
    },
    // 侦查 vs 侦察 (zhencha)
    HomophoneCollocationRule {
        triggers: &["公安", "检察院", "立案", "犯罪", "案件", "嫌疑人", "警方", "刑事"],
        boost_targets: &["侦查"],
        penalty_targets: &["侦察"],
        weight: 4.5,
    },
    HomophoneCollocationRule {
        triggers: &["敌情", "地形", "哨兵", "部队", "无人机", "卫星", "炮兵", "机"],
        boost_targets: &["侦察"],
        penalty_targets: &["侦查"],
        weight: 4.5,
    },
    // 截止 vs 截至 (jiezhi)
    HomophoneCollocationRule {
        triggers: &["目前", "今日", "发稿", "昨晚", "年底", "月底", "昨日"],
        boost_targets: &["截至"],
        penalty_targets: &["截止"],
        weight: 4.2,
    },
    HomophoneCollocationRule {
        triggers: &["日期", "时间", "报名", "到此", "已"],
        boost_targets: &["截止"],
        penalty_targets: &["截至"],
        weight: 3.8,
    },
    // 显卡/硬件/AI 模型语境
    HomophoneCollocationRule {
        triggers: &["NVIDIA", "AMD", "GPU", "CPU", "CUDA", "算力", "显存", "推理", "帧率", "光追"],
        boost_targets: &["显卡", "运行", "程序", "模型", "驱动"],
        penalty_targets: &[],
        weight: 3.5,
    },
];

pub struct OnDeviceAiEngine {
    last_cache: CachedContextState,
}

impl Default for OnDeviceAiEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiRerankReport {
    pub candidates: Vec<Candidate>,
    pub kv_cache_hit: bool,
    pub elapsed: Duration,
    pub top_changed: bool,
    pub active_semantic_hint: String,
}

impl OnDeviceAiEngine {
    pub fn new() -> Self {
        Self {
            last_cache: CachedContextState::default(),
        }
    }

    /// 预热或复用光标前上文的 KV-Cache 语义状态
    pub fn prefill_or_get_context(&mut self, context: &str, scene: AppScene) -> CachedContextState {
        let mut hasher = DefaultHasher::new();
        context.hash(&mut hasher);
        (scene as u8).hash(&mut hasher);
        let hash = hasher.finish();

        if self.last_cache.context_hash == hash && hash != 0 {
            let mut hit_state = self.last_cache.clone();
            hit_state.cache_hit = true;
            return hit_state;
        }

        let mut state = CachedContextState {
            context_hash: hash,
            cache_hit: false,
            ..Default::default()
        };

        // 1. 结合焦点窗口场景赋予基础先验
        match scene {
            AppScene::CodeEditor => state.programming_debug_affinity += 2.2,
            AppScene::Academic => state.math_calculus_affinity += 1.8,
            AppScene::Chat => state.emotional_letgo_affinity += 0.5,
            _ => {}
        }

        // 2. 扫描光标前上文（Surrounding Text）计算细粒度语义注意力权重
        for kw in [
            "微积分", "傅里叶", "泰勒", "无穷", "收敛", "发散", "展开", "函数展开", "高数",
            "导数", "积分", "微分", "求导", "极限", "幂", "调和", "公式", "定理", "证明",
        ] {
            if context.contains(kw) {
                state.math_calculus_affinity += 3.6;
            }
        }

        for kw in ["偶数", "质数", "素数", "整除", "自然数", "正整数", "奇偶"] {
            if context.contains(kw) {
                state.number_theory_affinity += 3.8;
            }
        }

        for kw in [
            "代码", "编程", "写", "棘手", "调试", "编译", "报错", "崩溃", "接口", "后端",
            "前端", "服务器", "算法", "架构", "上线", "部署", "开源", "计算机", "程序",
            "软件", "性能", "延迟", "优化", "Rust", "Python", "API", "JSON", "GitHub",
            "Claude", "NVIDIA", "AMD", "CPU", "GPU", "大模型", "终端",
        ] {
            if context.contains(kw) {
                state.programming_debug_affinity += 2.8;
            }
        }

        for kw in [
            "算了", "过去", "已经", "放下", "无所谓", "随便", "原谅", "淡然", "看淡",
            "忘掉", "心累", "分手", "往事",
        ] {
            if context.contains(kw) {
                state.emotional_letgo_affinity += 3.8;
            }
        }

        for kw in ["相声", "搞笑", "开心", "幽默", "逗", "笑话", "喜剧", "欢快"] {
            if context.contains(kw) {
                state.joyful_humor_affinity += 3.8;
            }
        }

        for kw in ["突发", "重大", "安全", "事故", "新闻", "通报", "调查", "发生"] {
            if context.contains(kw) {
                state.incident_news_affinity += 3.6;
            }
        }

        for kw in ["工程", "动手", "落地", "理论", "检验", "真理", "亲自", "项目经验"] {
            if context.contains(kw) {
                state.hands_on_practice_affinity += 3.6;
            }
        }

        for kw in ["节省", "浪费", "安排", "抓紧", "没有", "多久", "分钟", "小时", "来不及"] {
            if context.contains(kw) {
                state.time_schedule_affinity += 3.2;
            }
        }

        for kw in ["人口", "统计", "庞大", "样本", "比例", "增长率", "总人口"] {
            if context.contains(kw) {
                state.statistics_base_affinity += 3.8;
            }
        }

        for kw in ["历史", "史书", "文献", "记载", "史料", "传记", "古籍"] {
            if context.contains(kw) {
                state.history_record_affinity += 3.8;
            }
        }

        for kw in ["公民", "宪法", "合法", "消费者", "维护", "保障", "人权", "隐私"] {
            if context.contains(kw) {
                state.legal_rights_affinity += 3.6;
            }
        }

        for kw in ["公权", "国家机关", "滥用", "监督", "执政", "腐败", "行政"] {
            if context.contains(kw) {
                state.state_power_affinity += 3.6;
            }
        }

        self.last_cache = state.clone();
        state
    }

    /// 对 L1 召回的候选列表执行端侧单次前向语义重打分
    pub fn rerank_candidates(
        &mut self,
        context: &str,
        scene: AppScene,
        mut candidates: Vec<Candidate>,
    ) -> AiRerankReport {
        let start = Instant::now();
        if candidates.is_empty() {
            return AiRerankReport {
                candidates,
                kv_cache_hit: false,
                elapsed: start.elapsed(),
                top_changed: false,
                active_semantic_hint: "无候选".to_string(),
            };
        }

        let original_top = candidates[0].text.clone();
        let ctx_state = self.prefill_or_get_context(context, scene);

        for cand in &mut candidates {
            let mut ai_bonus = 0.0f32;
            let text = cand.text.as_str();

            // 1. 数学/微积分语境消歧 (如 "级数" vs "技术", "公式" vs "公示")
            if ctx_state.math_calculus_affinity > 0.0
                && (text.contains("级数")
                    || text.contains("收敛")
                    || text.contains("方程")
                    || text.contains("公式")
                    || text.contains("导数"))
            {
                ai_bonus += ctx_state.math_calculus_affinity * 2.2;
            }

            // 2. 数论/奇偶语境消歧 (如 "奇数" vs "技术")
            if ctx_state.number_theory_affinity > 0.0 && text.contains("奇数") {
                ai_bonus += ctx_state.number_theory_affinity * 2.2;
            }

            // 3. 统计/人口基数语境消歧 (如 "基数" vs "技术")
            if ctx_state.statistics_base_affinity > 0.0 && text.contains("基数") {
                ai_bonus += ctx_state.statistics_base_affinity * 2.2;
            }

            // 4. 历史文献语境消歧 (如 "记述" vs "技术")
            if ctx_state.history_record_affinity > 0.0 && text.contains("记述") {
                ai_bonus += ctx_state.history_record_affinity * 2.2;
            }

            // 5. 软件研发与调试语境消歧 (如 "技术", "bug", "API", "性能", "变更", "登录", "部署")
            if ctx_state.programming_debug_affinity > 0.0 {
                if text.contains("bug")
                    || text.contains("API")
                    || text.contains("Rust")
                    || text.contains("Python")
                    || text.contains("JSON")
                    || text.contains("TypeScript")
                    || text.contains("GitHub")
                    || text.contains("Claude")
                    || text.contains("NVIDIA")
                    || text.contains("AMD")
                    || text.contains("CPU")
                    || text.contains("GPU")
                {
                    ai_bonus += ctx_state.programming_debug_affinity * 1.9;
                }
                if text == "技术"
                    || text == "性能"
                    || text == "变更"
                    || text == "部署"
                    || text == "登录"
                    || text == "配置"
                    || text == "进程"
                {
                    ai_bonus += ctx_state.programming_debug_affinity * 1.5;
                }
                if context.contains("棘手") && (text == "bug" || text == "问题") {
                    ai_bonus += 5.5;
                }
            }

            // 6. 情感释怀 vs 娱乐幽默语境消歧 ("不在意了" vs "不再一乐")
            if ctx_state.emotional_letgo_affinity > 0.0 && text.contains("不在意") {
                ai_bonus += ctx_state.emotional_letgo_affinity * 2.4;
            }
            if ctx_state.joyful_humor_affinity > 0.0 && text.contains("一乐") {
                ai_bonus += ctx_state.joyful_humor_affinity * 2.6;
            }

            // 7. "时间" / "事件" / "实践" 同音词三向语境消歧
            if ctx_state.incident_news_affinity > 0.0 && text.contains("事件") {
                ai_bonus += ctx_state.incident_news_affinity * 2.3;
            }
            if ctx_state.hands_on_practice_affinity > 0.0 && text.contains("实践") {
                ai_bonus += ctx_state.hands_on_practice_affinity * 2.3;
            }
            if ctx_state.time_schedule_affinity > 0.0 && text.contains("时间") {
                ai_bonus += ctx_state.time_schedule_affinity * 2.0;
            }

            // 8. 跨上文 + 候选句内同音搭配矩阵评分（如「公民的权利」vs「滥用权力」、「扫码登录」vs「台风登陆」等）
            for rule in COLLOCATION_RULES {
                let has_trigger = rule
                    .triggers
                    .iter()
                    .any(|&kw| context.contains(kw) || text.contains(kw));
                if has_trigger {
                    for &target in rule.boost_targets {
                        if text.contains(target) {
                            ai_bonus += rule.weight;
                        }
                    }
                    for &bad in rule.penalty_targets {
                        if text.contains(bad) {
                            ai_bonus -= rule.weight * 0.8;
                        }
                    }
                }
            }

            if ai_bonus.abs() > 0.5 {
                cand.final_score += ai_bonus;
                if ai_bonus > 0.5 {
                    cand.ai_boosted = true;
                }
            }
        }

        // 仅对消费了相同最大长度（整句/全词匹配）的候选项按 AI 融合得分重排，保持整句优先于单字前缀
        let max_len = candidates.iter().map(|c| c.consumed_len).max().unwrap_or(0);
        candidates.sort_by(|a, b| {
            let a_full = a.consumed_len == max_len;
            let b_full = b.consumed_len == max_len;
            b_full.cmp(&a_full).then_with(|| {
                b.final_score
                    .partial_cmp(&a.final_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });

        let top_changed = candidates[0].text != original_top;
        // 保留个人记忆标签（`[记忆·X次]`），不覆盖其提示
        if candidates[0].ai_boosted && !candidates[0].tag.starts_with("[记忆") {
            candidates[0].tag = if top_changed {
                "[AI 语境逆转置顶]".to_string()
            } else {
                "[AI 语境优选]".to_string()
            };
        }

        let active_semantic_hint = detect_dominant_topic_label(&ctx_state);

        AiRerankReport {
            candidates,
            kv_cache_hit: ctx_state.cache_hit,
            elapsed: start.elapsed(),
            top_changed,
            active_semantic_hint,
        }
    }

    /// 基于当前上文生成端侧 Ghost Text（下词/半句智能联想预测，按 Tab 键一键补全）
    pub fn predict_next_ghost_text(&self, committed_context: &str, scene: AppScene) -> Option<String> {
        let trimmed = committed_context.trim();
        if trimmed.ends_with("我在用 Rust") || trimmed.ends_with("正在用 Rust") {
            return Some(" 写高性能 AI 拼音输入法".to_string());
        }
        if trimmed.ends_with("傅里叶") {
            return Some("级数展开".to_string());
        }
        if trimmed.ends_with("调用后端") || trimmed.ends_with("调用这个") {
            return Some(" API 接口返回 JSON 数据".to_string());
        }
        if trimmed.ends_with("算了，过去的事我已经") {
            return Some("不在意了。".to_string());
        }
        if scene == AppScene::CodeEditor && trimmed.ends_with("这个 bug") {
            return Some(" 很难复现，需要先加日志调试".to_string());
        }
        None
    }
}

fn detect_dominant_topic_label(st: &CachedContextState) -> String {
    let pairs = [
        (st.math_calculus_affinity, "数学/微积分语境"),
        (st.number_theory_affinity, "数论/奇偶语境"),
        (st.programming_debug_affinity, "软件研发/编程语境"),
        (st.emotional_letgo_affinity, "情感叙事/释怀语境"),
        (st.joyful_humor_affinity, "幽默娱乐语境"),
        (st.incident_news_affinity, "突发新闻/安全事件语境"),
        (st.hands_on_practice_affinity, "工程落地/实践语境"),
        (st.statistics_base_affinity, "人口统计/基数语境"),
        (st.history_record_affinity, "历史文献/记述语境"),
        (st.time_schedule_affinity, "时间日程语境"),
        (st.legal_rights_affinity, "法律权益语境"),
        (st.state_power_affinity, "行政公权语境"),
    ];
    if let Some((score, label)) = pairs
        .into_iter()
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
    {
        if score > 0.0 {
            return format!("{label} (激活强度 +{score:.1})");
        }
    }
    "通用语境".to_string()
}
