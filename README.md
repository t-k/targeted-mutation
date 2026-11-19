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
tmt [DIR] [--package NAME] [--json] [--show-unreached] [--no-verify] [--quiet]
```

Exit status: `0` no gaps, `2` gaps found, `1` the analysis could not run (tests fail on the unmutated code, the crate does not build, ...).

Your source tree is never modified. The crate is copied to `<target dir>/tmt/src` and built in `<target dir>/tmt/target`.

## How it works

1. **Instrument.** Every comparison and every deletable statement (a call, a `?` check, an assignment) in the crate is rewritten into a probe. All mutants are compiled into the same binary and switched on by an environment variable, so the crate is built once.
2. **Trace.** The tests run once. Each probe records, per test, what it saw. For integer comparisons this is the difference `lhs - rhs` nearest to zero on each side, and whether the operands were ever equal.
3. **Plan.** An integer comparison is a threshold on `lhs - rhs`. Every threshold between the nearest observation below and the nearest observation above gives the same result on all observed values, so every test executes such a mutant exactly as it executes the original. Those mutants are input gaps, found by arithmetic. By default one of them is re-run to confirm (`--no-verify` skips that).
4. **Run.** The remaining mutants change the outcome of some observation. Each is run only against the tests whose observations it changes. If they all pass, no assertion noticed: an assertion gap.

Mutants that cannot differ from the original within the operand types (`x >= 0` for an unsigned `x`) are not reported.

A probe that does not compile at some site (for example a guarded `let x; x = ...;`) is dropped and the build is retried; the summary line counts those sites.

## Formal artifacts

- `formal/lean/Planner.lean`: the survivor range is sound (every threshold in it agrees with the original on all observations), exact (every threshold outside it is separated by an observation), and the three-value trace summary is enough to decide it. Checked with Lean 4 core.
- `formal/pict/planner.pict`: 3-way combinations of operator, observation shape, literal position and type range. The generated table (`tests/data/planner.pict.tsv`) is replayed against a brute-force oracle in `plan::tests::pict_table`.
- `formal/tla/RepairLoop.tla`: the instrument, build, disable-failing-probes loop terminates and never ships a probe that does not compile. Checked with TLC.

```text
lean formal/lean/Planner.lean
pict formal/pict/planner.pict /o:3 /e:formal/pict/planner.seed.tsv > tests/data/planner.pict.tsv
java -jar tla2tools.jar formal/tla/RepairLoop.tla
```

## Limits

- Rust only, `std` crates only, one package per run. Doc tests are not run.
- Code inside macro invocations (`assert!(a < b)`, `matches!`), `const` contexts and test code is not mutated.
- Mutations: comparison operators and constants, and statement deletion. Boolean and arithmetic operators are not mutated yet.
- Float, string and other non-integer comparisons are tracked by ordering only (`<` versus `<=`), without a value range.
- The survivor prediction assumes deterministic tests. A wrong prediction shows up as `UNSTABLE`.
- Equivalent mutants are not always detected. If `max(a, b)` is written with `a >= b`, then `a > b` is reported although it cannot be observed.
- Observations made on threads that a test spawns cannot be attributed to a test; the mutant is then run against the whole test binary.
- Path dependencies that point outside the workspace break in the copy.

## Evaluating against other tools

A mutation score is not comparable across tools whose mutants differ in difficulty. The comparison this project aims for is: with the same CI time, how many real past bugs (not shown to the tool) are caught by the tests it led you to write.

## Prior work

The idea of generating mutants with knowledge of the tests is called *test-aware mutant generation*. Related lines of work: SEMu (symbolic execution to kill surviving mutants), DSpot (assertion amplification), the W-method (complete test suites for finite state machines) and Meta's ACH (LLM-generated mutants with tests that kill them). This tool is the solver-free, AI-free corner of that space: it reads the test trace and does interval arithmetic.

## Development

```text
cargo test          # unit tests, PICT table, end-to-end runs on fixtures/
cargo run -- .      # the tool on itself
```
