[← Siesta](../README.md)

# Where files live

Two folders, kept apart on purpose: one for what you write, one for what the app writes.

```text
~/.config/siesta/           what you write; fine for a dotfiles repo
  settings.json             settings (the Settings page edits it)
  keymap.json               your key bindings over the defaults (optional)
  collections/              the sidebar's COLLECTIONS: query files, any subfolders
  connections/              one JSON file per connection (passwords only encrypted)

~/.siesta/                  what the app writes; keep it private
  key                       the key the passwords are encrypted with
  files.json                the sidebar's FILES: paths of files added from elsewhere
  state.json                open tabs and cursors, reopened on launch
  history/                  every run (statement, connection, outcome; never the data)
  cache/                    database structure per connection, for the DB tab and completion
```

- **Collections** are plain files and folders: Finder, an editor or git work on them too.
- **Added files** stay where they are; Siesta keeps only their paths.
- **A file's connection and database** are its own `-- @connection` / `-- @database` lines. Nothing about them is kept in `~/.siesta`.
- `~/.siesta` holds the password key and the statements you ran. Keep it out of dotfiles.
- `SIESTA_CONFIG_DIR` and `SIESTA_DATA_DIR` replace the two folders (tests, sandboxes).
