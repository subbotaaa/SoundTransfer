//! HTTP-сервер управления для Home Assistant и прочей автоматизации.
//!
//! Слушает TCP-порт (по умолчанию 48101):
//!   POST /start  — запустить передачу/приём (как кнопка «Запустить»)
//!   POST /stop   — остановить
//!   GET  /status — {"running":true|false,"info":"..."}
//!
//! Без аутентификации — рассчитан на доверенную домашнюю сеть.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const CONTROL_PORT: u16 = 48101;

pub struct ControlState {
    /// Команда от HTTP-потока к GUI: Some(true)=start, Some(false)=stop.
    pub pending: Mutex<Option<bool>>,
    /// Запрошенная громкость (0..1.5), применяет GUI.
    pub pending_volume: Mutex<Option<f32>>,
    /// Запрос установки обновления (POST /update), применяет GUI.
    pub pending_update: AtomicBool,
    /// Текущая громкость для /status (f32 в битах, зеркалит GUI).
    pub volume: std::sync::atomic::AtomicU32,
    /// Текущее состояние (выставляет GUI).
    pub running: AtomicBool,
    /// Человекочитаемое описание состояния для /status.
    pub info: Mutex<String>,
    /// Контекст egui — чтобы разбудить update() после команды.
    pub repaint: Mutex<Option<eframe::egui::Context>>,
}

impl ControlState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::new(None),
            pending_volume: Mutex::new(None),
            pending_update: AtomicBool::new(false),
            volume: std::sync::atomic::AtomicU32::new(1.0f32.to_bits()),
            running: AtomicBool::new(false),
            info: Mutex::new(String::new()),
            repaint: Mutex::new(None),
        })
    }
}

pub fn spawn(port: u16, state: Arc<ControlState>) {
    std::thread::spawn(move || {
        // Несколько попыток: после самообновления старый процесс может
        // ещё пару секунд держать порт.
        let mut listener = None;
        for attempt in 0..10 {
            match TcpListener::bind(("0.0.0.0", port)) {
                Ok(l) => {
                    listener = Some(l);
                    break;
                }
                Err(e) if attempt == 9 => {
                    log::warn!(
                        "порт управления :{port} занят ({e}) — управление из HA недоступно"
                    );
                    return;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(500)),
            }
        }
        let listener = listener.unwrap();
        log::info!("управление: http://0.0.0.0:{port} (/start, /stop, /status)");
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { continue };
            let Ok(peer_clone) = sock.try_clone() else { continue };
            let mut line = String::new();
            let mut reader = BufReader::new(peer_clone);
            if reader.read_line(&mut line).is_err() {
                continue;
            }

            let (code, body) = route(&line, &state);
            let resp = format!(
                "HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
        }
    });
}

fn route(request_line: &str, state: &ControlState) -> (&'static str, String) {
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    match (method, path) {
        ("POST", "/start") => {
            *state.pending.lock().unwrap() = Some(true);
            wake(state);
            ("200 OK", r#"{"ok":true,"cmd":"start"}"#.to_string())
        }
        ("POST", "/stop") => {
            *state.pending.lock().unwrap() = Some(false);
            wake(state);
            ("200 OK", r#"{"ok":true,"cmd":"stop"}"#.to_string())
        }
        ("GET", "/status") | ("GET", "/") => {
            let running = state.running.load(Ordering::Relaxed);
            let info = state.info.lock().unwrap().clone();
            let volume =
                (f32::from_bits(state.volume.load(Ordering::Relaxed)) * 100.0).round() as u32;
            (
                "200 OK",
                format!(
                    r#"{{"running":{running},"volume":{volume},"info":"{}"}}"#,
                    info.replace('"', "'")
                ),
            )
        }
        // POST /volume/80 — громкость приёмника в процентах (0..150).
        ("POST", p) if p.starts_with("/volume/") => {
            match p["/volume/".len()..].parse::<u32>() {
                Ok(pct) if pct <= 150 => {
                    *state.pending_volume.lock().unwrap() = Some(pct as f32 / 100.0);
                    wake(state);
                    ("200 OK", format!(r#"{{"ok":true,"volume":{pct}}}"#))
                }
                _ => (
                    "400 Bad Request",
                    r#"{"ok":false,"error":"volume 0..150"}"#.to_string(),
                ),
            }
        }
        // POST /update — установить доступное обновление (если есть).
        ("POST", "/update") => {
            state.pending_update.store(true, Ordering::Relaxed);
            wake(state);
            ("200 OK", r#"{"ok":true,"cmd":"update"}"#.to_string())
        }
        _ => ("404 Not Found", r#"{"ok":false}"#.to_string()),
    }
}

fn wake(state: &ControlState) {
    if let Some(ctx) = state.repaint.lock().unwrap().as_ref() {
        ctx.request_repaint();
    }
}
