# targeted-mutation

[![Crates.io version](https://img.shields.io/crates/v/targeted-mutation.svg)](https://crates.io/crates/targeted-mutation)

**Targeted mutation testing** generates the realistic wrong implementations that your existing tests would let through, and tells you which input or assertion would catch them.

Classic mutation testing changes `<` to `<=` and then asks whether a test fails. This tool works the other way round: it first watches what the tests actually feed the code, and from that computes the mutants that are guaranteed to pass, without running them. It needs no AI and no solver.

The command is `cargo tmt`. It works on Rust crates.

## What it reports

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
- `--since REV`: only the code changed since a git revision, plus untracked files. This is the mode for a pull request: `cargo tmt --since "$(git merge-base origin/main HEAD)"` (the merge base, so that what others added to the main branch meanwhile is not taken for yours). A run of a part can tell that a finding is new; it cannot tell that one outside of it is gone, which takes a run of the whole.
- `--lines FILE:FROM-TO`: only these lines (repeatable; `FILE` is relative to the package).

Which tests to use:

- `--also-test PACKAGE`: also run the tests of another workspace package (repeatable).
- `--dependents`: also run the tests of every workspace package that depends on the analysed one. A gap that an integration test elsewhere closes is then not reported.
- `--doc`: also run the doc tests. rustdoc recompiles them for every mutant they reach, so this is slow.
- `--isolate-tests`: run each selected libtest test in its own process. `--test-target NAME` selects Cargo test executable names, and `--include-test NAME` and `--exclude-test NAME` select exact libtest names. These flags are repeatable and enable isolation. `--include-ignored` also allows ignored tests. Keep the default `--jobs 1` when tests share ports or files.
- `--test-wrapper PATH`: in isolated mode, run a workspace-relative executable from the working copy with the test executable and its arguments. A wrapper can set up a private temporary directory and then run the supplied command, preserving its status and the `TMT_*` environment. The report records the selected tests and wrapper. Isolated selection works with standard libtest executables and does not run doc tests.

Findings you have already dealt with:

- A comment `// tmt:ignore <reason>` on the line, or on the line above, silences the findings of that line.
- `--write-baseline FILE` records the findings of a run; `--baseline FILE` leaves the recorded ones out. A finding is known by its `id`: its kind, file, the function or constant it is in, its code, and which of the places with that code in that item it is. No line numbers and none of the words that describe the mutants are in it, so it survives code moving around and this tool wording things differently. (All findings of one kind at one place share an id.) Files written before version 2 of the format, which list fingerprints, are still read. Suppressed findings are counted in the summary and do not affect the exit status.

Running:

- `--jobs N`: run N mutants at the same time (default 1). Raise it only if the tests can run concurrently with themselves: no fixed ports, no shared files.
- `--work-dir DIR`: where the working copy and its build go. It must be a directory of its own: the tool deletes files from the copy inside it, and refuses a directory that contains the workspace or a `src` it did not create.
- `--all-files`: copy files that git ignores too, subject to the default exclusions for Git metadata, `node_modules` and build caches.
- `--copy-fixture PATH`: copy a particular workspace-relative file or directory even if ignored, for example `--copy-fixture sdk/node_modules`. Prepare it before running tmt. Links are resolved only within the workspace and excluded paths are protected.
- `--git-index`: give tests an isolated Git index for commands such as `git ls-files`. The copy has its own metadata. History, remotes and staged blob contents are unavailable.
- `--no-verify`: trust the trace; do not re-run tests against predicted survivors.
- `--no-effects`: do not trace surviving mutants to say what they change. (Where several mutants survive at one place, one of them is followed through the tests; the report says which, in the text and as `effects_of`.)
- `--features A,B`, `--all-features`, `--no-default-features`, `--locked`: as for `cargo test`, and told to every cargo command of the run.
- `--audit N`: check the run itself, three ways. Up to N of the surviving mutants (spread evenly, the same ones every time) are run once more against every test of every binary, not only the tests chosen for them; one that fails is counted as found out and listed. Up to N of the mutants that were run as probes are also built from the source, as a person would write them, and run against all tests; where the result is not the one of the probe, both are listed (a build and a full test run each: slow). The summary says how many were chosen, how many could be compared, and how many could not (they did not build, or their probe run was given up). A negated comparison that no mutant of the source stands for is compared with the negation written out, `!(a >= b)`. And the tests are recorded a second time; binaries whose two recordings differ are listed, since their tests do not always do the same.
- `--opt-level N`: build the tests at optimisation level N (0 to 3) instead of the profile's own. Worth it when tests spend their time in loops (see "Speed").
- `--rebuild`: also run the mutants that need a build of their own (see "How it works", step 5). Without it the summary says how many were left out.
- `--source-only`: build supported mutants directly from source and run them against the selected tests. Scope it with `--lines` or `--since` when tests launch children with `Command::env_clear()` or otherwise omit the probe environment. This mode is slower and reports surviving source mutants without trace-based input or assertion suggestions.

Output: text by default, `--json`, or `--sarif` (SARIF 2.1.0 for code scanning: paths from the top of the git repository, the finding's id as `partialFingerprints`, and what the run looked at under the run's `properties`). The JSON report has the same under `analysis`: package, `--since` and `--lines`, features, and whether mutants were built and predictions re-run. `--show-unreached` lists sites with no recorded execution. Long campaigns print completed, failed, timed-out, survived and unevaluated counts on stderr. `--quiet` suppresses progress. If a baseline test fails, stderr includes the available diagnostics and the full log path.

Exit status: `0` no gaps, `2` gaps found or a prediction failed its re-run (`UNSTABLE`), `1` the analysis could not run (tests fail on the unmutated code, the crate does not build, ...), and `3` sites were selected but no mutants executed and no gaps were reported. A scoped selection with zero sites skips the build and baseline. JSON and SARIF expose `evaluation` as `no_sites` or `no_mutants_executed`. Structured `skipped` records include their source span and reason, with a compiler diagnostic for repaired probes.

Your source tree is never modified. In a git repository only the files git does not ignore are copied (submodules by their own rules), so build output, caches and local files kept out of the repository stay where they are. The copy contains no symbolic links: a linked file is copied as a file, and a linked directory is followed only if it lies inside the workspace. The workspace is copied to `<target dir>/tmt/src` (or `--work-dir`) and built in a `target` directory next to that copy. Cargo intermediates also stay in that directory, even if `build.build-dir` is configured for ordinary builds. Path dependencies that point outside the workspace are rewritten to absolute paths in the copy.

## How it works

1. **Instrument.** Every comparison, boolean operator, arithmetic operator and deletable statement (a call, a `?` check, an assignment) in the crate is rewritten into a probe. All mutants are compiled into the same binary and switched on by an environment variable, so the crate is built once.
2. **Trace.** The tests run once. Each probe records, per test, what it saw. For integer comparisons this is the difference `lhs - rhs` nearest to zero on each side, and whether the operands were ever equal.
3. **Plan.** An integer comparison is a threshold on `lhs - rhs`. Every threshold between the nearest observation below and the nearest observation above gives the same result on all observed values, so every test executes such a mutant exactly as it executes the original. Those mutants are input gaps, found by arithmetic. By default one of them is re-run to confirm (`--no-verify` skips that).
4. **Run.** The remaining mutants change the outcome of some observation. Each is run only against the tests whose observations it changes. If they all pass, no assertion noticed: an assertion gap.

5. **Build the rest one by one.** Some code cannot carry a probe: nothing in a `const fn` or in the value of a `const` or `static` can ask at run time which mutant is on, a function that returns `impl Iterator` cannot return an iterator of another type beside its own, two literals (`60 * 60`) have no type of their own for a probe to work with, and for a compound assignment to more than a variable (`*map.entry(k).or_insert(0) += 1`) the order of evaluation depends on the type. The mutants of such code, and of every site whose probe did not compile, are written into the source, built and run against all tests, one after the other, the way cargo-mutants does it. This is the slow part, so it is done only with `--rebuild`; without it the summary says how many mutants were left out.

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

A probe that does not compile at some site is dropped and the build is retried. With `--rebuild` the mutants of that site are then built one by one (step 5), and only a site that has no mutant in source form (a deleted `let x; x = ...;`) is counted as skipped in the summary line; without it every such site is. A value that does not have the function's return type (`Default::default()` for a type without a default) is not a mutant; the summary counts those as left out.

Functions, `impl` blocks and modules marked `#[mutants::skip]` (also inside `cfg_attr`) are not mutated.

### Compared with cargo-mutants

For `<` and `>`, `==` and the reverse operator are run as well. Only a replacement that answers like the original for everything the tests saw is not run (it passes them all, and the gap says so). For `<=` and `>=` the negation is run. On integers that is `>` and `<`. On anything else it is not: for operands without an order (NaN) the negation is true and the operator false, so there the operator is run as a mutant of its own, whether or not the tests gave such operands (a mutant that changes the way the code goes can be the first to). Two mutants that both change an outcome are never taken for one, even where they answer alike for all that was seen: once the code goes another way, the comparison meets values the tests never gave it.

Every mutant that cargo-mutants 27.1.0 lists for the crates in `fixtures/` has a counterpart here: `tests/parity.rs` checks the stored lists (`tests/data/cargo-mutants`) against the sites this tool finds, and names the exceptions with their reason (a `||` inside a let chain, which does not compile). An empty body is not made for a function that consists of one statement, because deleting that statement is the same mutant. The same test checks any other crate on request (see its last function); that is how the table above was completed against real code. On top of those come the mutants that need the trace: shifted boundaries, an ignored operand of `&&` / `||`, a result replaced after the body ran, deleted statements, `min` / `max`, range ends.

The difference is in how they are run. cargo-mutants builds every mutant and runs all tests against it. Here one build carries all mutants that a probe can carry, each is run only against the tests that execute it, and those that provably pass every test are not run at all but reported with the input that is missing. Only the rest (step 5) costs a build each, and is run when `--rebuild` asks for it: with that option every mutant of cargo-mutants is run, without it the summary says how many were not.

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

- `formal/lean/Planner.lean`: over mathematical integers and a complete set of observations, the survivor range is sound (every threshold in it agrees with the original on all observations), exact (every threshold outside it is separated by an observation), and the three-value trace summary is enough to decide it. Checked with Lean 4 core.
- `formal/pict/planner.pict`: 3-way combinations of operator, observation shape, literal position and type range. The generated table (`tests/data/planner.pict.tsv`) is replayed against a brute-force oracle in `plan::tests::pict_table`.
- `formal/tla/RepairLoop.tla`: the instrument, build, disable-failing-probes loop terminates and never ships a probe that does not compile. Checked with TLC. The model covers compile errors only; that a probe which compiles also behaves like the code it replaces is what `fixtures/semantics` tests.
- `formal/tla/JobPool.tla`: the parallel runner runs every mutant exactly once and has every result when the workers stop. Checked with TLC.
- `formal/tla/TraceBacklog.tla`: the observations a thread keeps back while recording. Nothing a thread knows is anywhere but in the trace, in its backlog or on its way, and when every thread has ended the trace has the nearest distance. Three wrong variants (no write at the end of a thread, a backlog that keeps the first value, news that is dropped) are told apart. Checked with TLC.
- `formal/tla/WorkCopy.tla`: the working copy across runs. Whatever earlier runs left behind, and wherever one was interrupted, the build of a run sees exactly the files that run wants, and instrumented text is never in the copy without the log naming it. Checked with TLC.
- `formal/pict/workcopy.pict`: what a file was after the last run, what the user did to it since, and whether this run instruments it. The table (`tests/data/workcopy.pict.tsv`) is replayed in `driver::tests::work_copy_table`.

```text
lean formal/lean/Planner.lean
pict formal/pict/planner.pict /o:3 /e:formal/pict/planner.seed.tsv > tests/data/planner.pict.tsv
java -jar tla2tools.jar formal/tla/RepairLoop.tla
```

## Limits

- Rust only, one package per run. The direct runner sets the libtest thread count, and the isolated runner uses one test per process. tmt does not load nextest configuration or Cargo target runners. Use the explicit selection and wrapper flags for suites with extra requirements.
- Probe recording and probe mutants need the environment variables passed to the process under test. A subprocess that clears them can execute code without recording it. Use `--source-only` for those sites.
- A module that two targets of the package both declare (`mod common;` in `lib.rs` and in `main.rs`) cannot be probed; its sites are counted as skipped.
- Source files that start with a byte order mark or a `#!` line are not instrumented.
- A macro that shadows a standard one is only recognised when it is defined at the top level of the same file.
- Code in the arguments of macros other than the ones listed above, in array lengths, enum discriminants, `const { }` blocks and const generic arguments, and in test code is not mutated. (`const fn` and the values of constants are: step 5.)
- Casts are not mutated. The result of a `bool` or `Option` function is replaced only at its normal exit, not at an early `return` (the replaced body covers that case).
- The body of a function that returns an `impl Trait` other than an iterator is not replaced: no value of the hidden type can be made up.
- Two mutants can be the same program: a body replaced by `true` and a result that is always `true`. Both are reported, on the same line.
- For floats, a gap is reported only while no test compares equal operands. After that, thresholds within rounding distance of the constant still survive and are not reported.
- Strings and other non-numeric types have no notion of "next value": only `<` versus `<=` is checked.
- The survivor prediction assumes deterministic tests. A wrong prediction shows up as `UNSTABLE`.
- Equivalent mutants are not always detected. If `max(a, b)` is written with `a >= b`, then `a > b` is reported although no test could observe it. The same goes for an operand that is constant by construction (`is_large(n) && FEATURE_ENABLED`).
- Observations made on threads that a test spawns, and all observations of doc tests, cannot be attributed to a single test; the mutant is then run against the whole test binary.
- A thread without a name, and the main thread, write every observation at once (slow in a loop); a thread with a name keeps distances back as described under "Speed". If such a thread is still running when the process exits, what it observed in its last 20 ms can be missing from the trace, as anything it would have observed a moment later is. A survivor predicted from that is re-run like every other and shows up as `UNSTABLE` if the prediction was wrong.
- A comparison, a guard or an operand that a mutant replaces by a constant is still evaluated, and so is a field's value when the mutant takes the field from the base (which is then evaluated twice). A test that only notices that a call inside them no longer happens does not count against these mutants; built with `--rebuild`, a guard or a field is replaced outright.
- A probe keeps the order of evaluation and the life of temporaries of the code it stands for, with two exceptions that need unusual code to be seen: a temporary with a destructor inside the value of a field of a struct literal with a base (`S { x: guard().value(), ..base }`) is dropped before the next field is evaluated, and `*p += f()` on a number takes the place before it calls `f`, which a `DerefMut` with side effects can observe.
- A thread that a test names exactly like another test of the same binary is taken for that test, and mutants are then run against the wrong test. `--audit` shows it.
- A replaced operator in a mutant that is built keeps its operands by parentheses where the new operator binds otherwise (`8 - (2 + 3)`). A crate that denies `unused_parens` rejects those where they turn out not to be needed; they are then counted as not compiling.
- The end of a range is taken in or left out by a probe only where a `for` runs through the range directly: there `a..b + 1` gives the loop what `a..=b` would. A range that is kept, indexed with or given to a method is another type with its end taken in, and that mutant is one of those built with `--rebuild`. So is every range of two literals (`0..3`), whose type a probe could not know.
- Even in a `for`, `a..b` cannot be made into `a..=b` by a probe where `b` is the largest value of its type. Where a test runs such a range the mutant is not run, and a run that only gets there with the mutant on is given up; the summary says how many mutants were not evaluated for that. With `--rebuild` they are written into the source and run as what they are. A run is given up by taking away a file the driver made and named (by its full path, in the working directory of the analysis, which no other run uses), and by that alone; taking a file away works on a full disk too: it is known also where the code runs in a program that a test started and whose failure the test takes for its own, and a test that ends with the same status as the runtime is still a test that failed. In doc tests a run that is given up counts as a failed one.
- The probe of `a.max(b)` / `a.min(b)` borrows `a` and calls the method on it as written. For a type that cannot be copied and whose method takes it by value (`String`), or that has only one of the two methods, the probe does not compile and the site is left as it is.
- A mutant that was built on its own (`--rebuild`) and passes every test is reported as `SURVIVOR`: nothing is recorded for such code, so it is not known whether a test runs it at all.
- With `--jobs N` each test binary gets its share of the cores (never fewer than two test threads), and a mutant that ran out of time while others were running is run once more on its own; the second run counts. A machine that is loaded by something else can still turn a slow mutant into a timeout (counted as killed, and said in the summary).

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

Update `Cargo.toml` and `Cargo.lock`, then push the release commit to `main` and check CI. Run the [Release workflow](https://github.com/t-k/targeted-mutation/actions/workflows/release.yml) manually to verify the package. To publish, push a tag matching the package version, such as `v0.1.1`. The tag run verifies the package again and publishes it to crates.io using Trusted Publishing.

## License

Apache License, Version 2.0. See [LICENSE](LICENSE).
