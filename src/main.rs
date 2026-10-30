// リリースビルドではコンソール窓を出さない
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

use edit_video::app::EditVideoApp;
use eframe::egui;

/// 日本語表示用フォントの候補（先に見つかったものを使う）
const JAPANESE_FONTS: &[&str] = &[
    r"C:\Windows\Fonts\NotoSansJP-VF.ttf",
    r"C:\Windows\Fonts\YuGothM.ttc",
    r"C:\Windows\Fonts\meiryo.ttc",
    r"C:\Windows\Fonts\msgothic.ttc",
];

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("EditVideo")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "EditVideo",
        options,
        Box::new(|cc| {
            install_japanese_font(&cc.egui_ctx);
            let initial = std::env::args_os().nth(1).map(Into::into);
            Ok(Box::new(EditVideoApp::new(cc, initial)))
        }),
    )
}

/// Windows のシステムフォントを読み込んで登録する。
/// 通常の文字は欧文も含めて日本語フォントで描き（字面の高さを揃える）、
/// 等幅は既定フォントを優先して日本語フォントをフォールバックにする
fn install_japanese_font(ctx: &egui::Context) {
    let Some(bytes) = JAPANESE_FONTS.iter().find_map(|p| std::fs::read(p).ok()) else {
        return;
    };
    // 可変フォント（Noto Sans JP）は既定のウェイトが細いので Regular を指定する
    let data = egui::FontData::from_owned(bytes).tweak(egui::FontTweak {
        coords: egui::epaint::text::VariationCoords::new([(b"wght", 400.0)]),
        ..Default::default()
    });
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("japanese".to_owned(), Arc::new(data));
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "japanese".to_owned());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("japanese".to_owned());
    ctx.set_fonts(fonts);
}
