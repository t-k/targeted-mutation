# targeted-mutation

[![Crates.io version](https://img.shields.io/crates/v/targeted-mutation.svg)](https://crates.io/crates/targeted-mutation)

**Targeted mutation testing** generates the realistic wrong implementations that your existing tests would let through, and tells you which input or assertion would catch them.

Classic mutation testing changes `<` to `<=` and then asks whether a test fails. This tool works the other way round: it first watches what the tests actually feed the code, and from that computes the mutants that are guaranteed to pass, without running them. It needs no AI and no solver.

The command is `cargo tmt`. It works on Rust crates.

## Try it

```sh
cargo install targeted-mutation
cargo new --lib demo && cd demo
```

Put this in `src/lib.rs`:

```rust
pub fn is_eligible(age: u32) -> bool {
    age >= 18
}

#[test]
fn minor_is_rejected() {
    assert!(!is_eligible(17));
}

#[test]
fn adult_is_accepted() {
    assert!(is_eligible(20));
}
```

```text
$ cargo tmt --quiet
INPUT GAP  src/lib.rs:2:5
    age >= 18
  passes every test as: `age >= 19`, `age >= 20`  [re-run: tests pass]
  nearest values the tests passed: `age` = 17, 20
  add a test with `age` = 18 (the original gives true)

demo: 4 sites, 1 input gaps, 0 assertion gaps, 8 mutants run (6 killed), 0 sites unreached
$ echo $?
2
```

Add the test it asks for and run it again:

```rust
#[test]
fn eighteen_is_accepted() {
    assert!(is_eligible(18));
}
```

```text
$ cargo tmt --quiet
demo: 4 sites, 0 input gaps, 0 assertion gaps, 7 mutants run (7 killed), 0 sites unreached
$ echo $?
0
```

`cargo tmt` builds and runs the crate's tests and build scripts, as `cargo test` does: run it on code you trust.

## What it reports

The examples below are crates under `fixtures/` in the repository.

```rust
pub fn is_eligible(age: u32) -> bool {
    age >= 18
}

#[test] fn minor_is_rejected() { assert!(!is_eligible(17)); }
#[test] fn adult_is_accepted() { assert!(is_eligible(20)); }
```

```text
$ cargo tmt fixtures/eligibility
INPUT GAP  src/lib.rs:4:5
    age >= 18
  passes every test as: `age >= 19`, `age >= 20`  [re-run: tests pass]
  nearest values the tests passed: `age` = 17, 20
  add a test with `age` = 18 (the original gives true)
```

The tests saw 17 and 20, so any threshold in 18..=20 behaves the same on every test. The tool derives that range from the two observed values and names the one input that pins the boundary.

A value that no input can give is not asked for. Compared with a number, `blocks * 8` is only ever a multiple of eight, `flags & 0xF0` only has bits of the mask, and `n % 10` is smaller than ten: when no such value lies between the ones the tests gave, nothing is reported, and otherwise the values named are ones the expression can be. A comparison that such a value cannot make come out the other way (`n % 10 < 10`) is not asked to. This is read from the last operation of the expression, for integers, and only where it also holds when the arithmetic wraps (a product by a power of two, a shift, a mask, a remainder).

The second kind of finding is about assertions rather than inputs. `fixtures/batch-store` has an atomic batch commit and a test that checks a failing batch returns an error, but never looks at the store afterwards:

```text
$ cargo tmt fixtures/batch-store
ASSERTION GAP  src/lib.rs:33:9
    self.check_preconditions(batch)?;
  passes every test as: delete `self.check_preconditions(batch)?;`
  these tests run the changed code and still pass: tests::batch_writes_are_visible_after_commit, tests::commit_fails_when_an_update_target_is_missing
  add an assertion on what this code is responsible for
```

Without the precondition check the commit still fails, but the first write of the batch has already been applied. One assertion on the store after the failed commit closes the gap.

| Finding | Meaning | What to add |
|---|---|---|
| `INPUT GAP` | No test passes a value that separates the mutant from the original. | A test with the listed input. |
| `ASSERTION GAP` | Tests execute the changed behaviour and still pass. | An assertion in one of the listed tests. |
| `UNREACHED` (`--show-unreached`) | No test executes the code. | A test. |
| `UNSTABLE` | The trace predicted a survivor, but the re-run failed. | The tests are not deterministic; fix that first. |

For an assertion gap the tool runs the surviving mutant once more with tracing on and compares the trace with the original run. The lines starting with `with the mutant,` say what changed in the tests that let it through:

```text
ASSERTION GAP  src/lib.rs:33:9
    self.check_preconditions(batch)?;
  passes every test as: delete `self.check_preconditions(batch)?;`
  these tests run the changed code and still pass: tests::batch_writes_are_visible_after_commit, tests::commit_fails_when_an_update_target_is_missing
  with the mutant, `tests::commit_fails_when_an_update_target_is_missing` now runs `self.docs.insert(key.clone(), value.clone());` (src/lib.rs:59)
```

The failing commit now gets as far as inserting a document: that is the state to assert on. When the trace shows no change at all, the tests reach the same code with the same outcomes, which means they do not use the result of the mutated code (or the mutant is equivalent).

Findings on the same source line are printed as one block.

## Usage

```text
cargo install targeted-mutation
cargo tmt [DIR] [OPTIONS]
```

Building it takes Rust 1.88 or newer. The crates it analyses are built with whatever toolchain is active for them.

What to analyse:

- `--package NAME`: the workspace member to analyse. Not needed when `DIR` is the member's own directory.
- `--since REV`: only the code changed since a git revision, plus untracked files. This is the mode for a pull request: `cargo tmt --since "$(git merge-base origin/main HEAD)"` (the merge base, so that what others added to the main branch meanwhile is not taken for yours). A run of a part can tell that a finding is new; it cannot tell that one outside of it is gone, which takes a run of the whole. With `--rebuild` or `--source-only`, each change is also tried undone (see [A change undone](#a-change-undone)).
- `--lines FILE:FROM-TO`: only these lines (repeatable; `FILE` is relative to the package).

Which tests to use:

- `--also-test PACKAGE`: also run the tests of another workspace package (repeatable).
- `--dependents`: also run the tests of every workspace package that depends on the analysed one. A gap that an integration test elsewhere closes is then not reported.
- `--doc`: also run the doc tests. rustdoc recompiles them for every mutant they reach, so this is slow.
- `--isolate-tests`: run each selected libtest test in its own process. `--test-target NAME` selects Cargo test executable names, and `--include-test NAME` and `--exclude-test NAME` select exact libtest names. These flags are repeatable and enable isolation. `--include-ignored` also allows ignored tests. Keep the default `--jobs 1` when tests share ports or files.
- `--test-target NAME` also narrows the Cargo build when Cargo can express the selection exactly. Cargo still builds required dependencies, including binaries used by integration tests. Selections such as a subset of workspace libraries or targets whose required features cannot be confirmed use the usual package-wide Cargo build. JSON reports show `analysis.test_selection.build_scope` as `selected_targets` or `package_wide`.
- `--test-wrapper PATH`: in isolated mode, run a workspace-relative executable from the working copy with the test executable and its arguments. A wrapper can set up a private temporary directory and then run the supplied command, preserving its status and the `TMT_*` environment. The report records the selected tests and wrapper. Isolated selection works with standard libtest executables and does not run doc tests.
- Use `--baseline-jobs N` to run N isolated baseline tests at once. The default is 1. Baseline progress shows the number completed, and a failure identifies the selected test and its log.
- Set `--wrapper-grace SECONDS` to control cleanup on Unix, where a timeout sends the wrapper's process group SIGTERM and gives it up to 2 seconds to clean up by default. Choose 0 to force-stop immediately. The result is still a timeout.

Findings you have already dealt with:

- A comment `// tmt:ignore <reason>` on the line, or on the line above, silences the findings of that line.
- `--write-baseline FILE` records the findings of a run; `--baseline FILE` leaves the recorded ones out. A finding is known by its `id`: its kind, file, the function or constant it is in, its code, and which of the places with that code in that item it is. No line numbers and none of the words that describe the mutants are in it, so it survives code moving around and this tool wording things differently. (All findings of one kind at one place share an id.) Files written before version 2 of the format, which list fingerprints, are still read. Suppressed findings are counted in the summary and do not affect the exit status.

Running:

- `--jobs N`: run N mutants at the same time (default 1). Raise it only if the tests can run concurrently with themselves: no fixed ports, no shared files.
- `--work-dir DIR`: where the working copy and its build go. It must be a directory of its own: the tool deletes files from the copy inside it, and refuses a directory that contains the workspace or a `src` it did not create.
- The working directory is locked throughout execution. On Unix, an active Cargo or test child keeps the lock if the TMT parent exits. A legacy lock from an older TMT requires a one-time migration: confirm that the older campaign and its children are stopped before removing the old marker, or use a fresh work directory.
- `--all-files`: copy files that git ignores too, subject to the default exclusions for Git metadata, `node_modules` and build caches.
- `--copy-fixture PATH`: copy a particular workspace-relative file or directory even if ignored, for example `--copy-fixture sdk/node_modules`. Prepare it before running tmt. Links are resolved only within the workspace and excluded paths are protected.
- Use `--copy-fixture-exclude PATH` to omit a workspace-relative entry from a declared `--copy-fixture`, for example an unused dangling command shim. The exclusion is recorded in the report, and paths escaping the workspace are rejected.
- Use `--copy-output-fixture FROM=TO` to stage a workspace file or directory under the isolated Cargo output. For example, `--copy-output-fixture tools/runner-node=debug/runner-node` makes that directory available beside ordinary debug binaries, including in children that clear their environment. The destination is relative to the isolated output, and existing unmanaged files are protected. Destinations that differ only in the case of their letters are refused: on a file system that does not tell case apart they are one file.
- `--git-index`: give tests an isolated Git index for commands such as `git ls-files`. The copy has its own metadata. History, remotes and staged blob contents are unavailable.
- `--no-verify`: trust the trace; do not re-run tests against predicted survivors.
- `--no-effects`: do not trace surviving mutants to say what they change. (Where several mutants survive at one place, one of them is followed through the tests; the report says which, in the text and as `effects_of`.)
- `--features A,B`, `--all-features`, `--no-default-features`, `--locked`: as for `cargo test`, and told to every cargo command of the run.
- `--audit N`: check the run itself, three ways. Up to N of the surviving mutants (spread evenly, the same ones every time) are run once more against every test of every binary, not only the tests chosen for them; one that fails is counted as found out and listed. Up to N of the mutants that were run as probes are also built from the source, as a person would write them, and run against all tests; where the result is not the one of the probe, both are listed (a build and a full test run each: slow). The summary says how many were chosen, how many could be compared, and how many could not (they did not build, or their probe run was given up). A negated comparison that no mutant of the source stands for is compared with the negation written out, `!(a >= b)`. And the tests are recorded a second time; binaries whose two recordings differ are listed, since their tests do not always do the same.
- `--opt-level N`: build the tests at optimisation level N (0 to 3) instead of the profile's own. Worth it when tests spend their time in loops (see "Speed").
- `--rebuild`: also run the mutants that need a build of their own (see "How it works", step 5). Without it the summary says how many were left out.
- `--error EXPR`: add `Err(EXPR)` body replacements for return types written as `Result`, including nested values such as `Option<Result<T, E>>`. For a function returning `impl Iterator<Item = Result<T, E>>`, use it with `--rebuild` or `--source-only`. Repeat this flag for more error payloads. The expression is Rust code evaluated in the function's scope. For a `Result<T, String>`, use `--error '"mutated".into()'`.
- `--source-only`: build supported mutants directly from source and run them against the selected tests. Scope it with `--lines` or `--since` when tests launch children with `Command::env_clear()` or otherwise omit the probe environment. This mode is slower and reports surviving source mutants without trace-based input or assertion suggestions.
- `--timeout SECONDS`: set the time limit for each test runner under a mutant. When an executable has a slow baseline, TMT measures the exact filters it runs and reuses those timings. It falls back to the full baseline when a filtered timing is unavailable. Progress shows slow tests and their chosen limit. If a comparable baseline meets or exceeds an explicit limit, TMT warns before running the affected mutants and keeps the limit. The warning is written to stderr even with `--quiet`. In source-only mode, a timeout or an unevaluated mutant causes exit status 3, even if the report also contains survivors.
- Set `--timeout-recheck serial|none` to choose the policy for parallel timeouts. The default is a serial recheck. The confirmation phase reports its initial and pending counts, and the report shows confirmed, recovered and unconfirmed timeouts. With `none`, a parallel timeout stays unconfirmed and returns exit status 3.
- Use `--checkpoint` to append completed outcomes to `checkpoint.jsonl` in the work directory. Rerun with the same settings and work directory to reuse decisions for matching candidates, inputs, tools and test selection. Baselines still run, and witness diagnostics can be regenerated. A campaign without a final `campaign_completed` event is partial. Use checkpoints for deterministic suites with the same external test data and services. The settings include every environment variable of the run, since a test may read any of them: where variables change from one invocation to the next (a CI job's run id), nothing is reused. A source mutant whose build failed without the compiler refusing the code (it was killed, the disk was full) is counted as not evaluated and is not remembered.

For a project switching its existing mutation testing command to TMT:

```sh
cargo tmt --source-only --literal-operators --doc
```

This runs source edits with Rust's operator precedence and includes doctests. Add `--workspace` to mutate every workspace member, or `--test-workspace` to let sibling packages' tests kill the selected package's mutants. Use `--test-tool nextest --nextest-profile ci` without `--doc` for a CI job that uses a Nextest profile. The exit statuses are 0 for success, 1 for an analysis error, 2 for test gaps, and 3 for an incomplete analysis.

For Cargo workspace workflows:

```sh
cargo tmt --workspace
cargo tmt -p app -p shared --test-workspace
cargo tmt --workspace --source-only --shard 0/4 --json
cargo tmt --source-only --test-tool nextest --nextest-profile ci
```

- `--workspace` selects all workspace members, and repeated `-p` selects specific members. Catalogue mode reuses one source snapshot across packages. Execution keeps one working copy per package. An analysis failure in one member leaves the other reports available. JSON has `packages` and `errors` arrays, and SARIF has one run per successful package. Baseline files and their paths apply to a single package. `--lines` paths remain relative to each selected package.
- `--jsonl` writes and flushes one JSON object for each completed package, with either a `report` or an `error`. It preserves completed packages when a later analysis fails or the campaign is interrupted.
- `--test-workspace` runs tests from all workspace members against each selected package's mutations.
- `--list --json` previews source edits without compiling or running any tests. Each edit includes its package-relative file and original byte range. The catalogue includes source-based distinguishing candidates whose applicability still has to be verified. Trace-derived variants, including concrete integer thresholds, depend on a test run.
- `--source-only --shard K/N` chooses a zero-based shard of the ordered catalogue. `--sharding slice` uses consecutive ranges and is the default. `--sharding round-robin` interleaves candidates. Use the same source, selection and configuration in every CI job. An empty shard succeeds without running tests.
- `--source-only --literal-operators` changes just the operator token. It allows the replacement operator's precedence to affect the result. The default keeps the original operands together.
- On Unix, `--source-only --test-tool nextest` uses an installed cargo-nextest and its workspace configuration. Use `--nextest-profile` to choose a profile and that profile's filters to select tests.

With either rebuild mode, TMT can run an additional test in the working copy to show how the original and a surviving mutant differ. The report includes both observed values and an assertion based on the original. A selected library test harness is required, and JSON reports expose the evidence as `findings[].witness`.

The rules recognize specific Rust patterns in the files of the library (the crate root and its modules, where no other target builds the file) and simple tests in the same file. The test that shows the difference is added to that file in the working copy:

- In integer-slice searches, try two matches to distinguish `position` and `rposition`. For a surviving inclusive range, try editing just the extra zero in a literal test slice.
- Try a partial multi-bit mask to distinguish any-bit and all-bit checks, and a non-ASCII character to distinguish text character counts and byte lengths.
- Substitute same-type integer arguments or record fields in comparisons and put the two values on opposite sides of the boundary. For slice sorting by a record field, try two records whose order depends on the choice of key.
- When a failing batch is only checked for an error, try a read borrowed from a nearby test for a key in the attempted batch.
- For `a * b / c` on integer arguments with a literal `c`, try dividing first (`a / c * b`), which rounds earlier. Take a call from the tests and change `a` by the smallest remainder that shows the difference; when every `a` in the tests is a multiple of `c`, the report says so. `fixtures/rounding` is an example: all six mutants that are switched while running are found out, and this one passes until an amount that is not round is tested.
- For a function that makes one value of several arguments (a `format!` or a tuple of them, such as the key of a map), try the value without each argument. Tests pass it when they never have two values in one place that differ in that argument alone: `fixtures/keyed` keeps texts by language and page, tests both languages with both pages, and still passes with a key that leaves the language out, because no store in the tests holds two languages. Where the tests write and read such values through methods with literal arguments, a value one test writes is read with another test's value for that argument, in one place: two lines that tell the two keys apart. Otherwise the report says what kind of test to add.
- When a test checks an integer codec by round trip, try changing the byte order on both sides. TMT checks that the selected tests detect each component change alone and pass with both changes. A fixed byte array on the encoding side can distinguish the implementations.

```sh
cargo tmt fixtures/shapes --rebuild --lines src/lib.rs:4-9 --lines src/lib.rs:28-29
cargo tmt fixtures/operators --rebuild --lines src/lib.rs:72-74
cargo tmt fixtures/distinguishing --rebuild
cargo tmt fixtures/batch-store --rebuild --lines src/lib.rs:33
cargo tmt fixtures/rounding --rebuild
cargo tmt fixtures/keyed --rebuild
```

### A change undone

With `--since REV` and one of the rebuild modes, the old code is a mutant too. Each hunk of the diff in the selected production code is put back as it was at `REV`, built, and run against all tests. When they pass, the tests do not tell the change from what was there before:

```text
$ cargo tmt --rebuild --since "$(git merge-base origin/main HEAD)"
UNTESTED CHANGE  src/lib.rs:9:1
    now - used < 30 && created <= used
  passes every test with the code as it was: `let _ = used; now - created < 30`
  no test tells this change from the code as it was at 91e1dc927360: add one that fails without it
```

Here a session's age is now counted from its last use, and every session in the tests was last used when it was made. A test with a session used later fails on the old code and closes the finding.

- Left out: hunks that change no token (comments, documentation, spacing), and hunks that only touch test code.
- A hunk that does not build alone counts as a mutant that does not compile. When a file has such a hunk and others, all of the file's changes are also undone together, and after that all changes of all files.
- A change that is a refactoring passes with the old code by design. Silence it with `// tmt:ignore <reason>` on its first line, or keep it in the baseline.
- Without a rebuild mode the changes are counted among the mutants that need a build of their own. They are not tried with `--shard` or `--list`.
- In JSON the finding has `kind: "survivor"` and `change_since` with the revision.

### Tests that changed

With `--since REV`, a change to the tests of a file (`#[cfg(test)]` code and `#[test]` functions in it) puts the whole program of that file in scope, not only its changed lines. In the lines that did not change, a mutant that passes is reported only if the tests as they were at `REV` found it out. They are built once more, with the program as it is, and run against those mutants:

```text
INPUT GAP  src/lib.rs:27:5
    age >= 18
  passes every test as: `age >= 19`, `age >= 20`  [re-run: tests pass]
  nearest values the tests passed: `age` = 17, 20
  add a test with `age` = 18 (the original gives true)
  the tests as they were at e76c0bd70fff found this out: a change to the tests lets it through
```

- This needs no rebuild mode: it costs two more builds, and only when tests changed.
- Gaps that were there before the change are not reported in such lines; a run of the whole reports them.
- When the earlier tests do not build with the program as it is, or fail on it (the program changed with them), they cannot say what they found out, and only the changed lines are looked at.
- Code that a deleted test leaves with no test at all is not reported here (it is unreached: see `--show-unreached` on a run of the whole). Tests in files of their own (`tests/*.rs`) are not followed back to the code they test.
- In JSON these findings are listed under `weakened`.

Output: text by default, `--json`, or `--sarif` (SARIF 2.1.0 for code scanning: paths from the top of the git repository, the finding's id as `partialFingerprints`, and what the run looked at under the run's `properties`; with `--workspace` there is one run per package, each with an `automationDetails.id` of its own, and a package whose analysis failed is a run with `executionSuccessful: false` and the error). The JSON report has the same under `analysis`: package, `--since` and `--lines`, features, and whether mutants were built and predictions re-run. `--show-unreached` lists sites with no recorded execution. Long campaigns print completed, failed, timed-out, survived and unevaluated counts on stderr. `--quiet` suppresses progress. If a baseline test fails, stderr includes the available diagnostics and the full log path.

Exit status: `0` no gaps, `1` analysis error (such as failing unmutated tests), `2` gaps or an unstable prediction, and `3` work selected but no mutations executed. In source-only mode, `3` also takes precedence when a mutant times out or its source was not compiled. Code under a `cfg` that the features of the build turn off is reported as `source_not_built` too (the features are the ones cargo reports for the build; a `cfg` that depends on anything else is taken to be on), and the text output says how many mutants that holds for. A scoped selection with zero sites skips the build and baseline. JSON and SARIF expose `evaluation` as `no_sites` or `no_mutants_executed` for those cases. Structured `skipped` records include their source span and reason, with a compiler diagnostic for repaired probes.

Your source tree is never modified. In a git repository only the files git does not ignore are copied (submodules by their own rules), so build output, caches and local files kept out of the repository stay where they are. The copy contains no symbolic links: a linked file is copied as a file, and a linked directory is followed only if it lies inside the workspace. The workspace is copied to `<target dir>/tmt/src` (or `--work-dir`) and built in a `target` directory next to that copy. Cargo intermediates also stay in that directory, even if `build.build-dir` is configured for ordinary builds. Path dependencies that point outside the workspace are rewritten to absolute paths in the copy.

## How it works

1. **Instrument.** Every comparison, boolean operator, arithmetic operator and deletable statement (a call, a `?` check, an assignment) in the crate is rewritten into a probe. All mutants are compiled into the same binary and switched on by an environment variable, so the crate is built once.
2. **Trace.** The tests run once. Each probe records, per test, what it saw. For integer comparisons this is the difference `lhs - rhs` nearest to zero on each side, and whether the operands were ever equal.
3. **Plan.** An integer comparison is a threshold on `lhs - rhs`. Every threshold between the nearest observation below and the nearest observation above gives the same result on all observed values, so every test executes such a mutant exactly as it executes the original. Those mutants are input gaps, found by arithmetic. By default one of them is re-run to confirm (`--no-verify` skips that).
4. **Run.** The remaining mutants change the outcome of some observation. Each is run only against the tests whose observations it changes. If they all pass, no assertion noticed: an assertion gap.

5. **Build the rest one by one.** Some code cannot carry a probe: nothing in a `const fn` or in the value of a `const` or `static` can ask at run time which mutant is on, a function that returns `impl Iterator` cannot return an iterator of another type beside its own, two literals (`60 * 60`) have no type of their own for a probe to work with, and for a compound assignment to more than a variable (`*map.entry(k).or_insert(0) += 1`) the order of evaluation depends on the type. The mutants of such code, and of every site whose probe did not compile, are written into the source, built and run against all tests, one after the other, the way cargo-mutants does it. This is the slow part, so it is done with `--rebuild` or `--source-only`; without it the summary says how many mutants were left out.

Mutants that cannot differ from the original within the operand types (`x >= 0` for an unsigned `x`) are not reported.

### What is mutated

| Code | Mutants | Guaranteed to survive when |
|---|---|---|
| `a < b`, `a >= 18` on integers | the constant shifted by any amount, the operator negated | no test passes a value between the old and the new boundary |
| `x > 30.5` on floats | `>=`, and any threshold between the two nearest observed values | the operands were never equal in a test |
| comparisons of other types | `<` and `<=` swapped, negation | the operands were never equal in a test |
| `a == b`, `a != b` | negation, `true` / `false` | the result is the same in every test |
| `a && b`, `a \|\| b` | one operand's value ignored (`a && { b; true }`, `{ a; b }`), the other operator | the ignored operand never decided the result in a test |
| `a + b`, `-`, `*`, `/`, `%` on primitive numbers, and `+=` etc. | two other operators each: `+` to `-` and `*`, `-` to `+` and `/`, `*` to `/` and `+`, `/` to `*` and `%`, `%` to `/` and `+` | the two operators agree on every pair of operands the tests pass (`fee` is always 0, `factor` is always 1) |
| `a & b`, `\|`, `^`, `<<`, `>>` on integers, and `&=` etc. | `&` to `\|` and `^`, `\|` to `&` and `^`, `^` to `&` and `\|`, `<<` and `>>` swapped | as above |
| `!x` on `bool` | `x` | never: always run |
| `-x`, `-1` | `x`, `1` | never: always run |
| any function | its body replaced by each value its return type suggests, without running it: `()`, `0` / `1` / `-1`, `true` / `false`, `String::new()` / `"xyzzy".into()`, `None` / `Some(..)`, `Ok(..)`, `vec![]` / `vec![..]`, empty and one-element collections, `Box::new(..)`, tuples and arrays of those, `Default::default()` for the rest | never: always run |
| the guard of a `match` arm | `true`, `false` | the guard had the same value in every test |
| a field of a struct literal with a base (`S { depth, ..base }`) | the field left to the base | never: always run |
| a function returning `bool` | the value at its normal exit replaced by `true` / `false` | the function returned the same value in every test |
| `a.max(b)`, `a.min(b)` on numbers | the other of the two | the operands were equal in every test |
| `a..b`, `a..=b` on integers | the end included or excluded | never: always run |
| a function returning `Option<T>` | `None` at its normal exit | the function returned `None` in every test |
| an arm of a `match` that ends in a catch-all | the arm removed, so the value falls through | never: always run |
| `f(x);`, `check()?;`, `x += 1;`, `x = y;` | the statement deleted | never: these are always run |

Arithmetic on other types (`String + &str`, `Duration + Duration`) and `!` on anything but `bool` are left as they are.

A finding is not repeated by what encloses it: if `a > 0` is true in every test, the report says so once, and does not add that `a > 0 && b` has an operand that never decides.

Code in the arguments of `assert_eq!`, `assert_ne!`, `vec!`, `format!`, `print!`, `println!`, `eprint!`, `eprintln!`, `write!`, `writeln!`, `panic!`, `unreachable!`, `todo!`, `unimplemented!`, `format_args!` and `matches!` is mutated like any other code. `assert!`, `debug_assert!` and `dbg!` put the source text of their argument into their output, so their arguments are not touched.

A probe that does not compile at some site is dropped and the build is retried. With `--rebuild` the source mutants of that site are built one by one (step 5). The summary reports skipped sites and candidates that need a source build. A value that does not have the function's return type (`Default::default()` for a type without a default) is not a mutant; the summary counts those as left out.

Functions, `impl` blocks and modules marked `#[mutants::skip]` (also inside `cfg_attr`) are not mutated.

### Compared with cargo-mutants

For `<` and `>`, `==` and the reverse operator are run as well. Only a replacement that answers like the original for everything the tests saw is not run (it passes them all, and the gap says so). For `<=` and `>=` the negation is run. On integers that is `>` and `<`. On anything else it is not: for operands without an order (NaN) the negation is true and the operator false, so there the operator is run as a mutant of its own, whether or not the tests gave such operands (a mutant that changes the way the code goes can be the first to). Two mutants that both change an outcome are never taken for one, even where they answer alike for all that was seen: once the code goes another way, the comparison meets values the tests never gave it.

The repository compares TMT with stored cargo-mutants 27.1.0 mutation lists for eight fixture crates: `tests/parity.rs` checks the stored lists (`tests/data/cargo-mutants`) against the sites this tool finds, and names the exceptions with their reason (a `||` inside a let chain, which does not compile). An empty body is not made for a function that consists of one statement, because deleting that statement is the same mutant. The same test checks any other crate on request (see its last function); that is how the table above was completed against real code. On top of those come the mutants that need the trace: shifted boundaries, an ignored operand of `&&` / `||`, a result replaced after the body ran, deleted statements, `min` / `max`, range ends.

The difference is in how they are run. cargo-mutants builds every mutant and runs all tests against it. Here one build carries all mutants that a probe can carry, each is run only against the tests that execute it, and those that provably pass every test are not run at all but reported with the input that is missing. Only the rest (step 5) costs a build each, and is run when `--rebuild` asks for it: the summary reports how many TMT candidates require that option. The fixture comparison checks the family and location of a candidate. Exact source edits are visible with `--list --json`.

## What "guaranteed to survive" rests on

An input gap is a statement about the mutant the tool actually runs, under these assumptions:

- **Deterministic tests.** The prediction is made from one run of the tests. By default the tests are run again against the listed survivors; if that run fails, the finding is reported as `UNSTABLE` and the exit status is non-zero.
- **The mutant keeps evaluating what the original evaluates.** A comparison replaced by `true` still evaluates both operands; `a && { b; true }` still runs `b` when `a` is true. The report shows these forms rather than the shorter `a`, because a test can notice that `b` no longer runs.
- **`PartialOrd` is consistent.** While tracing, operands are compared once with `partial_cmp` and `<`, `<=`, `>`, `>=` are derived from the result. A type whose `lt` disagrees with its `partial_cmp` (which the trait forbids) is not handled.
- **Integer mutants are computed without overflow.** `a >= b - 1 (without overflow)` means the comparison of the mathematical values; written in the operands' type it could overflow.
- **Float thresholds are computed on the difference in `f64`.** The printed threshold (`celsius > 35.25`) is the same thing up to rounding; for `f32` operands it can differ in the last bit.
- **Overflow and division by zero are never assumed equal.** An arithmetic mutant is a guaranteed survivor only where both operators are defined on every observed pair of operands and agree.

The probes are written to leave the unmutated program alone: every operand is evaluated once, in the original order, and its temporaries are dropped where the original drops them. `fixtures/semantics` holds tests that would fail otherwise. Where this cannot be guaranteed from the syntax, the code is not probed: a compound assignment to anything but a plain variable, field or dereference (`v[i] += x`), and the arguments of a macro that the file defines itself under a standard name.

## Formal artifacts

- `formal/lean/Planner.lean`: over mathematical integers and a complete set of observations, the survivor range is sound (every threshold in it agrees with the original on all observations), exact (every threshold outside it is separated by an observation), and the three-value trace summary is enough to decide it. For an expression that cannot be every number: the thresholds between the nearest values it can be are the original for all of them, the others are told apart by one of those two values, and a gap with no such value between the observations has no mutant; a product by a power of two stays a multiple when it wraps, and one by another number need not. Checked with Lean 4 core.
- `formal/pict/planner.pict`: 3-way combinations of operator, observation shape, literal position and type range. The generated table (`tests/data/planner.pict.tsv`) is replayed against a brute-force oracle in `plan::tests::pict_table`.
- `formal/tla/RepairLoop.tla`: the instrument, build, disable-failing-probes loop terminates and never ships a probe that does not compile. Checked with TLC. The model covers compile errors only; that a probe which compiles also behaves like the code it replaces is what `fixtures/semantics` tests.
- `formal/tla/JobPool.tla`: the parallel runner runs every mutant exactly once and has every result when the workers stop. Checked with TLC.
- `formal/tla/CampaignPhases.tla`: baseline and mutant workers stay within their limits, and provisional parallel timeouts stay out of the record until resolved by policy. Checked with TLC.
- `formal/tla/CampaignCheckpoint.tla`: completed outcomes are reused only for the same context and candidate inventory. Checked with TLC.
- `formal/tla/TraceBacklog.tla`: the observations a thread keeps back while recording. Nothing a thread knows is anywhere but in the trace, in its backlog or on its way, and when every thread has ended the trace has the nearest distance. Three wrong variants (no write at the end of a thread, a backlog that keeps the first value, news that is dropped) are told apart. Checked with TLC.
- `formal/tla/WorkCopy.tla`: the working copy across runs. Whatever earlier runs left behind, and wherever one was interrupted, the build of a run sees exactly the files that run wants, and instrumented text is never in the copy without the log naming it. Checked with TLC.
- `formal/tla/TargetCoverage.tla`: a source mutant with no compiled file is unevaluated, and a changed file is restored before its result is recorded. Checked with TLC.
- `formal/pict/workcopy.pict`: what a file was after the last run, what the user did to it since, and whether this run instruments it. The table (`tests/data/workcopy.pict.tsv`) is replayed in `driver::tests::work_copy_table`.

```text
lean formal/lean/Planner.lean
pict formal/pict/planner.pict /o:3 /e:formal/pict/planner.seed.tsv > tests/data/planner.pict.tsv
java -jar tla2tools.jar formal/tla/RepairLoop.tla
```

## Limits

- Each package contributes its production libraries and binaries. TMT follows explicit Rust module declarations and literal `include!` paths, and analyses a file included by multiple targets once per package. Workspace runs select multiple packages. The direct runner sets the libtest thread count, and the isolated runner uses one test per process. Nextest configuration applies when using the Unix source-only nextest runner.
- Probe recording and probe mutants need the environment variables passed to the process under test. A subprocess that clears them can execute code without recording it. Use `--source-only` for those sites.
- Secondary targets and modules shared by targets need a source build. Run with `--rebuild` or `--source-only` to evaluate them. Cargo artifacts and rustc dependency files identify source files that were absent from the build. Their candidates are reported as unevaluated. This check is at file granularity. Use the feature and target settings that enable the Rust items you want to test.
- Source files that start with a byte order mark or a `#!` line are not instrumented.
- A macro that shadows a standard one is only recognised when it is defined at the top level of the same file.
- Code in the arguments of macros other than the ones listed above and in test code is not mutated. Supported mutations in inline const blocks, array lengths, enum discriminants, const generic expressions, `const fn` and constant initializers need a source build.
- Casts are not mutated. Result probes for `bool` or `Option` require a body whose exits can all be observed. TMT omits them when a `return`, `?` or unexpanded macro could bypass the probe. Whole-body mutations are still available.
- The body of a function that returns an `impl Trait` other than an iterator is not replaced: no value of the hidden type can be made up.
- Two mutants can be the same program: a body replaced by `true` and a result that is always `true`. Both are reported, on the same line.
- For floats, a gap is reported only while no test compares equal operands. After that, thresholds within rounding distance of the constant still survive and are not reported.
- Strings and other non-numeric types have no notion of "next value": only `<` versus `<=` is checked.
- The survivor prediction assumes deterministic tests. A wrong prediction shows up as `UNSTABLE`.
- Equivalent mutants are not always detected. If `max(a, b)` is written with `a >= b`, then `a > b` is reported although no test could observe it. The same goes for an operand that is constant by construction (`is_large(n) && FEATURE_ENABLED`).
- Observations made on threads that a test spawns, and all observations of doc tests, cannot be attributed to a single test; the mutant is then run against the whole test binary.
- A thread without a name, and the main thread, write every observation at once (slow in a loop); a thread with a name keeps distances back as described under "Speed". If such a thread is still running when the process exits, what it observed in its last 20 ms can be missing from the trace, as anything it would have observed a moment later is. A survivor predicted from that is re-run like every other and shows up as `UNSTABLE` if the prediction was wrong.
- A comparison, a guard or an operand that a mutant replaces by a constant is still evaluated, and so is a field's value when the mutant takes the field from the base (which is then evaluated twice). The source edit for a field removes it from the update and evaluates the existing base expression once. Use `--source-only` when the test needs to distinguish these evaluation effects.
- A probe keeps the order of evaluation and the life of temporaries of the code it stands for, with two exceptions that need unusual code to be seen: a temporary with a destructor inside the value of a field of a struct literal with a base (`S { x: guard().value(), ..base }`) is dropped before the next field is evaluated, and `*p += f()` on a number takes the place before it calls `f`, which a `DerefMut` with side effects can observe.
- A thread that a test names exactly like another test of the same binary is taken for that test, and mutants are then run against the wrong test. `--audit` shows it.
- A replaced operator in a mutant that is built keeps its operands by parentheses where the new operator binds otherwise (`8 - (2 + 3)`). A crate that denies `unused_parens` rejects those where they turn out not to be needed; they are then counted as not compiling.
- The end of a range is taken in or left out by a probe only where a `for` runs through the range directly: there `a..b + 1` gives the loop what `a..=b` would. A range that is kept, indexed with or given to a method is another type with its end taken in, and that mutant is one of those built with `--rebuild`. So is every range of two literals (`0..3`), whose type a probe could not know.
- Even in a `for`, `a..b` cannot be made into `a..=b` by a probe where `b` is the largest value of its type. Where a test runs such a range the mutant is not run, and a run that only gets there with the mutant on is given up; the summary says how many mutants were not evaluated for that. With `--rebuild` they are written into the source and run as what they are. A run is given up by taking away a file the driver made and named (by its full path, in the working directory of the analysis, which no other run uses), and by that alone; taking a file away works on a full disk too: it is known also where the code runs in a program that a test started and whose failure the test takes for its own, and a test that ends with the same status as the runtime is still a test that failed. In doc tests a run that is given up counts as a failed one.
- The probe of `a.max(b)` / `a.min(b)` borrows `a` and calls the method on it as written. For a type that cannot be copied and whose method takes it by value (`String`), or that has only one of the two methods, the probe does not compile and the site is left as it is.
- Code that a test runs in the child of a `fork` before its `exec` (a `pre_exec` hook, for example) is not traced: the child has the locks of the tracing runtime as they were at the `fork`, and could wait for ever on one that a thread it does not have was holding. A site that only such code reaches is listed as not reached. A mutant still acts there.
- A timeout, and the end of every run of the tests, stops the process group of the tests. A program that the tests started in a process group or session of its own is not in it, and with a mutant the test that would have stopped it often fails first. So each run is given a locked file that it passes on to whatever it starts, and whoever still has that file open when the run is over is stopped (the summary on stderr says how many). Set `TMT_LEAVE_PROCESSES=1` to leave them running. Not found this way: a program that closes the files it was given (a daemon that detaches itself properly, or a child of Python's `subprocess`, which closes them by default). Finding who has the file needs `/proc` on Linux and `lsof` elsewhere.
- A mutant that was built on its own (`--rebuild`) and passes every test is reported as `SURVIVOR`: nothing is recorded for such code, so it is not known whether a test runs it at all.
- With `--jobs N` each test binary gets its share of the cores (never fewer than two test threads), and the default recheck policy reruns a mutant that timed out in parallel on its own and uses the second verdict. A machine that is loaded by something else can still turn a slow mutant into a timeout (counted as killed, and said in the summary).

## Speed

- The crate is built once; mutants are switched on by an environment variable.
- A mutant is run only against the tests whose observations it changes, and the run stops at the first test that fails.
- A mutant that makes a loop endless is stopped after 5 seconds plus five times what its test binary takes without recording. The summary says how many were stopped that way; each of them costs that limit.
- Probes cost time where tests spend theirs in loops. In an unoptimised build (what `cargo test` makes) a probe that has nothing to do is a load, a compare and the operation itself, but nothing around it is inlined: a loop of plain arithmetic and comparisons took 13 times as long with probes as without, and 170 times as long while recording. `--opt-level 1` removes nearly all of it (the same loop: as fast as without probes, and 13 times as long while recording), at the price of a slower build and of tests that are not built exactly as `cargo test` builds them.
- While recording, a probe that sees nothing new does not take the recorder's lock, and a distance that only comes closer to zero (a loop counting towards its bound, in every round) is not written each time but kept and written when the thread ends, when any other thread ends, and every 20 ms.
- The working copy is kept between runs. A file is rewritten only when its instrumented text changed, so cargo rebuilds what changed and nothing else.
- Mutants that need a build of their own (step 5) cost a build and a full test run each. Their number is printed before they start.
- `--since` and `--lines` cut the work down to the code under review.

## Evaluating against other tools

A mutation score is not comparable across tools whose mutants differ in difficulty. The comparison this project aims for is: with the same CI time, how many real past bugs (not shown to the tool) are caught by the tests it led you to write.

## Prior work

The idea of generating mutants with knowledge of the tests is called *test-aware mutant generation*. Related lines of work: SEMu (symbolic execution to kill surviving mutants), DSpot (assertion amplification), the W-method (complete test suites for finite state machines) and Meta's ACH (LLM-generated mutants with tests that kill them). This tool is the solver-free, AI-free corner of that space: it reads the test trace and does interval arithmetic.

## Development

```text
cargo test          # unit tests, PICT tables, end-to-end runs on fixtures/, parity with cargo-mutants
cargo test -- --ignored   # also: every mutant of four fixtures built from the source and compared with its probe (minutes)
cargo run -- .      # the tool on itself
```

The lists in `tests/data/cargo-mutants` are `cargo mutants --list` run in a copy of each fixture.

### Releasing

Update `Cargo.toml` and `Cargo.lock`, then push the release commit to `main` and wait for CI to pass. Run the [Release workflow](https://github.com/t-k/targeted-mutation/actions/workflows/release.yml) manually to verify the package. To publish, push a tag matching the package version, such as `v0.1.1`. The tag run checks that the tagged commit is on `main` and has a successful CI run (the release workflow itself runs no tests), verifies the package again and publishes it to crates.io using Trusted Publishing.

## License

Apache License, Version 2.0. See [LICENSE](LICENSE).
