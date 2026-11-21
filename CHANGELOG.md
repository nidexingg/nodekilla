# Release Notes

See what **🦀NODEKILLA.RS** has changed:

## v0.3.0

- perf(scan): Split discovery and sizing into separate parallel phases, with directory-level tasks shared across workers.
- feat(cli): Add `--no-size` to skip size calculations. It works with `--dry-run` but cannot be combined with `--sort-size`.
- feat(cli): Add `-j N` / `--workers N` to configure worker counts for discovery, sizing, and deletion.
- feat(output): Report discovery and sizing times separately, or show when sizing was skipped.
- fix(scan): Detect `node_modules` directly inside the scan target.
- fix(selection): Clamp numeric ranges to the displayed list before iterating to avoid stalls from oversized input.

## v0.2.0

- feat(cli): Accept an optional target directory while keeping the executable's directory as the default.
- feat(cli): Add `-s` / `--sort-size` for largest-first sorting and `-n` / `--dry-run` to preview selected folders without deleting them.
- perf(scan): Scan and size top-level projects in parallel.
- perf(cleanup): Delete confirmed folders in parallel.
- feat(cleanup): Check that each target is named `node_modules` and resolves within the scan root before deletion.

## v0.1.0

- chore(release): Ship a standalone executable built with the Rust standard library and no external dependencies.
- feat(scan): Scan non-hidden project folders next to the executable in case-insensitive alphabetical order. Skip hidden directories and stop descending when a `node_modules` folder is found.
- feat(output): Show human-readable folder sizes and estimated reclaimable space, skipping unreadable entries.
- feat(selection): Support individual numbers, ranges, select-all, and quit commands.
- feat(cleanup): Require explicit `yes` confirmation before permanent deletion, report per-folder results, and summarize estimated space freed.