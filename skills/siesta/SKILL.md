---
name: siesta
description: Run SQL against Postgres and MySQL / MariaDB and Elasticsearch requests with the Siesta CLI and plain `.sql` / `.es` files, inspect a database schema (tables, columns, indexes, DDL; Elasticsearch indices and mappings) before writing queries, and manage connections in `~/.config/siesta/connections/`. Use when the user asks to query a database or an Elasticsearch cluster, check data, explore tables or indices, run or fix a `.sql` / `.es` file, or set up a connection, and `siesta` is installed or the project has `.sql` / `.es` files.
---

# Siesta: SQL files and a database CLI

`siesta` is one binary: the desktop app with no arguments (or a path), a CLI with a subcommand.
From an agent, use the CLI only; never launch the GUI.

## Rules for agents

- Read the schema before writing SQL: `siesta schema -c NAME` lists tables, `siesta schema -c NAME TABLE`
  gives the columns, types, primary key and indexes. Do not guess column names.
- Pass `--output json` (or `jsonl`) when you read the result; without a TTY JSON is the default.
- Prefer a read-only connection. Add `--read-only` to anything that must not change data: the
  client refuses writes before sending (and the server session is read-only). Use it for every exploratory query. It is a safety default, not a security
  boundary; a read-only database user is the real protection.
- Never pass `--yes` on your own. A refused statement (exit `4`) is a question for the user: show them
  the statement and the connection, and run it with `--yes` only after they agree. On a `prod`
  connection `--yes` does not work without a terminal (exit `4`, `prod_needs_terminal`): the user runs it
  themselves in a terminal.
- Never write passwords into a connection file; they live only encrypted (`password_enc`, local key in
  `~/.siesta/key`; `siesta connections set-password NAME`, done by the user). Never read or print the key. `siesta connections` shows connections
  with secrets masked and whether a password is set.
- Results are capped at 100 rows by default; check `rows_truncated`. Narrow the query
  (`WHERE`, `LIMIT`, `count(*)`, aggregates) instead of raising `--max-rows` for a big table.

## Commands

Statements are addressed by `@name`, `:LINE` (any line of the statement) or `#N` (1-based).

| Command | What it does |
|---|---|
| `siesta list [PATH]` | Statements in a `.sql` / `.es` file or directory: file, line, name, first line, kind |
| `siesta run FILE [STATEMENTS]...` | Run statements of a file (none named = all, in file order) |
| `siesta exec [SQL]` | Run SQL from the argument, or from stdin when omitted or `-`; may hold several statements |
| `siesta connections` | Connections of `~/.config/siesta/connections/`, secrets masked, with `password: set` / `none` |
| `siesta connections list` | The same as `siesta connections` |
| `siesta connections test NAME` | Open a connection and report the server version |
| `siesta connections databases NAME` | Databases on the server of a connection that its credentials can access (`name`); with `-d` / `-- @database` you can run in any of them |
| `siesta connections set-password NAME` | Store the password of a connection, encrypted, in its file (hidden prompt, or one line from stdin); the user does this, not you |
| `siesta schema [TABLE]` | Every table and view, or one table (`table` or `schema.table`) with columns and indexes |
| `siesta mcp` | MCP server on stdio (see below) |

`list`, `run`, `exec`, `connections` and `schema` take `--output table|json|jsonl|csv`
(`table` in a terminal, `json` in a pipe).

`run` and `exec` options:

- `-c`, `--connection NAME` — the connection. Without it `run` uses the file's `-- @connection`;
  no connection at all is an error (exit `3`). `exec` and `schema` need `-c`.
- `-d`, `--database NAME` — the database on that server. Precedence: the flag, then the file's
  `-- @database` (`run` only), then the connection's `database` (the default; none = the server's default).
  Run `siesta connections databases NAME` to see what is available.
- `--max-rows N` — rows per statement (default 100, `0` = the `results.max_rows` setting, which is also the upper limit).
- `--read-only` — force the connection read-only.
- `--yes` — skip the confirmations (`-- @confirm`, destructive statements under the setting; on a `prod` connection only in a terminal, see Safety).

`schema` options:

- `-c`, `--connection NAME` — required.
- `-d`, `--database NAME` — read the structure of that database instead of the connection's default.
- `--ddl` — print the `CREATE TABLE` / `CREATE VIEW` text of the table (MySQL: `SHOW CREATE TABLE`;
  Postgres: assembled from the catalog, without storage options and ownership).

Exit codes:

| Code | Meaning |
|---|---|
| `0` | OK |
| `2` | Connection error: cannot connect, authentication failed, connection lost |
| `3` | Parse error (unclosed string / comment), unknown connection, statement or table, no connection chosen, a connection of the other kind (`connection_mismatch`: `.es` on SQL, `.sql` on Elasticsearch), bad arguments |
| `4` | Refused: confirmation needed and `--yes` not given, or a `read_only` connection was asked to run a statement that writes (or switch to read-write) |
| `5` | SQL error from the server |
| `130` | A second SIGINT / SIGTERM while the first one's cancel was still waiting for the server |

With several statements the first failure stops the run (setting `run.stop_on_error`); the exit code is
that of the failure. With `run.stop_on_error` off the run goes on and the exit code is the first
failure's code.

SIGINT (<kbd>Ctrl</kbd> <kbd>C</kbd>) or SIGTERM while a statement runs cancels it on the server
(Postgres `57014`, MySQL / MariaDB `70100`, exit `5`) and stops the run: later statements do not run.

### MCP server

`siesta mcp` serves the commands as MCP tools over stdio, for clients without a shell:
`connections`, `schema` (`table`, `connection`, `database`, `ddl`), `list_statements` (`path`) and
`run_sql` — `sql` (as `exec`) or `file` with `statements` (as `run`), plus `connection`, `database`,
`max_rows`. Each tool returns the command's JSON output; a non-zero exit code sets `isError` and is
appended to the text. A cancelled tool call (the client's `notifications/cancelled`, e.g. <kbd>Esc</kbd>
in Claude Code) cancels its statement on the server and gets no reply. `run_sql` never passes `--yes` and cannot answer a prompt. Refused with `4`
before anything runs — ask the user to run these:

- writes on a `read_only` connection (`read_only_refused`);
- on a `prod` connection, everything except reads and `UPDATE` with a top-level `WHERE`
  (`prod_refused`: `DELETE`, `INSERT`, `DROP`, `TRUNCATE`, `ALTER`, `WITH … UPDATE`, `SET`…);
  `UPDATE … WHERE` runs there without confirmation;
- statements that need confirmation (`confirmation_required`: `-- @confirm`, and destructive ones
  under `run.confirm_destructive = always`, see Safety). Relative paths resolve against the
directory the client starts the server in. Register it, for example in Claude Code (`siesta` must be on `PATH`: `cargo install --path crates/app`):
`claude mcp add -s user siesta -- siesta mcp`, then check `claude mcp get siesta` says Connected. The tools
appear in a new session.

## JSON output

`run` and `exec` with `--output json` print one object for one statement, an array for several.
`--output jsonl` prints one object per line, as each statement finishes. Shape (`siesta.run/v1`):

```json
{
  "schema": "siesta.run/v1",
  "file": "reports.sql",
  "statement": { "name": "users_by_day", "line": 12 },
  "connection": "dev-pg",
  "database": "app",
  "columns": [ { "name": "id", "type": "int8" }, { "name": "payload", "type": "jsonb" } ],
  "rows": [ [1, { "ip": "10.0.0.1" }] ],
  "rows_truncated": false,
  "rows_affected": null,
  "duration_ms": 38,
  "notices": []
}
```

- `rows` is an array of arrays in column order. `NULL` is `null`; JSON / JSONB are nested values,
  not strings; numeric and date/time values are strings (exact); bytes are a hex string.
- `database` is the effective database (flag, directive or the connection's `database`); `null` when none of
  them is set, i.e. the server's default.
- `rows_truncated` is true when `--max-rows` cut the result.
- `rows_affected` is a number for INSERT / UPDATE / DELETE / DDL, `null` for queries.
- `file` is `null` for `exec`; `line` counts from the start of the SQL given.
- A failed statement has `"error": { "code": "sql_error" | "connection_lost" | "read_only_refused", "sqlstate", "message", "detail", "hint", "position" }`
  instead of the result fields, and the exit code is `5` (`2` for a lost connection, `4` for `read_only_refused`).
- `notices` carries server warnings and siesta's notes, e.g. `2 more result sets not shown`: only the first
  result set of a statement that returns several (`CALL p()`) is printed.
- A server that does not answer within `run.connect_timeout_ms` (default 10 s) fails with exit `2` and
  "No response from <host>:<port> within … s. Check the driver and port …": usually a wrong `driver`
  or port in the connection.
- Errors before any statement runs (exit `2`, `3`, `4`) print `{"error": {"code", "message", "file", "line", "hint"}}`.
  Read `hint`: it names the fix, for example the available connections.

`schema --output json`: `{"schema": "siesta.schema/v1", "connection", "schemas": [{"name", "tables": [{"name", "kind": "table"|"view", "columns": [{"name", "data_type", "nullable", "primary_key", "ordinal"}], "indexes": [{"name", "columns", "unique", "primary"}]}]}]}`.
With a table: `{"schema": "siesta.schema/v1", "connection", "table": {"schema", "name", "kind", "columns", "indexes", "ddl"?}}`.

`csv` prints the header and rows of each result (NULL as an empty field), without summaries.

## File syntax

Plain SQL that also runs in `psql` / `mysql`. `;` ends a statement (`$$ … $$` bodies and, for MySQL,
`DELIMITER` blocks are handled). Directives are comments before a statement:

```sql
-- @connection dev-pg

-- @name users_by_day
SELECT date_trunc('day', created_at) AS day, count(*) FROM users GROUP BY 1;

-- @name purge_sessions
-- @confirm
DELETE FROM sessions WHERE expires_at < now();
```

| Directive | Effect |
|---|---|
| `-- @connection NAME` | Default connection of the file, at the top before the first statement; `-c` overrides it |
| `-- @database NAME` | Database for the file, at the top before the first statement; overrides the connection's `database`, `-d` overrides it |
| `-- @name ID` | Name the statement for addressing: `siesta run FILE ID` |
| `-- @page-size N` | Page size in the GUI; the CLI uses `--max-rows` instead |
| `-- @confirm` | Always ask before running; the CLI refuses with exit `4` unless `--yes` |

## Elasticsearch (`.es` files)

An `.es` file is Kibana Dev Tools console text: a `METHOD /path` line starts a request, the lines up to the
next request line are its JSON body (NDJSON for `_bulk` / `_msearch`; Kibana's `"""…"""` strings work).
Comments are `#` or `//`; directives are the same words with `#`: `# @connection`, `# @name`,
`# @page-size`, `# @confirm` (`@database` does not apply). A file pastes into Kibana unchanged.

```
# @connection es-dev

# @name adults
GET /users/_search
{ "query": { "range": { "age": { "gte": 18 } } } }

POST /_sql?format=json
{ "query": "SELECT name, age FROM users ORDER BY age DESC" }
```

- Results are tables: `_search` hits as rows (`_id`, `_index` when several indices, then the `_source`
  fields; nested objects stay JSON), `_sql` columns / rows, `_cat/*` lists (Siesta adds `format=json`);
  any other response is one `response` cell with the whole JSON. `HEAD` gives the `status`.
- `--max-rows` pages a `_search` with `from` / `size` unless its body sets `size` or `from` itself
  (aggregations: `"size": 0`, then the response cell holds `aggregations`). `notices` gives the hit total.
- `exec -c ES_CONNECTION 'GET /_cat/indices'` runs requests too. `schema -c ES_CONNECTION` lists indices as
  tables with their mapping fields (dotted paths) as columns; `--ddl` prints `PUT /index` with the mapping.
- Errors: exit `5` with `sqlstate` = Elasticsearch's error type (`index_not_found_exception`) and its `reason`.
- Kinds: `_search` is a query; `GET`, `HEAD` and reading `POST`s (`_count`, `_msearch`, `_mget`, `_sql`,
  `_field_caps`, `_validate`, `_explain`, `_analyze`, …) are reads; everything else writes.
  `read_only` refuses writes on the client only (Elasticsearch has no read-only session).
  On `prod`, an agent may run reads and `POST <index>/_update/<id>`; nothing else (`prod_refused`).
  `run.confirm_destructive = always` asks for deleting an index, `_delete_by_query`, `_update_by_query`
  without a query and `_close`.
- Ctrl+C drops the request; Elasticsearch may still finish it (bounded by `run.statement_timeout_ms`).

## Connections

One JSON file per connection in the user's config dir, `~/.config/siesta/connections/NAME.json`
(the file name is the connection name; the desktop app's "New connection" form writes it). There are no
project-level connection files.

A connection name is `[a-z0-9][a-z0-9_-]*`, at most 64 characters (lowercase ASCII): it is the file name,
the value of `-- @connection NAME` and the CLI argument. Old connections named otherwise (`My DB.json`) are
renamed automatically (to `my-db`; the password is re-encrypted and `-- @connection` lines in the
collections and added files are rewritten) the first time the app or the CLI runs; the CLI reports it
on stderr, so JSON on stdout stays clean.

```jsonc
// ~/.config/siesta/connections/dev-pg.json
{ "driver": "postgres", "host": "localhost", "port": 5432, "database": "app", "user": "app" }
// ~/.config/siesta/connections/prod-mysql.json
{ "driver": "mysql", "url": "mysql://ro@db.internal:3306/shop", "read_only": true, "prod": true }
// ~/.config/siesta/connections/es-dev.json
{ "driver": "elasticsearch", "url": "https://es.internal:9200", "user": "elastic" }
```

- Elasticsearch: `driver` `elasticsearch` (or an `http://` / `https://` url), `host` / `port` (9200);
  `https` when `ssl.mode` is set and not `disable` (`require` skips the certificate check, `ssl.ca` adds a CA).
  `user` + password is basic auth; a password without `user` is an API key. No `database`, `socket`,
  `schema` or `init_sql`.
- Keys: `driver` (`postgres` | `mysql` | `elasticsearch`; MariaDB is `mysql`), or `url`, or `host` / `socket` / `port` / `database`
  (the default database, optional: a file or `-d` can pick any other the credentials can access) / `user`; `ssl` (`mode`, `ca`, `cert`, `key`), `read_only`, `prod` (production: every
  non-read statement needs confirmation in a terminal), `schema` (Postgres `search_path`), `init_sql`, `color`.
  `socket`: `true` connects through the local Unix socket found at connect time (Postgres: a folder with `.s.PGSQL.<port>` among `/tmp`,
  `/var/run/postgresql`, Homebrew's; MySQL: the usual `mysql.sock` / `mysqld.sock` files), or a path (Postgres: the folder, MySQL: the file); `host` is then ignored.
  A `host` that starts with `/` still works as a socket path. `siesta connections` shows `socket`.
  Postgres without `user` connects as the OS user.
  There is no `password` key.
- `${env:NAME}` reads the environment.
- The password lives only as `password_enc` in the connection file, encrypted with the key in
  `~/.siesta/key` and decrypted when a session opens; the user sets it in the connection form or
  with `siesta connections set-password NAME`. Without `password_enc` Siesta connects without a password;
  if the server then rejects the login, the error says to set one (exit `2`): ask the user to run
  `set-password`; never type a password yourself. "Can't decrypt the password of `X`" (exit `2`) means the
  key is missing or different: the user sets the password again.
- Check with `siesta connections`, then `siesta connections test NAME`.

### Folders

- `~/.config/siesta/` — only what the user writes: `settings.json`, `keymap.json`, `connections/`,
  `collections/` (the user's `.sql` files, with subfolders). Fine for a dotfiles repo. Edit connection files here (never a password).
- `~/.siesta/` — only what Siesta writes: `state.json`, `files.json`, `history/`, `cache/`, the password `key`. Every statement you run (CLI or MCP) is recorded in `history/` with source `cli` / `mcp`, visible to the user in the app's History (⌘Y).
  Never read the key, never edit these files, and keep this folder out of dotfiles.
- `SIESTA_CONFIG_DIR` and `SIESTA_DATA_DIR` replace them.

## Safety

- On a connection with `"prod": true` every statement that is not a plain read (DML, DDL, `SELECT … INTO`, `FOR UPDATE`, `SET`, `BEGIN`, `CALL`, …) needs confirmation; `SELECT` / `SHOW` / `DESCRIBE` / `EXPLAIN` run without it. Elsewhere a
  statement needs it when it has `-- @confirm`, or, with the setting `run.confirm_destructive` = `always`,
  when it is `UPDATE` / `DELETE` without `WHERE`, `DROP`, `TRUNCATE` or `ALTER`.
- Without a terminal the CLI never asks: it exits `4` (error code `confirmation_required`) before running
  anything, so a batch is never half-run. In a terminal it asks `[y/N]`. `--yes` skips the question, but not for a non-read statement on a `prod` connection without a terminal: that is refused with `prod_needs_terminal` (exit `4`), so you cannot unlock production with a flag; the user runs it in a terminal.
- `siesta connections` shows the `prod` flag of every connection. For a prod connection prefer `--read-only`
  and ask the user before every run.
- `--read-only` and `"read_only": true` set a read-only session on the server, plus a client-side check by
  statement text: anything that writes (INSERT, UPDATE, DELETE, DDL, `SELECT … INTO`, `FOR UPDATE`,
  data-modifying `WITH`, `EXPLAIN ANALYZE <DML>`, CALL, COPY, PREPARE, unknown statements, …) and every
  switch to read-write (`BEGIN READ WRITE`, `SET default_transaction_read_only`, `RESET ALL`, …) is refused
  before it is sent: exit `4`, error code `read_only_refused`. The whole batch is checked before anything
  runs. Reads (SELECT, SHOW, DESCRIBE, EXPLAIN) and plain `SET` / transaction control pass. A function that
  writes inside a SELECT is not seen by the text check; the server's read-only session stops it. It is
  **not a security boundary**: for real protection connect with a read-only database user.

## Workflow for a query

1. `siesta connections` — pick the connection; `siesta connections test NAME` if unsure.
2. `siesta schema -c NAME` and `siesta schema -c NAME TABLE` — learn the tables and columns.
3. `siesta exec -c NAME --read-only --output json 'SELECT …'` — try the query.
4. Read `rows`, `rows_truncated`, `error`. On an error, fix the SQL from `message` and `position`.
5. To keep a query, write it to a `.sql` file with `-- @name` and run it with `siesta run FILE NAME`.
6. Changing data: show the statement and the connection to the user first; exit `4` means ask them.
