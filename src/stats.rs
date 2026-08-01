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

/// Накопительные счётчики отправителя (GUI считает скорости по дельтам).
#[derive(Default)]
pub struct SenderStats {
    pub pkts: AtomicU64,
    pub bytes: AtomicU64,
    pub ring_overflow: AtomicU64,
    /// Строка состояния («48000 Гц, 2 кан → 192.168.1.44:48100»).
    pub status: Mutex<Option<String>>,
    /// Ошибка, завершившая работу (для показа в GUI).
    pub error: Mutex<Option<String>>,
}

/// Накопительные счётчики приёмника.
#[derive(Default)]
pub struct ReceiverStats {
    pub pkts: AtomicU64,
    pub bytes: AtomicU64,
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
