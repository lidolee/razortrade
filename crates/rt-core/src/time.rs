//! Drop 19 Part C — G1: einheitliches Timestamp-Format über die gesamte
//! Codebase.
//!
//! Das Problem: `DateTime::<Utc>::to_rfc3339()` in chrono emittiert
//! **Nanosekunden** und Suffix `+00:00`. Python's `datetime.isoformat()`
//! liefert standardmässig keine Sub-Sekunden und `+00:00`. Beim
//! lexikografischen Vergleich am Sekunden-Rand (z.B. in SQLite-Queries
//! wie `retry_after_at <= ?` oder `expires_at > ?`) führt das zu
//! subtilen Fehlern: `"2026-04-20T10:00:00+00:00"` ist kleiner als
//! `"2026-04-20T10:00:00.000000001+00:00"`, aber beide beschreiben
//! "genau 10:00:00" UTC je nach Interpretation.
//!
//! Lösung: alles schreibt im gleichen, mikrosekunden-präzisen Format
//! mit `Z`-Suffix:
//!
//! ```text
//! 2026-04-20T10:45:00.123456Z
//! ```
//!
//! - 26 Zeichen immer (fixer Widht → lexikografischer Vergleich sortiert
//!   korrekt chronologisch)
//! - Mikrosekunden genug für unsere Granularität (kein HFT)
//! - `Z` statt `+00:00` ist gültiges RFC 3339 und `DateTime::parse_from_rfc3339`
//!   akzeptiert beides.

use chrono::{DateTime, SecondsFormat, Utc};

/// Kanonischer ISO-8601/RFC-3339-Timestamp für alle rt-daemon Writes
/// in SQLite. Mikrosekunden, Z-Suffix, fix 26 Zeichen.
pub fn canonical_iso(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Shortcut für `canonical_iso(Utc::now())`.
pub fn now_iso() -> String {
    canonical_iso(Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn format_has_micros_and_z_suffix() {
        let dt = Utc.with_ymd_and_hms(2026, 4, 20, 10, 45, 0).unwrap();
        let s = canonical_iso(dt);
        assert_eq!(s, "2026-04-20T10:45:00.000000Z");
        assert!(s.ends_with('Z'));
        assert_eq!(s.len(), 27); // YYYY-MM-DDTHH:MM:SS.NNNNNNZ
    }

    #[test]
    fn lexicographic_ordering_matches_chronological() {
        let a = Utc.with_ymd_and_hms(2026, 4, 20, 9, 59, 59).unwrap();
        let b = Utc.with_ymd_and_hms(2026, 4, 20, 10, 0, 0).unwrap();
        let sa = canonical_iso(a);
        let sb = canonical_iso(b);
        assert!(sa < sb);
    }

    #[test]
    fn parse_round_trip() {
        let s = "2026-04-20T10:45:00.123456Z";
        let dt = DateTime::parse_from_rfc3339(s).unwrap();
        let back = canonical_iso(dt.with_timezone(&Utc));
        assert_eq!(back, s);
    }
}
