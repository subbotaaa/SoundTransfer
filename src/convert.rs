//! Конвертация сэмплов между f32 (внутренний формат) и wire-форматами.

/// f32 [-1;1] -> s16le. `dst.len()` должен быть `src.len() * 2`.
pub fn f32_to_s16le(src: &[f32], dst: &mut [u8]) {
    debug_assert_eq!(dst.len(), src.len() * 2);
    for (s, out) in src.iter().zip(dst.chunks_exact_mut(2)) {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        out.copy_from_slice(&v.to_le_bytes());
    }
}

/// f32 -> f32le байты. `dst.len()` должен быть `src.len() * 4`.
pub fn f32_to_f32le(src: &[f32], dst: &mut [u8]) {
    debug_assert_eq!(dst.len(), src.len() * 4);
    for (s, out) in src.iter().zip(dst.chunks_exact_mut(4)) {
        out.copy_from_slice(&s.to_le_bytes());
    }
}

/// s16le байты -> f32, дописывает в `dst`.
pub fn s16le_to_f32(src: &[u8], dst: &mut Vec<f32>) {
    dst.extend(src.chunks_exact(2).map(|b| {
        i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0
    }));
}

/// f32le байты -> f32, дописывает в `dst`.
pub fn f32le_to_f32(src: &[u8], dst: &mut Vec<f32>) {
    dst.extend(src.chunks_exact(4).map(|b| {
        f32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s16_round_trip_is_close() {
        let src = [0.0f32, 0.5, -0.5, 0.999, -0.999];
        let mut bytes = vec![0u8; src.len() * 2];
        f32_to_s16le(&src, &mut bytes);
        let mut back = Vec::new();
        s16le_to_f32(&bytes, &mut back);
        for (a, b) in src.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1.0 / 16000.0, "{a} vs {b}");
        }
    }

    #[test]
    fn s16_clamps_out_of_range() {
        let src = [2.0f32, -2.0];
        let mut bytes = vec![0u8; 4];
        f32_to_s16le(&src, &mut bytes);
        let mut back = Vec::new();
        s16le_to_f32(&bytes, &mut back);
        assert!(back[0] > 0.99 && back[1] <= -0.99);
    }

    #[test]
    fn f32_round_trip_is_exact() {
        let src = [0.0f32, 0.123456, -0.98765, 1.5];
        let mut bytes = vec![0u8; src.len() * 4];
        f32_to_f32le(&src, &mut bytes);
        let mut back = Vec::new();
        f32le_to_f32(&bytes, &mut back);
        assert_eq!(&src[..], &back[..]);
    }
}
