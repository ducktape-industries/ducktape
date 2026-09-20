//! `[[variants]]` — load-time expansion of one spec file into a family of
//! finer tags.
//!
//! a variant is pure sugar over the documented "finer tag with its own spec"
//! pattern: each entry registers an ADDITIONAL spec under the composed tag
//! `{parent_tag}_{suffix}`, inheriting `bin`/`env`/`prompt`/`output`/
//! `timeout_secs`/`hard_timeout_factor`/`isolation`/`context` (and
//! `description`) from the parent,
//! with the variant's own FULL argv. there is no
//! merging, no placeholder, no substitution — the
//! "argv is literal" invariant holds per tag, exactly as if the operator had
//! written one file per tag. this is deliberately NOT the removed
//! dispatch-time model routing (`[models]` / `{model}`): expansion happens
//! once at load, and dispatch still resolves one explicit tag to one fixed
//! argv.
//!
//! ## the tag grammar the app relies on
//!
//! a suffix is `<model>_<effort>`, each side `[a-z0-9.-]+` — so a suffix
//! carries EXACTLY one `_`, the parent tag must carry none, and every
//! composed tag splits into exactly three segments on `_`:
//! `provider_model_effort` (e.g. `codex_gpt-5.5_xhigh`, `claude_opus_max`).
//! the app's picker decomposes tags on that promise; the loader enforces it
//! fail-loud rather than letting a malformed family load as opaque tags.

use serde::Deserialize;

use crate::spec::{CapabilitySpec, validate_tag};

/// the on-disk shape of one `[[variants]]` entry — a dumb serde mirror,
/// validated by [`expand`]. unknown fields fail loud like everywhere else in
/// the spec format.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawVariant {
    /// composes the variant's tag as `{parent_tag}_{suffix}`; must be
    /// `<model>_<effort>` with both sides `[a-z0-9.-]+`.
    pub(crate) suffix: String,
    /// the variant's FULL argv — required and verbatim, never derived from
    /// the parent's args.
    pub(crate) args: Vec<String>,
}

/// expand a parsed base spec's variants into their own [`CapabilitySpec`]s
/// (the base itself is not included). every rejection names `origin` and the
/// offending suffix, matching the loader's fail-loud style.
pub(crate) fn expand(
    base: &CapabilitySpec,
    variants: &[RawVariant],
    origin: &str,
) -> Result<Vec<CapabilitySpec>, String> {
    if variants.is_empty() {
        return Ok(Vec::new());
    }
    // the exactly-three-segments grammar needs an underscore-free parent: a
    // parent like "my_llm" would compose four-segment tags the app cannot
    // decompose. a spec with such a tag can still ship finer tags the
    // documented way — one file per tag — just not via [[variants]].
    if base.tag.contains('_') {
        return Err(format!(
            "{origin}: [[variants]] requires an underscore-free parent tag \
             (composed tags are provider_model_effort), got {:?}",
            base.tag
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut specs = Vec::with_capacity(variants.len());
    for variant in variants {
        validate_suffix(&variant.suffix).map_err(|e| format!("{origin}: {e}"))?;
        let tag = format!("{}_{}", base.tag, variant.suffix);
        // the composed tag obeys THE consensus tag rule like any other —
        // charset holds by construction, so this is the ≤64-byte gate.
        validate_tag(&tag).map_err(|e| format!("{origin}: variant tag {tag:?}: {e}"))?;
        if !seen.insert(tag.clone()) {
            return Err(format!(
                "{origin}: duplicate variant suffix {:?} (tag {tag:?})",
                variant.suffix
            ));
        }
        specs.push(CapabilitySpec {
            tag,
            description: base.description.clone(),
            bin: base.bin.clone(),
            companions: base.companions.clone(),
            env: base.env.clone(),
            args: variant.args.clone(),
            timeout_secs: base.timeout_secs,
            hard_timeout_factor: base.hard_timeout_factor,
            output: base.output,
            // WHICH MCP syntax the CLI takes its tool plane in is a property of
            // the CLI, so every variant is wired the same way the base is.
            tools: base.tools,
            // HOW the executor authenticates — its broker and its config home —
            // is a property of the CLI, not of the model or the effort a variant
            // pins. so the auth section inherits whole.
            isolation: base.isolation.clone(),
            // WHERE the CLI auto-loads its ambient instructions is a property of
            // the CLI, like its auth — not of the model or effort a variant pins.
            context: base.context.clone(),
            // interactive TUI eligibility is a property of the CLI, not the
            // model pin, so variants inherit the parent's argv whole.
            interactive: base.interactive.clone(),
            // WHERE the CLI's build comes from is a property of the CLI too;
            // a variant runs the same executable.
            source: base.source.clone(),
        });
    }
    Ok(specs)
}

/// the suffix shape rule: `<model>_<effort>`, both sides non-empty
/// `[a-z0-9.-]` — no `_` inside either side, so the composed tag splits into
/// exactly three segments.
fn validate_suffix(suffix: &str) -> Result<(), String> {
    let mut parts = suffix.split('_');
    if let (Some(model), Some(effort), None) = (parts.next(), parts.next(), parts.next()) {
        let side_ok = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-".contains(&b))
        };
        if side_ok(model) && side_ok(effort) {
            return Ok(());
        }
    }
    Err(format!(
        "variant suffix {suffix:?} must be \"<model>_<effort>\" with model and \
         effort each matching [a-z0-9.-]+ (exactly one '_')"
    ))
}

#[cfg(test)]
mod tests {
    use crate::spec::{CapabilitySpec, OutputFormat, builtin_specs};

    fn family_toml(extra: &str) -> String {
        format!(
            r#"
spec = 1
[capability]
tag = "prov"
description = "a provider family"
[detect]
bin = "prov-cli"
companions = ["prov-helper"]
env = "MOCK_PROV_BIN"
[invoke]
args = ["run", "--default"]
prompt = "stdin"
timeout_secs = 120
hard_timeout_factor = 12
[output]
format = "text"
{extra}
"#
        )
    }

    #[test]
    fn variants_expand_into_composed_tags_with_inherited_fields_and_own_argv() {
        let toml = family_toml(
            r#"
[[variants]]
suffix = "m1_low"
args = ["run", "--model", "m1", "--effort", "low"]

[[variants]]
suffix = "m1_high"
args = ["run", "--model", "m1", "--effort", "high"]
"#,
        );
        let specs = CapabilitySpec::parse_all(&toml, "t").unwrap();
        assert_eq!(
            specs.iter().map(|s| s.tag.as_str()).collect::<Vec<_>>(),
            vec!["prov", "prov_m1_low", "prov_m1_high"],
            "base first, variants in declaration order"
        );
        assert_eq!(
            specs[0].args,
            vec!["run", "--default"],
            "base argv untouched"
        );
        let v = &specs[1];
        assert_eq!(v.args, vec!["run", "--model", "m1", "--effort", "low"]);
        assert_eq!(v.bin, "prov-cli", "bin inherited");
        assert_eq!(v.companions, ["prov-helper"], "companions inherited");
        assert_eq!(v.env.as_deref(), Some("MOCK_PROV_BIN"), "env inherited");
        assert_eq!(v.description, "a provider family", "description inherited");
        assert_eq!(v.timeout_secs, 120, "timeout inherited");
        assert_eq!(v.hard_timeout_factor, 12, "hard timeout factor inherited");
        assert_eq!(v.output, OutputFormat::Text, "output inherited");
    }

    #[test]
    fn malformed_suffixes_are_rejected_by_shape_and_charset() {
        for bad in [
            "noeffort",   // zero '_' — not <model>_<effort>
            "a_b_c",      // two '_' — composed tag would split into 4
            "_low",       // empty model side
            "model_",     // empty effort side
            "Model_low",  // uppercase
            "m odel_low", // space
            "model_löw",  // non-ascii
        ] {
            let toml = family_toml(&format!(
                "[[variants]]\nsuffix = \"{bad}\"\nargs = [\"run\"]\n"
            ));
            let err = CapabilitySpec::parse_all(&toml, "t").unwrap_err();
            assert!(
                err.contains("variant suffix") && err.contains("<model>_<effort>"),
                "suffix {bad:?} must fail by shape, got {err:?}"
            );
        }
    }

    #[test]
    fn duplicate_suffixes_and_oversized_composed_tags_are_rejected() {
        let dup = family_toml(
            "[[variants]]\nsuffix = \"m_low\"\nargs = [\"a\"]\n\n\
             [[variants]]\nsuffix = \"m_low\"\nargs = [\"b\"]\n",
        );
        let err = CapabilitySpec::parse_all(&dup, "t").unwrap_err();
        assert!(err.contains("duplicate variant suffix"), "got {err:?}");

        // "prov_" (5) + 58-byte model + "_low" (4) = 67 bytes > 64.
        let long = family_toml(&format!(
            "[[variants]]\nsuffix = \"{}_low\"\nargs = [\"a\"]\n",
            "m".repeat(58)
        ));
        let err = CapabilitySpec::parse_all(&long, "t").unwrap_err();
        assert!(err.contains("64 bytes"), "got {err:?}");
    }

    #[test]
    fn variants_under_an_underscored_parent_tag_are_rejected() {
        let toml = family_toml("[[variants]]\nsuffix = \"m_low\"\nargs = [\"a\"]\n")
            .replace(r#"tag = "prov""#, r#"tag = "pro_v""#);
        let err = CapabilitySpec::parse_all(&toml, "t").unwrap_err();
        assert!(err.contains("underscore-free parent tag"), "got {err:?}");
    }

    #[test]
    fn unknown_fields_inside_a_variant_fail_loud() {
        // same posture as the rest of the format: a typo'd or stale field in
        // a [[variants]] entry is a boot error, never silently ignored.
        let toml = family_toml("[[variants]]\nsuffix = \"m_low\"\nargs = [\"a\"]\nmodel = \"m\"\n");
        let err = CapabilitySpec::parse_all(&toml, "t").unwrap_err();
        assert!(err.contains("not a valid spec"), "got {err:?}");

        // args is required — a variant without its own argv is meaningless.
        let toml = family_toml("[[variants]]\nsuffix = \"m_low\"\n");
        let err = CapabilitySpec::parse_all(&toml, "t").unwrap_err();
        assert!(err.contains("not a valid spec"), "got {err:?}");
    }

    #[test]
    fn variants_inherit_auth_and_context_sections() {
        let toml = family_toml(
            r#"
[isolation]
config_home_env = "PROV_HOME"

[context]
path = "config-home:AGENTS.md"

[[variants]]
suffix = "m1_low"
args = ["run", "--model", "m1"]
"#,
        );
        let specs = CapabilitySpec::parse_all(&toml, "t").unwrap();
        let get = |tag: &str| specs.iter().find(|s| s.tag == tag).unwrap();

        // the auth section is inherited — how the CLI authenticates is a
        // property of the CLI, not of the model/effort the variant pins.
        let v = get("prov_m1_low");
        assert_eq!(
            v.isolation,
            get("prov").isolation,
            "the [isolation] block is inherited whole"
        );
        assert_eq!(v.isolation.config_home_env.as_deref(), Some("PROV_HOME"));
        // WHERE the soul is delivered is a property of the CLI, not of the model
        // or effort a variant pins — so it inherits like auth does.
        assert_eq!(
            v.context,
            get("prov").context,
            "the [context] location is inherited whole"
        );
        assert_eq!(
            v.context,
            Some(crate::ContextLocation::ConfigHome("AGENTS.md".into()))
        );
    }

    #[test]
    fn embedded_tags_obey_the_picker_grammar() {
        // tag-agnostic data invariant: every embedded tag is either a base
        // (no '_') or a well-formed provider_model_effort composition.
        for spec in builtin_specs() {
            let segments: Vec<&str> = spec.tag.split('_').collect();
            assert!(
                segments.len() == 1 || segments.len() == 3,
                "embedded tag {:?} is neither base nor provider_model_effort",
                spec.tag
            );
            assert!(
                segments.iter().all(|s| !s.is_empty()),
                "embedded tag {:?} has an empty segment",
                spec.tag
            );
        }
    }

    #[test]
    fn embedded_curated_matrix_pins_the_shipped_tags_and_argv() {
        // a DATA regression test, deliberately unlike the executor-agnostic
        // engine tests: the curated model/effort matrix is a shipped contract
        // the app's picker (and operators' agent records) depend on, so the
        // exact tags and the base argvs are pinned here.
        let specs = builtin_specs();
        let get = |tag: &str| {
            specs
                .iter()
                .find(|s| s.tag == tag)
                .unwrap_or_else(|| panic!("embedded specs must include {tag:?}"))
        };

        // base tags keep their no-model argv — tools-enabled sandbox posture
        // (workspace-write / acceptEdits), no model, no effort flag. The MCP
        // server is NOT here: its address is the run's own, so `crate::mcp_argv`
        // composes it per run.
        assert_eq!(
            get("codex").args,
            vec!["app-server", "-c", "sandbox_mode=\"workspace-write\""],
        );
        assert_eq!(
            get("claude").args,
            vec![
                "-p",
                "--input-format",
                "stream-json",
                "--replay-user-messages",
                "--permission-prompts",
                "host",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "acceptEdits",
                "--allowedTools",
                "Bash",
                "Read",
                "Edit",
                "Write",
                "Glob",
                "Grep",
                "mcp__ducktape"
            ],
        );

        // spot-pin one corner of each matrix, incl. the embedded-quotes arg
        // codex parses as TOML (there is no shell; argv goes straight to exec).
        assert_eq!(
            get("codex_gpt-5.5_xhigh").args,
            vec![
                "app-server",
                "-c",
                "sandbox_mode=\"workspace-write\"",
                "-c",
                "model=\"gpt-5.5\"",
                "-c",
                "model_reasoning_effort=\"xhigh\"",
            ],
        );
        assert_eq!(
            get("claude_opus_max").args,
            vec![
                "-p",
                "--input-format",
                "stream-json",
                "--replay-user-messages",
                "--permission-prompts",
                "host",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "acceptEdits",
                "--allowedTools",
                "Bash",
                "Read",
                "Edit",
                "Write",
                "Glob",
                "Grep",
                "mcp__ducktape",
                "--model",
                "opus",
                "--effort",
                "max",
            ],
        );

        // the agentic posture rides every embedded spec: the slower agentic
        // timeout budget.
        for spec in &specs {
            assert_eq!(spec.timeout_secs, 600, "{}: agentic timeout", spec.tag);
            // every shipped executor delivers the assembled soul NATIVELY (a file
            // its CLI auto-loads), never by inflating the stdin prompt. WHICH file
            // is spec data — the assertion is that the door exists.
            assert!(
                spec.context.is_some(),
                "{}: an embedded executor names its own context file",
                spec.tag
            );
        }

        // the shipped AUTH posture, per family, inherited by every variant: a
        // host broker + a fresh config home, so the credential never enters the
        // child. They differ only in wire shape — codex speaks the OpenAI
        // Responses API, claude the Anthropic Messages API. Mounting a
        // credential dir instead is not representable at all (the spec format
        // has no `[sandbox]` table); this pins which broker each family is on.
        for spec in specs.iter().filter(|s| s.tag.starts_with("codex")) {
            assert_eq!(
                spec.isolation.broker,
                Some(crate::spec::BrokerKind::CodexResponses),
                "{}: codex authenticates through the host broker",
                spec.tag
            );
            assert_eq!(
                spec.isolation.config_home_env.as_deref(),
                Some("CODEX_HOME")
            );
        }
        for spec in specs.iter().filter(|s| s.tag.starts_with("claude")) {
            assert_eq!(
                spec.isolation.broker,
                Some(crate::spec::BrokerKind::AnthropicMessages),
                "{}: claude authenticates through the host Anthropic broker",
                spec.tag
            );
            assert_eq!(
                spec.isolation.config_home_env.as_deref(),
                Some("CLAUDE_CONFIG_DIR")
            );
        }

        // the full matrix is present: 19 codex + 16 claude variants + 3 bases.
        // codex efforts are per-model — the 5.6 family reaches `max`, 5.5 caps
        // at `xhigh` — so the codex side is not a rectangle.
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            for effort in ["low", "medium", "high", "xhigh", "max"] {
                get(&format!("codex_{model}_{effort}"));
            }
        }
        for effort in ["low", "medium", "high", "xhigh"] {
            get(&format!("codex_gpt-5.5_{effort}"));
        }
        for model in ["fable", "opus", "sonnet", "haiku"] {
            for effort in ["low", "medium", "high", "max"] {
                get(&format!("claude_{model}_{effort}"));
            }
        }
        assert_eq!(
            get("pi").isolation.broker,
            Some(crate::spec::BrokerKind::Pi)
        );
        assert_eq!(specs.len(), 38, "3 bases + 19 codex + 16 claude variants");
    }
}
