//! mDNS-автообнаружение: приёмник анонсирует себя в локальной сети,
//! отправитель находит его без ввода IP.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

pub const SERVICE_TYPE: &str = "_soundtransfer._udp.local.";

fn host_label() -> String {
    let name = gethostname::gethostname().to_string_lossy().into_owned();
    let name = name.trim_end_matches(".local").trim_end_matches('.');
    if name.is_empty() {
        "soundtransfer".to_string()
    } else {
        name.to_string()
    }
}

/// Анонсирует приёмник на `port`. Возвращённый daemon нужно держать
/// живым, пока приёмник работает.
pub fn advertise(port: u16) -> Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new().context("mDNS: запуск daemon")?;
    let host = host_label();
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        &host,
        &format!("{host}.local."),
        (), // адреса подставляются автоматически (enable_addr_auto)
        port,
        None::<std::collections::HashMap<String, String>>,
    )
    .context("mDNS: описание сервиса")?
    .enable_addr_auto();
    daemon.register(info).context("mDNS: регистрация")?;
    log::info!("mDNS: анонсирую {SERVICE_TYPE} на порту {port}");
    Ok(daemon)
}

#[derive(Debug, Clone)]
pub struct Found {
    pub name: String,
    pub ip: IpAddr,
    pub port: u16,
}

/// Ищет приёмники в сети в течение `timeout`.
pub fn discover(timeout: Duration) -> Result<Vec<Found>> {
    let daemon = ServiceDaemon::new().context("mDNS: запуск daemon")?;
    let rx = daemon.browse(SERVICE_TYPE).context("mDNS: browse")?;
    let mut found: Vec<Found> = Vec::new();
    let deadline = Instant::now() + timeout;

    // Один хост может резолвиться несколько раз с разными адресами
    // (IPv6, loopback…) — держим по одной записи на сервис с лучшим IP.
    fn score(ip: &IpAddr) -> u8 {
        match ip {
            IpAddr::V4(v4) if !v4.is_loopback() => 3,
            IpAddr::V4(_) => 2,
            IpAddr::V6(_) => 1,
        }
    }

    let mut names: Vec<String> = Vec::new();
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match rx.recv_timeout(deadline - now) {
            Ok(ServiceEvent::ServiceResolved(svc)) => {
                let best = svc
                    .get_addresses()
                    .iter()
                    .map(|a| a.to_ip_addr())
                    .max_by_key(score);
                let Some(ip) = best else { continue };
                let name = svc
                    .get_fullname()
                    .split('.')
                    .next()
                    .unwrap_or("?")
                    .to_string();
                let port = svc.get_port();
                let key = format!("{}:{port}", svc.get_fullname());
                match names.iter().position(|n| *n == key) {
                    Some(i) => {
                        if score(&ip) > score(&found[i].ip) {
                            found[i].ip = ip;
                        }
                    }
                    None => {
                        names.push(key);
                        found.push(Found { name, ip, port });
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    Ok(found)
}
