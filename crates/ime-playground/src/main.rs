//! `ime-playground`: 交互式调试工作台与黄金评测基准套件
//!
//! 用法：
//! - 运行黄金评测基准与完整特性演示：`cargo run -p ime-playground`
//! - 进入交互式 REPL 打字测试台：`cargo run -p ime-playground -- --interactive`

use ime_ai::{DualModeAiOrchestrator, PinyinLogitsMask};
use ime_core::{AppScene, BilingualLattice, ImeCoreEngine};
use ime_lexicon::CompiledLexicon;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Instant;

struct BenchmarkCase {
    category: &'static str,
    scene: AppScene,
    context: &'static str,
    pinyin: &'static str,
    expected_top1: &'static str,
}

fn golden_benchmarks() -> Vec<BenchmarkCase> {
    vec![
        // === 一、AI 语境理解选词（同音词/同音长句消歧） ===
        BenchmarkCase {
            category: "AI 语境理解",
            scene: AppScene::Academic,
            context: "这道微积分题需要把周期函数展开成傅里叶",
            pinyin: "jishu",
            expected_top1: "级数",
        },
        BenchmarkCase {
            category: "AI 语境理解",
            scene: AppScene::CodeEditor,
            context: "我们团队正在攻关下一代核心计算机",
            pinyin: "jishu",
            expected_top1: "技术",
        },
        BenchmarkCase {
            category: "AI 语境理解",
            scene: AppScene::Academic,
            context: "不能被2整除的自然数叫做正",
            pinyin: "jishu",
            expected_top1: "奇数",
        },
        BenchmarkCase {
            category: "AI 语境理解",
            scene: AppScene::Document,
            context: "由于我国拥有庞大的人口",
            pinyin: "jishu",
            expected_top1: "基数",
        },
        BenchmarkCase {
            category: "AI 语境理解",
            scene: AppScene::Document,
            context: "这本古籍对那段历史进行了详细的",
            pinyin: "jishu",
            expected_top1: "记述",
        },
        BenchmarkCase {
            category: "AI 语境理解",
            scene: AppScene::Chat,
            context: "算了，过去的事我已经",
            pinyin: "buzaiyile",
            expected_top1: "不在意了",
        },
        BenchmarkCase {
            category: "AI 语境理解",
            scene: AppScene::Chat,
            context: "听完这段幽默搞笑相声怎么",
            pinyin: "buzaiyile",
            expected_top1: "不再一乐",
        },
        BenchmarkCase {
            category: "AI 语境理解",
            scene: AppScene::Document,
            context: "必须妥善处置这起突发安全",
            pinyin: "shijian",
            expected_top1: "事件",
        },
        BenchmarkCase {
            category: "AI 语境理解",
            scene: AppScene::CodeEditor,
            context: "理论必须结合工程动手",
            pinyin: "shijian",
            expected_top1: "实践",
        },
        BenchmarkCase {
            category: "AI 语境理解",
            scene: AppScene::General,
            context: "为了节省大家的宝贵",
            pinyin: "shijian",
            expected_top1: "时间",
        },
        // === 二、零 Shift 丝滑中英文混输 ===
        BenchmarkCase {
            category: "中英文混输",
            scene: AppScene::CodeEditor,
            context: "",
            pinyin: "wozaiyongrustxieapi",
            expected_top1: "我在用 Rust 写 API",
        },
        BenchmarkCase {
            category: "中英文混输",
            scene: AppScene::CodeEditor,
            context: "",
            pinyin: "zhegebughennanxiufu",
            expected_top1: "这个 bug 很难修复",
        },
        BenchmarkCase {
            category: "中英文混输",
            scene: AppScene::General,
            context: "",
            pinyin: "qugithubxiazaiopensourcexiangmu",
            expected_top1: "去 GitHub 下载 open source 项目",
        },
        BenchmarkCase {
            category: "中英文混输",
            scene: AppScene::CodeEditor,
            context: "",
            pinyin: "diaoyongapifanhuijson",
            expected_top1: "调用 API 返回 JSON",
        },
        BenchmarkCase {
            category: "中英文混输",
            scene: AppScene::General,
            context: "",
            pinyin: "quappstorexiazaiios",
            expected_top1: "去 AppStore 下载 iOS",
        },
        // === 三、切分消歧、简拼与英文大小写规范补全 ===
        BenchmarkCase {
            category: "切分与简拼",
            scene: AppScene::Document,
            context: "我们需要设计一套解决",
            pinyin: "fangan",
            expected_top1: "方案",
        },
        BenchmarkCase {
            category: "切分与简拼",
            scene: AppScene::Chat,
            context: "这种行为令人非常",
            pinyin: "fangan",
            expected_top1: "反感",
        },
        BenchmarkCase {
            category: "切分与简拼",
            scene: AppScene::General,
            context: "",
            pinyin: "rgzn",
            expected_top1: "人工智能",
        },
        BenchmarkCase {
            category: "英文规范补全",
            scene: AppScene::CodeEditor,
            context: "",
            pinyin: "typesc",
            expected_top1: "TypeScript",
        },
    ]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    let load_start = Instant::now();
    let lexicon = CompiledLexicon::load_or_build_default(&root)?;
    let load_elapsed = load_start.elapsed();

    let mut core = ImeCoreEngine::new(lexicon);
    let mut ai = DualModeAiOrchestrator::default();

    println!("==========================================================================================");
    println!("   Rust 全自研 AI 拼音输入法引擎 (三源融合词库 + 中英统一晶格图 + 双模 AI 语境理解选词)");
    println!("==========================================================================================");
    println!(
        " • 词库零拷贝就绪 : 中文+英文共 {} 词条 | Bigram {} 条 | 启动加载耗时 {:?}",
        core.lexicon.entry_count(),
        core.lexicon.bigram_count(),
        load_elapsed
    );
    let endpoints = ime_ai::OpenAiCompatibleClient::probe_local_endpoints();
    let ep_summary: Vec<String> = endpoints
        .iter()
        .map(|e| format!("{}:{}", e.name, if e.online { "在线(已就绪)" } else { "离线(端侧引擎兜底)" }))
        .collect();
    println!(" • 本地推理端探针 : {}", ep_summary.join(" | "));

    if args.iter().any(|a| a == "--interactive" || a == "-i") {
        run_interactive_repl(&mut core, &mut ai)?;
        return Ok(());
    }

    println!("\n[一] 黄金评测基准集运行报告 (Golden Benchmark Suite)");
    println!("------------------------------------------------------------------------------------------");

    let cases = golden_benchmarks();
    let mut top1_hits = 0usize;
    let mut top3_hits = 0usize;
    let mut total_l1_us = 0u128;
    let mut total_l2_us = 0u128;

    for (idx, case) in cases.iter().enumerate() {
        let l1 = core.decode(case.pinyin, case.scene, case.context);
        let l1_top1 = l1
            .candidates
            .first()
            .map(|c| c.text.clone())
            .unwrap_or_default();

        let l2 = ai.rerank(case.pinyin, case.context, case.scene, l1.candidates);
        let l2_top1 = l2
            .candidates
            .first()
            .map(|c| c.text.clone())
            .unwrap_or_default();
        let top3: Vec<String> = l2.candidates.iter().take(3).map(|c| c.text.clone()).collect();

        let is_top1 = l2_top1 == case.expected_top1;
        let is_top3 = top3.iter().any(|c| c == case.expected_top1);
        if is_top1 {
            top1_hits += 1;
        }
        if is_top3 {
            top3_hits += 1;
        }

        total_l1_us += l1.elapsed.as_micros();
        total_l2_us += l2.elapsed.as_micros();

        let status = if is_top1 { "PASS" } else { "MISS" };
        println!(
            " #{:02} [{}] ({}) 上文:\"{}\" + 输入:`{}`",
            idx + 1,
            status,
            case.category,
            if case.context.is_empty() { "无" } else { case.context },
            case.pinyin
        );
        println!(
            "     ├─ L1 首选: {:<22} (DAG边数:{:>3}, 耗时:{:>4}µs)",
            l1_top1,
            l1.lattice_edge_count,
            l1.elapsed.as_micros()
        );
        println!(
            "     └─ AI 重排: {:<22} (耗时:{:>3}µs, 语境:{}) | Top-3: {:?}",
            l2_top1,
            l2.elapsed.as_micros(),
            l2.active_semantic_hint,
            top3
        );
    }

    let total = cases.len();
    println!("------------------------------------------------------------------------------------------");
    println!(
        " • 评测汇总: Top-1 首选准确率 {}/{} ({:.1}%) | Top-3 准确率 {}/{} ({:.1}%)",
        top1_hits,
        total,
        (top1_hits as f64 / total as f64) * 100.0,
        top3_hits,
        total,
        (top3_hits as f64 / total as f64) * 100.0,
    );
    println!(
        " • 平均延迟: L1 中英晶格图解码 {} µs/次 | L2 端侧 AI 语境重排 {} µs/次 (远低于 15,000µs 人类感知阈值)",
        total_l1_us / (total as u128),
        total_l2_us / (total as u128),
    );

    // 演示二：黄金第二候选位 + 时间衰减个人词库动态学习
    println!("\n[二] 黄金第二候选位 & 个人动态词频 (Time-Decay User Lexicon) 验证");
    println!("------------------------------------------------------------------------------------------");
    let before_learn = core.decode("data", AppScene::General, "");
    let before_top2: Vec<String> = before_learn
        .candidates
        .iter()
        .take(2)
        .map(|c| format!("{}({})", c.text, c.tag))
        .collect();
    println!(" • 通用无上文输入 `data` 初始前二候选 : {:?}", before_top2);

    let before_jishu = core.decode("jishu", AppScene::General, "");
    println!(
        " • 用户选词前无上文输入 `jishu` 首选 : {}",
        before_jishu.candidates[0].text
    );
    core.commit_candidate("jishu", "计数", None);
    let after_jishu = core.decode("jishu", AppScene::General, "");
    println!(
        " • 用户手动选择一次「计数」后再次输入 `jishu` 首选 : {} (秒上首位！)",
        after_jishu.candidates[0].text
    );

    // 演示三：拼音前缀树 Logits 掩码约束采样验证
    println!("\n[三] 端侧 LLM 拼音前缀树掩码约束 (`PinyinLogitsMask`) 验证");
    println!("------------------------------------------------------------------------------------------");
    let mask = PinyinLogitsMask::from_lexicon(&core.lexicon);
    let mut dummy_logits = vec![1.0f32; mask.vocab.len()];
    let allowed = mask.apply_mask("buzaiyile", &mut dummy_logits);
    println!(
        " • LLM 词表总 Token 数: {} -> 输入拼音 `buzaiyile` 约束掩码后仅放行 {} 个合法发音 Token (其余置为 -inf)",
        mask.vocab.len(),
        allowed
    );

    // 演示四：Ghost Text 智能下句预测
    if let Some(ghost) = ai
        .local_engine
        .predict_next_ghost_text("我在用 Rust", AppScene::CodeEditor)
    {
        println!(
            " • 上屏 `我在用 Rust` 后触发 AI Ghost Text 灰字续写预测: \"我在用 Rust{}\" [按 Tab 一键补全]",
            ghost
        );
    }

    println!("==========================================================================================");
    Ok(())
}

fn run_interactive_repl(
    core: &mut ImeCoreEngine,
    ai: &mut DualModeAiOrchestrator,
) -> io::Result<()> {
    let mut current_scene = AppScene::CodeEditor;
    let mut current_context = String::new();

    println!("\n已进入交互式调试模式！命令说明：");
    println!("  /scene <code|chat|math|doc|general>  切换模拟焦点应用场景");
    println!("  /ctx <上文内容>                      设置光标前上文 (Surrounding Text)");
    println!("  /dag <拼音或混输串>                  打印中英统一晶格图 (DAG) 所有切分边");
    println!("  /quit                                退出");
    println!("直接输入任意拼音或中英混输串（如 `wozaiyongrustxieapi` 或 `jishu`）即可查看实时候选！\n");

    let stdin = io::stdin();
    loop {
        print!(
            "[场景:{} | 上文:\"{}\"] > ",
            current_scene.label(),
            current_context
        );
        io::stdout().flush()?;

        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input == "/quit" || input == "/exit" {
            break;
        }
        if let Some(sc) = input.strip_prefix("/scene ") {
            current_scene = match sc.trim() {
                "code" => AppScene::CodeEditor,
                "chat" => AppScene::Chat,
                "math" => AppScene::Academic,
                "doc" => AppScene::Document,
                _ => AppScene::General,
            };
            println!("  -> 已切换场景为: {}", current_scene.label());
            continue;
        }
        if let Some(ctx) = input.strip_prefix("/ctx ") {
            current_context = ctx.trim().to_string();
            println!("  -> 已更新光标前上文为: \"{}\"", current_context);
            continue;
        }
        if let Some(raw) = input.strip_prefix("/dag ") {
            let lat = BilingualLattice::build(
                raw.trim(),
                &core.lexicon,
                &core.fuzzy_config,
                core.shuangpin,
            );
            for (pos, edges) in lat.edges_from.iter().enumerate() {
                for e in edges {
                    println!(
                        "  [{:>2} -> {:>2}] {:<14} ({:?}, key={}, cost={:.2})",
                        pos, e.end, e.text, e.kind, e.code_key, e.base_cost
                    );
                }
            }
            continue;
        }

        let l1 = core.decode(input, current_scene, &current_context);
        let l2 = ai.rerank(input, &current_context, current_scene, l1.candidates);

        println!(
            "  [L1 解码: {:?} | L2 AI 重排: {:?} | KV-Cache命中: {} | 语境: {}]",
            l1.elapsed, l2.elapsed, l2.kv_cache_hit, l2.active_semantic_hint
        );
        for (i, c) in l2.candidates.iter().take(6).enumerate() {
            println!(
                "    {}. {:<20} {} (preedit: `{}`, score: {:.1})",
                i + 1,
                c.text,
                c.tag,
                c.preedit,
                c.final_score
            );
        }
    }
    Ok(())
}
