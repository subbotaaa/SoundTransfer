//! Отправитель (Windows): захват → кольцевой буфер → пакетизация → UDP.
#![cfg(windows)]

use std::fs::File;
use std::io::Write as _;
use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};

use crate::capture;
use crate::convert;
use crate::protocol::{HEADER_LEN, Header, PacketType, WireFormat};
use crate::stats::EverySecond;

pub struct SendOpts {
    pub target: SocketAddr,
    pub frames_per_packet: usize,
    pub wire_format: WireFormat,
    pub device: Option<String>,
}

/// Диагностический режим (M1): пишет ~`seconds` секунд захваченного PCM
/// (raw f32le interleaved) в файл и печатает peak/RMS для проверки.
pub fn dump(path: &Path, seconds: f32, device: Option<&str>) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<f32>>();
    let (stream, fmt) = capture::start_loopback(device, move |chunk| {
        let _ = tx.send(chunk.to_vec());
    })?;
    log::info!(
        "формат захвата: {} Гц, {} канала(ов), пишу {seconds} с в {}",
        fmt.sample_rate,
        fmt.channels,
        path.display()
    );

    let total_samples = (seconds * fmt.sample_rate as f32) as usize * fmt.channels as usize;
    let mut file = File::create(path)
        .with_context(|| format!("не удалось создать {}", path.display()))?;
    let mut written = 0usize;
    let mut peak = 0f32;
    let mut sum_sq = 0f64;
    let deadline = Instant::now() + Duration::from_secs_f32(seconds + 5.0);
    let mut callbacks = 0u64;

    while written < total_samples && Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(chunk) => {
                callbacks += 1;
                let mut bytes = vec![0u8; chunk.len() * 4];
                convert::f32_to_f32le(&chunk, &mut bytes);
                file.write_all(&bytes)?;
                for &s in &chunk {
                    peak = peak.max(s.abs());
                    sum_sq += (s as f64) * (s as f64);
                }
                written += chunk.len();
            }
            Err(_) => log::warn!("нет данных от захвата уже 500 мс (тишина в системе?)"),
        }
    }
    drop(stream);

    let rms = if written > 0 {
        (sum_sq / written as f64).sqrt()
    } else {
        0.0
    };
    log::info!(
        "готово: {written} сэмплов, {callbacks} callback'ов, peak={peak:.4}, rms={rms:.5}"
    );
    if written == 0 {
        anyhow::bail!("захват не дал ни одного сэмпла — loopback не работает");
    }
    if peak < 1e-6 {
        log::warn!(
            "захвачена сплошная тишина — включите звук на ПК во время дампа, чтобы проверить тракт"
        );
    }
    log::info!(
        "проверка в Audacity: File → Import → Raw Data, encoding: 32-bit float, little-endian, {} channels, {} Hz",
        fmt.channels,
        fmt.sample_rate
    );
    Ok(())
}

pub fn run(opts: SendOpts, stop: Arc<AtomicBool>) -> Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:0").context("bind UDP")?;
    sock.connect(opts.target)
        .with_context(|| format!("connect {}", opts.target))?;

    // Кольцевой буфер ~500 мс — с запасом; сеть выгребает почти сразу.
    let overflow = Arc::new(AtomicU64::new(0));
    let overflow_cb = overflow.clone();

    // Ёмкость посчитаем после того, как узнаем формат, поэтому захват
    // стартуем с буфером на максимальный разумный формат (48k/8ch хватит).
    let rb = HeapRb::<f32>::new(48000 * 8 / 2); // 500 мс при 48к стерео × запас
    let (mut prod, mut cons) = rb.split();

    let (stream, fmt) = capture::start_loopback(opts.device.as_deref(), move |chunk| {
        let pushed = prod.push_slice(chunk);
        if pushed < chunk.len() {
            overflow_cb.fetch_add((chunk.len() - pushed) as u64, Ordering::Relaxed);
        }
    })?;
    log::info!(
        "поток: {} Гц, {} канала(ов) → {} ({:?}, {} фреймов/пакет)",
        fmt.sample_rate,
        fmt.channels,
        opts.target,
        opts.wire_format,
        opts.frames_per_packet
    );

    let channels = fmt.channels as usize;
    let samples_per_packet = opts.frames_per_packet * channels;
    let bps = opts.wire_format.bytes_per_sample();
    let mut fbuf = vec![0f32; samples_per_packet];
    let mut pkt = vec![0u8; HEADER_LEN + samples_per_packet * bps];

    let mut header = Header {
        ptype: PacketType::Audio,
        channels: fmt.channels as u8,
        format: opts.wire_format,
        sample_rate: fmt.sample_rate,
        seq: 0,
        sample_pos: 0,
    };

    let mut ticker = EverySecond::new();
    let mut hello_timer = Instant::now() - Duration::from_secs(2);
    let mut pkts_sent = 0u64;
    let mut bytes_sent = 0u64;

    while !stop.load(Ordering::Relaxed) {
        // HELLO раз в секунду: несёт параметры формата, позволяет приёмнику
        // залочить отправителя и пересинхронизироваться после рестарта.
        if hello_timer.elapsed() >= Duration::from_secs(1) {
            hello_timer = Instant::now();
            let mut hello = [0u8; HEADER_LEN];
            Header {
                ptype: PacketType::Hello,
                ..header
            }
            .write(&mut hello);
            let _ = sock.send(&hello);
        }

        if cons.occupied_len() >= samples_per_packet {
            cons.pop_slice(&mut fbuf);
            header.ptype = PacketType::Audio;
            header.write(&mut pkt[..HEADER_LEN]);
            match opts.wire_format {
                WireFormat::S16le => {
                    convert::f32_to_s16le(&fbuf, &mut pkt[HEADER_LEN..]);
                }
                WireFormat::F32le => {
                    convert::f32_to_f32le(&fbuf, &mut pkt[HEADER_LEN..]);
                }
            }
            match sock.send(&pkt) {
                Ok(n) => {
                    pkts_sent += 1;
                    bytes_sent += n as u64;
                }
                Err(e) => log::warn!("send: {e}"),
            }
            header.seq = header.seq.wrapping_add(1);
            header.sample_pos += opts.frames_per_packet as u64;
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }

        if ticker.tick() {
            log::info!(
                "tx: {} пак/с, {:.1} кбит/с, ring {} мс, переполнений {}",
                pkts_sent,
                bytes_sent as f64 * 8.0 / 1000.0,
                cons.occupied_len() / channels * 1000 / fmt.sample_rate as usize,
                overflow.load(Ordering::Relaxed),
            );
            pkts_sent = 0;
            bytes_sent = 0;
        }
    }

    // BYE — приёмник узнает о штатном завершении.
    let mut bye = [0u8; HEADER_LEN];
    Header {
        ptype: PacketType::Bye,
        ..header
    }
    .write(&mut bye);
    let _ = sock.send(&bye);
    drop(stream);
    log::info!("остановлено");
    Ok(())
}
