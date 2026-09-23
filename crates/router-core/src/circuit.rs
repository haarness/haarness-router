use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct CircuitBreaker {
    state: Mutex<State>,
    failure_threshold: u32,
    cooldown: Duration,
}

#[derive(Debug)]
struct State {
    failures: u32,
    opened_at: Option<Instant>,
    half_open_in_flight: bool,
    in_flight: u32,
    generation: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct CircuitPermit {
    generation: u64,
    half_open: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

impl CircuitBreaker {
    #[must_use]
    pub fn new(failure_threshold: u32, cooldown: Duration) -> Self {
        Self {
            state: Mutex::new(State {
                failures: 0,
                opened_at: None,
                half_open_in_flight: false,
                in_flight: 0,
                generation: 0,
            }),
            failure_threshold,
            cooldown,
        }
    }

    pub fn allow(&self) -> Option<CircuitPermit> {
        let mut state = self.state.lock().expect("circuit lock poisoned");
        let Some(opened_at) = state.opened_at else {
            state.in_flight = state.in_flight.saturating_add(1);
            return Some(CircuitPermit {
                generation: state.generation,
                half_open: false,
            });
        };
        if opened_at.elapsed() < self.cooldown || state.half_open_in_flight {
            return None;
        }
        state.half_open_in_flight = true;
        state.in_flight = state.in_flight.saturating_add(1);
        Some(CircuitPermit {
            generation: state.generation,
            half_open: true,
        })
    }

    pub fn success(&self, _permit: CircuitPermit) {
        let mut state = self.state.lock().expect("circuit lock poisoned");
        state.in_flight = state.in_flight.saturating_sub(1);
        state.failures = 0;
        state.opened_at = None;
        state.half_open_in_flight = false;
        state.generation = state.generation.wrapping_add(1);
    }

    pub fn failure(&self, permit: CircuitPermit) {
        let mut state = self.state.lock().expect("circuit lock poisoned");
        state.in_flight = state.in_flight.saturating_sub(1);
        if permit.half_open {
            state.half_open_in_flight = false;
            state.opened_at = Some(Instant::now());
            return;
        }
        // A success recorded after this attempt began invalidates its failure
        // for consecutive-failure purposes. Defer opening while older attempts
        // remain in flight so a concurrent connection wave cannot trip the
        // whole target before its successful responses arrive.
        if permit.generation == state.generation {
            state.failures = state.failures.saturating_add(1);
        }
        if state.failures >= self.failure_threshold && state.in_flight == 0 {
            state.opened_at = Some(Instant::now());
        }
    }

    /// Release a caller-cancelled attempt without treating it as evidence of
    /// upstream success or failure, including a cancelled half-open probe.
    pub(crate) fn cancel(&self, permit: CircuitPermit) {
        let mut state = self.state.lock().expect("circuit lock poisoned");
        state.in_flight = state.in_flight.saturating_sub(1);
        if permit.half_open && permit.generation == state.generation {
            state.half_open_in_flight = false;
        }
        if state.failures >= self.failure_threshold
            && state.in_flight == 0
            && state.opened_at.is_none()
        {
            state.opened_at = Some(Instant::now());
        }
    }

    pub fn stream_failure(&self) {
        let mut state = self.state.lock().expect("circuit lock poisoned");
        state.failures = state.failures.saturating_add(1);
        if state.failures >= self.failure_threshold && state.in_flight == 0 {
            state.opened_at = Some(Instant::now());
        }
    }

    #[must_use]
    pub fn state(&self) -> CircuitState {
        let state = self.state.lock().expect("circuit lock poisoned");
        if state.half_open_in_flight {
            CircuitState::HalfOpen
        } else if state.opened_at.is_some() {
            CircuitState::Open
        } else {
            CircuitState::Closed
        }
    }

    #[cfg(test)]
    fn is_open(&self) -> bool {
        self.state() == CircuitState::Open
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::CircuitBreaker;

    #[test]
    fn opens_after_threshold_and_resets_on_success() {
        let breaker = CircuitBreaker::new(2, Duration::ZERO);
        let first = breaker.allow().unwrap();
        breaker.failure(first);
        let second = breaker.allow().unwrap();
        breaker.failure(second);
        assert!(breaker.is_open());
        let probe = breaker.allow().unwrap();
        breaker.success(probe);
        assert!(breaker.allow().is_some());
    }

    #[test]
    fn cancellation_does_not_reset_failures_or_strand_half_open_probe() {
        let breaker = CircuitBreaker::new(1, Duration::ZERO);
        let failed = breaker.allow().unwrap();
        let cancelled = breaker.allow().unwrap();
        breaker.failure(failed);
        breaker.cancel(cancelled);
        assert!(breaker.is_open());
        let probe = breaker.allow().unwrap();
        breaker.cancel(probe);
        assert!(breaker.is_open());
        let next = breaker
            .allow()
            .expect("cancelled probe must not block the next caller");
        breaker.success(next);
        assert!(!breaker.is_open());
    }

    #[test]
    fn concurrent_connection_wave_does_not_open_before_a_success() {
        let breaker = CircuitBreaker::new(2, Duration::from_secs(60));
        let first = breaker.allow().unwrap();
        let second = breaker.allow().unwrap();
        let successful = breaker.allow().unwrap();
        breaker.failure(first);
        breaker.failure(second);
        assert!(!breaker.is_open());
        breaker.success(successful);
        assert!(breaker.allow().is_some());
    }
}
