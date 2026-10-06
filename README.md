# feed-index

Polls RSS and Atom feeds listed in an OPML file and indexes their entries into PostgreSQL, so
what was published stays queryable after the feed has rotated it out.

Sibling of [`maildir-index`](https://github.com/toddbony/maildir-index): same database
conventions (libpq environment, password only via `PGPASSFILE`, sqlx migrations applied by the
program), same packaging (a `.deb` with a oneshot service and a timer that it never enables).

## Status

**Scaffold.** The schema (`migrations/0001_init.sql`, in the Postgres schema `feeds`) and the build
brief (`CLAUDE.md`) exist; the fetcher does not yet. The schema has not been applied anywhere.

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

## Licence

Dual-licensed under MIT or Apache-2.0, at your option.
