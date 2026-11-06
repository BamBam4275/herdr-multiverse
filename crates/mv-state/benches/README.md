# State history scale workloads

The stable Rust integration harness runs these workloads without a new benchmark
dependency or manifest changes. The generators and measurements live in
`scale_support/mod.rs`; `../tests/scale_history.rs` is their Cargo entry point.

```sh
MV_STATE_SCALE=1 CARGO_BUILD_JOBS=2 cargo test -p mv-state --release --locked \
  --test scale_history release_scale_budgets -- --nocapture --test-threads=1
```

Without `MV_STATE_SCALE=1`, Cargo runs only the small 1,000-line/50-task contract
check (plus small damage and 4 MiB size-boundary fixtures). Enabling scale budgets in a debug build
fails explicitly. Run the large test on an otherwise quiet machine, one test
thread at a time; fixture creation, validation, and cleanup are outside the timed
operations. Each large fixture is generated once, used once per operation, and
removed by `TempDir`. No real fleet, Herdr, or existing repository is accessed.

The two sizes are 250,000/500,000 routing records, 2,000/4,000 task directories,
and 8/16 candidate lane directories. Histories include all legacy/current role
spellings, dispatch/start/result events, addressed and unrelated CRC-protected
messages, updates, backlog, broken CRCs, and incomplete envelopes. A separate
small fixture verifies invalid UTF-8 and an unterminated final record; the
activity reader currently rejects a non-UTF-8 ledger, so those bytes are kept
out of the successful inbox timing fixture.

| Operation | Budget per size | What is measured |
| --- | ---: | --- |
| Index rebuild | 2 s | Full scan, JSON-lines sidecar + fingerprint, atomic writes/fsync; caller holds project lock |
| Tail, missing/current/corrupt cache | 2 s each | Current `index::records(All)` read plus selecting the last 20 physical records |
| Activity inbox peek | 2 s | Existing cursor validation, message classification, sort, filtering and limit |
| Task reconciliation read inputs | 3 s | `records(Tasks)`, parsing, unique task IDs, one `read_status` per task |
| Lane discovery | 1 s | `Worktrees::lanes`, validation, canonicalization and sorting |
| Prune status scan | 10 s | `Worktrees::prune(false, true)` for every candidate, through refusal for absent terminal status |

Budgets are regression ceilings for local release builds, not cross-platform
service guarantees. Every operation prints elapsed milliseconds and its budget;
all measurements finish before budget assertions run. Doubling ratios are
reported as observations rather than asserted, because filesystem and shared-host
noise can dominate a single sample. Returned records, physical offsets, roles,
CRC rejection, inbox sequences/cursor preservation, task-status totals and prune
refusals are asserted independently of timing.

There is no bounded tail or full task reconcile/list API at this checkout.
The tail and task measurements deliberately use the public state primitives and
do not claim CLI/reconcile coverage. Prune candidates are synthetic unregistered
directories with missing terminal status: the scan must retain all of them.
This measures the history/status gate, excluding branch ancestry, registered
worktree cleanliness, process checks, rescue, and removal. Lane discovery sees
8/16 candidates, not thousands of registered worktrees.

Watch the implementations as well as ratios: cached reads still validate the
entire ledger; durable index append scans old history again; and prune scans the
ledger once for each lane. Repeated append over growing history is quadratic in
records, and prune is proportional to lanes times history length (quadratic when
both grow together).

The scale test exposed a 4 MiB routing-log cap in the shared worktree reader.
`worktrees::read_regular` now exempts exactly `.multiverse/routing.log` from that
artifact cap. Ordinary CI asserts that a larger ledger reaches the status gate
and that a status file over 4 MiB is still refused. Symlink, hard-link and
file-identity checks remain in place.

## Recorded sample

One macOS arm64 run with Rust 1.99.1, optimized, one test thread. These are
single wall-clock samples with warm filesystem caches, not percentile estimates.

| Operation | 250k records, 2k tasks | 500k records, 4k tasks |
| --- | ---: | ---: |
| Tail, missing cache | 339 ms | 484 ms |
| Index rebuild | 432 ms | 607 ms |
| Tail, current cache | 386 ms | 762 ms |
| Inbox peek | 193 ms | 382 ms |
| Task read inputs | 415 ms | 876 ms |
| Tail, corrupt cache | 216 ms | 458 ms |
| Lane discovery, 8 / 16 candidates | 0.71 ms | 0.37 ms |
| Prune status scan, 8 / 16 candidates | 753 ms | 2,858 ms |

The 3.79x prune increase when both inputs double agrees with the nested scan in
`worktrees::terminal_statuses`; cached tail/inbox/read-input workloads increased
about 2x. A previous run measured a 4.12x prune increase. Discovery is too short and too filesystem-sensitive for this ratio to
be useful. All release ceilings passed. Before the ledger size fix, the 250k
prune test failed with `expected a regular, unlinked file` instead of reaching
the missing-status refusal, so its apparent sub-millisecond time was invalid.

A further unmeasured nested path is `worktrees::retirement_authorized`: each
unmerged lane without a landed patch may enumerate every task directory to
find an assessment. That path scales with lanes times task directories and is
not reached by these conservative missing-status candidates.
