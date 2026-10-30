//! アプリ状態と画面描画

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use eframe::egui;

use crate::export::command::secs_string;
use crate::export::{ExportEvent, ExportJob, ExportMode, ExportSettings, Quality};
use crate::media::ffmpeg::{self, FfmpegPaths};
use crate::media::player::{PlaybackState, Player, PlayerCommand, PlayerEvent};
use crate::media::probe;
use crate::model::history::History;
use crate::model::timeline::{Segment, Timeline};
use crate::model::{MediaInfo, Timestamp};
use crate::ui::preview::Preview;
use crate::ui::transport::{self, Icon};

/// トランスポート操作
#[derive(Clone, Copy)]
enum TransportAction {
    MarkIn,
    MarkOut,
    GoToIn,
    GoToOut,
    GoToStart,
    GoToEnd,
    Step(i64),
    TogglePlay,
}

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
    /// IN/OUT 入力欄（秒）
    in_text: String,
    out_text: String,
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
            in_text: String::new(),
            out_text: String::new(),
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
                        self.in_text.clear();
                        self.out_text.clear();
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

    /// トランスポート系のキー（Premiere Pro 準拠。テキスト入力中は無効）。
    /// 本格的なキーマップは後続ステップ
    fn handle_keys(&mut self, ctx: &egui::Context) {
        if self.player.is_none() || ctx.text_edit_focused() {
            return;
        }
        use egui::{Key, Modifiers};
        // consume_key は余分な Shift を無視して一致するので、Shift 付きを先に判定する
        let bindings = [
            (Modifiers::SHIFT, Key::ArrowLeft, TransportAction::Step(-5)),
            (Modifiers::SHIFT, Key::ArrowRight, TransportAction::Step(5)),
            (Modifiers::SHIFT, Key::I, TransportAction::GoToIn),
            (Modifiers::SHIFT, Key::O, TransportAction::GoToOut),
            (Modifiers::NONE, Key::Space, TransportAction::TogglePlay),
            (Modifiers::NONE, Key::ArrowLeft, TransportAction::Step(-1)),
            (Modifiers::NONE, Key::ArrowRight, TransportAction::Step(1)),
            (Modifiers::NONE, Key::I, TransportAction::MarkIn),
            (Modifiers::NONE, Key::O, TransportAction::MarkOut),
            (Modifiers::NONE, Key::Home, TransportAction::GoToStart),
            (Modifiers::NONE, Key::End, TransportAction::GoToEnd),
        ];
        let actions: Vec<_> = ctx.input_mut(|i| {
            bindings
                .iter()
                .filter(|(m, k, _)| i.consume_key(*m, *k))
                .map(|(_, _, a)| *a)
                .collect()
        });
        for a in actions {
            self.run_transport(a);
        }
    }

    fn run_transport(&mut self, action: TransportAction) {
        let duration = self.timeline.duration();
        match action {
            TransportAction::MarkIn => {
                self.timeline.set_in(self.position);
                self.refresh_in_out_text();
            }
            TransportAction::MarkOut => {
                self.timeline.set_out(self.position);
                self.refresh_in_out_text();
            }
            // IN/OUT 未設定なら先頭/末尾へ（Premiere と同じ）
            TransportAction::GoToIn => self.send_player(PlayerCommand::Seek(
                self.timeline.in_point.unwrap_or(Timestamp::ZERO),
            )),
            TransportAction::GoToOut => self.send_player(PlayerCommand::Seek(
                self.timeline.out_point.unwrap_or(duration),
            )),
            TransportAction::GoToStart => self.send_player(PlayerCommand::Seek(Timestamp::ZERO)),
            TransportAction::GoToEnd => self.send_player(PlayerCommand::Seek(duration)),
            TransportAction::Step(n) => self.send_player(PlayerCommand::StepFrame(n)),
            TransportAction::TogglePlay => self.toggle_play(),
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
            (Icon::MarkIn, "インをマーク (I)", TransportAction::MarkIn),
            (
                Icon::MarkOut,
                "アウトをマーク (O)",
                TransportAction::MarkOut,
            ),
            (
                Icon::GoToIn,
                "インへ移動 (Shift+I)",
                TransportAction::GoToIn,
            ),
            (
                Icon::StepBack,
                "1フレーム前へ戻る (←)",
                TransportAction::Step(-1),
            ),
            (play.0, play.1, TransportAction::TogglePlay),
            (
                Icon::StepForward,
                "1フレーム先へ進む (→)",
                TransportAction::Step(1),
            ),
            (
                Icon::GoToOut,
                "アウトへ移動 (Shift+O)",
                TransportAction::GoToOut,
            ),
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
        let font = egui::FontId::monospace(14.0);
        let painter = ui.painter();
        painter.text(
            row.left_center(),
            egui::Align2::LEFT_CENTER,
            self.position.to_hms(),
            font.clone(),
            ui.visuals().selection.stroke.color,
        );
        painter.text(
            row.right_center(),
            egui::Align2::RIGHT_CENTER,
            duration.to_hms(),
            font,
            ui.visuals().text_color(),
        );
        if let Some(action) = clicked {
            self.run_transport(action);
        }
    }

    /// IN/OUT 入力欄を現在の IN/OUT 点に合わせる
    fn refresh_in_out_text(&mut self) {
        let text = |t: Option<Timestamp>| t.map(secs_string).unwrap_or_default();
        self.in_text = text(self.timeline.in_point);
        self.out_text = text(self.timeline.out_point);
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
        ui.separator();
        self.in_out_editor(ui);
    }

    /// 動作確認用の簡易 IN/OUT 入力（本格的なタイムライン UI は後続ステップ）
    fn in_out_editor(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("IN/OUT（秒）");
            ui.add(egui::TextEdit::singleline(&mut self.in_text).desired_width(80.0));
            ui.label("〜");
            ui.add(egui::TextEdit::singleline(&mut self.out_text).desired_width(80.0));
            if ui.button("設定").clicked() {
                match (parse_secs(&self.in_text), parse_secs(&self.out_text)) {
                    (Ok(i), Ok(o)) => {
                        self.timeline.clear_in_out();
                        if let Some(o) = o {
                            self.timeline.set_out(o);
                        }
                        if let Some(i) = i {
                            self.timeline.set_in(i);
                        }
                    }
                    _ => self.error = Some("IN/OUT は秒の数値で入力してください".into()),
                }
            }
            if ui.button("クリア").clicked() {
                self.timeline.clear_in_out();
                self.in_text.clear();
                self.out_text.clear();
            }
        });
        let fmt = |t: Option<Timestamp>| t.map_or("—".to_owned(), |t| t.to_hms());
        let ranges = self.timeline.export_ranges();
        let total = ranges
            .iter()
            .fold(Timestamp::ZERO, |acc, r| acc + (r.end - r.start));
        ui.label(format!(
            "IN {} / OUT {}　書き出し対象: {} 区間・{}",
            fmt(self.timeline.in_point),
            fmt(self.timeline.out_point),
            ranges.len(),
            total.to_hms()
        ));
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

/// 秒の入力をタイムスタンプに（空欄は None）
fn parse_secs(s: &str) -> Result<Option<Timestamp>, ()> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let v: f64 = s.parse().map_err(|_| ())?;
    if !v.is_finite() || v < 0.0 {
        return Err(());
    }
    Ok(Some(Timestamp((v * 1e6).round() as i64)))
}

impl eframe::App for EditVideoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_probe(&ctx);
        self.poll_export();
        self.sync_player_timeline();
        self.poll_player(&ctx);
        self.handle_keys(&ctx);

        let open_shortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::O);
        if ui.ctx().input_mut(|i| i.consume_shortcut(&open_shortcut)) {
            self.open_dialog(ui.ctx());
        }
        let export_shortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::M);
        if ui.ctx().input_mut(|i| i.consume_shortcut(&export_shortcut)) {
            self.export_dialog_open();
        }

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
                self.media_info(ui);
                ui.add_space(4.0);
            });
            egui::CentralPanel::default().show(ui, |ui| self.preview.show(ui));
        }

        self.export_windows(&ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_secs_cases() {
        assert_eq!(parse_secs(""), Ok(None));
        assert_eq!(parse_secs(" 2.5 "), Ok(Some(Timestamp(2_500_000))));
        assert_eq!(parse_secs("0"), Ok(Some(Timestamp(0))));
        assert_eq!(parse_secs("-1"), Err(()));
        assert_eq!(parse_secs("abc"), Err(()));
        assert_eq!(parse_secs("inf"), Err(()));
    }
}
