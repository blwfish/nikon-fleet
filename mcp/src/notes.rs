//! A Rust port of `mcp-agent-notes` (github.com/blwfish/mcp-agent-notes, 0.2.0): structured,
//! prioritized onboarding knowledge for an MCP server, delivered two ways.
//!
//! - [`render_instructions`]: a small, bounded blob for `InitializeResult.instructions`, the one
//!   MCP channel that reaches every connecting client. Only one-line summaries, never detail.
//! - [`strategy`], [`tactics`], [`find`]: the operations of a query tool, for full detail on demand.
//!
//! The behaviour is the Python reference's, not a reinterpretation: `tests/fixtures/agent_notes_golden.json`
//! holds vectors generated from the Python implementation, and the tests here check this port
//! against them. Regenerate the file from the reference if the reference changes.
//!
//! Deliberately not ported: the pluggable `scorer` seam of `find_notes` (nothing here needs a
//! semantic matcher yet). Matching is the reference's token overlap: `addresses` first, then `summary`.

use std::collections::BTreeSet;

use time::Date;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Critical,
    High,
    Medium,
    Low,
}

impl Priority {
    fn rank(self) -> u8 {
        match self {
            Priority::Critical => 0,
            Priority::High => 1,
            Priority::Medium => 2,
            Priority::Low => 3,
        }
    }
}

/// What question a note answers; orthogonal to [`Priority`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    /// What am I trying to do; what are my options.
    Strategy,
    /// Given a plan, how do I execute this one step.
    Tactic,
}

impl NoteKind {
    fn label(self) -> &'static str {
        match self {
            NoteKind::Strategy => "STRATEGY",
            NoteKind::Tactic => "TACTIC",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Stable slug; only a sort tiebreak, never rendered.
    pub id: String,
    pub added: Date,
    pub priority: Priority,
    pub kind: NoteKind,
    /// One line that stands alone.
    pub summary: String,
    /// Full body (markdown); the summary stands in when empty.
    pub detail: String,
    /// Topics this note is about; empty means universal.
    pub tags: Vec<String>,
    /// Problem or symptom phrasings that [`find`] matches against; not displayed.
    pub addresses: Vec<String>,
    /// Pointer to material too large to inline; shown, never fetched.
    pub location: Option<String>,
}

impl Note {
    pub fn new(id: &str, added: Date, priority: Priority, kind: NoteKind, summary: &str) -> Self {
        Note {
            id: id.into(),
            added,
            priority,
            kind,
            summary: summary.into(),
            detail: String::new(),
            tags: Vec::new(),
            addresses: Vec::new(),
            location: None,
        }
    }
    pub fn detail(mut self, d: &str) -> Self {
        self.detail = d.into();
        self
    }
    pub fn tags(mut self, t: &[&str]) -> Self {
        self.tags = t.iter().map(|s| s.to_string()).collect();
        self
    }
    pub fn addresses(mut self, a: &[&str]) -> Self {
        self.addresses = a.iter().map(|s| s.to_string()).collect();
        self
    }
    pub fn location(mut self, l: &str) -> Self {
        self.location = Some(l.into());
        self
    }
}

/// Critical first, then most recent first, then id.
fn sort_key(n: &Note) -> (u8, i32, &str) {
    (n.priority.rank(), -n.added.to_julian_day(), n.id.as_str())
}

fn sorted(notes: &[Note]) -> Vec<&Note> {
    let mut v: Vec<&Note> = notes.iter().collect();
    v.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
    v
}

fn render_note_full(n: &Note) -> String {
    let body = if n.detail.is_empty() { &n.summary } else { &n.detail };
    let mut lines = vec![format!("## {}", n.summary), body.clone()];
    if let Some(l) = &n.location {
        lines.push(format!("See: {l}"));
    }
    lines.join("\n")
}

pub struct RenderOptions<'a> {
    /// A hand-written blurb, shown first and verbatim; omitted when empty.
    pub capability_statement: &'a str,
    pub query_tool_name: &'a str,
    pub now: Date,
    pub recent_days: i64,
    pub max_notes: usize,
}

/// The compact string for `InitializeResult.instructions`. Topic-blind by design: the handshake
/// happens once per connection and cannot be repeated, so gating it on a snapshot of what is
/// active could hide a note for the rest of the session.
///
/// Every CRITICAL note, never truncated; HIGH notes fill what `max_notes` leaves; then anything
/// added within `recent_days` that is not already shown; then a fixed calling convention that
/// names the query tool (so the blob's size does not grow with the corpus).
pub fn render_instructions(notes: &[Note], o: &RenderOptions<'_>) -> String {
    let ordered = sorted(notes);
    let criticals: Vec<&Note> = ordered.iter().copied().filter(|n| n.priority == Priority::Critical).collect();
    let highs: Vec<&Note> = ordered.iter().copied().filter(|n| n.priority == Priority::High).collect();
    let budget = o.max_notes.saturating_sub(criticals.len());
    let mut highlighted = criticals;
    highlighted.extend(highs.into_iter().take(budget));
    let shown: BTreeSet<&str> = highlighted.iter().map(|n| n.id.as_str()).collect();

    let cutoff = i64::from(o.now.to_julian_day()) - o.recent_days;
    let recent: Vec<&Note> = ordered
        .iter()
        .copied()
        .filter(|n| !shown.contains(n.id.as_str()) && i64::from(n.added.to_julian_day()) >= cutoff)
        .collect();

    let mut sections: Vec<String> = Vec::new();
    if !o.capability_statement.is_empty() {
        sections.push(o.capability_statement.to_string());
    }
    if !highlighted.is_empty() {
        let mut block = vec!["Key things to know:".to_string()];
        block.extend(highlighted.iter().map(|n| format!("- {}", n.summary)));
        sections.push(block.join("\n"));
    }
    if !recent.is_empty() {
        let mut block = vec!["Recently added:".to_string()];
        block.extend(recent.iter().map(|n| format!("- {}", n.summary)));
        sections.push(block.join("\n"));
    }
    let q = o.query_tool_name;
    sections.push(format!(
        "For more, call {q}(operation=\"strategy\") for a tiered overview, {q}(operation=\"strategy\"|\"tactics\", \
         topic=\"...\") for one topic in full, or {q}(operation=\"find\", problem=\"...\") to search by symptom."
    ));
    sections.join("\n\n")
}

/// Query operation `strategy`. Without `topic`: every STRATEGY note, in full when all its tags are
/// active (an untagged note is always in full), as a one-liner otherwise. With `topic`: that
/// topic's STRATEGY notes in full, whatever is active (naming a topic is the fresher signal).
pub fn strategy(notes: &[Note], topic: Option<&str>, active: &BTreeSet<String>) -> String {
    let candidates: Vec<&Note> = sorted(notes).into_iter().filter(|n| n.kind == NoteKind::Strategy).collect();
    if let Some(t) = topic {
        let matched: Vec<&Note> = candidates.into_iter().filter(|n| n.tags.iter().any(|x| x == t)).collect();
        if matched.is_empty() {
            return format!("No strategy notes found for topic \"{t}\".");
        }
        return matched.iter().map(|n| render_note_full(n)).collect::<Vec<_>>().join("\n\n");
    }
    if candidates.is_empty() {
        return "No strategy notes available.".into();
    }
    let (full, brief): (Vec<&Note>, Vec<&Note>) =
        candidates.into_iter().partition(|n| n.tags.iter().all(|t| active.contains(t)));
    let mut sections = Vec::new();
    if !full.is_empty() {
        sections.push(full.iter().map(|n| render_note_full(n)).collect::<Vec<_>>().join("\n\n"));
    }
    if !brief.is_empty() {
        let mut block = vec!["Also available:".to_string()];
        block.extend(brief.iter().map(|n| format!("- {}", n.summary)));
        sections.push(block.join("\n"));
    }
    sections.join("\n\n")
}

/// Query operation `tactics`. Without `topic`: the topics that have TACTIC notes, generated from
/// the notes' tags. With `topic`: that topic's TACTIC notes in full.
pub fn tactics(notes: &[Note], topic: Option<&str>) -> String {
    let candidates: Vec<&Note> = notes.iter().filter(|n| n.kind == NoteKind::Tactic).collect();
    if let Some(t) = topic {
        let mut matched: Vec<&Note> = candidates.into_iter().filter(|n| n.tags.iter().any(|x| x == t)).collect();
        matched.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        if matched.is_empty() {
            return format!("No tactic notes found for topic \"{t}\".");
        }
        return matched.iter().map(|n| render_note_full(n)).collect::<Vec<_>>().join("\n\n");
    }
    let topics: BTreeSet<&str> = candidates.iter().flat_map(|n| n.tags.iter().map(String::as_str)).collect();
    if topics.is_empty() {
        return "No tactical topics available.".into();
    }
    format!("Available tactical topics: {}", topics.into_iter().collect::<Vec<_>>().join(", "))
}

/// Lowercase runs of ASCII letters and digits.
fn tokens(text: &str) -> BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Any `addresses` overlap outranks any summary-only match, whatever the counts.
fn score(query: &BTreeSet<String>, n: &Note) -> f64 {
    if query.is_empty() {
        return 0.0;
    }
    let addr = tokens(&n.addresses.join(" "));
    let overlap = query.intersection(&addr).count();
    if overlap > 0 {
        return 1000.0 + overlap as f64;
    }
    query.intersection(&tokens(&n.summary)).count() as f64
}

/// The notes that best match a free-text problem description, best first. A non-positive
/// `top_n`, no notes or a blank query give nothing (a negative `top_n` is not "all but the last").
pub fn find_notes<'a>(notes: &'a [Note], query: &str, top_n: i64) -> Vec<&'a Note> {
    if top_n <= 0 || notes.is_empty() || query.trim().is_empty() {
        return Vec::new();
    }
    let q = tokens(query);
    let mut scored: Vec<(f64, &Note)> = notes.iter().map(|n| (score(&q, n), n)).filter(|(s, _)| *s > 0.0).collect();
    scored.sort_by(|(sa, a), (sb, b)| sb.partial_cmp(sa).unwrap_or(std::cmp::Ordering::Equal).then_with(|| sort_key(a).cmp(&sort_key(b))));
    scored.into_iter().take(top_n as usize).map(|(_, n)| n).collect()
}

/// Query operation `find`: the best matches of both kinds, each labelled and in full.
pub fn find(notes: &[Note], problem: &str, top_n: i64) -> String {
    let results = find_notes(notes, problem, top_n);
    if results.is_empty() {
        return format!("No notes found matching \"{problem}\".");
    }
    results
        .iter()
        .map(|n| {
            let mut block = vec![format!("[{}] {}", n.kind.label(), n.summary)];
            if !n.detail.is_empty() {
                block.push(n.detail.clone());
            }
            if let Some(l) = &n.location {
                block.push(format!("See: {l}"));
            }
            block.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use time::format_description::well_known::Iso8601;

    const GOLDEN: &str = include_str!("../tests/fixtures/agent_notes_golden.json");

    fn date(s: &str) -> Date {
        Date::parse(s, &Iso8601::DATE).unwrap()
    }

    fn load() -> (Vec<Note>, Date, Vec<Value>) {
        let v: Value = serde_json::from_str(GOLDEN).unwrap();
        let strs = |x: &Value| x.as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect::<Vec<_>>();
        let notes = v["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| Note {
                id: n["id"].as_str().unwrap().into(),
                added: date(n["added"].as_str().unwrap()),
                priority: match n["priority"].as_str().unwrap() {
                    "critical" => Priority::Critical,
                    "high" => Priority::High,
                    "medium" => Priority::Medium,
                    "low" => Priority::Low,
                    other => panic!("{other}"),
                },
                kind: match n["kind"].as_str().unwrap() {
                    "strategy" => NoteKind::Strategy,
                    "tactic" => NoteKind::Tactic,
                    other => panic!("{other}"),
                },
                summary: n["summary"].as_str().unwrap().into(),
                detail: n["detail"].as_str().unwrap().into(),
                tags: strs(&n["tags"]),
                addresses: strs(&n["addresses"]),
                location: n["location"].as_str().map(String::from),
            })
            .collect();
        (notes, date(v["now"].as_str().unwrap()), v["cases"].as_array().unwrap().clone())
    }

    #[test]
    fn every_golden_vector_from_the_python_reference_matches() {
        let (notes, now, cases) = load();
        assert!(cases.len() >= 28);
        for c in &cases {
            let a = &c["args"];
            let got = match c["fn"].as_str().unwrap() {
                "render" => render_instructions(
                    &notes,
                    &RenderOptions {
                        capability_statement: a["capability_statement"].as_str().unwrap(),
                        query_tool_name: a["query_tool_name"].as_str().unwrap(),
                        now,
                        recent_days: a["recent_days"].as_i64().unwrap(),
                        max_notes: a["max_notes"].as_u64().unwrap() as usize,
                    },
                ),
                "strategy" => {
                    let active: BTreeSet<String> =
                        a["active_topics"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect();
                    strategy(&notes, a["topic"].as_str(), &active)
                }
                "tactics" => tactics(&notes, a["topic"].as_str()),
                "find" => find(&notes, a["problem"].as_str().unwrap(), a["top_n"].as_i64().unwrap()),
                other => panic!("unknown fn {other}"),
            };
            assert_eq!(got, c["expected"].as_str().unwrap(), "case {:?}", c["name"]);
        }
    }

    #[test]
    fn the_fixture_exercises_the_boundaries_it_claims_to() {
        // Guard against a regenerated fixture that no longer covers them.
        let (_, _, cases) = load();
        let names: Vec<&str> = cases.iter().map(|c| c["name"].as_str().unwrap()).collect();
        for want in [
            "render recent 7 days exactly on the edge",
            "render recent 6 days: edge note out",
            "render max_notes 2 keeps every critical",
            "find negative top_n",
            "strategy topic is exact not prefix",
        ] {
            assert!(names.contains(&want), "{want}");
        }
        let edge = cases.iter().find(|c| c["name"] == "render recent 7 days exactly on the edge").unwrap()["expected"].as_str().unwrap().to_string();
        let out = cases.iter().find(|c| c["name"] == "render recent 6 days: edge note out").unwrap()["expected"].as_str().unwrap().to_string();
        assert!(edge.contains("Low note on the recent edge.") && !out.contains("Low note on the recent edge."));
    }

    #[test]
    fn criticals_are_never_truncated_and_details_are_never_rendered_in_the_handshake() {
        let (notes, now, _) = load();
        let s = render_instructions(
            &notes,
            &RenderOptions { capability_statement: "", query_tool_name: "q", now, recent_days: 0, max_notes: 0 },
        );
        for n in notes.iter().filter(|n| n.priority == Priority::Critical) {
            assert!(s.contains(&n.summary), "{}", n.id);
        }
        for n in &notes {
            if !n.detail.is_empty() {
                assert!(!s.contains(&n.detail), "detail of {} leaked into the handshake", n.id);
            }
        }
    }

    #[test]
    fn find_degenerate_inputs_are_empty_not_everything() {
        let (notes, _, _) = load();
        assert!(find_notes(&notes, "capture", 0).is_empty());
        assert!(find_notes(&notes, "capture", -1).is_empty());
        assert!(find_notes(&[], "capture", 5).is_empty());
        assert!(find_notes(&notes, "", 5).is_empty());
        assert!(find_notes(&notes, "!!! ---", 5).is_empty(), "punctuation only has no tokens");
    }
}
