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
            running: AtomicBool::new(false),
            info: Mutex::new(String::new()),
            repaint: Mutex::new(None),
        })
    }
}

pub fn spawn(port: u16, state: Arc<ControlState>) {
    std::thread::spawn(move || {
        let listener = match TcpListener::bind(("0.0.0.0", port)) {
            Ok(l) => l,
            Err(e) => {
                log::warn!("порт управления :{port} занят ({e}) — управление из HA недоступно");
                return;
            }
        };
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
            (
                "200 OK",
                format!(
                    r#"{{"running":{running},"info":"{}"}}"#,
                    info.replace('"', "'")
                ),
            )
        }
        _ => ("404 Not Found", r#"{"ok":false}"#.to_string()),
    }
}

fn wake(state: &ControlState) {
    if let Some(ctx) = state.repaint.lock().unwrap().as_ref() {
        ctx.request_repaint();
    }
}
