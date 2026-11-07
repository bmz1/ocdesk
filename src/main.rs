mod api;
mod controller;
mod settings;
mod sidecar;
mod store;
mod ui;

use std::sync::OnceLock;
use std::time::Instant;

use anyhow::Result;
use gpui_kit::*;

use crate::sidecar::SidecarGuard;
use crate::ui::app_view::AppView;

/// Process start, for the cold-start budget (logged on first frame).
pub static STARTED: OnceLock<Instant> = OnceLock::new();

fn init_logging() -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let dir = dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("ocdesk")
        .join("logs");
    std::fs::create_dir_all(&dir)?;
    let (file, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(&dir, "ocdesk.log"));
    let filter = EnvFilter::try_from_env("OCDESK_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,ocdesk=debug,sse=debug,sidecar=info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(file).with_ansi(false))
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();
    tracing::info!(log_dir = %dir.display(), "ocdesk starting");
    Ok(guard)
}

fn main() -> Result<()> {
    STARTED.get_or_init(Instant::now);
    let _log_guard = init_logging()?;
    sidecar::install_panic_hook();
    let runtime = api::Runtime::new()?;
    let rt = runtime.handle();

    // The sidecar lives in its own process group, so a terminal Ctrl-C never
    // reaches it. Forward SIGINT/SIGTERM by killing it ourselves.
    #[cfg(unix)]
    rt.spawn(async {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut int), Ok(mut term)) = (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) else {
            return;
        };
        tokio::select! { _ = int.recv() => {}, _ = term.recv() => {} }
        tracing::info!("signal received, shutting down");
        sidecar::kill_sidecar();
        std::process::exit(130);
    });

    let _sidecar_guard = SidecarGuard;
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            cx.on_app_quit(|_| {
                sidecar::kill_sidecar();
                async {}
            })
            .detach();
            let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
            let opts = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("ocdesk".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            if let Err(e) = gpui_kit::open_window(opts, cx, |window, cx| {
                cx.new(|cx| AppView::new(rt, window, cx))
            }) {
                tracing::error!("failed to open window: {e:#}");
                cx.quit();
            }
        });
    tracing::info!("event loop ended");
    drop(runtime);
    Ok(())
}
