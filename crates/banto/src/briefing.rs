//! Rendering a brigade member's role briefing.
//!
//! Extracted from the emporium because a Codex member's briefing is not
//! rendered by the process that launches it: Claude takes its briefing on the
//! launch argv (`--append-system-prompt`), but Codex has no equivalent flag,
//! so banto injects a `SessionStart` hook and the briefing is rendered later,
//! in the separate `banto _hook` process the hook spawns
//! (docs/notes/codex-briefing-spike.md). Both paths render the same text from
//! the same template, which is the reason this is one module rather than two
//! copies.

use banto_core::model::{BrigadeId, BrigadeRole};
use banto_io::store::Store;

/// Facts a Codex member needs that a Claude one does not, appended to the
/// operator's own template by [`with_codex_addendum`].
///
/// Both are measured, not defensive (docs/notes/codex-briefing-spike.md).
/// Codex defers MCP tools: banto's three are absent from the tool set the
/// model is given, so a member that waits to be offered them waits forever —
/// but a tool named outright is dispatched normally. And Codex's own
/// developer prompt casts the model as the primary agent of *its* multi-agent
/// feature, which a brigade briefing reads as license to spawn sub-agents for
/// work the cell's own peers exist to do.
///
/// Kept out of the operator's configurable template deliberately: the
/// template is where they say what a role is *for*, and these are product
/// facts they should not have to know or maintain.
const CODEX_ADDENDUM: &str = "\
banto's tools are not listed among your available tools — this product loads \
MCP tools lazily. `mcp__banto__check_messages`, `mcp__banto__send_to_peer` \
and `mcp__banto__brigade_status` work when called by those exact names, so \
call them; do not conclude from their absence that you have no channel.

The brigade is not this product's own multi-agent feature, and you are not \
its primary agent. Do not spawn sub-agents for brigade work: work through \
your brigade peers, via banto.";

/// The member's briefing with the Codex-only facts appended.
///
/// An empty template still yields the addendum: the operator clearing the
/// template is them declining to give a *role*, not declining to tell the
/// model how to reach its cell.
pub(crate) fn with_codex_addendum(briefing: &str) -> String {
    if briefing.is_empty() {
        return CODEX_ADDENDUM.to_string();
    }
    format!("{briefing}\n\n{CODEX_ADDENDUM}")
}

/// The other members this one can address, newest roster from the store.
///
/// Read at launch rather than taken from the core's own view of the cell:
/// `{peers}` has to name the members that exist *now*, which is later than
/// any decision the core made about the brigade (adding a Worker changes it,
/// and a Director resumed into an existing brigade never went through
/// formation at all).
pub(crate) fn peers_of(store: &Store, brigade_id: BrigadeId, role: BrigadeRole) -> Vec<String> {
    store
        .brigade_members(brigade_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|member| member.role != role)
        .map(|member| member.token)
        .collect()
}

/// Substitute `{brigade}` / `{token}` / `{peers}` into a briefing template.
///
/// Plain string replacement, deliberately: this is banto's own config text
/// being filled in for banto's own launch, not a templating language, and
/// an unrecognized `{...}` is left alone rather than treated as an error —
/// the same leniency the rest of the config layer promises. A member with
/// no addressable peers yet renders as "none yet" rather than an empty gap,
/// so the sentence still reads as a sentence.
pub(crate) fn render(
    template: &str,
    brigade_id: BrigadeId,
    token: &str,
    peers: &[String],
) -> String {
    let peers = if peers.is_empty() {
        "none yet".to_string()
    } else {
        peers.join(", ")
    };
    template
        .replace("{brigade}", &brigade_id.to_string())
        .replace("{token}", token)
        .replace("{peers}", &peers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_every_placeholder() {
        let out = render(
            "brigade {brigade}, you are {token}, peers: {peers}",
            7,
            "worker-1",
            &["director".to_string(), "worker-2".to_string()],
        );
        assert_eq!(
            out,
            "brigade 7, you are worker-1, peers: director, worker-2"
        );
    }

    #[test]
    fn an_empty_roster_still_reads_as_a_sentence() {
        assert_eq!(
            render("peers: {peers}.", 1, "director", &[]),
            "peers: none yet."
        );
    }

    #[test]
    fn the_codex_addendum_names_every_tool_a_member_cannot_see() {
        let out = with_codex_addendum("you are worker-1");
        assert!(out.starts_with("you are worker-1\n\n"));
        for tool in [
            "mcp__banto__check_messages",
            "mcp__banto__send_to_peer",
            "mcp__banto__brigade_status",
        ] {
            assert!(out.contains(tool), "addendum must name {tool}");
        }
    }

    #[test]
    fn clearing_the_template_still_leaves_the_member_a_channel() {
        assert_eq!(with_codex_addendum(""), CODEX_ADDENDUM);
    }

    #[test]
    // Lenient like the rest of the config layer: a typo in the operator's own
    // template is not worth failing a launch over.
    fn an_unknown_placeholder_is_left_alone() {
        assert_eq!(
            render("{token} {nope}", 1, "director", &[]),
            "director {nope}"
        );
    }
}
