//! Original local notification tones, shared with the web player and WAV exporter.
use std::{collections::BTreeMap, f64::consts::PI, sync::OnceLock};

type Score = BTreeMap<String, Vec<[f64; 3]>>;

pub fn render(name: &str, volume: f32) -> Option<Vec<f32>> {
    static SCORE: OnceLock<Score> = OnceLock::new();
    let score = SCORE.get_or_init(|| {
        serde_json::from_str(include_str!("../../../../shared/sound-cues.json")).expect("built-in cue score")
    });
    let notes = score.get(name)?;
    let length = notes.iter().map(|[_, at, duration]| ((at + duration) * 48.0).round() as usize).max()?;
    let mut samples = vec![0.0; length];
    let gain = if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 0.0 };
    for &[frequency, at, duration] in notes {
        let start = (at * 48.0).round() as usize;
        let length = (duration * 48.0).round() as usize;
        for i in 0..length {
            let phase = 2.0 * PI * frequency * i as f64 / 48_000.0;
            let progress = i as f64 / length as f64;
            let envelope = (PI * progress).sin().powi(2) * (-2.0 * progress).exp();
            samples[start + i] += gain * (0.18 * envelope * (phase.sin() + 0.18 * (2.0 * phase).sin())) as f32;
        }
    }
    Some(samples)
}

/// Only these deliberate control confirmations sound while deafened.
pub fn while_deafened(name: &str) -> bool {
    matches!(name, "mute" | "unmute" | "deafen" | "undeafen")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_cues_are_short_quiet_finite_and_fade_at_both_ends() {
        let score: Score = serde_json::from_str(include_str!("../../../../shared/sound-cues.json")).unwrap();
        assert_eq!(score.len(), 14);
        for name in score.keys() {
            let pcm = render(name, 1.0).unwrap();
            assert!(pcm.len() < 14_400, "{name}");
            assert!(pcm.iter().all(|s| s.is_finite() && s.abs() < 0.1), "{name}");
            assert!(pcm.iter().any(|s| s.abs() > 0.01), "{name}");
            assert!(pcm[0].abs() < 0.0001 && pcm.last().unwrap().abs() < 0.0001);
            assert!(render(name, 0.0).unwrap().iter().all(|s| *s == 0.0));
        }
        assert!(render("unknown", 1.0).is_none());
    }
}
