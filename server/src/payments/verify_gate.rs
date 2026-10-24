//! Limits outbound `verify_payment` calls. The webhook route is unauthenticated
//! and the checkout poll is buyer-triggered, so without a limit every request
//! could cost a provider API call (DoS amplification, and the provider's rate
//! limit would then block real verifies too).
//!
//! - At most one verify per checkout per `cooldown` (default 10 s), keyed on our
//!   checkout id, never on unverified request fields.
//! - At most `max_concurrent` (default 8) verify calls in flight process-wide.
//!
//! The state is in memory, per process: with several instances the limits are
//! per instance, which still bounds the amplification.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(10);
pub const DEFAULT_MAX_CONCURRENT: usize = 8;
/// Cooldown entries are pruned once the map grows past this many.
const PRUNE_AT: usize = 10_000;

pub struct VerifyGate {
    cooldown: Duration,
    last: Mutex<HashMap<Uuid, Instant>>,
    permits: Arc<Semaphore>,
}

impl Default for VerifyGate {
    fn default() -> Self {
        Self::new(DEFAULT_COOLDOWN, DEFAULT_MAX_CONCURRENT)
    }
}

impl VerifyGate {
    pub fn new(cooldown: Duration, max_concurrent: usize) -> Self {
        Self {
            cooldown,
            last: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(max_concurrent)),
        }
    }

    /// A slot for one verify call, or `None` when `max_concurrent` are in flight.
    pub fn try_permit(&self) -> Option<OwnedSemaphorePermit> {
        self.permits.clone().try_acquire_owned().ok()
    }

    /// Records a verify attempt for `checkout_id` now, unless one was made
    /// within the cooldown (then returns false and records nothing).
    pub fn try_start(&self, checkout_id: Uuid) -> bool {
        let now = Instant::now();
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = last.get(&checkout_id)
            && now.duration_since(*at) < self.cooldown
        {
            return false;
        }
        if last.len() >= PRUNE_AT {
            let cooldown = self.cooldown;
            last.retain(|_, at| now.duration_since(*at) < cooldown);
        }
        last.insert(checkout_id, now);
        true
    }

    /// Forgets every cooldown (tests; an operator-triggered re-check).
    pub fn clear(&self) {
        self.last.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooldown_is_per_checkout() {
        let gate = VerifyGate::new(Duration::from_secs(60), 2);
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        assert!(gate.try_start(a));
        assert!(!gate.try_start(a));
        assert!(gate.try_start(b));
        gate.clear();
        assert!(gate.try_start(a));
    }

    #[test]
    fn zero_cooldown_never_blocks() {
        let gate = VerifyGate::new(Duration::ZERO, 1);
        let a = Uuid::new_v4();
        assert!(gate.try_start(a));
        assert!(gate.try_start(a));
    }

    #[test]
    fn permits_are_capped() {
        let gate = VerifyGate::new(Duration::ZERO, 2);
        let p1 = gate.try_permit().unwrap();
        let _p2 = gate.try_permit().unwrap();
        assert!(gate.try_permit().is_none());
        drop(p1);
        assert!(gate.try_permit().is_some());
    }
}
