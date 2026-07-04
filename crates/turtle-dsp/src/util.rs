//! Small shared DSP helpers.

/// One-pole smoothing coefficient for a given time constant in milliseconds.
/// `0.0` (or less) means "instant" (coeff `1.0`).
pub(crate) fn one_pole_coeff(time_ms: f32, sample_rate: f32) -> f32 {
    if time_ms <= 0.0 {
        return 1.0;
    }
    let samples = time_ms * 0.001 * sample_rate;
    (1.0 / samples).clamp(0.0, 1.0)
}
