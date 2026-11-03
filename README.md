# ocsess

A keyboard-first TUI for browsing and managing OpenCode sessions.

![ocsess session browser](docs/ocsess.jpg)

## Features

- Live sessions table: read-only SQLite polling every 700 ms, running state from the OpenCode API every 2 s.
- Sort, global search, per-column filters, show/hide/reorder columns, adjustable widths.
- Open a session directly in its directory with `opencode -s <id> <dir>`.
- Rename, fork, archive, unarchive, delete, and compact sessions.
- Gzip backups before archive and before permanent deletion (including child sessions).
- Per-call detail view: cost, input/output/reasoning/cache tokens, duration, finish reason, and tools used.
- Keyboard-first with mouse parity: click headers, select rows, double-click to open, wheel to scroll.

## Install

Requires Rust 1.89+ and OpenCode.

```bash
cargo build --release
./target/release/ocsess
```

Or install the binary:

```bash
cargo install --path .
```

## Usage

```bash
ocsess                 # TUI
ocsess --dump          # sessions as TSV, no TUI
ocsess --db PATH       # explicit SQLite database
```

The TUI reads `~/.local/share/opencode/opencode.db` in read-only mode. Actions go through the local OpenCode API (`~/.local/state/opencode/service.json`).

## Keys

| Key | Action |
|-----|--------|
| `↑↓` / `j k` | move |
| `gg` / `G` | top / bottom |
| `←→` / `h l` | active column |
| `s` | sort active column |
| `/` | global search |
| `f` | per-column filter (`~` contains, `=` exact, `>N`, `<N`) |
| `F` / `Esc` | clear search and filters |
| `A` | show / hide archived |
| `c` | columns: toggle, width, order |
| `Enter` | open the session in opencode |
| `i` | call detail |
| `d` | duplicate (fork) |
| `r` | rename |
| `a` / `u` | archive / unarchive |
| `D` | delete (gzip backup first) |
| `m` | compact |
| `R` | reload |
| `q` | quit |

Mouse: click a header to sort, click a row to select, double-click to open, wheel scrolls.

## Call detail

Press `i` on a session to see every LLM call:

- Summary: calls, total cost, average cost per call, tools, tool errors, tokens, cache, models.
- Table: time, model, cost, input/output/cache tokens, duration, finish reason, tools used.
- Per-call tools are aggregated (`shell×3`, `read!` when a tool errored).
- Refreshes every 2 seconds while open.

Detail keys: `↑↓` scroll, `g`/`G` newest/oldest, `PgUp`/`PgDn`, `Ctrl+d`/`Ctrl+u`, `R` reload, `i`/`Esc` close.

## Colors

- State: green running, blue suspended, gray idle, magenta archived.
- Cost: green `< $0.05`, yellow `< $0.5`, orange `< $2`, red `>= $2`.
- Updated: cyan for "now", gray for the last week, dim for older.
- Current session: `◆` prefix. Active header: cyan. Sort arrow: yellow.

## Data notes

- Sessions live in `session_v2`; message counts come from `session_message`.
- Per-call usage is stored in `session_message.data` for `type='assistant'`: `cost`, `tokens.*`, `model.id`, `finish`, and inline tool parts.
- One assistant message is treated as one LLM call. Retries and streaming chunks are not separate calls.
- The SQLite schema is internal to OpenCode and may drift; the app falls back to `GET /api/session` if a query fails.

## Files

- Config: `~/.config/ocsess/config.json`
- Backups: `~/.local/state/ocsess/backups/*.json.gz` (directory 700, files 600)

Archiving is native (`time_archived`) and reversible. Permanent deletion always writes gzip backups first.
