use super::*;
use std::path::PathBuf;

const LARGE_PASTE_CHARS: usize = 16_000;
const LARGE_PASTE_LINES: usize = 200;

impl Composer {
    pub(super) fn attachment_notice(&self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.controller
            .update(cx, |c, cx| c.show_notice(message, cx));
    }
    pub(super) fn add_attachments(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let key = self.draft_key.clone();
        self.add_attachments_to(key, paths, cx);
    }
    fn add_attachments_to(
        &mut self,
        key: Option<String>,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let target = if key == self.draft_key {
            &mut self.attachments
        } else {
            self.attachment_drafts.entry(key).or_default()
        };
        let mut errors = Vec::new();
        for path in paths {
            match std::fs::canonicalize(&path) {
                Ok(path) if path.is_file() => {
                    if target.contains(&path) {
                        continue;
                    }
                    if target.len() >= 100 {
                        errors.push("At most 100 attachments per prompt".into());
                        break;
                    }
                    target.push(path);
                }
                _ => errors.push(format!(
                    "Cannot attach {} (choose a readable file)",
                    path.display()
                )),
            }
        }
        if !errors.is_empty() {
            self.attachment_notice(errors.join("; "), cx);
        }
        cx.notify();
    }
    pub(super) fn pick_attachments(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let key = self.draft_key.clone();
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach files".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(Ok(Some(paths))) => this.add_attachments_to(key, paths, cx),
                Ok(Ok(None)) => {}
                _ => this.attachment_notice(
                    "File picker unavailable. Drop files here or paste copied files.",
                    cx,
                ),
            });
        })
        .detach();
    }
    pub(super) fn paste_attachments(
        &mut self,
        item: &ClipboardItem,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut paths = Vec::new();
        let mut images = Vec::new();
        for entry in item.entries() {
            match entry {
                ClipboardEntry::ExternalPaths(p) => paths.extend(p.paths().iter().cloned()),
                ClipboardEntry::Image(i) => images.push((
                    format!(
                        "pasted-image.{}",
                        format!("{:?}", i.format()).to_lowercase()
                    ),
                    i.bytes().to_vec(),
                )),
                _ => {}
            }
        }
        // Native clipboards can expose a text fallback for an image or paths.
        // Prefer the rich payload so the same paste isn't attached twice.
        if !paths.is_empty() || !images.is_empty() {
            self.add_attachments(paths, cx);
            for (name, bytes) in images {
                self.save_paste(name, bytes, cx);
            }
            return true;
        }
        if let Some(text) = item.text()
            && (text.chars().count() > LARGE_PASTE_CHARS
                || text.lines().count() > LARGE_PASTE_LINES)
        {
            self.save_paste("pasted-text.txt".into(), text.into_bytes(), cx);
            return true;
        }
        false
    }
    fn save_paste(&mut self, name: String, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if self.attachments.len() + self.attachment_writes >= 100 {
            self.attachment_notice("At most 100 attachments per prompt", cx);
            return;
        }
        let key = self.draft_key.clone();
        self.attachment_writes += 1;
        cx.notify();
        // Files remain available for queued prompts and history replay. They
        // live in app data rather than OS temp (which may be purged mid-turn).
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { save_pasted_file(&name, &bytes) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.attachment_writes = this.attachment_writes.saturating_sub(1);
                match result {
                    Ok(path) => this.add_attachments_to(key, vec![path], cx),
                    Err(e) => this.attachment_notice(format!("Could not save paste: {e:#}"), cx),
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn render_attachments(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut row = h_flex()
            .id("attachment-chips")
            .gap_1()
            .flex_wrap()
            .max_h(px(120.))
            .overflow_y_scroll();
        for (i, path) in self.attachments.iter().enumerate() {
            let label = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            row = row.child(
                Button::new(SharedString::from(format!("attachment-{i}")))
                    .label(format!("{label} ×"))
                    .max_w(px(260.))
                    .overflow_hidden()
                    .tooltip(path.display().to_string())
                    .outline()
                    .xsmall()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if i < this.attachments.len() {
                            this.attachments.remove(i);
                        }
                        cx.notify();
                    })),
            );
        }
        if self.attachment_writes > 0 {
            row = row.child(div().text_xs().child("Saving pasted attachment…"));
        }
        row.into_any_element()
    }
}
fn save_pasted_file(name: &str, bytes: &[u8]) -> anyhow::Result<PathBuf> {
    use std::io::Write as _;
    let directory = dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("no app data directory"))?
        .join("ocdesk")
        .join("attachments");
    std::fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let directory = directory.join(format!("{:016x}", rand::random::<u64>()));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&directory)?;
    let path = directory.join(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(&path)?.write_all(bytes)?;
    Ok(path)
}
