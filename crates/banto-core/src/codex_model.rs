//! Choosing the model a new Codex session is launched with from the one word
//! the operator configured or typed.
//!
//! Codex has no alias or "latest" for `-m`: the string has to be (a
//! prefix-extension of) a slug in its model catalog, and an unknown one is sent
//! to the API as it stands. So a family word such as `terra` is resolved here,
//! at the moment a session is created, to the newest slug of that family in the
//! catalog compiled into the installed binary — `codex debug models --bundled`,
//! which `banto_io::codex_catalog` runs. A catalog read at startup and kept would
//! go stale the day Codex updates itself, and a configured slug would then name
//! a model the new binary no longer lists.
//!
//! The command prints one line of JSON, `{"models":[{…}]}`, whose objects have
//! many fields; this reads three — `slug`, `visibility` (`"list"` or `"hide"`)
//! and `priority` (lower first) — and nothing else. It is a visible debug
//! subcommand whose shape tracks Codex's internal struct, so everything is
//! optional here but the slug (a model without one cannot be launched).
//!
//! The command and the shape of its output were run and read by another member
//! of this brigade against a real Codex; this module's tests use a synthetic
//! catalog, never one captured from a machine.

use serde::Deserialize;

/// What one catalog entry says about itself.
#[derive(Debug, Clone, PartialEq)]
struct CatalogModel {
    slug: String,
    /// Whether the model is offered to the operator. Only `"list"` and an
    /// absent field count: any other word (`"hide"`, `"none"`, whatever a
    /// later Codex invents) keeps a model out of a family match, since a model
    /// Codex does not show is not one to pick for someone by word.
    visible: bool,
    priority: Option<f64>,
}

/// The catalog, as far as it could be read.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    models: Vec<CatalogModel>,
}

#[derive(Deserialize)]
struct RawCatalog {
    models: Vec<serde_json::Value>,
}

impl Catalog {
    /// Parse the command's standard output. `None` when it is not a catalog at
    /// all (not JSON, or no `models` array); an entry without a string `slug`
    /// is skipped, and every other field is optional. Standard output only, so
    /// a warning on standard error cannot get in. Tolerated ahead of the JSON:
    /// a UTF-8 byte-order mark (Windows tools write one, and Rust's `trim` does
    /// not count it as whitespace), blank lines, and any other line before the
    /// one that opens the object — a log line — by looking for the first line
    /// that does. The JSON itself must be on one line, as the command prints it.
    pub fn parse(stdout: &str) -> Option<Self> {
        let clean = |text: &str| -> String {
            text.trim()
                .trim_start_matches('\u{feff}')
                .trim_start()
                .to_string()
        };
        let whole = clean(stdout);
        let raw: RawCatalog = serde_json::from_str(&whole).ok().or_else(|| {
            stdout
                .lines()
                .map(clean)
                .filter(|line| line.starts_with('{'))
                .find_map(|line| serde_json::from_str(&line).ok())
        })?;
        let models = raw
            .models
            .iter()
            .filter_map(|model| {
                let slug = model.get("slug")?.as_str()?.to_string();
                let visible = match model.get("visibility").and_then(|v| v.as_str()) {
                    Some(word) => word == "list",
                    None => true,
                };
                let priority = model.get("priority").and_then(serde_json::Value::as_f64);
                Some(CatalogModel {
                    slug,
                    visible,
                    priority,
                })
            })
            .collect();
        Some(Self { models })
    }
}

/// A slug's version: the first `-`-separated token made only of digits and
/// dots (`6.1` in `gpt-6.1-terra`), as numbers. `None` when the slug has no
/// such token. Compared component by component, a missing component counting as
/// zero, so `6.1 > 6 = 6.0 > 5.6` and `10.0 > 9.9`: the comparison is numeric,
/// never alphabetical.
fn version_of(slug: &str) -> Option<Vec<u64>> {
    slug.split('-').find_map(|token| {
        let numeric = !token.is_empty()
            && token.chars().all(|c| c.is_ascii_digit() || c == '.')
            && token.chars().any(|c| c.is_ascii_digit());
        if !numeric {
            return None;
        }
        token
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect()
    })
}

fn compare_versions(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    let len = a.len().max(b.len());
    (0..len)
        .map(|i| {
            let left = a.get(i).copied().unwrap_or(0);
            let right = b.get(i).copied().unwrap_or(0);
            left.cmp(&right)
        })
        .find(|ordering| ordering.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

/// Whether `word` is one whole `-`-separated token of `slug`, ignoring case:
/// `terra` is in `gpt-6.1-terra` and not in `gpt-6.1-terrain`.
fn names_family(slug: &str, word: &str) -> bool {
    slug.split('-')
        .any(|token| token.eq_ignore_ascii_case(word))
}

/// Which of two family members is the newer: the higher version; a slug with a
/// version beats one without; then the lower `priority` (a missing one last);
/// then the slug itself, so the answer never depends on the catalog's order.
fn newer(a: &CatalogModel, b: &CatalogModel) -> std::cmp::Ordering {
    let by_version = match (version_of(&a.slug), version_of(&b.slug)) {
        (Some(x), Some(y)) => compare_versions(&x, &y),
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (None, None) => std::cmp::Ordering::Equal,
    };
    let rank = |m: &CatalogModel| m.priority.unwrap_or(f64::INFINITY);
    by_version
        .then_with(|| rank(b).total_cmp(&rank(a)))
        .then_with(|| b.slug.cmp(&a.slug))
}

/// What to launch a new Codex session with, and what to tell the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The `-m` value; `None` passes no `-m` at all.
    pub model: Option<String>,
    /// A line for the status bar, or `None` when nothing worth saying
    /// happened (an empty request, or a slug the catalog already lists).
    pub notice: Option<String>,
}

/// Resolve `requested` (a family word or a slug, as configured or typed)
/// against what the catalog command produced: its standard output, or why it
/// could not be run.
///
/// 1. Empty: no `-m`.
/// 2. The catalog lists that exact slug: as it is.
/// 3. Visible slugs with the request as a whole `-`-separated token, ignoring
///    case: the newest of them. A hidden model never wins a family match.
/// 4. Nothing matches: as given, since the operator may know a slug this
///    binary's catalog lacks — and said so.
/// 5. The catalog could not be read: a request holding a digit is taken for a
///    slug and passed as it is; a bare family word is not passed at all, and
///    the notice says Codex will then use its own default.
pub fn resolve(requested: &str, catalog: &Result<String, String>) -> Resolved {
    let requested = requested.trim();
    if requested.is_empty() {
        return Resolved {
            model: None,
            notice: None,
        };
    }
    let parsed = match catalog {
        Ok(stdout) => Catalog::parse(stdout).ok_or_else(|| "unparseable output".to_string()),
        Err(why) => Err(why.clone()),
    };
    let catalog = match parsed {
        Ok(catalog) => catalog,
        Err(why) => {
            return if requested.chars().any(|c| c.is_ascii_digit()) {
                Resolved {
                    model: Some(requested.to_string()),
                    notice: Some(format!(
                        "codex worker: could not read Codex's model list ({why}) — \
                         \"{requested}\" passed as given"
                    )),
                }
            } else {
                Resolved {
                    model: None,
                    notice: Some(format!(
                        "codex worker: could not read Codex's model list ({why}) — \
                         \"{requested}\" not resolved, so no -m: Codex uses its own default model"
                    )),
                }
            };
        }
    };
    if catalog.models.iter().any(|m| m.slug == requested) {
        return Resolved {
            model: Some(requested.to_string()),
            notice: None,
        };
    }
    let family = catalog
        .models
        .iter()
        .filter(|m| m.visible && names_family(&m.slug, requested))
        .max_by(|a, b| newer(a, b));
    match family {
        Some(model) => Resolved {
            model: Some(model.slug.clone()),
            notice: Some(format!("codex worker: {requested} → {}", model.slug)),
        },
        None => Resolved {
            model: Some(requested.to_string()),
            notice: Some(format!(
                "codex worker: \"{requested}\" is not in Codex's model list — passed as given"
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic catalog: the fields this module reads plus ones it must
    /// ignore, in the order a real one is unlikely to have.
    fn catalog() -> Result<String, String> {
        Ok(r#"{"models":[
            {"slug":"gpt-5.6-terra","display_name":"x","visibility":"list","priority":4,"shell_type":"a"},
            {"slug":"gpt-6.1-terra","visibility":"list","priority":2,"extra":{"nested":[1,2]}},
            {"slug":"gpt-6-terra","visibility":"list","priority":3},
            {"slug":"gpt-6.1-sol","visibility":"list","priority":1},
            {"slug":"gpt-6-astra","visibility":"list","priority":5},
            {"slug":"gpt-7-terra","visibility":"hide","priority":0},
            {"slug":"gpt-6.1-terrain","visibility":"list","priority":0}
        ]}"#
        .to_string())
    }

    fn model(resolved: &Resolved) -> Option<&str> {
        resolved.model.as_deref()
    }

    #[test]
    fn an_empty_request_passes_no_model_and_says_nothing() {
        for blank in ["", "   "] {
            let resolved = resolve(blank, &catalog());
            assert_eq!(resolved.model, None, "{blank:?}");
            assert_eq!(resolved.notice, None);
        }
        // Not even a catalog is needed to know that.
        assert_eq!(resolve("", &Err("gone".to_string())).model, None);
    }

    #[test]
    fn an_exact_slug_is_passed_as_it_is_without_a_notice() {
        let resolved = resolve("gpt-5.6-terra", &catalog());
        assert_eq!(model(&resolved), Some("gpt-5.6-terra"));
        assert_eq!(resolved.notice, None);
    }

    #[test]
    fn an_exact_slug_wins_even_when_a_family_member_is_newer() {
        // Pinning an older model on purpose is what an exact slug is for.
        assert_eq!(
            model(&resolve("gpt-6-terra", &catalog())),
            Some("gpt-6-terra")
        );
    }

    #[test]
    fn a_family_word_picks_the_newest_member_by_version() {
        let resolved = resolve("terra", &catalog());
        assert_eq!(model(&resolved), Some("gpt-6.1-terra"));
        assert_eq!(
            resolved.notice.as_deref(),
            Some("codex worker: terra → gpt-6.1-terra")
        );
    }

    #[test]
    fn the_family_match_ignores_case_and_surrounding_space() {
        assert_eq!(
            model(&resolve("  TeRRa ", &catalog())),
            Some("gpt-6.1-terra")
        );
    }

    #[test]
    fn versions_compare_numerically_component_by_component() {
        let catalog = Ok(r#"{"models":[
            {"slug":"gpt-9.9-terra"},{"slug":"gpt-10.0-terra"},
            {"slug":"gpt-2-terra"},{"slug":"gpt-5.6-terra"}
        ]}"#
        .to_string());
        // Lexically "9.9" and "5.6" sort after "10.0"; numerically they do not.
        assert_eq!(model(&resolve("terra", &catalog)), Some("gpt-10.0-terra"));

        let shorter = Ok(r#"{"models":[
            {"slug":"gpt-6-terra","priority":0},{"slug":"gpt-6.1-terra","priority":9},
            {"slug":"gpt-5.6-terra","priority":0}
        ]}"#
        .to_string());
        // 6.1 > 6 > 5.6 whatever the priorities say.
        assert_eq!(model(&resolve("terra", &shorter)), Some("gpt-6.1-terra"));
    }

    #[test]
    fn six_equals_six_point_zero_so_the_priority_decides_between_them() {
        let catalog = Ok(r#"{"models":[
            {"slug":"gpt-6.0-terra","priority":7},{"slug":"gpt-6-terra","priority":2}
        ]}"#
        .to_string());
        assert_eq!(model(&resolve("terra", &catalog)), Some("gpt-6-terra"));
    }

    #[test]
    fn a_hidden_model_never_wins_a_family_match() {
        // `gpt-7-terra` is the highest version and has the best priority, and
        // is hidden.
        assert_eq!(model(&resolve("terra", &catalog())), Some("gpt-6.1-terra"));
        // Only hidden members of the family: nothing visible to pick.
        let only_hidden = Ok(
            r#"{"models":[{"slug":"gpt-7-terra","visibility":"hide"},{"slug":"gpt-8-terra","visibility":"none"}]}"#
                .to_string(),
        );
        let resolved = resolve("terra", &only_hidden);
        assert_eq!(model(&resolved), Some("terra"), "falls to case 4");
        assert!(
            resolved
                .notice
                .unwrap()
                .contains("not in Codex's model list")
        );
    }

    #[test]
    fn a_hidden_model_can_still_be_asked_for_by_its_exact_slug() {
        assert_eq!(
            model(&resolve("gpt-7-terra", &catalog())),
            Some("gpt-7-terra")
        );
    }

    #[test]
    fn a_family_is_a_whole_token_not_a_substring() {
        // `terrain` holds `terra`'s letters; `gpt-6.1-terrain` must not win
        // (its priority is the best on the list).
        let catalog = Ok(r#"{"models":[
            {"slug":"gpt-9-terrain","priority":0},{"slug":"gpt-1-terra","priority":9}
        ]}"#
        .to_string());
        assert_eq!(model(&resolve("terra", &catalog)), Some("gpt-1-terra"));
        // And a prefix of a token is not the token.
        let resolved = resolve("terr", &catalog);
        assert_eq!(model(&resolved), Some("terr"));
        assert!(
            resolved
                .notice
                .unwrap()
                .contains("not in Codex's model list")
        );
    }

    #[test]
    fn a_request_matching_nothing_is_passed_as_given_and_said_so() {
        let resolved = resolve("gpt-9-mystery", &catalog());
        assert_eq!(model(&resolved), Some("gpt-9-mystery"));
        assert_eq!(
            resolved.notice.as_deref(),
            Some("codex worker: \"gpt-9-mystery\" is not in Codex's model list — passed as given")
        );
    }

    #[test]
    fn an_empty_catalog_is_readable_and_matches_nothing() {
        let resolved = resolve("terra", &Ok(r#"{"models":[]}"#.to_string()));
        assert_eq!(model(&resolved), Some("terra"));
        assert!(
            resolved
                .notice
                .unwrap()
                .contains("not in Codex's model list")
        );
    }

    #[test]
    fn an_unreadable_catalog_passes_a_request_with_a_digit_as_a_slug() {
        let resolved = resolve("gpt-6.1-terra", &Err("codex was not found".to_string()));
        assert_eq!(model(&resolved), Some("gpt-6.1-terra"));
        let notice = resolved.notice.unwrap();
        assert!(notice.contains("could not read Codex's model list (codex was not found)"));
        assert!(notice.contains("passed as given"));
    }

    #[test]
    fn an_unreadable_catalog_passes_no_model_for_a_bare_family_word_and_names_the_default() {
        for why in ["codex was not found", "exit status 2", "timed out after 5s"] {
            let resolved = resolve("terra", &Err(why.to_string()));
            assert_eq!(resolved.model, None, "{why}");
            let notice = resolved.notice.unwrap();
            assert!(notice.contains(why), "{notice}");
            assert!(
                notice.contains("Codex uses its own default model"),
                "{notice}"
            );
        }
    }

    #[test]
    fn output_that_is_not_a_catalog_is_an_unreadable_one() {
        for output in [
            "",
            "not json",
            "[]",
            "{}",
            "{\"models\":3}",
            "{\"models\":{}}",
        ] {
            let resolved = resolve("terra", &Ok(output.to_string()));
            assert_eq!(resolved.model, None, "{output:?}");
            assert!(
                resolved.notice.unwrap().contains("unparseable output"),
                "{output:?}"
            );
        }
        let resolved = resolve("gpt-6.1-terra", &Ok("garbage".to_string()));
        assert_eq!(model(&resolved), Some("gpt-6.1-terra"));
    }

    #[test]
    fn a_warning_line_ahead_of_the_json_is_tolerated() {
        let output = format!(
            "warning: something\n{}\n",
            r#"{"models":[{"slug":"gpt-6.1-terra"}]}"#
        );
        assert_eq!(model(&resolve("terra", &Ok(output))), Some("gpt-6.1-terra"));
    }

    #[test]
    fn objects_missing_priority_or_visibility_or_slug_are_tolerated() {
        let catalog = Ok(r#"{"models":[
            {"slug":"gpt-5-terra"},
            {"visibility":"list","priority":0},
            {"slug":42,"visibility":"list"},
            {"slug":"gpt-6-terra","visibility":"list"},
            "a string",
            null
        ]}"#
        .to_string());
        // No visibility reads as visible; no priority never blocks a match.
        assert_eq!(model(&resolve("terra", &catalog)), Some("gpt-6-terra"));
        assert_eq!(
            model(&resolve("gpt-5-terra", &catalog)),
            Some("gpt-5-terra")
        );
    }

    #[test]
    fn a_slug_with_no_version_loses_to_one_with_a_version_and_priority_orders_the_rest() {
        let catalog = Ok(r#"{"models":[
            {"slug":"terra-preview","priority":0},{"slug":"gpt-1-terra","priority":9}
        ]}"#
        .to_string());
        assert_eq!(model(&resolve("terra", &catalog)), Some("gpt-1-terra"));

        let unversioned = Ok(r#"{"models":[
            {"slug":"terra-b","priority":5},{"slug":"terra-a","priority":1},{"slug":"terra-c"}
        ]}"#
        .to_string());
        assert_eq!(model(&resolve("terra", &unversioned)), Some("terra-a"));
    }

    #[test]
    fn the_answer_does_not_depend_on_the_order_of_the_catalog() {
        let forward = r#"{"models":[{"slug":"gpt-6.1-terra"},{"slug":"gpt-6.1-terra-x"}]}"#;
        let backward = r#"{"models":[{"slug":"gpt-6.1-terra-x"},{"slug":"gpt-6.1-terra"}]}"#;
        assert_eq!(
            resolve("terra", &Ok(forward.to_string())).model,
            resolve("terra", &Ok(backward.to_string())).model
        );
    }

    #[test]
    fn unknown_extra_fields_and_other_top_level_keys_are_ignored() {
        let output = r#"{"version":3,"models":[{"slug":"gpt-6.1-terra","novel":true}],"more":[]}"#;
        assert_eq!(
            model(&resolve("terra", &Ok(output.to_string()))),
            Some("gpt-6.1-terra")
        );
    }

    #[test]
    fn a_leading_byte_order_mark_is_tolerated_alone_and_after_a_log_line() {
        let json = r#"{"models":[{"slug":"gpt-6.1-terra"}]}"#;
        for output in [
            format!("\u{feff}{json}"),
            format!("\u{feff}{json}\n"),
            format!("warning: something\n\u{feff}{json}\n"),
            format!("\n\n  \u{feff}{json}"),
            format!("\u{feff}\u{feff}{json}"),
        ] {
            assert_eq!(
                model(&resolve("terra", &Ok(output.clone()))),
                Some("gpt-6.1-terra"),
                "{output:?}"
            );
        }
        // A mark with no JSON behind it is still not a catalog.
        assert!(
            resolve("terra", &Ok("\u{feff}".to_string()))
                .notice
                .unwrap()
                .contains("unparseable output")
        );
    }
}
