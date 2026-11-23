//! Asking agents to carry on after their work was interrupted: by a restart of herdr or the
//! computer, or by a usage limit.
//!
//! Pure helpers: the reasons, the message, and reading Codex's usage limit notice. The app
//! decides when to send it (`app::auto_continue`).

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::detect::AgentState;

/// How long an agent may take to come back and settle before herdr stops waiting to send it
/// the message.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Codex's notice can name a reset at most a week away; anything later is a misread.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(8 * 24 * 60 * 60);

/// The notice, the composer and the status line below it fit in this many non-empty lines.
const USAGE_LIMIT_NOTICE_LINES: usize = 12;

/// Boot times read through the uptime can drift by a second or so between calls; a reboot
/// moves them by far more.
const BOOT_TIME_TOLERANCE_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoContinueReason {
    ComputerRestarted,
    HerdrRestarted,
    UsageLimitReset,
}

impl AutoContinueReason {
    /// What `{reason}` stands for in the message.
    pub(crate) fn text(self) -> &'static str {
        match self {
            Self::ComputerRestarted => "the computer restarted",
            Self::HerdrRestarted => "herdr restarted",
            Self::UsageLimitReset => "your usage limit reset",
        }
    }
}

/// The message to send, with `{reason}` filled in. An empty template falls back to the default.
pub(crate) fn auto_continue_message(template: &str, reason: AutoContinueReason) -> String {
    let template = if template.trim().is_empty() {
        crate::config::DEFAULT_AUTO_CONTINUE_MESSAGE
    } else {
        template
    };
    template.replace("{reason}", reason.text())
}

/// When this machine booted, read once per process.
pub(crate) fn current_boot_time() -> Option<u64> {
    static BOOT_TIME: OnceLock<Option<u64>> = OnceLock::new();
    *BOOT_TIME.get_or_init(crate::platform::boot_time_unix_secs)
}

/// Why a session saved by an earlier server is being restored: the computer restarted when
/// the boot time changed, otherwise herdr did.
pub(crate) fn restart_reason(
    saved_boot_time: Option<u64>,
    current_boot_time: Option<u64>,
) -> AutoContinueReason {
    match (saved_boot_time, current_boot_time) {
        (Some(saved), Some(current)) if saved.abs_diff(current) > BOOT_TIME_TOLERANCE_SECS => {
            AutoContinueReason::ComputerRestarted
        }
        _ => AutoContinueReason::HerdrRestarted,
    }
}

/// A message waiting to be sent to one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingAutoContinue {
    pub reason: AutoContinueReason,
    /// Not before this, plus the delay: when a usage limit resets.
    pub not_before: Option<Instant>,
    /// Since when the agent has been sitting idle, ready for a prompt.
    pub ready_since: Option<Instant>,
    pub created_at: Instant,
}

impl PendingAutoContinue {
    pub(crate) fn new(reason: AutoContinueReason, now: Instant) -> Self {
        Self {
            reason,
            not_before: None,
            ready_since: None,
            created_at: now,
        }
    }

    pub(crate) fn after(mut self, not_before: Instant) -> Self {
        self.not_before = Some(not_before);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoContinueStep {
    Wait,
    Send,
    Cancel(&'static str),
}

/// What to do with a pending message, given the agent's state now. `state` is `None` when the
/// pane runs no known agent at the moment, as while a relaunched agent starts; that is waited
/// out like any other state that isn't ready.
///
/// The agent must sit idle for the whole delay (and the delay must have passed since a usage
/// limit reset). Turning to work after it was ready means someone else gave it something to
/// do, so the message is dropped.
pub(crate) fn auto_continue_step(
    pending: &mut PendingAutoContinue,
    state: Option<AgentState>,
    delay: Duration,
    now: Instant,
) -> AutoContinueStep {
    match state {
        Some(AgentState::Idle) => {
            let ready_since = *pending.ready_since.get_or_insert(now);
            let ready_for_long_enough = now.saturating_duration_since(ready_since) >= delay;
            let reset_passed = pending
                .not_before
                .is_none_or(|not_before| now >= not_before + delay);
            if ready_for_long_enough && reset_passed {
                AutoContinueStep::Send
            } else {
                AutoContinueStep::Wait
            }
        }
        Some(AgentState::Working) if pending.ready_since.is_some() => {
            AutoContinueStep::Cancel("the agent started working")
        }
        _ => {
            pending.ready_since = None;
            let waiting_from = pending.not_before.map_or(pending.created_at, |not_before| {
                not_before.max(pending.created_at)
            });
            if now.saturating_duration_since(waiting_from) > SETTLE_TIMEOUT + delay {
                AutoContinueStep::Cancel("the agent never became ready")
            } else {
                AutoContinueStep::Wait
            }
        }
    }
}

/// How long until a retry time read from the screen, from the local time `now`. A bare time
/// that has already passed today means tomorrow.
pub(crate) fn duration_until_retry(
    now: time::PrimitiveDateTime,
    retry: LocalRetryTime,
) -> Option<Duration> {
    let clock = time::Time::from_hms(retry.hour, retry.minute, 0).ok()?;
    let target = match retry.date {
        Some((year, month, day)) => {
            let month = time::Month::try_from(month).ok()?;
            time::PrimitiveDateTime::new(
                time::Date::from_calendar_date(year, month, day).ok()?,
                clock,
            )
        }
        None => {
            let today = time::PrimitiveDateTime::new(now.date(), clock);
            if today > now {
                today
            } else {
                today + time::Duration::days(1)
            }
        }
    };
    let wait = Duration::try_from(target - now).ok()?;
    (wait <= MAX_RETRY_WAIT).then_some(wait)
}

/// A local wall-clock time from an agent's screen. `date` is `(year, month, day)`, or `None`
/// for a time later today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalRetryTime {
    pub date: Option<(i32, u8, u8)>,
    pub hour: u8,
    pub minute: u8,
}

/// Reads when Codex says its usage limit resets, from the last usage limit notice on the
/// screen: "You've hit your usage limit. … Try again at 3:45 PM." for later today, or
/// "… Try again at Nov 27th, 2026 3:45 PM." for a later day.
///
/// Only the bottom of the screen counts, so a notice from an earlier turn that is still in view
/// doesn't schedule another message.
pub(crate) fn codex_usage_limit_retry_at(screen: &str) -> Option<LocalRetryTime> {
    let lines: Vec<&str> = screen
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let tail = &lines[lines.len().saturating_sub(USAGE_LIMIT_NOTICE_LINES)..];
    // The notice can wrap across lines.
    let text = tail
        .iter()
        .flat_map(|line| line.split_whitespace())
        .collect::<Vec<_>>()
        .join(" ");
    let lower = text.to_ascii_lowercase();
    let notice = lower.rfind("hit your usage limit")?;
    let after = notice + lower[notice..].find("try again at ")? + "try again at ".len();
    parse_retry_time(&text[after..])
}

fn parse_retry_time(text: &str) -> Option<LocalRetryTime> {
    let words: Vec<&str> = text.split(' ').collect();
    if let Some((hour, minute)) = parse_clock(words.first()?, words.get(1)?) {
        return Some(LocalRetryTime {
            date: None,
            hour,
            minute,
        });
    }
    let (first, mut words) = (words[0], words[1..].iter().copied());
    let month = month_number(first)?;
    let day = words
        .next()?
        .trim_end_matches(',')
        .trim_end_matches(|ch: char| ch.is_ascii_alphabetic())
        .parse::<u8>()
        .ok()
        .filter(|day| (1..=31).contains(day))?;
    let year = words.next()?.trim_end_matches(',').parse::<i32>().ok()?;
    let (hour, minute) = parse_clock(words.next()?, words.next()?)?;
    Some(LocalRetryTime {
        date: Some((year, month, day)),
        hour,
        minute,
    })
}

/// "3:45" and "PM." as a 24-hour time.
fn parse_clock(clock: &str, meridiem: &str) -> Option<(u8, u8)> {
    let (hour, minute) = clock.split_once(':')?;
    let hour = hour
        .parse::<u8>()
        .ok()
        .filter(|hour| (1..=12).contains(hour))?;
    let minute = minute.parse::<u8>().ok().filter(|minute| *minute < 60)?;
    let hour = match meridiem
        .trim_end_matches(['.', ','])
        .to_ascii_uppercase()
        .as_str()
    {
        "AM" => hour % 12,
        "PM" => hour % 12 + 12,
        _ => return None,
    };
    Some((hour, minute))
}

fn month_number(name: &str) -> Option<u8> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let name = name.to_ascii_lowercase();
    MONTHS
        .iter()
        .position(|month| name.starts_with(month))
        .and_then(|index| u8::try_from(index + 1).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_names_the_reason() {
        assert_eq!(
            auto_continue_message("Carry on ({reason}).", AutoContinueReason::HerdrRestarted),
            "Carry on (herdr restarted)."
        );
        assert_eq!(
            auto_continue_message("", AutoContinueReason::ComputerRestarted),
            "Please continue where you left off. (the computer restarted)"
        );
    }

    #[test]
    fn a_changed_boot_time_means_the_computer_restarted() {
        let reason = |saved, current| restart_reason(saved, current);
        assert_eq!(
            reason(Some(1_000), Some(9_000)),
            AutoContinueReason::ComputerRestarted
        );
        assert_eq!(
            reason(Some(1_000), Some(1_001)),
            AutoContinueReason::HerdrRestarted
        );
        assert_eq!(
            reason(None, Some(1_000)),
            AutoContinueReason::HerdrRestarted
        );
    }

    #[test]
    fn a_message_waits_for_the_agent_to_sit_idle_for_the_delay() {
        let start = Instant::now();
        let delay = Duration::from_secs(60);
        let mut pending = PendingAutoContinue::new(AutoContinueReason::HerdrRestarted, start);
        let at = |secs| start + Duration::from_secs(secs);

        // Still starting: unknown and working states before it settles are waited out.
        assert_eq!(
            auto_continue_step(&mut pending, Some(AgentState::Unknown), delay, at(1)),
            AutoContinueStep::Wait
        );
        assert_eq!(
            auto_continue_step(&mut pending, Some(AgentState::Working), delay, at(2)),
            AutoContinueStep::Wait
        );
        assert_eq!(
            auto_continue_step(&mut pending, Some(AgentState::Idle), delay, at(10)),
            AutoContinueStep::Wait
        );
        // A question on screen restarts the clock.
        assert_eq!(
            auto_continue_step(&mut pending, Some(AgentState::Blocked), delay, at(30)),
            AutoContinueStep::Wait
        );
        assert_eq!(
            auto_continue_step(&mut pending, Some(AgentState::Idle), delay, at(40)),
            AutoContinueStep::Wait
        );
        assert_eq!(
            auto_continue_step(&mut pending, Some(AgentState::Idle), delay, at(99)),
            AutoContinueStep::Wait
        );
        assert_eq!(
            auto_continue_step(&mut pending, Some(AgentState::Idle), delay, at(100)),
            AutoContinueStep::Send
        );
    }

    #[test]
    fn a_message_is_dropped_when_the_agent_gets_other_work_or_never_settles() {
        let start = Instant::now();
        let delay = Duration::from_secs(60);
        let at = |secs| start + Duration::from_secs(secs);

        let mut pending = PendingAutoContinue::new(AutoContinueReason::HerdrRestarted, start);
        auto_continue_step(&mut pending, Some(AgentState::Idle), delay, at(5));
        assert_eq!(
            auto_continue_step(&mut pending, Some(AgentState::Working), delay, at(6)),
            AutoContinueStep::Cancel("the agent started working")
        );

        // While a relaunched agent starts, no agent is known; the wait starts again.
        let mut pending = PendingAutoContinue::new(AutoContinueReason::HerdrRestarted, start);
        auto_continue_step(&mut pending, Some(AgentState::Idle), delay, at(5));
        assert_eq!(
            auto_continue_step(&mut pending, None, delay, at(6)),
            AutoContinueStep::Wait
        );
        assert_eq!(pending.ready_since, None);

        let mut pending = PendingAutoContinue::new(AutoContinueReason::HerdrRestarted, start);
        assert_eq!(
            auto_continue_step(
                &mut pending,
                Some(AgentState::Unknown),
                delay,
                at(16 * 60 + 1)
            ),
            AutoContinueStep::Cancel("the agent never became ready")
        );
    }

    #[test]
    fn a_usage_limit_message_waits_for_the_reset_and_the_delay() {
        let start = Instant::now();
        let delay = Duration::from_secs(60);
        let at = |secs| start + Duration::from_secs(secs);
        let mut pending =
            PendingAutoContinue::new(AutoContinueReason::UsageLimitReset, start).after(at(3600));

        assert_eq!(
            auto_continue_step(&mut pending, Some(AgentState::Idle), delay, at(1)),
            AutoContinueStep::Wait
        );
        assert_eq!(
            auto_continue_step(&mut pending, Some(AgentState::Idle), delay, at(3659)),
            AutoContinueStep::Wait
        );
        assert_eq!(
            auto_continue_step(&mut pending, Some(AgentState::Idle), delay, at(3660)),
            AutoContinueStep::Send
        );
    }

    #[test]
    fn retry_times_become_waits_from_now() {
        let now = time::PrimitiveDateTime::new(
            time::Date::from_calendar_date(2026, time::Month::November, 23).expect("date"),
            time::Time::from_hms(14, 0, 0).expect("time"),
        );
        let later_today = LocalRetryTime {
            date: None,
            hour: 15,
            minute: 45,
        };
        assert_eq!(
            duration_until_retry(now, later_today),
            Some(Duration::from_secs(105 * 60))
        );
        let tomorrow = LocalRetryTime {
            date: None,
            hour: 9,
            minute: 0,
        };
        assert_eq!(
            duration_until_retry(now, tomorrow),
            Some(Duration::from_secs(19 * 3600))
        );
        let dated = LocalRetryTime {
            date: Some((2026, 11, 25)),
            hour: 14,
            minute: 0,
        };
        assert_eq!(
            duration_until_retry(now, dated),
            Some(Duration::from_secs(48 * 3600))
        );
        let past = LocalRetryTime {
            date: Some((2026, 11, 1)),
            hour: 14,
            minute: 0,
        };
        assert_eq!(duration_until_retry(now, past), None);
        let far = LocalRetryTime {
            date: Some((2027, 1, 1)),
            hour: 14,
            minute: 0,
        };
        assert_eq!(duration_until_retry(now, far), None);
    }

    #[test]
    fn codex_retry_times_are_read_for_today_and_later_days() {
        let today = LocalRetryTime {
            date: None,
            hour: 15,
            minute: 45,
        };
        assert_eq!(
            codex_usage_limit_retry_at(
                "■ You've hit your usage limit. Upgrade to Pro, or try again at 3:45 PM."
            ),
            Some(today)
        );
        assert_eq!(
            codex_usage_limit_retry_at(
                "■ You've hit your usage limit. Visit the usage page to purchase more\n  credits or try again at Nov 27th, 2026\n  12:05 AM."
            ),
            Some(LocalRetryTime {
                date: Some((2026, 11, 27)),
                hour: 0,
                minute: 5,
            })
        );
        assert_eq!(
            codex_usage_limit_retry_at("■ You've hit your usage limit. Try again later."),
            None
        );
        assert_eq!(codex_usage_limit_retry_at("Try again at 3:45 PM."), None);
        let scrolled_up = format!(
            "■ You've hit your usage limit. Try again at 3:45 PM.\n{}",
            "› later work\n".repeat(12)
        );
        assert_eq!(codex_usage_limit_retry_at(&scrolled_up), None);
    }
}
