# CLAUDE.md: build brief for the feed-index fetcher

You are building the **fetcher**: a Rust command-line program that polls RSS/Atom feeds listed in
an OPML file and indexes their entries into PostgreSQL. The schema already exists
(`migrations/0001_init.sql`) and its reasoning is in `docs/schema.md`. **Read both before writing
code.** This file is the spec; where it and your instincts disagree, follow this file and say so in
your summary.

This program is a sibling of [`maildir-index`](https://github.com/toddbony/maildir-index) (same
owner, same licence, same database, same deployment pattern). Where this brief says "as in
maildir-index", read that repository's code and copy the approach; copying code from it is fine
(same author, same dual licence).

## Ground rules

- **This repository is public.** Never commit a real feed list, real host names, IP addresses or
  site paths. Test fixtures are hand-written feeds on `example.org` / `example.com`. Config
  examples use placeholders.
- **Log hygiene.** Log feed ids, feed **hosts**, HTTP status codes, outcomes, counts and durations.
  Never log entry titles, entry links, entry content, or full feed URLs (the owner's reading list is
  personal even though each feed is public). A test enforces this; see Tests.
- **Untrusted input.** Feed documents are attacker-controlled: cap sizes, never panic on them,
  never follow `file:` or other non-http(s) URLs, and never render their HTML anywhere.
- Work on a branch named `fetcher`, in small commits with clear messages. Don't push; the owner
  pushes.
- **`migrations/0001_init.sql` is frozen once it has been applied anywhere.** As of this brief it
  has not been applied, so fixing a real defect in it is allowed: say so explicitly in your summary
  if you do. Schema changes after that go in `0002_…`.

## What exists

```
migrations/0001_init.sql   schema v1 in schema "feeds": feeds, entries, fetch_runs, fetches
docs/schema.md             why the schema is shaped this way (read it)
example.toml, example.opml config examples (become /usr/share/doc/feed-index/*.example)
README.md, LICENSE-*       dual MIT OR Apache-2.0
```

## Crate

One crate, `feed-index`, library + one binary of the same name, edition 2024. Suggested
dependencies (check current versions and APIs; adjust if a crate's API differs from what's assumed):

- `feed-rs` (RSS 0.9/1.0/2.0, Atom, JSON Feed), `roxmltree` or `quick-xml` (OPML)
- `reqwest` with `rustls-tls`, `gzip`, `brotli`, `deflate`, `stream`; **no** default native-tls
- `html2text`, `sha2` + `hex`, `url`, `chrono`
- `sqlx` 0.9 with `postgres`, `runtime-tokio`, `tls-rustls`, `migrate`, `chrono`
- `tokio`, `clap` (derive), `serde` + `toml`, `anyhow`, `tracing` + `tracing-subscriber`
- the `whoami` `std` feature workaround from maildir-index's `Cargo.toml` (read its comment)

Use **runtime queries** (`sqlx::query`, `query_as`), not the compile-time `query!` macros, so the
package builds without a database and without committed `.sqlx` data. **Every** table name in SQL
is schema-qualified (`feeds.entries`); nothing may depend on `search_path`.

## Database

**Connection: as in maildir-index `src/db.rs`.** libpq environment only (`PGHOST`, `PGPORT`,
`PGDATABASE`, `PGUSER`, `PGSSLMODE`, password **only** via `PGPASSFILE`); refuse `PGPASSWORD`;
refuse an unrecognised `PGSSLMODE`; never fall back to `~/.pgpass`; `application_name` =
`feed-index`. With `PGSSLMODE=require` a non-TLS connection must fail.

**Migrations: the migration record lives in the `feeds` schema.** Build the migrator from
`sqlx::migrate!()` and before running it call
`dangerous_set_table_name("feeds._sqlx_migrations")` and `create_schema("feeds")`. This is
required, not a preference: the production database also holds maildir-index, whose migrator uses
`public._sqlx_migrations` and has its own version 1. A shared table breaks one program or the other
(`VersionMismatch(1)` here, `VersionMissing` there); this was reproduced before writing this brief.
The migration SQL therefore does **not** contain `CREATE SCHEMA` (the migrator creates it first).

**One run at a time.** Take a session advisory lock (`pg_try_advisory_lock`, a fixed key derived
from the string `feed-index fetch`) on a dedicated connection held for the whole run, and release
it explicitly at the end. If the lock is taken, log one line and exit 0 without fetching. (Keep it
off the pooled connections: a session lock on a pooled connection outlives the code that took it.)

## Configuration

**One TOML file**, default `/etc/feed-index/config.toml`, overridable with `--config`. See
`example.toml` for every key and its default; unknown keys are an error (`deny_unknown_fields`).
Validate ranges (`default_poll_minutes` 5–1440, `concurrency` 1–16, sizes and timeouts > 0).
Report TOML errors by line number and message, as maildir-index does.

**The feed list is OPML 2.0**, at the path the config names. Parsing rules:

- A feed is any `<outline>` with an `xmlUrl` attribute, at any depth.
- `title` = its `title` attribute, else `text`, else the URL's host.
- `folder` = `text` (else `title`) of the **nearest enclosing** outline without `xmlUrl`; NULL at top level.
- `htmlUrl` → `html_url`. Optional `poll_minutes` (integer 5–1440; else the config default) and
  `weight` (integer; else 0). Other attributes are ignored.
- Only `http` and `https` URLs. Anything else, a duplicate `xmlUrl`, or an invalid
  `poll_minutes`/`weight` is a **configuration error**: the run stops before any change.

**Sync (every `fetch`, and the `sync` command):** upsert `feeds.feeds` by `xml_url` (update title,
folder, html_url, poll_minutes, weight; clear `retired_at`). Active feeds not in the file get
`retired_at = now()`. A newly added feed is due immediately. Changing `poll_minutes` does not move
`next_fetch_at`.

**Retire guard** (the analogue of maildir-index's gone guard): if the file holds zero feeds, or
fewer than half of the currently active feeds, retire nothing, log an error and exit non-zero. It
protects the history from a truncated or wrong file.

## Fetching

**Which feeds:** active (`retired_at IS NULL`) feeds with `next_fetch_at <= now()`. `--force`
ignores `next_fetch_at`; `--feed ID` (repeatable) restricts to those feed ids. Fetch with at most
`concurrency` requests in flight. **One bad feed never stops a run.**

**Request:** `GET xml_url` with

- `User-Agent: feed-index/<version> (+<contact>)`
- `Accept: application/rss+xml, application/atom+xml, application/feed+json, application/xml;q=0.9, text/xml;q=0.8, */*;q=0.5`
- `If-None-Match: <etag>` / `If-Modified-Since: <last_modified>` when stored
- compressed responses accepted; at most 5 redirects, http(s) only; `timeout_seconds` for the
  whole exchange. Read the body as a stream and stop at `max_feed_bytes`.

**Outcomes** (one `feeds.fetches` row per attempt, one of the schema's `outcome` values):

| Response | Outcome | Feed row |
|---|---|---|
| 200, parses as a feed | `ok` | store `etag`, `last_modified` from this response (NULL if absent); `declared_title`; `resolved_url` = final URL if different from `xml_url`, else NULL; `last_ok_at`; reset failures |
| 304 | `not_modified` | keep `etag`/`last_modified`; `last_ok_at`; reset failures |
| 200, does not parse | `parse_error` | failure. If the content type is HTML, say so in `error` ("got text/html, not a feed"): that is how moved feeds usually look |
| body over the cap | `too_large` | failure; nothing parsed |
| any other status | `http_error` | failure; `http_status` recorded |
| DNS, TLS, connect, timeout | `network_error` | failure |

`error` is a short description, never the response body. Every attempt sets `last_attempt_at`.

**Scheduling after the attempt:**

- Success: `next_fetch_at = now + poll_minutes`, plus a random 0–10% jitter.
- Failure: `consecutive_failures += 1`, `last_error` set,
  `next_fetch_at = now + min(poll_minutes × 2^(failures−1), 24 h)`.
- `429` or `503` with a `Retry-After` (seconds or HTTP date): `next_fetch_at` is at least that time,
  capped at 24 h.

`xml_url` is never changed by the program, whatever the redirects say.

## Entries

For each entry in a parsed document:

- **Key:** the entry's id (RSS `guid`, Atom `id`) trimmed, `key_source = 'id'`. If it is empty or
  absent: `fp:` + hex SHA-256 of `normalised_link + "\n" + normalised_title`,
  `key_source = 'fingerprint'`. Normalised link: parsed URL with lowercase scheme and host, no
  fragment, query parameters starting with `utm_` removed, otherwise as written. Normalised title:
  whitespace runs collapsed to one space, trimmed. **`feed-rs` (3.0) synthesises an id when an
  entry has none** (`parser::Builder` → `assign_missing_ids`), so `Entry::id` is never empty and a
  synthesised id would be indistinguishable from a real one. Build the parser with
  `parser::Builder::new().id_generator(…)` returning a sentinel that cannot occur in XML (e.g. a
  string starting with `\u{0}`), and treat that sentinel as "no id". Verify this against the
  installed version and say what you found in the summary.
- `link`: the `alternate` link if there is one, else the first link. `title`, `author` (first
  author's name): decoded text.
- `published_at` / `updated_at`: as parsed; NULL when absent, unparseable or outside 1971–2100.
- `summary_html`, `content_html`: exactly as the feed gave them (after `feed-rs` decoding), NULL if absent.
- `body_text` / `body_source`: content if it has text, else summary, rendered with `html2text`
  and then **normalised and link-reduced exactly as in maildir-index** (`src/extract.rs`,
  `src/links.rs`: invisible characters, whitespace and blank-line collapsing, Safe Links/Google
  unwrapping, query/fragment dropped). Plain-text content is normalised without rendering. Nothing
  left → NULL / `'none'`. A renderer panic is contained: `body_text` NULL, `body_source = 'none'`,
  the entry is still stored.
- `content_sha256`: SHA-256 over title, link, summary_html and content_html, each length-prefixed
  (so field boundaries can't shift). Strip NUL from every text column first.
- Duplicate keys within one document: keep the first, ignore the rest.

**Write rules** (one transaction per feed fetch):

- New `(feed_id, entry_key)` → insert. `backfill = true` when this is the feed's **first ever
  successful parse** (`last_ok_at` was NULL before this fetch), else false. Set
  `last_new_entry_at` on the feed.
- Existing, same `content_sha256` → **no write at all**.
- Existing, different hash → update the content fields, `revisions += 1`,
  `last_changed_at = now()`. `first_seen_at` and `backfill` never change.

**End of run:** delete `feeds.fetches` rows older than `fetch_history_days`, write the
`fetch_runs` totals, and print one summary line to stdout, `key=value` style:

```
feed-index run=812 due=9 ok=7 not_modified=1 failed=1 new_entries=14 changed_entries=2 retired=0 duration_s=6
```

plus one line per failed feed: `feed-index feed_id=12 host=news.example.org outcome=http_error status=404`.

**Exit status:** non-zero for run-level failures only (configuration, OPML, retire guard, database,
lock errors). Individual feed failures are normal for a fetcher, are recorded per feed, and do not
fail the unit; `status` and the data are where they are seen.

## Commands

```
feed-index migrate                       apply embedded migrations and exit
feed-index sync                          migrate, then sync the OPML into feeds.feeds; print added/updated/retired
feed-index fetch [--feed ID]... [--force]
                                         migrate, sync, fetch due feeds (as above)
feed-index status [--quiet-days N]       one line per active feed: id, folder, title, last ok,
                                         last new entry, failures, last outcome, entry count.
                                         Marks failing feeds, and feeds with no new entry for N days (default 14)
feed-index probe URL... | --opml FILE [--show]
                                         NO database: fetch and parse each feed exactly as `fetch`
                                         would (no conditional headers), print outcome, status,
                                         final URL, declared title, entry count, id vs fingerprint
                                         counts, newest published date. --show adds the first 3
                                         entries' titles and the first 300 characters of body_text,
                                         for human review
```

`probe` is how the owner checks a feed list before loading it, so it must work without
configuration beyond the defaults (`--config` optional; use defaults when the file is missing). It
prints to stdout for a human, so feed URLs and entry titles **are** allowed in its output; the log
hygiene rule applies to logs.

## Tests

Unit tests with hand-written fixtures in `tests/fixtures/` (`example.org` feeds). HTTP tests run
against a **local** in-process server (e.g. `axum` or `hyper` on 127.0.0.1, or `wiremock`); no test
may touch the internet. Integration tests use a **local** PostgreSQL via `DATABASE_URL` (skip, with
a clear message, if unset); never a shared or production database. Required cases:

1. **Migration coexistence:** in one database, run a stand-in sqlx migrator that uses the default
   `public._sqlx_migrations` with its own version 1, then this program's migrator, then each again
   → all four succeed; `feeds._sqlx_migrations` holds only this program's rows and the public table
   only the stand-in's.
2. OPML: nested folders, attribute defaults, `title`/`text` fallback; duplicate `xmlUrl`, bad
   `poll_minutes` and a non-http URL → configuration error, database unchanged.
3. Sync: removed feed → retired; re-added → `retired_at` cleared; title change → updated.
4. Retire guard: empty OPML, and an OPML with fewer than half the active feeds → nothing retired, non-zero exit.
5. RSS 2.0 with guids, Atom with ids, RSS 1.0 (RDF) and JSON Feed fixtures parse and store correctly.
6. RSS without guids → fingerprint keys; the same item with `utm_*` parameters or a fragment added
   to its link → the same key.
7. **Idempotency:** the same document fetched twice → second fetch: 0 new, 0 changed, no row updated.
8. **Edit:** same guid, changed title → one row, `revisions = 1`, `last_changed_at` set, title updated.
9. **Backfill:** entries from a feed's first fetch have `backfill = true`; an entry added later has false.
10. **Conditional GET:** the server sends an ETag; the next fetch sends `If-None-Match`, gets 304 →
    `not_modified`, ETag kept, no entry writes.
11. **Failure backoff:** 404 → `http_error`, failures 1, next fetch one interval out; a second 404
    doubles it; a success resets failures and `last_error`.
12. `429` with `Retry-After: 3600` → `next_fetch_at` about an hour out.
13. Body over `max_feed_bytes` → `too_large`, nothing parsed or stored.
14. An HTML page instead of a feed → `parse_error`, error mentions text/html.
15. A 301 to another path → followed, `resolved_url` recorded, `xml_url` unchanged.
16. NUL bytes in title and content → stripped, entry stored. The html2text `rowspan="0"` table that
    panics (see maildir-index tests) in content → `body_source = 'none'`, entry stored.
17. Duplicate guid inside one document → one row.
18. Due selection: a not-yet-due feed is skipped; `--force` fetches it; a retired feed is never fetched.
19. `fetches` rows older than `fetch_history_days` are deleted at the end of a run.
20. **Run lock:** a second run started while the first holds the lock exits 0 without fetching.
21. **Log hygiene:** fetch fixtures with distinctive titles, links and content (and full feed
    URLs), capture all log output at `trace` level, assert none of those strings appear.
22. Dates: absent → NULL; year 1900 or 2200 → NULL.

## Packaging (`cargo deb`)

Same pattern as maildir-index: the package installs things and registers units; it **enables
nothing** and **installs no config into `/etc`**.

- `/usr/bin/feed-index`
- `usr/lib/systemd/system/feed-index-fetch.service`: `Type=oneshot`,
  `ExecStart=/usr/bin/feed-index fetch`, `User=feed-index`, `Group=feed-index`,
  `EnvironmentFile=-/etc/default/feed-index`, `Wants=`/`After=network-online.target`, hardening
  (`NoNewPrivileges=yes`, `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes`,
  `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX`). Output goes to the journal.
- `usr/lib/systemd/system/feed-index-fetch.timer`: `OnCalendar=*:0/5`, `Persistent=true`,
  `RandomizedDelaySec=30`. Every 5 minutes is right even though most feeds poll hourly: each run
  fetches only what is due, so fast feeds (`poll_minutes="10"`) and slow ones share one timer.
- `/usr/share/doc/feed-index/`: `config.example.toml`, `feeds.example.opml`, `default.example` (the
  `PG*` variables, `PGSSLMODE=require`, `PGPASSFILE=…`) and `README.deploy.md`: install with
  `dpkg -i`; copy the examples into place; `probe --opml` the feed list; `migrate`, `sync`, then one
  `fetch` by hand; `status`; **then** enable the timer; how to run as another user with a drop-in;
  and that **`systemctl disable --now` the timer before an upgrade** if it must stay paused, because
  postinst restarts an enabled timer.
- `postinst`/`prerm`: exactly the maildir-index pattern (system user/group `feed-index` created only
  if absent; never modify an existing user; restart the timer after upgrade only if it is enabled;
  prerm stops timer and service).
- `depends = "$auto, adduser, systemd"`. Maintainer placeholder as in maildir-index.

## Acceptance (what "done" means for this session)

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (with a local
  `DATABASE_URL`) all green.
- All 22 test cases above exist and pass.
- `cargo deb` builds; `dpkg-deb -c target/debian/*.deb` lists exactly the files above.
- `README.md` updated: status, the five commands, configuration (libpq environment + TOML + OPML).
- A short summary for the owner: what was built, any deviation from this brief and why, any schema
  concern, how missing ids were handled in `feed-rs`, and anything you could not verify.

**Out of scope:** any web page or API, reading state, model-written summaries, the real feed list,
and any deployment to a real host.
