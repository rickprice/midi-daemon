use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum TimerEvent {
    Tick { tick: u64, bpm: f64, ppqn: u32 },
}

/// Spawns a high-resolution timer that sends TimerEvent::Tick into `tx`.
/// BPM and PPQN are atomically adjustable at runtime (from Lua via set_bpm/set_ppqn).
pub struct Timer {
    pub bpm: Arc<AtomicU32>,   // stored as bpm * 100 for integer atomics
    pub ppqn: Arc<AtomicU32>,
    pub running: Arc<AtomicBool>,
}

impl Timer {
    pub fn new(default_bpm: f64, default_ppqn: u32) -> Self {
        Timer {
            bpm: Arc::new(AtomicU32::new((default_bpm * 100.0) as u32)),
            ppqn: Arc::new(AtomicU32::new(default_ppqn)),
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn get_bpm(&self) -> f64 {
        self.bpm.load(Ordering::Relaxed) as f64 / 100.0
    }

    pub fn set_bpm(&self, bpm: f64) {
        self.bpm.store((bpm * 100.0) as u32, Ordering::Relaxed);
    }

    pub fn get_ppqn(&self) -> u32 {
        self.ppqn.load(Ordering::Relaxed)
    }

    pub fn set_ppqn(&self, ppqn: u32) {
        self.ppqn.store(ppqn, Ordering::Relaxed);
    }

    /// Spawn the timer loop in a dedicated thread.
    /// Returns the thread handle.
    pub fn spawn(
        &self,
        tx: mpsc::Sender<TimerEvent>,
    ) -> std::thread::JoinHandle<()> {
        let bpm_atomic = Arc::clone(&self.bpm);
        let ppqn_atomic = Arc::clone(&self.ppqn);
        let running = Arc::clone(&self.running);

        std::thread::spawn(move || {
            let mut tick: u64 = 0;

            loop {
                if !running.load(Ordering::Relaxed) {
                    break;
                }

                let bpm = bpm_atomic.load(Ordering::Relaxed) as f64 / 100.0;
                let ppqn = ppqn_atomic.load(Ordering::Relaxed);

                // Duration per tick = 60 / (bpm * ppqn) seconds
                let tick_secs = 60.0 / (bpm * ppqn as f64);
                let duration = Duration::from_secs_f64(tick_secs);

                let event = TimerEvent::Tick { tick, bpm, ppqn };
                if tx.blocking_send(event).is_err() {
                    // Receiver dropped — route is being torn down
                    break;
                }

                tick = tick.wrapping_add(1);
                spin_sleep::sleep(duration);
            }
        })
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn new_sets_bpm_and_ppqn() {
        let t = Timer::new(120.0, 24);
        assert!((t.get_bpm() - 120.0).abs() < 0.01);
        assert_eq!(t.get_ppqn(), 24);
    }

    #[test]
    fn set_get_bpm_roundtrip() {
        let t = Timer::new(120.0, 24);
        t.set_bpm(140.0);
        assert!((t.get_bpm() - 140.0).abs() < 0.01);
    }

    #[test]
    fn set_get_bpm_fractional() {
        let t = Timer::new(120.0, 24);
        t.set_bpm(98.75);
        // Stored as integer * 100, so precision to 0.01
        assert!((t.get_bpm() - 98.75).abs() < 0.01);
    }

    #[test]
    fn set_get_ppqn_roundtrip() {
        let t = Timer::new(120.0, 24);
        t.set_ppqn(96);
        assert_eq!(t.get_ppqn(), 96);
    }

    #[test]
    fn stop_sets_running_false() {
        let t = Timer::new(120.0, 24);
        assert!(t.running.load(Ordering::Relaxed));
        t.stop();
        assert!(!t.running.load(Ordering::Relaxed));
    }

    #[test]
    fn drop_stops_timer() {
        let t = Timer::new(120.0, 24);
        let running = Arc::clone(&t.running);
        assert!(running.load(Ordering::Relaxed));
        drop(t);
        assert!(!running.load(Ordering::Relaxed));
    }

    #[test]
    fn stop_is_idempotent() {
        let t = Timer::new(120.0, 24);
        t.stop();
        t.stop(); // must not panic
        assert!(!t.running.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn spawned_timer_delivers_ticks() {
        // 600 BPM × 1 PPQN = 10 ticks/second → expect several ticks in 500 ms
        let t = Timer::new(600.0, 1);
        let (tx, mut rx) = mpsc::channel(16);
        let _handle = t.spawn(tx);

        let mut count = 0u32;
        let deadline = tokio::time::timeout(Duration::from_millis(500), async {
            while let Some(_) = rx.recv().await {
                count += 1;
                if count >= 3 {
                    break;
                }
            }
        });
        deadline.await.expect("timed out before receiving 3 ticks");
        assert!(count >= 3);
    }

    #[tokio::test]
    async fn tick_event_carries_correct_bpm_and_ppqn() {
        let t = Timer::new(120.0, 24);
        let (tx, mut rx) = mpsc::channel(4);
        let _handle = t.spawn(tx);

        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");

        let TimerEvent::Tick { bpm, ppqn, .. } = ev;
        assert!((bpm - 120.0).abs() < 0.01);
        assert_eq!(ppqn, 24);
    }

    #[tokio::test]
    async fn tick_counter_is_monotonically_increasing() {
        let t = Timer::new(600.0, 1);
        let (tx, mut rx) = mpsc::channel(16);
        let _handle = t.spawn(tx);

        let mut prev: Option<u64> = None;
        let _ = tokio::time::timeout(Duration::from_millis(300), async {
            while let Some(TimerEvent::Tick { tick, .. }) = rx.recv().await {
                if let Some(p) = prev {
                    assert_eq!(tick, p + 1, "tick counter skipped or went backwards");
                }
                prev = Some(tick);
                if tick >= 4 {
                    break;
                }
            }
        })
        .await;
        assert!(prev.is_some(), "received no ticks");
    }

    #[tokio::test]
    async fn channel_close_stops_timer_thread() {
        let t = Timer::new(600.0, 1);
        let running = Arc::clone(&t.running);
        let (tx, rx) = mpsc::channel(4);
        let handle = t.spawn(tx);

        // Receive one tick then drop the receiver — timer thread should exit
        drop(rx);
        // Give the thread a moment to observe the closed channel
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(handle.is_finished(), "timer thread did not exit after channel close");
        let _ = running; // keep alive for inspection above
    }

    #[tokio::test]
    async fn bpm_change_reflected_in_subsequent_ticks() {
        let t = Timer::new(600.0, 1);
        let (tx, mut rx) = mpsc::channel(16);
        let _handle = t.spawn(tx);

        // Receive first tick (original BPM)
        let first = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await.unwrap().unwrap();
        let TimerEvent::Tick { bpm: bpm0, .. } = first;
        assert!((bpm0 - 600.0).abs() < 0.01);

        t.set_bpm(300.0);

        // Receive ticks until we see the updated BPM
        let mut saw_new = false;
        let _ = tokio::time::timeout(Duration::from_millis(500), async {
            while let Some(TimerEvent::Tick { bpm, .. }) = rx.recv().await {
                if (bpm - 300.0).abs() < 0.01 {
                    saw_new = true;
                    break;
                }
            }
        })
        .await;
        assert!(saw_new, "never observed updated BPM in tick events");
    }
}
