---
name: siesta
description: Run SQL against Postgres and MySQL / MariaDB with the Siesta CLI and plain `.sql` files, inspect a database schema (tables, columns, indexes, DDL) before writing queries, and manage connections in `siesta.json` / `siesta.private.json`. Use when the user asks to query a database, check data, explore tables, run or fix a `.sql` file, or set up a database connection, and `siesta` is installed or the project has `.sql` files or a `siesta.json`.
---

# Siesta: SQL files and a database CLI

`siesta` is one binary: the desktop app with no arguments (or a path), a CLI with a subcommand.
From an agent, use the CLI only; never launch the GUI.

## Rules for agents

- Read the schema before writing SQL: `siesta schema -c NAME` lists tables, `siesta schema -c NAME TABLE`
  gives the columns, types, primary key and indexes. Do not guess column names.
- Pass `--output json` (or `jsonl`) when you read the result; without a TTY JSON is the default.
- Prefer a read-only connection. Add `--read-only` to anything that must not change data: the
  server rejects writes. Use it for every exploratory query. It is a safety default, not a security
  boundary; a read-only database user is the real protection.
- Never pass `--yes` on your own. A refused statement (exit `4`) is a question for the user: show them
  the statement and the connection, and run it with `--yes` only after they agree.
- Never write passwords into `siesta.json`; they belong in `siesta.private.json` (git-ignored) or
  `${env:NAME}`. `siesta connections` shows connections with secrets masked.
- Results are capped at 100 rows by default; check `rows_truncated`. Narrow the query
  (`WHERE`, `LIMIT`, `count(*)`, aggregates) instead of raising `--max-rows` for a big table.

## Commands

Statements are addressed by `@name`, `:LINE` (any line of the statement) or `#N` (1-based).

| Command | What it does |
|---|---|
| `siesta list [PATH]` | Statements in a `.sql` file or directory: file, line, name, first line, kind |
| `siesta run FILE [STATEMENTS]...` | Run statements of a file (none named = all, in file order) |
| `siesta exec [SQL]` | Run SQL from the argument, or from stdin when omitted or `-`; may hold several statements |
| `siesta connections` | Connections visible from the current directory, secrets masked |
| `siesta connections list` | The same as `siesta connections` |
| `siesta connections test NAME` | Open a connection and report the server version |
| `siesta schema [TABLE]` | Every table and view, or one table (`table` or `schema.table`) with columns and indexes |

`list`, `run`, `exec`, `connections` and `schema` take `--output table|json|jsonl|csv`
(`table` in a terminal, `json` in a pipe).

`--connections-file PATH` adds a connection file above the `siesta*.json` chain (repeatable);
`list`, `run`, `exec`, `connections` and `schema` accept it. `connections`, `exec` and `schema` look
for `siesta.json` from the current directory up to the project root (the nearest folder with
`.git` or `.siesta`), so run them from inside the project.

`run` and `exec` options:

- `-c`, `--connection NAME` — the connection. Without it `run` uses the file's `-- @connection`;
  no connection at all is an error (exit `3`). `exec` and `schema` need `-c`.
- `--max-rows N` — rows per statement (default 100, `0` = the `results.max_rows` setting, which is also the upper limit).
- `--read-only` — force the connection read-only.
- `--yes` — skip the confirmations (every statement on a `prod` connection, `-- @confirm`; see Safety).

`schema` options:

- `-c`, `--connection NAME` — required.
- `--ddl` — print the `CREATE TABLE` / `CREATE VIEW` text of the table (MySQL: `SHOW CREATE TABLE`;
  Postgres: assembled from the catalog, without storage options and ownership).

Exit codes:

| Code | Meaning |
|---|---|
| `0` | OK |
| `2` | Connection error: cannot connect, authentication failed, connection lost |
| `3` | Parse error (unclosed string / comment), unknown connection, statement or table, no connection chosen, bad arguments |
| `4` | Refused: confirmation needed and `--yes` not given, or a `read_only` connection was asked to switch to read-write |
| `5` | SQL error from the server |

With several statements the first failure stops the run (setting `run.stop_on_error`); the exit code is
that of the failure. With `run.stop_on_error` off the run goes on and the exit code is the first
failure's code.

## JSON output

`run` and `exec` with `--output json` print one object for one statement, an array for several.
`--output jsonl` prints one object per line, as each statement finishes. Shape (`siesta.run/v1`):

```json
{
  "schema": "siesta.run/v1",
  "file": "reports.sql",
  "statement": { "name": "users_by_day", "line": 12 },
  "connection": "dev-pg",
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
- `rows_truncated` is true when `--max-rows` cut the result.
- `rows_affected` is a number for INSERT / UPDATE / DELETE / DDL, `null` for queries.
- `file` is `null` for `exec`; `line` counts from the start of the SQL given.
- A failed statement has `"error": { "code": "sql_error" | "connection_lost" | "read_only_refused", "sqlstate", "message", "detail", "hint", "position" }`
  instead of the result fields, and the exit code is `5` (`2` for a lost connection, `4` for `read_only_refused`).
- `notices` carries server warnings and siesta's notes, e.g. `2 more result sets not shown`: only the first
  result set of a statement that returns several (`CALL p()`) is printed.
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
| `-- @name ID` | Name the statement for addressing: `siesta run FILE ID` |
| `-- @page-size N` | Page size in the GUI; the CLI uses `--max-rows` instead |
| `-- @confirm` | Always ask before running; the CLI refuses with exit `4` unless `--yes` |

## Connections

Plain JSON, committed without secrets, with secrets in a private file next to it:

```jsonc
// siesta.json
{ "dev-pg": { "driver": "postgres", "host": "localhost", "port": 5432, "database": "app", "user": "app" },
  "prod-mysql": { "driver": "mysql", "url": "mysql://ro@db.internal:3306/shop", "read_only": true } }
// siesta.private.json (git-ignored)
{ "dev-pg": { "password": "dev" }, "prod-mysql": { "password": "${env:SHOP_DB_PASSWORD}" } }
```

- Keys: `driver` (`postgres` | `mysql`; MariaDB is `mysql`), or `url`, or `host` / `port` / `database` /
  `user` / `password`; `ssl` (`mode`, `ca`, `cert`, `key`), `read_only`, `prod` (production: every
  statement needs `--yes`), `schema` (Postgres `search_path`), `init_sql`, `color`.
- `${env:NAME}` reads the environment, `${dotenv:NAME}` the `.env` next to the file.
- Files are found in every folder from the project root down to the `.sql` file; deeper folders and
  the private file win. The user's global connections (`~/.config/siesta/connections.json` and
  `connections.private.json`, written by the desktop app's "New connection" form) are read first, so a
  project's `siesta.json` overrides a global connection of the same name.
- Check with `siesta connections`, then `siesta connections test NAME`.

## Safety

- On a connection with `"prod": true` EVERY statement needs confirmation, even a `SELECT`. Elsewhere a
  statement needs it when it has `-- @confirm`, or, with the setting `run.confirm_destructive` = `always`,
  when it is `UPDATE` / `DELETE` without `WHERE`, `DROP`, `TRUNCATE` or `ALTER`.
- Without a terminal the CLI never asks: it exits `4` (error code `confirmation_required`) before running
  anything, so a batch is never half-run. In a terminal it asks `[y/N]`. `--yes` skips the question.
- `siesta connections` shows the `prod` flag of every connection. For a prod connection prefer `--read-only`
  and ask the user before every run.
- `--read-only` and `"read_only": true` are a safety default of the session, enforced by the server
  (not by parsing the SQL), plus a cheap guard: statements that switch the session to read-write
  (`BEGIN READ WRITE`, `SET TRANSACTION READ WRITE`, `SET default_transaction_read_only`, `RESET ALL`, …)
  are refused with exit `4` and error code `read_only_refused`. It is **not a security boundary**: for real
  protection connect with a read-only database user.

## Workflow for a query

1. `siesta connections` — pick the connection; `siesta connections test NAME` if unsure.
2. `siesta schema -c NAME` and `siesta schema -c NAME TABLE` — learn the tables and columns.
3. `siesta exec -c NAME --read-only --output json 'SELECT …'` — try the query.
4. Read `rows`, `rows_truncated`, `error`. On an error, fix the SQL from `message` and `position`.
5. To keep a query, write it to a `.sql` file with `-- @name` and run it with `siesta run FILE NAME`.
6. Changing data: show the statement and the connection to the user first; exit `4` means ask them.
