mod app;
mod config;
mod p2p;
mod stream;

use app::CameraViewerApp;
use config::{load_env_config, locate_env_file};
use eframe::egui;
use std::sync::Arc;
use stream::scan_active_nvr_channels;

fn load_app_icon() -> Option<Arc<egui::IconData>> {
    let png_bytes = include_bytes!("../assets/icon.png");
    let img = image::load_from_memory(png_bytes).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    Some(Arc::new(egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    }))
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let env_path = locate_env_file();
    let env_cfg = load_env_config(&env_path);

    if args.iter().any(|a| a == "--version" || a == "-v") {
        println!("nvr-command-center v{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "NVR Command Center v{} (Native Rust + GPU + Cloud P2P)\n\
             Usage: rtsp-camera-viewer [OPTIONS]\n\n\
             Options:\n\
               --scan       Probe NVR channels 1..16 in parallel and exit\n\
               --version    Print version information\n\
               --help       Print this help message",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }

    if args.iter().any(|a| a == "--scan") {
        println!(
            "Scanning channels 1..16 on {} ({})...",
            env_cfg.active_host(),
            env_cfg.brand
        );
        let active = scan_active_nvr_channels(&env_cfg, 16);
        println!("Active channels ({}): {:?}", active.len(), active);
        return Ok(());
    }

    let app_id = if std::env::var_os("SNAP").is_some() {
        "nvr-command-center_nvr-command-center"
    } else {
        "rtsp-camera-viewer"
    };

    let mut viewport = egui::ViewportBuilder::default()
        .with_app_id(app_id)
        .with_title(format!(
            "NVR Command Center v{} - {} Cameras ({})",
            env!("CARGO_PKG_VERSION"),
            env_cfg.num_cameras,
            if env_cfg.use_cloud_p2p {
                "Cloud P2P"
            } else {
                env_cfg.active_host()
            }
        ))
        .with_inner_size([1600.0, 920.0])
        .with_min_inner_size([800.0, 500.0]);

    if let Some(icon) = load_app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "NVR Command Center",
        native_options,
        Box::new(move |cc| Ok(Box::new(CameraViewerApp::new(cc, env_path, env_cfg)))),
    )
}
