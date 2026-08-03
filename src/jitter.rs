//! Логика джиттер-буфера: отслеживание seq и компенсация дрейфа часов.
//! Чистые структуры без I/O — полностью покрываются юнит-тестами.

/// Если разрыв больше этого числа пакетов, считаем что отправитель
/// перезапустился, и просто пересинхронизируемся без вставки тишины.
const RESTART_GAP_PACKETS: i32 = 2000;

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Принять пакет; перед ним вставить `gap_frames` фреймов тишины
    /// (потерянные пакеты — сохраняем тайминг потока).
    Accept { gap_frames: u64 },
    /// Поздний или дублирующийся пакет — отбросить.
    Drop,
}

#[derive(Debug, Default)]
pub struct SeqTracker {
    next: Option<u32>,
    pub lost_packets: u64,
    pub late_packets: u64,
}

impl SeqTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.next = None;
    }

    pub fn on_audio(&mut self, seq: u32, frames_per_packet: u32) -> Verdict {
        let Some(next) = self.next else {
            self.next = Some(seq.wrapping_add(1));
            return Verdict::Accept { gap_frames: 0 };
        };
        let diff = seq.wrapping_sub(next) as i32;
        if diff == 0 {
            self.next = Some(seq.wrapping_add(1));
            Verdict::Accept { gap_frames: 0 }
        } else if diff > RESTART_GAP_PACKETS {
            self.next = Some(seq.wrapping_add(1));
            Verdict::Accept { gap_frames: 0 }
        } else if diff > 0 {
            self.lost_packets += diff as u64;
            self.next = Some(seq.wrapping_add(1));
            Verdict::Accept {
                gap_frames: diff as u64 * frames_per_packet as u64,
            }
        } else {
            self.late_packets += 1;
            Verdict::Drop
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slip {
    None,
    /// Буфер стабильно выше цели — выбросить один фрейм из входа.
    DropFrame,
    /// Буфер стабильно ниже цели — продублировать один фрейм входа.
    DupFrame,
}

/// Компенсация дрейфа часов отправителя и приёмника: EMA заполненности буфера;
/// при устойчивом отклонении от цели за пределы мёртвой зоны —
/// drop/dup одного фрейма на каждые ~10 мс входа (0.4 % slip, неслышимо).
#[derive(Debug)]
pub struct DriftComp {
    target_frames: f64,
    deadband_frames: f64,
    ema: f64,
    /// Окно EMA в фреймах входа (~2 c потока).
    ema_window_frames: f64,
    /// Сколько slip-фреймов на один входной фрейм в активном режиме.
    slip_per_frame: f64,
    accum: f64,
    pub dropped_frames: u64,
    pub duplicated_frames: u64,
}

impl DriftComp {
    pub fn new(target_frames: u32, sample_rate: u32) -> Self {
        Self {
            target_frames: target_frames as f64,
            // Мёртвая зона ±4 мс.
            deadband_frames: sample_rate as f64 * 0.004,
            ema: target_frames as f64,
            ema_window_frames: sample_rate as f64 * 2.0,
            // 1 фрейм слипа на 10 мс входа.
            slip_per_frame: 1.0 / (sample_rate as f64 * 0.010),
            accum: 0.0,
            dropped_frames: 0,
            duplicated_frames: 0,
        }
    }

    pub fn ema_fill_frames(&self) -> f64 {
        self.ema
    }

    /// Вызывается на каждый входящий пакет. `fill_frames` — текущая
    /// заполненность кольцевого буфера, `frames_in` — фреймов в пакете.
    pub fn on_packet(&mut self, fill_frames: usize, frames_in: usize) -> Slip {
        let alpha = (frames_in as f64 / self.ema_window_frames).min(1.0);
        self.ema += alpha * (fill_frames as f64 - self.ema);
        let dev = self.ema - self.target_frames;
        if dev.abs() <= self.deadband_frames {
            self.accum = 0.0;
            return Slip::None;
        }
        self.accum += frames_in as f64 * self.slip_per_frame;
        if self.accum < 1.0 {
            return Slip::None;
        }
        self.accum -= 1.0;
        if dev > 0.0 {
            self.dropped_frames += 1;
            Slip::DropFrame
        } else {
            self.duplicated_frames += 1;
            Slip::DupFrame
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_in_order() {
        let mut t = SeqTracker::new();
        assert_eq!(t.on_audio(10, 128), Verdict::Accept { gap_frames: 0 });
        assert_eq!(t.on_audio(11, 128), Verdict::Accept { gap_frames: 0 });
        assert_eq!(t.lost_packets, 0);
    }

    #[test]
    fn seq_gap_inserts_silence() {
        let mut t = SeqTracker::new();
        t.on_audio(1, 128);
        assert_eq!(t.on_audio(4, 128), Verdict::Accept { gap_frames: 256 });
        assert_eq!(t.lost_packets, 2);
        assert_eq!(t.on_audio(5, 128), Verdict::Accept { gap_frames: 0 });
    }

    #[test]
    fn seq_late_and_dup_dropped() {
        let mut t = SeqTracker::new();
        t.on_audio(10, 128);
        t.on_audio(11, 128);
        assert_eq!(t.on_audio(11, 128), Verdict::Drop);
        assert_eq!(t.on_audio(5, 128), Verdict::Drop);
        assert_eq!(t.late_packets, 2);
    }

    #[test]
    fn seq_wraps_around() {
        let mut t = SeqTracker::new();
        t.on_audio(u32::MAX, 128);
        assert_eq!(t.on_audio(0, 128), Verdict::Accept { gap_frames: 0 });
        assert_eq!(t.on_audio(1, 128), Verdict::Accept { gap_frames: 0 });
    }

    #[test]
    fn seq_huge_gap_treated_as_restart() {
        let mut t = SeqTracker::new();
        t.on_audio(1, 128);
        assert_eq!(t.on_audio(100_000, 128), Verdict::Accept { gap_frames: 0 });
        assert_eq!(t.lost_packets, 0);
    }

    /// Симуляция: приёмник потребляет чуть медленнее, чем шлёт отправитель
    /// (дрейф +100 ppm) — буфер растёт, компенсатор должен дропать фреймы.
    #[test]
    fn drift_comp_drops_when_buffer_grows() {
        let sr = 48000u32;
        let target = 960u32; // 20 ms
        let mut dc = DriftComp::new(target, sr);
        let mut fill = target as f64;
        let mut slips = 0u64;
        // 60 секунд по 128 фреймов; дрейф: буфер растёт на 0.0128 фрейма/пакет
        for _ in 0..(60 * 375) {
            fill += 128.0 * 100e-6; // +100 ppm
            match dc.on_packet(fill as usize, 128) {
                Slip::DropFrame => {
                    fill -= 1.0;
                    slips += 1;
                }
                Slip::DupFrame => fill += 1.0,
                Slip::None => {}
            }
        }
        // За 60 с при 100 ppm набегает ~288 фреймов дрейфа; компенсатор
        // должен удерживать буфер около цели (в пределах мёртвой зоны + запас).
        assert!(slips > 0, "компенсатор не сработал");
        assert!(
            (fill - target as f64).abs() < 400.0,
            "буфер уплыл: fill={fill}"
        );
    }

    #[test]
    fn drift_comp_duplicates_when_buffer_shrinks() {
        let sr = 48000u32;
        let target = 960u32;
        let mut dc = DriftComp::new(target, sr);
        let mut fill = target as f64;
        for _ in 0..(60 * 375) {
            fill -= 128.0 * 100e-6;
            match dc.on_packet(fill.max(0.0) as usize, 128) {
                Slip::DupFrame => fill += 1.0,
                Slip::DropFrame => fill -= 1.0,
                Slip::None => {}
            }
        }
        assert!(dc.duplicated_frames > 0);
        assert!((fill - target as f64).abs() < 400.0);
    }

    #[test]
    fn drift_comp_idle_inside_deadband() {
        let mut dc = DriftComp::new(960, 48000);
        for _ in 0..10_000 {
            assert_eq!(dc.on_packet(960, 128), Slip::None);
        }
        assert_eq!(dc.dropped_frames + dc.duplicated_frames, 0);
    }
}
