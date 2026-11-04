# ocsess

TUI to browse, filter, sort, and act on OpenCode sessions. Rust + ratatui, single binary.

## Commands

```bash
cargo run                    # dev
cargo test                   # unit checks for filter/sort/state parsing
cargo build --release        # binary at target/release/ocsess
cargo run -- --dump          # headless: print rows, no TUI (debugging)
cargo run -- --serve 8765    # read-only web view on 127.0.0.1
```

## Layout

| File | Role |
|------|------|
| `src/main.rs` | Event loop, terminal suspend/resume, key + mouse dispatch, CLI flags |
| `src/app.rs` | App state, filtering/sorting, modals, action dispatch |
| `src/db.rs` | Read-only SQLite queries, `Session` row mapping, status derivation |
| `src/api.rs` | OpenCode service discovery, HTTP actions, backup export |
| `src/ui.rs` | Table, header, filter bar, modals, toasts |
| `src/web.rs` | Read-only HTTP server, cached snapshot, JSON endpoints |
| `src/web/index.html` | Embedded web page (HTML, CSS and JS inline) |
| `src/config.rs` | Persisted columns/sort/widths in `~/.config/ocsess/config.json` |

## OpenCode data

- Live DB: `~/.local/share/opencode/opencode.db` (WAL). Layout is detected per database: **v2** uses `session_v2` + `session_message`; **v1** uses `session` + `message` + `part`. Never hard-code `session_v2` alone: a missing column must become `NULL`, not a query error.
- Open the DB read-only (`mode=ro`, `busy_timeout`). Actions go through the local API so OpenCode stays the only writer.
- API URL + password: `~/.local/state/opencode/service.json`. Basic auth user `opencode`, password from that file. `opencode service start` if unreachable.
- Actions: `PATCH /api/session/{id}` (`title`, `metadata`), `POST /api/session/{id}/fork`, `POST /api/session/{id}/compact`, `DELETE /api/session/{id}`, `GET /api/session`.
- Open a session: `opencode -s <id> <directory>` (positional directory); suspend/restore the TUI around it.
- Status comes from `GET /api/session/active` (running) merged with `time_busy` (v1), `time_idle`, `time_suspended`, `idle_outcome`. v2 dropped `time_busy`: with the API offline the fallback is `time_updated > time_idle` within 15 minutes (`db::BUSY_WINDOW_MS`).
- `PATCH` no longer accepts `archived` (v2 ignores it silently, and there is no archive endpoint). `Api::supports_archive` probes `/api/info` for the server version once at startup: on v2 (`major >= 2`) `a`/`u` are hidden and refuse with a message, on v1 they work. `Api::set_archived` also re-reads the session and errors if the server did not apply the change.
- Per-call data is in the message `data` JSON (`type='assistant'` on v2): `cost`, `tokens.*`, `model.id` or `modelID`, `finish`, and tool parts (`content[]` inline on v2, `part` rows on v1). `session_part` is empty in v2; `part` is v1.
- Schema is internal and may drift: on query failure fall back to `GET /api/session?limit=1000`.

## Design decisions

- No cache database. The source DB is already SQLite and small; polling it every ~700 ms is the live feed.
- Archive = gzip export to `~/.local/state/ocsess/backups/` then `archived=true`. Permanent delete backs up the session and all descendants first.
- Compact spends tokens: confirm before sending.
- Keyboard first, mouse parity: header click sorts, row click selects, double-click opens, wheel scrolls.
- Colors are semantic: green/blue/gray/magenta states, cost heat, recency, cyan current-session marker, yellow sort arrow.
- `i` opens a per-call detail view using `db::load_calls`; queries are on demand and auto-refresh every 2 seconds.
- `--serve [PORT]` adds a read-only web view on `127.0.0.1` (default 8765). A background thread refreshes a cached JSON snapshot every second and merges the active set every two; handlers never touch the DB or API. No new crates: a small HTTP/1.1 GET/HEAD server over `std::net`.

## Conventions

- New behaviour ships with one runnable check in `cargo test` or an assert-based self-check; no test frameworks beyond std.
- Keep dependencies minimal and justify each new crate in the PR/commit.
- User-facing messages in English; code, comments, and commits in English.
