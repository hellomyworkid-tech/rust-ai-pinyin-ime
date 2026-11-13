//! 引擎封装与 C 接口的集成测试。用仓库里的词库（没有完整词库时退回种子词库）。

use ime_ffi::*;
use serde_json::Value;
use std::ffi::{c_char, CStr, CString};
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// 词库文件路径；文件不存在时先用种子词表生成一份
fn lexicon_path() -> PathBuf {
    let root = repo_root();
    ime_lexicon::CompiledLexicon::load_or_build_default(root.clone()).expect("lexicon");
    root.join("data/compiled/merged_lexicon.bin")
}

fn engine() -> ImeEngine {
    ImeEngine::open(&lexicon_path(), None).expect("open engine")
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("bad JSON {text}: {e}"))
}

fn type_letters(engine: &ImeEngine, letters: &str) -> Value {
    let mut last = Value::Null;
    for ch in letters.chars() {
        last = json(&engine.key(IME_KEY_CHAR, ch as u32));
        assert_eq!(last["action"], "update", "typing {ch:?} in {letters:?}: {last}");
    }
    last
}

fn top_text(envelope: &Value) -> &str {
    envelope["view"]["visible_candidates"][0]["text"]
        .as_str()
        .unwrap_or("")
}

#[test]
fn typing_and_space_commits_top_candidate() {
    let engine = engine();
    assert!(!engine.is_composing());
    let last = type_letters(&engine, "nihao");
    assert!(engine.is_composing());
    assert_eq!(top_text(&last), "你好");
    assert_eq!(last["view"]["visible"], true);
    assert_eq!(last["view"]["preedit_text"], "ni'hao");
    assert_eq!(last["view"]["page_size"], DEFAULT_PAGE_SIZE);
    assert_eq!(last["view"]["composed_full_sentence"], "你好");
    assert_eq!(last["view"]["syllable_spans"][1]["code_key"], "hao");

    let commit = json(&engine.key(IME_KEY_SPACE, 0));
    assert_eq!(commit["action"], "commit");
    assert_eq!(commit["text"], "你好");
    assert_eq!(commit["view"]["visible"], false);
    assert!(!engine.is_composing());
    assert_eq!(json(&engine.view())["visible"], false);
}

#[test]
fn candidates_carry_only_the_fields_the_host_uses() {
    let engine = engine();
    let last = type_letters(&engine, "nihao");
    let candidate = last["view"]["visible_candidates"][0].as_object().unwrap();
    let mut keys: Vec<&str> = candidate.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["ai_boosted", "consumed_len", "preedit", "tag", "text"]);
    assert_eq!(candidate["consumed_len"], 5);
    // 整句预览的分段是 ["文字", 是否已选定] 这样的二元组
    assert_eq!(last["view"]["full_sentence_parts"][0][0], "你好");
    assert_eq!(last["view"]["full_sentence_parts"][0][1], false);
}

#[test]
fn idle_keys_pass_through() {
    let engine = engine();
    for kind in [
        IME_KEY_SPACE,
        IME_KEY_ENTER,
        IME_KEY_BACKSPACE,
        IME_KEY_ESCAPE,
        IME_KEY_PAGE_UP,
        IME_KEY_PAGE_DOWN,
        IME_KEY_CURSOR_LEFT,
        IME_KEY_CURSOR_RIGHT,
        99,
    ] {
        assert_eq!(json(&engine.key(kind, 0))["action"], "pass", "kind {kind}");
    }
    assert_eq!(json(&engine.select_candidate(0))["action"], "pass");
    assert_eq!(json(&engine.tap_syllable(0))["action"], "pass");
    assert_eq!(json(&engine.commit_full_sentence())["action"], "pass");
    // 空闲时的大写字母、标点、数字、非法码点都交还给宿主
    for value in ['A' as u32, ',' as u32, '1' as u32, '\'' as u32, '你' as u32, 0xD800] {
        assert_eq!(json(&engine.key(IME_KEY_CHAR, value))["action"], "pass", "{value:#x}");
    }
    assert!(!engine.is_composing());
}

#[test]
fn punctuation_never_joins_the_composition() {
    let engine = engine();
    type_letters(&engine, "nihao");
    assert_eq!(json(&engine.key(IME_KEY_CHAR, ',' as u32))["action"], "pass");
    assert_eq!(json(&engine.key(IME_KEY_CHAR, '5' as u32))["action"], "pass");
    assert_eq!(json(&engine.view())["preedit_text"], "ni'hao");
    // 宿主的做法：先整句上屏，再自己插入标点
    let commit = json(&engine.commit_full_sentence());
    assert_eq!(commit["action"], "commit");
    assert_eq!(commit["text"], "你好");
}

#[test]
fn enter_commits_raw_letters_and_escape_cancels() {
    let engine = engine();
    type_letters(&engine, "nihao");
    let commit = json(&engine.key(IME_KEY_ENTER, 0));
    assert_eq!(commit["action"], "commit");
    assert_eq!(commit["text"], "nihao");

    type_letters(&engine, "nihao");
    let cancelled = json(&engine.key(IME_KEY_ESCAPE, 0));
    assert_eq!(cancelled["action"], "update");
    assert_eq!(cancelled["view"]["visible"], false);
    assert!(!engine.is_composing());

    type_letters(&engine, "ni");
    engine.reset();
    assert!(!engine.is_composing());
    assert_eq!(json(&engine.view())["visible"], false);
}

#[test]
fn backspace_edits_then_empties_the_composition() {
    let engine = engine();
    type_letters(&engine, "ni");
    let one = json(&engine.key(IME_KEY_BACKSPACE, 0));
    assert_eq!(one["action"], "update");
    assert_eq!(one["view"]["preedit_text"], "n");
    let none = json(&engine.key(IME_KEY_BACKSPACE, 0));
    assert_eq!(none["action"], "update");
    assert_eq!(none["view"]["visible"], false);
    assert!(!engine.is_composing());
    assert_eq!(json(&engine.key(IME_KEY_BACKSPACE, 0))["action"], "pass");
}

#[test]
fn english_toggle() {
    let engine = engine();
    assert!(!engine.is_english());
    let toggled = json(&engine.key(IME_KEY_TOGGLE_ENGLISH, 0));
    assert_eq!(toggled["action"], "update");
    assert_eq!(toggled["view"]["visible"], false);
    assert!(engine.is_english());
    assert_eq!(json(&engine.key(IME_KEY_CHAR, 'n' as u32))["action"], "pass");
    assert_eq!(json(&engine.key(IME_KEY_SPACE, 0))["action"], "pass");
    engine.key(IME_KEY_TOGGLE_ENGLISH, 0);
    assert!(!engine.is_english());

    // 输入中途切换：原样字母上屏，进入英文
    type_letters(&engine, "nihao");
    let commit = json(&engine.key(IME_KEY_TOGGLE_ENGLISH, 0));
    assert_eq!(commit["action"], "commit");
    assert_eq!(commit["text"], "nihao");
    assert!(engine.is_english());
    assert!(!engine.is_composing());
}

#[test]
fn uppercase_is_kept_inside_a_composition() {
    let engine = engine();
    type_letters(&engine, "wozaiyong");
    let upper = json(&engine.key(IME_KEY_CHAR, 'R' as u32));
    assert_eq!(upper["action"], "update");
    assert!(engine.is_composing());
}

#[test]
fn selecting_a_prefix_candidate_pins_it_and_the_rest_follows() {
    let engine = engine();
    let last = type_letters(&engine, "jintiantianqihenhao");
    let candidates = last["view"]["visible_candidates"].as_array().unwrap();
    // 找一个只覆盖前半段的候选（「今天」）
    let (index, _) = candidates
        .iter()
        .enumerate()
        .find(|(_, c)| c["text"] == "今天")
        .expect("prefix candidate 今天");
    let pinned = json(&engine.select_candidate(index));
    assert_eq!(pinned["action"], "update", "{pinned}");
    assert!(engine.is_composing());
    let parts = pinned["view"]["full_sentence_parts"].as_array().unwrap();
    assert_eq!(parts[0][0], "今天");
    assert_eq!(parts[0][1], true);
    assert!(pinned["view"]["active_syllable_index"].as_u64().is_some());

    // 越界下标只刷新视图
    let out_of_range = json(&engine.select_candidate(100_000));
    assert_eq!(out_of_range["action"], "update");

    let commit = json(&engine.commit_full_sentence());
    assert_eq!(commit["action"], "commit");
    assert!(commit["text"].as_str().unwrap().starts_with("今天"));
    assert!(!engine.is_composing());
}

#[test]
fn tapping_a_syllable_toggles_single_syllable_mode() {
    let engine = engine();
    type_letters(&engine, "nihao");
    let tapped = json(&engine.tap_syllable(0));
    assert_eq!(tapped["action"], "update");
    assert_eq!(tapped["view"]["active_syllable_index"], 0);
    assert_eq!(tapped["view"]["cursor_byte_pos"], 0);
    let again = json(&engine.tap_syllable(0));
    assert_eq!(again["view"]["active_syllable_index"], Value::Null);
    assert_eq!(top_text(&again), "你好");
    // 不存在的音节：视图不变
    let missing = json(&engine.tap_syllable(42));
    assert_eq!(missing["action"], "update");
    assert_eq!(missing["view"]["active_syllable_index"], Value::Null);
}

#[test]
fn cursor_keys_move_by_syllable() {
    let engine = engine();
    type_letters(&engine, "nihao");
    let left = json(&engine.key(IME_KEY_CURSOR_LEFT, 0));
    assert_eq!(left["view"]["active_syllable_index"], 1);
    let left = json(&engine.key(IME_KEY_CURSOR_LEFT, 0));
    assert_eq!(left["view"]["active_syllable_index"], 0);
    let right = json(&engine.key(IME_KEY_CURSOR_RIGHT, 0));
    assert_eq!(right["view"]["active_syllable_index"], 1);
    let right = json(&engine.key(IME_KEY_CURSOR_RIGHT, 0));
    assert_eq!(right["view"]["active_syllable_index"], Value::Null);
}

#[test]
fn paging() {
    let engine = engine();
    engine.configure(r#"{"page_size":3}"#).unwrap();
    let first = type_letters(&engine, "shi");
    assert_eq!(first["view"]["page_size"], 3);
    assert_eq!(first["view"]["page_index"], 0);
    assert_eq!(first["view"]["visible_candidates"].as_array().unwrap().len(), 3);
    let total = first["view"]["total_candidates"].as_u64().unwrap();
    assert!(total > 3, "shi should have more than one page, got {total}");
    let second = json(&engine.key(IME_KEY_PAGE_DOWN, 0));
    assert_eq!(second["view"]["page_index"], 1);
    assert_ne!(top_text(&second), top_text(&first));
    let back = json(&engine.key(IME_KEY_PAGE_UP, 0));
    assert_eq!(back["view"]["page_index"], 0);
    assert_eq!(top_text(&back), top_text(&first));
}

#[test]
fn space_after_paging_commits_the_first_candidate() {
    // 触屏键盘翻页只是为了多取候选，高亮的始终是全部候选里的第 0 个。
    // 以前空格提交的是「引擎停着的那一页」的第一个：shi、翻一页、空格，上屏的是第 30 个候选「诗」
    for letters in ["shi", "woxiangqugongyuan"] {
        let plain = engine();
        let first = type_letters(&plain, letters);
        let expected = json(&plain.key(IME_KEY_SPACE, 0));
        assert_eq!(expected["action"], "commit", "{letters}");
        assert_eq!(expected["text"], top_text(&first), "{letters}");

        let paged = engine();
        let first = type_letters(&paged, letters);
        let total = first["view"]["total_candidates"].as_u64().unwrap();
        assert!(total > DEFAULT_PAGE_SIZE as u64, "{letters} needs a second page, got {total}");
        let second = json(&paged.key(IME_KEY_PAGE_DOWN, 0));
        assert_eq!(second["view"]["page_index"], 1);
        assert_ne!(top_text(&second), top_text(&first));
        let commit = json(&paged.key(IME_KEY_SPACE, 0));
        assert_eq!(commit, expected, "{letters}: paging must not change what Space commits");
        assert!(!paged.is_composing());
    }
}

#[test]
fn candidates_returns_any_range_without_moving_the_engine() {
    let engine = engine();
    // 没有输入：空
    assert_eq!(json(&engine.candidates(0, 30)), json(r#"{"total":0,"offset":0,"candidates":[]}"#));

    let first = type_letters(&engine, "shi");
    let total = first["view"]["total_candidates"].as_u64().unwrap() as usize;
    assert!(total > 2 * DEFAULT_PAGE_SIZE, "shi should fill more than two pages, got {total}");

    // 第 0 段与视图里的第一页相同，候选对象的字段也相同
    let head = json(&engine.candidates(0, DEFAULT_PAGE_SIZE));
    assert_eq!(head["total"], total);
    assert_eq!(head["offset"], 0);
    assert_eq!(head["candidates"], first["view"]["visible_candidates"]);

    // 后面的段与翻页看到的相同（另开一个引擎去翻页，对照用）
    let reference = engine_with_input("shi");
    let second_page = json(&reference.key(IME_KEY_PAGE_DOWN, 0));
    let more = json(&engine.candidates(DEFAULT_PAGE_SIZE, DEFAULT_PAGE_SIZE));
    assert_eq!(more["total"], total);
    assert_eq!(more["offset"], DEFAULT_PAGE_SIZE);
    assert_eq!(more["candidates"], second_page["view"]["visible_candidates"]);

    // 任意起点和长度；末尾不足的只给剩下的；越界给空
    let tail = json(&engine.candidates(total - 2, 10));
    assert_eq!(tail["candidates"].as_array().unwrap().len(), 2);
    assert_eq!(tail["offset"], total - 2);
    let beyond = json(&engine.candidates(total + 5, 10));
    assert_eq!(beyond["total"], total);
    assert_eq!(beyond["offset"], total + 5);
    assert_eq!(beyond["candidates"].as_array().unwrap().len(), 0);
    assert_eq!(json(&engine.candidates(0, 0))["candidates"].as_array().unwrap().len(), 0);
    assert_eq!(json(&engine.candidates(usize::MAX, usize::MAX))["candidates"].as_array().unwrap().len(), 0);
    // 一次最多 MAX_CANDIDATES_PER_CALL 个
    let all = json(&engine.candidates(0, usize::MAX));
    assert_eq!(all["candidates"].as_array().unwrap().len(), total.min(MAX_CANDIDATES_PER_CALL));

    // 引擎一直停在第 0 页：视图没变，空格提交的还是第一个候选，下标与点选一致
    let view = json(&engine.view());
    assert_eq!(view, first["view"]);
    let picked = more["candidates"][3]["text"].as_str().unwrap().to_string();
    let commit = json(&engine.select_candidate(DEFAULT_PAGE_SIZE + 3));
    assert_eq!(commit["text"], picked.as_str());

    // （刚才的点选被个人词库记住了，首选可能已经换了：与这一次的首选比）
    let again = type_letters(&engine, "shi");
    engine.candidates(DEFAULT_PAGE_SIZE, DEFAULT_PAGE_SIZE);
    let commit = json(&engine.key(IME_KEY_SPACE, 0));
    assert_eq!(commit["text"], top_text(&again));
}

fn engine_with_input(letters: &str) -> ImeEngine {
    let engine = engine();
    type_letters(&engine, letters);
    engine
}

/// 评审给出的复现：在已选定的多音节片段里再次改选，解码器给不出任何候选
fn compose_with_zero_candidates(engine: &ImeEngine) {
    type_letters(engine, "shyuan");
    assert_eq!(json(&engine.select_candidate(1))["action"], "update");
    type_letters(engine, "hao");
    assert_eq!(json(&engine.tap_syllable(1))["action"], "update");
    let stuck = json(&engine.select_candidate(1));
    assert_eq!(stuck["action"], "update");
    assert_eq!(stuck["view"]["total_candidates"], 0, "repro no longer reaches the zero-candidate state: {stuck}");
    assert_eq!(stuck["view"]["composed_full_sentence"], "");
    assert!(engine.is_composing());
}

#[test]
fn a_composition_without_candidates_can_still_be_committed() {
    // 回车一直能上屏：它给出的文本就是另外两条路应当给出的
    let by_enter = engine();
    compose_with_zero_candidates(&by_enter);
    let expected = json(&by_enter.key(IME_KEY_ENTER, 0));
    assert_eq!(expected["action"], "commit");
    let raw = expected["text"].as_str().unwrap().to_string();
    assert!(raw.contains("yuanhao"), "pinned text + the remaining letters, got {raw}");

    // 以前：整句上屏返回 update、什么也没提交，宿主接着插入的标点就跑到了拼音前面
    let by_commit = engine();
    compose_with_zero_candidates(&by_commit);
    let commit = json(&by_commit.commit_full_sentence());
    assert_eq!(commit["action"], "commit");
    assert_eq!(commit["text"], raw.as_str());
    assert!(!by_commit.is_composing());

    // 以前：空格什么也不做
    let by_space = engine();
    compose_with_zero_candidates(&by_space);
    let commit = json(&by_space.key(IME_KEY_SPACE, 0));
    assert_eq!(commit["action"], "commit");
    assert_eq!(commit["text"], raw.as_str());
    assert!(!by_space.is_composing());
}

#[test]
fn cursor_keys_never_pass_while_composing() {
    let engine = engine();
    compose_with_zero_candidates(&engine);
    // 以前：没有音节可移动时光标右移返回 pass，宿主会去移动输入框里的光标
    for kind in [IME_KEY_CURSOR_RIGHT, IME_KEY_CURSOR_LEFT, IME_KEY_CURSOR_RIGHT] {
        let moved = json(&engine.key(kind, 0));
        assert_eq!(moved["action"], "update", "kind {kind}");
        assert_eq!(moved["view"]["visible"], true);
        assert!(engine.is_composing());
    }
    engine.reset();
    // 空闲时照旧放行
    assert_eq!(json(&engine.key(IME_KEY_CURSOR_RIGHT, 0))["action"], "pass");
    assert_eq!(json(&engine.key(IME_KEY_CURSOR_LEFT, 0))["action"], "pass");
}

#[test]
fn composition_length_is_capped() {
    let engine = engine();
    let syllables = "nihao".repeat(MAX_COMPOSITION_BYTES / 5);
    assert_eq!(syllables.len(), MAX_COMPOSITION_BYTES);
    let full = type_letters(&engine, &syllables);
    let letters_in = |view: &Value| -> usize {
        view["preedit_text"].as_str().unwrap().chars().filter(|c| c.is_ascii_alphabetic()).count()
    };
    assert_eq!(letters_in(&full["view"]), MAX_COMPOSITION_BYTES);

    // 到了上限：字母和分词符被吃掉，视图原样返回
    for ch in ['a', 'Z', '\''] {
        let swallowed = json(&engine.key(IME_KEY_CHAR, ch as u32));
        assert_eq!(swallowed["action"], "update", "{ch:?}");
        assert_eq!(swallowed["view"], full["view"], "{ch:?}");
    }
    assert_eq!(json(&engine.view()), full["view"]);
    // 标点和数字照旧交还宿主
    assert_eq!(json(&engine.key(IME_KEY_CHAR, ',' as u32))["action"], "pass");

    // 退格腾出位置之后又可以输入；仍然不能超过上限
    let shorter = json(&engine.key(IME_KEY_BACKSPACE, 0));
    assert_eq!(letters_in(&shorter["view"]), MAX_COMPOSITION_BYTES - 1);
    let again = json(&engine.key(IME_KEY_CHAR, 'o' as u32));
    assert_eq!(letters_in(&again["view"]), MAX_COMPOSITION_BYTES);
    let swallowed = json(&engine.key(IME_KEY_CHAR, 'n' as u32));
    assert_eq!(swallowed["view"], again["view"]);

    // 照常上屏
    let commit = json(&engine.commit_full_sentence());
    assert_eq!(commit["action"], "commit");
    assert!(commit["text"].as_str().unwrap().starts_with("你好"), "{commit}");
    assert!(!engine.is_composing());
}

#[test]
fn configure_applies_fuzzy_and_scene_and_rejects_bad_json() {
    let engine = engine();
    assert!(engine.configure("not json").is_err());
    assert!(engine.configure(r#"{"scene":"Nowhere"}"#).is_err());
    assert!(engine.configure("{}").is_ok());
    assert!(engine
        .configure(r#"{"fuzzy":{"z_zh":false,"c_ch":false,"s_sh":false,"n_l":false,"f_h":false,"in_ing":false,"en_eng":false,"an_ang":false,"typo_correction":true},"scene":"Chat","unknown":1}"#)
        .is_ok());

    // 模糊音 s/sh：关掉时 "si" 的候选里没有「是」，打开后有
    let has = |engine: &ImeEngine, text: &str| -> bool {
        let last = type_letters(engine, "si");
        let found = last["view"]["visible_candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["text"] == text);
        engine.reset();
        found
    };
    assert!(!has(&engine, "是"));
    engine.configure(r#"{"fuzzy":{"s_sh":true}}"#).unwrap();
    assert!(has(&engine, "是"));
}

#[test]
fn context_changes_the_ranking() {
    let engine = engine();
    engine.configure(r#"{"scene":"Academic"}"#).unwrap();
    engine.set_context(Some("这道微积分题需要展开成傅里叶"));
    let math = type_letters(&engine, "jishu");
    assert_eq!(top_text(&math), "级数");
    engine.reset();

    engine.configure(r#"{"scene":"CodeEditor"}"#).unwrap();
    engine.set_context(Some("我们团队正在攻关核心计算机"));
    let code = type_letters(&engine, "jishu");
    assert_eq!(top_text(&code), "技术");
    engine.reset();
    engine.set_context(None);
}

#[test]
fn user_dictionary_is_written_to_the_given_path() {
    let dir = std::env::temp_dir().join(format!("ime-ffi-test-{}", std::process::id()));
    let path = dir.join("nested").join("user_dict.json");
    let _ = std::fs::remove_dir_all(&dir);
    {
        let engine = ImeEngine::open(&lexicon_path(), Some(&path)).expect("open");
        let last = type_letters(&engine, "shiyan");
        let candidates = last["view"]["visible_candidates"].as_array().unwrap();
        assert!(candidates.len() >= 2);
        // 主动改选首选之后的第一个词，个人词库会记住
        let commit = json(&engine.select_candidate(second_word_index(&last)));
        assert_eq!(commit["action"], "commit");
    }
    // 写盘在后台线程
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(path.exists(), "user dict should be saved at {}", path.display());
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("shi"), "{saved}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 输入 `letters`，改选第 1 个候选（不是首选），返回它的文字。个人词库会记住这次改选
/// 首选之后的第一个词候选的下标（跳过表情候选：表情选中后不进个人词库）
fn second_word_index(view: &Value) -> usize {
    view["view"]["visible_candidates"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, c)| c["tag"] != "[表情]")
        .map(|(i, _)| i)
        .expect("首选之后应还有词候选")
}

fn pick_second_candidate(engine: &ImeEngine, letters: &str) -> String {
    let last = type_letters(engine, letters);
    let index = second_word_index(&last);
    let chosen = last["view"]["visible_candidates"][index]["text"].as_str().unwrap().to_string();
    let commit = json(&engine.select_candidate(index));
    assert_eq!(commit["action"], "commit");
    assert_eq!(commit["text"], chosen.as_str());
    chosen
}

fn top_for(engine: &ImeEngine, letters: &str) -> String {
    let top = top_text(&type_letters(engine, letters)).to_string();
    engine.reset();
    top
}

/// 反复调用 `reload_user_dict` 直到它报告「重新读入了」（另一个引擎的写盘在后台线程上）
fn reload_until_changed(engine: &ImeEngine) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if engine.reload_user_dict().expect("reload") {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "the other engine's write never showed up");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn engines_sharing_one_user_dictionary_do_not_erase_each_other() {
    // iOS 给每个宿主 App 各开一个键盘进程，它们共用一个词库文件，各自在内存里有一份。
    // 以前：先打开的 B 下一次上屏会用自己的旧快照把 A 学的东西盖掉
    let dir = std::env::temp_dir().join(format!("ime-ffi-shared-{}", std::process::id()));
    let path = dir.join("user_dict.json");
    let _ = std::fs::remove_dir_all(&dir);
    let lexicon = lexicon_path();

    let b = ImeEngine::open(&lexicon, Some(&path)).expect("open b");
    let a = ImeEngine::open(&lexicon, Some(&path)).expect("open a");
    let default_shiyan = top_for(&b, "shiyan");
    let default_jishu = top_for(&b, "jishu");
    assert!(!b.reload_user_dict().expect("reload"), "nothing has been written yet");

    // A 学到一个改选
    let learned_by_a = pick_second_candidate(&a, "shiyan");
    assert_ne!(learned_by_a, default_shiyan);
    assert_eq!(top_for(&a, "shiyan"), learned_by_a);

    // B 的键盘出现：重新读入，看到 A 学的；再问一次就没有变化了
    assert_eq!(top_for(&b, "shiyan"), default_shiyan);
    reload_until_changed(&b);
    assert_eq!(top_for(&b, "shiyan"), learned_by_a);
    assert!(!b.reload_user_dict().expect("reload"));

    // B 自己再学一个别的，然后关掉（销毁时等写盘完成）
    let learned_by_b = pick_second_candidate(&b, "jishu");
    assert_ne!(learned_by_b, default_jishu);
    drop(b);

    // 之后打开的引擎两样都有
    let c = ImeEngine::open(&lexicon, Some(&path)).expect("open c");
    assert_eq!(top_for(&c, "shiyan"), learned_by_a, "A's learning must survive B's write");
    assert_eq!(top_for(&c, "jishu"), learned_by_b);

    // A 一直开着：它的键盘再出现时也跟上 B
    assert_eq!(top_for(&a, "jishu"), default_jishu);
    reload_until_changed(&a);
    assert_eq!(top_for(&a, "jishu"), learned_by_b);

    // 正在输入时重新读入：候选按新词库重算。文件被写坏：报错，内存里的词库不变
    type_letters(&c, "shiyan");
    let learned_by_a_again = {
        let last = type_letters(&a, "gongshi");
        let chosen = last["view"]["visible_candidates"][1]["text"].as_str().unwrap().to_string();
        assert_eq!(json(&a.select_candidate(1))["action"], "commit");
        chosen
    };
    reload_until_changed(&c);
    assert!(c.is_composing());
    assert_eq!(json(&c.view())["visible_candidates"][0]["text"], learned_by_a.as_str());
    c.reset();
    assert_eq!(top_for(&c, "gongshi"), learned_by_a_again);

    drop(a);
    std::fs::write(&path, b"{ not json").unwrap();
    assert!(c.reload_user_dict().is_err());
    assert_eq!(top_for(&c, "shiyan"), learned_by_a);

    // 没有个人词库文件的引擎：没有可读的
    assert!(!engine().reload_user_dict().expect("reload"));
    drop(c);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_reports_a_missing_lexicon() {
    let error = ImeEngine::open(&repo_root().join("no/such/lexicon.bin"), None)
        .err()
        .expect("must fail");
    assert!(error.contains("lexicon.bin"), "{error}");
}

// ---------------------------------------------------------------------------
// C 接口
// ---------------------------------------------------------------------------

unsafe fn take(ptr: *mut c_char) -> String {
    assert!(!ptr.is_null());
    let text = CStr::from_ptr(ptr).to_str().unwrap().to_string();
    ime_string_free(ptr);
    text
}

#[test]
fn c_abi_round_trip() {
    let lexicon = CString::new(lexicon_path().to_str().unwrap()).unwrap();
    unsafe {
        assert_eq!(ime_ffi_abi_version(), IME_FFI_ABI_VERSION);
        let mut error: *mut c_char = std::ptr::null_mut();
        let engine = ime_engine_new(lexicon.as_ptr(), std::ptr::null(), &mut error);
        assert!(!engine.is_null());
        assert!(error.is_null());

        for ch in "nihao".chars() {
            let envelope = json(&take(ime_engine_key(engine, IME_KEY_CHAR, ch as u32)));
            assert_eq!(envelope["action"], "update");
        }
        assert!(ime_engine_is_composing(engine));
        assert!(!ime_engine_is_english(engine));
        let view = json(&take(ime_engine_view(engine)));
        assert_eq!(view["visible_candidates"][0]["text"], "你好");
        let refreshed = json(&take(ime_engine_refresh_after_ai(engine)));
        assert_eq!(refreshed["visible_candidates"][0]["text"], "你好");

        let context = CString::new("你好").unwrap();
        ime_engine_set_context(engine, context.as_ptr());
        ime_engine_set_context(engine, std::ptr::null());

        let settings = CString::new(r#"{"page_size":10}"#).unwrap();
        assert_eq!(ime_engine_configure(engine, settings.as_ptr()), 0);
        let bad = CString::new("[").unwrap();
        assert_eq!(ime_engine_configure(engine, bad.as_ptr()), -1);
        assert_eq!(ime_engine_configure(engine, std::ptr::null()), -1);

        let tapped = json(&take(ime_engine_tap_syllable(engine, 1)));
        assert_eq!(tapped["view"]["active_syllable_index"], 1);
        take(ime_engine_tap_syllable(engine, 1));

        let commit = json(&take(ime_engine_select_candidate(engine, 0)));
        assert_eq!(commit["action"], "commit");
        assert_eq!(commit["text"], "你好");
        assert!(!ime_engine_is_composing(engine));
        assert_eq!(json(&take(ime_engine_commit_full_sentence(engine)))["action"], "pass");

        ime_engine_reset(engine);
        ime_engine_free(engine);
    }
}

#[test]
fn c_abi_space_after_paging_and_candidates() {
    let lexicon = CString::new(lexicon_path().to_str().unwrap()).unwrap();
    unsafe {
        let engine = ime_engine_new(lexicon.as_ptr(), std::ptr::null(), std::ptr::null_mut());
        assert!(!engine.is_null());
        // 没有输入
        let idle = json(&take(ime_engine_candidates(engine, 0, 30)));
        assert_eq!(idle, json(r#"{"total":0,"offset":0,"candidates":[]}"#));

        let mut first = Value::Null;
        for ch in "shi".chars() {
            first = json(&take(ime_engine_key(engine, IME_KEY_CHAR, ch as u32)));
        }
        let top = top_text(&first).to_string();
        let total = first["view"]["total_candidates"].as_u64().unwrap();
        assert!(total > 30);

        // 加载更多：不动引擎
        let more = json(&take(ime_engine_candidates(engine, 30, 30)));
        assert_eq!(more["total"], total);
        assert_eq!(more["offset"], 30);
        assert_eq!(more["candidates"].as_array().unwrap().len() as u64, (total - 30).min(30));
        let candidate = more["candidates"][0].as_object().unwrap();
        let mut keys: Vec<&str> = candidate.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["ai_boosted", "consumed_len", "preedit", "tag", "text"]);
        assert_eq!(json(&take(ime_engine_view(engine)))["page_index"], 0);
        assert_eq!(
            json(&take(ime_engine_candidates(engine, 0, u32::MAX)))["candidates"].as_array().unwrap().len() as u64,
            total.min(MAX_CANDIDATES_PER_CALL as u64)
        );
        assert_eq!(
            json(&take(ime_engine_candidates(engine, u32::MAX, u32::MAX)))["candidates"].as_array().unwrap().len(),
            0
        );

        // 别的宿主真的翻了页：视图换页，空格提交的仍然是全部候选里的第 0 个
        let second = json(&take(ime_engine_key(engine, IME_KEY_PAGE_DOWN, 0)));
        assert_eq!(second["view"]["page_index"], 1);
        assert_eq!(second["view"]["visible_candidates"], more["candidates"]);
        let commit = json(&take(ime_engine_key(engine, IME_KEY_SPACE, 0)));
        assert_eq!(commit["action"], "commit");
        assert_eq!(commit["text"], top.as_str());
        assert!(!ime_engine_is_composing(engine));

        assert!(ime_engine_candidates(std::ptr::null_mut(), 0, 30).is_null());
        ime_engine_free(engine);
    }
}

#[test]
fn c_abi_reload_user_dict() {
    let dir = std::env::temp_dir().join(format!("ime-ffi-c-reload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let lexicon = CString::new(lexicon_path().to_str().unwrap()).unwrap();
    let dict = CString::new(dir.join("user_dict.json").to_str().unwrap()).unwrap();
    unsafe {
        assert_eq!(ime_engine_reload_user_dict(std::ptr::null_mut()), -1);
        // 没有启用持久化
        let plain = ime_engine_new(lexicon.as_ptr(), std::ptr::null(), std::ptr::null_mut());
        assert_eq!(ime_engine_reload_user_dict(plain), 0);
        ime_engine_free(plain);

        let reader = ime_engine_new(lexicon.as_ptr(), dict.as_ptr(), std::ptr::null_mut());
        let writer = ime_engine_new(lexicon.as_ptr(), dict.as_ptr(), std::ptr::null_mut());
        assert!(!reader.is_null() && !writer.is_null());
        assert_eq!(ime_engine_reload_user_dict(reader), 0);

        let mut last = Value::Null;
        for ch in "shiyan".chars() {
            last = json(&take(ime_engine_key(writer, IME_KEY_CHAR, ch as u32)));
        }
        let index = second_word_index(&last);
        let learned = last["view"]["visible_candidates"][index]["text"].as_str().unwrap().to_string();
        assert_eq!(json(&take(ime_engine_select_candidate(writer, index as u32)))["action"], "commit");
        // 销毁时等写盘完成
        ime_engine_free(writer);

        assert_eq!(ime_engine_reload_user_dict(reader), 1);
        assert_eq!(ime_engine_reload_user_dict(reader), 0);
        for ch in "shiyan".chars() {
            last = json(&take(ime_engine_key(reader, IME_KEY_CHAR, ch as u32)));
        }
        assert_eq!(top_text(&last), learned);
        ime_engine_reset(reader);

        // 读不出来：-1，内存里的词库不变
        std::fs::write(dir.join("user_dict.json"), b"[").unwrap();
        assert_eq!(ime_engine_reload_user_dict(reader), -1);
        for ch in "shiyan".chars() {
            last = json(&take(ime_engine_key(reader, IME_KEY_CHAR, ch as u32)));
        }
        assert_eq!(top_text(&last), learned);
        ime_engine_free(reader);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn c_abi_tolerates_null_and_bad_arguments() {
    unsafe {
        let mut error: *mut c_char = std::ptr::null_mut();
        let missing = CString::new("/no/such/lexicon.bin").unwrap();
        assert!(ime_engine_new(missing.as_ptr(), std::ptr::null(), &mut error).is_null());
        assert!(!error.is_null());
        assert!(take(error).contains("lexicon.bin"));
        assert!(ime_engine_new(std::ptr::null(), std::ptr::null(), std::ptr::null_mut()).is_null());

        // 个人词库路径给了却不是 UTF-8：报错，而不是悄悄当成「不持久化」
        let lexicon = CString::new(lexicon_path().to_str().unwrap()).unwrap();
        let not_utf8 = CString::new(vec![b'/', b't', b'm', b'p', b'/', 0xFF, 0xFE]).unwrap();
        let mut error: *mut c_char = std::ptr::null_mut();
        assert!(ime_engine_new(lexicon.as_ptr(), not_utf8.as_ptr(), &mut error).is_null());
        assert!(!error.is_null());
        assert!(take(error).contains("user_dict_path"));
        assert!(ime_engine_new(lexicon.as_ptr(), not_utf8.as_ptr(), std::ptr::null_mut()).is_null());

        let null: *mut ime_engine = std::ptr::null_mut();
        assert!(ime_engine_key(null, IME_KEY_CHAR, 'a' as u32).is_null());
        assert!(ime_engine_view(null).is_null());
        assert!(!ime_engine_is_composing(null));
        assert!(!ime_engine_is_english(null));
        ime_engine_set_context(null, std::ptr::null());
        ime_engine_reset(null);
        ime_engine_set_ai_notify(null, None, std::ptr::null_mut());
        ime_engine_free(null);
        ime_string_free(std::ptr::null_mut());
    }
}

#[test]
fn footprint_is_reported_on_apple_platforms() {
    let bytes = ime_phys_footprint();
    if cfg!(any(target_os = "macos", target_os = "ios")) {
        assert!(bytes > 1024 * 1024, "footprint {bytes}");
    } else {
        assert_eq!(bytes, 0);
    }
}

#[test]
fn engine_is_usable_from_several_threads() {
    let engine = std::sync::Arc::new(engine());
    let mut workers = Vec::new();
    for _ in 0..4 {
        let engine = std::sync::Arc::clone(&engine);
        workers.push(std::thread::spawn(move || {
            for _ in 0..25 {
                json(&engine.key(IME_KEY_CHAR, 'n' as u32));
                json(&engine.key(IME_KEY_CHAR, 'i' as u32));
                json(&engine.view());
                engine.is_composing();
                engine.reset();
            }
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert!(!engine.is_composing());
}

// ---------------------------------------------------------------------------
// AI 后端
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Request { id: u64, context: String, pinyin: String, candidates: Vec<String> },
    Cancel(u64),
}

fn recording_chooser() -> (Arc<dyn Fn(ChoiceEvent<'_>) + Send + Sync>, mpsc::Receiver<Seen>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let callback = Arc::new(move |event: ChoiceEvent<'_>| {
        let seen = match event {
            ChoiceEvent::Request { id, context, pinyin, candidates } => Seen::Request {
                id,
                context: context.to_string(),
                pinyin: pinyin.to_string(),
                candidates: candidates.to_vec(),
            },
            ChoiceEvent::Cancel { id } => Seen::Cancel(id),
        };
        let _ = tx.lock().unwrap().send(seen);
    });
    (callback, rx)
}

/// 等到下一个 Request 事件（中途的 Cancel 跳过）
fn next_request(rx: &mpsc::Receiver<Seen>) -> (u64, String, String, Vec<String>) {
    loop {
        match rx.recv_timeout(Duration::from_secs(5)).expect("chooser request") {
            Seen::Request { id, context, pinyin, candidates } => return (id, context, pinyin, candidates),
            Seen::Cancel(_) => continue,
        }
    }
}

#[test]
fn host_chooser_round_trip_promotes_the_delivered_candidate() {
    let engine = engine();
    let notified = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&notified);
    engine.set_notify(Some(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    })));
    let (callback, rx) = recording_chooser();
    engine.set_chooser(Some(callback));

    engine.set_context(Some("我对天发誓这是我的"));
    let before = type_letters(&engine, "shiyan");
    assert_eq!(top_text(&before), "实验");

    // 逐键输入时每个前缀都可能发请求，最后一个才是整串的
    let (id, context, pinyin, candidates) = loop {
        let request = next_request(&rx);
        if request.2 == "shiyan" {
            break request;
        }
    };
    assert_eq!(pinyin, "shiyan");
    assert_eq!(context, "我对天发誓这是我的");
    assert_eq!(candidates[0], "实验");
    assert!(candidates.iter().any(|c| c == "誓言"), "{candidates:?}");

    // 不在候选里的答案、过期的编号都不起作用
    assert_eq!(engine.deliver_choice(id + 1000, Some("誓言")), IME_CHOICE_STALE);
    assert_eq!(notified.load(Ordering::SeqCst), 0);

    assert_eq!(engine.deliver_choice(id, Some("誓言")), IME_CHOICE_ACCEPTED);
    assert_eq!(notified.load(Ordering::SeqCst), 1, "采纳后应当通知宿主刷新");
    // 同一个编号只能答复一次
    assert_eq!(engine.deliver_choice(id, Some("实验")), IME_CHOICE_STALE);

    let after = json(&engine.refresh_after_ai());
    assert_eq!(after["visible_candidates"][0]["text"], "誓言");
    assert_eq!(after["visible_candidates"][0]["ai_boosted"], true);
    assert_eq!(after["composed_full_sentence"], "誓言");
    // 被换下来的原首选还在
    assert!(after["visible_candidates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["text"] == "实验"));

    let commit = json(&engine.key(IME_KEY_SPACE, 0));
    assert_eq!(commit["text"], "誓言");
}

#[test]
fn host_chooser_rejects_answers_outside_the_pool_and_handles_no_answer() {
    let engine = engine();
    let (callback, rx) = recording_chooser();
    engine.set_chooser(Some(callback));
    engine.set_context(Some("我对天发誓这是我的"));

    type_letters(&engine, "shiyan");
    let (id, ..) = loop {
        let request = next_request(&rx);
        if request.2 == "shiyan" {
            break request;
        }
    };
    assert_eq!(engine.deliver_choice(id, Some("石燕子")), IME_CHOICE_IMPLAUSIBLE);
    assert_eq!(top_text(&json(&format!(r#"{{"view":{}}}"#, engine.refresh_after_ai()))), "实验");
    engine.reset();

    // 换一段上文再来一次（上面那次刷新会为同一输入重新发请求，用上文把两轮的请求区分开）
    engine.set_context(Some("他对天发誓这是他的"));
    type_letters(&engine, "shiyan");
    let (id, ..) = loop {
        let request = next_request(&rx);
        if request.1 == "他对天发誓这是他的" && request.2 == "shiyan" {
            break request;
        }
    };
    assert_eq!(engine.deliver_choice(id, None), IME_CHOICE_NO_ANSWER);
    assert_eq!(json(&engine.refresh_after_ai())["visible_candidates"][0]["text"], "实验");
}

#[test]
fn finishing_the_composition_cancels_the_pending_request() {
    let engine = engine();
    let (callback, rx) = recording_chooser();
    engine.set_chooser(Some(callback));
    engine.set_context(Some("我对天发誓这是我的"));
    type_letters(&engine, "shiyan");
    let (id, ..) = loop {
        let request = next_request(&rx);
        if request.2 == "shiyan" {
            break request;
        }
    };
    // 用户不等结果直接上屏：请求作废，宿主收到取消
    let commit = json(&engine.key(IME_KEY_SPACE, 0));
    assert_eq!(commit["text"], "实验");
    let cancelled = loop {
        match rx.recv_timeout(Duration::from_secs(5)).expect("cancel event") {
            Seen::Cancel(cancelled) if cancelled == id => break cancelled,
            _ => continue,
        }
    };
    assert_eq!(cancelled, id);
    assert_eq!(engine.deliver_choice(id, Some("誓言")), IME_CHOICE_STALE);
}

#[test]
fn ai_result_is_applied_after_the_host_paged() {
    // 以前：引擎停在后面的页上时结果到达，刷新不重排、翻回第 0 页也不重排，也不会再有第二次通知，
    // 用户看到并提交的是 AI 之前的顺序
    let engine = engine();
    let notified = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&notified);
    engine.set_notify(Some(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    })));
    let (callback, rx) = recording_chooser();
    engine.set_chooser(Some(callback));
    engine.set_context(Some("我对天发誓这是我的"));
    let before = type_letters(&engine, "shiyan");
    assert_eq!(top_text(&before), "实验");
    let (id, ..) = loop {
        let request = next_request(&rx);
        if request.2 == "shiyan" {
            break request;
        }
    };

    // 宿主翻到后面的页
    let paged = json(&engine.key(IME_KEY_PAGE_DOWN, 0));
    assert_eq!(paged["view"]["page_index"], 1);

    assert_eq!(engine.deliver_choice(id, Some("誓言")), IME_CHOICE_ACCEPTED);
    assert_eq!(notified.load(Ordering::SeqCst), 1);
    let after = json(&engine.refresh_after_ai());
    assert_eq!(after["page_index"], 0);
    assert_eq!(after["visible_candidates"][0]["text"], "誓言");
    assert_eq!(after["visible_candidates"][0]["ai_boosted"], true);
    assert_eq!(json(&engine.candidates(0, 1))["candidates"][0]["text"], "誓言");
    assert_eq!(json(&engine.key(IME_KEY_SPACE, 0))["text"], "誓言");
}

#[test]
fn a_failed_model_load_does_not_disturb_the_pending_chooser_request() {
    // load_llm 先卸载已有模型；没装模型时它不应去动调度器，
    // 否则当前输入正在等的宿主选词结果到达时就不再触发刷新回调
    let engine = engine();
    let notified = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&notified);
    engine.set_notify(Some(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    })));
    let (callback, rx) = recording_chooser();
    engine.set_chooser(Some(callback));
    engine.set_context(Some("我对天发誓这是我的"));
    type_letters(&engine, "shiyan");
    let (id, ..) = loop {
        let request = next_request(&rx);
        if request.2 == "shiyan" {
            break request;
        }
    };

    assert!(engine.load_llm(&repo_root().join("no/such/model.gguf"), 0).is_err());
    assert!(!engine.is_llm_loaded());

    assert_eq!(engine.deliver_choice(id, Some("誓言")), IME_CHOICE_ACCEPTED);
    assert_eq!(notified.load(Ordering::SeqCst), 1);
    assert_eq!(json(&engine.refresh_after_ai())["visible_candidates"][0]["text"], "誓言");
    engine.reset();
    engine.set_chooser(None);
}

#[test]
fn replacing_the_chooser_cancels_through_the_old_callback() {
    let engine = engine();
    let (old_callback, old_rx) = recording_chooser();
    engine.set_chooser(Some(old_callback));
    engine.set_context(Some("我对天发誓这是我的"));
    type_letters(&engine, "shiyan");
    let (id, ..) = loop {
        let request = next_request(&old_rx);
        if request.2 == "shiyan" {
            break request;
        }
    };
    while old_rx.try_recv().is_ok() {}

    // 换成新的选词器：等待中的请求作废。以前这个 Cancel 发给了新回调（它从没见过这个编号），旧回调什么也收不到
    let (new_callback, new_rx) = recording_chooser();
    engine.set_chooser(Some(new_callback));
    assert_eq!(old_rx.try_recv(), Ok(Seen::Cancel(id)), "the old callback is told before set_chooser returns");
    assert!(new_rx.try_recv().is_err(), "the new callback never saw request {id}");
    assert_eq!(engine.deliver_choice(id, Some("誓言")), IME_CHOICE_STALE);

    // 新选词器照常工作；旧回调不再被调用
    engine.reset();
    engine.set_context(Some("他对天发誓这是他的"));
    type_letters(&engine, "shiyan");
    let (new_id, ..) = loop {
        let request = next_request(&new_rx);
        if request.1 == "他对天发誓这是他的" && request.2 == "shiyan" {
            break request;
        }
    };
    assert_eq!(engine.deliver_choice(new_id, Some("誓言")), IME_CHOICE_ACCEPTED);
    assert_eq!(json(&engine.refresh_after_ai())["visible_candidates"][0]["text"], "誓言");
    engine.reset();
    engine.set_chooser(None);
    assert!(old_rx.try_recv().is_err(), "nothing reaches a replaced callback");
}

#[test]
fn removing_the_chooser_stops_requests() {
    let engine = engine();
    let (callback, rx) = recording_chooser();
    engine.set_chooser(Some(callback));
    engine.set_context(Some("我对天发誓这是我的"));
    type_letters(&engine, "shiyan");
    next_request(&rx);
    engine.reset();
    engine.set_chooser(None);
    while rx.try_recv().is_ok() {}

    type_letters(&engine, "shiyan");
    std::thread::sleep(Duration::from_millis(150));
    assert!(rx.try_recv().is_err(), "卸载之后不应再收到请求");
    engine.reset();
}

#[test]
fn no_requests_without_context_for_short_inputs_or_single_characters() {
    let engine = engine();
    let (callback, rx) = recording_chooser();
    engine.set_chooser(Some(callback));
    // 没有上文的短输入、单字查询：规则重排足够，不去打扰模型
    engine.set_context(Some(""));
    type_letters(&engine, "shiyan");
    engine.reset();
    engine.set_context(Some("我对天发誓这是我的"));
    type_letters(&engine, "shi");
    engine.reset();
    std::thread::sleep(Duration::from_millis(150));
    let asked: Vec<(String, String)> = rx
        .try_iter()
        .filter_map(|event| match event {
            Seen::Request { context, pinyin, .. } => Some((context, pinyin)),
            Seen::Cancel(_) => None,
        })
        .collect();
    assert!(
        !asked.iter().any(|(context, pinyin)| context.is_empty() || pinyin == "shi"),
        "{asked:?}"
    );
}

#[test]
fn explicit_override_is_remembered_for_the_same_context() {
    let engine = engine();
    let select = |context: &str, want: &str| {
        engine.set_context(Some(context));
        let last = type_letters(&engine, "shiyan");
        let index = last["view"]["visible_candidates"]
            .as_array()
            .unwrap()
            .iter()
            .position(|c| c["text"] == want)
            .unwrap_or_else(|| panic!("候选里应有 {want}"));
        assert!(index > 0);
        assert_eq!(json(&engine.select_candidate(index))["text"], want);
    };
    let top = |context: &str| -> (String, String) {
        engine.set_context(Some(context));
        let last = type_letters(&engine, "shiyan");
        engine.reset();
        let first = &last["view"]["visible_candidates"][0];
        (first["text"].as_str().unwrap().to_string(), first["tag"].as_str().unwrap().to_string())
    };

    select("我对天发誓这是我的", "誓言");
    assert_eq!(top("我对天发誓这是我的"), ("誓言".to_string(), "[记忆·上文]".to_string()));
    // 关掉之后不再按上文记忆排首位（记录还在，重新打开就恢复）
    engine.configure(r#"{"context_memory":false}"#).unwrap();
    assert_ne!(top("我对天发誓这是我的").1, "[记忆·上文]");
    engine.configure(r#"{"context_memory":true,"memory_blocks_model":false}"#).unwrap();
    assert_eq!(top("我对天发誓这是我的").1, "[记忆·上文]");
}

/// 真模型的测试一个一个跑：各自都要加载一份模型，同时跑的话 phys_footprint 的读数互相干扰
static REAL_MODEL_TESTS: Mutex<()> = Mutex::new(());

fn real_model_test_turn() -> std::sync::MutexGuard<'static, ()> {
    REAL_MODEL_TESTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 真模型：个人记忆形成之后，换了上文模型照样能把词换过来
#[test]
fn local_model_still_decides_by_context_after_memory_has_formed() {
    let Some(model) = std::env::var_os("IME_LLM_TEST_MODEL") else {
        eprintln!("跳过：没有设置 IME_LLM_TEST_MODEL");
        return;
    };
    if !ImeEngine::llm_supported() {
        eprintln!("跳过：这个构建没有启用 llama 特性");
        return;
    }
    let _turn = real_model_test_turn();
    let engine = engine();
    engine.load_llm(std::path::Path::new(&model), 0).expect("load model");
    let top_after_model = |context: &str, pinyin: &str| -> String {
        engine.reset();
        engine.set_context(Some(context));
        type_letters(&engine, pinyin);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while engine.ai_result_pending() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        json(&engine.refresh_after_ai())["visible_candidates"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // 在各自的上文里把「公式」用空格上屏三次，形成个人记忆
    for _ in 0..3 {
        assert_eq!(top_after_model("这道题要用到求根", "gongshi"), "公式");
        assert_eq!(json(&engine.key(IME_KEY_SPACE, 0))["text"], "公式");
    }
    // 换一个上文：模型把「攻势」排到首位（旧做法里个人记忆会挡住模型，首选仍是「公式」）
    assert_eq!(top_after_model("对方发起了猛烈的", "gongshi"), "攻势");
    engine.reset();
    // 旧做法作对照
    engine.configure(r#"{"memory_blocks_model":true}"#).unwrap();
    assert_eq!(top_after_model("对方发起了猛烈的", "gongshi"), "公式");
    engine.reset();
}

#[test]
fn local_model_is_reported_as_unsupported_or_missing() {
    let engine = engine();
    assert!(!engine.is_llm_loaded());
    let error = engine
        .load_llm(&repo_root().join("no/such/model.gguf"), 0)
        .expect_err("must fail");
    assert!(!error.is_empty());
    assert!(!engine.is_llm_loaded());
    engine.unload_llm();
    if !ImeEngine::llm_supported() {
        assert!(error.contains("llama"), "{error}");
    }
}

/// 真模型：已经装着模型时再加载，先卸载旧的。加载失败之后引擎里没有模型（以前旧模型留着，
/// 新旧两份在加载期间同时驻留）
#[test]
fn loading_a_model_unloads_the_installed_one_first() {
    let Some(model) = std::env::var_os("IME_LLM_TEST_MODEL") else {
        eprintln!("跳过：没有设置 IME_LLM_TEST_MODEL");
        return;
    };
    if !ImeEngine::llm_supported() {
        eprintln!("跳过：这个构建没有启用 llama 特性");
        return;
    }
    let _turn = real_model_test_turn();
    let engine = engine();
    let model = std::path::Path::new(&model);
    engine.load_llm(model, 0).expect("load model");
    assert!(engine.is_llm_loaded());
    let one_model = ime_phys_footprint();

    // 换模型（这里用同一个文件）：加载期间的峰值不应接近两份模型
    let peak = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let (peak, stop) = (Arc::clone(&peak), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                peak.fetch_max(ime_phys_footprint(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_micros(200));
            }
        })
    };
    engine.load_llm(model, 0).expect("reload model");
    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap();
    assert!(engine.is_llm_loaded());
    let peak = peak.load(Ordering::Relaxed);
    eprintln!(
        "footprint: one model {:.1} MiB, peak while replacing it {:.1} MiB",
        one_model as f64 / 1048576.0,
        peak as f64 / 1048576.0
    );
    if one_model > 0 {
        // 实测（qwen2.5-0.5b / Qwen3-0.6B，Q4_K_M）：先卸载再加载时峰值不超过单份 5 MiB，
        // 两份同时驻留时多 44–69 MiB。界限取在中间
        const DOUBLE_RESIDENT_MARGIN: u64 = 24 * 1024 * 1024;
        assert!(
            peak < one_model + DOUBLE_RESIDENT_MARGIN,
            "peak {peak} vs one model {one_model}: two models were resident at once"
        );
    }

    // 加载失败：旧模型已经卸掉，不会悄悄留着
    engine
        .load_llm(&repo_root().join("no/such/model.gguf"), 0)
        .expect_err("must fail");
    assert!(!engine.is_llm_loaded());

    // 之后还能再装上并正常工作
    engine.load_llm(model, 0).expect("load again");
    assert!(engine.is_llm_loaded());
    engine.unload_llm();
    assert!(!engine.is_llm_loaded());
}

/// 真模型的端到端测试：IME_LLM_TEST_MODEL 指向 GGUF 文件、并启用 llama 特性时才运行
#[test]
fn local_model_fixes_the_homophone_by_context() {
    let Some(model) = std::env::var_os("IME_LLM_TEST_MODEL") else {
        eprintln!("跳过：没有设置 IME_LLM_TEST_MODEL");
        return;
    };
    if !ImeEngine::llm_supported() {
        eprintln!("跳过：这个构建没有启用 llama 特性");
        return;
    }
    let _turn = real_model_test_turn();
    let engine = engine();
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    engine.set_notify(Some(Arc::new(move || {
        let _ = tx.lock().unwrap().send(());
    })));
    let before_load = ime_phys_footprint();
    engine.load_llm(std::path::Path::new(&model), 0).expect("load model");
    assert!(engine.is_llm_loaded());

    engine.set_context(Some("我对天发誓这是我的"));
    let before = type_letters(&engine, "shiyan");
    assert_eq!(top_text(&before), "实验");
    rx.recv_timeout(Duration::from_secs(20)).expect("模型结果应当触发刷新回调");
    let after = json(&engine.refresh_after_ai());
    assert_eq!(after["visible_candidates"][0]["text"], "誓言");
    assert_eq!(after["visible_candidates"][0]["ai_boosted"], true);
    let peak = ime_phys_footprint();
    eprintln!(
        "footprint: before load {:.1} MiB, after scoring {:.1} MiB",
        before_load as f64 / 1048576.0,
        peak as f64 / 1048576.0
    );
    assert_eq!(json(&engine.key(IME_KEY_SPACE, 0))["text"], "誓言");

    engine.unload_llm();
    assert!(!engine.is_llm_loaded());
    // 卸载后回到规则重排
    engine.set_context(Some("我对天发誓这是我的"));
    type_letters(&engine, "gongyuan");
    engine.reset();
}

unsafe extern "C" fn c_chooser(user: *mut std::ffi::c_void, id: u64, request_json: *const c_char) {
    let sink = &*(user as *const Mutex<Vec<(u64, Option<String>)>>);
    let json = (!request_json.is_null()).then(|| CStr::from_ptr(request_json).to_str().unwrap().to_string());
    sink.lock().unwrap().push((id, json));
}

unsafe extern "C" fn c_notify(user: *mut std::ffi::c_void) {
    (*(user as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst);
}

/// 等 `sink` 里出现拼音为 `pinyin` 的请求，返回它的编号和内容
fn wait_for_c_request(sink: &Mutex<Vec<(u64, Option<String>)>>, pinyin: &str) -> (u64, Value) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let found = sink.lock().unwrap().iter().rev().find_map(|(id, json)| {
            let value: Value = serde_json::from_str(json.as_deref()?).ok()?;
            (value["pinyin"] == pinyin).then_some((*id, value))
        });
        if let Some(found) = found {
            return found;
        }
        assert!(std::time::Instant::now() < deadline, "没有收到选词请求");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn c_abi_ai_result_is_applied_after_paging() {
    // 与 ai_result_is_applied_after_the_host_paged 相同的场景，走 C 接口
    let lexicon = CString::new(lexicon_path().to_str().unwrap()).unwrap();
    let sink: Mutex<Vec<(u64, Option<String>)>> = Mutex::new(Vec::new());
    let notified = AtomicUsize::new(0);
    unsafe {
        let engine = ime_engine_new(lexicon.as_ptr(), std::ptr::null(), std::ptr::null_mut());
        assert!(!engine.is_null());
        ime_engine_set_ai_notify(engine, Some(c_notify), &notified as *const _ as *mut _);
        ime_engine_set_chooser(engine, Some(c_chooser), &sink as *const _ as *mut _);
        let context = CString::new("我对天发誓这是我的").unwrap();
        ime_engine_set_context(engine, context.as_ptr());
        let mut before = Value::Null;
        for ch in "shiyan".chars() {
            before = json(&take(ime_engine_key(engine, IME_KEY_CHAR, ch as u32)));
        }
        assert_eq!(top_text(&before), "实验");
        let (id, _) = wait_for_c_request(&sink, "shiyan");

        // 宿主翻到后面的页，结果这时才到
        let paged = json(&take(ime_engine_key(engine, IME_KEY_PAGE_DOWN, 0)));
        assert_eq!(paged["view"]["page_index"], 1);
        let choice = CString::new("誓言").unwrap();
        assert_eq!(ime_engine_deliver_choice(engine, id, choice.as_ptr()), IME_CHOICE_ACCEPTED);
        assert_eq!(notified.load(Ordering::SeqCst), 1);

        let after = json(&take(ime_engine_refresh_after_ai(engine)));
        assert_eq!(after["page_index"], 0);
        assert_eq!(after["visible_candidates"][0]["text"], "誓言");
        assert_eq!(after["visible_candidates"][0]["ai_boosted"], true);
        let head = json(&take(ime_engine_candidates(engine, 0, 2)));
        assert_eq!(head["candidates"][0]["text"], "誓言");
        let commit = json(&take(ime_engine_key(engine, IME_KEY_SPACE, 0)));
        assert_eq!(commit["action"], "commit");
        assert_eq!(commit["text"], "誓言");

        ime_engine_set_chooser(engine, None, std::ptr::null_mut());
        ime_engine_set_ai_notify(engine, None, std::ptr::null_mut());
        ime_engine_free(engine);
    }
}

#[test]
fn c_abi_chooser_and_local_model() {
    let lexicon = CString::new(lexicon_path().to_str().unwrap()).unwrap();
    let sink: Mutex<Vec<(u64, Option<String>)>> = Mutex::new(Vec::new());
    let notified = AtomicUsize::new(0);
    unsafe {
        let engine = ime_engine_new(lexicon.as_ptr(), std::ptr::null(), std::ptr::null_mut());
        assert!(!engine.is_null());
        ime_engine_set_ai_notify(engine, Some(c_notify), &notified as *const _ as *mut _);
        ime_engine_set_chooser(engine, Some(c_chooser), &sink as *const _ as *mut _);

        let context = CString::new("我对天发誓这是我的").unwrap();
        ime_engine_set_context(engine, context.as_ptr());
        for ch in "shiyan".chars() {
            take(ime_engine_key(engine, IME_KEY_CHAR, ch as u32));
        }
        // 请求在引擎的后台线程上发出
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (id, request) = loop {
            let found = sink.lock().unwrap().iter().rev().find_map(|(id, json)| {
                let value: Value = serde_json::from_str(json.as_deref()?).ok()?;
                (value["pinyin"] == "shiyan").then_some((*id, value))
            });
            if let Some(found) = found {
                break found;
            }
            assert!(std::time::Instant::now() < deadline, "没有收到选词请求");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(request["id"], id);
        assert_eq!(request["context"], "我对天发誓这是我的");
        assert_eq!(request["candidates"][0], "实验");

        let bad = CString::new("不存在").unwrap();
        assert_eq!(ime_engine_deliver_choice(engine, id, bad.as_ptr()), IME_CHOICE_IMPLAUSIBLE);
        assert_eq!(ime_engine_deliver_choice(engine, id, std::ptr::null()), IME_CHOICE_STALE);
        assert_eq!(ime_engine_deliver_choice(std::ptr::null_mut(), id, std::ptr::null()), IME_CHOICE_STALE);
        assert_eq!(notified.load(Ordering::SeqCst), 0);

        // 卸载：之后不再有回调
        ime_engine_reset(engine);
        ime_engine_set_chooser(engine, None, std::ptr::null_mut());
        let seen = sink.lock().unwrap().len();
        for ch in "shiyan".chars() {
            take(ime_engine_key(engine, IME_KEY_CHAR, ch as u32));
        }
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(sink.lock().unwrap().len(), seen);

        // 本地模型：不存在的文件
        assert!(!ime_engine_llm_loaded(engine));
        let missing = CString::new("/no/such/model.gguf").unwrap();
        let mut error: *mut c_char = std::ptr::null_mut();
        assert_eq!(ime_engine_load_llm(engine, missing.as_ptr(), 0, &mut error), -1);
        assert!(!take(error).is_empty());
        assert_eq!(ime_engine_load_llm(engine, std::ptr::null(), 0, std::ptr::null_mut()), -1);
        ime_engine_unload_llm(engine);
        assert!(!ime_engine_llm_loaded(engine));
        let _ = ime_llm_supported();

        ime_engine_free(engine);
    }
}

/// iOS 默认在常用词后面跟上表情；`{"emoji":false}` 关掉。选中表情只上屏表情
#[test]
fn emoji_candidates_are_on_by_default_and_can_be_turned_off() {
    let engine = engine();
    let view = type_letters(&engine, "mao");
    let candidates = view["view"]["visible_candidates"].as_array().unwrap().clone();
    let cat = candidates.iter().position(|c| c["text"] == "猫").expect("候选里应有「猫」");
    assert_eq!(candidates[cat + 1]["text"], "🐈", "{candidates:?}");
    assert_eq!(candidates[cat + 1]["tag"], "[表情]");
    assert_ne!(candidates[0]["tag"], "[表情]");
    let envelope = json(&engine.select_candidate(cat + 1));
    assert_eq!(envelope["text"], "🐈");

    engine.reset();

    // 英文词后面也跟（按小写查）：ok 后面是 👌 🆗
    let view = type_letters(&engine, "ok");
    let candidates = view["view"]["visible_candidates"].as_array().unwrap().clone();
    let ok = candidates.iter().position(|c| c["text"] == "ok").expect("候选里应有英文「ok」");
    assert_eq!(candidates[ok]["tag"], "[英文]");
    assert_eq!(candidates[ok + 1]["text"], "👌", "{candidates:?}");
    assert_eq!(candidates[ok + 2]["text"], "🆗", "{candidates:?}");
    engine.reset();

    assert!(engine.configure(r#"{"emoji":false}"#).is_ok());
    let view = type_letters(&engine, "mao");
    assert!(view["view"]["visible_candidates"].as_array().unwrap().iter().all(|c| c["tag"] != "[表情]"));
    engine.reset();
}

#[test]
fn chooser_memory_setting_accepts_the_four_policies_and_rejects_others() {
    let engine = engine();
    for policy in ["blocked", "contested", "always", "taught"] {
        assert!(engine.configure(&format!(r#"{{"chooser_memory":"{policy}"}}"#)).is_ok(), "{policy}");
    }
    assert!(engine.configure(r#"{"chooser_memory":"sometimes"}"#).is_err());
    assert!(engine.configure(r#"{"chooser_memory":true}"#).is_err());
}

/// iOS 默认 taught：首选只是用户接受首选养成的记忆时照样问选词器；首选是用户主动改选过的词、
/// 而且这串拼音只有它一个词有记忆时不问；用户在这串拼音上用过两个词之后又会问
#[test]
fn memory_top_asks_the_chooser_unless_the_user_taught_it() {
    let context = "我对天发誓这是我的";
    // 只认这次探测发出的请求（上文也要对上）：上屏阶段也打 shiyan，它的请求可能在清空之后才到
    let requested_for_full_input = |rx: &mpsc::Receiver<Seen>| -> bool {
        std::thread::sleep(Duration::from_millis(150));
        rx.try_iter().any(|event| {
            matches!(event, Seen::Request { pinyin, context: asked_context, .. } if pinyin == "shiyan" && asked_context == context)
        })
    };
    // 在和下文无关的上文里打 shiyan，选中 want；返回它在候选里的位置（0 是接受首选，大于 0 是主动改选）
    let commit = |engine: &ImeEngine, want: &str| -> usize {
        engine.set_context(Some("这一句和上面无关"));
        let last = type_letters(engine, "shiyan");
        let index = last["view"]["visible_candidates"]
            .as_array()
            .unwrap()
            .iter()
            .position(|c| c["text"] == want)
            .unwrap_or_else(|| panic!("候选里应有 {want}"));
        let envelope = json(&engine.select_candidate(index));
        assert_eq!(envelope["text"], want);
        index
    };
    let memory_top = |engine: &ImeEngine, rx: &mpsc::Receiver<Seen>, want: &str| -> bool {
        while rx.try_recv().is_ok() {}
        engine.set_context(Some(context));
        let view = type_letters(engine, "shiyan");
        assert_eq!(top_text(&view), want);
        assert!(view["view"]["visible_candidates"][0]["tag"].as_str().unwrap().starts_with("[记忆"));
        let asked = requested_for_full_input(rx);
        engine.reset();
        asked
    };

    // 用户反复接受首选「实验」：首选带个人记忆标签，但只是习惯，上文明确时仍交给选词器
    let habit = engine();
    let (callback, rx) = recording_chooser();
    habit.set_chooser(Some(callback));
    for _ in 0..3 {
        assert_eq!(commit(&habit, "实验"), 0, "「实验」应是没有上文时的首选");
    }
    assert!(memory_top(&habit, &rx, "实验"), "只靠接受首选养成的记忆不该挡住选词器");
    // contested（原先的默认）：只有一个词有记忆，不问
    habit.configure(r#"{"chooser_memory":"contested"}"#).unwrap();
    assert!(!memory_top(&habit, &rx, "实验"));

    // 另一个用户把「誓言」主动改选出来（首选不是它）：教过的词，这串拼音只有它有记忆，不问
    let taught = engine();
    let (callback, rx) = recording_chooser();
    taught.set_chooser(Some(callback));
    assert!(commit(&taught, "誓言") > 0, "「誓言」应是主动改选");
    assert!(!memory_top(&taught, &rx, "誓言"), "用户教过的词不该再让选词器换掉");
    // 用户也用过「实验」：两个词都有记忆，有争议，又交给选词器
    commit(&taught, "实验");
    while rx.try_recv().is_ok() {}
    taught.set_context(Some(context));
    type_letters(&taught, "shiyan");
    assert!(requested_for_full_input(&rx), "两个词都有记忆时应请求选词器");
    taught.reset();
    // 换成 blocked：不问
    taught.configure(r#"{"chooser_memory":"blocked"}"#).unwrap();
    while rx.try_recv().is_ok() {}
    taught.set_context(Some(context));
    type_letters(&taught, "shiyan");
    assert!(!requested_for_full_input(&rx));
    taught.reset();
}

#[test]
fn ai_pending_follows_the_chooser_request() {
    let engine = engine();
    assert!(!engine.ai_result_pending());
    let (callback, rx) = recording_chooser();
    engine.set_chooser(Some(callback));
    engine.set_context(Some("我对天发誓这是我的"));
    type_letters(&engine, "shiyan");
    let (id, ..) = loop {
        let request = next_request(&rx);
        if request.2 == "shiyan" {
            break request;
        }
    };
    assert!(engine.ai_result_pending());
    unsafe {
        let raw = &engine as *const ImeEngine;
        assert!(ime_engine_ai_pending(raw));
    }
    assert_eq!(engine.deliver_choice(id, Some("誓言")), IME_CHOICE_ACCEPTED);
    assert!(!engine.ai_result_pending());
    unsafe {
        assert!(!ime_engine_ai_pending(&engine as *const ImeEngine));
        assert!(!ime_engine_ai_pending(std::ptr::null()));
    }
}

