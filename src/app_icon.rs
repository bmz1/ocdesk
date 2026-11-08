//! Native application icons, including launches outside an installed bundle.
use std::sync::{Arc, OnceLock};

pub const PNG: &[u8] = include_bytes!("../packaging/icon.png");

/// GPUI forwards this to X11's _NET_WM_ICON. Wayland uses the desktop entry.
pub fn window_icon() -> Arc<image::RgbaImage> {
    static ICON: OnceLock<Arc<image::RgbaImage>> = OnceLock::new();
    ICON.get_or_init(|| {
        // Keep the property small enough for X11's normal request-size limit.
        let image = image::load_from_memory_with_format(PNG, image::ImageFormat::Png)
            .expect("embedded application icon must be a valid PNG")
            .resize_exact(128, 128, image::imageops::FilterType::Lanczos3)
            .into_rgba8();
        Arc::new(image)
    })
    .clone()
}

#[cfg(target_os = "macos")]
pub fn set_dock_icon() -> anyhow::Result<()> {
    use anyhow::Context as _;
    use objc2::{AnyThread as _, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{NSBundle, NSData, ns_string};

    let main_thread =
        MainThreadMarker::new().context("Dock icon must be set on the main thread")?;
    // Installed .app bundles supply their own icon to both Finder and the Dock.
    let bundle = NSBundle::mainBundle();
    if bundle
        .objectForInfoDictionaryKey(ns_string!("CFBundleIconFile"))
        .is_some()
        || bundle
            .objectForInfoDictionaryKey(ns_string!("CFBundleIconName"))
            .is_some()
    {
        return Ok(());
    }
    let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(PNG))
        .context("Could not decode the embedded Dock icon")?;
    // SAFETY: this runs on AppKit's main thread and passes a live, non-nil NSImage.
    unsafe {
        NSApplication::sharedApplication(main_thread).setApplicationIconImage(Some(&image));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn embedded_icon_is_visible_and_fits_an_x11_property() {
        let icon = window_icon();
        assert_eq!(icon.dimensions(), (128, 128));
        assert!(
            icon.pixels().any(|p| p.0[3] != 0),
            "icon cannot be completely transparent"
        );
        assert!((2 + icon.width() * icon.height()) * 4 < 256 * 1024);
        assert!(Arc::ptr_eq(&icon, &window_icon()), "decode only once");
    }
}
