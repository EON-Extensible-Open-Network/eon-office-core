// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Hybrid logical clock (Kulkarni et al., 2014).
//!
//! Every document write is stamped. Stamps from two devices compare the way
//! people expect -- later edits win -- even when one device's wall clock is
//! wrong, because a stamp never goes backwards and every stamp a device has
//! *seen* pushes its own clock past it.
//!
//! A stamp is a string that sorts correctly as bytes:
//!
//! ```text
//! 0001763539200000-0000-3f2a9c41d07e88b1
//! └ wall ms, 16 dig ┘ └ctr┘ └ node id ──┘
//! ```
//!
//! The node id breaks ties, so two devices can never produce the same stamp.

use crate::error::{Error, Result};

const MAX_COUNTER: u32 = 0xffff;
/// A stamp more than a day ahead of this machine's clock is refused rather
/// than adopted: adopting it would drag every later local write into the
/// future with it.
const MAX_DRIFT_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone)]
pub struct Clock {
    node: String,
    last_ms: u64,
    counter: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Parts {
    pub ms: u64,
    pub counter: u32,
}

impl Clock {
    #[must_use]
    pub fn new(node: String) -> Self {
        Clock {
            node,
            last_ms: 0,
            counter: 0,
        }
    }

    #[must_use]
    pub fn node(&self) -> &str {
        &self.node
    }

    /// A stamp for a local event at wall time `wall_ms`.
    pub fn tick(&mut self, wall_ms: u64) -> String {
        if wall_ms > self.last_ms {
            self.last_ms = wall_ms;
            self.counter = 0;
        } else if self.counter >= MAX_COUNTER {
            // 65,536 events inside one millisecond: borrow the next one.
            self.last_ms += 1;
            self.counter = 0;
        } else {
            self.counter += 1;
        }
        format(self.last_ms, self.counter, &self.node)
    }

    /// Fold in a stamp received from elsewhere, so the next local stamp is
    /// later than it.
    pub fn observe(&mut self, stamp: &str, wall_ms: u64) -> Result<()> {
        let (parts, _) = parse(stamp)?;
        if parts.ms > wall_ms.saturating_add(MAX_DRIFT_MS) {
            return Err(Error::params(
                "a change is stamped more than a day in the future; refusing to adopt its clock",
            ));
        }
        let previous = Parts {
            ms: self.last_ms,
            counter: self.counter,
        };
        let latest = previous.max(parts);
        if wall_ms > latest.ms {
            self.last_ms = wall_ms;
            self.counter = 0;
        } else {
            self.last_ms = latest.ms;
            self.counter = latest.counter;
        }
        Ok(())
    }
}

#[must_use]
pub fn format(ms: u64, counter: u32, node: &str) -> String {
    format!("{ms:016}-{counter:04x}-{node}")
}

pub fn parse(stamp: &str) -> Result<(Parts, &str)> {
    let bad = || Error::params("not a clock stamp");
    let mut fields = stamp.splitn(3, '-');
    let (Some(ms), Some(counter), Some(node)) = (fields.next(), fields.next(), fields.next())
    else {
        return Err(bad());
    };
    if ms.len() != 16 || counter.len() != 4 || node.is_empty() || node.len() > 32 {
        return Err(bad());
    }
    if !node.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad());
    }
    let ms = ms.parse::<u64>().map_err(|_| bad())?;
    let counter = u32::from_str_radix(counter, 16).map_err(|_| bad())?;
    Ok((Parts { ms, counter }, node))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_never_go_backwards_even_when_the_wall_clock_does() {
        let mut clock = Clock::new("aa".into());
        let a = clock.tick(1_000);
        let b = clock.tick(500); // the wall clock jumped back
        let c = clock.tick(500);
        assert!(a < b && b < c, "{a} {b} {c}");
    }

    #[test]
    fn observing_a_later_stamp_moves_the_clock_past_it() {
        let mut slow = Clock::new("01".into());
        let mut fast = Clock::new("02".into());
        let remote = fast.tick(10_000);
        slow.observe(&remote, 2_000).ok();
        let local = slow.tick(2_000);
        assert!(local > remote, "{local} should sort after {remote}");
    }

    #[test]
    fn a_stamp_far_in_the_future_is_refused() {
        let mut clock = Clock::new("01".into());
        let far = format(10 * MAX_DRIFT_MS, 0, "02");
        assert!(clock.observe(&far, 1_000).is_err());
    }

    #[test]
    fn stamps_round_trip_and_ties_break_on_the_node() {
        let a = format(5, 1, "0a");
        let b = format(5, 1, "0b");
        assert!(a < b);
        let (parts, node) = parse(&a).ok().unwrap_or((Parts { ms: 0, counter: 0 }, ""));
        assert_eq!((parts.ms, parts.counter, node), (5, 1, "0a"));
        assert!(parse("garbage").is_err());
        assert!(parse("12-0000-zz").is_err());
    }

    #[test]
    fn the_counter_overflows_into_the_next_millisecond() {
        let mut clock = Clock::new("01".into());
        let mut last = clock.tick(7);
        for _ in 0..70_000 {
            let next = clock.tick(7);
            assert!(next > last);
            last = next;
        }
    }
}
