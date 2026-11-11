//! The rule for a member's display name, in one place. Pure.
//!
//! A member's token (`worker-2`) is its identifier and never changes; a display
//! name is what its Director calls it, given and changed at will, so that the
//! Director, its Workers and the operator can tell a standing role apart. The
//! Director chooses the name through an MCP tool and the operator reads it in the
//! emporium, so one function decides what a name may be: the tool uses it to
//! refuse a bad one, and every reader uses [`readable`] so a value that does not
//! meet it — old data, a hand-edited row — reads as no name at all.
//!
//! What a name may not contain, and why each:
//! - **control characters and invisible format characters** (an escape that
//!   opens a terminal sequence, a bidi override that reorders a row, a
//!   zero-width joiner): dropped by the reduction every text someone else wrote
//!   goes through (`display_text`), so what is stored is what is shown;
//! - **a newline or other line break**: a name is one line (the reduction folds
//!   whitespace, so a break can never start a line of its own);
//! - **an ASCII double quote `"`**: a name is shown inside quotes — in message
//!   headers an AI reads (`[from worker-3 "impl" — to you]`), in `brigade_status`
//!   and in the operator's statuses — and must not be able to end them;
//! - **square brackets `[` and `]`**: they are the delimiters of that message
//!   header, which is provenance (who sent this), so a name must never be able to
//!   close the bracket or open another;
//! - **more than 24 display columns**: it has to fit a tile's title row.
//!
//! It also may not be, or look like, a token, nor repeat another member's name
//! (a name that could be taken for an address or for someone else defeats the
//! point of one).

use unicode_width::UnicodeWidthStr;

use crate::display_text::display_name;

/// The widest a name may be, in display columns.
pub const MEMBER_NAME_COLUMNS: usize = 24;

/// Why a name was refused, worded for the Director that chose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameRejection {
    /// A non-empty input with nothing visible left once reduced.
    NothingVisible,
    /// Wider than [`MEMBER_NAME_COLUMNS`] columns once reduced.
    TooWide { columns: usize },
    /// Contains an ASCII double quote: a name is shown inside quotes.
    HasQuote,
    /// Contains `[` or `]`: the delimiters of the message header a name appears in.
    HasBracket,
    /// Is, or looks like, a token.
    LooksLikeToken,
    /// Another member of the brigade already has this name (any case).
    Taken { by: String },
}

impl std::fmt::Display for NameRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NameRejection::NothingVisible => write!(
                f,
                "The name has nothing visible in it (control and invisible characters are \
                 dropped). Use plain text, or an empty name to clear it."
            ),
            NameRejection::TooWide { columns } => write!(
                f,
                "The name is {columns} columns wide; the limit is {MEMBER_NAME_COLUMNS}. \
                 Choose a shorter one."
            ),
            NameRejection::HasQuote => write!(
                f,
                "The name must not contain a double quote (\"): it is shown inside quotes."
            ),
            NameRejection::HasBracket => write!(
                f,
                "The name must not contain [ or ]: they delimit the message headers it \
                 appears in."
            ),
            NameRejection::LooksLikeToken => write!(
                f,
                "The name is, or looks like, a member token (\"director\", \"goinkyo\", \
                 \"worker-<number>\"). A name is not an address: pick something that says \
                 what the Worker is for."
            ),
            NameRejection::Taken { by } => {
                write!(
                    f,
                    "Another member of this brigade is already named that ({by})."
                )
            }
        }
    }
}

/// Whether `text` is a token, or reads like one: `director`, `goinkyo`, or
/// `worker-` followed by digits, in any case.
pub fn looks_like_token(text: &str) -> bool {
    let lower = text.to_lowercase();
    if lower == "director" || lower == "goinkyo" {
        return true;
    }
    lower
        .strip_prefix("worker-")
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
}

/// The part of the rule that needs nothing but the name itself: reduced and
/// one line, visible, at most [`MEMBER_NAME_COLUMNS`] columns, no double quote,
/// not token-like. `Ok` is the canonical name.
fn check_alone(input: &str) -> Result<String, NameRejection> {
    let name = display_name(input).ok_or(NameRejection::NothingVisible)?;
    let columns = name.width();
    if columns > MEMBER_NAME_COLUMNS {
        return Err(NameRejection::TooWide { columns });
    }
    if name.contains('"') {
        return Err(NameRejection::HasQuote);
    }
    if name.contains(['[', ']']) {
        return Err(NameRejection::HasBracket);
    }
    if looks_like_token(&name) {
        return Err(NameRejection::LooksLikeToken);
    }
    Ok(name)
}

/// What a stored name reads as: itself if it meets the rule on its own and is
/// already in its canonical form, otherwise nothing. (Whether another member
/// has the same name is a judgement about a whole roster — see
/// [`dedupe_roster`].)
///
/// "Already canonical" matters: the Director's input is reduced before it is
/// stored, so a stored value that reduction would change was not written by the
/// tool, and is not trusted to be safe to show.
pub fn readable(stored: &str) -> Option<String> {
    let name = check_alone(stored).ok()?;
    (name == stored).then_some(name)
}

/// The member names of one brigade as readers see them: each stored name that
/// is [`readable`], is not any member's token, and is not already taken by an
/// earlier member of `members` (a name is unique in a brigade, any case). The
/// caller passes the members in join order, so the earliest member to hold a
/// name keeps it.
///
/// `members` is `(token, stored name)`; the result has the same length and
/// order.
pub fn dedupe_roster(members: &[(&str, Option<&str>)]) -> Vec<Option<String>> {
    let mut seen: Vec<String> = Vec::new();
    members
        .iter()
        .map(|(_, stored)| {
            let name = readable((*stored)?)?;
            let lower = name.to_lowercase();
            let is_a_token = members
                .iter()
                .any(|(token, _)| token.to_lowercase() == lower);
            if is_a_token || seen.contains(&lower) {
                return None;
            }
            seen.push(lower);
            Some(name)
        })
        .collect()
}

/// Validate `input` as the new name of `member` in a brigade whose members, with
/// the names readers see for them, are `roster` (`(token, name)`, `member`
/// included). `Ok(Some(name))` is the canonical name to store, `Ok(None)` means
/// clear the name (the input was empty or only whitespace), `Err` says why not.
///
/// The member's own current name never counts against it: giving a member the
/// name it has, or the same name in another case, is fine.
pub fn validate(
    input: &str,
    member: &str,
    roster: &[(&str, Option<&str>)],
) -> Result<Option<String>, NameRejection> {
    if input.trim().is_empty() {
        return Ok(None);
    }
    let name = check_alone(input)?;
    let lower = name.to_lowercase();
    for (token, other) in roster {
        if *token == member {
            continue;
        }
        if token.to_lowercase() == lower {
            return Err(NameRejection::LooksLikeToken);
        }
        if other.is_some_and(|other| other.to_lowercase() == lower) {
            return Err(NameRejection::Taken {
                by: (*token).to_string(),
            });
        }
    }
    Ok(Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster<'a>() -> Vec<(&'a str, Option<&'a str>)> {
        vec![
            ("director", None),
            ("worker-1", Some("impl")),
            ("worker-2", None),
            ("goinkyo", None),
        ]
    }

    #[test]
    fn a_plain_name_is_accepted_and_empty_input_clears() {
        assert_eq!(
            validate("  reviewer  ", "worker-2", &roster()),
            Ok(Some("reviewer".to_string()))
        );
        assert_eq!(validate("", "worker-2", &roster()), Ok(None));
        assert_eq!(validate("   \t\n", "worker-1", &roster()), Ok(None));
    }

    #[test]
    fn hostile_input_is_reduced_before_it_is_stored() {
        // Escape sequences and bidi/zero-width characters are dropped, runs of
        // whitespace become one space.
        assert_eq!(
            validate(
                "re\u{1b}Jview\u{202e}er \n  two\u{200b}",
                "worker-2",
                &roster()
            ),
            Ok(Some("reJviewer two".to_string()))
        );
        // The printable tail of an escape sequence is left behind by the
        // reduction, and its `[` is refused: it cannot pass for a frame.
        assert_eq!(
            validate("a\u{1b}[2Jb", "worker-2", &roster()),
            Err(NameRejection::HasBracket)
        );
        // Nothing left once reduced is refused, not silently cleared.
        assert_eq!(
            validate("\u{1b}\u{200b}\u{202e}", "worker-2", &roster()),
            Err(NameRejection::NothingVisible)
        );
    }

    #[test]
    fn the_limit_is_twenty_four_display_columns_and_longer_is_refused() {
        let ok = "a".repeat(24);
        assert_eq!(validate(&ok, "worker-2", &roster()), Ok(Some(ok)));
        assert_eq!(
            validate(&"a".repeat(25), "worker-2", &roster()),
            Err(NameRejection::TooWide { columns: 25 })
        );
        // Wide characters take two columns each: 12 fit, 13 do not.
        let wide12 = "\u{30c6}".repeat(12);
        assert_eq!(validate(&wide12, "worker-2", &roster()), Ok(Some(wide12)));
        assert_eq!(
            validate(&"\u{30c6}".repeat(13), "worker-2", &roster()),
            Err(NameRejection::TooWide { columns: 26 })
        );
        // The limit is on what is stored, not what was typed: invisible padding
        // does not count.
        let padded = format!("{}\u{200b}", "a".repeat(24));
        assert!(validate(&padded, "worker-2", &roster()).is_ok());
    }

    #[test]
    fn a_name_that_is_or_looks_like_a_token_is_refused() {
        for bad in [
            "director",
            "Director",
            "goinkyo",
            "GOINKYO",
            "worker-1",
            "worker-7",
            "Worker-12",
            "worker-007",
        ] {
            assert_eq!(
                validate(bad, "worker-2", &roster()),
                Err(NameRejection::LooksLikeToken),
                "{bad}"
            );
        }
        // Not token-shaped: allowed.
        for good in [
            "worker",
            "worker-",
            "worker-a",
            "worker 3",
            "worker-3a",
            "director's",
        ] {
            assert!(validate(good, "worker-2", &roster()).is_ok(), "{good}");
        }
        // Equal to any member's token, whatever shape the token has.
        let odd = vec![("director", None), ("scout", None), ("worker-2", None)];
        assert_eq!(
            validate("Scout", "worker-2", &odd),
            Err(NameRejection::LooksLikeToken)
        );
    }

    #[test]
    fn a_name_with_a_square_bracket_is_refused_so_it_cannot_close_a_message_header() {
        for bad in ["a]b", "[a", "x] — to you\n[from", "[]", "ok ]"] {
            assert_eq!(
                validate(bad, "worker-2", &roster()),
                Err(NameRejection::HasBracket),
                "{bad:?}"
            );
        }
        // The other delimiters of the line are not structural inside quotes.
        for fine in ["a (b)", "a \u{2014} b", "a, b", "{x}", "a \u{b7} b"] {
            assert!(validate(fine, "worker-2", &roster()).is_ok(), "{fine:?}");
        }
        assert_eq!(readable("a]b"), None);
        assert!(NameRejection::HasBracket.to_string().contains("[ or ]"));
    }

    #[test]
    fn a_name_with_a_double_quote_is_refused() {
        assert_eq!(
            validate("the \"fast\" one", "worker-2", &roster()),
            Err(NameRejection::HasQuote)
        );
        // A single quote is fine.
        assert!(validate("Sam's", "worker-2", &roster()).is_ok());
    }

    #[test]
    fn a_name_another_member_has_is_refused_in_any_case_but_ones_own_is_fine() {
        assert_eq!(
            validate("Impl", "worker-2", &roster()),
            Err(NameRejection::Taken {
                by: "worker-1".to_string()
            })
        );
        // Its own name again, or changing the case of it.
        assert_eq!(
            validate("impl", "worker-1", &roster()),
            Ok(Some("impl".to_string()))
        );
        assert_eq!(
            validate("IMPL", "worker-1", &roster()),
            Ok(Some("IMPL".to_string()))
        );
    }

    #[test]
    fn rejections_say_why() {
        let text = |r: NameRejection| r.to_string();
        assert!(text(NameRejection::TooWide { columns: 30 }).contains("30 columns"));
        assert!(text(NameRejection::TooWide { columns: 30 }).contains("24"));
        assert!(text(NameRejection::HasQuote).contains("double quote"));
        assert!(text(NameRejection::LooksLikeToken).contains("not an address"));
        assert!(
            text(NameRejection::Taken {
                by: "worker-1".to_string()
            })
            .contains("worker-1")
        );
        assert!(text(NameRejection::NothingVisible).contains("nothing visible"));
    }

    #[test]
    fn a_stored_value_that_fails_the_rule_reads_as_no_name() {
        assert_eq!(readable("reviewer"), Some("reviewer".to_string()));
        for stored in [
            "",
            "   ",
            " padded ",
            "two  spaces",
            "esc\u{1b}[31m",
            "bidi\u{202e}",
            "zero\u{200b}width",
            "quote\"d",
            "worker-3",
            "Director",
            &"x".repeat(25),
            "line\nbreak",
        ] {
            assert_eq!(readable(stored), None, "{stored:?}");
        }
    }

    #[test]
    fn a_roster_reads_each_name_once_and_never_a_token() {
        let members = [
            ("director", Some("boss")),
            ("worker-1", Some("impl")),
            // A duplicate of worker-1's, in another case: the later one loses.
            ("worker-2", Some("IMPL")),
            // Equal to a member's token.
            ("worker-3", Some("goinkyo")),
            ("worker-4", Some("garbage\u{1b}")),
            ("worker-5", None),
            ("goinkyo", None),
        ];
        assert_eq!(
            dedupe_roster(&members),
            [
                Some("boss".to_string()),
                Some("impl".to_string()),
                None,
                None,
                None,
                None,
                None
            ]
        );
    }
}
