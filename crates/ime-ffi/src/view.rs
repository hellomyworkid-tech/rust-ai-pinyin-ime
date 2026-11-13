//! 交给宿主的 JSON：`CandidateWindowViewModel` 的精简版。
//!
//! 完整视图里每个候选都带着分词区间、音节区间和打分，30 个候选一次按键要序列化约 10 KB。
//! 界面只用得到文字、拼音、覆盖长度和标签，所以这里只输出这些字段（字段名与完整视图一致，
//! 宿主那边用同一套解码代码即可）。

use ime_core::{Candidate, SegmentSpan};
use ime_session::{CandidateWindowViewModel, KeyAction};
use serde::Serialize;

#[derive(Serialize)]
pub(crate) struct CandidateOut<'a> {
    text: &'a str,
    preedit: &'a str,
    consumed_len: usize,
    tag: &'a str,
    ai_boosted: bool,
}

impl<'a> CandidateOut<'a> {
    fn new(candidate: &'a Candidate) -> Self {
        Self {
            text: &candidate.text,
            preedit: &candidate.preedit,
            consumed_len: candidate.consumed_len,
            tag: &candidate.tag,
            ai_boosted: candidate.ai_boosted,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct ViewOut<'a> {
    visible: bool,
    preedit_text: &'a str,
    page_index: usize,
    page_size: usize,
    total_candidates: usize,
    visible_candidates: Vec<CandidateOut<'a>>,
    selected_candidate_index: usize,
    composed_full_sentence: &'a str,
    full_sentence_parts: &'a [(String, bool)],
    syllable_spans: &'a [SegmentSpan],
    cursor_byte_pos: usize,
    active_syllable_index: Option<usize>,
    ai_semantic_hint: &'a str,
    ghost_text_preview: Option<&'a str>,
}

impl<'a> ViewOut<'a> {
    pub(crate) fn new(vm: &'a CandidateWindowViewModel) -> Self {
        Self {
            visible: vm.visible,
            preedit_text: &vm.preedit_text,
            page_index: vm.page_index,
            page_size: vm.page_size,
            total_candidates: vm.total_candidates,
            visible_candidates: vm.visible_candidates.iter().map(CandidateOut::new).collect(),
            selected_candidate_index: vm.selected_candidate_index,
            composed_full_sentence: &vm.composed_full_sentence,
            full_sentence_parts: &vm.full_sentence_parts,
            syllable_spans: &vm.syllable_spans,
            cursor_byte_pos: vm.cursor_byte_pos,
            active_syllable_index: vm.active_syllable_index,
            ai_semantic_hint: &vm.ai_semantic_hint,
            ghost_text_preview: vm.ghost_text_preview.as_deref(),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "action", rename_all = "lowercase")]
enum Envelope<'a> {
    Update {
        view: ViewOut<'a>,
    },
    Commit {
        text: &'a str,
        ghost: Option<&'a str>,
        view: ViewOut<'a>,
    },
    Pass,
}

pub(crate) const PASS_JSON: &str = r#"{"action":"pass"}"#;

/// `ime_engine_candidates` 的返回值：全部候选里的一段，候选对象与视图里的相同
#[derive(Serialize)]
struct CandidatesOut<'a> {
    total: usize,
    offset: usize,
    candidates: Vec<CandidateOut<'a>>,
}

/// `all` 里的 `[offset, offset + count)`；越界的部分直接没有
pub(crate) fn candidates_json(all: &[Candidate], offset: usize, count: usize) -> String {
    let start = offset.min(all.len());
    let end = offset.saturating_add(count).min(all.len());
    let out = CandidatesOut {
        total: all.len(),
        offset,
        candidates: all[start..end].iter().map(CandidateOut::new).collect(),
    };
    serde_json::to_string(&out).unwrap_or_else(|_| no_candidates_json())
}

pub(crate) fn no_candidates_json() -> String {
    r#"{"total":0,"offset":0,"candidates":[]}"#.to_string()
}

/// 只有视图对象，没有信封
pub(crate) fn view_json(vm: &CandidateWindowViewModel) -> String {
    serde_json::to_string(&ViewOut::new(vm)).unwrap_or_else(|_| idle_view_json())
}

pub(crate) fn idle_view_json() -> String {
    let idle = CandidateWindowViewModel::default();
    serde_json::to_string(&ViewOut::new(&idle)).unwrap_or_else(|_| "{}".to_string())
}

/// 一次按键 / 选词的结果。`after` 是动作完成之后的视图（上屏之后通常已经空闲）
pub(crate) fn action_json(action: &KeyAction, after: &CandidateWindowViewModel) -> String {
    let envelope = match action {
        KeyAction::EatAndUpdateUi(vm) => Envelope::Update {
            view: ViewOut::new(vm),
        },
        KeyAction::EatAndCommit {
            committed_text,
            ghost_text,
        } => Envelope::Commit {
            text: committed_text,
            ghost: ghost_text.as_deref(),
            view: ViewOut::new(after),
        },
        KeyAction::PassThrough => Envelope::Pass,
    };
    serde_json::to_string(&envelope).unwrap_or_else(|_| PASS_JSON.to_string())
}
