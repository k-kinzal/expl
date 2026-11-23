//! Support for expl's own tests: wall-clock bounds that stretch on a
//! loaded machine.
//!
//! A test that bounds how long something takes is only as honest as the
//! machine is idle. Every such bound goes through [`scaled`], which
//! stretches it by the factor in [`TIMEOUT_SCALE_VAR`], so a busy machine
//! (an adoption gate, an evaluation running beside other work) can give the
//! suite more time without editing it. At the default scale of 1 each bound
//! is exactly what the test states, so a regression a bound guards against
//! still fails there.

use std::time::Duration;

/// The environment variable holding the factor that stretches wall-clock
/// bounds in tests: a positive number, 1 when unset or unusable.
pub const TIMEOUT_SCALE_VAR: &str = "EXPL_TEST_TIMEOUT_SCALE";

/// The factor in [`TIMEOUT_SCALE_VAR`]; 1 when it is unset, not a number,
/// or not positive and finite.
#[must_use]
pub fn timeout_scale() -> f64 {
    std::env::var(TIMEOUT_SCALE_VAR)
        .ok()
        .and_then(|scale| scale.trim().parse::<f64>().ok())
        .filter(|scale| scale.is_finite() && *scale > 0.0)
        .unwrap_or(1.0)
}

/// `base` stretched by [`timeout_scale`]: `base` itself at the default
/// scale, and `base` again when the product is too large to represent or
/// rounds down to nothing.
#[must_use]
pub fn scaled(base: Duration) -> Duration {
    Duration::try_from_secs_f64(base.as_secs_f64() * timeout_scale())
        .ok()
        .filter(|scaled| !scaled.is_zero() || base.is_zero())
        .unwrap_or(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_scale_leaves_a_bound_as_stated() {
        if std::env::var_os(TIMEOUT_SCALE_VAR).is_none() {
            assert!((timeout_scale() - 1.0).abs() < f64::EPSILON);
            assert_eq!(scaled(Duration::from_secs(5)), Duration::from_secs(5));
        }
        let scale = timeout_scale();
        assert!(scale.is_finite() && scale > 0.0, "{scale}");
        assert!(scaled(Duration::ZERO).is_zero());
    }
}
