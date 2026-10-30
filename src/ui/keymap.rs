//! キーボードショートカット（Adobe Premiere Pro の Windows 既定に準拠）
//!
//! 「修飾キー＋キー（または入力文字）→ `Action`」の表をここに集約し、UI 側は `Action` だけを扱う。

use eframe::egui::{Event, Key, Modifiers};

/// ショートカットで実行する操作
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    TogglePlay,
    /// L（K 押下中は 1 フレーム進む）
    ShuttleRight,
    /// K
    ShuttleStop,
    /// J（K 押下中は 1 フレーム戻る。逆再生は後続ステップ）
    ShuttleLeft,
    Step(i64),
    PrevEdit,
    NextEdit,
    GoToStart,
    GoToEnd,
    GoToIn,
    GoToOut,
    MarkIn,
    MarkOut,
    ClearIn,
    ClearOut,
    ClearInOut,
    MarkClip,
    Split,
    DeleteSelected,
    ToggleSelected,
    /// IN〜OUT 範囲を削除（リフト / 抽出）
    DeleteInOut,
    /// 再生位置〜直前の編集点を削除
    TrimPrev,
    /// 再生位置〜直後の編集点を削除
    TrimNext,
    SelectAll,
    DeselectAll,
    Undo,
    Redo,
    SelectTool,
    RazorTool,
    ZoomIn,
    ZoomOut,
    ZoomFit,
    Open,
    Export,
}

impl Action {
    /// キーを押しっぱなしにしたときの連続入力を受け付けるか
    fn repeatable(self) -> bool {
        matches!(
            self,
            Action::Step(_)
                | Action::PrevEdit
                | Action::NextEdit
                | Action::ZoomIn
                | Action::ZoomOut
                | Action::Undo
                | Action::Redo
        )
    }
}

/// ショートカットの入力条件
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// 修飾キー（完全一致）＋キー
    Key(Modifiers, Key),
    /// 入力された文字（キー配列に依存する記号用。日本語キーボードでは `'` は Shift+7）
    Text(&'static str),
}

const NONE: Modifiers = Modifiers::NONE;
const SHIFT: Modifiers = Modifiers::SHIFT;
const CTRL: Modifiers = Modifiers::COMMAND;
const CTRL_SHIFT: Modifiers = Modifiers {
    shift: true,
    ..Modifiers::COMMAND
};

/// 割り当て表
pub const BINDINGS: &[(Trigger, Action)] = {
    use Action as A;
    use Trigger::{Key as K, Text as T};
    &[
        // 再生・移動
        (K(NONE, Key::Space), A::TogglePlay),
        (K(NONE, Key::L), A::ShuttleRight),
        (K(NONE, Key::K), A::ShuttleStop),
        (K(NONE, Key::J), A::ShuttleLeft),
        (K(NONE, Key::ArrowRight), A::Step(1)),
        (K(NONE, Key::ArrowLeft), A::Step(-1)),
        (K(SHIFT, Key::ArrowRight), A::Step(5)),
        (K(SHIFT, Key::ArrowLeft), A::Step(-5)),
        (K(NONE, Key::ArrowUp), A::PrevEdit),
        (K(NONE, Key::ArrowDown), A::NextEdit),
        (K(NONE, Key::Home), A::GoToStart),
        (K(NONE, Key::End), A::GoToEnd),
        (K(SHIFT, Key::I), A::GoToIn),
        (K(SHIFT, Key::O), A::GoToOut),
        // マーク
        (K(NONE, Key::I), A::MarkIn),
        (K(NONE, Key::O), A::MarkOut),
        (K(CTRL_SHIFT, Key::I), A::ClearIn),
        (K(CTRL_SHIFT, Key::O), A::ClearOut),
        (K(CTRL_SHIFT, Key::X), A::ClearInOut),
        (K(NONE, Key::X), A::MarkClip),
        // 編集
        (K(CTRL, Key::K), A::Split),
        (K(NONE, Key::Delete), A::DeleteSelected),
        (K(NONE, Key::Backspace), A::DeleteSelected),
        (K(SHIFT, Key::Delete), A::DeleteSelected),
        (K(SHIFT, Key::E), A::ToggleSelected),
        (T(";"), A::DeleteInOut),
        (T("'"), A::DeleteInOut),
        (K(NONE, Key::Q), A::TrimPrev),
        (K(NONE, Key::W), A::TrimNext),
        (K(CTRL, Key::A), A::SelectAll),
        (K(CTRL_SHIFT, Key::A), A::DeselectAll),
        (K(CTRL, Key::Z), A::Undo),
        (K(CTRL_SHIFT, Key::Z), A::Redo),
        // ツール・表示・ファイル
        (K(NONE, Key::V), A::SelectTool),
        (K(NONE, Key::C), A::RazorTool),
        (T("="), A::ZoomIn),
        (T("-"), A::ZoomOut),
        (T("\\"), A::ZoomFit),
        (K(CTRL, Key::O), A::Open),
        (K(CTRL, Key::M), A::Export),
    ]
};

/// 修飾キーが一致するか（Windows では Ctrl が ctrl / command の両方に立つので同一視する）
fn same_modifiers(a: Modifiers, b: Modifiers) -> bool {
    a.alt == b.alt && a.shift == b.shift && (a.ctrl || a.command) == (b.ctrl || b.command)
}

/// 入力イベント1件に対応する操作
pub fn lookup(event: &Event) -> Option<Action> {
    match event {
        Event::Key {
            key,
            pressed: true,
            repeat,
            modifiers,
            ..
        } => BINDINGS
            .iter()
            .find(|(t, _)| matches!(t, Trigger::Key(m, k) if k == key && same_modifiers(*m, *modifiers)))
            .map(|&(_, a)| a)
            .filter(|a| !*repeat || a.repeatable()),
        Event::Text(text) => BINDINGS
            .iter()
            .find(|(t, _)| matches!(t, Trigger::Text(s) if s == text))
            .map(|&(_, a)| a),
        _ => None,
    }
}

/// イベント列から操作を取り出し、該当したイベントは取り除く（ボタンのフォーカス移動などに使われないように）
pub fn take_actions(events: &mut Vec<Event>) -> Vec<Action> {
    let mut actions = Vec::new();
    events.retain(|ev| match lookup(ev) {
        Some(a) => {
            actions.push(a);
            false
        }
        None => true,
    });
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(modifiers: Modifiers, key: Key) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    /// Windows の Ctrl は ctrl と command の両方が立つ
    const WIN_CTRL: Modifiers = Modifiers {
        ctrl: true,
        command: true,
        ..Modifiers::NONE
    };

    #[test]
    fn modifiers_are_matched_exactly() {
        assert_eq!(lookup(&key(NONE, Key::I)), Some(Action::MarkIn));
        assert_eq!(lookup(&key(SHIFT, Key::I)), Some(Action::GoToIn));
        let ctrl_shift = Modifiers {
            shift: true,
            ..WIN_CTRL
        };
        assert_eq!(lookup(&key(ctrl_shift, Key::I)), Some(Action::ClearIn));
        assert_eq!(lookup(&key(WIN_CTRL, Key::K)), Some(Action::Split));
        assert_eq!(lookup(&key(NONE, Key::K)), Some(Action::ShuttleStop));
        assert_eq!(lookup(&key(WIN_CTRL, Key::Z)), Some(Action::Undo));
        assert_eq!(lookup(&key(ctrl_shift, Key::Z)), Some(Action::Redo));
        // 割り当てのない組み合わせ
        assert_eq!(lookup(&key(Modifiers::ALT, Key::I)), None);
        assert_eq!(lookup(&key(WIN_CTRL, Key::I)), None);
    }

    #[test]
    fn symbols_use_typed_text() {
        assert_eq!(lookup(&Event::Text(";".into())), Some(Action::DeleteInOut));
        assert_eq!(lookup(&Event::Text("'".into())), Some(Action::DeleteInOut));
        assert_eq!(lookup(&Event::Text("=".into())), Some(Action::ZoomIn));
        assert_eq!(lookup(&Event::Text("-".into())), Some(Action::ZoomOut));
        assert_eq!(lookup(&Event::Text("\\".into())), Some(Action::ZoomFit));
        // 文字キーの入力文字には反応しない（キーイベント側で判定する）
        assert_eq!(lookup(&Event::Text("i".into())), None);
    }

    #[test]
    fn repeat_only_for_repeatable_actions() {
        let held = |k| Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: true,
            modifiers: NONE,
        };
        assert_eq!(lookup(&held(Key::ArrowRight)), Some(Action::Step(1)));
        assert_eq!(lookup(&held(Key::Space)), None);
        // キーを離したイベントは無視
        let released = Event::Key {
            key: Key::Space,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: NONE,
        };
        assert_eq!(lookup(&released), None);
    }

    #[test]
    fn no_duplicate_triggers() {
        for (i, (a, _)) in BINDINGS.iter().enumerate() {
            for (b, _) in &BINDINGS[i + 1..] {
                let dup = match (a, b) {
                    (Trigger::Key(ma, ka), Trigger::Key(mb, kb)) => {
                        ka == kb && same_modifiers(*ma, *mb)
                    }
                    (Trigger::Text(x), Trigger::Text(y)) => x == y,
                    _ => false,
                };
                assert!(!dup, "重複: {a:?}");
            }
        }
    }

    #[test]
    fn take_actions_removes_matched_events() {
        let mut events = vec![
            key(NONE, Key::Space),
            Event::Text(" ".into()),
            key(NONE, Key::F5),
            Event::Text(";".into()),
        ];
        let actions = take_actions(&mut events);
        assert_eq!(actions, vec![Action::TogglePlay, Action::DeleteInOut]);
        assert_eq!(events.len(), 2);
    }
}
