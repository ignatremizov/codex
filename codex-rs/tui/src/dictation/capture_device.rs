//! CPAL callbacks only downmix, meter, and enqueue fixed-size blocks; no VAD or networking.

use super::BLOCK_SAMPLES;
use super::Block;
use super::Control;
use super::enqueue;
use cpal::FromSample;
use cpal::Sample;
use cpal::SizedSample;
use cpal::traits::DeviceTrait;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc as ingress;
use std::time::Duration;

pub(super) fn build_input<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    input: ingress::SyncSender<Block>,
    control: Arc<Control>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let failure = control.clone();
    let channels = usize::from(config.channels);
    let mut previous = None;
    let mut expected = Duration::ZERO;
    device.build_input_stream(
        config,
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            if !control.accepting() {
                return;
            }
            let captured = info.timestamp().capture;
            if let Some(last) = previous
                && captured.checked_duration_since(last).is_none_or(|elapsed| {
                    elapsed.abs_diff(expected) > Duration::from_millis(/*millis*/ 20)
                })
            {
                control.fail();
                return;
            }
            previous = Some(captured);
            expected = Duration::from_secs_f64(
                data.len() as f64 / channels as f64 / f64::from(config.sample_rate),
            );
            if !data.len().is_multiple_of(channels) {
                control.fail();
                return;
            }
            for samples in data.chunks(BLOCK_SAMPLES * channels) {
                let mut block = Block {
                    samples: [0; BLOCK_SAMPLES],
                    len: samples.len() / channels,
                };
                for (dest, frame) in block.samples.iter_mut().zip(samples.chunks_exact(channels)) {
                    let mono = frame
                        .iter()
                        .map(|sample| f32::from_sample(*sample))
                        .sum::<f32>()
                        / channels as f32;
                    if !mono.is_finite() {
                        control.fail();
                        return;
                    }
                    *dest = (mono.clamp(/*min*/ -1.0, /*max*/ 1.0) * f32::from(i16::MAX)) as i16;
                    control
                        .peak
                        .fetch_max(dest.unsigned_abs(), Ordering::Relaxed);
                }
                if !enqueue(&input, block, &control) {
                    return;
                }
            }
        },
        move |_| failure.fail(),
        /*timeout*/ None,
    )
}
