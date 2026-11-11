//! Window-wide interface preferences: a small key/value table for what
//! belongs to the whole emporium rather than to one brigade. Today that is
//! the sidebar width; brigade arrangement lives on the brigade's own rows
//! (`brigades.rs`, migration v15).

use rusqlite::{OptionalExtension, params};

use super::{Store, StoreError};

const SIDEBAR_WIDTH_KEY: &str = "sidebar_width";

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
