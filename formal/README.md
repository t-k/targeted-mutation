# Formal Verification Artifacts

Core invariants of `targeted-mutation` — survivor interval arithmetic, combinatorial edge cases, crash recovery, concurrency protocols, and process cleanup — are machine-checked alongside the Rust test suite.

## Lean 4 proofs (`formal/lean/`)

| Model | What it verifies |
|---|---|
| `Planner.lean` | Over mathematical integers and complete observations, the survivor interval is sound and exact, the 3-value trace summary is sufficient, 4-bit comparison truth-table masks and `BitVec` disjoint-bit identities hold, reachability filtering (power-of-two products, masks, remainders) is sound under wrapping, zero-allocation `replace` vs `replacen(..., 1)` equivalence holds, and sequence-end (`first`/`last`) separation holds |

```sh
lean formal/lean/Planner.lean
```

## PICT combinatorial tables (`formal/pict/`)

3-way covering arrays generated from PICT models and replayed in Rust tests (`tests/data/*.pict.tsv`):

| Model | Scope |
|---|---|
| `planner.pict` | Comparison operator, observation shape, literal position, and type range (`plan::tests::pict_table`) |
| `workcopy.pict` | Previous file state, user edits, and instrumentation decisions (`driver::tests::work_copy_table`) |
| `slice_ends.pict` | Sequence receiver shapes and observed lengths (`tests/e2e.rs::slice_end_table`) |
| `unpinned_calls.pict` | Test call contexts, argument shapes, and function kinds (`witness::assertions::tests::pict_table`) |
| `reverts.pict` | Diff hunks, test-only edits, and `cfg`-disabled code (`reverts::tests`) |
| `output-fixture.pict` & `output-destinations.pict` | Staged output fixture ownership, replacements, and case/path conflicts (`output_fixture::tests`) |

```sh
pict formal/pict/planner.pict /o:3 /e:formal/pict/planner.seed.tsv > tests/data/planner.pict.tsv
```

## TLA+ models (`formal/tla/`)

| Model | What it verifies |
|---|---|
| `RepairLoop.tla` | Instrument → build → drop-failing-probe loop terminates and ships no uncompilable probe |
| `JobPool.tla` | Parallel runner executes every mutant exactly once and collects all results |
| `CampaignPhases.tla` | Baseline and mutant worker bounds, and provisional parallel timeout resolution order |
| `CampaignCheckpoint.tla` | Safe outcome reuse only on exact context/inventory match, including partial completion on `Unable` |
| `TraceBacklog.tla` | Per-thread observation batching never loses nearest distances |
| `WorkCopy.tla` | Crash-safe working copy synchronization across interrupted runs |
| `TargetCoverage.tla` | Uncompiled source targets remain unevaluated and mutated files are restored before recording verdicts |
| `OutputFixture.tla` | Unowned output preservation and ownership journaling across interruptions |
| `WorkLock.tla` | Exclusive cross-process work-directory locking and legacy sentinel isolation |
| `Leftovers.tla` | Subreaper/PID-1 orphan sweeping (`is_left_candidate`) never stops active sibling children and always drains leftovers |
| `NextestStop.tla` | Process-group reaping before publishing test verdicts |
| `WorkspaceRun.tla` | Multi-package workspace scheduling and isolated member error handling |
| `TestPoll.tla` | Incremental test output polling and timeout termination |
| `PositionWitness.tla`, `CompoundWitness.tla`, `SharedWitness.tla` | Witness discovery, verification ordering, and shared-build binary preservation |

```sh
java -jar tla2tools.jar -config formal/tla/Leftovers.cfg formal/tla/Leftovers.tla
```

## Property tests (`tests/properties.rs`)

`proptest` checks span validity, line-ending preservation, and valid Rust parsing over generated programs and this repository's own sources:

```sh
cargo test --test properties
PROPTEST_CASES=256 cargo test --test properties
```
