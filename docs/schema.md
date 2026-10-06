# Schema v1: design notes

## One schema, including the migration record

Everything lives in the Postgres schema `feeds`, and sqlx's migration record is
`feeds._sqlx_migrations`, not the default `public._sqlx_migrations`.

This is not tidiness. The intended deployment shares a database with
[`maildir-index`](https://github.com/toddbony/maildir-index), which also migrates with sqlx and
also has a migration version 1. sqlx validates *every* row in its migration table against the
migrations it knows. With one shared table:

- this program fails at startup with `VersionMismatch(1)` (its version 1 has a different checksum);
- and once it has written a row there, the other program's next run fails with `VersionMissing`.

Tested on Postgres 16 with both programs' real migrations: shared table → `VersionMismatch(1)`;
separate tables → both migrate cleanly, in either order, repeatedly. So the migrator is built with
`dangerous_set_table_name("feeds._sqlx_migrations")` and `create_schema("feeds")`, and every query
names `feeds.` explicitly: nothing depends on `search_path`.

## A feed is its configured URL

`feeds.xml_url` is the URL as written in the OPML file, and it is what is requested every time.
Redirects are followed, and where the request ended up is recorded (`resolved_url`), but the
configured URL is never rewritten by the program: a publisher's redirect is evidence for the
owner to act on, not an instruction.

Feed rows are configuration, synced from the OPML file on every run. A feed that disappears from
the file is **retired** (`retired_at`), never deleted, because its entries are the only lasting
copy of what it published. Putting it back clears `retired_at`.

## An entry is (feed, key)

Feeds identify entries with an RSS `guid` or Atom `id`, but not reliably: some feeds omit it. So
`entry_key` is the id as written (trimmed) when present, else `fp:` + SHA-256 of the normalised
link and title. The fingerprint is only a fallback; a feed without ids that changes a title will
produce a second entry, and that is accepted.

The key is unique **per feed**, not globally: the same post reached through two feeds is two rows,
joined by `link` when it matters (the same rule `maildir-index` uses for the same email in two
accounts).

## Keep what the feed said; derive the rest

`summary_html` and `content_html` are stored exactly as the feed gave them, because feeds drop old
entries and publishers edit them; there is no going back for it later. They are untrusted HTML and
must be sanitised before display.

`body_text` is derived (content, else summary, through `html2text` and the same normalisation as
`maildir-index`) for search and for blurbs. `body_source` and `fetcher_version` make it re-derivable.

## Edits and back catalogues

`content_sha256` covers title, link, summary and content. When it changes, the row is updated in
place, `revisions` goes up and `last_changed_at` is set. Earlier versions are not kept: these are
public posts, and the publisher's current text is what matters.

A feed's first successful fetch returns its back catalogue (often 10–50 entries, some years old).
Those rows get `backfill = true` so a "what's new" view can leave them out.

## Fetch state lives on the feed

`etag` / `last_modified` (conditional GET), `next_fetch_at` (poll interval, failure backoff or
`Retry-After`), `consecutive_failures` and `last_error` are columns on `feeds.feeds`, because the
fetcher needs them on every run. `feeds.fetches` is the history of attempts, for diagnosing a feed
after the fact, and is pruned after a retention period.

`last_new_entry_at` exists because feeds fail **silently**: a moved feed often keeps answering
`200` with a stale document, or `304` forever. "OK, but nothing new for far longer than usual" is
the signal, and it is for whoever reads the data (a brief, a status page) to raise.

## Not in this schema

Reading state (read / saved / dismissed) belongs to whatever presents the entries and is not
rebuildable from the feeds, so it will get its own table, owned by that program, in a later
migration. The fetcher never touches it.
