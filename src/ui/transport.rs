//! トランスポート操作のアイコンボタン（Premiere Pro のプログラムモニター風）
//!
//! フォントの字形に依存しないよう、アイコンは図形で描く。

use eframe::egui::{self, Color32, Pos2, Rect, Shape, Stroke, pos2, vec2};

/// ボタン1つの大きさ
pub const BUTTON_SIZE: egui::Vec2 = vec2(32.0, 26.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    /// `{` インをマーク
    MarkIn,
    /// `}` アウトをマーク
    MarkOut,
    /// `{←` インへ移動
    GoToIn,
    /// `◀|` 1フレーム戻る
    StepBack,
    /// `▶` 再生
    Play,
    /// `■` 停止
    Stop,
    /// `|▶` 1フレーム進む
    StepForward,
    /// `→}` アウトへ移動
    GoToOut,
}

/// rect にアイコンボタンを置く。ホバー時のみ背景を塗る（Premiere と同じく枠なし）
pub fn icon_button(ui: &mut egui::Ui, rect: Rect, icon: Icon, tooltip: &str) -> egui::Response {
    let resp = ui
        .interact(
            rect,
            ui.scope_id().with(("transport", icon)),
            egui::Sense::click(),
        )
        .on_hover_text(tooltip);
    let visuals = ui.visuals();
    let color = if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 3.0, visuals.widgets.hovered.weak_bg_fill);
        visuals.widgets.hovered.fg_stroke.color
    } else {
        visuals.widgets.inactive.fg_stroke.color
    };
    ui.painter().extend(icon_shapes(icon, rect, color));
    resp
}

/// アイコンの図形。座標は中心からの相対値（縦 ±7 程度）で定義し、rect の高さに合わせて拡縮する
pub fn icon_shapes(icon: Icon, rect: Rect, color: Color32) -> Vec<Shape> {
    let c = rect.center();
    let s = rect.height() / BUTTON_SIZE.y;
    let p = |x: f32, y: f32| pos2(c.x + x * s, c.y + y * s);
    let stroke = Stroke::new(1.5 * s, color);
    let fill = |pts: Vec<Pos2>| Shape::convex_polygon(pts, color, Stroke::NONE);
    let bar = |x0: f32, x1: f32| {
        Shape::rect_filled(Rect::from_min_max(p(x0, -6.0), p(x1, 6.0)), 0.0, color)
    };
    // 中括弧 `{`（dir = 1.0）/ `}`（dir = -1.0）。x は括弧の中心
    let brace = |x: f32, dir: f32| {
        let pts = [
            (2.0, -7.0),
            (0.5, -7.0),
            (-0.5, -6.0),
            (-0.5, -1.0),
            (-2.0, 0.0),
            (-0.5, 1.0),
            (-0.5, 6.0),
            (0.5, 7.0),
            (2.0, 7.0),
        ];
        Shape::line(
            pts.iter().map(|&(px, py)| p(x + px * dir, py)).collect(),
            stroke,
        )
    };
    // 横向きの矢印（先端 x_tip、根元 x_tail）
    let arrow = |x_tip: f32, x_tail: f32| {
        let back = if x_tail > x_tip { 3.0 } else { -3.0 };
        vec![
            Shape::line_segment([p(x_tail, 0.0), p(x_tip, 0.0)], stroke),
            Shape::line(
                vec![p(x_tip + back, -3.0), p(x_tip, 0.0), p(x_tip + back, 3.0)],
                stroke,
            ),
        ]
    };

    match icon {
        Icon::MarkIn => vec![brace(1.0, 1.0)],
        Icon::MarkOut => vec![brace(-1.0, -1.0)],
        Icon::GoToIn => {
            let mut v = vec![brace(-5.0, 1.0)];
            v.extend(arrow(-2.0, 7.0));
            v
        }
        Icon::GoToOut => {
            let mut v = vec![brace(5.0, -1.0)];
            v.extend(arrow(2.0, -7.0));
            v
        }
        Icon::Play => vec![fill(vec![p(-5.0, -7.0), p(7.0, 0.0), p(-5.0, 7.0)])],
        Icon::Stop => vec![Shape::rect_filled(
            Rect::from_min_max(p(-5.5, -5.5), p(5.5, 5.5)),
            0.0,
            color,
        )],
        Icon::StepBack => vec![
            fill(vec![p(3.0, -6.0), p(3.0, 6.0), p(-6.0, 0.0)]),
            bar(5.0, 7.0),
        ],
        Icon::StepForward => vec![
            bar(-7.0, -5.0),
            fill(vec![p(-3.0, -6.0), p(6.0, 0.0), p(-3.0, 6.0)]),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Icon; 8] = [
        Icon::MarkIn,
        Icon::MarkOut,
        Icon::GoToIn,
        Icon::StepBack,
        Icon::Play,
        Icon::Stop,
        Icon::StepForward,
        Icon::GoToOut,
    ];

    #[test]
    fn icons_fit_in_button() {
        for scale in [1.0, 2.0] {
            let rect = Rect::from_min_size(pos2(100.0, 50.0), BUTTON_SIZE * scale);
            for icon in ALL {
                let shapes = icon_shapes(icon, rect, Color32::WHITE);
                assert!(!shapes.is_empty(), "{icon:?}");
                for shape in &shapes {
                    let b = shape.visual_bounding_rect();
                    assert!(rect.contains_rect(b), "{icon:?} {b:?} not in {rect:?}");
                }
            }
        }
    }

    #[test]
    fn mirrored_icons_are_symmetric() {
        let rect = Rect::from_center_size(Pos2::ZERO, BUTTON_SIZE);
        let bounds = |icon| {
            icon_shapes(icon, rect, Color32::WHITE)
                .iter()
                .fold(Rect::NOTHING, |acc, s| acc.union(s.visual_bounding_rect()))
        };
        for (a, b) in [
            (Icon::MarkIn, Icon::MarkOut),
            (Icon::GoToIn, Icon::GoToOut),
            (Icon::StepBack, Icon::StepForward),
        ] {
            let (ra, rb) = (bounds(a), bounds(b));
            assert!((ra.min.x + rb.max.x).abs() < 0.01, "{a:?}/{b:?}");
            assert!((ra.max.x + rb.min.x).abs() < 0.01, "{a:?}/{b:?}");
        }
    }
}
