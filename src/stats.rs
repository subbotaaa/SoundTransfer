//! Статистика, разделяемая между рабочими потоками, CLI-логом и GUI.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub struct EverySecond {
    last: Instant,
    period: Duration,
}

impl EverySecond {
    pub fn new() -> Self {
        Self {
            last: Instant::now(),
            period: Duration::from_secs(1),
        }
    }

    /// true — пора печатать очередную строку статистики.
    pub fn tick(&mut self) -> bool {
        if self.last.elapsed() >= self.period {
            self.last = Instant::now();
            true
        } else {
            false
        }
    }
}

/// Атомарное хранение f32 (для уровней/громкости между потоками).
pub fn store_f32(slot: &std::sync::atomic::AtomicU32, v: f32) {
    slot.store(v.to_bits(), Ordering::Relaxed);
}

pub fn load_f32(slot: &std::sync::atomic::AtomicU32) -> f32 {
    f32::from_bits(slot.load(Ordering::Relaxed))
}

/// Накопительные счётчики отправителя (GUI считает скорости по дельтам).
#[derive(Default)]
pub struct SenderStats {
    pub pkts: AtomicU64,
    pub bytes: AtomicU64,
    pub ring_overflow: AtomicU64,
    /// Пиковый уровень последнего пакета, 0..1 (f32 в битах).
    pub level: std::sync::atomic::AtomicU32,
    /// Строка состояния («48000 Гц, 2 кан » IP:порт»).
    pub status: Mutex<Option<String>>,
    /// Ошибка, завершившая работу (для показа в GUI).
    pub error: Mutex<Option<String>>,
}

/// Накопительные счётчики приёмника.
#[derive(Default)]
pub struct ReceiverStats {
    pub pkts: AtomicU64,
    pub bytes: AtomicU64,
    /// Пиковый уровень последнего пакета (после громкости), 0..1.
    pub level: std::sync::atomic::AtomicU32,
    /// Текущая заполненность буфера, мс.
    pub fill_ms: AtomicU64,
    pub lost: AtomicU64,
    pub late: AtomicU64,
    pub underruns: AtomicU64,
    pub cuts: AtomicU64,
    pub slip_dropped: AtomicU64,
    pub slip_duplicated: AtomicU64,
    pub ring_overflow: AtomicU64,
    pub status: Mutex<Option<String>>,
    pub error: Mutex<Option<String>>,
}

pub fn set_msg(slot: &Mutex<Option<String>>, msg: impl Into<String>) {
    if let Ok(mut g) = slot.lock() {
        *g = Some(msg.into());
    }
}

pub fn get_msg(slot: &Mutex<Option<String>>) -> Option<String> {
    slot.lock().ok().and_then(|g| g.clone())
}

/// Помощник GUI: считает скорость (в единицах/с) по накопительному счётчику.
pub struct RateMeter {
    last: Option<(Instant, u64)>,
    pub per_sec: f64,
}

impl RateMeter {
    pub fn new() -> Self {
        Self {
            last: None,
            per_sec: 0.0,
        }
    }

    pub fn update(&mut self, counter: &AtomicU64) {
        let now = Instant::now();
        let val = counter.load(Ordering::Relaxed);
        if let Some((t, prev)) = self.last {
            let dt = now.duration_since(t).as_secs_f64();
            if dt >= 1.0 {
                self.per_sec = (val.saturating_sub(prev)) as f64 / dt;
                self.last = Some((now, val));
            }
        } else {
            self.last = Some((now, val));
        }
    }

    pub fn reset(&mut self) {
        self.last = None;
        self.per_sec = 0.0;
    }
}
