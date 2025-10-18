// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::{Duration, Instant};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct TokenBucket {
    last_refill_time: Instant,
    tokens: f64,
    fill_rate: f64, // tokens per second
    capacity: f64,
}

impl TokenBucket {
    pub fn new(capacity: f64, fill_rate: f64) -> Self {
        TokenBucket {
            last_refill_time: Instant::now(),
            tokens: capacity, // Start with a full bucket
            fill_rate,
            capacity,
        }
    }

    pub fn consume(&mut self, tokens_to_consume: f64) -> bool {
        self.refill();

        if self.tokens >= tokens_to_consume {
            self.tokens -= tokens_to_consume;
            true
        } else {
            false
        }
    }

    pub fn set_rate(&mut self, new_rate_bps: f64) {
        self.fill_rate = new_rate_bps;
        self.capacity = new_rate_bps;
        self.tokens = new_rate_bps;
        self.last_refill_time = Instant::now();
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill_time);
        self.last_refill_time = now;

        let tokens_to_add = elapsed.as_secs_f64() * self.fill_rate;
        self.tokens = f64::min(self.capacity, self.tokens + tokens_to_add);
    }

    pub fn get_tokens(&self) -> f64 {
        self.tokens
    }
}

pub async fn consume_tokens(bucket_arc: &Arc<Mutex<TokenBucket>>, amount_bytes: f64) {
    let rate_bps = { bucket_arc.lock().await.fill_rate };
    if rate_bps == 0.0 {
        return; // Unlimited
    }

    // If the request is larger than the bucket, sleep for the average time required
    // This prevents a single large request from hogging the bucket's refill logic
    let capacity = { bucket_arc.lock().await.capacity };
    if amount_bytes > capacity {
        let required_duration =
            Duration::from_secs_f64((amount_bytes * 8.0) / rate_bps);
        tokio::time::sleep(required_duration).await;
        return;
    }

    loop {
        let wait_time = {
            let mut bucket = bucket_arc.lock().await;
            bucket.refill();

            if bucket.tokens >= amount_bytes {
                bucket.tokens -= amount_bytes;
                break; // Success! Exit the loop.
            }

            // Not enough tokens, calculate wait time and release the lock
            let tokens_needed = amount_bytes - bucket.tokens;
            Duration::from_secs_f64(((tokens_needed * 8.0) / rate_bps).max(0.001))
        }; // The lock on `bucket` is dropped here

        // Sleep for the calculated duration WITHOUT holding the lock
        tokio::time::sleep(wait_time).await;
    }
}
