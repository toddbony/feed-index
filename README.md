# feed-index

Polls RSS and Atom feeds listed in an OPML file and indexes their entries into PostgreSQL, so
what was published stays queryable after the feed has rotated it out.

Sibling of [`maildir-index`](https://github.com/toddbony/maildir-index): same database
conventions (libpq environment, password only via `PGPASSFILE`, sqlx migrations applied by the
program), same packaging (a `.deb` with a oneshot service and a timer that it never enables).

## Status

**Fetcher implemented (0.1.0).** All five commands work; schema v1
(`migrations/0001_init.sql`, in the Postgres schema `feeds`) is unchanged from the scaffold and
has not been applied to a production database yet. Reads RSS 0.9x/1.0/2.0, Atom and JSON Feed
(via `feed-rs`).

## Design in one paragraph

A timer runs `feed-index fetch` every five minutes. Each run syncs the OPML file into
`feeds.feeds`, fetches only the feeds that are due (each feed has its own interval, so a
breaking-news feed every 10 minutes and a weekly newsletter hourly share one timer), uses
conditional GET, backs off failing feeds, and stores each entry once, keyed by its id (or a
fingerprint of link and title when it has none). Edits update the entry and count a revision. A
feed's first fetch is marked as back catalogue. See `docs/schema.md` for the reasoning.

Everything, including sqlx's migration record, lives in the schema `feeds`, so the program can
share a database with other sqlx-migrated programs. (With a shared `public._sqlx_migrations`, two
programs that each have a migration version 1 break each other; see `docs/schema.md`.)

## Commands

```
feed-index migrate                       apply the embedded migrations and exit
feed-index sync                          migrate, then sync the OPML into feeds.feeds;
                                         prints added/updated/retired/unchanged
feed-index fetch [--feed ID]... [--force]
                                         migrate, sync, and fetch the feeds that are due
feed-index status [--quiet-days N]       one line per active feed; marks failing feeds and
                                         feeds with no new entry for N days (default 14)
feed-index probe URL... | --opml FILE [--show]
                                         no database: fetch and parse as `fetch` would and
                                         print what was found (--show: first 3 entries)
```

All take `--config FILE` (default `/etc/feed-index/config.toml`); `probe` uses the defaults
when that file does not exist.

`fetch` prints one line per run, plus one per failed feed:

```
feed-index run=812 due=9 ok=7 not_modified=1 failed=1 new_entries=14 changed_entries=2 retired=0 duration_s=6
feed-index feed_id=12 host=news.example.org outcome=http_error status=404
```

It exits non-zero only for run-level failures (configuration, OPML, retire guard, database).
A failing feed is recorded on the feed and in `feeds.fetches`, backed off
(`poll_minutes × 2^(failures−1)`, at most 24 h, at least any `Retry-After` on 429/503), and shown
by `status`; it does not fail the run. Only one run at a time: a run that finds another one
holding the lock logs one line and exits 0.

## Configuration

**Database:** the standard libpq environment only: `PGHOST`, `PGPORT`, `PGDATABASE`, `PGUSER`,
`PGSSLMODE`, and the password **only** through `PGPASSFILE` (pgpass format, mode 0600).
`PGPASSWORD` is refused, and so is an invalid `PGSSLMODE`; `~/.pgpass` is never read. With
`PGSSLMODE=require` a server without TLS is an error, never a silent downgrade.

**Everything else:** one TOML file (see [`example.toml`](example.toml); unknown keys are an
error):

```toml
opml = "/etc/feed-index/feeds.opml"      # the feed list
contact = "https://example.org/me"       # User-Agent: feed-index/<version> (+<contact>)
default_poll_minutes = 60                # 5..=1440
concurrency = 4                          # requests in flight, 1..=16
max_feed_bytes = 10_485_760              # larger bodies are abandoned (too_large)
timeout_seconds = 30                     # whole exchange, redirects and body included
fetch_history_days = 90                  # feeds.fetches retention
```

**Feed list:** OPML 2.0 (see [`example.opml`](example.opml)), as exported by most feed
readers. Every `<outline>` with an `xmlUrl` is a feed, at any depth; the nearest enclosing
outline without `xmlUrl` is its folder. Optional attributes: `poll_minutes` (5..=1440) and
`weight` (integer, stored for whatever ranks entries later). Only `http`/`https` URLs. A
duplicate `xmlUrl`, a bad attribute or a non-http(s) URL stops the run before anything changes.
A feed removed from the file is retired (its entries stay); putting it back reactivates it. If
the file holds no feeds, or fewer than half of the active ones, nothing is retired and the run
fails (the retire guard).

## Privacy and untrusted input

Logs carry feed ids, hosts, HTTP status codes, outcomes, counts and durations, never entry
titles, links or content, and never full feed URLs (a test fetches distinctive fixtures with
`RUST_LOG=trace` and checks). `status` and `probe` print to stdout for a human, so they do show
titles and URLs. Feed documents are untrusted: bodies are capped (after decompression),
redirects are limited to five and to http(s), the parser and the HTML renderer run under
`catch_unwind`, NUL bytes are stripped, and the stored HTML (`summary_html`, `content_html`)
must be sanitised by whatever displays it.

## Building, testing, packaging

```
cargo build --release
DATABASE_URL=postgres:///feed_index_test?host=/var/run/postgresql cargo test
cargo deb            # target/debian/feed-index_*.deb
```

Database tests create and drop their own throwaway database on the server named by
`DATABASE_URL` (the database in the URL must exist and the role needs `CREATEDB`); use a
local, disposable server. Without `DATABASE_URL` they are skipped with a message. HTTP tests
use a local server on 127.0.0.1; no test touches the internet. The Debian package installs the
binary, a oneshot service and a 5-minute timer, and enables nothing; see
[`debian/README.deploy.md`](debian/README.deploy.md).

## Licence

Dual-licensed under MIT or Apache-2.0, at your option.
