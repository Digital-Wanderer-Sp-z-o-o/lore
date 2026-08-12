// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

pub struct Retry {
    current: u64,
    maximum: u64,
    jitter: f32,
    counter: usize,
    limit: usize,
}

impl Retry {
    pub async fn wait(&mut self) -> bool {
        if self.counter >= self.limit {
            return false;
        }

        // Keep jitter proportional to the exponential delay. Capping it at a
        // small fixed value makes many clients line up again once the backoff
        // grows, causing a new connection storm on every retry wave.
        let jitter = retry_jitter(self.current, self.jitter, rand::random::<f32>());

        tokio::time::sleep(Duration::from_millis(self.current + jitter)).await;

        self.current = std::cmp::min(self.current * 2, self.maximum);
        self.counter += 1;

        true
    }

    pub fn counter(&self) -> usize {
        self.counter
    }

    pub fn limit(&self) -> usize {
        self.limit
    }
}

fn retry_jitter(current: u64, jitter_ratio: f32, random_fraction: f32) -> u64 {
    (current as f64 * jitter_ratio as f64 * random_fraction.clamp(0.0, 1.0) as f64) as u64
}

const DEFAULT_JITTER: f32 = 0.1;

/// Create a retry waiter, start and maximum times in milliseconds. Will give up
/// after trying for the limit number of times.
pub fn retry(start: u64, maximum: u64, limit: usize) -> Retry {
    Retry {
        current: start,
        maximum,
        jitter: DEFAULT_JITTER,
        counter: 0,
        limit,
    }
}

#[cfg(test)]
mod tests {
    use super::retry_jitter;

    #[test]
    fn jitter_scales_with_current_backoff() {
        assert_eq!(retry_jitter(1_000, 0.1, 1.0), 100);
        assert_eq!(retry_jitter(30_000, 0.1, 1.0), 3_000);
    }

    #[test]
    fn jitter_fraction_is_bounded() {
        assert_eq!(retry_jitter(1_000, 0.1, -1.0), 0);
        assert_eq!(retry_jitter(1_000, 0.1, 2.0), 100);
    }
}
