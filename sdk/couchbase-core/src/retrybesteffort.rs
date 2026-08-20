/*
 *
 *  * Copyright (c) 2025 Couchbase, Inc.
 *  *
 *  * Licensed under the Apache License, Version 2.0 (the "License");
 *  * you may not use this file except in compliance with the License.
 *  * You may obtain a copy of the License at
 *  *
 *  *    http://www.apache.org/licenses/LICENSE-2.0
 *  *
 *  * Unless required by applicable law or agreed to in writing, software
 *  * distributed under the License is distributed on an "AS IS" BASIS,
 *  * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *  * See the License for the specific language governing permissions and
 *  * limitations under the License.
 *
 */

use std::fmt::Debug;
use std::time::Duration;

use crate::retry::{RetryAction, RetryReason, RetryRequest, RetryStrategy};

/// A retry strategy that retries all eligible operations using a configurable
/// backoff calculator.
///
/// This is the default retry strategy used by the SDK. It retries idempotent
/// operations unconditionally, and non-idempotent operations only when the
/// [`RetryReason`] indicates it is safe to do so.
///
/// The backoff duration between retries is determined by the [`BackoffCalculator`]
/// provided at construction time. If none is specified, an
/// [`ExponentialBackoffCalculator`] with sensible defaults is used.
///
/// # Example
///
/// ```rust
/// use couchbase_core::retrybesteffort::BestEffortRetryStrategy;
/// use std::sync::Arc;
///
/// // Use the default exponential backoff:
/// let strategy = Arc::new(BestEffortRetryStrategy::default());
/// ```
#[derive(Debug, Clone)]
pub struct BestEffortRetryStrategy<Calc> {
    backoff_calc: Calc,
}

impl<Calc> BestEffortRetryStrategy<Calc>
where
    Calc: BackoffCalculator,
{
    /// Creates a new `BestEffortRetryStrategy` with the given backoff calculator.
    pub fn new(calc: Calc) -> Self {
        Self { backoff_calc: calc }
    }
}

impl Default for BestEffortRetryStrategy<ExponentialBackoffCalculator> {
    fn default() -> Self {
        Self::new(ExponentialBackoffCalculator::default())
    }
}

impl<Calc> RetryStrategy for BestEffortRetryStrategy<Calc>
where
    Calc: BackoffCalculator,
{
    fn retry_after(&self, request: &RetryRequest, reason: &RetryReason) -> Option<RetryAction> {
        if request.is_idempotent() || reason.allows_non_idempotent_retry() {
            Some(RetryAction::new(
                self.backoff_calc.backoff(request.retry_attempts()),
            ))
        } else {
            None
        }
    }
}

/// A calculator that determines the backoff duration between retry attempts.
///
/// Implement this trait to provide custom backoff logic. The SDK ships with
/// [`ExponentialBackoffCalculator`] as the default implementation.
pub trait BackoffCalculator: Debug + Send + Sync {
    /// Returns the duration to wait before the given retry attempt number.
    ///
    /// `retry_attempts` starts at 0 for the first retry.
    fn backoff(&self, retry_attempts: u32) -> Duration;
}

/// An exponential backoff calculator that increases the delay between retries
/// exponentially, clamped between a minimum and maximum duration.
///
/// The backoff for attempt `n` is calculated as:
///
/// ```text
/// clamp(min * backoff_factor ^ n, min, max)
/// ```
///
/// # Defaults
///
/// | Parameter | Default |
/// |-----------|---------|
/// | `min` | 1 ms |
/// | `max` | 1000 ms |
/// | `backoff_factor` | 2.0 |
///
/// # Example
///
/// ```rust
/// use couchbase_core::retrybesteffort::{BestEffortRetryStrategy, ExponentialBackoffCalculator};
/// use std::time::Duration;
/// use std::sync::Arc;
///
/// let calc = ExponentialBackoffCalculator::new(
///     Duration::from_millis(5),   // min
///     Duration::from_millis(500), // max
///     2.0,                        // backoff_factor
/// );
/// let strategy = Arc::new(BestEffortRetryStrategy::new(calc));
/// ```
#[derive(Clone, Debug)]
pub struct ExponentialBackoffCalculator {
    min: Duration,
    max: Duration,
    backoff_factor: f64,
}

impl ExponentialBackoffCalculator {
    /// Creates a new `ExponentialBackoffCalculator`.
    ///
    /// * `min` — Minimum backoff duration (floor).
    /// * `max` — Maximum backoff duration (ceiling).
    /// * `backoff_factor` — Multiplier applied per retry attempt (typically `2.0`).
    pub fn new(min: Duration, max: Duration, backoff_factor: f64) -> Self {
        Self {
            min,
            max,
            backoff_factor,
        }
    }
}

impl BackoffCalculator for ExponentialBackoffCalculator {
    fn backoff(&self, retry_attempts: u32) -> Duration {
        let factor = self.backoff_factor.powi(retry_attempts as i32);

        // Multiply before narrowing. Casting the factor to an integer first
        // truncated it, so the 1.5 every caller passes became 1, 1, 2, 3, 5, 7,
        // 11 -- a schedule that is neither the documented one above nor
        // monotonic in the factor: 1.5 and 1.9 produced identical backoffs.
        let millis = self.min.as_millis() as f64 * factor;

        if !millis.is_finite() || millis >= u64::MAX as f64 {
            return self.max;
        }

        let mut backoff = Duration::from_millis(millis as u64);

        if backoff > self.max {
            backoff = self.max;
        }
        if backoff < self.min {
            backoff = self.min
        }

        backoff
    }
}

impl Default for ExponentialBackoffCalculator {
    fn default() -> Self {
        Self {
            min: Duration::from_millis(1),
            max: Duration::from_millis(1000),
            backoff_factor: 2.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exponential_backoff() {
        let calculator = ExponentialBackoffCalculator::new(
            Duration::from_millis(10),
            Duration::from_millis(1000),
            2.0,
        );

        assert_eq!(calculator.backoff(0), Duration::from_millis(10));
        assert_eq!(calculator.backoff(1), Duration::from_millis(20));
        assert_eq!(calculator.backoff(2), Duration::from_millis(40));
        assert_eq!(calculator.backoff(3), Duration::from_millis(80));
        assert_eq!(calculator.backoff(4), Duration::from_millis(160));
        assert_eq!(calculator.backoff(5), Duration::from_millis(320));
        assert_eq!(calculator.backoff(6), Duration::from_millis(640));
        assert_eq!(calculator.backoff(7), Duration::from_millis(1000));
    }

    /// 1.5 is the factor every `ensure_*` poll in the crate passes, and it is
    /// the case the 2.0 test above cannot see: truncating the factor to an
    /// integer turned this schedule into 100/100/200/300/500/700.
    #[test]
    fn a_fractional_factor_is_not_truncated() {
        let calculator = ExponentialBackoffCalculator::new(
            Duration::from_millis(100),
            Duration::from_millis(1000),
            1.5,
        );

        assert_eq!(calculator.backoff(0), Duration::from_millis(100));
        assert_eq!(calculator.backoff(1), Duration::from_millis(150));
        assert_eq!(calculator.backoff(2), Duration::from_millis(225));
        assert_eq!(calculator.backoff(3), Duration::from_millis(337));
        assert_eq!(calculator.backoff(4), Duration::from_millis(506));
        assert_eq!(calculator.backoff(5), Duration::from_millis(759));
        // 1139ms, past the ceiling.
        assert_eq!(calculator.backoff(6), Duration::from_millis(1000));
    }

    /// Truncation also made the calculator insensitive to its own argument:
    /// every factor in [1.5, 2.0) produced the same schedule.
    #[test]
    fn two_different_factors_give_two_different_schedules() {
        let gentle = ExponentialBackoffCalculator::new(
            Duration::from_millis(100),
            Duration::from_secs(10),
            1.5,
        );
        let steep = ExponentialBackoffCalculator::new(
            Duration::from_millis(100),
            Duration::from_secs(10),
            1.9,
        );

        assert_ne!(gentle.backoff(3), steep.backoff(3));
    }

    #[test]
    fn test_exponential_backoff_overflows_u128() {
        let calculator = ExponentialBackoffCalculator::new(
            Duration::from_millis(100),
            Duration::from_millis(1000),
            1.5,
        );

        assert_eq!(calculator.backoff(208), Duration::from_millis(1000));
    }

    #[test]
    fn test_exponential_backoff_overflows_u64() {
        let calculator = ExponentialBackoffCalculator::new(
            Duration::from_millis(100),
            Duration::from_millis(1000),
            1.5,
        );

        assert_eq!(calculator.backoff(207), Duration::from_millis(1000));
    }
}
