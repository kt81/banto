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

use banto_core::config::{BrigadeConfig, GoinkyoConsult};
use banto_core::model::{AgentKind, BrigadeId, BrigadeRole};
use banto_io::claude_briefing::{BriefingPattern, PEERS_HOLE};
use banto_io::codex_briefing::BRIEFING_LAST_SENTENCE;
use banto_io::store::Store;

/// banto's tools any member may call, by the names Codex dispatches them under.
const CODEX_MEMBER_TOOLS: [&str; 3] = [
    "mcp__banto__check_messages",
    "mcp__banto__send_to_peer",
    "mcp__banto__brigade_status",
];

/// The tools only a Director may call. A Worker is not told them: calling one
/// would only get it a tool error. The list was once a fixed sentence naming the
/// three above, and the four Director tools added after it never reached it, so a
/// Codex Director had no name to call them by; a test in `crate::mcp` now checks
/// every advertised tool against these two lists.
const CODEX_DIRECTOR_TOOLS: [&str; 4] = [
    "mcp__banto__request_decision",
    "mcp__banto__name_member",
    "mcp__banto__consult_goinkyo",
    "mcp__banto__dismiss_goinkyo",
];

/// Facts a Codex member needs that a Claude one does not, appended to the
/// operator's own template by [`with_codex_addendum`].
///
/// Both are measured, not defensive (docs/notes/codex-briefing-spike.md).
/// Codex defers MCP tools: banto's are absent from the tool set the model is
/// given, so a member that waits to be offered them waits forever — but a tool
/// named outright is dispatched normally. And Codex's own developer prompt
/// casts the model as the primary agent of *its* multi-agent feature, which a
/// brigade briefing reads as license to spawn sub-agents for work the cell's
/// own peers exist to do.
///
/// The last sentence is for a history that carries the briefing more than
/// once. `banto _hook` withholds a copy the session already holds
/// (`banto_io::codex_briefing`), but not every case can be proven, so a member
/// can still see two — and the roster in the older one may be out of date. It
/// is also how that module recognises a banto briefing among a history's
/// developer messages, so it must stay the very last thing in the text:
/// whatever else is appended to a briefing goes before the addendum.
///
/// Kept out of the operator's configurable template deliberately: the
/// template is where they say what a role is *for*, and these are product
/// facts they should not have to know or maintain.
fn codex_addendum(role: BrigadeRole) -> String {
    let mut tools = CODEX_MEMBER_TOOLS.to_vec();
    if role == BrigadeRole::Director {
        tools.extend(CODEX_DIRECTOR_TOOLS);
    }
    let named: Vec<String> = tools.iter().map(|tool| format!("`{tool}`")).collect();
    let (last, rest) = named.split_last().expect("every role may call a tool");
    format!(
        "banto's tools are not listed among your available tools — this product loads \
         MCP tools lazily. {} and {last} work when called by those exact names, so \
         call them; do not conclude from their absence that you have no channel.\n\n\
         The brigade is not this product's own multi-agent feature, and you are not \
         its primary agent. Do not spawn sub-agents for brigade work: work through \
         your brigade peers, via banto.\n\n\
         {BRIEFING_LAST_SENTENCE}",
        rest.join(", ")
    )
}

/// The member's briefing with the Codex-only facts appended.
///
/// An empty template still yields the addendum: the operator clearing the
/// template is them declining to give a *role*, not declining to tell the
/// model how to reach its cell.
pub(crate) fn with_codex_addendum(briefing: &str, role: BrigadeRole) -> String {
    let addendum = codex_addendum(role);
    if briefing.is_empty() {
        return addendum;
    }
    format!("{briefing}\n\n{addendum}")
}

// ---------------------------------------------------------------------------
// The consultation paragraph a Director is given after its template.
//
// Each block is written once and the four levels are joins of them with single
// spaces, so a fix to a sentence reaches every level that says it. The macros
// exist because `concat!` takes literals only.
// ---------------------------------------------------------------------------

macro_rules! lead {
    ($level:literal) => {
        concat!(
            "When to consult the Goinkyo \u{2014} the operator's setting, goinkyo_consult = \"",
            $level,
            "\"."
        )
    };
}

macro_rules! arb {
    () => {
        "Call consult_goinkyo with kind \"arbitration\" when you and a Worker disagree and one more exchange has not settled it, when two reports contradict each other and no check you can name decides between them, or when a choice is yours and you cannot make it."
    };
}

macro_rules! des {
    () => {
        "Call it with kind \"design\" before the shape of a lasting change is settled. A lasting change adds, replaces or removes something later work will rest on or the operator will live with \u{2014} a setting, a command, a stored or exchanged format, a mechanism, a dependency \u{2014} or changes what existing behaviour is meant to be; its shape is whether, when and how. When the operator will decide it on your recommendation, that is before you give the recommendation, and the question counts, not your answer: if a course you are weighing makes such a change, consult even when you will recommend against it. When it is yours to decide, it is before the errand that carries it out goes to a Worker, or before you write its first line. A shape needs no consultation when the operator stated it themselves \u{2014} consult only on what they left open \u{2014} or when an earlier consultation covered it. Three more things need none: one more of a kind that already exists, made the way the others are (another entry in a list, another case in a table); a mechanical edit; and a repair that makes something do what it was already meant to do, unless the repair itself makes a lasting change."
    };
}

macro_rules! after {
    () => {
        "A shape the operator has approved is settled at this level even if no consultation came first: the time for one was before you asked."
    };
}

macro_rules! dec {
    () => {
        "And call it with kind \"design\", giving the courses you weighed as `alternatives`, whenever you \u{2014} not the operator \u{2014} choose one course over another in the work itself: which of several fixes to take, what to leave out, whether to live with a limitation or remove it, what to advise the operator to do first. Do it before you act on the choice or recommend it to the operator. A choice counts when the outcome differs by it \u{2014} what the code or its documents end up being, or what the operator is advised to do. Three things do not count: what a test or a measurement would settle, which is a Worker's errand; what one small edit undoes with nothing resting on it, such as a name; and how you run the brigade \u{2014} who gets which errand, in what order and in how many steps, what you ask for back. A consultation about a choice of course is over once it is answered: dismiss it. At this level approval does not stand in for a consultation: a shape you proposed and the operator approved with none behind it is consulted before it is built, and any change goes back to them."
    };
}

macro_rules! only {
    () => {
        "Nothing else at this level: designs and choices of course are yours to make."
    };
}

macro_rules! tail {
    () => {
        "Being sure of your answer exempts nothing: that is the case this exists for. A consultation is a separate session that reads the code for itself; it takes minutes and costs more than a Worker's errand. The operator chose this level knowing that, so do not economise below it, and do not consult beyond it unless the operator asks. Tell the operator in one line when you consult, and in one line why when you recommend or go ahead without consulting on something that fits a case above."
    };
}

macro_rules! asked {
    () => {
        "A consultation the operator asks for is filed with kind \"arbitration\" when it is about a disagreement or an impasse, and with kind \"design\" otherwise."
    };
}

macro_rules! dissent {
    () => {
        "When you go against what the Goinkyo advised, tell the operator so, and why, in one line."
    };
}

macro_rules! off {
    () => {
        "Only when the operator asks you to: do not call consult_goinkyo on your own initiative. If a consultation is already open, finish it and dismiss it as usual."
    };
}

const CONSULT_OFF: &str = concat!(lead!("off"), " ", off!(), " ", asked!(), " ", dissent!());

const CONSULT_DISPUTES: &str = concat!(
    lead!("disputes"),
    " ",
    arb!(),
    " ",
    only!(),
    " ",
    tail!(),
    " ",
    asked!(),
    " ",
    dissent!()
);

const CONSULT_DESIGN: &str = concat!(
    lead!("design"),
    " ",
    arb!(),
    " ",
    des!(),
    " ",
    after!(),
    " ",
    tail!(),
    " ",
    asked!(),
    " ",
    dissent!()
);

const CONSULT_DECISIONS: &str = concat!(
    lead!("decisions"),
    " ",
    arb!(),
    " ",
    des!(),
    " ",
    dec!(),
    " ",
    tail!(),
    " ",
    asked!(),
    " ",
    dissent!()
);

/// The paragraph that tells a Director the operator's consultation policy.
pub(crate) fn consult_paragraph(level: GoinkyoConsult) -> &'static str {
    match level {
        GoinkyoConsult::Off => CONSULT_OFF,
        GoinkyoConsult::Disputes => CONSULT_DISPUTES,
        GoinkyoConsult::Design => CONSULT_DESIGN,
        GoinkyoConsult::Decisions => CONSULT_DECISIONS,
    }
}

/// A member's complete role briefing, or `None` when there is nothing to give
/// it. The one place a briefing is assembled, for both launch paths: the Claude
/// launch (`crate::embedded::emporium`) and the Codex hook (`crate::hook`) each
/// ask for it, so they cannot disagree about what a member is told.
///
/// The role's template rendered; for a Director whose template is not empty and
/// whose `goinkyo_consult` is not empty, the consultation paragraph after it; and
/// on Codex, last, the addendum. An empty template is the operator declining to
/// give the role any text, and the paragraph does not outlive it: Claude then
/// gets no flag at all, Codex only the addendum. Workers and the Goinkyo never
/// get a paragraph.
pub(crate) fn compose(
    brigade: &BrigadeConfig,
    agent: AgentKind,
    role: BrigadeRole,
    brigade_id: BrigadeId,
    token: &str,
    peers: &[String],
    request: Option<&str>,
) -> Option<String> {
    let rendered = brigade
        .prompt_for(role)
        .map(|template| render(template, brigade_id, token, peers, request));
    let mut text = rendered.unwrap_or_default();
    if role == BrigadeRole::Director
        && !text.is_empty()
        && let Some(level) = brigade.goinkyo_consult()
    {
        text.push_str("\n\n");
        text.push_str(consult_paragraph(level));
    }
    match agent {
        AgentKind::Codex => Some(with_codex_addendum(&text, role)),
        AgentKind::ClaudeCode => (!text.is_empty()).then_some(text),
    }
}

/// [`compose`] for a Claude member, as a pattern to compare a recorded briefing
/// against: the same text, with `{peers}` left a hole instead of filled from a
/// roster (`banto_io::claude_briefing`). Rendering through [`compose`] with a
/// marker in place of the roster keeps this the one place the template, the
/// consultation paragraph and the empty-template rule are applied.
///
/// `None` exactly when [`compose`] gives no briefing, so the two cannot
/// disagree about whether there is one to compare.
pub(crate) fn compose_pattern(
    brigade: &BrigadeConfig,
    role: BrigadeRole,
    brigade_id: BrigadeId,
    token: &str,
    request: Option<&str>,
) -> Option<BriefingPattern> {
    compose(
        brigade,
        AgentKind::ClaudeCode,
        role,
        brigade_id,
        token,
        &[PEERS_HOLE.to_string()],
        request,
    )
    .map(|marked| BriefingPattern::from_marked(&marked, peers_ends))
}

/// The members this one can address, newest roster from the store — the
/// same [`BrigadeRole::can_reach`] rule [`crate::mcp::validate_target`]
/// gates actual delivery with and `tool_brigade_status` groups its roster
/// by, not "every member whose role isn't mine": that would name a member
/// this one could never actually reach once a third role exists that isn't
/// mutually addressable with this one's own (e.g. a Worker and a Goinkyo).
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
        .filter(|member| role.can_reach(member.role))
        .map(|member| member.token)
        .collect()
}

/// What `{peers}` renders to when the member has no addressable peer yet.
const NO_PEERS: &str = "none yet";

/// What `{peers}` puts between two tokens.
const PEERS_SEPARATOR: &str = ", ";

/// The longest a `{peers}` text is looked at: a hundred Workers is about a
/// kilobyte, so beyond this no real roster lives. A longer one reads as a
/// changed briefing, which costs one redundant flag; what the cap buys is that
/// matching a recorded copy never reads more than this per hole, whatever the
/// transcript holds.
const MAX_PEERS_LEN: usize = 4096;

/// Whether `byte` may be in a member token: the characters banto's own tokens
/// are made of (`director`, `goinkyo`, `worker-<n>`).
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// The byte length of every prefix of `text` that [`render`] can put where
/// `{peers}` stands, ascending: "none yet", or one or more member tokens joined
/// by [`PEERS_SEPARATOR`], a token being one or more of `[A-Za-z0-9_-]`. Looks
/// no further than [`MAX_PEERS_LEN`], and in one pass.
///
/// It sits beside [`render`] because a recorded briefing's `{peers}` is compared
/// against it ([`compose_pattern`]): a looser rule would let that comparison step
/// over text the operator later deleted from the template (`worker-1.Never-merge`
/// would pass for a roster), a tighter one would call a changed roster a changed
/// briefing.
pub(crate) fn peers_ends(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let limit = bytes.len().min(MAX_PEERS_LEN);
    let mut ends = Vec::new();
    let mut pos = 0;
    loop {
        let token_start = pos;
        while pos < limit && is_token_byte(bytes[pos]) {
            pos += 1;
            // Every prefix that ends inside a token is a roster of its own.
            ends.push(pos);
        }
        if pos == token_start || !text[pos..].starts_with(PEERS_SEPARATOR) {
            break;
        }
        pos += PEERS_SEPARATOR.len();
    }
    // "none yet" begins with the token "none", so its end is past every end the
    // tokens found: pushing it last keeps the list ascending.
    if text.starts_with(NO_PEERS) {
        ends.push(NO_PEERS.len());
    }
    ends
}

/// Whether the whole of `text` is something [`render`] can put where `{peers}`
/// stands: [`peers_ends`] reaches its end.
#[cfg(test)]
pub(crate) fn is_peers_text(text: &str) -> bool {
    peers_ends(text).last() == Some(&text.len())
}

/// Substitute `{brigade}` / `{token}` / `{peers}` — and, when `request` is
/// given, `{request}` — into a briefing template.
///
/// Plain string replacement, deliberately: this is banto's own config text
/// being filled in for banto's own launch, not a templating language, and
/// an unrecognized `{...}` is left alone rather than treated as an error —
/// the same leniency the rest of the config layer promises. A member with
/// no addressable peers yet renders as "none yet" rather than an empty gap,
/// so the sentence still reads as a sentence.
///
/// `request` is `None` for a Director or a Worker — their templates have no
/// `{request}` to begin with, and leaving the substitution out entirely
/// (rather than passing an empty string) means a template that *did*
/// happen to contain the literal text `{request}` would render unchanged
/// for them, same as any other placeholder banto doesn't recognize in that
/// role's template.
pub(crate) fn render(
    template: &str,
    brigade_id: BrigadeId,
    token: &str,
    peers: &[String],
    request: Option<&str>,
) -> String {
    let peers = if peers.is_empty() {
        NO_PEERS.to_string()
    } else {
        peers.join(PEERS_SEPARATOR)
    };
    let rendered = template
        .replace("{brigade}", &brigade_id.to_string())
        .replace("{token}", token)
        .replace("{peers}", &peers);
    match request {
        Some(request) => rendered.replace("{request}", request),
        None => rendered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_every_placeholder() {
        let out = render(
            "brigade {brigade}, you are {token}, peers: {peers}, request: {request}",
            7,
            "worker-1",
            &["director".to_string(), "worker-2".to_string()],
            Some("/tmp/goinkyo/7.txt"),
        );
        assert_eq!(
            out,
            "brigade 7, you are worker-1, peers: director, worker-2, request: /tmp/goinkyo/7.txt"
        );
    }

    #[test]
    fn request_placeholder_is_left_alone_when_none_is_given() {
        // A Director/Worker template happening to contain the literal text
        // `{request}` must render unchanged for them, the same as any other
        // placeholder banto doesn't recognize in that role's template — see
        // `render`'s own doc for why `None` means "don't touch it" rather
        // than "replace it with an empty string".
        assert_eq!(
            render("no request here: {request}", 1, "director", &[], None),
            "no request here: {request}"
        );
    }

    #[test]
    fn an_empty_roster_still_reads_as_a_sentence() {
        assert_eq!(
            render("peers: {peers}.", 1, "director", &[], None),
            "peers: none yet."
        );
    }

    #[test]
    fn peers_of_only_names_members_this_role_can_actually_reach() {
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(brigade_id, "director", BrigadeRole::Director, None)
            .unwrap();
        store
            .add_brigade_member(brigade_id, "worker-1", BrigadeRole::Worker, None)
            .unwrap();
        store
            .add_brigade_member(brigade_id, "goinkyo", BrigadeRole::Goinkyo, None)
            .unwrap();

        let mut director_peers = peers_of(&store, brigade_id, BrigadeRole::Director);
        director_peers.sort();
        assert_eq!(
            director_peers,
            vec!["goinkyo".to_string(), "worker-1".to_string()]
        );

        assert_eq!(
            peers_of(&store, brigade_id, BrigadeRole::Worker),
            vec!["director".to_string()],
            "a Worker must never be told a Goinkyo is one of its peers"
        );
        assert_eq!(
            peers_of(&store, brigade_id, BrigadeRole::Goinkyo),
            vec!["director".to_string()],
            "a Goinkyo must never be told a Worker is one of its peers"
        );
    }

    const MEMBER_TOOLS: [&str; 3] = [
        "mcp__banto__check_messages",
        "mcp__banto__send_to_peer",
        "mcp__banto__brigade_status",
    ];

    const DIRECTOR_TOOLS: [&str; 4] = [
        "mcp__banto__request_decision",
        "mcp__banto__name_member",
        "mcp__banto__consult_goinkyo",
        "mcp__banto__dismiss_goinkyo",
    ];

    #[test]
    fn the_codex_addendum_names_every_tool_a_worker_cannot_see() {
        let out = with_codex_addendum("you are worker-1", BrigadeRole::Worker);
        assert!(out.starts_with("you are worker-1\n\n"));
        for tool in MEMBER_TOOLS {
            assert!(out.contains(tool), "addendum must name {tool}");
        }
    }

    #[test]
    fn both_roles_are_told_the_latest_briefing_replaces_earlier_ones() {
        for role in [BrigadeRole::Director, BrigadeRole::Worker] {
            for template in ["you are someone", ""] {
                let out = with_codex_addendum(template, role);
                assert!(
                    out.ends_with(
                        "If this conversation holds more than one banto briefing, the most recent one is current and replaces the earlier ones."
                    ),
                    "{role:?}: {out}"
                );
            }
        }
    }

    #[test]
    fn a_codex_director_is_told_the_names_of_its_own_tools_too() {
        let out = with_codex_addendum("you are the director", BrigadeRole::Director);
        for tool in MEMBER_TOOLS.iter().chain(&DIRECTOR_TOOLS) {
            assert!(out.contains(tool), "addendum must name {tool}");
        }
    }

    #[test]
    fn a_codex_worker_is_not_told_the_directors_tools() {
        // A Worker that called one would only get a tool error.
        let out = with_codex_addendum("you are worker-1", BrigadeRole::Worker);
        for tool in DIRECTOR_TOOLS {
            assert!(!out.contains(tool), "a Worker must not be offered {tool}");
        }
    }

    #[test]
    fn clearing_the_template_still_leaves_the_member_a_channel() {
        for role in [BrigadeRole::Director, BrigadeRole::Worker] {
            let out = with_codex_addendum("", role);
            assert!(out.starts_with("banto's tools"), "{out}");
            assert!(out.contains("mcp__banto__send_to_peer"), "{out}");
        }
    }

    #[test]
    // Lenient like the rest of the config layer: a typo in the operator's own
    // template is not worth failing a launch over.
    fn an_unknown_placeholder_is_left_alone() {
        assert_eq!(
            render("{token} {nope}", 1, "director", &[], None),
            "director {nope}"
        );
    }

    // --- compose -----------------------------------------------------------

    fn config_with(director: &str, consult: &str) -> BrigadeConfig {
        BrigadeConfig {
            director_prompt: director.to_string(),
            goinkyo_consult: consult.to_string(),
            ..BrigadeConfig::default()
        }
    }

    fn compose_director(config: &BrigadeConfig, agent: AgentKind) -> Option<String> {
        compose(
            config,
            agent,
            BrigadeRole::Director,
            7,
            "director",
            &["worker-1".to_string()],
            None,
        )
    }

    #[test]
    fn a_level_change_is_delivered_on_resume_whichever_way_it_went() {
        // The renderer and the dedupe are separate modules that must agree on
        // what a banto briefing is; this pairs the real output of one with the
        // real verdict of the other.
        let rollout = |texts: &[&str]| {
            let lines: Vec<String> = texts
                .iter()
                .map(|text| {
                    serde_json::json!({"type": "response_item", "payload": {
                        "type": "message", "role": "developer",
                        "content": [{"type": "input_text", "text": text}]}})
                    .to_string()
                })
                .collect();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("rollout.jsonl");
            std::fs::write(
                &path,
                format!(
                    "{}
",
                    lines.join(
                        "
"
                    )
                ),
            )
            .unwrap();
            (dir, path)
        };
        let render = |level: &str| {
            compose_director(
                &config_with(&BrigadeConfig::default().director_prompt, level),
                AgentKind::Codex,
            )
            .unwrap()
        };
        let (x, y) = (render("decisions"), render("off"));
        assert_ne!(x, y);

        let (_dir, path) = rollout(&[&y, &x]);
        assert_eq!(
            banto_io::codex_briefing::verdict(Some("resume"), Some(&path), &y),
            banto_io::codex_briefing::Verdict::Deliver,
            "the history ends with X, so Y must be told again"
        );
        let (_dir, path) = rollout(&[&x, &y]);
        assert_eq!(
            banto_io::codex_briefing::verdict(Some("resume"), Some(&path), &y),
            banto_io::codex_briefing::Verdict::Suppress,
            "the history ends with Y itself"
        );
    }

    #[test]
    fn every_level_names_itself_and_off_leaves_the_tool_to_the_operator() {
        for (level, name) in [
            (GoinkyoConsult::Off, "off"),
            (GoinkyoConsult::Disputes, "disputes"),
            (GoinkyoConsult::Design, "design"),
            (GoinkyoConsult::Decisions, "decisions"),
        ] {
            let text = consult_paragraph(level);
            assert!(
                text.contains(&format!("goinkyo_consult = \"{name}\"")),
                "{name}: {text}"
            );
            // The tool is never forbidden: off is an instruction about whose
            // initiative a consultation is, and the operator's own request
            // still gets one.
            assert!(!text.contains("refuses"), "{name}: {text}");
            assert_eq!(
                text.contains(" Only when the operator asks you to: do not call"),
                name == "off"
            );
        }
    }

    #[test]
    fn the_levels_are_joins_of_the_same_blocks() {
        let off = consult_paragraph(GoinkyoConsult::Off);
        let disputes = consult_paragraph(GoinkyoConsult::Disputes);
        let design = consult_paragraph(GoinkyoConsult::Design);
        let decisions = consult_paragraph(GoinkyoConsult::Decisions);

        // Every level but off carries the arbitration trigger; only the three
        // that consult on their own initiative carry the cost paragraph.
        let arbitration =
            "Call consult_goinkyo with kind \"arbitration\" when you and a Worker disagree";
        let cost = "A consultation is a separate session that reads the code for itself";
        for text in [disputes, design, decisions] {
            assert!(text.contains(arbitration));
            assert!(text.contains(cost));
        }
        assert!(!off.contains(arbitration) && !off.contains(cost));

        // What a level that was asked to consult must say either way: the
        // operator's own request and a dissent from the Goinkyo's advice.
        for text in [off, disputes, design, decisions] {
            assert!(text.contains("A consultation the operator asks for is filed with kind"));
            assert!(text.ends_with(
                "When you go against what the Goinkyo advised, tell the operator so, and why, in one line."
            ));
        }

        // Design adds the lasting-change rule and what follows an approval;
        // decisions adds the choice rule on top; disputes has neither.
        assert!(design.contains("before the shape of a lasting change is settled"));
        assert!(decisions.contains("before the shape of a lasting change is settled"));
        assert!(!disputes.contains("lasting change"));
        assert!(!off.contains("lasting change"));
        assert!(design.contains("the time for one was before you asked"));
        // Decisions leaves the approval sentence out: its own rule says approval
        // does not stand in for a consultation, which the sentence would contradict.
        assert!(!decisions.contains("the time for one was before you asked"));
        assert!(!disputes.contains("the time for one was before you asked"));
        assert!(decisions.contains("giving the courses you weighed as `alternatives`"));
        assert!(!design.contains("giving the courses you weighed"));
        assert!(decisions.contains("approval does not stand in for a consultation"));
        assert!(!design.contains("approval does not stand in for a consultation"));
        assert!(disputes.contains("Nothing else at this level"));
        assert!(!design.contains("Nothing else at this level"));

        // Off says only whose initiative is withheld; it never forbids the tool.
        assert!(off.contains("do not call consult_goinkyo on your own initiative"));
        assert!(!off.contains("refuses"));
        assert!(decisions.len() > design.len() && design.len() > disputes.len());
        assert!(disputes.len() > off.len());
    }

    #[test]
    fn each_paragraph_is_one_run_of_single_spaced_text() {
        for level in [
            GoinkyoConsult::Off,
            GoinkyoConsult::Disputes,
            GoinkyoConsult::Design,
            GoinkyoConsult::Decisions,
        ] {
            let text = consult_paragraph(level);
            assert!(!text.contains('\n') && !text.contains("  "), "{text}");
            assert!(!text.ends_with(' '), "{text}");
        }
    }

    #[test]
    fn a_claude_director_gets_its_template_then_the_level_paragraph() {
        let config = config_with("You lead {brigade}: {peers}.", "design");
        let out = compose_director(&config, AgentKind::ClaudeCode).unwrap();
        assert_eq!(
            out,
            format!(
                "You lead 7: worker-1.\n\n{}",
                consult_paragraph(GoinkyoConsult::Design)
            )
        );
    }

    #[test]
    fn the_pattern_matches_the_briefing_for_any_roster_and_nothing_that_differs_elsewhere() {
        let config = config_with(
            "You lead {brigade} as {token}: {peers}. Be brief.",
            "design",
        );
        let pattern = compose_pattern(&config, BrigadeRole::Director, 7, "director", None).unwrap();
        for peers in [
            vec![],
            vec!["worker-1".to_string()],
            vec![
                "worker-1".to_string(),
                "worker-2".to_string(),
                "goinkyo".to_string(),
            ],
        ] {
            let real = compose(
                &config,
                AgentKind::ClaudeCode,
                BrigadeRole::Director,
                7,
                "director",
                &peers,
                None,
            )
            .unwrap();
            assert!(pattern.matches(&real), "{real}");
        }
        // The hole is only where `{peers}` is: another brigade, another token or
        // another template is a different briefing.
        let other_brigade = compose_director_for(&config, 8, "director");
        assert!(!pattern.matches(&other_brigade));
        let other_token = compose_director_for(&config, 7, "worker-1");
        assert!(!pattern.matches(&other_token));
        let changed = config_with(
            "You lead {brigade} as {token}: {peers}. Be thorough.",
            "design",
        );
        assert!(!pattern.matches(&compose_director_for(&changed, 7, "director")));
        let other_level = config_with(
            "You lead {brigade} as {token}: {peers}. Be brief.",
            "decisions",
        );
        assert!(!pattern.matches(&compose_director_for(&other_level, 7, "director")));
    }

    #[test]
    fn matching_a_recorded_briefing_stays_fast_whatever_the_transcript_holds() {
        use banto_io::claude_briefing::PEERS_HOLE;
        let token_run = "a".repeat(1 << 20);
        let started = std::time::Instant::now();
        // A template ending in the roster, against a megabyte with no end to it.
        let tail = BriefingPattern::from_marked(&format!("Workers: {PEERS_HOLE}"), peers_ends);
        assert!(!tail.matches(&format!("Workers: {token_run}")));
        assert!(!tail.matches(&format!("Workers: {token_run}!")));
        // Holes in a row, each able to end at every character of the run.
        let rows = BriefingPattern::from_marked(&PEERS_HOLE.repeat(5), peers_ends);
        assert!(!rows.matches(&format!("{token_run}!")));
        assert!(!rows.matches(&token_run));
        let rows = BriefingPattern::from_marked(&format!("{}!", PEERS_HOLE.repeat(5)), peers_ends);
        assert!(!rows.matches(&format!("{}?", &token_run[..MAX_PEERS_LEN * 3])));
        // A roster of any real size is still found among all that.
        let roster = (1..=100)
            .map(|n| format!("worker-{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        assert!(roster.len() < MAX_PEERS_LEN);
        let ok = BriefingPattern::from_marked(&format!("A {PEERS_HOLE}. B"), peers_ends);
        assert!(ok.matches(&format!("A {roster}. B")));
        // A generous wall-clock bound: the work done was cubic or worse before.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn what_render_puts_where_peers_stands_is_always_a_peers_text() {
        let tokens = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        for peers in [
            tokens(&[]),
            tokens(&["director"]),
            tokens(&["worker-1", "worker-2", "goinkyo"]),
            tokens(&["worker-10", "worker-2"]),
        ] {
            let rendered = render("{peers}", 1, "x", &peers, None);
            assert!(is_peers_text(&rendered), "{rendered:?}");
        }
    }

    #[test]
    fn only_a_roster_is_a_peers_text() {
        for yes in [
            "none yet",
            "worker-1",
            "worker-1, worker-2",
            "Worker_3, a-b",
        ] {
            assert!(is_peers_text(yes), "{yes:?}");
        }
        for no in [
            "",
            "none yet.",
            "worker-1 worker-2",
            "a.b, c",
            "[worker-1]",
            "worker-1.",
            "worker-1.Never-merge-without-asking",
            "worker-1,worker-2",
            "worker-1, ",
            ", worker-1",
            "worker-1,  worker-2",
            "worker-1\nworker-2",
            "worker-1. Never merge without asking me",
            "worker-1, worker-2.\n\nThey are live agent sessions.",
        ] {
            assert!(!is_peers_text(no), "{no:?}");
        }
    }

    /// The shipped Director template and consultation paragraph, with the
    /// sentence after `{peers}` as the operator may have edited it.
    fn shipped_director_pattern_and_briefing(recorded_extra: &str) -> (BriefingPattern, String) {
        let config = BrigadeConfig::default();
        let pattern = compose_pattern(&config, BrigadeRole::Director, 7, "director", None).unwrap();
        let template = config.director_prompt.clone();
        let (before, after) = template.split_once("{peers}").unwrap();
        // What was recorded: the same template with a sentence of its own right
        // after the roster, and a roster that is not today's.
        let recorded_template = format!("{before}{{peers}}{recorded_extra}{after}");
        let recorded = compose(
            &BrigadeConfig {
                director_prompt: recorded_template,
                ..config
            },
            AgentKind::ClaudeCode,
            BrigadeRole::Director,
            7,
            "director",
            &["worker-1".to_string(), "worker-3".to_string()],
            None,
        )
        .unwrap();
        (pattern, recorded)
    }

    fn transcript_with_snapshot(dir: &std::path::Path, last_section: &str) -> std::path::PathBuf {
        let line = serde_json::json!({
            "type": "attachment",
            "attachment": {"type": "prompt_snapshot", "systemPrompt": ["x", last_section]},
        });
        let path = dir.join("sess.jsonl");
        std::fs::write(&path, format!("{line}\n")).unwrap();
        path
    }

    #[test]
    fn the_shipped_director_briefing_recorded_for_another_roster_is_not_a_difference() {
        let dir = tempfile::tempdir().unwrap();
        let (pattern, recorded) = shipped_director_pattern_and_briefing("");
        assert!(recorded.ends_with(consult_paragraph(GoinkyoConsult::Design)));
        let path = transcript_with_snapshot(dir.path(), &recorded);
        assert_eq!(
            banto_io::claude_briefing::decide(Some(&path), &pattern),
            banto_io::claude_briefing::Outcome::Matches
        );
    }

    #[test]
    fn a_sentence_the_operator_deleted_after_the_roster_is_a_difference() {
        let dir = tempfile::tempdir().unwrap();
        let (pattern, recorded) =
            shipped_director_pattern_and_briefing(". Never merge without asking me");
        assert!(recorded.contains("Never merge without asking me"));
        let path = transcript_with_snapshot(dir.path(), &recorded);
        assert_eq!(
            banto_io::claude_briefing::decide(Some(&path), &pattern),
            banto_io::claude_briefing::Outcome::Differs
        );
    }

    #[test]
    fn a_sentence_the_operator_added_after_the_roster_is_a_difference() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = BrigadeConfig::default();
        let (_, recorded) = shipped_director_pattern_and_briefing("");
        config.director_prompt = config
            .director_prompt
            .replace("{peers}.", "{peers}. Never merge without asking me.");
        let pattern = compose_pattern(&config, BrigadeRole::Director, 7, "director", None).unwrap();
        let path = transcript_with_snapshot(dir.path(), &recorded);
        assert_eq!(
            banto_io::claude_briefing::decide(Some(&path), &pattern),
            banto_io::claude_briefing::Outcome::Differs
        );
    }

    fn compose_director_for(config: &BrigadeConfig, brigade: BrigadeId, token: &str) -> String {
        compose(
            config,
            AgentKind::ClaudeCode,
            BrigadeRole::Director,
            brigade,
            token,
            &["worker-1".to_string()],
            None,
        )
        .unwrap()
    }

    #[test]
    fn a_template_without_peers_gives_an_exact_pattern_and_the_request_is_substituted() {
        let config = BrigadeConfig {
            goinkyo_prompt: "Consultation at {request} in {brigade}.".to_string(),
            ..BrigadeConfig::default()
        };
        let pattern = compose_pattern(
            &config,
            BrigadeRole::Goinkyo,
            7,
            "goinkyo",
            Some("/tmp/7.txt"),
        )
        .unwrap();
        assert!(pattern.matches("Consultation at /tmp/7.txt in 7."));
        assert!(!pattern.matches("Consultation at /tmp/8.txt in 7."));
    }

    #[test]
    fn the_pattern_exists_exactly_when_a_briefing_does() {
        let empty = config_with("", "decisions");
        assert!(compose_pattern(&empty, BrigadeRole::Director, 7, "director", None).is_none());
        let config = BrigadeConfig::default();
        for role in [
            BrigadeRole::Director,
            BrigadeRole::Worker,
            BrigadeRole::Goinkyo,
        ] {
            assert_eq!(
                compose_pattern(&config, role, 7, "x", None).is_some(),
                compose(&config, AgentKind::ClaudeCode, role, 7, "x", &[], None).is_some(),
                "{role:?}"
            );
        }
    }

    #[test]
    fn the_shipped_director_prompt_is_followed_by_the_level_paragraph_too() {
        let config = BrigadeConfig::default();
        let out = compose_director(&config, AgentKind::ClaudeCode).unwrap();
        assert!(out.ends_with(consult_paragraph(GoinkyoConsult::Design)));
        assert!(out.starts_with("You are the Director of banto brigade 7."));
    }

    #[test]
    fn an_empty_director_prompt_gets_no_paragraph_on_either_product() {
        let config = config_with("", "decisions");
        assert_eq!(compose_director(&config, AgentKind::ClaudeCode), None);
        // Codex: the addendum alone, as before the setting existed.
        assert_eq!(
            compose_director(&config, AgentKind::Codex).unwrap(),
            with_codex_addendum("", BrigadeRole::Director)
        );
    }

    #[test]
    fn an_empty_level_adds_no_paragraph_and_an_invalid_one_reads_as_design() {
        let empty = config_with("You lead.", "");
        assert_eq!(
            compose_director(&empty, AgentKind::ClaudeCode).as_deref(),
            Some("You lead.")
        );
        let invalid = config_with("You lead.", "sometimes");
        assert_eq!(
            compose_director(&invalid, AgentKind::ClaudeCode).unwrap(),
            format!("You lead.\n\n{}", consult_paragraph(GoinkyoConsult::Design))
        );
    }

    #[test]
    fn workers_and_the_goinkyo_get_no_paragraph() {
        let config = BrigadeConfig::default();
        for role in [BrigadeRole::Worker, BrigadeRole::Goinkyo] {
            for agent in [AgentKind::ClaudeCode, AgentKind::Codex] {
                let out = compose(&config, agent, role, 7, "x", &[], Some("/tmp/r.txt")).unwrap();
                assert!(
                    !out.contains("When to consult the Goinkyo"),
                    "{role:?} {agent:?}"
                );
            }
        }
        // The Goinkyo's `{request}` still renders on the Claude path.
        let config = BrigadeConfig {
            goinkyo_prompt: "read {request}".to_string(),
            ..BrigadeConfig::default()
        };
        assert_eq!(
            compose(
                &config,
                AgentKind::ClaudeCode,
                BrigadeRole::Goinkyo,
                7,
                "goinkyo",
                &[],
                Some("/tmp/r.txt")
            )
            .as_deref(),
            Some("read /tmp/r.txt")
        );
    }

    #[test]
    fn on_codex_the_paragraph_comes_before_the_addendum_and_the_sentence_stays_last() {
        let config = config_with("You lead.", "decisions");
        let out = compose_director(&config, AgentKind::Codex).unwrap();
        let paragraph_at = out
            .find(consult_paragraph(GoinkyoConsult::Decisions))
            .unwrap();
        let addendum_at = out.find("banto's tools are not listed").unwrap();
        assert!(paragraph_at < addendum_at);
        assert!(out.starts_with("You lead.\n\n"));
        assert!(out.ends_with(BRIEFING_LAST_SENTENCE));
    }

    #[test]
    fn the_codex_director_briefing_stays_under_the_spill_budget() {
        // Codex stores a hook's context over about 10,000 bytes as a truncated
        // preview, which can never be matched again (`banto_io::codex_briefing`).
        // The budget keeps a margin under it for the longest level, on the shipped
        // template, with a full roster (eight Workers and a Goinkyo) and the addendum.
        let config = config_with(&BrigadeConfig::default().director_prompt, "decisions");
        let mut peers = vec!["worker-1".to_string(); 8];
        peers.push("goinkyo".to_string());
        let out = compose(
            &config,
            AgentKind::Codex,
            BrigadeRole::Director,
            123_456_789,
            "director",
            &peers,
            None,
        )
        .unwrap();
        for (level, name) in [
            (GoinkyoConsult::Off, "off"),
            (GoinkyoConsult::Disputes, "disputes"),
            (GoinkyoConsult::Design, "design"),
            (GoinkyoConsult::Decisions, "decisions"),
        ] {
            eprintln!(
                "goinkyo_consult {name}: {} bytes",
                consult_paragraph(level).len()
            );
        }
        eprintln!(
            "codex director briefing, decisions level, nine peers: {} bytes",
            out.len()
        );
        assert!(out.len() < 9_900, "{} bytes", out.len());
    }

    #[test]
    fn the_last_sentence_is_the_one_the_dedupe_recognises() {
        for role in [BrigadeRole::Director, BrigadeRole::Worker] {
            assert!(with_codex_addendum("x", role).ends_with(BRIEFING_LAST_SENTENCE));
            assert!(with_codex_addendum("", role).ends_with(BRIEFING_LAST_SENTENCE));
        }
        assert_eq!(
            BRIEFING_LAST_SENTENCE,
            "If this conversation holds more than one banto briefing, the most recent one is current and replaces the earlier ones."
        );
    }
}
