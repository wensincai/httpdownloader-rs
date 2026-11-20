//! hd-gui — egui GUI for hd-core (M4).
//!
//! Features: download list with progress bars, add dialog, settings window,
//! dark/light theme toggle, clipboard URL monitor. Shares queue.json +
//! settings.json + .hdmeta with the hd-cli frontend.

use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;

use hd_core::engine::{Engine, Event, RunParams};
use hd_core::model::{AuthCreds, DownloadInfo, DownloadState};
use hd_core::queue::Queue;
use hd_core::settings::Settings;
use hd_core::throttle::Throttle;
use hd_core::engine::load_resume;
use hd_core::{Url};

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 640.0])
            .with_min_inner_size([720.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "HTTP Downloader (Rust)",
        native,
        Box::new(|_cc| Ok(Box::new(HdApp::new()))),
    )
}

/// One row in the GUI task list.
struct Task {
    info: DownloadInfo,
    downloaded: u64,
    speed: u64,
    state: DownloadState,
    error: Option<String>,
    /// active engine invocation
    running: Option<Running>,
    /// pause requested while running (so Canceled maps to Paused, not Stopped)
    pause_requested: bool,
    speed_tick: Instant,
}

struct Running {
    cancel: tokio::sync::mpsc::Sender<()>,
}

#[derive(Default)]
struct AddDialog {
    open: bool,
    url: String,
    output: String,
    parts: String,
    limit: String,
    user: String,
    pass: String,
    proxy: String,
    key: String,
}

impl AddDialog {
    fn clear(&mut self) {
        *self = AddDialog { open: true, ..Default::default() };
    }
}

enum Fwd {
    Progress(u64, u64),      // task key, downloaded total
    Terminal(u64, bool),     // task key, ok
    Fail(u64, String),
}

struct HdApp {
    rt: Arc<tokio::runtime::Runtime>,
    engine: Arc<Engine>,
    global: Arc<Mutex<Throttle>>,
    settings: Settings,
    tasks: Vec<Task>,
    next_key: u64,
    events_rx: std_mpsc::Receiver<Fwd>,
    events_tx: std_mpsc::Sender<Fwd>,
    add: AddDialog,
    show_settings: bool,
    dark: bool,
    clipboard_monitor: bool,
    clipboard: Option<arboard::Clipboard>,
    last_clip: Option<String>,
    toasts: Vec<(String, Instant)>,
}

impl HdApp {
    fn new() -> Self {
        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime"),
        );
        let settings = Settings::load();
        let global = Arc::new(Mutex::new(Throttle::new(settings.global_speed_limit)));
        let engine = Arc::new(Engine::new());

        let mut tasks = Vec::new();
        let mut next_key = 1;
        // adopt the shared queue (queued/paused entries from CLI or previous run)
        let q = Queue::load();
        for mut info in q.items {
            let state = match info.state {
                DownloadState::Completed | DownloadState::Failed => info.state,
                _ => DownloadState::Queued,
            };
            info.state = state;
            tasks.push(Task {
                downloaded: 0,
                speed: 0,
                state,
                info,
                error: None,
                running: None,
                pause_requested: false,
                speed_tick: Instant::now(),
            });
            next_key += 1;
        }

        let (tx, rx) = std_mpsc::channel();
        let clipboard = arboard::Clipboard::new().ok();

        HdApp {
            rt,
            engine,
            global,
            settings,
            tasks,
            next_key,
            events_rx: rx,
            events_tx: tx,
            add: AddDialog::default(),
            show_settings: false,
            dark: true,
            clipboard_monitor: false,
            clipboard,
            last_clip: None,
            toasts: Vec::new(),
        }
    }

    fn persist_queue(&self) {
        let mut q = Queue::default();
        for t in &self.tasks {
            let mut info = t.info.clone();
            info.state = t.state;
            info.downloaded = t.downloaded;
            info.file_size = info.file_size.or(None);
            q.items.push(info);
        }
        if let Err(e) = q.save() {
            tracing::error!("queue save failed: {e}");
        }
    }

    fn start_task(&mut self, key: u64) {
        let Some(t) = self.tasks.iter_mut().find(|t| key_of(t) == key) else {
            return;
        };
        if t.running.is_some() || t.state == DownloadState::Completed {
            return;
        }
        let (evt_tx, _evt_rx) = tokio::sync::broadcast::channel(1024);
        let (cancel_tx, cancel_rx) = tokio::sync::mpsc::channel::<()>(1);

        let params = RunParams {
            url: t.info.url.clone(),
            out_path: t.info.file_path.clone(),
            parts: t.info.parts,
            task_limit: t.info.speed_limit,
            auth: t.info.auth.clone(),
            extra_headers: t.info.extra_headers.clone(),
            proxy: t.info.proxy.clone(),
            resume: load_resume(&t.info.file_path),
            sftp_key: t.info.sftp_key.clone(),
        };

        let engine = self.engine.clone();
        let global = self.global.clone();
        let fwd = self.events_tx.clone();
        let rt = self.rt.clone();

        rt.spawn(async move {
            let mut rx = evt_tx.subscribe();
            let fwd2 = fwd.clone();
            // forward progress events to the UI thread
            let pump = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(Event::Progress(_, total)) => {
                            if fwd2.send(Fwd::Progress(key, total)).is_err() {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
            let res = engine.run(params, global, cancel_rx, evt_tx).await;
            let _ = pump.await;
            match res {
                Ok(()) => { let _ = fwd.send(Fwd::Terminal(key, true)); }
                Err(e) => { let _ = fwd.send(Fwd::Fail(key, e.to_string())); }
            }
        });

        t.running = Some(Running { cancel: cancel_tx });
        t.state = DownloadState::Connecting;
        t.pause_requested = false;
        t.error = None;
    }

    fn request_pause(&mut self, key: u64) {
        let Some(t) = self.tasks.iter_mut().find(|t| key_of(t) == key) else {
            return;
        };
        t.pause_requested = true;
        if let Some(r) = &t.running {
            let _ = r.cancel.try_send(());
        }
    }

    fn running_count(&self) -> usize {
        self.tasks.iter().filter(|t| t.running.is_some()).count()
    }

    fn pump_events(&mut self) {
        while let Ok(msg) = self.events_rx.try_recv() {
            match msg {
                Fwd::Progress(key, total) => {
                    if let Some(t) = self.tasks.iter_mut().find(|t| key_of(t) == key) {
                        let now = Instant::now();
                        let el = now.duration_since(t.speed_tick).as_secs_f64();
                        if el >= 1.0 {
                            let delta = total.saturating_sub(t.downloaded);
                            t.speed = (delta as f64 / el) as u64;
                            t.speed_tick = now;
                        }
                        t.downloaded = total;
                        t.state = DownloadState::Downloading;
                    }
                }
                Fwd::Terminal(key, ok) => {
                    if let Some(t) = self.tasks.iter_mut().find(|t| key_of(t) == key) {
                        t.running = None;
                        t.state = if ok {
                            DownloadState::Completed
                        } else if t.pause_requested {
                            DownloadState::Paused
                        } else {
                            DownloadState::Stopped
                        };
                    }
                    self.persist_queue();
                }
                Fwd::Fail(key, m) => {
                    if let Some(t) = self.tasks.iter_mut().find(|t| key_of(t) == key) {
                        t.running = None;
                        if t.pause_requested {
                            t.state = DownloadState::Paused;
                        } else {
                            t.state = DownloadState::Failed;
                            t.error = Some(m.clone());
                            self.toasts.push((format!("download failed: {m}"), Instant::now()));
                        }
                    }
                    self.persist_queue();
                }
            }
        }
        self.toasts.retain(|(_, t)| t.elapsed() < Duration::from_secs(6));
    }

    fn poll_clipboard(&mut self) {
        if !self.clipboard_monitor {
            return;
        }
        let Some(clip) = &mut self.clipboard else { return };
        if let Ok(text) = clip.get_text() {
            let s = text.trim().to_string();
            if s.is_empty() || Some(&s) == self.last_clip.as_ref() {
                return;
            }
            if Url::parse(&s).is_ok() {
                self.last_clip = Some(s.clone());
                self.add.clear();
                self.add.url = s.clone();
                self.toasts.push((format!("clipboard URL detected: {s}"), Instant::now()));
            }
        }
    }

    fn add_from_dialog(&mut self) {
        let url = match Url::parse(self.add.url.trim()) {
            Ok(u) => u,
            Err(e) => {
                self.toasts.push((format!("bad url: {e}"), Instant::now()));
                return;
            }
        };
        let out_dir = if self.add.output.trim().is_empty() {
            self.settings.download_directory.clone().unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
            })
        } else {
            PathBuf::from(self.add.output.trim())
        };
        let name = url
            .resource
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("download.bin")
            .to_string();
        let parts = self.add.parts.trim().parse().unwrap_or(self.settings.default_parts);
        let mut info = DownloadInfo::new(self.next_key, url, out_dir.join(name), parts.max(1));
        info.speed_limit = parse_limit(&self.add.limit);
        if !self.add.user.is_empty() {
            info.auth = Some(AuthCreds::new(&self.add.user, &self.add.pass));
        }
        if !self.add.proxy.trim().is_empty() {
            match hd_core::proxy::ProxyConfig::parse(self.add.proxy.trim()) {
                Ok(p) => info.proxy = Some(p),
                Err(e) => {
                    self.toasts.push((format!("bad proxy: {e}"), Instant::now()));
                    return;
                }
            }
        }
        if !self.add.key.trim().is_empty() {
            info.sftp_key = Some(PathBuf::from(self.add.key.trim()));
        }
        self.next_key += 1;
        self.tasks.push(Task {
            info,
            downloaded: 0,
            speed: 0,
            state: DownloadState::Queued,
            error: None,
            running: None,
            pause_requested: false,
            speed_tick: Instant::now(),
        });
        self.add.open = false;
        self.persist_queue();
    }
}

fn key_of(t: &Task) -> u64 {
    t.info.id
}

fn parse_limit(s: &str) -> Option<u64> {
    let s = s.trim().to_uppercase();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.strip_suffix('M') {
        Some(n) => (n, 1024.0 * 1024.0),
        None => match s.strip_suffix('K') {
            Some(n) => (n, 1024.0),
            None => (s.as_str(), 1.0),
        },
    };
    num.trim().parse::<f64>().ok().map(|v| (v * mult) as u64)
}

fn fmt_size(n: u64) -> String {
    const U: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

impl eframe::App for HdApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // theme
        ctx.set_theme(if self.dark { egui::Theme::Dark } else { egui::Theme::Light });

        self.pump_events();
        self.poll_clipboard();

        // scheduler: start queued tasks up to the concurrency limit
        let max = self.settings.max_concurrent_downloads.max(1) as usize;
        if self.running_count() < max {
            let next = self
                .tasks
                .iter()
                .find(|t| t.running.is_none() && t.state == DownloadState::Queued)
                .map(key_of);
            if let Some(key) = next {
                self.start_task(key);
            }
        }

        self.top_bar(ctx);
        self.task_list(ctx);
        self.add_window(ctx);
        self.settings_window(ctx);
        self.toasts_ui(ctx);

        ctx.request_repaint_after(Duration::from_millis(400));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist_queue();
    }
}

impl HdApp {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("➕ Add").clicked() {
                    self.add.clear();
                }
                if ui.button("⚙ Settings").clicked() {
                    self.show_settings = !self.show_settings;
                }
                ui.separator();
                ui.checkbox(&mut self.clipboard_monitor, "Monitor clipboard");
                ui.separator();
                ui.checkbox(&mut self.dark, "Dark mode");
                ui.separator();
                ui.label(format!(
                    "running {}/{}  (global limit {})",
                    self.running_count(),
                    self.settings.max_concurrent_downloads,
                    if self.settings.global_speed_limit == 0 {
                        "∞".to_string()
                    } else {
                        format!("{}/s", fmt_size(self.settings.global_speed_limit))
                    }
                ));
            });
        });
    }

    fn task_list(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if self.tasks.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label("No downloads. Click ➕ Add or enable clipboard monitoring.");
                    });
                    return;
                }
                let keys: Vec<u64> = self.tasks.iter().map(key_of).collect();
                for key in keys {
                    // snapshot the row before handing &mut self to the UI closure
                    let row = {
                        let Some(t) = self.tasks.iter().find(|t| key_of(t) == key) else {
                            continue;
                        };
                        let name = t
                            .info
                            .file_path
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| t.info.url.to_string());
                        let frac = t
                            .info
                            .file_size
                            .map(|s| (t.downloaded as f32 / s.max(1) as f32).clamp(0.0, 1.0))
                            .unwrap_or(0.0);
                        let size_txt = t
                            .info
                            .file_size
                            .map(|s| format!("{} / {}", fmt_size(t.downloaded), fmt_size(s)))
                            .unwrap_or_else(|| fmt_size(t.downloaded));
                        (name, frac, size_txt, t.speed, t.state, t.error.clone())
                    };
                    let (name, frac, size_txt, speed, state, err) = row;
                    ui.horizontal(|ui| {
                        ui.monospace(format!("#{:<3}", key));
                        ui.label(egui::RichText::new(&name).strong());

                        let bar = egui::ProgressBar::new(frac).show_percentage();
                        ui.add(bar);

                        ui.label(size_txt);
                        ui.label(format!("{}/s", fmt_size(speed)));
                        ui.colored_label(state_color(state), format!("{:?}", state));

                        match state {
                            DownloadState::Downloading | DownloadState::Connecting => {
                                if ui.button("⏸").clicked() {
                                    self.request_pause(key);
                                }
                            }
                            DownloadState::Paused | DownloadState::Stopped | DownloadState::Failed
                                if ui.button("▶").clicked() => {
                                    if let Some(t2) =
                                        self.tasks.iter_mut().find(|t2| key_of(t2) == key)
                                    {
                                        t2.state = DownloadState::Queued;
                                    }
                                    self.persist_queue();
                                }
                            _ => {}
                        }
                        if ui.button("✖").clicked() {
                            self.request_pause(key);
                            self.tasks.retain(|t2| key_of(t2) != key);
                            self.persist_queue();
                        }
                    });
                    if let Some(e) = &err {
                        ui.colored_label(egui::Color32::RED, format!("  └ {e}"));
                    }
                    ui.separator();
                }
            });
        });
    }

    fn add_window(&mut self, ctx: &egui::Context) {
        if !self.add.open {
            return;
        }
        let mut open = self.add.open;
        let mut cancelled = false;
        egui::Window::new("Add download")
            .open(&mut open)
            .default_width(420.0)
            .show(ctx, |ui| {
                egui::Grid::new("add_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("URL");
                    ui.add_sized([300.0, 20.0], egui::TextEdit::singleline(&mut self.add.url));
                    ui.end_row();
                    ui.label("Output dir");
                    ui.add_sized([300.0, 20.0], egui::TextEdit::singleline(&mut self.add.output));
                    ui.end_row();
                    ui.label("Parts");
                    ui.add_sized([300.0, 20.0], egui::TextEdit::singleline(&mut self.add.parts));
                    ui.end_row();
                    ui.label("Speed limit (e.g. 1M)");
                    ui.add_sized([300.0, 20.0], egui::TextEdit::singleline(&mut self.add.limit));
                    ui.end_row();
                    ui.label("User");
                    ui.add_sized([300.0, 20.0], egui::TextEdit::singleline(&mut self.add.user));
                    ui.end_row();
                    ui.label("Password");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.add.pass).password(true),
                    );
                    ui.end_row();
                    ui.label("Proxy (http:// or socks5://)");
                    ui.add_sized([300.0, 20.0], egui::TextEdit::singleline(&mut self.add.proxy));
                    ui.end_row();
                    ui.label("SFTP key file");
                    ui.add_sized([300.0, 20.0], egui::TextEdit::singleline(&mut self.add.key));
                    ui.end_row();
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Add").clicked() {
                        self.add_from_dialog();
                    }
                    if ui.button("Cancel").clicked() {
                        cancelled = true;
                    }
                });
            });
        if cancelled {
            open = false;
        }
        self.add.open = open;
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut dirty = false;
        egui::Window::new("Settings")
            .open(&mut self.show_settings)
            .default_width(420.0)
            .show(ctx, |ui| {
                egui::Grid::new("set_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Global speed limit (0 = unlimited)");
                    let mut gl = self.settings.global_speed_limit.to_string();
                    if ui.add_sized([200.0, 20.0], egui::TextEdit::singleline(&mut gl)).changed() {
                        if let Ok(v) = gl.trim().parse() {
                            self.settings.global_speed_limit = v;
                            dirty = true;
                        }
                    }
                    ui.end_row();
                    ui.label("Max concurrent downloads");
                    let mut mc = self.settings.max_concurrent_downloads.to_string();
                    if ui.add_sized([200.0, 20.0], egui::TextEdit::singleline(&mut mc)).changed() {
                        if let Ok(v) = mc.trim().parse() {
                            self.settings.max_concurrent_downloads = v;
                            dirty = true;
                        }
                    }
                    ui.end_row();
                    ui.label("Default parts");
                    let mut dp = self.settings.default_parts.to_string();
                    if ui.add_sized([200.0, 20.0], egui::TextEdit::singleline(&mut dp)).changed() {
                        if let Ok(v) = dp.trim().parse() {
                            self.settings.default_parts = v;
                            dirty = true;
                        }
                    }
                    ui.end_row();
                    ui.label("Download directory");
                    let mut dd = self
                        .settings
                        .download_directory
                        .clone()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if ui.add_sized([200.0, 20.0], egui::TextEdit::singleline(&mut dd)).changed() {
                        self.settings.download_directory = Some(PathBuf::from(dd.trim()));
                        dirty = true;
                    }
                    ui.end_row();
                });
                if dirty && ui.button("Save").clicked() {
                    let _ = self.settings.save();
                    *self.global.lock().unwrap() = Throttle::new(self.settings.global_speed_limit);
                    self.toasts.push(("settings saved".into(), Instant::now()));
                }
            });
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        let toasts = self.toasts.clone();
        for (i, (msg, _)) in toasts.iter().enumerate() {
            egui::Window::new(format!("toast_{i}"))
                .fixed_pos(egui::pos2(16.0, 40.0 + 36.0 * i as f32))
                .title_bar(false)
                .resizable(false)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label(msg);
                });
        }
    }
}

fn state_color(s: DownloadState) -> egui::Color32 {
    use DownloadState::*;
    match s {
        Completed => egui::Color32::from_rgb(90, 190, 110),
        Failed => egui::Color32::from_rgb(220, 90, 90),
        Downloading | Connecting => egui::Color32::from_rgb(90, 160, 230),
        Paused | Pausing | Stopping | Stopped => egui::Color32::from_rgb(220, 180, 80),
        Queued => egui::Color32::GRAY,
        Skipped => egui::Color32::GRAY,
    }
}
