use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use libpulse_binding::{
    context::{Context as PulseContext, FlagSet as ContextFlagSet, State as ContextState},
    def::BufferAttr,
    mainloop::standard::{IterateResult, Mainloop},
    sample::{Format, Spec},
    stream::{FlagSet as StreamFlagSet, PeekResult, State as StreamState, Stream},
};

pub(crate) const AUDIO_SPECTRUM_BINS: usize = 64;

const AUDIO_FFT_SIZE: usize = 4096;
const AUDIO_HALF_FFT: usize = AUDIO_FFT_SIZE / 2;
const AUDIO_MIN_FREQUENCY_HZ: f32 = 20.0;
const AUDIO_MAX_FREQUENCY_HZ: f32 = 20_000.0;
const AUDIO_TILT_PIVOT_HZ: f32 = 200.0;
const AUDIO_TILT_EXPONENT: f32 = 0.30;
const AUDIO_DB_FLOOR: f32 = -80.0;
const AUDIO_DB_CEILING: f32 = -8.0;
const AUDIO_RESPONSE_POWER: f32 = 1.6;
const AUDIO_ATTACK_SECONDS: f32 = 0.030;
const AUDIO_RELEASE_SECONDS: f32 = 0.140;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StereoSpectrum {
    pub(crate) left: [f32; AUDIO_SPECTRUM_BINS],
    pub(crate) right: [f32; AUDIO_SPECTRUM_BINS],
}

impl Default for StereoSpectrum {
    fn default() -> Self {
        Self { left: [0.0; AUDIO_SPECTRUM_BINS], right: [0.0; AUDIO_SPECTRUM_BINS] }
    }
}

impl StereoSpectrum {
    pub(crate) fn flattened(&self) -> Arc<[f32]> {
        let mut values = Vec::with_capacity(AUDIO_SPECTRUM_BINS * 2);
        values.extend_from_slice(&self.left);
        values.extend_from_slice(&self.right);
        values.into()
    }
}

#[derive(Clone, Copy, Default)]
struct ComplexSample {
    real: f32,
    imaginary: f32,
}

struct SpectrumBandLayout {
    edges: [usize; AUDIO_SPECTRUM_BINS + 1],
    gain: [f32; AUDIO_SPECTRUM_BINS],
}

fn hann_window(index: usize, count: usize) -> f32 {
    if count < 2 {
        return 0.0;
    }
    0.5 * (1.0 - (std::f32::consts::TAU * index as f32 / (count - 1) as f32).cos())
}

fn fft_in_place(values: &mut [ComplexSample]) {
    debug_assert!(values.len().is_power_of_two());

    let mut reversed = 0;
    for index in 1..values.len() {
        let mut bit = values.len() >> 1;
        while reversed & bit != 0 {
            reversed ^= bit;
            bit >>= 1;
        }
        reversed ^= bit;
        if index < reversed {
            values.swap(index, reversed);
        }
    }

    let mut length = 2;
    while length <= values.len() {
        let angle = -std::f32::consts::TAU / length as f32;
        let step = ComplexSample { real: angle.cos(), imaginary: angle.sin() };
        for begin in (0..values.len()).step_by(length) {
            let half = length >> 1;
            let mut weight = ComplexSample { real: 1.0, imaginary: 0.0 };
            for offset in 0..half {
                let even = values[begin + offset];
                let source = values[begin + offset + half];
                let odd = ComplexSample {
                    real: source.real * weight.real - source.imaginary * weight.imaginary,
                    imaginary: source.real * weight.imaginary + source.imaginary * weight.real,
                };
                values[begin + offset] = ComplexSample {
                    real: even.real + odd.real,
                    imaginary: even.imaginary + odd.imaginary,
                };
                values[begin + offset + half] = ComplexSample {
                    real: even.real - odd.real,
                    imaginary: even.imaginary - odd.imaginary,
                };
                weight = ComplexSample {
                    real: weight.real * step.real - weight.imaginary * step.imaginary,
                    imaginary: weight.real * step.imaginary + weight.imaginary * step.real,
                };
            }
        }
        length <<= 1;
    }
}

fn hertz_to_mel(hertz: f32) -> f32 {
    2595.0 * (1.0 + hertz / 700.0).log10()
}

fn mel_to_hertz(mel: f32) -> f32 {
    700.0 * (10.0_f32.powf(mel / 2595.0) - 1.0)
}

fn hertz_to_upper_fft_bin(hertz: f32, sample_rate: u32) -> usize {
    if sample_rate == 0 {
        return 1;
    }
    ((hertz * AUDIO_FFT_SIZE as f32 / sample_rate as f32).ceil() as usize).clamp(1, AUDIO_HALF_FFT)
}

fn spectrum_band_layout(sample_rate: u32) -> SpectrumBandLayout {
    let mut layout = SpectrumBandLayout {
        edges: [0; AUDIO_SPECTRUM_BINS + 1],
        gain: [0.0; AUDIO_SPECTRUM_BINS],
    };
    if sample_rate == 0 {
        return layout;
    }

    let nyquist = sample_rate as f32 * 0.5;
    let max_hertz = AUDIO_MAX_FREQUENCY_HZ.min(nyquist);
    let min_hertz = AUDIO_MIN_FREQUENCY_HZ.min(max_hertz);
    let min_mel = hertz_to_mel(min_hertz);
    let max_mel = hertz_to_mel(max_hertz);
    let max_bin = hertz_to_upper_fft_bin(max_hertz, sample_rate);

    layout.edges[0] = hertz_to_upper_fft_bin(min_hertz, sample_rate);
    for band in 1..AUDIO_SPECTRUM_BINS {
        let progress = band as f32 / AUDIO_SPECTRUM_BINS as f32;
        let hertz = mel_to_hertz(min_mel + (max_mel - min_mel) * progress);
        let remaining = AUDIO_SPECTRUM_BINS - band;
        let largest_next = max_bin.saturating_sub(remaining);
        layout.edges[band] = hertz_to_upper_fft_bin(hertz, sample_rate)
            .max(layout.edges[band - 1] + 1)
            .min(largest_next);
    }
    layout.edges[AUDIO_SPECTRUM_BINS] = max_bin;

    for band in 0..AUDIO_SPECTRUM_BINS {
        let center_bin = (layout.edges[band] + layout.edges[band + 1]) as f32 * 0.5;
        let center_hertz = center_bin * sample_rate as f32 / AUDIO_FFT_SIZE as f32;
        layout.gain[band] = (center_hertz / AUDIO_TILT_PIVOT_HZ).powf(AUDIO_TILT_EXPONENT);
    }
    layout
}

fn complex_magnitude(value: ComplexSample) -> f32 {
    (value.real * value.real + value.imaginary * value.imaginary).sqrt()
}

fn shape_audio_response(unit: f32) -> f32 {
    let value = unit.clamp(0.0, 1.0);
    if value <= 0.5 {
        0.5 * (value * 2.0).powf(AUDIO_RESPONSE_POWER)
    } else {
        1.0 - 0.5 * ((1.0 - value) * 2.0).powf(AUDIO_RESPONSE_POWER)
    }
}

fn visual_audio_response(magnitude: f32, gain: f32) -> f32 {
    let compensated = (magnitude * gain).max(1.0e-12);
    let decibels = 20.0 * compensated.log10();
    let unit = ((decibels - AUDIO_DB_FLOOR) / (AUDIO_DB_CEILING - AUDIO_DB_FLOOR)).clamp(0.0, 1.0);
    shape_audio_response(unit).min(1.0)
}

pub(crate) fn spectrum_from_interleaved_pcm(samples: &[f32], sample_rate: u32) -> StereoSpectrum {
    if sample_rate == 0 || samples.len() < 4 {
        return StereoSpectrum::default();
    }

    let frame_count = (samples.len() / 2).min(AUDIO_FFT_SIZE);
    if frame_count < 2 {
        return StereoSpectrum::default();
    }
    let start_frame = samples.len() / 2 - frame_count;
    let mut left_fft = vec![ComplexSample::default(); AUDIO_FFT_SIZE];
    let mut right_fft = vec![ComplexSample::default(); AUDIO_FFT_SIZE];
    for index in 0..frame_count {
        let weight = hann_window(index, frame_count);
        let frame = start_frame + index;
        left_fft[index].real = samples[frame * 2] * weight;
        right_fft[index].real = samples[frame * 2 + 1] * weight;
    }
    fft_in_place(&mut left_fft);
    fft_in_place(&mut right_fft);

    let layout = spectrum_band_layout(sample_rate);
    let normalization = 2.0 / frame_count as f32;
    let mut spectrum = StereoSpectrum::default();
    for band in 0..AUDIO_SPECTRUM_BINS {
        let begin = layout.edges[band];
        let end = layout.edges[band + 1];
        let mut left_peak = 0.0_f32;
        let mut right_peak = 0.0_f32;
        for bin in begin..end {
            left_peak = left_peak.max(complex_magnitude(left_fft[bin]));
            right_peak = right_peak.max(complex_magnitude(right_fft[bin]));
        }
        spectrum.left[band] = visual_audio_response(left_peak * normalization, layout.gain[band]);
        spectrum.right[band] = visual_audio_response(right_peak * normalization, layout.gain[band]);
    }
    spectrum
}

fn smooth_spectrum(
    current: &StereoSpectrum,
    state: &mut StereoSpectrum,
    delta_seconds: f32,
) -> StereoSpectrum {
    for band in 0..AUDIO_SPECTRUM_BINS {
        let left_time = if current.left[band] > state.left[band] {
            AUDIO_ATTACK_SECONDS
        } else {
            AUDIO_RELEASE_SECONDS
        };
        let right_time = if current.right[band] > state.right[band] {
            AUDIO_ATTACK_SECONDS
        } else {
            AUDIO_RELEASE_SECONDS
        };
        let left_alpha = 1.0 - (-delta_seconds.max(0.0) / left_time).exp();
        let right_alpha = 1.0 - (-delta_seconds.max(0.0) / right_time).exp();
        state.left[band] += left_alpha * (current.left[band] - state.left[band]);
        state.right[band] += right_alpha * (current.right[band] - state.right[band]);
    }
    state.clone()
}

fn append_recent_pcm_bytes(retained: &mut Vec<f32>, bytes: &[u8], max_samples: usize) {
    if max_samples == 0 {
        retained.clear();
        return;
    }

    let chunks = bytes.chunks_exact(std::mem::size_of::<f32>());
    let incoming_samples = chunks.len();
    if incoming_samples >= max_samples {
        retained.clear();
        retained.extend(
            chunks
                .skip(incoming_samples - max_samples)
                .map(|bytes| f32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
        );
        return;
    }

    let overflow = retained.len().saturating_add(incoming_samples).saturating_sub(max_samples);
    if overflow > 0 {
        retained.drain(..overflow.min(retained.len()));
    }
    retained
        .extend(chunks.map(|bytes| f32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])));
}

fn append_recent_silence(retained: &mut Vec<f32>, incoming_samples: usize, max_samples: usize) {
    if max_samples == 0 {
        retained.clear();
        return;
    }
    if incoming_samples >= max_samples {
        retained.clear();
        retained.resize(max_samples, 0.0);
        return;
    }

    let overflow = retained.len().saturating_add(incoming_samples).saturating_sub(max_samples);
    if overflow > 0 {
        retained.drain(..overflow.min(retained.len()));
    }
    retained.resize(retained.len() + incoming_samples, 0.0);
}

pub(crate) struct PulseAudioCapture {
    // Keep the dependency objects in destruction order: stream -> context -> mainloop.
    stream: Stream,
    context: PulseContext,
    mainloop: Mainloop,
    sample_rate: u32,
    update_interval_seconds: f32,
    samples: Vec<f32>,
    smoothed_spectrum: StereoSpectrum,
}

impl PulseAudioCapture {
    pub(crate) fn connect(
        source: &str,
        sample_rate: u32,
        update_hz: u32,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Option<Self>> {
        if cancelled() {
            return Ok(None);
        }
        let sample_rate = sample_rate.clamp(8_000, 192_000);
        let update_hz = update_hz.clamp(5, 60);
        let spec = Spec { format: Format::FLOAT32NE, channels: 2, rate: sample_rate };
        if !spec.is_valid() {
            anyhow::bail!("invalid PulseAudio sample specification");
        }
        let fragment_frame_count = (sample_rate / update_hz).clamp(128, 4096) as usize;
        let fragment_bytes = fragment_frame_count
            .checked_mul(2)
            .and_then(|samples| samples.checked_mul(std::mem::size_of::<f32>()))
            .and_then(|bytes| u32::try_from(bytes).ok())
            .context("PulseAudio fragment size overflow")?;

        let mut mainloop = Mainloop::new().context("failed to create PulseAudio mainloop")?;
        let mut context = PulseContext::new(&mainloop, "we-layerd")
            .context("failed to create PulseAudio context")?;
        context
            .connect(None, ContextFlagSet::NOFLAGS, None)
            .context("failed to start PulseAudio context connection")?;
        if !drive_pulse_until(&mut mainloop, Duration::from_secs(2), &mut cancelled, || {
            match context.get_state() {
                ContextState::Ready => Ok(true),
                ContextState::Failed | ContextState::Terminated => {
                    Err(anyhow::anyhow!("PulseAudio context failed: {}", context.errno()))
                }
                _ => Ok(false),
            }
        })? {
            return Ok(None);
        }

        let mut stream = Stream::new(&mut context, "Wallpaper audio spectrum", &spec, None)
            .context("failed to create PulseAudio record stream")?;
        let buffer_attr = BufferAttr {
            maxlength: fragment_bytes.saturating_mul(4),
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: fragment_bytes,
        };
        stream
            .connect_record(Some(source), Some(&buffer_attr), StreamFlagSet::ADJUST_LATENCY)
            .context("failed to connect PulseAudio monitor source")?;
        if !drive_pulse_until(
            &mut mainloop,
            Duration::from_secs(2),
            &mut cancelled,
            || match stream.get_state() {
                StreamState::Ready => Ok(true),
                StreamState::Failed | StreamState::Terminated => {
                    Err(anyhow::anyhow!("PulseAudio record stream failed"))
                }
                _ => Ok(false),
            },
        )? {
            return Ok(None);
        }

        Ok(Some(Self {
            stream,
            context,
            mainloop,
            sample_rate,
            update_interval_seconds: 1.0 / update_hz as f32,
            samples: Vec::with_capacity(AUDIO_FFT_SIZE * 2),
            smoothed_spectrum: StereoSpectrum::default(),
        }))
    }

    pub(crate) fn poll_spectrum(&mut self) -> Result<Option<Arc<[f32]>>> {
        match self.mainloop.iterate(false) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) => anyhow::bail!("PulseAudio mainloop quit"),
            IterateResult::Err(error) => anyhow::bail!("PulseAudio mainloop failed: {error}"),
        }
        match self.context.get_state() {
            ContextState::Failed | ContextState::Terminated => {
                anyhow::bail!("PulseAudio context disconnected: {}", self.context.errno())
            }
            _ => {}
        }
        match self.stream.get_state() {
            StreamState::Failed | StreamState::Terminated => {
                anyhow::bail!("PulseAudio record stream disconnected")
            }
            _ => {}
        }

        let required_samples = AUDIO_FFT_SIZE * 2;
        let mut received_samples = false;
        loop {
            match self.stream.peek().context("failed to peek PulseAudio samples")? {
                PeekResult::Empty => break,
                PeekResult::Hole(bytes) => {
                    received_samples = received_samples || bytes > 0;
                    append_recent_silence(
                        &mut self.samples,
                        bytes / std::mem::size_of::<f32>(),
                        required_samples,
                    );
                }
                PeekResult::Data(bytes) => {
                    received_samples = received_samples || !bytes.is_empty();
                    append_recent_pcm_bytes(&mut self.samples, bytes, required_samples);
                }
            }
            self.stream.discard().context("failed to consume PulseAudio samples")?;
        }

        if !received_samples || self.samples.len() < required_samples {
            return Ok(None);
        }
        debug_assert_eq!(self.samples.len(), required_samples);
        let raw_spectrum = spectrum_from_interleaved_pcm(&self.samples, self.sample_rate);
        let spectrum = smooth_spectrum(
            &raw_spectrum,
            &mut self.smoothed_spectrum,
            self.update_interval_seconds,
        )
        .flattened();
        Ok(Some(spectrum))
    }
}

impl Drop for PulseAudioCapture {
    fn drop(&mut self) {
        let _ = self.stream.disconnect();
        self.context.disconnect();
    }
}

fn drive_pulse_until(
    mainloop: &mut Mainloop,
    timeout: Duration,
    cancelled: &mut impl FnMut() -> bool,
    mut ready: impl FnMut() -> Result<bool>,
) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if cancelled() {
            return Ok(false);
        }
        match mainloop.iterate(false) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) => anyhow::bail!("PulseAudio mainloop quit during setup"),
            IterateResult::Err(error) => {
                anyhow::bail!("PulseAudio mainloop failed during setup: {error}")
            }
        }
        if ready()? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out while connecting PulseAudio capture");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use super::{
        append_recent_pcm_bytes, append_recent_silence, hertz_to_upper_fft_bin,
        spectrum_band_layout, spectrum_from_interleaved_pcm, AUDIO_FFT_SIZE, AUDIO_SPECTRUM_BINS,
    };

    fn stereo_sine(
        frequency_hz: f32,
        sample_rate: u32,
        frames: usize,
        left_amplitude: f32,
        right_amplitude: f32,
    ) -> Vec<f32> {
        (0..frames)
            .flat_map(|index| {
                let sample = (TAU * frequency_hz * index as f32 / sample_rate as f32).sin();
                [sample * left_amplitude, sample * right_amplitude]
            })
            .collect()
    }

    fn expected_band(frequency_hz: f32, sample_rate: u32) -> usize {
        let layout = spectrum_band_layout(sample_rate);
        let bin = hertz_to_upper_fft_bin(frequency_hz, sample_rate);
        (0..AUDIO_SPECTRUM_BINS)
            .find(|band| bin >= layout.edges[*band] && bin < layout.edges[*band + 1])
            .unwrap_or(AUDIO_SPECTRUM_BINS - 1)
    }

    #[test]
    fn silence_produces_zero_spectrum_in_renderer_channel_order() {
        let spectrum = spectrum_from_interleaved_pcm(&vec![0.0; AUDIO_FFT_SIZE * 2], 48_000);
        assert!(spectrum.left.iter().all(|value| *value == 0.0));
        assert!(spectrum.right.iter().all(|value| *value == 0.0));

        let flattened = spectrum.flattened();
        assert_eq!(flattened.len(), AUDIO_SPECTRUM_BINS * 2);
        assert_eq!(&flattened[..AUDIO_SPECTRUM_BINS], spectrum.left.as_slice());
        assert_eq!(&flattened[AUDIO_SPECTRUM_BINS..], spectrum.right.as_slice());
    }

    #[test]
    fn sine_energy_is_concentrated_near_its_expected_frequency_band() {
        let sample_rate = 48_000;
        let frequency_hz = 1_500.0;
        let spectrum = spectrum_from_interleaved_pcm(
            &stereo_sine(frequency_hz, sample_rate, AUDIO_FFT_SIZE, 1.0, 1.0),
            sample_rate,
        );
        let peak = spectrum
            .left
            .iter()
            .enumerate()
            .max_by(|(_, left), (_, right)| left.total_cmp(right))
            .map(|(index, _)| index)
            .expect("spectrum has bins");

        let expected = expected_band(frequency_hz, sample_rate);
        assert!(peak.abs_diff(expected) <= 2, "unexpected 1.5 kHz peak band {peak}");
        assert!(spectrum.left[peak] > 0.8);
        assert!((spectrum.left[peak] - spectrum.right[peak]).abs() < 0.001);
    }

    #[test]
    fn quiet_music_level_is_mapped_to_a_useful_visual_response() {
        let sample_rate = 48_000;
        let spectrum = spectrum_from_interleaved_pcm(
            &stereo_sine(440.0, sample_rate, AUDIO_FFT_SIZE, 0.03, 0.03),
            sample_rate,
        );
        let response = spectrum.left.iter().copied().fold(0.0_f32, f32::max);

        assert!(response > 0.60, "quiet response is too small: {response}");
        assert!(response < 0.80, "quiet response is too large: {response}");
    }

    #[test]
    fn stereo_channels_remain_independent() {
        let sample_rate = 48_000;
        let spectrum = spectrum_from_interleaved_pcm(
            &stereo_sine(1_000.0, sample_rate, AUDIO_FFT_SIZE, 1.0, 0.0),
            sample_rate,
        );

        assert!(spectrum.left.iter().any(|value| *value > 0.8));
        assert!(spectrum.right.iter().all(|value| *value == 0.0));
    }

    #[test]
    fn pulse_backlog_retains_only_the_latest_bounded_sample_window() {
        let mut retained = vec![1.0_f32, 2.0];
        let incoming =
            (3_u32..=12).flat_map(|value| (value as f32).to_ne_bytes()).collect::<Vec<_>>();

        append_recent_pcm_bytes(&mut retained, &incoming, 4);
        assert_eq!(retained, vec![9.0, 10.0, 11.0, 12.0]);

        append_recent_silence(&mut retained, 10_000, 4);
        assert_eq!(retained, vec![0.0; 4]);
    }
}
