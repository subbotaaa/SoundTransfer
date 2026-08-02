//! Графический интерфейс (eframe/egui). Запускается, если бинарник
//! стартовали без аргументов — т.е. двойным кликом.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};
use eframe::egui;

use crate::control::{CONTROL_PORT, ControlState};
use crate::discovery;
use crate::protocol::DEFAULT_PORT;
use crate::receiver;
use crate::stats::{RateMeter, ReceiverStats, SenderStats, get_msg, set_msg};

/// В Dock иконка берётся у процесса, а не у .app-бандла, поэтому
/// выставляем её программно — работает и при запуске бинарника напрямую.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let bytes: &[u8] = include_bytes!("../assets/icon.png");
    let data = NSData::with_bytes(bytes);
    let app = NSApplication::sharedApplication(mtm);
    if let Some(img) = NSImage::initWithData(NSImage::alloc(), &data) {
        unsafe {
            app.setApplicationIconImage(Some(&img));
        }
    }
}

/// Пока окно скрыто в трее, приложение не должно занимать место в Dock
/// и в переключателе Cmd-Tab: policy Accessory убирает его оттуда,
/// Regular — возвращает при показе окна.
#[cfg(target_os = "macos")]
fn set_dock_visible(visible: bool) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let policy = if visible {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    };
    app.setActivationPolicy(policy);
    if visible {
        // После Accessory окно само не выходит на передний план.
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}

#[cfg(not(target_os = "macos"))]
fn set_dock_visible(_visible: bool) {}

/// Показ окна из трея на macOS. Пока окно скрыто (orderOut) и приложение
/// в режиме Accessory, egui-цикл может не проснуться на request_repaint,
/// поэтому окно выводим нативно и синхронно — обработчик меню трея
/// выполняется в главном потоке.
#[cfg(target_os = "macos")]
fn macos_show_from_tray() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    set_dock_visible(true);
    let app = NSApplication::sharedApplication(mtm);
    for win in app.windows().iter() {
        win.makeKeyAndOrderFront(None);
    }
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
}

#[cfg(not(target_os = "macos"))]
fn macos_show_from_tray() {}

enum Discovery {
    Idle,
    Searching,
    Done(Vec<discovery::Found>),
    Failed(String),
}

/// Иконка в системном трее (Windows) / строке меню (macOS) с меню
/// Показать / Запустить / Остановить / Выход. Команды меню идут через
/// тот же канал, что и Home Assistant (`ControlState::pending`), а
/// показ/закрытие окна — через `TrayCmd`.
struct TrayCmd {
    show_window: std::sync::atomic::AtomicBool,
    quit: std::sync::atomic::AtomicBool,
}

fn setup_tray(
    ctx: &egui::Context,
    control: &Arc<ControlState>,
) -> (Option<tray_icon::TrayIcon>, Arc<TrayCmd>) {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{TrayIconBuilder, TrayIconEvent};

    let cmd = Arc::new(TrayCmd {
        show_window: std::sync::atomic::AtomicBool::new(false),
        quit: std::sync::atomic::AtomicBool::new(false),
    });

    let menu = Menu::new();
    let build = menu.append_items(&[
        &MenuItem::with_id("show", "Показать окно", true, None),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id("start", "Запустить", true, None),
        &MenuItem::with_id("stop", "Остановить", true, None),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id("quit", "Выход", true, None),
    ]);
    if let Err(e) = build {
        log::warn!("трей: не удалось создать меню: {e}");
        return (None, cmd);
    }

    {
        let ctx = ctx.clone();
        let control = control.clone();
        let cmd = cmd.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            match event.id.as_ref() {
                "show" => {
                    macos_show_from_tray();
                    cmd.show_window.store(true, Ordering::Relaxed);
                }
                "start" => *control.pending.lock().unwrap() = Some(true),
                "stop" => *control.pending.lock().unwrap() = Some(false),
                "quit" => cmd.quit.store(true, Ordering::Relaxed),
                _ => {}
            }
            ctx.request_repaint();
        }));
    }
    {
        // Клик левой кнопкой по иконке — показать окно (Windows;
        // на macOS левый клик открывает меню).
        let ctx = ctx.clone();
        let cmd = cmd.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Up,
                ..
            } = event
            {
                macos_show_from_tray();
                cmd.show_window.store(true, Ordering::Relaxed);
                ctx.request_repaint();
            }
        }));
    }

    let rgba = include_bytes!("../assets/icon32.rgba").to_vec();
    let icon = match tray_icon::Icon::from_rgba(rgba, 32, 32) {
        Ok(i) => i,
        Err(e) => {
            log::warn!("трей: иконка не собралась: {e}");
            return (None, cmd);
        }
    };
    match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("SoundTransfer")
        .with_icon(icon)
        .build()
    {
        Ok(tray) => (Some(tray), cmd),
        Err(e) => {
            log::warn!("трей недоступен: {e}");
            (None, cmd)
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Mode {
    Send,
    Recv,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Settings {
    mode: Mode,
    target_ip: String,
    port: u16,
    buffer_ms: u32,
    device: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // На Windows чаще нужен отправитель, на Mac — приёмник.
            mode: if cfg!(windows) { Mode::Send } else { Mode::Recv },
            target_ip: String::new(),
            port: DEFAULT_PORT,
            buffer_ms: 20,
            device: String::new(),
        }
    }
}

enum Running {
    Send {
        stop: Arc<AtomicBool>,
        join: JoinHandle<()>,
        stats: Arc<SenderStats>,
    },
    Recv {
        stop: Arc<AtomicBool>,
        join: JoinHandle<()>,
        stats: Arc<ReceiverStats>,
    },
}

impl Running {
    fn stop_flag(&self) -> &Arc<AtomicBool> {
        match self {
            Running::Send { stop, .. } | Running::Recv { stop, .. } => stop,
        }
    }

    fn is_finished(&self) -> bool {
        match self {
            Running::Send { join, .. } | Running::Recv { join, .. } => join.is_finished(),
        }
    }

    fn error(&self) -> Option<String> {
        match self {
            Running::Send { stats, .. } => get_msg(&stats.error),
            Running::Recv { stats, .. } => get_msg(&stats.error),
        }
    }
}

pub struct App {
    settings: Settings,
    running: Option<Running>,
    devices: Vec<String>,
    pkt_rate: RateMeter,
    byte_rate: RateMeter,
    last_error: Option<String>,
    discovery: Arc<std::sync::Mutex<Discovery>>,
    control: Arc<ControlState>,
    _tray: Option<tray_icon::TrayIcon>,
    tray_cmd: Arc<TrayCmd>,
    /// --hidden: спрятать окно вскоре после старта. eframe 0.35 сам
    /// показывает окно после первого кадра, игнорируя with_visible(false),
    /// поэтому прячем с небольшой задержкой, когда его логика уже отработала.
    hide_deadline: Option<std::time::Instant>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, hidden: bool) -> Self {
        let settings = cc
            .storage
            .and_then(|s| eframe::get_value::<Settings>(s, eframe::APP_KEY))
            .unwrap_or_default();
        // HTTP-управление для Home Assistant: /start /stop /status.
        let control = ControlState::new();
        *control.repaint.lock().unwrap() = Some(cc.egui_ctx.clone());
        crate::control::spawn(CONTROL_PORT, control.clone());

        let (tray, tray_cmd) = setup_tray(&cc.egui_ctx, &control);

        Self {
            settings,
            running: None,
            devices: list_output_devices(),
            pkt_rate: RateMeter::new(),
            byte_rate: RateMeter::new(),
            last_error: None,
            discovery: Arc::new(std::sync::Mutex::new(Discovery::Idle)),
            control,
            _tray: tray,
            tray_cmd,
            hide_deadline: hidden
                .then(|| std::time::Instant::now() + Duration::from_millis(150)),
        }
    }

    fn start_discovery(&mut self) {
        let slot = self.discovery.clone();
        *slot.lock().unwrap() = Discovery::Searching;
        std::thread::spawn(move || {
            let result = discovery::discover(Duration::from_secs(3));
            let mut g = slot.lock().unwrap();
            *g = match result {
                Ok(found) => Discovery::Done(found),
                Err(e) => Discovery::Failed(format!("{e:#}")),
            };
        });
    }

    fn start(&mut self) {
        self.last_error = None;
        self.pkt_rate.reset();
        self.byte_rate.reset();
        let stop = Arc::new(AtomicBool::new(false));
        let device = if self.settings.device.is_empty() {
            None
        } else {
            Some(self.settings.device.clone())
        };
        match self.settings.mode {
            Mode::Send => {
                #[cfg(windows)]
                {
                    // Пустой IP (напр., старт из Home Assistant на чистых
                    // настройках) — ищем приёмник в сети сами.
                    if self.settings.target_ip.trim().is_empty() {
                        match discovery::discover(Duration::from_secs(3)) {
                            Ok(found) if !found.is_empty() => {
                                self.settings.target_ip = found[0].ip.to_string();
                                self.settings.port = found[0].port;
                            }
                            _ => {
                                self.last_error = Some(
                                    "Приёмник не найден — запустите приём на Mac или укажите IP"
                                        .to_string(),
                                );
                                return;
                            }
                        }
                    }
                    let stats = Arc::new(SenderStats::default());
                    let target = format!("{}:{}", self.settings.target_ip.trim(), self.settings.port);
                    use std::net::ToSocketAddrs;
                    let addr = match target.to_socket_addrs().ok().and_then(|mut a| a.next()) {
                        Some(a) => a,
                        None => {
                            self.last_error = Some(format!("Некорректный адрес: {target}"));
                            return;
                        }
                    };
                    let opts = crate::sender::SendOpts {
                        target: addr,
                        frames_per_packet: 128,
                        wire_format: crate::protocol::WireFormat::S16le,
                        device,
                    };
                    let s2 = stats.clone();
                    let stop2 = stop.clone();
                    let join = std::thread::spawn(move || {
                        if let Err(e) = crate::sender::run(opts, stop2, s2.clone()) {
                            set_msg(&s2.error, format!("{e:#}"));
                        }
                    });
                    self.running = Some(Running::Send { stop, join, stats });
                    self.control.running.store(true, Ordering::Relaxed);
                    *self.control.info.lock().unwrap() =
                        format!("send:{}", self.settings.target_ip.trim());
                }
                #[cfg(not(windows))]
                {
                    self.last_error =
                        Some("Отправка работает только на Windows".to_string());
                }
            }
            Mode::Recv => {
                let stats = Arc::new(ReceiverStats::default());
                let opts = receiver::RecvOpts {
                    port: self.settings.port,
                    buffer_ms: self.settings.buffer_ms,
                    device,
                };
                let s2 = stats.clone();
                let stop2 = stop.clone();
                let join = std::thread::spawn(move || {
                    if let Err(e) = receiver::run(opts, stop2, s2.clone()) {
                        set_msg(&s2.error, format!("{e:#}"));
                    }
                });
                self.running = Some(Running::Recv { stop, join, stats });
                self.control.running.store(true, Ordering::Relaxed);
                *self.control.info.lock().unwrap() = "recv".to_string();
            }
        }
    }

    fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            r.stop_flag().store(true, Ordering::Relaxed);
            self.last_error = r.error();
            match r {
                Running::Send { join, .. } | Running::Recv { join, .. } => {
                    let _ = join.join();
                }
            }
        }
        self.control.running.store(false, Ordering::Relaxed);
        self.control.info.lock().unwrap().clear();
        self.pkt_rate.reset();
        self.byte_rate.reset();
    }

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        let busy = self.running.is_some();
        egui::Grid::new("settings")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                if self.settings.mode == Mode::Send {
                    ui.label("IP приёмника (Mac):");
                    ui.horizontal(|ui| {
                        ui.add_enabled(
                            !busy,
                            egui::TextEdit::singleline(&mut self.settings.target_ip)
                                .desired_width(140.0)
                                .hint_text("192.168.1.44"),
                        );
                        let searching = matches!(
                            *self.discovery.lock().unwrap(),
                            Discovery::Searching
                        );
                        if ui
                            .add_enabled(!busy && !searching, egui::Button::new("🔍 Найти"))
                            .on_hover_text("Найти Mac с запущенным приёмником (mDNS)")
                            .clicked()
                        {
                            self.start_discovery();
                        }
                    });
                    ui.end_row();
                } else {
                    ui.label("Джиттер-буфер:");
                    ui.add_enabled(
                        !busy,
                        egui::Slider::new(&mut self.settings.buffer_ms, 5..=100).suffix(" мс"),
                    );
                    ui.end_row();
                }

                ui.label("Порт:");
                ui.add_enabled(
                    !busy,
                    egui::DragValue::new(&mut self.settings.port).range(1024..=65535),
                );
                ui.end_row();

                ui.label(if self.settings.mode == Mode::Send {
                    "Захват с устройства:"
                } else {
                    "Вывод в устройство:"
                });
                ui.add_enabled_ui(!busy, |ui| {
                    egui::ComboBox::from_id_salt("device")
                        .width(220.0)
                        .selected_text(if self.settings.device.is_empty() {
                            "По умолчанию (системное)"
                        } else {
                            self.settings.device.as_str()
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.settings.device,
                                String::new(),
                                "По умолчанию (системное)",
                            );
                            for d in &self.devices {
                                ui.selectable_value(
                                    &mut self.settings.device,
                                    d.clone(),
                                    d,
                                );
                            }
                        });
                });
                ui.end_row();
            });
    }

    fn ui_discovery(&mut self, ui: &mut egui::Ui) {
        if self.settings.mode != Mode::Send {
            return;
        }
        let state = self.discovery.clone();
        let mut clear = false;
        match &*state.lock().unwrap() {
            Discovery::Idle => {}
            Discovery::Searching => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Ищу приёмники в сети…");
                });
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            }
            Discovery::Done(found) if found.is_empty() => {
                ui.label(
                    egui::RichText::new(
                        "Не найдено — убедитесь, что на Mac запущен приём",
                    )
                    .color(egui::Color32::GRAY),
                );
            }
            Discovery::Done(found) => {
                // Один найденный — сразу подставляем; несколько — по клику.
                if found.len() == 1 {
                    let f = &found[0];
                    self.settings.target_ip = f.ip.to_string();
                    self.settings.port = f.port;
                    ui.label(format!("Найден: {} ({})", f.name, f.ip));
                    clear = true;
                } else {
                    ui.label("Найдено несколько — выберите:");
                    for f in found {
                        if ui.link(format!("{} — {}:{}", f.name, f.ip, f.port)).clicked()
                        {
                            self.settings.target_ip = f.ip.to_string();
                            self.settings.port = f.port;
                            clear = true;
                        }
                    }
                }
            }
            Discovery::Failed(e) => {
                ui.label(
                    egui::RichText::new(format!("Поиск не удался: {e}"))
                        .color(egui::Color32::LIGHT_RED),
                );
            }
        }
        if clear {
            *state.lock().unwrap() = Discovery::Idle;
        }
    }

    fn ui_stats(&mut self, ui: &mut egui::Ui) {
        let Some(r) = &self.running else { return };
        ui.separator();
        match r {
            Running::Send { stats, .. } => {
                self.pkt_rate.update(&stats.pkts);
                self.byte_rate.update(&stats.bytes);
                if let Some(s) = get_msg(&stats.status) {
                    ui.label(egui::RichText::new(format!("▶ {s}")).strong());
                }
                ui.label(format!(
                    "Отправка: {:.0} пак/с · {:.0} кбит/с",
                    self.pkt_rate.per_sec,
                    self.byte_rate.per_sec * 8.0 / 1000.0
                ));
                let ovf = stats.ring_overflow.load(Ordering::Relaxed);
                if ovf > 0 {
                    ui.label(format!("Переполнений буфера захвата: {ovf}"));
                }
            }
            Running::Recv { stats, .. } => {
                self.pkt_rate.update(&stats.pkts);
                self.byte_rate.update(&stats.bytes);
                match get_msg(&stats.status) {
                    Some(s) => {
                        ui.label(egui::RichText::new(format!("▶ {s}")).strong());
                    }
                    None => {
                        ui.label("Жду отправителя…");
                    }
                }
                ui.label(format!(
                    "Приём: {:.0} пак/с · {:.0} кбит/с · буфер {} мс",
                    self.pkt_rate.per_sec,
                    self.byte_rate.per_sec * 8.0 / 1000.0,
                    stats.fill_ms.load(Ordering::Relaxed)
                ));
                let lost = stats.lost.load(Ordering::Relaxed);
                let underruns = stats.underruns.load(Ordering::Relaxed);
                let cuts = stats.cuts.load(Ordering::Relaxed);
                ui.label(format!(
                    "Потеряно пакетов: {lost} · провалов буфера: {underruns} · срезов: {cuts}"
                ));
                if underruns > 0 {
                    ui.label(
                        egui::RichText::new(
                            "Если провалы растут — остановите и увеличьте джиттер-буфер",
                        )
                        .small()
                        .color(egui::Color32::GRAY),
                    );
                }
            }
        }
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if let Some(deadline) = self.hide_deadline {
            let now = std::time::Instant::now();
            if now >= deadline {
                self.hide_deadline = None;
                if self._tray.is_some() {
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    set_dock_visible(false);
                }
            } else {
                ui.ctx().request_repaint_after(deadline - now);
            }
        }

        // Рабочий поток мог завершиться сам (ошибка) — забираем состояние.
        if self.running.as_ref().is_some_and(|r| r.is_finished()) {
            self.stop();
        }

        // Команды из Home Assistant (HTTP /start, /stop) и меню трея.
        let pending = self.control.pending.lock().unwrap().take();
        match pending {
            Some(true) if self.running.is_none() => self.start(),
            Some(false) if self.running.is_some() => self.stop(),
            _ => {}
        }

        // Команды трея: показать окно / выйти.
        if self.tray_cmd.show_window.swap(false, Ordering::Relaxed) {
            set_dock_visible(true);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        let quitting = self.tray_cmd.quit.load(Ordering::Relaxed);
        if quitting {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // Кнопка закрытия окна прячет в трей (если трей есть);
        // по-настоящему выходим только через «Выход» в меню трея.
        if ui.ctx().input(|i| i.viewport().close_requested())
            && !quitting
            && self._tray.is_some()
        {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Visible(false));
            set_dock_visible(false);
        }

        {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("SoundTransfer");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new("Windows » Mac по локальной сети")
                            .small()
                            .color(egui::Color32::GRAY),
                    );
                });
            });
            ui.add_space(8.0);

            let busy = self.running.is_some();
            ui.add_enabled_ui(!busy, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.settings.mode, Mode::Send, "  Отправка  ");
                    ui.selectable_value(&mut self.settings.mode, Mode::Recv, "  Приём  ");
                });
            });
            if self.settings.mode == Mode::Send && !cfg!(windows) {
                ui.label(
                    egui::RichText::new("Отправка звука работает только на Windows")
                        .color(egui::Color32::LIGHT_RED),
                );
            }
            ui.add_space(8.0);

            self.ui_settings(ui);
            self.ui_discovery(ui);
            ui.add_space(12.0);

            let can_start = match self.settings.mode {
                Mode::Send => cfg!(windows) && !self.settings.target_ip.trim().is_empty(),
                Mode::Recv => true,
            };
            if self.running.is_none() {
                if ui
                    .add_enabled(
                        can_start,
                        egui::Button::new(
                            egui::RichText::new("▶  Запустить").size(16.0),
                        )
                        .min_size(egui::vec2(160.0, 32.0)),
                    )
                    .clicked()
                {
                    self.start();
                }
            } else if ui
                .add(
                    egui::Button::new(egui::RichText::new("⏹  Остановить").size(16.0))
                        .min_size(egui::vec2(160.0, 32.0)),
                )
                .clicked()
            {
                self.stop();
            }

            self.ui_stats(ui);

            if let Some(err) = &self.last_error {
                ui.separator();
                ui.label(egui::RichText::new(err).color(egui::Color32::LIGHT_RED));
            }
        }

        if self.running.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }

    fn on_exit(&mut self) {
        self.stop();
    }
}

fn list_output_devices() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|it| {
            it.filter_map(|d| d.description().ok().map(|desc| desc.name().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

pub fn run(hidden: bool) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([440.0, 400.0])
            .with_min_inner_size([380.0, 320.0])
            // --hidden: старт сразу в трее, окно покажется по клику.
            .with_visible(!hidden),
        ..Default::default()
    };
    eframe::run_native(
        "SoundTransfer",
        options,
        Box::new(move |cc| {
            #[cfg(target_os = "macos")]
            set_dock_icon();
            Ok(Box::new(App::new(cc, hidden)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("GUI: {e}"))
}
