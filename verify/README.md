# Model checking with Veil

Models of Varaq's concurrency protocols, written in [Veil](https://github.com/verse-lab/veil)
and checked by its explicit-state model checker. A model is a hand-written transition system:
each step stands for one atomic step of the Rust code (one atomic operation, or one critical
section under a lock), and the checker tries every interleaving of those steps in a small
instance.

**What this does and does not check.** The models are not extracted from the Rust code. They
check the *protocols*, as the comments in each model describe them, against the properties
listed below. A change to the code that the model does not reflect goes unchecked, so a model
needs updating when the code it cites changes. A pass is a bounded search: it covers every
interleaving within the instance's bounds and nothing beyond them.

| Model | Code | Properties |
|---|---|---|
| [`Latch.lean`](VaraqVerify/Latch.lean) | `latch.rs`: `HybridLatch`'s word (state, waiters, version), optimistic reads and `recheck`, `try_shared`, `upgrade`, `exclusive`, `announce`/`claim` | a reader whose `recheck` passes copied a value no writer was changing; one writer at a time, never alongside sharers; the sharer count matches the holders |
| [`Mvcc.lean`](VaraqVerify/Mvcc.lean) | `txn.rs` (`Snapshots`), `txn/clock.rs`, `txn/commits.rs`, and in `txn/db.rs`: `begin`, `precommit`, `commit_read_only`, `roll_back`, `refresh_visible_to_all`, `collect` | snapshot isolation's visibility rule (a transaction sees exactly the commits below its snapshot, and nothing of a running transaction), through `all_lwm` and through compacted lists of commits; `is_acked` agrees with what was acknowledged; each worker's commits stay sorted |

## Running

Install Lean through [elan](https://github.com/leanprover/elan), plus `clang`, `lld`,
`libc++-dev` and `libc++abi-dev`. Then, from this directory:

```bash
lake build
```

The first build compiles Veil and its dependencies, which takes a while. Each model then runs
its `#model_check`; a violation is an error that fails the build and prints the trace that
leads to it.

The larger instances in [`Deep/Configs.lean`](Deep/Configs.lean) take longer:

```bash
./deep.sh
```

`DEEP_TIMEOUT` sets how many seconds each may run (2400 by default).

## In CI

`.github/workflows/veil.yml` runs `lake build` on pull requests that touch this directory
and every night, and `deep.sh` every night and on demand (Actions → Veil model checking →
Run workflow).

When a nightly run fails, it opens an issue labelled `veil` with the counterexample traces
(`.github/scripts/veil-issue.sh`). While that issue is open, later failures add a comment to it
instead of opening another; close it once the cause is fixed.

## Checking the checks

Each model was run against deliberately broken versions of itself, and each broken version was
caught:

| Mutation | Caught by |
|---|---|
| `recheck` ignores the exclusive bit | `optRecheck` assertion |
| releasing the exclusive guard does not bump the version | `optRecheck` assertion |
| `claim` ignores sharers | `writer_alone` |
| the collector ignores slots holding `BEGINNING` | `visibility_committed` |
| the collector's bound ignores the clock's floor | `visibility_committed` |
| `compact` drops one entry more | `visibility_committed` |
| `all_lwm` published one higher | `visibility_running` |
| `cut` counts commits at the snapshot | `visibility_committed` |

When you change a model, it is worth doing the same: break the code it describes and see the
check fail.
