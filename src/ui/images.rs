//! Image attachments: thumbnails in the transcript and the composer, and a
//! viewer dialog that shows the image at full size.
//!
//! Images load from disk (the attachment's `file://` path); ocdesk never keeps
//! their bytes in the store.

use std::path::{Path, PathBuf};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex};
use gpui_kit::*;

const IMAGE_EXTENSIONS: [&str; 8] = ["png", "jpg", "jpeg", "webp", "gif", "svg", "bmp", "tiff"];

pub fn is_image(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.as_str()))
}

/// A square-ish thumbnail; click opens the viewer. Missing files show a
/// placeholder instead of failing.
pub fn thumbnail(id: impl Into<ElementId>, path: &Path, height: f32, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let open = path.to_path_buf();
    let name: SharedString = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
        .into();
    div()
        .id(id)
        .h(px(height))
        .max_w(px(height * 2.))
        .flex_none()
        .overflow_hidden()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .cursor_pointer()
        .tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(name.clone()).build(window, cx)
        })
        .on_click(move |_, window, cx| open_viewer(open.clone(), window, cx))
        .child(
            img(path.to_path_buf())
                .h(px(height))
                .object_fit(ObjectFit::Cover)
                .with_fallback(move || {
                    h_flex()
                        .size_full()
                        .px_2()
                        .justify_center()
                        .text_color(muted)
                        .child(Icon::new(IconName::GalleryVerticalEnd).small())
                        .into_any_element()
                }),
        )
}

/// The image at full size in a dialog, scaled to fit the window.
pub fn open_viewer(path: PathBuf, window: &mut Window, cx: &mut App) {
    let viewport = window.viewport_size();
    let (w, h) = (viewport.width * 0.85, viewport.height * 0.8);
    let title = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    window.open_dialog(cx, move |dialog, _, _| {
        let reveal = path.clone();
        dialog
            .title(title.clone())
            .w(w)
            .child(
                div()
                    .w_full()
                    .h(h - px(80.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        img(path.clone())
                            .max_w_full()
                            .max_h_full()
                            .object_fit(ObjectFit::Contain),
                    ),
            )
            .footer(
                h_flex().justify_end().child(
                    Button::new("image-copy-path")
                        .label("Copy path")
                        .small()
                        .ghost()
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                reveal.display().to_string(),
                            ))
                        }),
                ),
            )
    });
}

#[cfg(test)]
mod tests {
    #[::core::prelude::v1::test]
    fn recognises_image_extensions() {
        assert!(super::is_image(std::path::Path::new("/tmp/a.PNG")));
        assert!(!super::is_image(std::path::Path::new("/tmp/a.txt")));
    }
}
