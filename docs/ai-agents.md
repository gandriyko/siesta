[← Siesta](../README.md)

# AI agents

Siesta gives coding agents two ways in: a skill that teaches the CLI, and an MCP server.

## Skill

[`skills/siesta/SKILL.md`](../skills/siesta/SKILL.md) teaches an agent (Claude Code, Codex and others that read Agent Skills) to explore a schema and run queries through the CLI.

Copy the folder into the agent's skills directory, for example `~/.claude/skills/siesta`.

## MCP server

`siesta mcp` is a stdio MCP server with four tools:

| Tool | What it does |
|---|---|
| `connections` | Connections, secrets masked |
| `schema` | Tables, columns, indexes; DDL of one table |
| `list_statements` | Statements of a `.sql` / `.es` / `.mongodb.js` file or folder |
| `run_sql` | SQL, Elasticsearch requests or MongoDB calls, by the connection's driver |

Each tool runs the CLI command with JSON output, so the results, masking and exit codes are the same. Nothing opens on screen.

### Set up in Claude Code

```sh
cargo install --path crates/app          # puts `siesta` on PATH
claude mcp add -s user siesta -- siesta mcp
claude mcp get siesta                    # should say Connected
```

`-s user` makes the server available in every project. The tools appear in a new session.

## What an agent may run

An agent never skips a confirmation. Siesta refuses these before anything runs (exit `4`); the user runs them:

| Connection | The agent runs |
|---|---|
| Ordinary | Everything, except statements that need confirmation (`-- @confirm`) |
| `read_only` | Reads only |
| `prod` | Reads, and one kind of update: |
| | SQL: `UPDATE … WHERE` (reads run in a server read-only session) |
| | Elasticsearch: `POST <index>/_update/<id>` with a `doc` body |
| | MongoDB: `updateOne` / `updateMany` with a filter and update operators (`$set`, …) |

These are safety defaults, not a security boundary. For real protection give the agent's connection a read-only database user.

Every statement an agent runs is in the app's History with the source `mcp`.
