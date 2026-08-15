# DSH Atelier Project Instructions

## Scope

- Windows is the primary local development and acceptance platform.
- macOS must continue to compile and pass automated tests; desktop integration is verified on a Mac later.
- The first release manages DSH and opens its existing Web UI in the system browser.
- The next phase adds exactly one built-in Surface: `surface:dsh`, a system-WebView shell that loads the validated readiness URL of the managed DSH instance.
- `surface:dsh` must not copy, rebuild, inject into, or otherwise fork the upstream DSH Web UI. Do not add other custom application surfaces as part of this work.
- Atelier state belongs under `~/.atelier`; DSH retains its default `~/.dsh` user directory.

## Development

- Use test-driven development for behavior changes.
- Keep Tray and the controller in one process for the first release.
- Keep Tray, the controller, and the built-in DSH Surface in one Runtime process for the next phase. The controller remains the sole writer of DSH lifecycle state.
- Keep platform effects behind adapters so tests never open a browser, show notifications, modify autostart, or write to the real home directory.
- Keep window and WebView effects behind adapters so automated tests never create a real window or WebView.
- Do not access a live npm registry in normal tests. Use fixtures; live checks must be explicit smoke tests.
- Do not modify external DSH, Node, npm, `PATH`, `~/.npmrc`, or `~/.dsh` during tests.
- Run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-targets --all-features` before claiming completion.

## Architecture

- Keep one production crate until a second real consumer justifies extraction.
- The controller is the sole writer of DSH lifecycle state.
- Bootstrap remains independent of DSH, Tray, and browser concepts.
- Pass programs and arguments as structured values; do not build shell command strings except inside the Windows adapter for a resolved `.cmd` shim.
- Bind DSH only to `127.0.0.1` and validate readiness URLs before opening them.
- Only load the controller-validated readiness URL in `surface:dsh`; reject arbitrary navigation and send external links to the system browser.
- Reuse and focus one DSH Surface window. Closing it hides the window without stopping DSH; exiting Atelier still stops the managed DSH process.
