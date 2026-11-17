//! Time and the model residency window (ADR-0010, ADR-0015).

use chrono::{DateTime, Duration, Local, NaiveTime, TimeZone, Utc};

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Local>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Local> {
        Local::now()
    }
}

/// A fixed clock for tests.
pub struct FixedClock(pub DateTime<Local>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Local> {
        self.0
    }
}

/// Daily window [on, off) in local time during which the model is resident ("day"). Outside it is
/// "night". `on == off` means always on. A window may wrap midnight (on > off).
#[derive(Clone, Copy, Debug)]
pub struct ModelHours {
    pub on: NaiveTime,
    pub off: NaiveTime,
}

impl ModelHours {
    pub fn is_on(&self, now: DateTime<Local>) -> bool {
        self.contains(now.time())
    }

    /// Whether a time of day falls inside the window.
    pub fn contains(&self, t: NaiveTime) -> bool {
        if self.on == self.off {
            return true;
        }
        if self.on < self.off {
            t >= self.on && t < self.off
        } else {
            t >= self.on || t < self.off
        }
    }

    /// The next moment the window opens (now if it is already open).
    pub fn next_on(&self, now: DateTime<Local>) -> DateTime<Utc> {
        if self.is_on(now) {
            return now.with_timezone(&Utc);
        }
        let today = now.date_naive();
        for day in 0..2 {
            let date = today + Duration::days(day);
            if let Some(candidate) = Local
                .from_local_datetime(&date.and_time(self.on))
                .earliest()
                && candidate > now
            {
                return candidate.with_timezone(&Utc);
            }
        }
        (now + Duration::hours(1)).with_timezone(&Utc)
    }
}

/// The latest local occurrence of time-of-day `t` at or before `now` (today's, or yesterday's if
/// today's is still ahead). Used by the scheduler so a time missed during sleep fires once on wake.
pub fn latest_occurrence(t: NaiveTime, now: DateTime<Local>) -> DateTime<Local> {
    for back in 0..3 {
        let date = now.date_naive() - Duration::days(back);
        if let Some(c) = Local.from_local_datetime(&date.and_time(t)).earliest()
            && c <= now
        {
            return c;
        }
    }
    now
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(h: u32, m: u32) -> DateTime<Local> {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 11, 16)
            .unwrap()
            .and_hms_opt(h, m, 0)
            .unwrap();
        Local.from_local_datetime(&d).earliest().unwrap()
    }

    fn hours(on: (u32, u32), off: (u32, u32)) -> ModelHours {
        ModelHours {
            on: NaiveTime::from_hms_opt(on.0, on.1, 0).unwrap(),
            off: NaiveTime::from_hms_opt(off.0, off.1, 0).unwrap(),
        }
    }

    #[test]
    fn day_window() {
        let h = hours((9, 0), (19, 0));
        assert!(!h.is_on(at(8, 59)));
        assert!(h.is_on(at(9, 0)));
        assert!(h.is_on(at(18, 59)));
        assert!(!h.is_on(at(19, 0)));
        assert_eq!(
            h.next_on(at(20, 0)),
            at(9, 0).with_timezone(&Utc) + Duration::days(1)
        );
        assert_eq!(h.next_on(at(7, 0)), at(9, 0).with_timezone(&Utc));
        assert_eq!(h.next_on(at(10, 0)), at(10, 0).with_timezone(&Utc));
    }

    #[test]
    fn always_on_and_wrapping() {
        assert!(hours((0, 0), (0, 0)).is_on(at(3, 0)));
        let night = hours((22, 0), (6, 0));
        assert!(night.is_on(at(23, 0)) && night.is_on(at(5, 0)) && !night.is_on(at(12, 0)));
    }
}
