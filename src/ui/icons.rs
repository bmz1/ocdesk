//! Icons beyond gpui-component's default set. Only the listed Lucide SVGs
//! are embedded; every other path falls back to the default bundle.

use std::borrow::Cow;

use gpui_kit::assets::{Assets, IconName as Lucide};
use gpui_kit::component::Icon;
use gpui_kit::{AssetSource, Result, SharedString};

gpui_kit::assets::icon_assets!(
    Extra,
    [
        GitBranch,
        Pencil,
        Terminal,
        Slash,
        Plug,
        Sparkles,
        Layers,
        Workflow,
        Brain,
        CircleStop,
        ListChecks,
        GitFork,
        Pin,
        PinOff,
        Archive,
        MessageSquare,
        Shield,
        FolderOpen,
        Files,
        ChevronsDownUp,
        Bot,
        FileDiff,
        Globe,
        MousePointerClick,
        GitPullRequest,
        CircleCheck,
        CircleX,
        CircleDashed
    ]
);

/// The app's asset source: [`Extra`] first, then the component defaults.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match Extra.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut all = Assets.list(path)?;
        all.extend(Extra.list(path)?);
        all.sort();
        all.dedup();
        Ok(all)
    }
}

pub fn git_branch() -> Icon {
    Icon::new(Lucide::GitBranch)
}
pub fn pencil() -> Icon {
    Icon::new(Lucide::Pencil)
}
pub fn terminal() -> Icon {
    Icon::new(Lucide::Terminal)
}
pub fn slash() -> Icon {
    Icon::new(Lucide::Slash)
}
pub fn plug() -> Icon {
    Icon::new(Lucide::Plug)
}
pub fn sparkles() -> Icon {
    Icon::new(Lucide::Sparkles)
}
pub fn layers() -> Icon {
    Icon::new(Lucide::Layers)
}
pub fn workflow() -> Icon {
    Icon::new(Lucide::Workflow)
}
pub fn brain() -> Icon {
    Icon::new(Lucide::Brain)
}
pub fn circle_stop() -> Icon {
    Icon::new(Lucide::CircleStop)
}
pub fn list_checks() -> Icon {
    Icon::new(Lucide::ListChecks)
}
pub fn git_fork() -> Icon {
    Icon::new(Lucide::GitFork)
}
pub fn pin() -> Icon {
    Icon::new(Lucide::Pin)
}
pub fn pin_off() -> Icon {
    Icon::new(Lucide::PinOff)
}
pub fn archive() -> Icon {
    Icon::new(Lucide::Archive)
}
pub fn message_square() -> Icon {
    Icon::new(Lucide::MessageSquare)
}
pub fn shield() -> Icon {
    Icon::new(Lucide::Shield)
}
pub fn folder_open() -> Icon {
    Icon::new(Lucide::FolderOpen)
}
pub fn files() -> Icon {
    Icon::new(Lucide::Files)
}
pub fn collapse_all() -> Icon {
    Icon::new(Lucide::ChevronsDownUp)
}
pub fn bot() -> Icon {
    Icon::new(Lucide::Bot)
}
pub fn file_diff() -> Icon {
    Icon::new(Lucide::FileDiff)
}

pub fn git_pull_request() -> Icon {
    Icon::new(Lucide::GitPullRequest)
}

pub fn circle_check() -> Icon {
    Icon::new(Lucide::CircleCheck)
}

pub fn circle_x() -> Icon {
    Icon::new(Lucide::CircleX)
}

pub fn circle_dashed() -> Icon {
    Icon::new(Lucide::CircleDashed)
}
pub fn globe() -> Icon {
    Icon::new(Lucide::Globe)
}
pub fn mouse_pointer() -> Icon {
    Icon::new(Lucide::MousePointerClick)
}
