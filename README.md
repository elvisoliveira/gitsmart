# gitsmart

A read-only git dashboard for the terminal, in one binary that needs only `git`:

```
┌───────────────┬──────────────┐
│ working tree  │              │
├───────────────┤     diff     │
│     log       │ side by side │
├───────┬───────┤              │
│branchs│ tags  │              │
└───────┴───────┴──────────────┘
```

Moving through the lists on the left updates the diff on the right. Unpushed
commits get a bold-red hash. Typing filters the list under the cursor; the
working tree keeps its tree shape while filtering, the log drops the graph. The
log's first row, "Working Tree/Index", shows the uncommitted diff.

The side-by-side diff is rendered in-process: a port of the part of
[delta](https://github.com/dandavison/delta) used here (`--side-by-side
--wrap-max-lines 0 --dark`), checked to match delta's output cell for cell.
`gitsmart --render-diff WIDTH [--ansi] < patch` prints that rendering for
comparison.

## Keys

| key | action |
|---|---|
| `C-b h/j/k/l`, `C-w h/j/k/l`, `Tab` | move between panes |
| `⇧J/K`, `C-j/k`, arrows | move the list / scroll the diff |
| `⇧H/L` | first / last row |
| `C-d`/`C-u` | half page |
| `C-b z` | zoom the focused pane |
| `C-r` | reload everything |
| `C-g`, `C-b q` | quit |

## Install

```
cargo install --root ~/.local --git ssh://git@github.com/elvisoliveira/gitsmart
```

(`~/.local/bin/gitsmart`. With a private repo and an SSH agent, set
`CARGO_NET_GIT_FETCH_WITH_CLI=true`.) From a checkout: `cargo install --root ~/.local --path .`

Run it inside a repository: `gitsmart`, or `GIT_ROOT=/path gitsmart`.
