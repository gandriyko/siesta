[← Siesta](../README.md)

# Development

## Crates

| Crate | What it holds |
|---|---|
| `app` | The GPUI desktop app (and the `siesta` binary) |
| `cli` | CLI commands and the MCP server |
| `sql-split` | Statement splitting and safety checks: SQL, Elasticsearch requests, MongoDB calls |
| `db-conn` | Connection files, encrypted passwords |
| `db-exec` | Sessions (sqlx, Elasticsearch over HTTP, the MongoDB driver), paging, schema, history |
| `settings` | `settings.json` |
| `workspace` | Central folder scan, added files, UI state |

## Tests

```sh
cargo test --workspace          # unit and GUI tests
scripts/test-db.sh              # integration tests on Postgres, MariaDB, Elasticsearch and MongoDB in Docker
scripts/test-db.sh --down       # stop the containers
```

## Build

```sh
scripts/bundle-macos.sh         # target/release/Siesta.app (--install copies it to /Applications)
cargo install --path crates/app # the siesta binary on PATH
```

## Design

The principles and decisions behind the design are in [the specification](../SPEC.md). Siesta is a sibling of [Respite](https://github.com/gandriyko/respite), a text-first REST client built the same way.
