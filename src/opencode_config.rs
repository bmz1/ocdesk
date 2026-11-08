//! OpenCode's real JSON/JSONC files. Run filesystem operations off the UI thread.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

pub const TEMPLATE: &str =
    "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  // Configure OpenCode here.\n}\n";

#[derive(Clone, Debug)]
pub struct ConfigFile {
    pub path: PathBuf,
    pub inherited: bool,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub file: ConfigFile,
    pub original: Option<String>,
    resolved: Option<PathBuf>,
}

pub fn global_directory() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .map(|p| p.join("opencode"))
}

/// List every existing file, rather than silently picking one when both exist.
pub fn discover(global: &Path, project: Option<&Path>) -> Result<Vec<ConfigFile>> {
    let mut files = Vec::new();
    let directories: Vec<_> = match project {
        None => vec![(global.to_path_buf(), false)],
        Some(project) => {
            let project = fs::canonicalize(project)?;
            let mut direct: Vec<_> = project.ancestors().collect();
            direct.reverse();
            direct
                .iter()
                .map(|p| (p.to_path_buf(), *p != project))
                .chain(direct.iter().map(|p| (p.join(".opencode"), *p != project)))
                .collect()
        }
    };
    for (dir, inherited) in directories {
        for name in ["opencode.json", "opencode.jsonc"] {
            let path = dir.join(name);
            match fs::metadata(&path) {
                Ok(meta) if meta.is_file() => files.push(ConfigFile { path, inherited }),
                Ok(_) => bail!("{} is not a file", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("Could not inspect {}", path.display()));
                }
            }
        }
    }
    if !files.iter().any(|f| !f.inherited) {
        files.push(ConfigFile {
            path: project.unwrap_or(global).join("opencode.jsonc"),
            inherited: false,
        });
    }
    Ok(files)
}

pub fn read(file: ConfigFile) -> Result<Snapshot> {
    let original = match fs::read_to_string(&file.path) {
        Ok(text) => {
            if text.len() as u64 > crate::files::MAX_EDIT_BYTES {
                bail!("Configuration is too large to edit here");
            }
            Some(text)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("Could not read {}", file.path.display())),
    };
    let resolved = original
        .as_ref()
        .map(|_| fs::canonicalize(&file.path))
        .transpose()?;
    Ok(Snapshot {
        file,
        original,
        resolved,
    })
}

/// Accept JSONC, not JSON5; keep the exact source text for saving.
pub fn validate(text: &str) -> Result<()> {
    let options = jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        allow_bare_decimal_point_numbers: false,
        allow_non_finite_numbers: false,
        allow_extended_string_escapes: false,
    };
    let value: serde_json::Value = jsonc_parser::parse_to_serde_value(text, &options)?;
    if !value.is_object() {
        bail!("OpenCode configuration must be a JSON object");
    }
    Ok(())
}

fn unchanged(snapshot: &Snapshot) -> Result<()> {
    let current = match fs::read_to_string(&snapshot.file.path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if current != snapshot.original
        || (current.is_some() && Some(fs::canonicalize(&snapshot.file.path)?) != snapshot.resolved)
    {
        bail!(
            "The configuration changed on disk. Your edits were kept; discard and reload to review the file before saving."
        );
    }
    Ok(())
}

pub fn save(snapshot: &Snapshot, text: &str) -> Result<Snapshot> {
    if snapshot.file.inherited {
        bail!(
            "Inherited configuration is read-only here. Edit this project's config to override it."
        );
    }
    validate(text)?;
    unchanged(snapshot)?;
    let target = snapshot.resolved.as_ref().unwrap_or(&snapshot.file.path);
    let parent = target
        .parent()
        .context("Configuration has no parent directory")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".ocdesk-config-{}-{}.tmp",
        std::process::id(),
        rand::random::<u64>()
    ));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(&temporary)?;
        if snapshot.original.is_some() {
            output.set_permissions(fs::metadata(target)?.permissions())?;
        }
        output.write_all(text.as_bytes())?;
        output.sync_all()?;
        unchanged(snapshot)?;
        if snapshot.original.is_some() {
            fs::rename(&temporary, target)?;
        } else {
            fs::hard_link(&temporary, target)
                .context("Configuration was created by another writer; reload before saving")?;
        }
        Ok::<_, anyhow::Error>(())
    })();
    let _ = fs::remove_file(&temporary);
    result?;
    // Retain the bytes we wrote, so later external writes remain a conflict.
    Ok(Snapshot {
        file: snapshot.file.clone(),
        original: Some(text.to_string()),
        resolved: Some(fs::canonicalize(&snapshot.file.path)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> PathBuf {
        let p = std::env::temp_dir().join(format!("ocdesk-config-test-{}", rand::random::<u64>()));
        fs::create_dir_all(&p).unwrap();
        p
    }
    #[test]
    fn jsonc_validation_accepts_comments_but_rejects_json5_and_non_objects() {
        validate(TEMPLATE).unwrap();
        validate("{/* keep */ \"url\": \"https://example.com/a//b\",}").unwrap();
        for bad in [
            "",
            "null",
            "[]",
            "{unquoted: 1}",
            "{\"a\":1 \"b\":2}",
            "{'a':1}",
            "{\"a\":}",
        ] {
            assert!(validate(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn discovery_keeps_both_formats_and_marks_inherited_files() {
        let dir = fixture();
        let project = dir.join("project");
        fs::create_dir(&project).unwrap();
        fs::write(dir.join("opencode.jsonc"), TEMPLATE).unwrap();
        fs::write(project.join("opencode.json"), "{}").unwrap();
        fs::create_dir(project.join(".opencode")).unwrap();
        fs::write(project.join(".opencode/opencode.jsonc"), TEMPLATE).unwrap();
        let found = discover(&dir.join("global"), Some(&project)).unwrap();
        assert!(
            found
                .iter()
                .any(|f| f.path == dir.join("opencode.jsonc") && f.inherited)
        );
        assert_eq!(found.iter().filter(|f| !f.inherited).count(), 2);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn saving_preserves_source_and_refuses_external_changes_and_creation_races() {
        let dir = fixture();
        let path = dir.join("config/opencode.jsonc");
        let snap = read(ConfigFile {
            path: path.clone(),
            inherited: false,
        })
        .unwrap();
        let text = "{\r\n // retain comment\r\n \"model\": \"p/m\",\r\n}\r\n";
        let saved = save(&snap, text).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        assert!(
            save(&snap, "{}").is_err(),
            "creation must not replace an existing file"
        );
        fs::write(&path, "{\"external\":true}").unwrap();
        assert!(save(&saved, "{}").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"external\":true}");
        assert!(save(&saved, "{").is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
