use std::sync::Arc;
use std::time::Duration;

/// Configuration knobs for the discovery + gossip subsystems. Defaults
/// match plan-02 §"AdConfig"; tests construct values closer to zero so
/// they can drive ticks deterministically.
#[derive(Clone)]
pub struct AdConfig {
    /// Refresh of dynamic fields (load, vram_free, util, mem_available).
    /// Minimum 250 ms.
    pub fast_tick: Duration,
    /// Re-enumeration of static hardware (PCI, GPU list, NUMA, CPU info).
    /// Minimum 5 s.
    pub slow_tick: Duration,
    pub gossip_period: Duration,
    pub peer_grace: Duration,
    /// Callback returning the current local task count. Plan 04 supplies
    /// this; plan 02 reports 0 until then.
    pub task_count_fn: Option<Arc<dyn Fn() -> u32 + Send + Sync>>,
}

impl AdConfig {
    pub const MIN_FAST_TICK: Duration = Duration::from_millis(250);
    pub const MIN_SLOW_TICK: Duration = Duration::from_secs(5);
}

impl Default for AdConfig {
    fn default() -> Self {
        Self {
            fast_tick: Duration::from_secs(1),
            slow_tick: Duration::from_secs(60),
            gossip_period: Duration::from_secs(10),
            peer_grace: Duration::from_secs(90),
            task_count_fn: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AdConfigError {
    #[error("fast_tick {0:?} below minimum {1:?}")]
    FastTickTooLow(Duration, Duration),
    #[error("slow_tick {0:?} below minimum {1:?}")]
    SlowTickTooLow(Duration, Duration),
}

impl AdConfig {
    pub fn validate(&self) -> Result<(), AdConfigError> {
        if self.fast_tick < Self::MIN_FAST_TICK {
            return Err(AdConfigError::FastTickTooLow(self.fast_tick, Self::MIN_FAST_TICK));
        }
        if self.slow_tick < Self::MIN_SLOW_TICK {
            return Err(AdConfigError::SlowTickTooLow(self.slow_tick, Self::MIN_SLOW_TICK));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_passes_validation() {
        AdConfig::default().validate().unwrap();
    }

    #[test]
    fn fast_tick_below_minimum_rejected() {
        let mut cfg = AdConfig::default();
        cfg.fast_tick = Duration::from_millis(100);
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, AdConfigError::FastTickTooLow(_, _)));
    }

    #[test]
    fn slow_tick_below_minimum_rejected() {
        let mut cfg = AdConfig::default();
        cfg.slow_tick = Duration::from_secs(1);
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, AdConfigError::SlowTickTooLow(_, _)));
    }
}
