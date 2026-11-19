# targeted-mutation

**Targeted mutation testing** generates the realistic wrong implementations that your existing tests would let through, and tells you which input or assertion would catch them.

Classic mutation testing changes `<` to `<=` and then asks whether a test fails. This tool works the other way round: it first watches what the tests actually feed the code, and from that computes the mutants that are guaranteed to pass, without running them. It needs no AI and no solver.

The command is `tmt`. It works on Rust crates.

## What it reports

```rust
pub fn is_eligible(age: u32) -> bool {
    age >= 18
}

#[test] fn minor_is_rejected() { assert!(!is_eligible(17)); }
#[test] fn adult_is_accepted() { assert!(is_eligible(20)); }
```

```text
$ tmt fixtures/eligibility
INPUT GAP  src/lib.rs:4:5
    age >= 18
  passes every test as: `age >= 19`, `age >= 20`  [re-run: tests pass]
  nearest values the tests passed: `age` = 17, 20
  add a test with `age` = 18 (the original gives true)
```

The tests saw 17 and 20, so any threshold in 18..=20 behaves the same on every test. The tool derives that range from the two observed values and names the one input that pins the boundary.

The second kind of finding is about assertions rather than inputs. `fixtures/batch-store` has an atomic batch commit and a test that checks a failing batch returns an error, but never looks at the store afterwards:

```text
$ tmt fixtures/batch-store
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

## Usage

```text
cargo install --path .
tmt [DIR] [--package NAME] [--jobs N] [--doc] [--work-dir DIR] [--all-files] [--json] [--show-unreached] [--no-verify] [--quiet]
```

- `--package NAME`: the workspace member to analyse. Not needed when `DIR` is the member's own directory.
- `--jobs N`: run N mutants at the same time (default 1). Raise it only if the tests can run concurrently with themselves: no fixed ports, no shared files.
- `--doc`: also run the doc tests. rustdoc recompiles them for every mutant they reach, so this is slow.
- `--work-dir DIR`: where the working copy and its build go. It must be a directory of its own: the copy inside it is deleted and rebuilt on every run, and the tool refuses a directory that contains the workspace or a `src` it did not create.
- `--all-files`: copy files that git ignores too. Use it when the build needs generated files that are not tracked.

Exit status: `0` no gaps, `2` gaps found or a prediction failed its re-run (`UNSTABLE`), `1` the analysis could not run (tests fail on the unmutated code, the crate does not build, ...).

Your source tree is never modified. In a git repository only the files git does not ignore are copied (submodules by their own rules), so build output, caches and local files kept out of the repository stay where they are. The copy contains no symbolic links: a linked file is copied as a file, and a linked directory is followed only if it lies inside the workspace. The workspace is copied to `<target dir>/tmt/src` (or `--work-dir`) and built in a `target` directory next to that copy. Path dependencies that point outside the workspace are rewritten to absolute paths in the copy.

## How it works

1. **Instrument.** Every comparison, boolean operator, arithmetic operator and deletable statement (a call, a `?` check, an assignment) in the crate is rewritten into a probe. All mutants are compiled into the same binary and switched on by an environment variable, so the crate is built once.
2. **Trace.** The tests run once. Each probe records, per test, what it saw. For integer comparisons this is the difference `lhs - rhs` nearest to zero on each side, and whether the operands were ever equal.
3. **Plan.** An integer comparison is a threshold on `lhs - rhs`. Every threshold between the nearest observation below and the nearest observation above gives the same result on all observed values, so every test executes such a mutant exactly as it executes the original. Those mutants are input gaps, found by arithmetic. By default one of them is re-run to confirm (`--no-verify` skips that).
4. **Run.** The remaining mutants change the outcome of some observation. Each is run only against the tests whose observations it changes. If they all pass, no assertion noticed: an assertion gap.

Mutants that cannot differ from the original within the operand types (`x >= 0` for an unsigned `x`) are not reported.

### What is mutated

| Code | Mutants | Guaranteed to survive when |
|---|---|---|
| `a < b`, `a >= 18` on integers | the constant shifted by any amount, the operator negated | no test passes a value between the old and the new boundary |
| `x > 30.5` on floats | `>=`, and any threshold between the two nearest observed values | the operands were never equal in a test |
| comparisons of other types | `<` and `<=` swapped, negation | the operands were never equal in a test |
| `a == b`, `a != b` | negation, `true` / `false` | the result is the same in every test |
| `a && b`, `a \|\| b` | one operand's value ignored (`a && { b; true }`, `{ a; b }`), the other operator | the ignored operand never decided the result in a test |
| `a + b`, `-`, `*`, `/`, `%` on primitive numbers, and `+=` etc. | `+` and `-` swapped, `*` and `/` swapped, `%` to `/` | the two operators agree on every pair of operands the tests pass (`fee` is always 0, `factor` is always 1) |
| `a & b`, `\|`, `^`, `<<`, `>>` on integers, and `&=` etc. | `&` and `\|` swapped, `^` to `&`, `<<` and `>>` swapped | as above |
| `!x` on `bool` | `x` | never: always run |
| a function returning `bool` | the value at its normal exit replaced by `true` / `false` | the function returned the same value in every test |
| `f(x);`, `check()?;`, `x += 1;`, `x = y;` | the statement deleted | never: these are always run |

Arithmetic on other types (`String + &str`, `Duration + Duration`) and `!` on anything but `bool` are left as they are.

A finding is not repeated by what encloses it: if `a > 0` is true in every test, the report says so once, and does not add that `a > 0 && b` has an operand that never decides.

Code in the arguments of `assert_eq!`, `assert_ne!`, `vec!`, `format!`, `print!`, `println!`, `eprint!`, `eprintln!`, `write!`, `writeln!`, `panic!`, `unreachable!`, `todo!`, `unimplemented!`, `format_args!` and `matches!` is mutated like any other code. `assert!`, `debug_assert!` and `dbg!` put the source text of their argument into their output, so their arguments are not touched.

A probe that does not compile at some site (for example a guarded `let x; x = ...;`) is dropped and the build is retried; the summary line counts those sites.

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

```text
lean formal/lean/Planner.lean
pict formal/pict/planner.pict /o:3 /e:formal/pict/planner.seed.tsv > tests/data/planner.pict.tsv
java -jar tla2tools.jar formal/tla/RepairLoop.tla
```

## Limits

- Rust only, one package per run. Test binaries are started directly, not through `cargo test`: a configured target runner is not used, and the number of test threads is set by the tool (`RUST_TEST_THREADS` is ignored).
- A module that two targets of the package both declare (`mod common;` in `lib.rs` and in `main.rs`) cannot be probed; its sites are counted as skipped.
- Source files that start with a byte order mark or a `#!` line are not instrumented.
- A macro that shadows a standard one is only recognised when it is defined at the top level of the same file.
- Code in the arguments of macros other than the ones listed above, in `const` contexts and in test code is not mutated.
- Unary minus, casts, and return values other than `bool` are not mutated. A `bool` result is replaced only at the function's normal exit, not at an early `return`.
- For floats, a gap is reported only while no test compares equal operands. After that, thresholds within rounding distance of the constant still survive and are not reported.
- Strings and other non-numeric types have no notion of "next value": only `<` versus `<=` is checked.
- The survivor prediction assumes deterministic tests. A wrong prediction shows up as `UNSTABLE`.
- Equivalent mutants are not always detected. If `max(a, b)` is written with `a >= b`, then `a > b` is reported although no test could observe it. The same goes for an operand that is constant by construction (`is_large(n) && FEATURE_ENABLED`).
- Observations made on threads that a test spawns, and all observations of doc tests, cannot be attributed to a single test; the mutant is then run against the whole test binary.
- With `--jobs`, a mutant's time limit is still derived from the unloaded test run, so a heavily loaded machine can turn a slow mutant into a timeout (counted as killed).

## Evaluating against other tools

A mutation score is not comparable across tools whose mutants differ in difficulty. The comparison this project aims for is: with the same CI time, how many real past bugs (not shown to the tool) are caught by the tests it led you to write.

## Prior work

The idea of generating mutants with knowledge of the tests is called *test-aware mutant generation*. Related lines of work: SEMu (symbolic execution to kill surviving mutants), DSpot (assertion amplification), the W-method (complete test suites for finite state machines) and Meta's ACH (LLM-generated mutants with tests that kill them). This tool is the solver-free, AI-free corner of that space: it reads the test trace and does interval arithmetic.

## Development

```text
cargo test          # unit tests, PICT table, end-to-end runs on fixtures/
cargo run -- .      # the tool on itself
```

## License

Apache License, Version 2.0. See [LICENSE](LICENSE).
