//! タイムライン（セグメント・IN/OUT・再生ヘッドの表示とマウス操作）
//!
//! 描画とマウス操作の結果は `TimelineEvent` として返し、編集はアプリ側で行う。

use std::collections::BTreeSet;

use eframe::egui::{self, Color32, Rect, Stroke, pos2, vec2};

use crate::model::Timestamp;
use crate::model::timeline::Timeline;

/// 時刻目盛りの高さ
const RULER_HEIGHT: f32 = 22.0;
/// セグメント表示の高さ
const TRACK_HEIGHT: f32 = 44.0;
/// 目盛りラベルの最小間隔（px）
const MIN_TICK_SPACING: f64 = 90.0;
/// 拡大の上限（1フレームあたりの px）
const MAX_PX_PER_FRAME: f64 = 40.0;
/// キー操作 1 回の拡大率
const ZOOM_STEP: f64 = 1.5;

const CLIP_COLOR: Color32 = Color32::from_rgb(88, 120, 180);
const CLIP_SELECTED_COLOR: Color32 = Color32::from_rgb(135, 170, 235);
const DELETED_COLOR: Color32 = Color32::from_gray(64);
const DELETED_SELECTED_COLOR: Color32 = Color32::from_gray(105);
pub const PLAYHEAD_COLOR: Color32 = Color32::from_rgb(45, 140, 235);
const RAZOR_COLOR: Color32 = Color32::from_rgb(235, 90, 70);

/// 編集ツール
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tool {
    /// クリックでセグメント選択
    #[default]
    Select,
    /// クリック位置で分割
    Razor,
}

/// マウス操作の結果
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelineEvent {
    /// 再生ヘッドを移動
    Seek(Timestamp),
    /// セグメントを選択（None = 選択解除）。add = Ctrl+クリックで追加・解除
    Select { idx: Option<usize>, add: bool },
    /// レーザーツールで分割
    Split(Timestamp),
}

/// 表示範囲（ズーム・スクロール）の状態
#[derive(Debug)]
pub struct TimelineView {
    /// 表示左端（秒）
    start: f64,
    /// 1秒あたりの px。None = 全体表示
    pps: Option<f64>,
    /// 直近の描画幅（キー操作のズーム計算用）
    width: f32,
    /// 直近に描画した再生ヘッド（移動を検出して表示範囲を追従させる）
    last_position: Timestamp,
    dragging: bool,
}

impl Default for TimelineView {
    fn default() -> Self {
        Self {
            start: 0.0,
            pps: None,
            width: 1.0,
            last_position: Timestamp::ZERO,
            dragging: false,
        }
    }
}

impl TimelineView {
    /// 再生ヘッドをドラッグ中か
    pub fn dragging(&self) -> bool {
        self.dragging
    }

    /// 全体表示に戻す
    pub fn zoom_fit(&mut self) {
        self.pps = None;
        self.start = 0.0;
    }

    /// anchor（秒）の画面位置を保って factor 倍に拡大する
    pub fn zoom(&mut self, tl: &Timeline, anchor: f64, factor: f64) {
        let duration = tl.duration().as_secs_f64();
        let width = f64::from(self.width);
        let min = fit_pps(width, duration);
        let max = (max_fps(tl) * MAX_PX_PER_FRAME).max(min);
        let pps = self.pps.unwrap_or(min);
        let (start, pps) = zoom_about(self.start, pps, anchor, factor, min, max);
        if pps <= min {
            self.zoom_fit();
        } else {
            self.pps = Some(pps);
            self.start = clamp_start(start, pps, width, duration);
        }
    }

    pub fn zoom_in(&mut self, tl: &Timeline, anchor: f64) {
        self.zoom(tl, anchor, ZOOM_STEP);
    }

    pub fn zoom_out(&mut self, tl: &Timeline, anchor: f64) {
        self.zoom(tl, anchor, 1.0 / ZOOM_STEP);
    }

    /// タイムラインを描画し、マウス操作の結果を返す
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        tl: &Timeline,
        position: Timestamp,
        selection: &BTreeSet<usize>,
        tool: Tool,
    ) -> Vec<TimelineEvent> {
        let (rect, resp) = ui.allocate_exact_size(
            vec2(ui.available_width(), RULER_HEIGHT + TRACK_HEIGHT),
            egui::Sense::click_and_drag(),
        );
        let duration = tl.duration().as_secs_f64();
        self.width = rect.width().max(1.0);
        let width = f64::from(self.width);
        let mut events = Vec::new();

        // ホイール: Alt+ホイールでズーム、それ以外は横スクロール
        if resp.hovered() {
            let (scroll, alt, pointer) = ui.input(|i| {
                (
                    i.smooth_scroll_delta,
                    i.modifiers.alt,
                    i.pointer.hover_pos(),
                )
            });
            let delta = f64::from(scroll.x + scroll.y);
            if delta != 0.0 {
                if alt {
                    let anchor = pointer.map_or(self.start, |p| {
                        self.x_to_secs(p.x, rect.left(), width, duration)
                    });
                    self.zoom(tl, anchor, (delta * 0.005).exp());
                } else if let Some(pps) = self.pps {
                    self.start = clamp_start(self.start - delta / pps, pps, width, duration);
                }
            }
        }

        let pps = self.pps.unwrap_or_else(|| fit_pps(width, duration));
        if self.pps.is_none() {
            self.start = 0.0;
        }
        // 再生・キー操作で再生ヘッドが画面外に出たら表示範囲を送る
        if position != self.last_position && !self.dragging {
            self.start = clamp_start(
                follow(self.start, pps, width, position.as_secs_f64()),
                pps,
                width,
                duration,
            );
        }
        self.last_position = position;

        let ruler = Rect::from_min_size(rect.min, vec2(rect.width(), RULER_HEIGHT));
        let track = Rect::from_min_max(pos2(rect.left(), ruler.bottom()), rect.max);
        let to_x = |t: f64| secs_to_x(t, self.start, pps, rect.left());
        let to_secs = |x: f32| x_to_secs(x, self.start, pps, rect.left()).clamp(0.0, duration);
        let to_ts = |x: f32| Timestamp((to_secs(x) * 1e6).round() as i64);

        // マウス操作
        let press = resp.interact_pointer_pos();
        if resp.drag_started() || (resp.dragged() && resp.drag_delta().x != 0.0) {
            if let Some(p) = press {
                events.push(TimelineEvent::Seek(to_ts(p.x)));
            }
        } else if resp.clicked()
            && let Some(p) = press
        {
            let t = to_ts(p.x);
            if ruler.contains(p) {
                events.push(TimelineEvent::Seek(t));
            } else {
                match tool {
                    Tool::Select => {
                        let idx = (t < tl.duration())
                            .then(|| tl.locate(t).map(|(i, _)| i))
                            .flatten();
                        let add = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
                        events.push(TimelineEvent::Select { idx, add });
                    }
                    Tool::Razor => events.push(TimelineEvent::Split(t)),
                }
            }
        }
        self.dragging = resp.dragged();
        if tool == Tool::Razor && resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }

        // 描画
        let painter = ui.painter_at(rect);
        let visuals = ui.visuals();
        painter.rect_filled(rect, 0.0, visuals.extreme_bg_color);
        painter.rect_filled(ruler, 0.0, visuals.faint_bg_color);

        // 目盛り
        let step = tick_step(pps);
        let text_color = visuals.weak_text_color();
        let font = egui::FontId::monospace(10.0);
        let mut i = (self.start / step).floor() as i64;
        loop {
            let t = i as f64 * step;
            let x = to_x(t);
            if x > rect.right() || t > duration + step {
                break;
            }
            if t >= 0.0 {
                painter.line_segment(
                    [pos2(x, ruler.bottom() - 6.0), pos2(x, ruler.bottom())],
                    Stroke::new(1.0, text_color),
                );
                painter.text(
                    pos2(x + 3.0, ruler.top() + 2.0),
                    egui::Align2::LEFT_TOP,
                    tick_label(t, step),
                    font.clone(),
                    text_color,
                );
            }
            i += 1;
        }

        // セグメント
        let mut seg_start = 0.0;
        for (idx, seg) in tl.segments.iter().enumerate() {
            let seg_end = seg_start + seg.duration().as_secs_f64();
            let (x0, x1) = (to_x(seg_start), to_x(seg_end));
            seg_start = seg_end;
            if x1 < rect.left() || x0 > rect.right() {
                continue;
            }
            let selected = selection.contains(&idx);
            let color = match (seg.enabled, selected) {
                (true, false) => CLIP_COLOR,
                (true, true) => CLIP_SELECTED_COLOR,
                (false, false) => DELETED_COLOR,
                (false, true) => DELETED_SELECTED_COLOR,
            };
            // 隣のセグメントとの境目を見せるため左右を少し空ける
            let r = Rect::from_min_max(
                pos2(x0 + 0.5, track.top() + 4.0),
                pos2((x1 - 0.5).max(x0 + 1.0), track.bottom() - 4.0),
            );
            painter.rect_filled(r, 2.0, color);
            if selected {
                painter.rect_stroke(
                    r,
                    2.0,
                    Stroke::new(1.5, Color32::WHITE),
                    egui::StrokeKind::Inside,
                );
            }
        }

        // IN/OUT 範囲
        if let Some((a, b)) = tl.in_out_range() {
            let (x0, x1) = (to_x(a.as_secs_f64()), to_x(b.as_secs_f64()));
            painter.rect_filled(
                Rect::from_min_max(pos2(x0, ruler.top()), pos2(x1, ruler.bottom())),
                0.0,
                Color32::from_white_alpha(40),
            );
            painter.rect_filled(
                Rect::from_min_max(pos2(x0, track.top()), pos2(x1, track.bottom())),
                0.0,
                Color32::from_white_alpha(18),
            );
            let mark = Stroke::new(1.0, Color32::from_white_alpha(160));
            if tl.in_point.is_some() {
                painter.vline(x0, rect.y_range(), mark);
            }
            if tl.out_point.is_some() {
                painter.vline(x1, rect.y_range(), mark);
            }
        }

        // レーザーツールの分割位置
        if tool == Tool::Razor
            && let Some(p) = resp.hover_pos()
            && track.contains(p)
        {
            painter.vline(p.x, track.y_range(), Stroke::new(1.0, RAZOR_COLOR));
        }

        // 再生ヘッド
        let x = to_x(position.as_secs_f64());
        painter.vline(x, rect.y_range(), Stroke::new(1.5, PLAYHEAD_COLOR));
        painter.add(egui::Shape::convex_polygon(
            vec![
                pos2(x - 6.0, ruler.top()),
                pos2(x + 6.0, ruler.top()),
                pos2(x + 6.0, ruler.top() + 8.0),
                pos2(x, ruler.top() + 14.0),
                pos2(x - 6.0, ruler.top() + 8.0),
            ],
            PLAYHEAD_COLOR,
            Stroke::NONE,
        ));

        events
    }

    fn x_to_secs(&self, x: f32, left: f32, width: f64, duration: f64) -> f64 {
        let pps = self.pps.unwrap_or_else(|| fit_pps(width, duration));
        x_to_secs(x, self.start, pps, left)
    }
}

/// 素材の最大 fps（ズーム上限の計算用。素材が無ければ 30）
fn max_fps(tl: &Timeline) -> f64 {
    tl.sources
        .iter()
        .map(|s| s.fps.as_f64())
        .fold(None, |acc: Option<f64>, f| {
            Some(acc.map_or(f, |a| a.max(f)))
        })
        .unwrap_or(30.0)
}

/// 全体表示の 1秒あたり px
pub fn fit_pps(width: f64, duration: f64) -> f64 {
    if duration > 0.0 && width > 0.0 {
        width / duration
    } else {
        1.0
    }
}

pub fn secs_to_x(t: f64, start: f64, pps: f64, left: f32) -> f32 {
    left + ((t - start) * pps) as f32
}

pub fn x_to_secs(x: f32, start: f64, pps: f64, left: f32) -> f64 {
    start + f64::from(x - left) / pps
}

/// 表示左端を [0, 長さ - 表示幅] に収める
pub fn clamp_start(start: f64, pps: f64, width: f64, duration: f64) -> f64 {
    let max = (duration - width / pps).max(0.0);
    start.clamp(0.0, max)
}

/// anchor（秒）の画面位置を保って拡大率を factor 倍にする。戻り値は (左端, 1秒あたり px)
pub fn zoom_about(
    start: f64,
    pps: f64,
    anchor: f64,
    factor: f64,
    min_pps: f64,
    max_pps: f64,
) -> (f64, f64) {
    let new_pps = (pps * factor).clamp(min_pps, max_pps);
    let new_start = anchor - (anchor - start) * pps / new_pps;
    (new_start, new_pps)
}

/// t が表示範囲外なら、t が左から 1 割の位置に来るよう左端を送る
pub fn follow(start: f64, pps: f64, width: f64, t: f64) -> f64 {
    let visible = width / pps;
    if t < start || t > start + visible {
        t - visible * 0.1
    } else {
        start
    }
}

/// ラベルが重ならない目盛り間隔（秒）
pub fn tick_step(pps: f64) -> f64 {
    const STEPS: &[f64] = &[
        0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0,
        3600.0,
    ];
    STEPS
        .iter()
        .copied()
        .find(|s| s * pps >= MIN_TICK_SPACING)
        .unwrap_or(7200.0)
}

/// 目盛りラベル。間隔が1秒未満なら 0.1 秒単位まで出す
pub fn tick_label(t: f64, step: f64) -> String {
    if step < 1.0 {
        let tenths = (t * 10.0).round() as i64;
        let (m, rest) = (tenths / 600, tenths % 600);
        format!("{m:02}:{:02}.{}", rest / 10, rest % 10)
    } else {
        let s = t.round() as i64;
        let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
        if h > 0 {
            format!("{h}:{m:02}:{s:02}")
        } else {
            format!("{m:02}:{s:02}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x_and_secs_roundtrip() {
        let (start, pps, left) = (10.0, 50.0, 100.0);
        assert_eq!(secs_to_x(10.0, start, pps, left), 100.0);
        assert_eq!(secs_to_x(12.0, start, pps, left), 200.0);
        assert_eq!(x_to_secs(200.0, start, pps, left), 12.0);
        assert_eq!(x_to_secs(50.0, start, pps, left), 9.0);
    }

    #[test]
    fn fit_and_clamp() {
        assert_eq!(fit_pps(1000.0, 100.0), 10.0);
        assert_eq!(fit_pps(1000.0, 0.0), 1.0);
        // 表示幅 10 秒・長さ 100 秒 → 左端は [0, 90]
        assert_eq!(clamp_start(-5.0, 100.0, 1000.0, 100.0), 0.0);
        assert_eq!(clamp_start(95.0, 100.0, 1000.0, 100.0), 90.0);
        assert_eq!(clamp_start(40.0, 100.0, 1000.0, 100.0), 40.0);
        // 全体が収まる場合は 0
        assert_eq!(clamp_start(5.0, 10.0, 1000.0, 50.0), 0.0);
    }

    #[test]
    fn zoom_keeps_anchor_position() {
        let (start, pps) = (0.0, 10.0);
        let anchor = 30.0;
        let before = secs_to_x(anchor, start, pps, 0.0);
        let (s2, p2) = zoom_about(start, pps, anchor, 2.0, 1.0, 1000.0);
        assert_eq!(p2, 20.0);
        assert!((secs_to_x(anchor, s2, p2, 0.0) - before).abs() < 1e-3);
        // 上限・下限でクランプ
        assert_eq!(zoom_about(start, pps, anchor, 1000.0, 1.0, 50.0).1, 50.0);
        assert_eq!(zoom_about(start, pps, anchor, 0.001, 5.0, 50.0).1, 5.0);
    }

    #[test]
    fn follow_pages_when_out_of_view() {
        // 表示範囲 [10, 20)
        assert_eq!(follow(10.0, 100.0, 1000.0, 15.0), 10.0);
        assert_eq!(follow(10.0, 100.0, 1000.0, 25.0), 24.0);
        assert_eq!(follow(10.0, 100.0, 1000.0, 5.0), 4.0);
    }

    #[test]
    fn tick_step_and_label() {
        assert_eq!(tick_step(1000.0), 0.1);
        assert_eq!(tick_step(100.0), 1.0);
        assert_eq!(tick_step(10.0), 10.0);
        assert_eq!(tick_step(0.0001), 7200.0);
        assert_eq!(tick_label(83.0, 1.0), "01:23");
        assert_eq!(tick_label(3723.0, 60.0), "1:02:03");
        assert_eq!(tick_label(83.4, 0.2), "01:23.4");
        assert_eq!(tick_label(0.0, 0.1), "00:00.0");
    }
}
