// GUI-режим без консольного окна на Windows; для CLI-режимов
// подключаемся к родительской консоли в main().
#![cfg_attr(windows, windows_subsystem = "windows")]

mod control;
mod convert;
mod discovery;
mod gui;
mod update;
mod jitter;
mod protocol;
mod receiver;
mod stats;

mod capture;
mod sender;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use cpal::traits::{DeviceTrait, HostTrait};

use protocol::DEFAULT_PORT;

#[derive(Parser)]
#[command(
    name = "st",
    about = "SoundTransfer — передача звука с одного устройства на другое по локальной сети"
)]
struct Cli {
    /// Без подкоманды (двойной клик) открывается окно приложения.
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Запустить GUI со скрытым окном (только иконка в трее/строке меню) —
    /// для автозапуска при входе в систему
    #[arg(long)]
    hidden: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FormatArg {
    /// 16-бит (вдвое меньше трафика, на слух неотличимо)
    S16,
    /// 32-бит float как есть
    F32,
}

#[derive(Subcommand)]
enum Cmd {
    /// Захват звука и отправка на приёмник
    Send {
        /// IP или имя хоста приёмника. Не нужен при --dump
        target: Option<String>,
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Фреймов в пакете (128 = 2.67 мс при 48 кГц)
        #[arg(long, default_value_t = 128)]
        frames_per_packet: usize,
        #[arg(long, value_enum, default_value_t = FormatArg::S16)]
        format: FormatArg,
        /// Подстрока имени устройства вывода для захвата (по умолчанию — дефолтное)
        #[arg(long)]
        device: Option<String>,
        /// Диагностика: записать захваченный звук в raw-файл вместо отправки
        #[arg(long)]
        dump: Option<PathBuf>,
        /// Длительность записи для --dump, секунд
        #[arg(long, default_value_t = 10.0)]
        seconds: f32,
    },
    /// Приём и воспроизведение
    Recv {
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Целевой джиттер-буфер, мс (меньше = ниже задержка, больше = стабильнее)
        #[arg(long, default_value_t = 20)]
        buffer_ms: u32,
        /// Подстрока имени устройства вывода (по умолчанию — дефолтное)
        #[arg(long)]
        device: Option<String>,
        /// Громкость в процентах (0..150)
        #[arg(long, default_value_t = 100)]
        volume: u16,
    },
    /// Показать устройства вывода
    ListDevices,
}

fn stop_flag() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let _ = ctrlc::set_handler(move || {
        s.store(true, Ordering::Relaxed);
    });
    stop
}

/// В windows_subsystem="windows" консоли нет; для CLI-режимов
/// подключаемся к консоли родительского процесса (терминала).
#[cfg(windows)]
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn main() -> Result<()> {
    #[cfg(windows)]
    attach_parent_console();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();
    let cli = Cli::parse();

    let Some(cmd) = cli.cmd else {
        update::cleanup_leftovers();
        return gui::run(cli.hidden);
    };

    match cmd {
        Cmd::Send {
            target,
            port,
            frames_per_packet,
            format,
            device,
            dump,
            seconds,
        } => run_send(target, port, frames_per_packet, format, device, dump, seconds),
        Cmd::Recv {
            port,
            buffer_ms,
            device,
            volume,
        } => receiver::run(
            receiver::RecvOpts {
                port,
                buffer_ms,
                device,
                volume: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(
                    (volume.min(150) as f32 / 100.0).to_bits(),
                )),
            },
            stop_flag(),
            std::sync::Arc::new(stats::ReceiverStats::default()),
        ),
        Cmd::ListDevices => list_devices(),
    }
}

fn run_send(
    target: Option<String>,
    port: u16,
    frames_per_packet: usize,
    format: FormatArg,
    device: Option<String>,
    dump: Option<PathBuf>,
    seconds: f32,
) -> Result<()> {
    use anyhow::Context;
    use std::net::{SocketAddr, ToSocketAddrs};

    if let Some(path) = dump {
        return sender::dump(&path, seconds, device.as_deref());
    }
    let addr: SocketAddr = match target {
        Some(target) => (target.as_str(), port)
            .to_socket_addrs()
            .with_context(|| format!("не удалось разрешить адрес '{target}'"))?
            .next()
            .context("адрес не разрешился")?,
        None => {
            // IP не указан — ищем приёмник в сети по mDNS.
            log::info!("IP не указан — ищу приёмник в сети (3 с)…");
            let found = discovery::discover(std::time::Duration::from_secs(3))?;
            for f in &found {
                log::info!("найден: {} — {}:{}", f.name, f.ip, f.port);
            }
            let first = found.first().context(
                "приёмник не найден: запустите `st recv` на другом устройстве или укажите IP явно",
            )?;
            SocketAddr::new(first.ip, first.port)
        }
    };
    let wire_format = match format {
        FormatArg::S16 => protocol::WireFormat::S16le,
        FormatArg::F32 => protocol::WireFormat::F32le,
    };
    anyhow::ensure!(
        (64..=480).contains(&frames_per_packet),
        "--frames-per-packet должен быть в диапазоне 64..=480"
    );
    sender::run(
        sender::SendOpts {
            target: addr,
            frames_per_packet,
            wire_format,
            device,
        },
        stop_flag(),
        std::sync::Arc::new(stats::SenderStats::default()),
    )
}


fn list_devices() -> Result<()> {
    let host = cpal::default_host();
    let default_name = host
        .default_output_device()
        .and_then(|d| d.description().ok())
        .map(|d| d.name().to_string())
        .unwrap_or_default();
    println!("Устройства вывода ({:?}):", host.id());
    for device in host.output_devices()? {
        let name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "<unknown>".into());
        let mark = if name == default_name { " [default]" } else { "" };
        match device.default_output_config() {
            Ok(c) => println!(
                "  {name}{mark} — {} Гц, {} кан, {:?}",
                c.sample_rate(),
                c.channels(),
                c.sample_format()
            ),
            Err(_) => println!("  {name}{mark}"),
        }
    }
    Ok(())
}
