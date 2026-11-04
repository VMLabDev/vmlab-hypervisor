//! Measurements for a 10-second, 1 kHz left-channel sine and right-channel
//! 1 ms clicks at 0.25, 1.25, ..., 9.25 seconds, S16LE stereo at 48 kHz.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct Quality {
    pub frequency_hz: f64,
    pub speed_error_percent: f64,
    pub duration_seconds: f64,
    pub wall_seconds: Option<f64>,
    pub wall_speed_error_percent: Option<f64>,
    pub dropouts: usize,
    pub discontinuities: usize,
    pub thdn_db: f64,
    pub clicks: usize,
    pub click_interval_error_percent: f64,
}

#[derive(Deserialize)]
struct Period {
    offset: usize,
    bytes: usize,
    ns: u64,
}

pub fn measure(pcm: &[u8], timing: Option<&str>) -> Quality {
    assert!(pcm.len().is_multiple_of(4), "partial stereo frame");
    let left: Vec<f64> = pcm
        .as_chunks::<4>()
        .0
        .iter()
        .map(|f| f64::from(i16::from_le_bytes([f[0], f[1]])))
        .collect();
    let peak = left.iter().copied().map(f64::abs).fold(0.0, f64::max);
    assert!(peak > 10.0, "capture is silent");
    let first = left.iter().position(|x| x.abs() > peak * 0.02).unwrap();
    let last = left.iter().rposition(|x| x.abs() > peak * 0.02).unwrap();
    let signal = &left[first..=last];
    let crossings: Vec<f64> = signal
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[0] <= 0.0 && w[1] > 0.0)
        .map(|(i, w)| i as f64 - w[0] / (w[1] - w[0]))
        .collect();
    assert!(crossings.len() > 100, "too few sine cycles");
    let frequency =
        (crossings.len() - 1) as f64 * 48000.0 / (crossings.last().unwrap() - crossings[0]);
    // Fit both quadratures and DC at the nominal frequency. The residual
    // includes harmonics, noise, missing samples, and frequency error.
    let mut sin = 0.0;
    let mut cos = 0.0;
    let dc = signal.iter().sum::<f64>() / signal.len() as f64;
    for (i, x) in signal.iter().enumerate() {
        let angle = std::f64::consts::TAU * 1000.0 * i as f64 / 48000.0;
        sin += (x - dc) * angle.sin();
        cos += (x - dc) * angle.cos();
    }
    sin *= 2.0 / signal.len() as f64;
    cos *= 2.0 / signal.len() as f64;
    let mut residual = 0.0;
    let mut power = 0.0;
    for (i, x) in signal.iter().enumerate() {
        let angle = std::f64::consts::TAU * 1000.0 * i as f64 / 48000.0;
        let fitted = sin * angle.sin() + cos * angle.cos();
        residual += (x - dc - fitted).powi(2);
        power += fitted.powi(2);
    }
    let mut dropouts = 0;
    let mut zero_run = 0;
    for x in signal {
        if x.abs() <= peak * 0.001 {
            zero_run += 1;
            if zero_run == 48 {
                dropouts += 1;
            }
        } else {
            zero_run = 0;
        }
    }
    let max_step = 2.0 * peak * (std::f64::consts::PI * 1000.0 / 48000.0).sin();
    let discontinuities = signal
        .windows(2)
        .filter(|w| (w[1] - w[0]).abs() > max_step * 1.5 + 2.0)
        .count();
    let mut clicks = Vec::new();
    let mut was_high = false;
    for (i, frame) in pcm.as_chunks::<4>().0.iter().enumerate() {
        let high = f64::from(i16::from_le_bytes([frame[2], frame[3]])) > peak * 0.5;
        if high && !was_high {
            clicks.push(i);
        }
        was_high = high;
    }
    let click_error = clicks
        .windows(2)
        .map(|w| ((w[1] - w[0]) as f64 / 48000.0 - 1.0).abs() * 100.0)
        .fold(0.0, f64::max);
    let wall_seconds = timing.map(|text| {
        let periods: Vec<Period> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("audio timing JSON"))
            .collect();
        let at = |frame: usize| {
            let byte = frame * 4;
            let period = periods
                .iter()
                .find(|p| p.offset <= byte && byte < p.offset + p.bytes)
                .expect("missing timestamp for captured frame");
            period.ns as f64 / 1e9 - (period.offset + period.bytes - byte) as f64 / (48000.0 * 4.0)
        };
        at(last) - at(first)
    });
    Quality {
        frequency_hz: frequency,
        speed_error_percent: (frequency / 1000.0 - 1.0) * 100.0,
        duration_seconds: (last - first + 1) as f64 / 48000.0,
        wall_seconds,
        wall_speed_error_percent: wall_seconds
            .map(|wall| ((last - first) as f64 / 48000.0 / wall - 1.0) * 100.0),
        dropouts,
        discontinuities,
        thdn_db: 10.0 * (residual / power).log10(),
        clicks: clicks.len(),
        click_interval_error_percent: click_error,
    }
}

impl Quality {
    pub fn assert_clean(&self) {
        assert!(self.speed_error_percent.abs() <= 0.1, "{self:?}");
        assert!((self.duration_seconds - 10.0).abs() <= 0.01, "{self:?}");
        if let Some(error) = self.wall_speed_error_percent {
            assert!(error.abs() <= 0.1, "{self:?}");
        }
        assert_eq!(self.dropouts, 0, "{self:?}");
        assert_eq!(self.discontinuities, 0, "{self:?}");
        assert!(self.thdn_db < -50.0, "{self:?}");
        assert_eq!(self.clicks, 10, "{self:?}");
        assert!(self.click_interval_error_percent <= 0.1, "{self:?}");
    }
}

pub fn report(path: &Path, guest: &str) {
    if let Some(root) = std::env::var_os("VMLAB_HV_AUDIO_QUALITY_ARTIFACTS") {
        let root = Path::new(&root).join(guest);
        std::fs::create_dir_all(&root).unwrap();
        for extension in ["pcm", "timing", "rendered", "pcm.events"] {
            let source = path.with_extension(extension);
            if source.is_file() {
                std::fs::copy(source, root.join(format!("playback.{extension}"))).unwrap();
            }
        }
    }
    let pcm = std::fs::read(path).expect("guest playback PCM");
    let timing = std::fs::read_to_string(path.with_extension("timing")).unwrap();
    // Drivers may drain part of the final cyclic buffer after the requested
    // signal. Measure exactly the known signal, retaining its original offset.
    let first_click = pcm
        .as_chunks::<4>()
        .0
        .iter()
        .position(|f| i16::from_le_bytes([f[2], f[3]]) > 100)
        .expect("missing click train");
    let start = first_click
        .checked_sub(12000)
        .expect("missing signal lead-in");
    let end = (start + 480000) * 4;
    assert!(pcm.len() >= end, "incomplete ten-second guest signal");
    let mut signal = pcm[..end].to_vec();
    signal[..start * 4].fill(0);
    let submitted = measure(&signal, Some(&timing));
    eprintln!(
        "QUALITY {guest} submitted {}",
        serde_json::to_string(&submitted).unwrap()
    );
    let pcm = std::fs::read(path.with_extension("rendered")).expect("paced sink output");
    let rendered = measure(&pcm, None);
    eprintln!(
        "QUALITY {guest} rendered {}",
        serde_json::to_string(&rendered).unwrap()
    );
    if std::env::var_os("VMLAB_HV_AUDIO_QUALITY_REPORT_ONLY").is_none() {
        submitted.assert_clean();
        rendered.assert_clean();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(rate: usize) -> Vec<u8> {
        (0..rate * 10)
            .flat_map(|i| {
                let sine = (6000.0
                    * (std::f64::consts::TAU * 1000.0 * i as f64 / rate as f64).sin())
                    as i16;
                let click = if i % rate >= rate / 4 && i % rate < rate / 4 + 48 {
                    6000i16
                } else {
                    0
                };
                [sine.to_le_bytes(), click.to_le_bytes()].concat()
            })
            .collect()
    }

    #[test]
    fn clean_reference_meets_the_quality_bars() {
        measure(&reference(48000), None).assert_clean();
    }

    #[test]
    fn measurement_detects_speed_gaps_and_sample_jumps() {
        let slow = measure(&reference(48500), None);
        assert!(slow.speed_error_percent < -1.0);
        let mut broken = reference(48000);
        broken[96000..97024].fill(0);
        broken[192000..192002].copy_from_slice(&30000i16.to_le_bytes());
        let broken = measure(&broken, None);
        assert_eq!(broken.dropouts, 1);
        assert!(broken.discontinuities >= 2);
        assert!(broken.thdn_db > -40.0);
    }

    #[test]
    fn timing_detects_slow_delivery_even_when_pcm_pitch_is_exact() {
        let pcm = reference(48000);
        let timing: String = (0..480000)
            .step_by(256)
            .map(|frame| {
                let frames = 256.min(480000 - frame);
                serde_json::json!({
                    "offset": frame * 4, "bytes": frames * 4,
                    "ns": ((frame + frames) as f64 / 48000.0 * 1.01e9) as u64
                })
                .to_string()
                    + "\n"
            })
            .collect();
        let quality = measure(&pcm, Some(&timing));
        assert!(quality.speed_error_percent.abs() < 0.001);
        assert!((quality.wall_speed_error_percent.unwrap() + 0.990099).abs() < 0.001);
    }
}
