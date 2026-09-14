# Capability Spec Reference (v1)

A **capability spec** is a TOML file that teaches a Ducktape node how to run
one executor — an installed CLI that can turn a prompt into text. Everything
the node needs is in the file: how to detect the binary, where its Linux
build comes from, the exact argv to invoke it, and how to parse its output.
An executor using an implemented output parser and broker can be added by
configuration. New output or authentication contracts require host code and
tests. The embedded built-ins are spec files globbed out of
`crates/services/provider/specs/` at build time.

Specs are the data half of the capability system:

| Layer | Owns | Code |
|---|---|---|
| Consensus (`crates/modules/system/capability`) | *who provides what*, network-wide: node key → announced tag set | never reads specs |
| Host (`crates/services/provider`) | *actually running it*: spec loading, binary discovery, spawning, parsing | this document |

The consensus registry only ever sees **tags**. The spec behind a tag is
private to each host — two nodes can announce the same tag with differently
tuned specs and the network cannot tell, by design.

---

## Dispatch: explicit capability, nothing inferred

A job names the capability tag it needs; `ProviderSet::resolve(tag)` maps the
tag to the locally discovered provider. That is the whole dispatch model:

- **Consensus says WHAT** — an agent record carries a `capability` tag, the
  saga trigger carries the same tag, and work is leased over the tag's
  announced providers.
- **The spec says HOW** — binary, flags, sandbox posture, and (if the
  executor takes one) the model, written literally into `[invoke].args`.
  Model choice is host policy; no model name ever crosses consensus.
- **Nothing is inferred.** There is no routing table, no pattern matching,
  no default fallback. A tag either has a loaded spec or resolution fails
  loudly naming the loaded set.

A finer-grained need — "this executor, but pinned to a specific model, with
different flags" — is a **finer tag with its own spec file**, not a routing
rule. Define `tag = "myllm-large"` with the pinning flags in its args, have
agents name that tag, and only hosts carrying that spec (and binary) announce
it. [`[[variants]]`](#variants--one-file-a-family-of-finer-tags) is load-time
sugar for writing a whole family of such finer tags in one file.

---

## Trust model — read this first

A spec names an arbitrary local binary and the argv to run it with. **Loading
a spec is executing code by proxy.** Specs are operator-trusted configuration,
in the same trust class as a shell profile or a systemd unit:

- They load from exactly two places, both local and operator-controlled:
  1. the specs **embedded in the node binary** at compile time
     (`crates/services/provider/specs/*.toml`);
  2. the **operator spec directory** (see [Spec sources](#spec-sources)).
- They are **never fetched from the network**, and no consensus code path can
  read one (host-local files are non-deterministic input).
- Argv is **fully literal** and passed to exec **without shell
  interpretation** — no placeholders, no quoting, no expansion. The prompt
  reaches the child only via stdin, so job content cannot inject flags or
  commands.
- The child process runs **fenced**: its working directory is the workspace
  the run's provisioner materialized (an empty scratch dir when the embedder
  provisions none, never the node's data dir itself), non-interactive mode,
  and whatever sandbox flags the spec's argv encodes. Fence flags live in the
  spec — audit them when you audit the spec.

**Auth: the credential never enters the child.** There is exactly one way:

- **`[isolation]`.** The *host* holds the credential and keeps it in the node
  process. A run-scoped, loopback-only broker serves the model API, and the
  child gets only an unrelated opaque run bearer, a localhost base URL, and a
  **fresh, empty config home** (which is what stops the CLI reading the
  operator's real one and forces it through the broker). Codex, Claude Code,
  and Pi all use this path.

**A spec cannot ask for a host directory instead.** A run executes inside its
own microVM, which shares no filesystem with the host — the only things that
cross are the per-run block devices the backend builds. There is no `[sandbox]`
table in the format, so a spec that declares one fails to load rather than being
accepted and quietly ignored. An executor that can only authenticate by reading
its own dotfiles cannot be lent by a node.

---

## Spec sources

Specs load in two passes:

1. **Embedded built-ins** — every `specs/*.toml` compiled into the node
   (globbed by `build.rs`, sorted by file name). These parse through the
   exact same code path as operator files and serve as the reference
   examples.
2. **Workspace directory** — every `*.toml` in `<workspace>/capabilities`
   (`workspace_config::capability_dir`), only if it exists (an absent
   directory simply means "no operator specs"). Per workspace: two networks
   on one host offer two spec sets.

**Override rule:** an operator spec whose `tag` matches a built-in **replaces
it wholesale** — there is no field-level merging; the spec file is the unit of
override. This is the supported way to retune a built-in (different sandbox
flags, a different timeout, an explicit model flag): copy the embedded spec,
edit, drop it in the directory.

**Duplicate rule:** two files in the operator directory claiming the same tag
is a **hard error** naming both files. Precedence between operator files would
be file-order guessing; the node refuses to guess.

**Failure posture:** a spec that fails to parse or validate is a **boot
error**, not a skipped file. An operator config mistake should stop the node
loudly, not silently drop an executor and let jobs fail later with a confusing
"capability not provided". If the binary is missing, though, the capability is
simply not announced — a spec sitting in the directory is inert, not an error.

---

## Full annotated example

```toml
# capability spec format version. this build understands exactly 1; any other
# value is rejected loudly so a future-format spec is never silently misread.
spec = 1

[capability]
# the tag announced to the network-wide capability registry. shape rules are
# EXACTLY the consensus module's: 1..=64 bytes of [a-z0-9._-]. a tag that
# validates here can never bounce off an on-chain Announce.
tag = "myllm"
# human-facing one-liner for docs and status surfaces. optional.
description = "example one-shot CLI executor"

[detect]
# the binary name probed on PATH (first executable match wins).
bin = "myllm"
# optional env var naming an EXPLICIT binary path. when set, it wins over the
# PATH probe — and if it points at something that is not an executable file,
# the capability is dropped with a loud warning. an explicit override never
# silently falls back to PATH: you said "use this", and this doesn't exist.
env = "MYLLM_BIN"

[invoke]
# argv, passed to exec() verbatim — never through a shell, no placeholders.
# encode your fence here: sandbox flags, non-interactive mode, turn limits —
# and, if the executor takes one, the model as an ordinary literal flag.
args = ["run", "--json", "--model", "large-v2"]
# how the prompt reaches the child. v1 supports only "stdin": the prompt is
# written to the child's stdin (concurrently with output collection, so huge
# prompts can't deadlock the pipe) and then EOF. an argv placeholder would
# leak prompts into `ps` output and hit ARG_MAX — deliberately unsupported.
prompt = "stdin"
# the child's IDLE budget in seconds (1..=3600, default 300): any output on
# either stream REFRESHES it, so a long agentic run that keeps streaming
# (codex --json events, tool chatter) is never killed mid-work — only a child
# SILENT this long dies. a quiet-by-design CLI (claude -p prints one result at
# the end) gets exactly this many seconds of silence, so budget for its
# longest silent stretch. a continuously-chatty child is still bounded at
# `hard_timeout_factor` x this value (the host hard cap; the saga's consensus
# deadline bounds the run's outcome regardless).
# $DUCKTAPE_PROVIDER_TIMEOUT_SECS overrides every spec's idle budget at once
# (ops knob for slow hosts).
timeout_secs = 300
# the host hard cap as a multiple of the idle budget (1..=1000, default 36):
# even a child that never goes quiet is killed at `timeout_secs x this`.
hard_timeout_factor = 36

[output]
# which NAMED parser extracts the assistant's final text from stdout:
#   "jsonl-events" — a JSONL event stream; the LAST agent_message item wins;
#                    tolerates both item shapes seen in the wild and skips
#                    non-JSON noise lines.
#   "json-result"  — a {"type":"result",...} object, alone or the last
#                    result in a JSONL stream; is_error surfaces as an error.
#   "text"         — raw stdout, trimmed. THE GENERIC ESCAPE HATCH: any CLI
#                    that prints the answer plainly works with zero code.
#                    empty output on a zero exit is an error ("ran fine,
#                    said nothing" is a broken executor, not an answer).
# this is a CLOSED set on purpose: each name is a tested parser for a real
# output contract. a new name is a code change with tests — that's the point.
format = "text"
```

A file like this dropped into the operator directory is the whole wiring of
a new executor: after a node restart, if the binary is executable on `PATH`,
discovery builds the provider, the node **announces** the tag to the
capability registry (network-wide, member-gated, idempotent), and an agent
registered with that `capability` dispatches here with exactly the argv
above. The provider crate's unit test
`an_operator_spec_discovers_a_custom_executor` exercises this path with a
fake CLI.

---

## Field reference

### Top level

| Field | Type | Required | Rules |
|---|---|---|---|
| `spec` | integer | yes | must be `1` |
| `[source]` | table | no | the vendor release channel `ducktape agent install` resolves the latest build from — see [Source](#source--where-the-executable-comes-from) |
| `[isolation]` | table | no | host-owned auth broker + fresh executor config home — see [Isolation](#isolation--host-owned-auth) |
| `[tools]` | table | no | argv injected into every argv the file produces — see [Tools](#tools--argv-injected-into-every-argv-the-file-produces) |
| `[[variants]]` | array of tables | no | load-time expansion into finer tags — see [Variants](#variants--one-file-a-family-of-finer-tags) |

Unknown fields **anywhere** in the file are rejected — a typo, or a section
this format does not have (`[sandbox]`, `[models]`, `[session]`,
`[workspace]`), fails loud instead of being silently ignored.

### `[capability]`

| Field | Type | Required | Rules |
|---|---|---|---|
| `tag` | string | yes | 1..=64 bytes, `[a-z0-9._-]` only — the shared consensus rule (`capability_interface::validate_tag`) |
| `description` | string | no | free text |

### `[detect]`

| Field | Type | Required | Rules |
|---|---|---|---|
| `bin` | string | yes | non-empty; probed on `PATH` |
| `env` | string | no | env var naming an explicit binary path; override wins, broken override = warn + absent |
| `companions` | string array | no (default `[]`) | required executable sibling file names; each is mounted beside `bin` in sandbox guests, and a missing one makes the capability absent |

### `[source]`

| Field | Type | Required | Rules |
|---|---|---|---|
| `kind` | string | yes | `"claude-releases"` \| `"github-release"` \| `"github-bundle"`; the rest of the table is that kind's fields, any other field is unknown |
| `base` | string | `claude-releases` | https url of the feed: `<base>/latest`, `<base>/<version>/manifest.json`, `<base>/<version>/<platform>/<bin>`; `detect.companions` must be empty |
| `repo` | string | either GitHub kind | `owner/name` |
| `asset` | string | either GitHub kind | gzipped-tar asset name with `{arch}`: `x86_64` \| `aarch64` for `github-release`, `x64` \| `arm64` for `github-bundle` |
| `sums` | string | either GitHub kind | the release's sha256 sums file, `<hex>  <asset>` per line |
| `members` | string array | `github-release` | archive paths of exactly `detect.bin` and every `detect.companions` entry |
| `root` | string | `github-bundle` | one safe archive directory containing `detect.bin`; its entire tree is preserved, and `detect.companions` must be empty |

### `[invoke]`

| Field | Type | Required | Rules |
|---|---|---|---|
| `args` | string array | no (default `[]`) | passed verbatim to exec; fully literal, no placeholders |
| `prompt` | string | yes | must be `"stdin"` in v1 |
| `timeout_secs` | integer | no (default `300`) | 1..=3600; IDLE budget — output refreshes it; killed after this much silence, or at `hard_timeout_factor`x regardless |
| `hard_timeout_factor` | integer | no (default `36`) | 1..=1000; the host hard cap, as a multiple of the idle budget — a chatty-forever child is killed at `timeout_secs x` this |

### `[output]`

| Field | Type | Required | Rules |
|---|---|---|---|
| `format` | string | yes | `"jsonl-events"` \| `"json-result"` \| `"pi-json"` \| `"text"` |

### `[isolation]`

| Field | Type | Required | Rules |
|---|---|---|---|
| `config_home_env` | string | no | executor config-home env name such as `CODEX_HOME`; must match `[A-Z_][A-Z0-9_]*` |
| `broker` | string | no | `"codex-responses"` \| `"anthropic-messages"` \| `"pi"`; credentials remain in the host process |

Pi's `pi` broker selects Anthropic or OpenAI Codex from the borrowed credential;
without an explicit credential it uses the host Anthropic path. The runner
writes a fresh `PI_CODING_AGENT_DIR` with a model endpoint override and SSE
transport. Headless Pi runs explicitly load the staged Ducktape MCP extension;
interactive runs do not. Pi is solo-only: it declares no `restricted_args`, since
its interactive shell bypasses model-tool allowlists. Shared sessions are refused.
`pi-json` extracts the last completed assistant answer,
refuses failed or unfinished turns, and sums authoritative per-message usage
without counting streaming snapshots or repeated lifecycle events.

### `[tools]`

| Field | Type | Required | Rules |
|---|---|---|---|
| `args` | string array | yes (when the section is present) | spliced into every argv the file produces, immediately after `args[0]` |

### `[[variants]]`

| Field | Type | Required | Rules |
|---|---|---|---|
| `suffix` | string | yes | `<model>_<effort>`, each side `[a-z0-9.-]+` (so exactly one `_`) |
| `args` | string array | yes | the variant's **full** argv — verbatim, complete, never merged with or derived from the parent's args |

---

## Source — where the executable comes from

`[source]` names the vendor channel the executor's **Linux build** comes
from, which is what makes `ducktape agent install <tag>` able to fill the
workspace's `executors/` directory with it. The verb resolves the channel's
**latest** release at install time, downloads that release's artifact for the
guest's arch, verifies it against the checksum the vendor publishes **for
that release**, lifts the declared files into the directory and writes a
receipt (`<workspace>/executors.toml`: the version installed and the sha256
of the bytes written). No version and no hash is written down ahead of time,
in a spec or in Rust: what installs is whatever is current when the operator
approves it, and a receipt behind the channel's latest is offered as a bump on
the next install.

Three channel kinds exist:

- `claude-releases` — Anthropic's feed: `<base>/latest` is the version,
  `<base>/<version>/manifest.json` carries `platforms.<platform>.checksum`,
  and `<base>/<version>/<platform>/<detect.bin>` is the executable itself
  (`linux-x64` | `linux-arm64`). A single binary, so the spec declares no
  companions.
- `github-release` — a GitHub release: `releases/latest` names the tag,
  `asset` (with `{arch}` filled in) is a gzipped tar the release publishes a
  `sums` file beside, and `members` are the archive paths to lift. The
  members' file names must be exactly `detect.bin` plus `detect.companions`:
  a source that delivered fewer would leave a declared companion missing, and
  one that delivered more would bake an undeclared executable into the guest
  image.

- `github-bundle` — a GitHub release with a standalone executable and required
  sibling assets. The installer preserves `root` as a private package directory
  and exposes `detect.bin` through a relative symlink. Pi uses this source;
  copying only its executable loses package metadata and native modules.

A spec with no `[source]` is still a complete executor: the operator puts the
Linux build in the executors directory themselves, and `agent install`
reports it as their own.

---

## Isolation — host-owned auth

An executor CLI needs a model credential. `[isolation]` is the one way to
arrange that (see [Trust model](#trust-model--read-this-first)):

```toml
[isolation]
config_home_env = "CODEX_HOME"
broker = "codex-responses"
```

The host holds the credential and keeps it in the node process. The child
gets:

- a **loopback base URL** spliced into its argv as a custom model provider, and
- an **opaque random bearer** minted for this run,

neither of which can recover the credential, and both of which die with the run.
`config_home_env` is what makes this real rather than decorative: the child's
`CODEX_HOME` is a **fresh, empty** directory under the run's reserved
`.ducktape-run/` tree, so the CLI *cannot* fall back to reading the operator's
`auth.json`. Empty is guaranteed by the NAME: the slot under `.ducktape-run/` is
drawn fresh per run, so no later run can name — and therefore inherit — an
earlier one's config home, and the directory is removed when the run that owns it
ends rather than left in a workdir the next run mounts. That reserved directory is
also deleted before DuckFS or Forge scans the workspace, so the provider's runtime
state can never enter a snapshot or commit.

The broker is deliberately not a generic proxy: it binds an ephemeral loopback
port, requires the per-run bearer, accepts only the one API shape its kind
names (Responses POSTs for `codex-responses`, Messages POSTs for
`anthropic-messages`), enforces body/response/total-byte, concurrency and
request-count budgets, and is torn down when the run ends. It substitutes the
host credential only on the upstream hop.

**How the guest reaches it.** A microVM has its own network stack, so the
broker's `127.0.0.1` is not the guest's. `duck-guest-init` closes that gap
without giving the guest any host network at all: it listens on the broker's
port on the VM's *own* loopback and tunnels each connection to the host over
vsock. The CLI dials the localhost URL its env names and never knows.

---

## Tools — argv injected into every argv the file produces

`[tools]` names the flags that wire a tool plane in — in the built-ins, the
Ducktape MCP server — without making every argv in the file repeat them:

```toml
[tools]
args = ["-c", 'mcp_servers.ducktape.command="ducktape"', "-c", 'mcp_servers.ducktape.args=["mcp"]']
```

- **Insertion is immediately after `args[0]`, never at the end.** An argv
  like codex's ends in a bare `-` (the stdin marker) that must stay LAST;
  `args[0]` is always the mode/subcommand selector (`exec`, `-p`), so the
  slot after it is legal for every executor and stable across variants.
- It applies to the `[invoke] args` and to **every** `[[variants]]` `args`
  list (variants inherit `[tools]` like everything else — they never repeat
  it). No `[tools]` section, or an argv with fewer than one arg, means no
  insertion.
- Injection happens once, at load time: a spec in hand already has its
  tools, and one tag still means one fixed, fully literal argv.
- Override is still wholesale, by tag: an operator spec that replaces a
  built-in replaces its `[tools]` too. A `[tools]` section with no `args` is
  a hard error, like every other section that would do nothing.

The binary a `[tools]` argv names (`ducktape`, with `mcp` as its argument) is resolved
from the **run's `PATH`** — the provisioner puts its directory there — so specs
name no absolute path and stay portable across hosts. Claude's built-in also
passes `--allowedTools mcp__ducktape`: in `-p` print mode there is no human to
approve a tool call, so an unapproved MCP call is a denial and a merely
*configured* server would be dead weight. Headless Claude invocation also allows
shell and file tools inside the microVM, so builds need no permission prompt.
Interactive and restricted invocation keep their separate arguments. Claude streams verbose
JSON events and the provider extracts its terminal result. Events refresh the
idle budget and count toward the same 4 MiB output bound as other providers.

---

## Variants — one file, a family of finer tags

`[[variants]]` is **load-time sugar** over the finer-tag pattern above: each
entry registers an ADDITIONAL spec under the composed tag
`{parent_tag}_{suffix}`, exactly as if you had written one more spec file for
it. Nothing changes at dispatch time — **one tag still means one fixed,
fully literal argv**.

```toml
[capability]
tag = "myllm"
# … [detect] / [invoke] / [output] as above; the base tag's argv is untouched

# registers the tag "myllm_large-v2_high" with its OWN full argv.
[[variants]]
suffix = "large-v2_high"
args = ["run", "--model", "large-v2", "--effort", "high"]
```

A variant **inherits** `bin`, `env`, `prompt`, `timeout_secs`,
`hard_timeout_factor`, `output`, `[tools]` (and `description`) from the parent
spec; `args` is its own, whole, and literal. There is no field merging and no
placeholder substitution — the "argv is literal" invariant holds per tag, and
there is no routing table, no pattern matching, and no substitution anywhere in
the invoke path.

**The tag grammar.** A composed tag is `{provider}_{model}_{effort}` and
splits into **exactly three segments on `_`** — the contract the desktop
app's provider/model/effort picker decomposes tags by (e.g.
`codex_gpt-5.5_xhigh`, `claude_opus_max`). The loader enforces it fail-loud:
the parent tag must be underscore-free (a `my_llm` parent cannot declare
variants — write separate spec files instead); the suffix must be
`<model>_<effort>` with both sides non-empty `[a-z0-9.-]+`; the composed tag
must pass the shared consensus tag rule (≤ 64 bytes); duplicate suffixes in
one file — and composed tags colliding with any other tag in the operator
dir — are hard errors; unknown fields inside an entry are rejected. A tag
that does not follow the grammar is still a perfectly good tag — the app just
treats it as opaque (selectable as-is, no cascading picker).

**Override semantics are unchanged**: a variant tag is its own tag, and
operator specs override **by tag, wholesale**. Overriding a built-in base tag
(say `codex`) does NOT touch its built-in variant tags (`codex_*_*` remain);
override those individually if you want them retuned, or shadow them with an
operator file declaring its own `[[variants]]`.

The embedded built-ins use this to ship a curated model/effort matrix:
`codex` (base) plus
`codex_{gpt-5.6-sol,gpt-5.6-terra,gpt-5.6-luna}_{low,medium,high,xhigh,max}`
and `codex_gpt-5.5_{low,medium,high,xhigh}` (the effort set follows what each
model actually supports, so the codex side is not a rectangle), and `claude`
(base) plus `claude_{fable,opus,sonnet,haiku}_{low,medium,high,max}`.

---

## Environment knobs

| Variable | Effect |
|---|---|
| *(per spec)* `[detect].env` | each spec may name its own explicit-binary override var — see the embedded specs for theirs |
| `DUCKTAPE_PROVIDER_TIMEOUT_SECS` | overrides **every** spec's `timeout_secs` at once |

---

## Versioning

`spec = 1` is the only version this build reads, and there are no live
networks, so the format changes in place: a section this build does not know
fails loudly at boot as an unknown field, never as a silent behavior change.
Every run starts cold — a run's whole continuity is its envelope, which is
what lets any assignee execute it — and a run's cwd is the workspace its
provisioner materialized, so the format has no per-run session or workspace
sections. Yes, that is strict; strict is the point: a spec means one thing,
on every build that accepts it.
