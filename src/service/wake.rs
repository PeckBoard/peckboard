use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Detects system wake-from-sleep events by monitoring elapsed time between
/// periodic ticks. If the wall-clock gap between two ticks greatly exceeds the
/// expected interval, the machine was likely asleep.
pub struct WakeDetector {
    in_grace: Arc<AtomicBool>,
}

impl WakeDetector {
    /// Start the wake detector background task.
    ///
    /// Polls every 10 seconds. If the elapsed time since the last poll exceeds
    /// 30 seconds (3x the interval), a sleep/wake event is assumed. On detection
    /// the grace flag is set for 30 seconds.
    pub fn start() -> Self {
        let in_grace = Arc::new(AtomicBool::new(false));
        let flag = in_grace.clone();

        tokio::spawn(async move {
            const POLL_INTERVAL: Duration = Duration::from_secs(10);
            const WAKE_THRESHOLD: Duration = Duration::from_secs(30);
            const GRACE_DURATION: Duration = Duration::from_secs(30);

            let mut last_tick = Instant::now();

            loop {
                tokio::time::sleep(POLL_INTERVAL).await;

                let now = Instant::now();
                let elapsed = now.duration_since(last_tick);
                last_tick = now;

                if elapsed > WAKE_THRESHOLD {
                    tracing::warn!(
                        elapsed_secs = elapsed.as_secs(),
                        "Wake-from-sleep detected (expected ~10s, got {}s). \
                         Entering {}s grace window.",
                        elapsed.as_secs(),
                        GRACE_DURATION.as_secs(),
                    );

                    flag.store(true, Ordering::SeqCst);

                    let grace_flag = flag.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(GRACE_DURATION).await;
                        grace_flag.store(false, Ordering::SeqCst);
                        tracing::info!("Wake grace window expired, resuming normal operation");
                    });
                }
            }
        });

        WakeDetector { in_grace }
    }

    /// Returns `true` if the system recently woke from sleep and is still within
    /// the grace window.
    pub fn in_grace_window(&self) -> bool {
        self.in_grace.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wake_detector_initially_not_in_grace() {
        // Without starting the background task, the flag should be false.
        let detector = WakeDetector {
            in_grace: Arc::new(AtomicBool::new(false)),
        };
        assert!(!detector.in_grace_window());
    }

    #[test]
    fn test_wake_detector_grace_flag_readable() {
        let flag = Arc::new(AtomicBool::new(true));
        let detector = WakeDetector {
            in_grace: flag.clone(),
        };
        assert!(detector.in_grace_window());

        flag.store(false, Ordering::SeqCst);
        assert!(!detector.in_grace_window());
    }
}
