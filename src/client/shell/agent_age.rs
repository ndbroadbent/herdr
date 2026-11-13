//! How long ago an agent's state last changed, as the sidebar's `changed` token shows it: `0m`
//! for the first minute, then whole minutes, hours, days, weeks, months and years.

const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;
const WEEK: u64 = 7 * DAY;
const MONTH: u64 = 30 * DAY;
const YEAR: u64 = 365 * DAY;

/// A done agent's dot turns pink for this long after it finished.
pub(super) const RECENT_MS: u64 = MINUTE;

/// Each range: where it ends, the unit it counts in, and that unit's suffix.
const RANGES: [(u64, u64, &str); 6] = [
    (HOUR, MINUTE, "m"),
    (DAY, HOUR, "h"),
    (WEEK, DAY, "d"),
    (MONTH, WEEK, "w"),
    (YEAR, MONTH, "mo"),
    (u64::MAX, YEAR, "y"),
];

fn range(age_ms: u64) -> (u64, u64, &'static str) {
    RANGES
        .into_iter()
        .find(|(end, _, _)| age_ms < *end)
        .unwrap_or(RANGES[RANGES.len() - 1])
}

impl super::ClientShellState {
    /// Repaint once the soonest visible age label or recent dot is due to change.
    pub(crate) fn tick_agent_ages(&self) -> bool {
        self.hits
            .agent_age_repaint_at_unix_ms
            .is_some_and(|at| now_unix_ms() >= at)
    }
}

pub(super) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

pub(super) fn label(age_ms: u64) -> String {
    let (_, unit, suffix) = range(age_ms);
    format!("{}{suffix}", age_ms / unit)
}

/// How much older the agent must get before its label next changes.
pub(super) fn until_next_label(age_ms: u64) -> u64 {
    let (end, unit, _) = range(age_ms);
    let next = (age_ms / unit + 1).saturating_mul(unit).min(end);
    next - age_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_count_whole_units_and_start_at_zero_minutes() {
        for (age, expected) in [
            (0, "0m"),
            (59_999, "0m"),
            (MINUTE, "1m"),
            (4 * MINUTE + 59_999, "4m"),
            (59 * MINUTE, "59m"),
            (HOUR, "1h"),
            (23 * HOUR + 59 * MINUTE, "23h"),
            (DAY, "1d"),
            (6 * DAY, "6d"),
            (WEEK, "1w"),
            (29 * DAY, "4w"),
            (MONTH, "1mo"),
            (2 * MONTH, "2mo"),
            (YEAR - 1, "12mo"),
            (YEAR, "1y"),
            (3 * YEAR, "3y"),
        ] {
            assert_eq!(label(age), expected, "age {age}");
        }
    }

    #[test]
    fn the_next_change_lands_exactly_where_the_label_changes() {
        for age in [
            0,
            1,
            59_999,
            MINUTE,
            59 * MINUTE + 1,
            HOUR + 5,
            DAY - 1,
            6 * DAY + 7,
            29 * DAY,
            MONTH - 1,
            YEAR - 1,
            YEAR,
        ] {
            let wait = until_next_label(age);
            assert!(wait > 0, "age {age}");
            assert_eq!(label(age + wait - 1), label(age), "age {age}");
            assert_ne!(label(age + wait), label(age), "age {age}");
        }
    }
}
