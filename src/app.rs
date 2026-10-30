//! アプリ状態と画面描画

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

use eframe::egui;

use crate::media::ffmpeg::{self, FfmpegPaths};
use crate::media::probe;
use crate::model::MediaInfo;
use crate::model::history::History;
use crate::model::timeline::Timeline;

/// 対応する拡張子（ファイル選択ダイアログのフィルタ）
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "mkv", "webm", "avi", "m4v", "ts", "mts", "m2ts", "wmv", "flv",
];

type ProbeResult = (PathBuf, Result<MediaInfo, String>);

pub struct EditVideoApp {
    /// ffmpeg 探索結果（失敗時はエラー文言）
    ffmpeg: Result<FfmpegPaths, String>,
    timeline: Timeline,
    history: History,
    /// 画面に表示するエラー
    error: Option<String>,
    /// ffprobe 実行中の結果受信
    probing: Option<Receiver<ProbeResult>>,
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
        }
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        if self.probing.is_some() {
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

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let busy = self.probing.is_some();
            if ui
                .add_enabled(!busy, egui::Button::new("開く"))
                .on_hover_text("動画を開く (Ctrl+O)")
                .clicked()
            {
                self.open_dialog(ui.ctx());
            }
            if busy {
                ui.spinner();
                ui.label("読み込み中…");
            }
        });
    }

    fn media_info(&self, ui: &mut egui::Ui) {
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
    }
}

impl eframe::App for EditVideoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_probe();

        let open_shortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::O);
        if ui.ctx().input_mut(|i| i.consume_shortcut(&open_shortcut)) {
            self.open_dialog(ui.ctx());
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
    }
}
