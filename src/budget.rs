//! Guessing when a key's monthly budget resets.
//!
//! A refill is only visible when a key had run out of credits and later works again, so
//! each refill is known to have happened inside a window: after the last failed request or
//! credit check, and before the first success. Budgets reset monthly on the key's own day,
//! so the reset is projected onto the same day of the month (and time of day, UTC) in
//! later months, clamped to short months.

/// One observed refill: it happened after `after_ms` (if known) and by `at_ms`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Refill {
    pub after_ms: Option<i64>,
    pub at_ms: i64,
}

/// The best guess at a key's monthly reset moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResetGuess {
    /// A past reset moment; later resets fall on the same day of the month and time of day.
    pub anchor_ms: i64,
    /// Width of the refill window the guess came from (the reset is within ± half of it).
    pub window_ms: Option<i64>,
    /// How many of the recent refills fall on the same day of the month.
    pub seen: u32,
}

const DAY_MS: i64 = 86_400_000;
/// Refills this close to the same monthly moment count as the same reset day.
const SAME_DAY_TOLERANCE_MS: i64 = 2 * DAY_MS;
/// Only the most recent refills are considered: budgets and reset days can change.
const RECENT: usize = 4;

impl Refill {
    fn mid(&self) -> i64 {
        match self.after_ms {
            Some(a) if a < self.at_ms => a + (self.at_ms - a) / 2,
            _ => self.at_ms,
        }
    }
    fn window(&self) -> Option<i64> {
        self.after_ms.map(|a| (self.at_ms - a).max(0))
    }
}

/// Guess the reset moment from refills in time order. The latest refill sets it; earlier
/// refills that land on about the same day of the month confirm it, and one whose narrower
/// window, moved to the latest refill's month, falls inside the latest window sharpens it.
pub fn guess(refills: &[Refill]) -> Option<ResetGuess> {
    let latest = *refills.last()?;
    let (ly, lm, _) = civil(latest.mid());
    let recent = &refills[refills.len().saturating_sub(RECENT)..refills.len() - 1];
    let mut g = ResetGuess { anchor_ms: latest.mid(), window_ms: latest.window(), seen: 1 };
    for r in recent {
        let moved = in_month(r.mid(), ly, lm);
        if (moved - latest.mid()).abs() > SAME_DAY_TOLERANCE_MS {
            continue;
        }
        g.seen += 1;
        let inside = latest.after_ms.map_or(false, |a| a <= moved && moved <= latest.at_ms);
        if inside && r.window().map_or(false, |w| w < g.window_ms.unwrap_or(i64::MAX)) {
            g.anchor_ms = moved;
            g.window_ms = r.window();
        }
    }
    Some(g)
}

/// The reset at or before `now` and the next one after it.
pub fn cycle(anchor_ms: i64, now: i64) -> (i64, i64) {
    let (y, m, _) = civil(now);
    let this = in_month(anchor_ms, y, m);
    if this <= now {
        let (ny, nm) = add_months(y, m, 1);
        (this, in_month(anchor_ms, ny, nm))
    } else {
        let (py, pm) = add_months(y, m, -1);
        (in_month(anchor_ms, py, pm), this)
    }
}

/// `anchor`'s day of the month and time of day, moved to month `m` of year `y`.
fn in_month(anchor_ms: i64, y: i64, m: u32) -> i64 {
    let (_, _, d) = civil(anchor_ms);
    let time_of_day = anchor_ms.rem_euclid(DAY_MS);
    let day = d.min(days_in_month(y, m));
    days_from_civil(y, m, day) * DAY_MS + time_of_day
}

fn add_months(y: i64, m: u32, delta: i64) -> (i64, u32) {
    let idx = y * 12 + (m as i64 - 1) + delta;
    (idx.div_euclid(12), (idx.rem_euclid(12) + 1) as u32)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// (year, month, day) in UTC for a unix time in ms.
fn civil(ms: i64) -> (i64, u32, u32) {
    // Howard Hinnant's civil_from_days.
    let z = ms.div_euclid(DAY_MS) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i64, m: u32, d: u32, h: i64) -> i64 {
        days_from_civil(y, m, d) * DAY_MS + h * 3_600_000
    }

    #[test]
    fn civil_round_trips() {
        for &(y, m, d) in &[(1970, 1, 1), (2000, 2, 29), (2026, 11, 23), (2027, 12, 31), (1969, 12, 31)] {
            assert_eq!(civil(days_from_civil(y, m, d) * DAY_MS + 5), (y, m, d));
        }
    }

    #[test]
    fn next_reset_is_the_same_day_next_month() {
        let anchor = at(2026, 9, 14, 3);
        assert_eq!(cycle(anchor, at(2026, 11, 20, 0)), (at(2026, 11, 14, 3), at(2026, 12, 14, 3)));
        assert_eq!(cycle(anchor, at(2026, 11, 10, 0)), (at(2026, 10, 14, 3), at(2026, 11, 14, 3)));
        assert_eq!(cycle(anchor, at(2026, 12, 20, 0)), (at(2026, 12, 14, 3), at(2027, 1, 14, 3)));
    }

    #[test]
    fn day_31_clamps_to_short_months() {
        let anchor = at(2026, 1, 31, 0);
        assert_eq!(cycle(anchor, at(2026, 2, 10, 0)), (at(2026, 1, 31, 0), at(2026, 2, 28, 0)));
        assert_eq!(cycle(anchor, at(2026, 3, 1, 0)), (at(2026, 2, 28, 0), at(2026, 3, 31, 0)));
    }

    #[test]
    fn guess_uses_the_narrowest_agreeing_window() {
        let refills = [
            Refill { after_ms: Some(at(2026, 9, 14, 0)), at_ms: at(2026, 9, 14, 2) }, // 2h window
            Refill { after_ms: Some(at(2026, 10, 13, 12)), at_ms: at(2026, 10, 14, 12) }, // 24h window
            Refill { after_ms: Some(at(2026, 11, 13, 20)), at_ms: at(2026, 11, 14, 8) },
        ];
        let g = guess(&refills).unwrap();
        assert_eq!(g.seen, 3);
        assert_eq!(g.anchor_ms, at(2026, 11, 14, 1));
        assert_eq!(g.window_ms, Some(2 * 3_600_000));
    }

    #[test]
    fn an_older_window_outside_the_latest_only_confirms() {
        // Reset a day earlier this month than last: the narrow old window doesn't move the guess.
        let refills = [
            Refill { after_ms: Some(at(2026, 10, 14, 0)), at_ms: at(2026, 10, 14, 1) },
            Refill { after_ms: Some(at(2026, 11, 13, 0)), at_ms: at(2026, 11, 13, 4) },
        ];
        let g = guess(&refills).unwrap();
        assert_eq!(g.seen, 2);
        assert_eq!(g.anchor_ms, at(2026, 11, 13, 2));
        assert_eq!(g.window_ms, Some(4 * 3_600_000));
    }

    #[test]
    fn a_refill_on_another_day_starts_over() {
        let refills = [
            Refill { after_ms: Some(at(2026, 9, 14, 0)), at_ms: at(2026, 9, 14, 2) },
            Refill { after_ms: Some(at(2026, 11, 2, 0)), at_ms: at(2026, 11, 2, 6) },
        ];
        let g = guess(&refills).unwrap();
        assert_eq!(g.seen, 1);
        assert_eq!(g.anchor_ms, at(2026, 11, 2, 3));
        assert!(guess(&[]).is_none());
    }
}
