# Siesta

**A lightweight, text-first SQL client for Postgres and MySQL / MariaDB.** Queries are plain `.sql` files that also run in `psql` and `mysql`.

Built in Rust on [GPUI](https://www.gpui.rs); a sibling of [Respite](../respite), the REST client. The GUI and the CLI share one core. The full design is in [the specification](<Siesta специфікація легкого SQL-клієнта на Rust + GPUI.md>).

<!-- TODO: screenshot -->

## Connections

Connections are plain JSON: `siesta.json` (committed, no secrets) and `siesta.private.json` (git-ignored passwords). Both are found in every folder from the project root down to the `.sql` file.

```json
{ "dev-pg": { "driver": "postgres", "host": "localhost", "port": 5432, "database": "app", "user": "app" },
  "prod-mysql": { "driver": "mysql", "url": "mysql://ro@db.internal:3306/shop", "read_only": true } }
```

A `.sql` file picks its connection with `-- @connection dev-pg` on the first line. Values like `${env:SHOP_DB_PASSWORD}` are read from the environment.

## CLI

The desktop app and the CLI ship as one binary.

```text
siesta list          Statements of a file or directory: file, line, name, first line
siesta run           Run statements of a file (all of them in file order when none are named)
siesta exec          Run SQL from an argument or stdin
siesta connections   Connections, secrets masked; `connections test NAME` opens one
siesta schema        Tables, columns and indexes of a connection; --ddl prints CREATE TABLE
```

Statements are addressed by `-- @name`, `:LINE` or `#N`. Some useful flags:

```sh
siesta run reports.sql users_by_day --output json     # machine-readable output (default when piped)
siesta exec -c dev-pg 'SELECT count(*) FROM users'    # no file needed
siesta exec -c prod-mysql --read-only 'SELECT 1'      # the server refuses writes (a safety default, not a security boundary)
siesta run cleanup.sql --yes                          # confirm a destructive statement on a prod connection
siesta schema -c dev-pg users                         # columns before writing a query
```

Results are cut at 100 rows (`--max-rows`). `--output` is `table`, `json`, `jsonl` or `csv`; JSON follows the `siesta.run/v1` shape, with JSON / JSONB values nested.

Exit codes: `0` ok, `2` connection error, `3` parse error or unknown connection / statement, `4` refused (confirmation needed, pass `--yes`), `5` SQL error.

## AI agents

[`skills/siesta/SKILL.md`](skills/siesta/SKILL.md) teaches a coding agent (Claude Code, Codex and others that read Agent Skills) to explore a schema and run queries through the CLI. Copy the folder into the agent's skills directory, for example `~/.claude/skills/siesta`.

## Development

```text
crates/
  app/        GPUI desktop app
  cli/        siesta CLI commands
  sql-split/  statement splitter, directives
  db-conn/    siesta.json / siesta.private.json
  db-exec/    sqlx sessions, paging, schema
  settings/   settings.json
  workspace/  project folder, file index, UI state
```

Integration tests need Docker (Postgres and MariaDB): `scripts/test-db.sh`; `--down` stops the containers.

## License

MIT
