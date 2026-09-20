//! the agent's SOUL: its curated skills, assembled into ONE markdown document
//! the executor's own CLI auto-loads.
//!
//! a soul is not a stored object — it is a BUILD PRODUCT. consensus commits
//! WHICH skills an agent carries (name + duckfs pin + load mode); the host
//! materializes them as ro mounts and assembles them here, in curation order:
//! every `always` skill inlined in full (this is where the retired
//! `prompt_hash` blob's text now lives), then an INDEX of the on-demand ones
//! (name, description, where to read the body), then the ambient tool-plane
//! instruction. one document, one delivery — the provider decides the door
//! (the CLI's auto-load path, or the stdin prompt).
//!
//! PURE on purpose: no fs, no duckfs, no `SkillRef`. the provisioner (the only
//! layer allowed to touch the OS-side checkout engine) reads the materialized
//! `SKILL.md` files and calls [`assemble_context_doc`]; everything here is a
//! function of its argument, so the whole shape is unit-testable with no node.
//!
//! degrade rules, deliberately asymmetric: a skill with no/malformed
//! frontmatter still gets a name-only index entry (a cosmetic parse must never
//! fail a run), while a missing body for an `always` skill fails the run
//! loudly upstream — that one IS the persona, and running without it silently
//! produces a different agent.
//!
//! THREE TIERS of progressive disclosure, so run context never scales with the
//! size of the skill collection:
//! - tier 0, `always`: body inlined. costs O(persona).
//! - tier 1, curated `on_demand`: one index line + its mount path. costs
//!   O(curated count).
//! - tier 2, the GLOBAL LIBRARY under [`SKILL_LIBRARY_PREFIX`] in duckfs: NOT in
//!   context at all. the run is told, in one paragraph, that it exists and which
//!   MCP tools search and read it. costs O(1) — a thousand library skills cost
//!   the same as none.
//!
//! the bounds below are what keep tiers 0 and 1 finite. they fail the run rather
//! than truncate it: a silently trimmed persona is a DIFFERENT AGENT with no
//! signal, which is strictly worse than a run that stops and says why.

/// the ambient MCP instruction every run carries — MOVED verbatim from the run
/// envelope's runtime section, which this document replaces.
/// it deliberately does not enumerate the tools: the MCP server ships its own
/// instructions with the binary, so restating the surface here would only give
/// the two something to drift apart about.
const TOOL_PLANE_INSTRUCTION: &str = "A Ducktape MCP tool server named \"ducktape\" is available in this session. It is how you read and write Ducktape state — chat, tasks, pages, forge items, and duckfs files. Call its tools instead of guessing; its own instructions describe every tool it offers.";

/// the duckfs prefix the global skill library lives under — re-exported from
/// the module that owns it, never restated here: the string the assembler
/// advertises and the string the composer pins skills under must be the same
/// one, or the document points an agent at a prefix nothing lives in.
pub use crate::module_contracts::SKILL_LIBRARY_PREFIX;

/// the tier-2 pointer. named read operations with their real envelope shapes,
/// because a model that has to guess the call will guess wrong: `files.ls`
/// targets a `path`, `files.grep` targets a `prefix` and takes a `pattern`, and
/// `files.read` targets a `path` (`bin/node/src/mcp/tools/read.rs`).
///
/// `files.ls` is named FIRST, and it was the missing one: the library has no
/// index file, so listing the directory is the only way to see every skill it
/// holds — an agent told only to grep can find a skill it can already describe
/// and nothing else.
const SKILL_LIBRARY_SECTION: &str = "## The shared skill library\nBeyond the skills above, Ducktape carries a shared library of skills in duckfs under `/shared/skills/`, one directory per skill: `/shared/skills/<name>/SKILL.md`, whose YAML frontmatter carries a one-line `description`. It is NOT loaded into this context and costs you nothing until you read it. When your own skills do not cover the task in front of you, list the library with `ducktape_query` (`operation`: `files.ls`, `target`: `{\"path\": \"/shared/skills/\"}`) to see every skill it holds, or search it (`operation`: `files.grep`, `target`: `{\"prefix\": \"/shared/skills/\"}`, `input`: `{\"pattern\": what you are looking for}`) — then read the skill you want in full (`operation`: `files.read`, `target`: `{\"path\": \"/shared/skills/<name>/SKILL.md\"}`). Reading one is cheap; guessing at a task the library already answers is not.";

/// hard cap on the curated on-demand skills the index lists — the SAME number
/// consensus enforces on an agent's curated list (`MAX_SKILLS_PER_AGENT`),
/// deliberately re-exported rather than restated: two caps that could drift is
/// how you get a record consensus happily accepts and no run can load.
///
/// re-checked here anyway, because the consensus cap only binds at WRITE time —
/// a record registered before the cap existed still carries whatever it carries,
/// and the assembler is the last thing standing between it and a run.
pub use crate::module_contracts::MAX_SKILLS_PER_AGENT as MAX_INDEXED_SKILLS;

/// one curated skill, already materialized. `name` is the CURATED name
/// consensus committed (`SkillRef.name`), never a name read out of the document
/// — a doc must not be able to rename itself into another agent's heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDoc {
    pub name: String,
    /// the `description` from the doc's YAML frontmatter; `None` when it has
    /// none (or it is malformed) — a name-only index entry, never an error.
    pub description: Option<String>,
    /// the SKILL.md text with its frontmatter stripped ([`parse_skill_md`]).
    pub body: String,
    pub always: bool,
}

/// assemble the run's context document. CURATION ORDER is the agent's skills
/// order — never sorted: an agent's persona is a composed thing, and reordering
/// it is editing it.
///
/// NEVER EMPTY. an agent that curated no skills still gets the ambient tool-plane
/// paragraph — a fact about the world it woke up in, not about its curation. (the
/// empty-doc case is how the retired envelope runtime-section's tool-plane
/// instruction went missing for skill-less agents in the first place.)
///
/// `Err` = a bound was blown: the caller fails the run. checked HERE, in the
/// pure layer, so both node binaries reach the same verdict from the same
/// committed record.
pub fn assemble_context_doc(skills: &[SkillDoc]) -> Result<String, String> {
    // the curator must curate: an index of hundreds is a library, and a
    // library that ships in every prompt is exactly the cost this tiering exists
    // to avoid — the shared one costs nothing.
    let on_demand: Vec<&SkillDoc> = skills.iter().filter(|s| !s.always).collect();
    if on_demand.len() > MAX_INDEXED_SKILLS {
        return Err(format!(
            "{} on-demand skills exceed the index cap of {MAX_INDEXED_SKILLS}. curate fewer, or \
             leave them in the shared skill library at {SKILL_LIBRARY_PREFIX} — an agent pays \
             nothing for a skill it has not read.",
            on_demand.len()
        ));
    }

    let mut sections: Vec<String> = skills
        .iter()
        .filter(|s| s.always)
        .map(|s| format!("# {}\n{}", s.name, s.body.trim_end()))
        .collect();

    if !on_demand.is_empty() {
        let index: Vec<String> = on_demand
            .iter()
            .map(|s| {
                let where_ = format!("(`$DUCKTAPE_RUN_SKILLS/{}/SKILL.md`)", s.name);
                match &s.description {
                    Some(d) => format!("- **{}** — {d} {where_}", s.name),
                    None => format!("- **{}** {where_}", s.name),
                }
            })
            .collect();
        sections.push(format!(
            "## Skills available on demand\nRead the full text when the task \
             calls for it; each lives under the directory named by \
             $DUCKTAPE_RUN_SKILLS.\n{}",
            index.join("\n")
        ));
    }
    // the tool plane first, then the library that is READ through it: the
    // paragraph tells the agent to call tools, so it must already know it has
    // them. both are ambient — every run has them.
    sections.push(TOOL_PLANE_INSTRUCTION.to_string());
    sections.push(SKILL_LIBRARY_SECTION.to_string());
    Ok(sections.join("\n\n"))
}

/// split a `SKILL.md` into its frontmatter `description` and its body.
///
/// the frontmatter is the convention this repo's own `skills/` already follow:
/// a leading `---` line, `key: value` lines, a closing `---`. deliberately NOT
/// a YAML parser (no dep, no surface): anything that is not exactly that shape
/// is simply a document with no frontmatter — body verbatim, description
/// `None`. a cosmetic parse must never fail a run.
pub fn parse_skill_md(text: &str) -> (Option<String>, String) {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (None, text.to_string());
    };
    // the closing fence: the first line that is exactly `---`.
    let Some((front, body)) = rest.split_once("\n---\n").or_else(|| {
        rest.split_once("\r\n---\r\n")
            .or_else(|| rest.strip_suffix("\n---").map(|f| (f, "")))
    }) else {
        // an UNCLOSED fence is not frontmatter — the `---` was the document's.
        return (None, text.to_string());
    };
    let description = front.lines().find_map(|line| {
        let value = line.trim().strip_prefix("description:")?.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        (!value.is_empty()).then(|| value.to_string())
    });
    (description, body.trim_start_matches('\n').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, always: bool, description: Option<&str>, body: &str) -> SkillDoc {
        SkillDoc {
            name: name.into(),
            description: description.map(str::to_string),
            body: body.into(),
            always,
        }
    }

    /// the AMBIENT half of the document: a skill-less agent still learns that the
    /// tool plane and the library exist. it is not a soul, but it is a world.
    #[test]
    fn a_skill_less_agent_still_gets_the_tool_plane_and_the_library() {
        let doc = assemble_context_doc(&[]).unwrap();
        assert_eq!(doc.matches("A Ducktape MCP tool server").count(), 1);
        assert_eq!(doc.matches("## The shared skill library").count(), 1);
        // no curation => no headings, no index: exactly the two ambient paragraphs.
        assert!(!doc.contains("## Skills available on demand"), "got {doc}");
    }

    #[test]
    fn always_skills_inline_in_curation_order_and_on_demand_ones_only_index() {
        // curation order, NOT sorted: `zeta` was curated first, so it leads.
        let doc = assemble_context_doc(&[
            skill("zeta", true, Some("z desc"), "You are Zeta.\n"),
            skill(
                "release",
                false,
                Some("cut a release"),
                "the whole release body",
            ),
            skill("alpha", true, None, "Always quack twice."),
        ])
        .unwrap();
        assert!(
            doc.starts_with(
                "# zeta\nYou are Zeta.\n\n\
                 # alpha\nAlways quack twice.\n\n\
                 ## Skills available on demand\n\
                 Read the full text when the task calls for it; each lives under the \
                 directory named by $DUCKTAPE_RUN_SKILLS.\n\
                 - **release** — cut a release (`$DUCKTAPE_RUN_SKILLS/release/SKILL.md`)\n\n\
                 A Ducktape MCP tool server named \"ducktape\""
            ),
            "got {doc}"
        );
        // the on-demand skill is INDEXED, never inlined — the whole point of the
        // load mode is that its body does not inflate every prompt.
        assert!(
            !doc.contains("the whole release body"),
            "an on_demand body must not be inlined: {doc}"
        );
    }

    /// the tier-2 pointer, in the form an agent can actually act on: the prefix,
    /// the layout, and the two tools BY NAME (a model that has to guess the call
    /// guesses wrong).
    #[test]
    fn the_library_paragraph_names_the_prefix_the_layout_and_both_tools() {
        for doc in [
            assemble_context_doc(&[]).unwrap(),
            assemble_context_doc(&[skill("zeta", true, None, "You are Zeta.")]).unwrap(),
        ] {
            assert_eq!(doc.matches("## The shared skill library").count(), 1);
            assert!(doc.contains(SKILL_LIBRARY_PREFIX), "got {doc}");
            assert!(doc.contains("/shared/skills/<name>/SKILL.md"), "got {doc}");
            assert!(doc.contains("files.grep"), "got {doc}");
            assert!(doc.contains("files.read"), "got {doc}");
            // the whole point of the tier: it is a POINTER, not a payload.
            assert!(
                doc.contains("costs you nothing until you read it"),
                "got {doc}"
            );
        }
    }

    #[test]
    fn a_frontmatterless_skill_degrades_to_a_name_only_index_entry() {
        let doc = assemble_context_doc(&[skill("qa", false, None, "body")]).unwrap();
        assert!(
            doc.contains("- **qa** (`$DUCKTAPE_RUN_SKILLS/qa/SKILL.md`)"),
            "got {doc}"
        );
    }

    #[test]
    fn the_tool_plane_instruction_appears_exactly_once() {
        let doc = assemble_context_doc(&[
            skill("a", true, None, "A"),
            skill("b", true, None, "B"),
            skill("c", false, Some("c"), "C"),
        ])
        .unwrap();
        assert_eq!(
            doc.matches("A Ducktape MCP tool server").count(),
            1,
            "the ambient instruction is stated once: {doc}"
        );
    }

    #[test]
    fn an_over_cap_index_fails_loudly_and_points_at_the_library() {
        let many: Vec<SkillDoc> = (0..=MAX_INDEXED_SKILLS)
            .map(|i| skill(&format!("s{i}"), false, None, "b"))
            .collect();
        // the OWNER-facing error names the library: it is advice to whoever
        // curated 65 skills, not an instruction to a model.
        let err = assemble_context_doc(&many).unwrap_err();
        assert!(err.contains(&MAX_INDEXED_SKILLS.to_string()), "got {err}");
        assert!(err.contains(SKILL_LIBRARY_PREFIX), "got {err}");
        assert!(assemble_context_doc(&many[..MAX_INDEXED_SKILLS]).is_ok());
    }

    #[test]
    fn frontmatter_is_stripped_from_the_body_and_yields_the_description() {
        let (description, body) = parse_skill_md(
            "---\nname: qa\ndescription: Drive the QA fleet.\n---\n\n# QA\nthe body\n",
        );
        assert_eq!(description.as_deref(), Some("Drive the QA fleet."));
        assert_eq!(body, "# QA\nthe body\n");
    }

    #[test]
    fn a_malformed_or_absent_frontmatter_never_swallows_the_body() {
        // no frontmatter at all: body verbatim.
        assert_eq!(
            parse_skill_md("# QA\nthe body"),
            (None, "# QA\nthe body".to_string())
        );
        // an UNCLOSED fence is the document's own `---`, not frontmatter — the
        // body must survive intact (silently eating an always-skill's persona
        // is exactly the quiet corruption the loud degrade rules exist to kill).
        assert_eq!(
            parse_skill_md("---\nnot closed\nthe body"),
            (None, "---\nnot closed\nthe body".to_string())
        );
        // frontmatter with no description key: name-only, body still stripped.
        assert_eq!(
            parse_skill_md("---\nname: qa\n---\nbody"),
            (None, "body".to_string())
        );
        // quoted values unwrap.
        assert_eq!(
            parse_skill_md("---\ndescription: \"quoted\"\n---\nbody").0.as_deref(),
            Some("quoted")
        );
    }
}
