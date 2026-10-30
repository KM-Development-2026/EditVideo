//! プレビュー映像の表示（egui テクスチャ）

use eframe::egui;

use crate::model::Timestamp;

/// 受け取ったフレームを保持して表示する
#[derive(Default)]
pub struct Preview {
    texture: Option<egui::TextureHandle>,
    /// 表示中フレームのタイムライン時刻
    time: Option<Timestamp>,
}

impl Preview {
    /// フレームを設定する（テクスチャは使い回して更新する）
    pub fn set_frame(
        &mut self,
        ctx: &egui::Context,
        time: Timestamp,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) {
        let image =
            egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], rgba);
        let options = egui::TextureOptions::LINEAR;
        match &mut self.texture {
            Some(tex) => tex.set(image, options),
            None => self.texture = Some(ctx.load_texture("preview", image, options)),
        }
        self.time = Some(time);
    }

    pub fn clear(&mut self) {
        self.texture = None;
        self.time = None;
    }

    pub fn time(&self) -> Option<Timestamp> {
        self.time
    }

    /// 利用可能領域いっぱいに、アスペクト比を保って中央に表示する
    pub fn show(&self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(ui.available_size(), egui::Sense::hover());
        ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);
        let Some(tex) = &self.texture else { return };
        let size = fit_size(rect.size(), tex.size_vec2());
        let image_rect = egui::Rect::from_center_size(rect.center(), size);
        egui::Image::from_texture((tex.id(), size)).paint_at(ui, image_rect);
    }
}

/// image を area に収まる最大サイズへ拡縮する（アスペクト比維持）
pub fn fit_size(area: egui::Vec2, image: egui::Vec2) -> egui::Vec2 {
    if image.x <= 0.0 || image.y <= 0.0 || area.x <= 0.0 || area.y <= 0.0 {
        return egui::Vec2::ZERO;
    }
    let scale = (area.x / image.x).min(area.y / image.y);
    image * scale
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::vec2;

    #[test]
    fn fit_size_letterbox_and_pillarbox() {
        // 横長の領域に 16:9 → 高さ基準
        assert_eq!(
            fit_size(vec2(1000.0, 450.0), vec2(960.0, 540.0)),
            vec2(800.0, 450.0)
        );
        // 縦長の領域 → 幅基準
        assert_eq!(
            fit_size(vec2(480.0, 800.0), vec2(960.0, 540.0)),
            vec2(480.0, 270.0)
        );
        // 小さい画像は拡大する
        assert_eq!(
            fit_size(vec2(640.0, 480.0), vec2(320.0, 240.0)),
            vec2(640.0, 480.0)
        );
        assert_eq!(
            fit_size(vec2(0.0, 100.0), vec2(320.0, 240.0)),
            egui::Vec2::ZERO
        );
        assert_eq!(
            fit_size(vec2(100.0, 100.0), vec2(0.0, 0.0)),
            egui::Vec2::ZERO
        );
    }
}
