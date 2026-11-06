# ocsess

A keyboard-first TUI for browsing and managing OpenCode sessions.

![ocsess session browser](docs/ocsess.jpg)

## Features

- Live sessions table: read-only SQLite polling every 700 ms, running state from the OpenCode API every 2 s.
- Optional read-only web view on localhost (`--serve`), refreshed every second.
- Works with both OpenCode schema generations: v1 (`session`, `message`, `part`) and v2 (`session_v2`, `session_message`). The layout and columns are detected at load time.
- Sort, global search, per-column filters, show/hide/reorder columns, adjustable widths.
- Open a session directly in its directory with `opencode -s <id> <dir>`.
- Rename, fork, delete, and compact sessions. Archive and unarchive on OpenCode versions that support them.
- Gzip backups before archive and before permanent deletion (including child sessions).
- Per-call detail view: cost, input/output/reasoning/cache tokens, duration, finish reason, and tools used.
- Keyboard-first with mouse parity: click headers, select rows, double-click to open, wheel to scroll.

## Install

Prebuilt binaries for Linux (x86_64, aarch64) and macOS (Intel, Apple Silicon):

```bash
curl -sSfL https://raw.githubusercontent.com/iesta/ocsess/main/install.sh | sh
```

The script installs to `~/.local/bin`. Set `OCSESS_INSTALL_DIR` to change that, and `OCSESS_VERSION=v0.3.0` to pin a release. It verifies the SHA256 sum before installing and tells you if the directory is not on `PATH`.

From source, with Rust 1.89+:

```bash
cargo install --git https://github.com/iesta/ocsess    # install the binary
cargo build --release                                  # or build a checkout
./target/release/ocsess
```

## Usage

```bash
ocsess                 # TUI
ocsess --dump          # sessions as TSV, no TUI
ocsess --serve [PORT]  # read-only web view on 127.0.0.1 (default 8765)
ocsess --db PATH       # explicit SQLite database
```

The database is opened read-only. Actions go through the local OpenCode API, whose URL and password come from `~/.local/state/opencode/service.json`. For an action, if that file is missing or the service is down, ocsess runs `opencode service start` once and retries.

The database path is auto-discovered: `$XDG_DATA_HOME/opencode/opencode.db`, then `opencode debug paths`, and it can be overridden with `--db` or `OCSESS_DB`.

## Web view

```bash
ocsess --serve          # http://127.0.0.1:8765
ocsess --serve 8080     # custom port
ocsess --serve=8080     # same, one argument
ocsess --serve 0        # pick a free port and print it
```

The page refreshes itself every second: sort by clicking a header, search, toggle archived, and click a row for the per-call detail. A dot next to the title shows whether the OpenCode API is reachable; a banner appears if the server is lost and clears itself when it returns.

It is read-only and bound to `127.0.0.1`, so nothing outside the machine can reach it and nothing there can modify your sessions. Rename, fork, delete, and compact stay in the TUI.

Endpoints (GET and HEAD only; anything else is `405`):

| Route | Response |
|-------|----------|
| `GET /` | the page |
| `GET /api/sessions` | cached session list, rebuilt every second |
| `GET /api/sessions/{id}/calls` | per-call detail, `404` for an unknown id |
| `GET /api/health` | `ok`, session count, API status |

A background thread rebuilds one snapshot per second and serves the session list from memory, so polling never touches the database or the API. The per-call endpoint queries the database on demand.

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
| `a` / `u` | archive / unarchive (only where supported) |
| `D` | delete (gzip backup first) |
| `m` | compact |
| `R` | reload |
| `q` | quit |

Mouse: click a header to sort, click a row to select, double-click to open, wheel scrolls.

Opening the current session is refused, as are archive, delete, and compact on a running session.

## Call detail

Press `i` on a session to see every LLM call:

- Summary: calls, total cost, average cost per call, tools, tool errors, tokens, cache, models.
- Table: time, model, cost, input/output/cache tokens, duration, finish reason, tools used.
- Per-call tools are aggregated (`shell×3`, `read!` when a tool errored).
- Refreshes every 2 seconds while open.

Detail keys: `↑↓` scroll, `g`/`G` newest/oldest, `PgUp`/`PgDn`, `Ctrl+d`/`Ctrl+u`, `R` reload, `i`/`Esc` close. The web view shows the same table when you click a row.

## Colors

- State: green running, blue suspended, gray idle, magenta archived.
- Cost: green `< $0.05`, yellow `< $0.5`, orange `< $2`, red `>= $2`.
- Updated: cyan within the last minute, gray within the week, dim older.
- Current session: `◆` prefix. Active header: cyan. Sort arrow: yellow.

## Data notes

- The database layout is detected per database: sessions live in `session_v2` (v2) or `session` (v1), messages in `session_message` or `message`/`part`. A column missing from the installed schema reads as `NULL` instead of failing the query.
- Message counts come from `session_message` (v2) or `message` (v1).
- Per-call usage lives in the message `data` JSON: `cost`, `tokens.*`, `model.id` (or `modelID`), `finish`, and tool parts — inline in `content` on v2, in the `part` table on v1.
- One assistant message is treated as one LLM call. Retries and streaming chunks are not separate calls. The newest in-flight message has no cost or tokens yet, so its detail row shows `—` until the next refresh.
- If a query fails, the app falls back to `GET /api/session`; `--dump` does the same and reports the fallback on stderr.

## OpenCode compatibility

| OpenCode | Database | API |
|----------|----------|-----|
| v1 | `session`, `message`, `part`, `time_busy` | list/patch/delete/fork/compact |
| v2 | `session_v2`, `session_message` | same routes, payload changed |

Checked against OpenCode 2.0.19:

- **Running state.** The v2 API payload has no `time.busy`; it reports `time.created`, `updated`, `idle`, and `viewed`. The database still has a nullable `time_busy` column, written only while a run is live. ocsess uses the column when it is set and otherwise treats an update newer than the idle mark — and younger than 15 minutes — as running. When the service answers, its `/api/session/active` list overrides both, so a session killed mid-run stops looking busy after the window.
- **Archiving may have no write path.** `PATCH /api/session/{id}` accepts `archived` and some v2 builds then ignore it. There is no archive endpoint. ocsess asks `/api/info` for the server version once at startup: on a v2 server the `a`/`u` keys are hidden and pressing them explains why. On v1 they work as before. Archived data that already exists stays visible through the `A` filter and the Archived column.
- **Fork, rename, delete, and compact** keep working unchanged.
- **Session payloads** are wrapped in `data`, with the directory under `location.directory` and the model as an object; older services use a flat `directory` and a model string. Both shapes are parsed.

## Files

- Config: `~/.config/ocsess/config.json` (columns, widths, sort, archived visibility)
- Backups: `~/.local/state/ocsess/backups/*.json.gz` (directory 700, files 600)

`XDG_CONFIG_HOME` and `XDG_STATE_HOME` are honored.

Permanent deletion always writes gzip backups first, for the session and every descendant. Archiving backs up as well, and the backup is kept if the archive call fails.

## Development

```bash
cargo test                   # unit checks for filters, sorting, status, formats, web routes
cargo clippy --all-targets
cargo fmt --check
cargo run -- --dump          # headless, no TUI
cargo run -- --serve 8765    # web view
```

`AGENTS.md` describes the module layout and the data-flow decisions. New behaviour ships with a runnable check in `cargo test`; there are no test frameworks beyond `std`.
