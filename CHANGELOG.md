# Changelog

All notable changes to `targeted-mutation` (`cargo tmt`) are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.10.3] - 2026-11-25

### Improved

- **Runtime cast-source type observation for disjoint `|` ↔ `^`**:
  - Added lightweight `CastPrim` / `CastAny` runtime observation probes (`C <site> <slot> <min|-> <max|->`) for non-literal primitive casts (`as u32`, etc.) and primitive `From::from` operands inside `|` / `^`, allowing `rustc`'s own type inference to prove disjoint widening bit-packing even across inferred iterators and method chains such as `bytes.chunks(3)` and `chunk.get(1).copied().unwrap_or(0)`.
  - Stayed conservative on polymorphic/conflicting observations across calls or binaries, generic fallbacks, signed widening, pointer casts/aliases (preserving `&mut T` reborrows), and shadowed `From` traits (including `#![no_implicit_prelude]`).
- **Progress and empty-run diagnostic polish**:
  - Counted only files containing at least one active site in `tmt: N sites in M file(s)` (using singular `file` for 1).
  - Omitted the `--source-only` suggestion on `NoMutantsExecuted` when every candidate was skipped solely as unviable/uncompilable (`unreached` empty, `mutants_not_built == 0`, `mutants_not_evaluated == 0`).
  - Appended `(<N>s elapsed)` when repeated periodic build-progress ticks emit the same status line while waiting on crate compilation/linking.

## [0.10.2] - 2026-11-25

### Improved

- **Widening-cast bit-packing disjointness for `|` ↔ `^`**:
  - Extended static `possible_ones` bit-disjointness analysis to track lexical primitive integer/slice bindings and handle widening casts (`as u32`, nested casts, slice/array indexing, and primitive `u32::from(...)`), skipping equivalent `|` ↔ `^` (`ArithSwap2`) mutants in common binary-encoder patterns such as `((first as u32) << 16) | ((second as u32) << 8) | third as u32`.
  - Remained conservative on signed widening sign-extension, overloaded binary operators, shadowed bindings (`let`, `if let`/`while let` chains, inline `mod` namespaces, `#[cfg]` bindings), and shadowed primitive type names, and fixed parenthesis stripping in the driver's primitive-operand matcher so sibling parenthesized expressions like `(a << 16) | (b << 8)` are not mis-stripped.
- **Byte and character literal formatting in `nearest values` and comparison gaps**:
  - Recognized `b'...'` byte literals and `'...'` character literals as comparison `LiteralSide` values, instrumented `char` comparisons using an order-preserving bijection onto the contiguous scalar range `0..=0x10_D7FF` (eliminating the `0xD800..=0xDFFF` surrogate gap), and rendered observed values and suggested inputs with readable literal + numeric representations (e.g. `` `tag` = b'o' (111) ``, `` `ch` = 'o' (111) ``).
- **Work-directory lock contention diagnostics**:
  - Atomically recorded the lock holder's PID in `<work-dir>/.tmt-lock/pid` (best-effort) and read it safely without following symlinks or blocking on special files (`O_NOFOLLOW | O_NONBLOCK` on Unix, `FILE_FLAG_OPEN_REPARSE_POINT` on Windows), appending `(last recorded pid <PID>)` when available and hinting to pass a separate `--work-dir` for parallel runs.

## [0.10.1] - 2026-11-25

### Fixed

- **Effect-trace representative selection with `UNKNOWN_TEST` (`"?"`)**:
  - Picked the first named test as representative when `UNKNOWN_TEST` (`"?"`) is mixed with named tests in an effect-trace test list instead of rendering `` `?` (and N more) ``, and rendered unnamed-only lists as `"a test"` / `"a test (and N more)"`.
- **Static bit-disjointness filtering and clearer advice for `|` ↔ `^`**:
  - Recorded primitive integer type bounds on `|` and `^` probes and skipped the equivalent `|` ↔ `^` (`ArithSwap2`) mutation when runtime-verified primitive sub-expressions prove `(lhs_possible_ones & rhs_possible_ones) == 0` (covering constant shifts, bitmasks, unsigned remainders, and power-of-two mul/div, while conservatively preserving signed right shifts, overloaded operators, and any site where `differs2` was observed).
  - Updated `INPUT GAP` advice for `|` ↔ `^` when disjointness cannot be statically proved to explain that operands must share at least one set bit and that `|` and `^` are equivalent if disjoint by construction.
- **Standard collection capacity-hint noise filtering**:
  - Skipped pure capacity-hint expressions in standard collection constructors (`Vec`, `String`, `VecDeque`, `BinaryHeap`, `HashMap`, `HashSet`, `OsString`, `PathBuf`, `CString`, `BytesMut`, `SmallVec`, `IndexMap`, `IndexSet` `with_capacity` / `with_capacity_and_hasher` / `reserve` / `reserve_exact`) and `.reserve_exact(...)`, as well as statement deletion on plain-place `reserve` / `reserve_exact` calls, while keeping non-capacity arguments, side-effecting blocks/receivers, custom domain APIs, and fallible `try_reserve` intact.
- **Duplicate enclosing `ASSERTION GAP` on constant-operand `||` / `&&`**:
  - Suppressed surviving `||` ↔ `&&` (`LogicSwap`) assertion gaps when every observed evaluation produced a boolean value where `a || b == a && b` (all `(false, false)` for `||` or all `(true, true)` for `&&`) and the right-hand operand is syntactically side-effect-free, accounting for the survivor under `survivor_breakdown.redundant_constant` so report counts continue to reconcile.

## [0.10.0] - 2026-11-25

### Added

- **External parent-relative path staging (`--stage-external <SRC[=REL]>`)**:
  - Added repeatable `--stage-external` supporting both shorthand (`../../proto`) and explicit (`SRC=REL`) forms to stage parent-relative files and directories into a confined subdirectory layout under `<work-dir>/src` for monorepo subpackages whose `build.rs` or `include_str!` references shared parent paths.
  - Enforced symlink confinement to the staged root, case-insensitive collision rejection, and `WorkCopy` separation between staged external paths and the workspace copy.
- **Substring/module test filtering and Cargo-compatible target flags**:
  - Added repeatable `--test-filter <PATTERN>` for substring and module-prefix libtest filtering (combined with `--include-test` via union and `--exclude-test` via exclusion).
  - Added `--lib`, `--bins`, and repeatable `--test <NAME>` target selection flags (including explicit selection of `test = false` targets).
  - Added pre-build target validation against `cargo metadata` and actionable zero-test hints (suggesting `--lib` / underscore library target names when a binary target has no tests, and suggesting `--test-filter` when `--include-test` matches a runnable test only by substring).
- **Streaming Cargo build progress and persistent repair exclusion cache**:
  - Streamed `cargo test --no-run --message-format=json` output to report compiled/cached crate counts and the latest crate name to stderr every 5 seconds during long builds (suppressed under `--quiet`).
  - Cached compile-error repair exclusions in `<work-dir>/.tmt-repair-cache.json` using exclusive atomic writes (`create_new` + `rename`) and a SHA-256 fingerprint covering the full staged tree, external path dependencies, environment variables, Cargo configuration, and `rustc` version so identical repeat runs finish in a single build.

### Fixed

- **Skip pointless repair rebuilds on non-probe compile errors**:
  - Detected missing-file and compile-time environment errors (`include_str!`/`include_bytes!`/`include!`, undefined `env!`, and `E0583` missing modules) outside probe-inserted text and exited immediately without running an unnecessary repair rebuild.

## [0.9.0] - 2026-11-25

### Added

- **Reconcilable report mutant counts (`mutants_survived`, `survivor_breakdown`, `Report::counts_reconcile`)**:
  - Added top-level `mutants_survived` (`mutants_run - mutants_killed`) and `survivor_breakdown` (`reported`, `suppressed`, `redundant_constant`, `preexisting_unchanged`, `unchanged_not_compared`, `unconfirmed_gap`) to JSON reports so that every run satisfies `mutants_run == mutants_killed + mutants_survived`, `mutants_timed_out <= mutants_killed`, and `mutants_survived == sum(survivor_breakdown)`.
  - Added text-output breakdown lines for surviving mutants intentionally not emitted as findings.
  - Documented exact report count definitions, subset relationships, and evaluation statuses in `README.md` and `cargo tmt --help`.
- **Explicit partial evaluation status (`Evaluation::Partial`)**:
  - Added `evaluation: "partial"` in JSON/SARIF reports and `(partial evaluation: some mutants were not built or evaluated)` in text output whenever `mutants_run > 0` and any mutants remain unbuilt (`mutants_not_built > 0` without `--rebuild`), unevaluated (`mutants_not_evaluated > 0`), skipped (`skipped_sites > 0` or `UnparseableSource`), uncompared against earlier tests (`survivor_breakdown.unchanged_not_compared > 0`), unconfirmed timeouts, or `--source-only` timeouts.

### Changed

- **Public Rust API & JSON/SARIF `evaluation` compatibility**:
  - `report::Evaluation` now includes `Evaluation::Partial` (`"partial"` in JSON/SARIF), and `report::Report` includes `mutants_survived` and `survivor_breakdown`. Exhaustive `match` arms on `Evaluation`, struct literals constructing `Report` without `..Report::default()`, and JSON/SARIF consumers validating a closed set of `evaluation` strings should be updated for `"partial"`.

### Fixed

- **`--since` earlier-test comparison & unbuilt revert accounting**:
  - Distinguished confirmed preexisting survivors in unchanged code (`preexisting_unchanged`) from survivors whose earlier-test comparison could not be established (`unchanged_not_compared`, e.g. when earlier tests fail to build/pass or cannot all be restored) and marked the latter as `evaluation: "partial"`.
  - Counted unbuilt `--since` revert mutants across all changed files (including const-only files without probe sites) in both early-return and normal paths so they are not hidden as `no_sites` or omitted from `mutants_not_built`.
- **Witness evaluation accounting**:
  - Classified `Outcome::Unable` and non-compiler build failures during witness trials under `mutants_not_evaluated` instead of `mutants_killed` or `unviable_mutants`, and avoided checkpointing `Outcome::Unable` witness trials so resumed runs retry them.

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
