//! アプリ状態と画面描画

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use eframe::egui;

use crate::export::{ExportEvent, ExportJob, ExportMode, ExportSettings, Quality};
use crate::media::ffmpeg::{self, FfmpegPaths};
use crate::media::probe;
use crate::model::history::History;
use crate::model::timeline::Timeline;
use crate::model::{MediaInfo, Timestamp};

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
}

impl EditVideoApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
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
        }
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        if self.probing.is_some() || self.export.is_some() {
            return;
        }
        let ffprobe = match &self.ffmpeg {
            Ok(p) => p.ffprobe.clone(),
            Err(e) => {
                self.error = Some(e.clone());
                return;
            }
        };
        let Some(path) = rfd::FileDialog::new()
            .set_title("動画を開く")
            .add_filter("動画ファイル", VIDEO_EXTENSIONS)
            .add_filter("すべてのファイル", &["*"])
            .pick_file()
        else {
            return;
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

    fn poll_probe(&mut self) {
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
        egui::Grid::new("media_info")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.label("パス");
                ui.label(info.path.display().to_string());
                ui.end_row();
                ui.label("長さ");
                ui.label(info.duration.to_hms());
                ui.end_row();
                ui.label("解像度");
                ui.label(format!("{} × {}", info.width, info.height));
                ui.end_row();
                ui.label("fps");
                ui.label(format!("{:.3} ({})", info.fps.as_f64(), info.fps));
                ui.end_row();
                ui.label("音声");
                ui.label(if info.has_audio { "あり" } else { "なし" });
                ui.end_row();
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
        self.poll_probe();
        self.poll_export();

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

        egui::CentralPanel::default().show(ui, |ui| self.media_info(ui));

        let ctx = ui.ctx().clone();
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
