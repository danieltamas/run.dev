# run.dev release log

## 0.3.7 (2026-10-27)

Fix: the `t` shell now runs with the same environment as the service.

Pressing `t` on a service opened a shell in its directory but passed only the
`env:` map from the project YAML. The service process itself got more: the
directory's `.env` files, the YAML map, and the decrypted Cloak secrets. So a
command typed in that shell, such as `bun run legal:pull`, saw the on-disk
placeholder values of a Cloak-protected `.env` and failed with errors like
`role "dev" does not exist`, while the same command worked in an editor
terminal where the Cloak extension injects the real values.

- The shell receives the resolved service env (.env + YAML + Cloak), the same
  merge `spawn_process` uses, so scripts and one-off commands behave like the
  running service.
- Cloak secrets are resolved after the TUI is suspended, so a Touch ID or
  password prompt from `cloak export` is visible.
- When Cloak fails, the shell banner prints a warning and falls back to the
  on-disk `.env` instead of failing silently.
- Only plain environment variables are passed; the Node preload hook used for
  services is not installed in the interactive shell.

Users upgrading from 0.3.2 (the last published release) also get the fixes
shipped as 0.3.3 to 0.3.6: proxy FD exhaustion that killed all domains on
refresh, orphaned subsystem ports left behind on quit, copy and drag-select in
the log panel, a resizable wrapping command bar, and the AI chat seeing the
selected service's logs.
