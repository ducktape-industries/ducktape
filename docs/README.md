# Documentation index

One line per document, grouped by the question it answers, so an agent or an
operator loads the one file that answers it instead of the tree. The rule for
what lives here is `AGENTS.md` § "Docs Are Not a Record": `docs/` holds what
an operator executes and the few references code cites by path. A document
nothing cites is deleted, not archived.

## Start here

| Question | Read |
| --- | --- |
| What is this, how is the tree laid out, how do I build, test and run it | [`../README.md`](../README.md) |
| What rules bind an assistant working in this repo | [`../AGENTS.md`](../AGENTS.md) |

## Operate

| Question | Read |
| --- | --- |
| Which operator scripts and harnesses live under `ops/` | [`../ops/README.md`](../ops/README.md) |

`docs/superpowers/` is gitignored planning scratch; nothing under it ships.
