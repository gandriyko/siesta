[← Siesta](../README.md)

# CLI

The desktop app and the CLI are one binary: `siesta` with no arguments opens the app, with a command it runs in the terminal.

## Commands

| Command | What it does |
|---|---|
| `siesta list [PATH]` | Statements of a file or folder: file, line, name, first line |
| `siesta run FILE [STATEMENTS]…` | Run statements of a file (all of them, in order, when none are named) |
| `siesta exec [SQL]` | Run SQL from an argument or stdin |
| `siesta schema [TABLE]` | Tables, columns and indexes; `--ddl` prints `CREATE TABLE` |
| `siesta connections` | Connections, secrets masked |
| `siesta connections test NAME` | Open a connection and show the server version |
| `siesta connections databases NAME` | Databases the credentials can access |
| `siesta connections set-password NAME` | Store a password, encrypted |
| `siesta mcp` | MCP server for AI agents (see [AI agents](ai-agents.md)) |

Statements are addressed by `-- @name`, `:LINE` (any line of the statement) or `#N`.

## Examples

```sh
siesta run reports.sql users_by_day --output json         # machine-readable (the default when piped)
siesta exec -c dev-pg 'SELECT count(*) FROM users'        # no file needed
siesta exec -c dev-pg -d app_test 'SELECT current_database()'
siesta exec -c prod-mysql --read-only 'SELECT 1'          # writes are refused (exit 4)
siesta run cleanup.sql                                     # on prod, non-read statements ask [y/N]
siesta schema -c dev-pg users                             # columns before writing a query
```

## Flags

| Flag | Meaning |
|---|---|
| `-c`, `--connection NAME` | The connection (`run` defaults to the file's `-- @connection`) |
| `-d`, `--database NAME` | Another database on that server |
| `--output` | `table`, `json`, `jsonl` or `csv` |
| `--max-rows N` | Rows per statement (default 100) |
| `--read-only` | Refuse writes |
| `--yes` | Skip confirmations; on a `prod` connection only in a terminal |

JSON follows the `siesta.run/v1` shape; JSON / JSONB values stay nested.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | OK |
| `2` | Connection error |
| `3` | Parse error, unknown connection or statement, a file on a connection of the other kind |
| `4` | Refused: confirmation needed, or a write on a read-only connection |
| `5` | Error from the server |
