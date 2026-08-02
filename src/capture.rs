//! Захват системного звука Windows через WASAPI loopback (cpal).
//!
//! cpal включает loopback автоматически, если открыть output-устройство
//! через `build_input_stream`. Если этот путь окажется нерабочим —
//! запасной вариант на крейте `wasapi` живёт за feature `wasapi-capture`.
#![cfg(windows)]

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub struct CaptureFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Запускает loopback-захват дефолтного (или указанного) устройства вывода.
/// `on_chunk` вызывается из аудио-потока WASAPI с interleaved f32-сэмплами.
/// Возвращённый Stream нужно держать живым, пока идёт захват.
pub fn start_loopback(
    device_name: Option<&str>,
    mut on_chunk: impl FnMut(&[f32]) + Send + 'static,
) -> Result<(cpal::Stream, CaptureFormat)> {
    let host = cpal::default_host();
    let device = match device_name {
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
        "захват (loopback): {}",
        device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "<unknown>".into())
    );

    let config = device
        .default_output_config()
        .context("не удалось получить формат устройства вывода")?;
    let fmt = CaptureFormat {
        sample_rate: config.sample_rate(),
        channels: config.channels(),
    };
    let stream_config: cpal::StreamConfig = config.config();
    let err_cb = |e| log::error!("ошибка потока захвата: {e}");

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            stream_config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| on_chunk(data),
            err_cb,
            None,
        )?,
        cpal::SampleFormat::I16 => {
            let mut buf: Vec<f32> = Vec::new();
            device.build_input_stream(
                stream_config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    buf.clear();
                    buf.extend(data.iter().map(|&s| s as f32 / 32768.0));
                    on_chunk(&buf);
                },
                err_cb,
                None,
            )?
        }
        other => bail!("неподдерживаемый формат захвата: {other:?}"),
    };
    stream.play().context("не удалось запустить поток захвата")?;
    Ok((stream, fmt))
}
