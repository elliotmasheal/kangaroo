//! Pure domain model: the Self aggregate, the UserProfile aggregate,
//! Given-amendment negotiation, and self-amendment authority.
//!
//! Zero infrastructure dependencies: no persistence, no file IO, no async
//! runtime, no logging — only domain vocabulary and rules. Persistence DTOs
//! live in `infra`; serialization never leaks into this layer (guarded by
//! `tests/domain_purity.rs`).

pub mod amendment;
pub mod orchestration;
pub mod profile;
pub mod selfhood;

/// Why a revision was written. Shared audit vocabulary across both documents
/// and the negotiation log. Persistence maps these through the string forms
/// below; the vocabulary itself stays serialization-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// Identity initialization (the first Given write).
    Initialize,
    /// Experience-driven cognition (Earned solidification, profile facts).
    Experience,
    /// The user's hand edit of the UserProfile document.
    UserExplicitEdit,
    /// A Given change accepted through negotiation.
    Negotiated,
    /// A rollback to an earlier revision.
    Rollback,
    /// A document restored after a detected external edit or deletion.
    Recovered,
    /// An experience-driven Self amendment (Earned traits only).
    Amendment,
    /// A developer-driven Self amendment; only reachable in builds compiled
    /// with the `dev-self-amendment` feature.
    DeveloperAmendment,
}

impl ChangeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::Experience => "experience",
            Self::UserExplicitEdit => "user_explicit_edit",
            Self::Negotiated => "negotiated",
            Self::Rollback => "rollback",
            Self::Recovered => "recovered",
            Self::Amendment => "amendment",
            Self::DeveloperAmendment => "developer_amendment",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "initialize" => Self::Initialize,
            "experience" => Self::Experience,
            "user_explicit_edit" => Self::UserExplicitEdit,
            "negotiated" => Self::Negotiated,
            "rollback" => Self::Rollback,
            "recovered" => Self::Recovered,
            "amendment" => Self::Amendment,
            "developer_amendment" => Self::DeveloperAmendment,
            _ => return None,
        })
    }
}

/// Monotonic revision id within one document's revision log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RevisionId(pub u64);

/// Content fingerprint of one document rendering. `Fingerprint(0)` marks an
/// absent document; the collision odds of a real render hashing to 0 are
/// negligible (64-bit). The CAS contract is in-process only — every
/// write path starts from a fresh load, so fingerprints never cross a
/// process restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint(pub u64);

/// Milliseconds since the Unix epoch.
pub type TimestampMs = u64;

/// A calendar date. Human-editable documents store this as `YYYY-MM-DD`; the
/// string form is a persistence concern owned by `infra`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DateYmd {
    pub year: i16,
    pub month: u8,
    pub day: u8,
}

impl DateYmd {
    /// Validates the calendar date (leap years included).
    pub fn new(year: i16, month: u8, day: u8) -> Result<Self, String> {
        if !(1..=12).contains(&month) {
            return Err(format!("month out of range: {month}"));
        }
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let max_day = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            _ => {
                if leap {
                    29
                } else {
                    28
                }
            }
        };
        if !(1..=max_day).contains(&day) {
            return Err(format!("day out of range: {day}"));
        }
        Ok(Self { year, month, day })
    }

    /// Parses the `YYYY-MM-DD` form used in human-editable documents.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut parts = raw.split('-');
        let (year, month, day) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(y), Some(m), Some(d), None) => (y, m, d),
            _ => return Err(format!("date must be YYYY-MM-DD: {raw}")),
        };
        let year: i16 = year
            .parse()
            .map_err(|_| format!("date year is not numeric: {raw}"))?;
        let month: u8 = month
            .parse()
            .map_err(|_| format!("date month is not numeric: {raw}"))?;
        let day: u8 = day
            .parse()
            .map_err(|_| format!("date day is not numeric: {raw}"))?;
        Self::new(year, month, day)
    }

    /// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
    pub fn days_since_epoch(self) -> i64 {
        let y = if self.month <= 2 {
            i64::from(self.year) - 1
        } else {
            i64::from(self.year)
        };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = i64::from((self.month + 9) % 12);
        let doy = (153 * mp + 2) / 5 + i64::from(self.day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// Inverse of [`DateYmd::days_since_epoch`] (Hinnant's `civil_from_days`).
    pub fn from_days_since_epoch(days: i64) -> Self {
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if m <= 2 { y + 1 } else { y };
        Self {
            year: year as i16,
            month: m as u8,
            day: d as u8,
        }
    }
}

impl std::fmt::Display for DateYmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display_round_trip() {
        let parsed = DateYmd::parse("2026-11-03").expect("valid date");
        assert_eq!(parsed.year, 2026);
        assert_eq!(parsed.to_string(), "2026-11-03");
    }

    #[test]
    fn parse_rejects_malformed_input() {
        assert!(DateYmd::parse("2026/11/03").is_err());
        assert!(DateYmd::parse("2026-13-01").is_err());
        assert!(DateYmd::parse("2027-02-29").is_err());
        assert!(DateYmd::parse("2024-02-29").is_ok());
    }

    #[test]
    fn epoch_round_trip_matches_known_values() {
        assert_eq!(
            DateYmd::new(1970, 1, 1).expect("valid").days_since_epoch(),
            0
        );
        assert_eq!(
            DateYmd::from_days_since_epoch(0),
            DateYmd::new(1970, 1, 1).expect("valid")
        );
        let millenium = DateYmd::new(2000, 3, 1).expect("valid");
        assert_eq!(millenium.days_since_epoch(), 11_017);
        assert_eq!(DateYmd::from_days_since_epoch(11_017), millenium);
    }

    #[test]
    fn ordering_is_chronological() {
        let earlier = DateYmd::new(2026, 1, 1).expect("valid");
        let later = DateYmd::new(2027, 1, 1).expect("valid");
        assert!(earlier < later);
    }

    #[test]
    fn change_kind_vocabulary_round_trips() {
        for kind in [
            ChangeKind::Initialize,
            ChangeKind::Experience,
            ChangeKind::UserExplicitEdit,
            ChangeKind::Negotiated,
            ChangeKind::Rollback,
            ChangeKind::Recovered,
            ChangeKind::Amendment,
            ChangeKind::DeveloperAmendment,
        ] {
            assert_eq!(ChangeKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ChangeKind::parse("nonsense"), None);
    }
}
