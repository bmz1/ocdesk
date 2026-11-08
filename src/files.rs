//! Pure helpers for the editor tabs: path confinement, content
//! classification, line endings and language detection. No GPUI, no I/O.

use anyhow::{Result, bail};

/// Files above this size open read-only, showing their first part only.
pub const MAX_EDIT_BYTES: u64 = 2 * 1024 * 1024;
/// How much of a file is inspected for NUL bytes to call it binary.
const BINARY_SNIFF: usize = 8 * 1024;

/// Normalize a path relative to a folder, refusing anything that could
/// escape it. `experimental/fs/write` is not confined server-side, so every
/// write path goes through here.
pub fn rel_path(path: &str) -> Result<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        bail!("empty path");
    }
    if trimmed.starts_with('/') || trimmed.starts_with('\\') || trimmed.contains('\0') {
        bail!("path must be relative to the folder: {trimmed}");
    }
    // `C:\…` / `C:/…` on Windows.
    if trimmed.as_bytes().get(1) == Some(&b':') {
        bail!("path must be relative to the folder: {trimmed}");
    }
    let mut parts = Vec::new();
    for part in trimmed.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => bail!("path leaves the folder: {trimmed}"),
            p => parts.push(p),
        }
    }
    if parts.is_empty() {
        bail!("empty path");
    }
    Ok(parts.join("/"))
}

/// A path as shown by a tool call (often absolute) relative to `dir`, when
/// it is inside it.
pub fn relative_to(dir: &std::path::Path, path: &str) -> Option<String> {
    let p = std::path::Path::new(path);
    let rel = if p.is_absolute() {
        p.strip_prefix(dir).ok()?.to_str()?.to_string()
    } else {
        path.to_string()
    };
    rel_path(&rel).ok()
}

/// What a read produced, ready for the editor.
#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    /// Editable (or read-only when `truncated`) text, with `\n` line endings.
    Text {
        text: String,
        /// The file used CRLF; saving restores it.
        crlf: bool,
        /// Only the first [`MAX_EDIT_BYTES`] were read.
        truncated: bool,
    },
    Binary,
}

/// Classify the first bytes of a file whose full size is `total`.
pub fn decode(bytes: &[u8], total: u64) -> Content {
    if bytes[..bytes.len().min(BINARY_SNIFF)].contains(&0) {
        return Content::Binary;
    }
    let truncated = total > bytes.len() as u64;
    let text = match std::str::from_utf8(bytes) {
        Ok(t) => t.to_string(),
        // A truncated read can end inside a character: drop the partial tail.
        Err(e) if truncated && e.error_len().is_none() => {
            String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned()
        }
        Err(_) => return Content::Binary,
    };
    let crlf = text.contains("\r\n");
    let text = if crlf {
        text.replace("\r\n", "\n")
    } else {
        text
    };
    Content::Text {
        text,
        crlf,
        truncated,
    }
}

/// The bytes to write for editor `text`, restoring CRLF line endings.
pub fn encode(text: &str, crlf: bool) -> Vec<u8> {
    if crlf {
        text.replace("\r\n", "\n")
            .replace('\n', "\r\n")
            .into_bytes()
    } else {
        text.as_bytes().to_vec()
    }
}

/// The highlighter language for a path: its extension, or the file name
/// for extensionless files (`Makefile`, `Dockerfile`). Syntect resolves both.
pub fn language_for(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => match ext.to_ascii_lowercase().as_str() {
            "tsx" | "jsx" => "tsx".into(),
            "mjs" | "cjs" => "javascript".into(),
            "mts" | "cts" => "typescript".into(),
            "h" => "c".into(),
            "hpp" | "hh" | "cc" | "cxx" => "cpp".into(),
            "zsh" => "bash".into(),
            e => e.into(),
        },
        _ => match name.to_ascii_lowercase().as_str() {
            "makefile" | "gnumakefile" => "make".into(),
            "dockerfile" => "dockerfile".into(),
            ".bashrc" | ".zshrc" | ".profile" => "bash".into(),
            _ => "text".into(),
        },
    }
}

/// The last path segment, for tab labels.
pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn rel_path_confines() {
        assert_eq!(rel_path("src/main.rs").unwrap(), "src/main.rs");
        assert_eq!(rel_path("./src//a.rs").unwrap(), "src/a.rs");
        assert_eq!(rel_path("src\\a.rs").unwrap(), "src/a.rs");
        for bad in [
            "",
            "  ",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a/..",
            "C:/x",
            "a\0b",
            ".",
        ] {
            assert!(rel_path(bad).is_err(), "{bad:?}");
        }
    }

    #[::core::prelude::v1::test]
    fn relative_to_folder() {
        let dir = std::path::Path::new("/repo");
        assert_eq!(
            relative_to(dir, "/repo/src/a.rs").as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(relative_to(dir, "src/a.rs").as_deref(), Some("src/a.rs"));
        assert_eq!(relative_to(dir, "/other/a.rs"), None);
        assert_eq!(relative_to(dir, "/repo"), None);
    }

    #[::core::prelude::v1::test]
    fn decode_classifies() {
        assert_eq!(
            decode(b"a\r\nb\r\n", 6),
            Content::Text {
                text: "a\nb\n".into(),
                crlf: true,
                truncated: false
            }
        );
        assert_eq!(decode(b"PK\x03\x04\0\0", 6), Content::Binary);
        assert_eq!(decode(&[0xff, 0xfe, b'a'], 3), Content::Binary);
        // "é" is two bytes; a truncated read that splits it drops the half.
        let bytes = "aé".as_bytes();
        assert_eq!(
            decode(&bytes[..2], 100),
            Content::Text {
                text: "a".into(),
                crlf: false,
                truncated: true
            }
        );
        assert_eq!(
            decode(b"", 0),
            Content::Text {
                text: String::new(),
                crlf: false,
                truncated: false
            }
        );
    }

    #[::core::prelude::v1::test]
    fn crlf_round_trips() {
        let Content::Text { text, crlf, .. } = decode(b"x\r\ny\r\n", 6) else {
            panic!()
        };
        assert_eq!(encode(&text, crlf), b"x\r\ny\r\n");
        assert_eq!(encode("x\ny", false), b"x\ny");
    }

    #[::core::prelude::v1::test]
    fn languages() {
        assert_eq!(language_for("src/main.rs"), "rs");
        assert_eq!(language_for("a/App.TSX"), "tsx");
        assert_eq!(language_for("Makefile"), "make");
        assert_eq!(language_for(".gitignore"), "text");
        assert_eq!(language_for("LICENSE"), "text");
        assert_eq!(file_name("a/b/c.rs"), "c.rs");
    }
}
