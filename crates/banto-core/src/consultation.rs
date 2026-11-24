//! What a consultation is called, in one place. Pure.
//!
//! A Goinkyo's session would otherwise be listed under banto's own first
//! prompt, or Claude's generated title, which is the same for every
//! consultation. The Director that files one gives it a short title
//! (`consult_goinkyo`'s `title`), and the list shows that, behind a fixed label,
//! once the consultation has ended. The rule for the title is here so that the
//! tool that refuses a bad one and the list that shows a good one cannot drift.

use unicode_width::UnicodeWidthStr;

use crate::display_text::display_name;
use crate::member_name::MEMBER_NAME_COLUMNS;

/// The widest a title may be, in display columns: the same limit a member's
/// display name has, for the same reason that one is not larger (it has to fit
/// a tile's title row), and with it the label and the title together are 33
/// columns, which is inside the 34 the sidebar's list has at its default width
/// (`SIDEBAR_WIDTH` 36 less the border).
///
/// Wider than a list *row* can show, and that is deliberate: a row's title gets
/// the 34 columns less the fixed slots (dot, pin and role: 8 flat, 5 when the
/// list is grouped), the gap of at least 2 and the age ("5m" is 2 columns, "now"
/// and "12m" are 3), so 21 to 22 columns flat and 24 to 25 grouped. The label
/// takes 9 of them, so the Director's own words get 12 to 13 flat and 15 to 16
/// grouped; 12 and 15 are the figures that always hold. A limit that guaranteed the whole title was visible there
/// would be 12 columns, too few to say what a consultation is about; this one
/// instead tells the Director to put what identifies it first, and the Details
/// panel and a wider sidebar show the rest.
pub const CONSULT_TITLE_COLUMNS: usize = MEMBER_NAME_COLUMNS;

/// The label a consultation's title is listed behind. In code, not in the
/// store, so changing the word changes every consultation at once.
pub const CONSULT_TITLE_LABEL: &str = "Goinkyo: ";

/// Why a title was refused, worded for the Director that chose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitleRejection {
    /// Nothing visible once reduced (empty, whitespace, control characters).
    NothingVisible,
    /// Wider than [`CONSULT_TITLE_COLUMNS`] once reduced.
    TooWide { columns: usize },
}

impl std::fmt::Display for TitleRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TitleRejection::NothingVisible => write!(
                f,
                "`title` has nothing visible in it (control and invisible characters are \
                 dropped). Give the consultation a short title, one line of plain text."
            ),
            TitleRejection::TooWide { columns } => write!(
                f,
                "`title` is {columns} columns wide; the limit is {CONSULT_TITLE_COLUMNS}. \
                 Choose a shorter one, with what identifies the consultation first: the \
                 operator's list shows only its first dozen or so columns."
            ),
        }
    }
}

/// `input` reduced to one visible line and checked against the limit. `Ok` is
/// the title to store.
pub fn check_title(input: &str) -> Result<String, TitleRejection> {
    let title = display_name(input).ok_or(TitleRejection::NothingVisible)?;
    let columns = title.width();
    if columns > CONSULT_TITLE_COLUMNS {
        return Err(TitleRejection::TooWide { columns });
    }
    Ok(title)
}

/// A stored consultation title as the list shows it.
pub fn list_title(title: &str) -> String {
    format!("{CONSULT_TITLE_LABEL}{title}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_is_reduced_to_one_visible_line() {
        assert_eq!(
            check_title("  Layout \n drag\t\u{1b}[31mx  ").as_deref(),
            Ok("Layout drag [31mx")
        );
    }

    #[test]
    fn nothing_visible_is_refused() {
        for blank in ["", "   ", "\n\t", "\u{200b}\u{1b}"] {
            assert_eq!(check_title(blank), Err(TitleRejection::NothingVisible));
        }
    }

    #[test]
    fn the_limit_is_in_display_columns_not_characters() {
        let ascii = "a".repeat(CONSULT_TITLE_COLUMNS);
        assert_eq!(check_title(&ascii), Ok(ascii.clone()));
        let one_more = "a".repeat(CONSULT_TITLE_COLUMNS + 1);
        assert_eq!(
            check_title(&one_more),
            Err(TitleRejection::TooWide {
                columns: CONSULT_TITLE_COLUMNS + 1
            })
        );
        // Twelve wide characters are 24 columns; thirteen are 26.
        let wide = "\u{3042}".repeat(CONSULT_TITLE_COLUMNS / 2);
        assert_eq!(check_title(&wide), Ok(wide.clone()));
        assert!(matches!(
            check_title(&"\u{3042}".repeat(CONSULT_TITLE_COLUMNS / 2 + 1)),
            Err(TitleRejection::TooWide { columns: 26 })
        ));
    }

    #[test]
    fn the_label_and_a_full_title_fit_the_default_sidebar_list() {
        // 36 columns, less the border on each side.
        let inner = usize::from(crate::layout::SIDEBAR_WIDTH) - 2;
        assert!(CONSULT_TITLE_LABEL.width() + CONSULT_TITLE_COLUMNS <= inner);
    }

    #[test]
    fn the_list_shows_the_title_behind_the_label() {
        assert_eq!(list_title("Layout drag"), "Goinkyo: Layout drag");
    }
}
