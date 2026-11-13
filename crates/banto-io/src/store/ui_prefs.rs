//! Window-wide interface preferences: a small key/value table for what
//! belongs to the whole emporium rather than to one brigade. Today that is
//! the sidebar width and the brigade-quiet bell switch; brigade arrangement
//! lives on the brigade's own rows (`brigades.rs`, migration v15).

use rusqlite::{OptionalExtension, params};

use super::{Store, StoreError};

const SIDEBAR_WIDTH_KEY: &str = "sidebar_width";
const BELL_WHEN_FOCUSED_KEY: &str = "bell_when_focused";

impl Store {
    /// The stored sidebar width in columns, if there is a usable one. A value
    /// that is not a whole number of columns between 1 and 1000 reads as "no
    /// preference": the file outlives versions, and the width is clamped to
    /// the terminal at use anyway.
    pub fn sidebar_width(&self) -> Result<Option<u16>, StoreError> {
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM ui_prefs WHERE key = ?1",
                [SIDEBAR_WIDTH_KEY],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value
            .and_then(|text| text.trim().parse::<u16>().ok())
            .filter(|width| (1..=1000).contains(width)))
    }

    /// Stores the sidebar width. Idempotent.
    pub fn set_sidebar_width(&self, width: u16) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO ui_prefs (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SIDEBAR_WIDTH_KEY, width.to_string()],
        )?;
        Ok(())
    }

    /// The stored "ring the brigade-quiet bell even when the Director is
    /// focused" switch, if there is a usable value: only `1` and `0` are
    /// (anything else reads as "no preference", which the caller takes as off —
    /// the file outlives versions, and a stranger value must not turn a bell on).
    pub fn bell_when_focused(&self) -> Result<Option<bool>, StoreError> {
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM ui_prefs WHERE key = ?1",
                [BELL_WHEN_FOCUSED_KEY],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match value.as_deref().map(str::trim) {
            Some("1") => Some(true),
            Some("0") => Some(false),
            _ => None,
        })
    }

    /// Stores the switch. Idempotent.
    pub fn set_bell_when_focused(&self, on: bool) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO ui_prefs (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![BELL_WHEN_FOCUSED_KEY, if on { "1" } else { "0" }],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sidebar_width_round_trips_and_defaults_to_no_preference() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.sidebar_width().unwrap(), None);

        store.set_sidebar_width(52).unwrap();
        assert_eq!(store.sidebar_width().unwrap(), Some(52));
        // Idempotent, and a later value replaces it.
        store.set_sidebar_width(52).unwrap();
        store.set_sidebar_width(40).unwrap();
        assert_eq!(store.sidebar_width().unwrap(), Some(40));
    }

    #[test]
    fn the_bell_switch_round_trips_and_defaults_to_no_preference() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.bell_when_focused().unwrap(), None);

        store.set_bell_when_focused(true).unwrap();
        assert_eq!(store.bell_when_focused().unwrap(), Some(true));
        store.set_bell_when_focused(true).unwrap();
        store.set_bell_when_focused(false).unwrap();
        assert_eq!(store.bell_when_focused().unwrap(), Some(false));
        // Its own key: the width is untouched.
        assert_eq!(store.sidebar_width().unwrap(), None);
    }

    #[test]
    fn a_nonsensical_stored_bell_switch_reads_as_no_preference() {
        let store = Store::open_in_memory().unwrap();
        for bad in ["yes", "true", "2", "", "-1", "on"] {
            store
                .conn
                .execute(
                    "INSERT INTO ui_prefs (key, value) VALUES ('bell_when_focused', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    [bad],
                )
                .unwrap();
            assert_eq!(store.bell_when_focused().unwrap(), None, "{bad:?}");
        }
    }

    #[test]
    fn a_nonsensical_stored_width_reads_as_no_preference() {
        let store = Store::open_in_memory().unwrap();
        for bad in ["-5", "0", "1001", "99999999999", "wide", "", "36.5"] {
            store
                .conn
                .execute(
                    "INSERT INTO ui_prefs (key, value) VALUES ('sidebar_width', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    [bad],
                )
                .unwrap();
            assert_eq!(store.sidebar_width().unwrap(), None, "{bad:?}");
        }
        store
            .conn
            .execute(
                "UPDATE ui_prefs SET value = ' 48 ' WHERE key = 'sidebar_width'",
                [],
            )
            .unwrap();
        assert_eq!(store.sidebar_width().unwrap(), Some(48));
    }
}
