---
name: graft
description: Use when locating code, tracing who calls a symbol, or scoping a
  multi-file change in this repo — graft (tree-sitter symbol graph over the
  tree) answers in one call where a grep-and-read loop takes several.
---

# graft

`ops/graft` wraps the `graft` CLI (`npm i -g @nanonets/graft`) with the repo's
settings: telemetry off, no `.ignore` that re-admits the card cache to ripgrep,
and the tokens-saved banner stripped. Always call it as `ops/graft`, never bare
`graft`. The graph lives in `graft/` (gitignored, ~140 MB, `ops/graft build`
makes it in under a minute; every query refreshes changed files itself, so a
rebuild after an edit is never needed).

The graph is structural only: symbols plus name-resolved call edges. Rust is
in graft's "broad" tier, so an edge is a name match, not a type-resolved call.
Cards under `graft/` are symbol lists, not prose. Nothing here calls a model.

## Commands (one call answers; do not chain)

- `ops/graft callers <symbol> [--direction out] [--depth N|all]` — who calls or
  references it, with `file:line` at each site. Run before a rename, a
  signature change, or a deletion; `--depth all` before a multi-file refactor.
- `ops/graft ask "<words>" --source [-n N] [--in <path>/]` — ranked symbols
  matching the words, with the crux of each definition inlined. Ranking is
  lexical over symbol names: name the thing (`join gate settle`), not the
  concept (`how does a member join`). Weak hits mean switch tool, not reword.
- `ops/graft grep "<pattern>" [-i] [--in <path>/]` — every occurrence, grouped
  by enclosing symbol.
- `ops/graft skeleton <file>` — every signature in a file with its span.
- `ops/graft map` — directory clusters, hubs and hotspots for orientation.

`rg` stays right for anything graft does not index (docs, configs, a file
created this turn) and for a literal you already know the file of.
