//! アプリ状態と画面描画

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use eframe::egui;

use crate::export::{ExportEvent, ExportJob, ExportMode, ExportSettings, Quality};
use crate::media::ffmpeg::{self, FfmpegPaths};
use crate::media::player::{PlaybackState, Player, PlayerCommand, PlayerEvent};
use crate::media::probe;
use crate::model::history::History;
use crate::model::timeline::{Segment, Timeline};
use crate::model::{MediaInfo, Timestamp};
use crate::ui::keymap::{self, Action};
use crate::ui::preview::Preview;
use crate::ui::timeline::{self, TimelineEvent, TimelineView, Tool};
use crate::ui::transport::{self, Icon};

/// 対応する拡張子（ファイル選択ダイアログのフィルタ）
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "mkv", "webm", "avi", "m4v", "ts", "mts", "m2ts", "wmv", "flv",
];

type ProbeResult = (PathBuf, Result<MediaInfo, String>);

/// 書き出し設定ウィンドウの状態
struct ExportDialog {
    output: PathBuf,
    mode: ExportMode,
    quality: Quality,
}

/// 実行中の書き出し
struct RunningExport {
    job: ExportJob,
    progress: f32,
    cancelling: bool,
}

pub struct EditVideoApp {
    /// ffmpeg 探索結果（失敗時はエラー文言）
    ffmpeg: Result<FfmpegPaths, String>,
    timeline: Timeline,
    history: History,
    /// 画面に表示するエラー
    error: Option<String>,
    /// ffprobe 実行中の結果受信
    probing: Option<Receiver<ProbeResult>>,
    /// 選択中のセグメント番号
    selection: BTreeSet<usize>,
    tool: Tool,
    timeline_view: TimelineView,
    export_dialog: Option<ExportDialog>,
    export: Option<RunningExport>,
    /// 書き出し結果（Ok = 完了メッセージ, Err = 失敗メッセージ）
    export_result: Option<Result<String, String>>,
    player: Option<Player>,
    /// プレイヤーに渡したタイムラインのセグメント（変更検出用）
    player_segments: Vec<Segment>,
    preview: Preview,
    /// 表示中フレームのタイムライン時刻（再生ヘッド）
    position: Timestamp,
    playing: bool,
    /// シークスライダー操作中の値（秒）。フレームが届くまで表示に使う
    scrub: Option<f64>,
    scrub_dragging: bool,
}

impl EditVideoApp {
    /// `initial` を指定すると起動直後にその動画を開く（コマンドライン引数・exe へのドロップ）
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>) -> Self {
        let mut app = Self::empty();
        if let Some(path) = initial {
            app.open_path(&cc.egui_ctx, path);
        }
        app
    }

    fn empty() -> Self {
        let ffmpeg = ffmpeg::locate().map_err(|e| format!("{e:#}"));
        let error = ffmpeg.as_ref().err().cloned();
        Self {
            ffmpeg,
            timeline: Timeline::new(),
            history: History::new(),
            error,
            probing: None,
            selection: BTreeSet::new(),
            tool: Tool::default(),
            timeline_view: TimelineView::default(),
            export_dialog: None,
            export: None,
            export_result: None,
            player: None,
            player_segments: Vec::new(),
            preview: Preview::default(),
            position: Timestamp::ZERO,
            playing: false,
            scrub: None,
            scrub_dragging: false,
        }
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        if self.probing.is_some() || self.export.is_some() {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .set_title("動画を開く")
            .add_filter("動画ファイル", VIDEO_EXTENSIONS)
            .add_filter("すべてのファイル", &["*"])
            .pick_file()
        else {
            return;
        };
        self.open_path(ctx, path);
    }

    fn open_path(&mut self, ctx: &egui::Context, path: PathBuf) {
        let ffprobe = match &self.ffmpeg {
            Ok(p) => p.ffprobe.clone(),
            Err(e) => {
                self.error = Some(e.clone());
                return;
            }
        };
        // ffprobe は UI を止めないよう別スレッドで実行
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = probe::probe(&ffprobe, &path).map_err(|e| format!("{e:#}"));
            let _ = tx.send((path, result));
            ctx.request_repaint();
        });
        self.probing = Some(rx);
        self.error = None;
    }

    fn poll_probe(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.probing else { return };
        match rx.try_recv() {
            Ok((path, result)) => {
                self.probing = None;
                match result {
                    Ok(info) => {
                        // MVP では素材は常に1件
                        let mut tl = Timeline::new();
                        tl.add_source(info);
                        self.timeline = tl;
                        self.history.clear();
                        self.selection.clear();
                        self.timeline_view = TimelineView::default();
                        self.error = None;
                        self.start_player(ctx);
                    }
                    Err(e) => {
                        self.error = Some(format!("{} を開けません: {e}", path.display()));
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.probing = None;
                self.error = Some("ffprobe の実行スレッドが異常終了しました".into());
            }
        }
    }

    /// 現在のタイムラインでプレイヤーを作り直し、先頭フレームを表示する
    fn start_player(&mut self, ctx: &egui::Context) {
        // 既存プレイヤーは先に止める（ffmpeg を終了させる）
        self.player = None;
        self.preview.clear();
        self.position = Timestamp::ZERO;
        self.playing = false;
        self.scrub = None;
        let ffmpeg = match &self.ffmpeg {
            Ok(p) => p.ffmpeg.clone(),
            Err(e) => {
                self.error = Some(e.clone());
                return;
            }
        };
        let notify_ctx = ctx.clone();
        match Player::start(&ffmpeg, self.timeline.clone(), move || {
            notify_ctx.request_repaint()
        }) {
            Ok(player) => {
                player.seek(Timestamp::ZERO);
                self.player = Some(player);
                self.player_segments = self.timeline.segments.clone();
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    fn send_player(&self, cmd: PlayerCommand) {
        if let Some(p) = &self.player {
            p.send(cmd);
        }
    }

    fn toggle_play(&self) {
        self.send_player(if self.playing {
            PlayerCommand::Pause
        } else {
            PlayerCommand::Play
        });
    }

    /// タイムライン（セグメント構成）が変わったらプレイヤーへスナップショットを送る。
    /// IN/OUT は再生に影響しないので対象外
    fn sync_player_timeline(&mut self) {
        let Some(player) = &self.player else { return };
        if self.player_segments != self.timeline.segments {
            player.set_timeline(self.timeline.clone());
            self.player_segments = self.timeline.segments.clone();
        }
    }

    fn poll_player(&mut self, ctx: &egui::Context) {
        let Some(player) = &self.player else { return };
        // 複数届いていても表示するのは最新のフレームだけ
        let mut latest = None;
        while let Some(ev) = player.try_recv() {
            match ev {
                PlayerEvent::Frame {
                    time,
                    width,
                    height,
                    rgba,
                } => latest = Some((time, width, height, rgba)),
                PlayerEvent::State(s) => self.playing = s == PlaybackState::Playing,
                PlayerEvent::Ended => self.playing = false,
                PlayerEvent::Error(e) => self.error = Some(e),
            }
        }
        if let Some((time, width, height, rgba)) = latest {
            self.preview.set_frame(ctx, time, width, height, &rgba);
            self.position = time;
            if !self.scrub_dragging {
                self.scrub = None;
            }
        }
    }

    /// ショートカット（`ui::keymap`）。テキスト入力中は無効
    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.text_edit_focused() {
            return;
        }
        let (actions, k_held) = ctx.input_mut(|i| {
            (
                keymap::take_actions(&mut i.events),
                i.key_down(egui::Key::K),
            )
        });
        for a in actions {
            // K 押しながら L / J はコマ送り
            let a = match a {
                Action::ShuttleRight if k_held => Action::Step(1),
                Action::ShuttleLeft if k_held => Action::Step(-1),
                a => a,
            };
            self.run_action(ctx, a);
        }
    }

    /// 時刻のタイムコード表示（MVP は素材1件なので先頭素材の fps を使う）
    fn timecode(&self, t: Timestamp) -> String {
        match self.timeline.sources.first() {
            Some(src) => src.fps.timecode(t),
            None => t.to_hms(),
        }
    }

    /// 再生ヘッドの表示位置（スクラブ中は操作中の値）
    fn display_position(&self) -> Timestamp {
        self.scrub
            .map_or(self.position, |s| Timestamp((s * 1e6).round() as i64))
    }

    fn seek(&self, t: Timestamp) {
        self.send_player(PlayerCommand::Seek(t));
    }

    /// 編集操作を履歴付きで適用する。セグメントの並びが変わったら選択を解除する
    fn edit(&mut self, f: impl FnOnce(&mut Timeline) -> bool) -> bool {
        let count = self.timeline.segments.len();
        let changed = self.history.apply(&mut self.timeline, f);
        if changed && self.timeline.segments.len() != count {
            self.selection.clear();
        }
        changed
    }

    /// IN/OUT 点の変更を履歴付きで適用する
    fn edit_marks(&mut self, f: impl FnOnce(&mut Timeline)) {
        self.edit(|tl| {
            let before = (tl.in_point, tl.out_point);
            f(tl);
            before != (tl.in_point, tl.out_point)
        });
    }

    /// 選択中の各セグメントに f を適用する
    fn edit_selected(&mut self, f: impl Fn(&mut Timeline, usize) -> bool) {
        let selection: Vec<usize> = self.selection.iter().copied().collect();
        self.edit(|tl| selection.iter().fold(false, |acc, &i| f(tl, i) | acc));
    }

    fn undo_redo(&mut self, redo: bool) {
        let count = self.timeline.segments.len();
        let done = if redo {
            self.history.redo(&mut self.timeline)
        } else {
            self.history.undo(&mut self.timeline)
        };
        if done && self.timeline.segments.len() != count {
            self.selection.clear();
        }
    }

    fn run_action(&mut self, ctx: &egui::Context, action: Action) {
        // 動画を開く前に使えるのはファイル操作だけ
        if self.player.is_none() && !matches!(action, Action::Open | Action::Export) {
            return;
        }
        let duration = self.timeline.duration();
        let pos = self.position;
        match action {
            Action::TogglePlay => self.toggle_play(),
            Action::ShuttleRight => self.send_player(PlayerCommand::Play),
            Action::ShuttleStop => self.send_player(PlayerCommand::Pause),
            // 逆再生は後続ステップ
            Action::ShuttleLeft => {}
            Action::Step(n) => self.send_player(PlayerCommand::StepFrame(n)),
            Action::PrevEdit => {
                if let Some(t) = self.timeline.prev_edit(pos) {
                    self.seek(t);
                }
            }
            Action::NextEdit => {
                if let Some(t) = self.timeline.next_edit(pos) {
                    self.seek(t);
                }
            }
            Action::GoToStart => self.seek(Timestamp::ZERO),
            Action::GoToEnd => self.seek(duration),
            // IN/OUT 未設定なら先頭/末尾へ（Premiere と同じ）
            Action::GoToIn => self.seek(self.timeline.in_point.unwrap_or(Timestamp::ZERO)),
            Action::GoToOut => self.seek(self.timeline.out_point.unwrap_or(duration)),
            Action::MarkIn => self.edit_marks(|tl| tl.set_in(pos)),
            Action::MarkOut => self.edit_marks(|tl| tl.set_out(pos)),
            Action::ClearIn => self.edit_marks(Timeline::clear_in),
            Action::ClearOut => self.edit_marks(Timeline::clear_out),
            Action::ClearInOut => self.edit_marks(Timeline::clear_in_out),
            Action::MarkClip => {
                let range = self
                    .timeline
                    .locate(pos)
                    .and_then(|(i, _)| self.timeline.segment_range(i));
                if let Some((a, b)) = range {
                    self.edit_marks(|tl| {
                        tl.clear_in_out();
                        tl.set_in(a);
                        tl.set_out(b);
                    });
                }
            }
            Action::Split => {
                self.edit(|tl| tl.split_at(pos));
            }
            Action::DeleteSelected => self.edit_selected(Timeline::delete),
            Action::ToggleSelected => self.edit_selected(Timeline::toggle),
            Action::DeleteInOut => {
                if let Some((a, b)) = self.timeline.in_out_range() {
                    self.edit(|tl| tl.delete_range(a, b));
                }
            }
            Action::TrimPrev => {
                if let Some(a) = self.timeline.prev_edit(pos) {
                    self.edit(|tl| tl.delete_range(a, pos));
                }
            }
            Action::TrimNext => {
                if let Some(b) = self.timeline.next_edit(pos) {
                    self.edit(|tl| tl.delete_range(pos, b));
                }
            }
            Action::SelectAll => self.selection = (0..self.timeline.segments.len()).collect(),
            Action::DeselectAll => self.selection.clear(),
            Action::Undo => self.undo_redo(false),
            Action::Redo => self.undo_redo(true),
            Action::SelectTool => self.tool = Tool::Select,
            Action::RazorTool => self.tool = Tool::Razor,
            // キー操作のズームは再生ヘッドを基準にする（Premiere と同じ）
            Action::ZoomIn => self
                .timeline_view
                .zoom_in(&self.timeline, pos.as_secs_f64()),
            Action::ZoomOut => self
                .timeline_view
                .zoom_out(&self.timeline, pos.as_secs_f64()),
            Action::ZoomFit => self.timeline_view.zoom_fit(),
            Action::Open => self.open_dialog(ctx),
            Action::Export => self.export_dialog_open(),
        }
    }

    fn timeline_panel(&mut self, ui: &mut egui::Ui) {
        let events = self.timeline_view.show(
            ui,
            &self.timeline,
            self.display_position(),
            &self.selection,
            self.tool,
        );
        for ev in events {
            match ev {
                TimelineEvent::Seek(t) => {
                    self.scrub = Some(t.as_secs_f64());
                    self.seek(t);
                }
                TimelineEvent::Select { idx, add } => match (idx, add) {
                    (Some(i), true) => {
                        if !self.selection.remove(&i) {
                            self.selection.insert(i);
                        }
                    }
                    (Some(i), false) => self.selection = BTreeSet::from([i]),
                    (None, true) => {}
                    (None, false) => self.selection.clear(),
                },
                TimelineEvent::Split(t) => {
                    let t = self.timeline.snap_to_frame(t);
                    self.edit(|tl| tl.split_at(t));
                }
            }
        }
        // シークバーとタイムラインのどちらかでドラッグ中ならスクラブ扱い
        self.scrub_dragging |= self.timeline_view.dragging();
    }

    /// 選択中のセグメント・ツール・IN/OUT・書き出し対象の表示と編集ボタン
    fn edit_bar(&mut self, ui: &mut egui::Ui) {
        let mut action = None;
        ui.horizontal(|ui| {
            for (tool, label, tip) in [
                (Tool::Select, "選択", "選択ツール (V)"),
                (Tool::Razor, "レーザー", "レーザーツール (C)"),
            ] {
                if ui
                    .selectable_label(self.tool == tool, label)
                    .on_hover_text(tip)
                    .clicked()
                {
                    self.tool = tool;
                }
            }
            ui.separator();
            let selected = match self.selection.len() {
                0 => "選択: なし".to_owned(),
                1 => {
                    let i = *self.selection.first().unwrap();
                    let (a, b) = self.timeline.segment_range(i).unwrap_or_default();
                    format!(
                        "選択: セグメント{} {}–{}",
                        i + 1,
                        self.timecode(a),
                        self.timecode(b)
                    )
                }
                n => format!("選択: {n} セグメント"),
            };
            ui.label(selected);
            let has_sel = !self.selection.is_empty();
            let buttons = [
                ("分割", "再生位置で分割 (Ctrl+K)", true, Action::Split),
                (
                    "削除",
                    "選択セグメントを削除 (Delete)",
                    has_sel,
                    Action::DeleteSelected,
                ),
            ];
            for (label, tip, enabled, a) in buttons {
                if ui
                    .add_enabled(enabled, egui::Button::new(label))
                    .on_hover_text(tip)
                    .clicked()
                {
                    action = Some(a);
                }
            }
            if ui
                .add_enabled(has_sel, egui::Button::new("復元"))
                .on_hover_text("選択セグメントを復元（有効/無効の切り替えは Shift+E）")
                .clicked()
            {
                self.edit_selected(Timeline::restore);
            }
            ui.separator();
            let fmt = |t: Option<Timestamp>| t.map_or("—".to_owned(), |t| self.timecode(t));
            let ranges = self.timeline.export_ranges();
            let total = ranges
                .iter()
                .fold(Timestamp::ZERO, |acc, r| acc + (r.end - r.start));
            ui.label(format!(
                "IN {} / OUT {}　書き出し対象: {} 区間・{}",
                fmt(self.timeline.in_point),
                fmt(self.timeline.out_point),
                ranges.len(),
                self.timecode(total)
            ));
        });
        if let Some(a) = action {
            self.run_action(ui.ctx(), a);
        }
    }

    /// トランスポート（Premiere Pro のプログラムモニター風）。
    /// 上段: シークバー、下段: 現在時刻（左）・操作ボタン（中央）・総尺（右）
    fn transport(&mut self, ui: &mut egui::Ui) {
        if self.player.is_none() {
            return;
        }
        let duration = self.timeline.duration();
        ui.scope(|ui| {
            ui.spacing_mut().slider_width = ui.available_width();
            let mut secs = self.scrub.unwrap_or_else(|| self.position.as_secs_f64());
            let resp = ui
                .add(egui::Slider::new(&mut secs, 0.0..=duration.as_secs_f64()).show_value(false));
            // ドラッグ中も Seek を送る（スクラブ。古い要求はプレイヤー側で破棄される）
            if resp.changed() {
                self.scrub = Some(secs);
                self.send_player(PlayerCommand::Seek(Timestamp((secs * 1e6).round() as i64)));
            }
            self.scrub_dragging = resp.dragged();
        });

        let play = if self.playing {
            (Icon::Stop, "再生/停止 (Space)")
        } else {
            (Icon::Play, "再生/停止 (Space)")
        };
        let buttons = [
            (Icon::MarkIn, "インをマーク (I)", Action::MarkIn),
            (Icon::MarkOut, "アウトをマーク (O)", Action::MarkOut),
            (Icon::GoToIn, "インへ移動 (Shift+I)", Action::GoToIn),
            (Icon::StepBack, "1フレーム前へ戻る (←)", Action::Step(-1)),
            (play.0, play.1, Action::TogglePlay),
            (Icon::StepForward, "1フレーム先へ進む (→)", Action::Step(1)),
            (Icon::GoToOut, "アウトへ移動 (Shift+O)", Action::GoToOut),
        ];
        let gap = 2.0;
        let (row, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), transport::BUTTON_SIZE.y),
            egui::Sense::hover(),
        );
        let buttons_w = buttons.len() as f32 * (transport::BUTTON_SIZE.x + gap) - gap;
        let mut x = row.center().x - buttons_w / 2.0;
        let mut clicked = None;
        for (icon, tip, action) in buttons {
            let rect = egui::Rect::from_min_size(egui::pos2(x, row.top()), transport::BUTTON_SIZE);
            if transport::icon_button(ui, rect, icon, tip).clicked() {
                clicked = Some(action);
            }
            x += transport::BUTTON_SIZE.x + gap;
        }
        // 時刻表示は Premiere と同じく、現在時刻を再生ヘッドと同じ青、総尺を通常色で出す
        let font = egui::FontId::proportional(16.0);
        let painter = ui.painter();
        painter.text(
            row.left_center(),
            egui::Align2::LEFT_CENTER,
            self.timecode(self.display_position()),
            font.clone(),
            timeline::PLAYHEAD_COLOR,
        );
        painter.text(
            row.right_center(),
            egui::Align2::RIGHT_CENTER,
            self.timecode(duration),
            font,
            ui.visuals().text_color(),
        );
        if let Some(action) = clicked {
            let ctx = ui.ctx().clone();
            self.run_action(&ctx, action);
        }
    }

    fn can_export(&self) -> bool {
        !self.timeline.sources.is_empty()
            && self.probing.is_none()
            && self.export.is_none()
            && self.export_dialog.is_none()
    }

    /// 保存先を選んで書き出し設定ウィンドウを開く
    fn export_dialog_open(&mut self) {
        if !self.can_export() {
            return;
        }
        let Some(src) = self.timeline.sources.first() else {
            return;
        };
        let stem = src
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".into());
        let mut dialog = rfd::FileDialog::new()
            .set_title("書き出し先")
            .set_file_name(format!("{stem}_edit.mp4"))
            .add_filter("MP4", &["mp4"]);
        if let Some(dir) = src.path.parent() {
            dialog = dialog.set_directory(dir);
        }
        let Some(output) = dialog.save_file() else {
            return;
        };
        self.export_dialog = Some(ExportDialog {
            output,
            mode: ExportMode::default(),
            quality: Quality::default(),
        });
    }

    fn start_export(&mut self, ctx: &egui::Context, settings: ExportSettings) {
        let ffmpeg = match &self.ffmpeg {
            Ok(p) => p.ffmpeg.clone(),
            Err(e) => {
                self.error = Some(e.clone());
                return;
            }
        };
        let ctx = ctx.clone();
        match ExportJob::start(&ffmpeg, &self.timeline, settings, move || {
            ctx.request_repaint()
        }) {
            Ok(job) => {
                self.export = Some(RunningExport {
                    job,
                    progress: 0.0,
                    cancelling: false,
                });
                self.export_result = None;
            }
            Err(e) => self.export_result = Some(Err(format!("{e:#}"))),
        }
    }

    fn poll_export(&mut self) {
        let Some(running) = &mut self.export else {
            return;
        };
        while let Some(ev) = running.job.try_recv() {
            let result = match ev {
                ExportEvent::Progress(p) => {
                    running.progress = p;
                    continue;
                }
                ExportEvent::Finished(path) => Ok(format!("書き出しました: {}", path.display())),
                ExportEvent::Failed(msg) => Err(msg),
                ExportEvent::Cancelled => Ok("書き出しをキャンセルしました".into()),
            };
            self.export_result = Some(result);
            self.export = None;
            return;
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let busy = self.probing.is_some();
            if ui
                .add_enabled(!busy && self.export.is_none(), egui::Button::new("開く"))
                .on_hover_text("動画を開く (Ctrl+O)")
                .clicked()
            {
                self.open_dialog(ui.ctx());
            }
            if ui
                .add_enabled(self.can_export(), egui::Button::new("書き出し"))
                .on_hover_text("書き出し (Ctrl+M)")
                .clicked()
            {
                self.export_dialog_open();
            }
            if busy {
                ui.spinner();
                ui.label("読み込み中…");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 右から並ぶので Redo を先に置く
                if ui
                    .add_enabled(self.history.can_redo(), egui::Button::new("Redo"))
                    .on_hover_text("やり直し (Ctrl+Shift+Z)")
                    .clicked()
                {
                    self.undo_redo(true);
                }
                if ui
                    .add_enabled(self.history.can_undo(), egui::Button::new("Undo"))
                    .on_hover_text("取り消し (Ctrl+Z)")
                    .clicked()
                {
                    self.undo_redo(false);
                }
            });
        });
    }

    fn media_info(&mut self, ui: &mut egui::Ui) {
        let Some(info) = self.timeline.sources.first() else {
            ui.label("「開く」(Ctrl+O) で動画ファイルを選択してください。");
            return;
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(info.path.display().to_string());
            ui.separator();
            ui.label(format!("長さ {}", info.duration.to_hms()));
            ui.separator();
            ui.label(format!("{} × {}", info.width, info.height));
            ui.separator();
            ui.label(format!("{:.3} fps ({})", info.fps.as_f64(), info.fps));
            ui.separator();
            ui.label(if info.has_audio {
                "音声あり"
            } else {
                "音声なし"
            });
        });
    }

    fn export_windows(&mut self, ctx: &egui::Context) {
        if let Some(dialog) = &mut self.export_dialog {
            let mut start = false;
            let mut close = false;
            egui::Window::new("書き出し設定")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!("出力先: {}", dialog.output.display()));
                    ui.separator();
                    ui.label("モード");
                    for m in [ExportMode::Accurate, ExportMode::Fast] {
                        ui.radio_value(&mut dialog.mode, m, m.label());
                    }
                    ui.add_enabled_ui(dialog.mode == ExportMode::Accurate, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("画質");
                            for q in [Quality::High, Quality::Standard, Quality::Light] {
                                ui.radio_value(
                                    &mut dialog.quality,
                                    q,
                                    format!("{}（CRF {}）", q.label(), q.crf()),
                                );
                            }
                        });
                    });
                    ui.separator();
                    ui.horizontal(|ui| {
                        start = ui.button("実行").clicked();
                        close = ui.button("キャンセル").clicked();
                    });
                });
            if start {
                let settings = ExportSettings {
                    mode: dialog.mode,
                    quality: dialog.quality,
                    output: dialog.output.clone(),
                };
                self.export_dialog = None;
                self.start_export(ctx, settings);
            } else if close {
                self.export_dialog = None;
            }
        }

        if let Some(running) = &mut self.export {
            egui::Window::new("書き出し中")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.add(
                        egui::ProgressBar::new(running.progress)
                            .show_percentage()
                            .desired_width(320.0),
                    );
                    if running.cancelling {
                        ui.label("キャンセル中…");
                    } else if ui.button("キャンセル").clicked() {
                        running.job.cancel();
                        running.cancelling = true;
                    }
                });
            // 通知が来なくても定期的に状態を確認する
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        if let Some(result) = &self.export_result {
            let mut close = false;
            egui::Window::new("書き出し結果")
                .collapsible(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    match result {
                        Ok(msg) => {
                            ui.label(msg);
                        }
                        Err(msg) => {
                            ui.colored_label(ui.visuals().error_fg_color, "書き出しに失敗しました");
                            egui::ScrollArea::vertical()
                                .max_height(300.0)
                                .show(ui, |ui| {
                                    ui.add(egui::Label::new(egui::RichText::new(msg).monospace()));
                                });
                        }
                    }
                    close = ui.button("閉じる").clicked();
                });
            if close {
                self.export_result = None;
            }
        }
    }
}

impl eframe::App for EditVideoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_probe(&ctx);
        self.poll_export();
        self.poll_player(&ctx);
        self.handle_keys(&ctx);

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));

        if let Some(err) = self.error.clone() {
            egui::Panel::bottom("error").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, format!("エラー: {err}"));
                    if ui.small_button("閉じる").clicked() {
                        self.error = None;
                    }
                });
            });
        }

        if self.timeline.sources.is_empty() {
            egui::CentralPanel::default().show(ui, |ui| self.media_info(ui));
        } else {
            egui::Panel::bottom("controls").show(ui, |ui| {
                ui.add_space(4.0);
                self.transport(ui);
                ui.separator();
                self.timeline_panel(ui);
                self.edit_bar(ui);
                ui.separator();
                self.media_info(ui);
                ui.add_space(4.0);
            });
            egui::CentralPanel::default().show(ui, |ui| self.preview.show(ui));
        }

        self.export_windows(&ctx);
        // 今フレームの編集をプレイヤーへ反映する
        self.sync_player_timeline();
    }
}
