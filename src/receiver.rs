//! Приёмник: UDP → seq-трекинг/джиттер → кольцевой буфер → cpal playback.
//! Работает и на macOS (CoreAudio), и на Windows (WASAPI).

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};

use crate::convert;
use crate::jitter::{DriftComp, SeqTracker, Slip, Verdict};
use crate::protocol::{HEADER_LEN, Header, PacketType, WireFormat};
use crate::stats::{EverySecond, ReceiverStats, set_msg};

pub struct RecvOpts {
    pub port: u16,
    pub buffer_ms: u32,
    pub device: Option<String>,
    /// Громкость 0..~1.5 (f32 в битах) — можно менять на лету.
    pub volume: Arc<std::sync::atomic::AtomicU32>,
}

/// Общие атомики между сетевым потоком и аудио-callback'ом.
struct Shared {
    /// true — копим до target и молчим (старт или после underrun).
    prebuffering: AtomicBool,
    underruns: AtomicU64,
    cuts: AtomicU64,
    frames_played: AtomicU64,
}

/// Максимум тишины, вставляемой за один разрыв (защита от абсурдных дыр).
const MAX_GAP_MS: u64 = 500;
/// Порог среза переполнения: target + 40 мс.
const CUT_OVER_TARGET_MS: u32 = 40;
/// Сколько текущий отправитель может молчать, прежде чем мы примем другого.
/// Отправитель шлёт HELLO раз в секунду, аудио — сотни пакетов в секунду.
const PEER_SILENCE: Duration = Duration::from_millis(1500);
/// Полная тишина в сети — закрываем вывод и снова ждём отправителя.
const IDLE_RESTART: Duration = Duration::from_secs(10);
/// Как часто подтверждаем отправителю, что поток доходит.
const ACK_PERIOD: Duration = Duration::from_secs(1);

/// Чем закончилась сессия с конкретным отправителем.
enum Next {
    /// Взведён stop — выходим совсем.
    Stop,
    /// Нужен новый аудиопоток. `Some` — уже знаем нового отправителя
    /// (сменился формат), `None` — ждём заново.
    Restart(Option<(Header, SocketAddr)>),
}

pub fn run(opts: RecvOpts, stop: Arc<AtomicBool>, stats: Arc<ReceiverStats>) -> Result<()> {
    let sock = UdpSocket::bind(("0.0.0.0", opts.port))
        .with_context(|| format!("bind UDP :{}", opts.port))?;
    sock.set_read_timeout(Some(Duration::from_millis(100)))?;
    log::info!("слушаю UDP :{} — жду отправителя…", opts.port);

    // Анонс в mDNS, чтобы отправитель нашёл нас без ввода IP.
    // Не критично: при неудаче просто работаем по прямому IP.
    let _mdns = match crate::discovery::advertise(opts.port) {
        Ok(d) => Some(d),
        Err(e) => {
            log::warn!("mDNS-анонс не удался ({e:#}) — поиск по сети работать не будет");
            None
        }
    };

    // Сессия живёт, пока не сменился формат и отправитель не пропал совсем;
    // всё остальное (перезапуск отправителя, смена его порта) чиним на лету.
    let mut pending: Option<(Header, SocketAddr)> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let (first, peer) = match pending.take() {
            Some(known) => known,
            None => {
                set_msg(&stats.status, "жду отправителя…");
                stats.linked.store(false, Ordering::Relaxed);
                match wait_for_sender(&sock, &stop)? {
                    Some(found) => found,
                    None => return Ok(()),
                }
            }
        };
        match session(&sock, &opts, &stats, &stop, first, peer)? {
            Next::Stop => return Ok(()),
            Next::Restart(next) => pending = next,
        }
    }
}

/// Ждёт первый валидный пакет: из него узнаём формат и адрес отправителя.
/// `None` — сработал stop.
fn wait_for_sender(
    sock: &UdpSocket,
    stop: &AtomicBool,
) -> Result<Option<(Header, SocketAddr)>> {
    let mut buf = vec![0u8; 65536];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Some(h) = Header::parse(&buf[..n])
                    && h.ptype != PacketType::Ack
                {
                    return Ok(Some((h, from)));
                }
            }
            Err(e) if recoverable(&e) => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

/// Ошибки чтения, после которых сокет остаётся рабочим.
///
/// `ConnectionReset` на UDP — это особенность Windows: если наш ACK ушёл на
/// порт, где отправителя уже нет, приходит ICMP «порт недоступен», и ближайший
/// `recv_from` падает с WSAECONNRESET. Ронять из-за этого приём нельзя —
/// отправитель как раз перезапускается и сейчас появится снова.
fn recoverable(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionRefused
    )
}

/// Один аудиопоток: держим вывод открытым, пока формат не поменялся.
fn session(
    sock: &UdpSocket,
    opts: &RecvOpts,
    stats: &ReceiverStats,
    stop: &AtomicBool,
    first: Header,
    initial_peer: SocketAddr,
) -> Result<Next> {
    let mut peer = initial_peer;
    log::info!(
        "отправитель {}: {} Гц, {} канала(ов), {:?}",
        peer,
        first.sample_rate,
        first.channels,
        first.format
    );
    set_msg(
        &stats.status,
        format!(
            "{} » {} Гц, {} кан",
            peer, first.sample_rate, first.channels
        ),
    );

    let sample_rate = first.sample_rate;
    let channels = first.channels as usize;
    let target_frames = (sample_rate as u64 * opts.buffer_ms as u64 / 1000) as usize;
    let cut_frames =
        target_frames + (sample_rate as u64 * CUT_OVER_TARGET_MS as u64 / 1000) as usize;
    let max_gap_frames = sample_rate as u64 * MAX_GAP_MS / 1000;

    // Кольцевой буфер на 1 с — запас над порогом среза.
    let rb = HeapRb::<f32>::new(sample_rate as usize * channels);
    let (mut prod, mut cons) = rb.split();

    let shared = Arc::new(Shared {
        prebuffering: AtomicBool::new(true),
        underruns: AtomicU64::new(0),
        cuts: AtomicU64::new(0),
        frames_played: AtomicU64::new(0),
    });

    // --- Аудио-выход ---
    let host = cpal::default_host();
    let device = match opts.device.as_deref() {
        Some(name) => host
            .output_devices()?
            .find(|d| {
                d.description()
                    .map(|desc| desc.name().contains(name))
                    .unwrap_or(false)
            })
            .with_context(|| format!("устройство вывода, содержащее '{name}', не найдено"))?,
        None => host
            .default_output_device()
            .context("нет дефолтного устройства вывода")?,
    };
    log::info!(
        "вывод: {}",
        device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "<unknown>".into())
    );

    let config = pick_output_config(&device, sample_rate, channels as u16)?;
    let stream_config: cpal::StreamConfig = config.into();

    let cb_shared = shared.clone();
    let mut scratch = vec![0f32; 4096];
    let out_stream = device.build_output_stream(
        stream_config,
        move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let occupied_frames = cons.occupied_len() / channels;
            if cb_shared.prebuffering.load(Ordering::Relaxed) {
                if occupied_frames >= target_frames {
                    cb_shared.prebuffering.store(false, Ordering::Relaxed);
                } else {
                    out.fill(0.0);
                    return;
                }
            }
            // Срез переполнения: после Wi-Fi-затыка пакеты приходят пачкой,
            // буфер разбухает — сбрасываем задержку одним куском до цели.
            if occupied_frames > cut_frames {
                let mut excess = (occupied_frames - target_frames) * channels;
                while excess > 0 {
                    let take = excess.min(scratch.len());
                    let n = cons.pop_slice(&mut scratch[..take]);
                    if n == 0 {
                        break;
                    }
                    excess -= n;
                }
                cb_shared.cuts.fetch_add(1, Ordering::Relaxed);
            }
            let n = cons.pop_slice(out);
            if n < out.len() {
                out[n..].fill(0.0);
                cb_shared.underruns.fetch_add(1, Ordering::Relaxed);
                cb_shared.prebuffering.store(true, Ordering::Relaxed);
            }
            cb_shared
                .frames_played
                .fetch_add((n / channels) as u64, Ordering::Relaxed);
        },
        |e| log::error!("ошибка потока вывода: {e}"),
        None,
    )?;
    out_stream.play()?;

    // --- Сетевой цикл ---
    let mut buf = vec![0u8; 65536];
    let mut tracker = SeqTracker::new();
    let mut drift = DriftComp::new(target_frames as u32, sample_rate);
    let mut decoded: Vec<f32> = Vec::with_capacity(4096);
    let silence = vec![0f32; 4800 * channels];
    let mut ticker = EverySecond::new();
    let mut pkts = 0u64;
    let mut bytes = 0u64;
    let mut ring_overflow = 0u64;
    let mut last_from_peer = Instant::now();
    let mut last_audio = Instant::now();
    let mut ack_timer = Instant::now() - ACK_PERIOD;
    let mut ack_buf = [0u8; HEADER_LEN];
    stats.linked.store(true, Ordering::Relaxed);

    while !stop.load(Ordering::Relaxed) {
        // Подтверждение отправителю: «поток доходит». Он показывает это
        // в GUI и отдаёт в /status, чтобы Home Assistant видел правду.
        if ack_timer.elapsed() >= ACK_PERIOD {
            ack_timer = Instant::now();
            Header {
                ptype: PacketType::Ack,
                channels: channels as u8,
                format: first.format,
                sample_rate,
                seq: 0,
                sample_pos: shared.frames_played.load(Ordering::Relaxed),
            }
            .write(&mut ack_buf);
            let _ = sock.send_to(&ack_buf, peer);
        }

        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                let Some(h) = Header::parse(&buf[..n]) else {
                    continue;
                };
                if h.ptype == PacketType::Ack {
                    continue; // наше же эхо/чужой приёмник — не наше дело
                }
                if from != peer {
                    // Отправитель перезапустился: у него эфемерный исходящий
                    // порт, после каждого старта новый. Раньше мы намертво
                    // держались за старый адрес и молча выбрасывали весь
                    // поток — приходилось перезапускать приёмник руками.
                    let takeover = h.ptype == PacketType::Hello
                        || last_from_peer.elapsed() >= PEER_SILENCE;
                    if !takeover {
                        continue; // текущий отправитель жив — чужих игнорируем
                    }
                    if h.sample_rate != sample_rate || h.channels as usize != channels {
                        log::info!("новый отправитель {from} с другим форматом — пересобираю вывод");
                        return Ok(Next::Restart(Some((h, from))));
                    }
                    log::info!("отправитель сменился: {peer} » {from}");
                    peer = from;
                    tracker.reset();
                    drift = DriftComp::new(target_frames as u32, sample_rate);
                    shared.prebuffering.store(true, Ordering::Relaxed);
                    ack_timer = Instant::now() - ACK_PERIOD;
                    set_msg(
                        &stats.status,
                        format!("{peer} » {sample_rate} Гц, {channels} кан"),
                    );
                }
                last_from_peer = Instant::now();
                if h.sample_rate != sample_rate || h.channels as usize != channels {
                    // Тот же отправитель сменил формат (другое устройство
                    // захвата) — раньше это требовало ручного перезапуска.
                    log::info!(
                        "формат сменился: {} Гц, {} кан — пересобираю вывод",
                        h.sample_rate,
                        h.channels
                    );
                    return Ok(Next::Restart(Some((h, from))));
                }
                match h.ptype {
                    PacketType::Ack => {}
                    PacketType::Hello => {}
                    PacketType::Bye => {
                        log::info!("отправитель завершил передачу (BYE)");
                        shared.prebuffering.store(true, Ordering::Relaxed);
                        tracker.reset();
                    }
                    PacketType::Audio => {
                        last_audio = Instant::now();
                        pkts += 1;
                        bytes += n as u64;
                        stats.pkts.fetch_add(1, Ordering::Relaxed);
                        stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
                        let payload = &buf[HEADER_LEN..n];
                        let frames_in =
                            payload.len() / h.format.bytes_per_sample() / channels;
                        if frames_in == 0 {
                            continue;
                        }
                        match tracker.on_audio(h.seq, frames_in as u32) {
                            Verdict::Drop => continue,
                            Verdict::Accept { gap_frames } => {
                                // Потерянные пакеты замещаем тишиной — тайминг сохраняется.
                                let mut gap =
                                    gap_frames.min(max_gap_frames) as usize * channels;
                                while gap > 0 {
                                    let pushed = prod
                                        .push_slice(&silence[..gap.min(silence.len())]);
                                    if pushed == 0 {
                                        break;
                                    }
                                    gap -= pushed;
                                }
                            }
                        }

                        decoded.clear();
                        match h.format {
                            WireFormat::S16le => convert::s16le_to_f32(payload, &mut decoded),
                            WireFormat::F32le => convert::f32le_to_f32(payload, &mut decoded),
                        }

                        // Громкость (live) + защита от клиппинга; заодно peak.
                        let vol = crate::stats::load_f32(&opts.volume);
                        let mut peak = 0f32;
                        if (vol - 1.0).abs() > 1e-3 {
                            for s in decoded.iter_mut() {
                                *s = (*s * vol).clamp(-1.0, 1.0);
                                peak = peak.max(s.abs());
                            }
                        } else {
                            for s in decoded.iter() {
                                peak = peak.max(s.abs());
                            }
                        }
                        crate::stats::store_f32(&stats.level, peak);

                        // Компенсация дрейфа: изредка минус/плюс один фрейм.
                        let fill_frames = prod.occupied_len() / channels;
                        match drift.on_packet(fill_frames, frames_in) {
                            Slip::DropFrame => {
                                decoded.drain(..channels);
                            }
                            Slip::DupFrame => {
                                let frame: Vec<f32> = decoded[..channels].to_vec();
                                decoded.splice(0..0, frame);
                            }
                            Slip::None => {}
                        }

                        let pushed = prod.push_slice(&decoded);
                        if pushed < decoded.len() {
                            ring_overflow += (decoded.len() - pushed) as u64;
                        }
                    }
                }
            }
            Err(e) if recoverable(&e) => {}
            Err(e) => return Err(e.into()),
        }

        // Отправителя нет совсем — отпускаем устройство вывода и ждём заново.
        if last_audio.elapsed() >= IDLE_RESTART {
            log::info!(
                "отправитель молчит {} с — освобождаю вывод и жду заново",
                IDLE_RESTART.as_secs()
            );
            stats.linked.store(false, Ordering::Relaxed);
            crate::stats::store_f32(&stats.level, 0.0);
            stats.fill_ms.store(0, Ordering::Relaxed);
            return Ok(Next::Restart(None));
        }

        if ticker.tick() {
            let fill_ms =
                prod.occupied_len() / channels * 1000 / sample_rate as usize;
            stats.fill_ms.store(fill_ms as u64, Ordering::Relaxed);
            stats.lost.store(tracker.lost_packets, Ordering::Relaxed);
            stats.late.store(tracker.late_packets, Ordering::Relaxed);
            stats
                .underruns
                .store(shared.underruns.load(Ordering::Relaxed), Ordering::Relaxed);
            stats
                .cuts
                .store(shared.cuts.load(Ordering::Relaxed), Ordering::Relaxed);
            stats
                .slip_dropped
                .store(drift.dropped_frames, Ordering::Relaxed);
            stats
                .slip_duplicated
                .store(drift.duplicated_frames, Ordering::Relaxed);
            stats.ring_overflow.store(ring_overflow, Ordering::Relaxed);
            stats.linked.store(pkts > 0, Ordering::Relaxed);
            log::info!(
                "rx: {} пак/с, {:.1} кбит/с | буфер {} мс (EMA {:.1} мс) | потеряно {}, поздних {}, underruns {}, срезов {}, slip -{}/+{}, ring overflow {}",
                pkts,
                bytes as f64 * 8.0 / 1000.0,
                fill_ms,
                drift.ema_fill_frames() * 1000.0 / sample_rate as f64,
                tracker.lost_packets,
                tracker.late_packets,
                shared.underruns.load(Ordering::Relaxed),
                shared.cuts.load(Ordering::Relaxed),
                drift.dropped_frames,
                drift.duplicated_frames,
                ring_overflow,
            );
            pkts = 0;
            bytes = 0;
        }
    }
    drop(out_stream);
    log::info!(
        "остановлено; всего воспроизведено {} с",
        shared.frames_played.load(Ordering::Relaxed) / sample_rate as u64
    );
    Ok(Next::Stop)
}

fn pick_output_config(
    device: &cpal::Device,
    sample_rate: u32,
    channels: u16,
) -> Result<cpal::SupportedStreamConfig> {
    let mut supported: Vec<_> = device.supported_output_configs()?.collect();
    supported.retain(|c| {
        c.channels() == channels
            && c.sample_format() == cpal::SampleFormat::F32
            && c.min_sample_rate() <= sample_rate
            && c.max_sample_rate() >= sample_rate
    });
    if let Some(c) = supported.into_iter().next() {
        return Ok(c.with_sample_rate(sample_rate));
    }
    // Fallback: дефолтная конфигурация устройства, если параметры совпадают.
    let def = device.default_output_config()?;
    if def.sample_rate() == sample_rate
        && def.channels() == channels
        && def.sample_format() == cpal::SampleFormat::F32
    {
        return Ok(def);
    }
    bail!(
        "устройство вывода не поддерживает {} Гц / {} кан / f32 (дефолт: {} Гц, {} кан, {:?})",
        sample_rate,
        channels,
        def.sample_rate(),
        def.channels(),
        def.sample_format()
    )
}
