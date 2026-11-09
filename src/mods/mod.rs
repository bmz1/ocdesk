//! Experimental JavaScript mods (cargo feature `mods`), on gpui-kit's
//! `gpui-shell` runtime. A mod describes GPUI elements from JavaScript; Rust
//! draws them. See docs/mods.md.
//!
//! - [`reads`]: which store changes re-render which mod (pure, always tested).
//! - `host`: the `ocdesk` module mods import and the policy they run under.
//! - The panel that loads and hosts mods is `ui::mods`.

pub mod reads;

#[cfg(feature = "mods")]
pub mod host;

use std::path::PathBuf;

/// Where mods live: `$OCDESK_MODS_DIR`, else `<config>/ocdesk/mods`.
/// Each subdirectory with a `main.js` is one mod.
#[cfg(feature = "mods")]
pub fn mods_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("OCDESK_MODS_DIR") {
        return Some(PathBuf::from(dir));
    }
    Some(dirs::config_dir()?.join("ocdesk").join("mods"))
}

/// A mod's `localStorage` file, outside the mod's own folder so a reload or
/// reinstall keeps it.
#[cfg(feature = "mods")]
pub fn storage_path(name: &str) -> Option<PathBuf> {
    Some(
        dirs::config_dir()?
            .join("ocdesk")
            .join("mod-data")
            .join(format!("{name}.json")),
    )
}

/// The mods in `dir`, sorted by folder name.
#[cfg(any(test, feature = "mods"))]
pub fn discover(dir: &std::path::Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("main.js").is_file())
        .filter_map(|p| Some((p.file_name()?.to_str()?.to_owned(), p)))
        .collect();
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    #[test]
    fn discover_lists_folders_with_a_main_js_in_name_order() {
        let root = std::env::temp_dir().join(format!("ocdesk-mods-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (name, entry) in [("zeta", true), ("alpha", true), ("no-entry", false)] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            if entry {
                std::fs::write(dir.join("main.js"), "").unwrap();
            }
        }
        std::fs::write(root.join("main.js"), "").unwrap();

        let names: Vec<_> = super::discover(&root).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["alpha", "zeta"]);
        assert!(super::discover(&root.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
