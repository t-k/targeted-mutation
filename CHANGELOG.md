# Changelog

All notable changes to `targeted-mutation` (`cargo tmt`) are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.0] - 2026-11-25

### Added

- **Six value-aware targeted mutation rules**:
  - `s.starts_with(p)` / `s.ends_with(p)` → `s.contains(p)` (provable `INPUT GAP` when no test passes a string containing `p` away from the start/end).
  - `s.replace(a, b)` ↔ `s.replacen(a, b, 1)` (provable `INPUT GAP` when `a` never occurs more than once).
  - `x.clamp(lo, hi)` → `x.min(hi)` (drop lower bound) and `x.max(lo)` (drop upper bound), with infallible-bound probe specialization, reversed/NaN panic tracking, and source-built fallback for potentially panicking or side-effecting bounds.
  - `x.floor()` / `.ceil()` / `.round()` / `.trunc()` → two rounding-mode alternatives each (provable `INPUT GAP` with concrete distinguishing examples when observed inputs do not separate rounding modes).
  - `iter.all(closure)` ↔ `iter.any(closure)` on `Iterator` receivers (using method auto-reborrow and `Iterator` autoref specialization so `&mut` iterators, `Copy` iterators, and non-`Iterator` custom types compile without skips).
  - `[lit1, lit2, ...].contains(&x)` element pruning (source-built under `--rebuild` / `--source-only`), with canonical value deduplication (including `f32`/`f64`, hex/decimal, signed zero, and raw strings) and per-element survivor descriptions.
- **AST sub-hunk undo for `--since`**:
  - Decomposes multi-addition diff hunks into independent AST sub-hunk reverts for added `&&`/`||` terms, added struct literal fields (with `..base`), and added standalone statements, catching an untested clause inside an otherwise-tested hunk while preserving whole-hunk `all_undone` semantics, `#[cfg(test)]`/`#[mutants::skip]` guards, and `line!()`/`column!()`/`#[track_caller]` location sensitivity.
- **Stronger-assertion struct field narrowing**:
  - When a surviving mutant and the original return the same multi-field `Debug` struct differing on exactly one field (recursively, supporting nested structs, raw keywords like `r#type`/`r#gen`, and Unicode identifiers), compiler-verifies `&result.<path> == &(<value>)` and suggests `assert_eq!(call.<path>, <value>);` with `witness.field` populated in JSON (even when the struct does not implement `PartialEq`).
- **JSON `schema_version: 1`**:
  - Added top-level `schema_version: 1` (`report::SCHEMA_VERSION`) to single-package JSON reports, `--list --format json`, `--workspace --format json`, and every `--format jsonl` record (both success and error).
- **Terminal control-character sanitization**:
  - Sanitized untrusted project strings (paths, package names, source snippets, test names, witness output, CLI errors, and progress notes) across terminal stdout and stderr — stripping 7-bit/8-bit ANSI CSI/OSC/DCS/SOS/PM/APC/nF escape sequences, C0/C1/DEL control characters, Unicode BiDi override/isolate controls, and forged inline newlines — while keeping JSON, JSONL, and SARIF outputs lossless.

## [0.7.0] - 2026-11-24

### Added

- **Unified output format flag (`--format text|json|jsonl|sarif`)** while preserving `--json`, `--jsonl`, and `--sarif` as backward-compatible shorthand flags.
- **Grouped CLI help** organizing all options into six workflow sections (`Target & Scope`, `Mutation Mode`, `Test Execution & Selection`, `Output & Baselines`, `Performance & Cargo Build`, `Work Directory & Fixtures`) with compact `-h` summaries, detailed `--help` descriptions, usage examples, and an exit-code reference.
- **Expanded SARIF 2.1.0 reporting**:
  - Added rules for `unstable-prediction`, `weakened-test`, and `audit-disagreement`, plus `properties.audit` metadata on SARIF runs.
- **Structured audit diagnostics**:
  - Added `Audit.not_audited` for sampled survivors whose full re-run was `Unable` or an unconfirmed timeout.
  - Added `TraceDifference.rerun_failed` so trace rerun failures are distinguished from ordinary trace drift in text, JSON, and SARIF output.
- **Subreaper-aware orphan cleanup** (`ACTIVE_CHILDREN` tracking, PID 1 / Linux `PR_GET_CHILD_SUBREAPER` detection, Linux `pidfd_open` + `pidfd_send_signal` + `waitid(P_PIDFD)` with safe fallbacks, and non-Linux `ps`-bracketed `lsof` verification) with an updated `formal/tla/Leftovers.tla` model.
- **Documentation**:
  - Documented `i128`/`u128` probe specialization limits (`--source-only` workaround) and async/thread-pool worker thread attribution limits (`tokio`/`rayon`, `--isolate-tests` workaround) in `README.md`.

### Changed

- **Consistent CLI exit codes**:
  - All CLI argument parse, range, and conflict errors now exit with code `1` (instead of clap's default `2`), keeping exit code `2` unambiguous for test gaps, unstable predictions, and audit discrepancies in CI pipelines.
  - `--audit` discrepancies (`disagreements`, `source_disagreements`, and `trace_differences`) now count as gaps and return exit code `2` (while unconfirmed timeouts still take precedence with exit code `3`).
- **Baseline ignore-comment lexing**:
  - Replaced line-based comment scanning with a Rust lexer supporting string literals, raw strings, escaped character literals (`'\''`), and nested block comments (`/* ... */`). Inline trailing `// tmt:ignore` comments now apply only to their own line, while standalone comment lines continue to apply to the next line.
- **Stronger-assertion witness discovery**:
  - Added support for `super::f(...)` calls inside direct child `mod tests` modules, normalized argument token deduplication so duplicate/reformatted calls do not crowd out distinct inputs within the 4-call budget, and bounded scenario memory to `O(MAX_CALLS)`.
  - Included boundary source mutants in stronger-assertion witness synthesis and preserved original binaries with exclusive `create_new` temporary files during shared witness runs.
- **Parallel filtered timing pre-warming**:
  - Keyed cached filtered baseline timings by `(test_threads, filter)` and pre-warmed uncached jobs serially at the parallel thread count. Added a warning when combining `--source-only` with `--jobs > 1`.
- **Internal module structure**:
  - Extracted shared witness AST helpers into `src/witness/common.rs`, split `src/instrument.rs` and `src/driver.rs` into cohesive submodules, and decomposed `run_package` into focused phase helpers without changing public APIs.

### Fixed

- **Probe runtime memory ordering**: used `Release`/`Acquire` ordering on `MODE` so `TARGET` writes are guaranteed visible on weakly ordered architectures (e.g. ARM64), added fast `idle(site)` short-circuits, and fixed `-0.0`/`+0.0`/NaN handling in `MinMaxPrim`.
- **Checkpoint partial completion**: kept checkpoints resumable when `Outcome::Unable` remains in the `source` phase instead of marking the campaign completed (verified with `formal/tla/CampaignCheckpoint.tla`).
- **Work-copy and wrapper safety**: skipped special files (FIFOs, sockets, device nodes) and symlinks pointing into excluded paths during workspace copying; rejected absolute, parent-traversal, and control-character paths in `--test-wrapper`; rejected `--jobs 0`.
- **Output fixtures on case-insensitive filesystems**: safely allowed case-only renames of owned fixtures while rejecting hard links to unowned files (`same_directory_entry`).
- **Git diff & scope parsing**: unified `Scope::from_diff` with `hunks()` to avoid mistaking `+++ ` lines in diff bodies for file headers and handled `diff.suppressBlankEmpty=true`.
- **Instrumentation edge cases**: preserved inner attributes (`#![...]`) at the start of function bodies, avoided dropping meaningful trailing commas in 1-element tuples during `same_built_tokens`, inserted whitespace when removing unary `!`/`-` would merge tokens, and skipped `#[cfg]`/`#[cfg_attr]`/`#[mutants::skip]`-annotated assignments (including destructuring assignments).
- **Process cleanup and audit accuracy**: killed timed-out/errored child processes before leftover sweeps, guarded process-group signalling with `id > 1`, snapshotted nextest child groups before `wait`, skipped Linux pseudo-FDs (`/proc/<pid>/fd/*`) without dropping renamed regular files, and avoided reporting `Unable`/unconfirmed-timeout audit reruns as false disagreements.
- **SARIF path handling**: normalized Windows separators without breaking Unix literal backslashes or existing Windows baseline paths.
- **Campaign identity**: hashed canonicalized executable targets when the invoked binary is a symlink.

## [0.6.0] - 2026-11-24

- Added `first()` / `last()` sequence-end mutations when tests never observe sequences with more than one element.
- Added stronger-assertion witness synthesis under `--rebuild`, replaying existing unpinned calls from tests in place to suggest concrete `assert_eq!` values along with the test name, line, and column.
- Preserved original line endings across probes and source mutants and added property tests over generated Rust programs.

## [0.5.3] - 2026-11-23

- Refined token-level `cfg`-excluded diff comparisons for `--since` change-undone analysis (`line!`/`column!`/`#[track_caller]`, trailing commas, `RUSTFLAGS`, and macro prose vs. `#[doc = "..."]`).

## [0.5.2] - 2026-11-23

- Kept test code intact when undoing multiple hunks together, skipped changes entirely outside the active build's `cfg` features, and refined shift/equality reachability bounds.

## [0.5.1] - 2026-11-23

- Preserved test code when undoing single changes, restricted product reachability to primitive integer operations, and tried change-undone mutants even where no probe site exists.

## [0.5.0] - 2026-11-23

- Added `--since` change-undone analysis (`UNTESTED CHANGE`) and weakened-test detection (`weakened` findings when test edits allow previously caught mutants to survive).
- Added reachability filtering (with Lean 4 proofs) so impossible integer boundary values (multiples of powers of two, bitmasks, remainders) are not requested.
- Added rounding-order (`a * b / c` -> `a / c * b`) and multi-argument composite key omission witnesses under `--rebuild`.

## [0.4.1] - 2026-11-23

- Added Rust 2015 and 2018 edition support in the injected runtime, fixed Git quoted diff paths with spaces under `--since`, and improved unevaluated mutant reporting for `cfg`-disabled code.

## [0.4.0] - 2026-11-23

- Added leftover-process sweeping via inherited locked file descriptors (`TMT_LEAVE_PROCESSES` opt-out) with a TLA+ model (`formal/tla/Leftovers.tla`).
- Tightened unstable prediction classification and output fixture symlink/replacement safety.

## [0.3.1] - 2026-11-22

- Disabled probe recording in forked children before `exec` (`pre_exec` safety) so child processes cannot deadlock on inherited runtime locks.

## [0.3.0] - 2026-11-22

- Added resumable campaigns (`--checkpoint`), declared isolated Cargo output fixtures (`--copy-output-fixture`), Git index snapshots (`--git-index`), workspace runs (`--workspace`, `--test-workspace`, `--jsonl`, workspace SARIF), sharding (`--shard`, `--sharding`), `--list`, `--error`, `--source-only`, `--test-tool nextest`, and major performance optimizations across tracing, effects replay, and SARIF streaming.

## [0.2.1] - 2026-11-21

- Limited Cargo builds to selected `--test-target` artifacts when Cargo can express the selection exactly.

## [0.2.0] - 2026-11-20

- Initial public release of targeted mutation testing (`cargo tmt`) with runtime probes, interval-based input-gap planning, assertion-gap effect tracing, `--rebuild` witnesses, baselines, SARIF output, and formal Lean/PICT/TLA+ verification artifacts.

[0.8.0]: https://github.com/t-k/targeted-mutation/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/t-k/targeted-mutation/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/t-k/targeted-mutation/compare/v0.5.3...v0.6.0
[0.5.3]: https://github.com/t-k/targeted-mutation/compare/v0.5.2...v0.5.3
[0.5.2]: https://github.com/t-k/targeted-mutation/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/t-k/targeted-mutation/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/t-k/targeted-mutation/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/t-k/targeted-mutation/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/t-k/targeted-mutation/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/t-k/targeted-mutation/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/t-k/targeted-mutation/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/t-k/targeted-mutation/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/t-k/targeted-mutation/releases/tag/v0.2.0
