# lockdex

Static lock-order and deadlock analysis for Android, working directly from DEX
bytecode.

`lockdex` reads the compiled `classes.dex` of a component — typically
`system_server` — reconstructs which locks are acquired while other locks are
already held, builds the global lock-order graph, and reports the cycles in it:
the AB-BA inversions that are candidate deadlocks. It also exports the graph as a
pprof profile and an HPROF heap dump so it can be explored in existing viewers.

No annotations (`@GuardedBy` is not needed), no instrumented build, no runtime
trace. One dex is the whole program for analysis purposes, so lock identity and
the call graph are resolved across the entire component.

See [`docs/FINDINGS.md`](docs/FINDINGS.md) for example output — candidate
lock-order inversions found in `system_server`, each with a diagram and the call
path on both sides (e.g. `UserController.mLock ⇄ UserManagerService.mUsersLock`).

## Install

```sh
git clone https://github.com/fiveapplesonthetable/lockdex
cd lockdex
cargo build --release          # produces ./target/release/lockdex
```

Requirements:

- A Rust toolchain (stable).
- **`dexdump`** — the one runtime dependency. It ships with the Android SDK build
  tools (`$ANDROID_SDK_ROOT/build-tools/<ver>/dexdump`) and with an AOSP host
  build (`out/host/linux-x86/bin/dexdump`). Put it on `PATH`, or set
  `LOCKDEX_DEXDUMP=/path/to/dexdump`.
- Optional: `dot` (Graphviz) to render the cycle graph to SVG.

## Quick start on an AOSP tree

After a normal build, the dexed jars live under your output directory. Point
lockdex at the whole component or at the output root.

```sh
# build lockdex
cargo build --release

# make dexdump reachable (Android build tools, or an AOSP host build)
export PATH="$ANDROID_BUILD_TOP/out/host/linux-x86/bin:$PATH"

# analyze system_server: the single dex jar Soong stages for it
./target/release/lockdex analyze \
    "$ANDROID_BUILD_TOP/out/soong/system_server_dexjars/services.jar" \
    --out-dir ./lockdex-out

# or just hand it the output root and let it find the system_server jars
./target/release/lockdex analyze "$ANDROID_BUILD_TOP/out" --out-dir ./lockdex-out
```

When given a directory, lockdex looks for `soong/system_server_dexjars/*.jar`
(the jars that make up the boot/system-server image). `--scope <substr>` narrows
that to jars whose name contains the substring, e.g. `--scope services`.

Input can also be any single `.dex`, or a `.jar`/`.apk` (multidex is handled —
every `classes*.dex` is read and merged so calls resolve across dexes).

`dexdump` is the only external dependency. If it is not on `PATH`, point at it
with `LOCKDEX_DEXDUMP=/path/to/dexdump`. `dot` (Graphviz) is used to render an SVG
if present.

## Output

Without `--out-dir`, lockdex prints a text report (or `--format json|dot`). With
`--out-dir DIR` it writes the full set:

| file | what |
|---|---|
| `report.txt` | the deadlock report — small actionable cycles first, then large "lock tangle" SCCs, each with the locks and the order edges (`file:line`) |
| `lockgraph.json` | the full graph and findings, for tooling |
| `lockgraph.dot` / `.svg` | the lock-order graph, cycles in red |
| `lockorder.pb.gz` | pprof of the lock-order graph — `go tool pprof -http=: lockorder.pb.gz` |
| `methodlock.hprof` | the method/lock dependency graph as an HPROF heap dump for Perfetto |

In the pprof, each lock is a function and each order edge is a weighted sample
tagged `cycle=yes|no`, so `-tagfocus=cycle:yes` isolates the deadlock subgraph and
the flat/cum weights rank the busiest locks. In the HPROF (drag it into
<https://ui.perfetto.dev>), each method is an object whose outgoing references are
named by the lock held at the call, so walking the heap graph walks the lock
dependency chain.

With `--out-dir`, stdout is a short summary of what was found and which files
hold it (the full report goes to `report.txt`, not the terminal). Without it, the
full text report prints to stdout. Output is deterministic: the same dex always
produces byte-identical artifacts.

## Reading the report

`report.txt` opens with a one-line summary, then the findings, smallest first:

```
=== DEADLOCK #1: 2 locks ===
   com.android.server.am.UserController.mLock
   com.android.server.pm.UserManagerService.mUsersLock
   conflicting order edges:
     UserController.mLock -> UserManagerService.mUsersLock  [7x]  …UserController.finishUserStopped(…):1607 (interproc)
     UserManagerService.mUsersLock -> UserController.mLock   [1x]  …UserManagerService.removeUserState(…):7733 (interproc)
```

- The two indented lines are the **locks in the cycle** (their canonical
  `Class.field` identities).
- Each **order edge** `A -> B` reads as: *at this `file:line`, `A` is held and the
  code reaches an acquisition of `B`.* `[7x]` is how many distinct sites induce
  that ordering; `interproc` means `B` is taken in a callee (a `nested` tag means
  both locks are taken in the one method).
- The two edges together are the **inversion**: one path takes A→B, another takes
  B→A. That is the AB-BA. To confirm it is a real deadlock you still need the two
  sites to run on different threads (see below).

A **`LOCK TANGLE`** block instead of `DEADLOCK` is a large strongly-connected
component — many locks mutually out-of-order. These reflect a globally
interconnected lock hierarchy (the `system_server` AMS/ATMS/WMS locks are the
classic case) rather than one fixable inversion; skim them, but the small
`DEADLOCK` cycles are where the actionable bugs are.

Open `cycles.svg` for the same small cycles as a picture, and use `verify` to pull
the exact source for any of them.

## Verifying candidates against source

A reported cycle is a *candidate*. The `verify` command analyzes, then for each
small cycle pulls the source at both edge sites from a checkout, follows to where
the *target* lock is actually acquired, and prints the two orderings side by side
with a verdict:

```sh
lockdex verify "$ANDROID_BUILD_TOP/out/soong/system_server_dexjars/services.jar" \
    --src-root "$ANDROID_BUILD_TOP/frameworks/base" \
    --max-locks 3 \
    --out verify.txt
```

For each candidate it shows the `synchronized(A)` block and the call that reaches
`B`, then the `synchronized(B)` sites in B's class — so you can read the AB-BA
directly. Example output:

```
CANDIDATE 1 : 2 locks
   lock  …RemoteTaskStore.mRemoteDeviceTaskLists
   lock  …RemoteTaskStore.mRemoteTaskListeners

   mRemoteDeviceTaskLists -> mRemoteTaskListeners
      hold mRemoteDeviceTaskLists at  …/RemoteTaskStore.java:179
        >>  179   notifyListeners();         // inside synchronized(mRemoteDeviceTaskLists)
      acquire mRemoteTaskListeners at  …/RemoteTaskStore.java:188  synchronized (mRemoteTaskListeners)

   mRemoteTaskListeners -> mRemoteDeviceTaskLists
      hold mRemoteTaskListeners at  …/RemoteTaskStore.java:130
        >>  130   getMostRecentTasks();      // inside synchronized(mRemoteTaskListeners)
      acquire mRemoteDeviceTaskLists at  …/RemoteTaskStore.java:113  synchronized (mRemoteDeviceTaskLists)

   VERDICT: BOTH orderings located in source — distinct locks acquired in
            opposite order. Real AB-BA if the two sites can run on different threads.
```

`--out-dir DIR` writes the bundle: `verify.txt` plus, per candidate cycle,
`candNN.{dot,svg,pb.gz,hprof}`. The `svg`/`dot` is the call-path DAG — the locks
as red boxes joined by the actual call path of each order edge
(`held in → calls… → acquires`), the shared lock nodes forming the AB-BA loop;
it's the view to *look at* one deadlock. The `pb.gz` (pprof) and `hprof` are that
single deadlock's **method graph** for drilling/filtering (`go tool pprof`,
Perfetto) — handy when a candidate's paths are large. (`--out FILE` still writes
just the text report.)

The verdict stops short of asserting the deadlock: confirming it also needs the
two sites to run on different threads concurrently, which the tool deliberately
does not guess (a sound answer would require a may-happen-in-parallel analysis).
For Binder-entry methods like the pair above, concurrency is essentially always
possible; read the two sites and decide.

## How it works

### 1. Why bytecode

Lock analysis needs three facts, and all three are direct in DEX and approximate
in source:

- **Which value is the lock.** `monitor-enter v0` names a register; tracing it
  back through the instructions gives the field, class constant, or `this` it
  holds. A field lock is identified by the field's *declaring class and name*, so
  every alias of one object (`a.mPm.mLock`, `b.mPm.mLock`, a getter
  `getPm().mLock`) collapses to a single node, `PackageManagerService.mLock`.
- **Where a call goes.** Virtual and interface calls are resolved against the
  full class hierarchy present in the dex, restricted to the types that are
  actually instantiated (rapid type analysis). Lambdas are ordinary synthetic
  classes after desugaring, so callbacks are not a special case.
- **What is held.** `monitor-enter`/`monitor-exit` are explicit opcodes;
  `java.util.concurrent` locks (`Lock.lock`/`unlock`/`tryLock`, and `readLock()`
  /`writeLock()` views) are ordinary calls, modelled directly.

### 2. Per-method summaries (parallel)

Each method is analyzed once, independently, on a forward pass over its
instructions:

- a small abstract value is tracked per register, so each acquire resolves to a
  lock identity;
- a held-lock stack is maintained; when a lock is acquired while others are held,
  an order edge `held → acquired` is recorded, carrying the full set of locks
  held at that point (its *guard set*);
- every call site records the locks held at the call and the call's arguments;
- trivial getters are summarized so `synchronized(getLock())` resolves to the
  underlying field.

This phase is embarrassingly parallel across methods.

### 3. Stitching the call graph

A bounded fixpoint propagates each method's acquired locks to its callers through
the resolved call graph. Holding `L1` across a call whose callee (transitively)
acquires `L2` yields the order edge `L1 → L2`. Calls that hand work to another
thread — `Handler.post`, `Executor.execute`, `Thread.start`, … — are *severed*:
a lock held when the runnable is posted is not held when it runs, so it does not
propagate across the boundary.

### 4. Cycles, then refinement

Tarjan's algorithm finds the strongly connected components of the lock-order
graph. A 2-node SCC is the classic AB-BA inversion; larger SCCs are tangled lock
hierarchies. Candidates are then filtered:

- **Guard refinement** — if both orderings of a pair always occur while some
  common outer lock is held, the two acquisitions are mutually exclusive and the
  cycle is not a real deadlock.
- **try-lock** — a `tryLock` cannot block, so its edge never closes a cycle.
- **Reentrancy** — re-acquiring a lock already held is not an edge.

What remains is reported, smallest (most actionable) cycles first.

## Tuning the async list

Held locks are *severed* at calls that defer work to another thread, so a lock
held when work is posted is not treated as held when it runs. The built-ins cover
`Handler.post*` / `sendMessage*`, `Executor.execute`, `ExecutorService.submit`,
`Thread.start`, `AsyncTask.execute`, etc. — but only by name, so a project's own
dispatcher won't be recognized.

`--async-sinks FILE` adjusts the list on top of the built-ins. One entry per line.
An entry may be a fully-qualified `pkg.Class.method`, a simple `Class.method`, or
a bare `method` (matches that method on any class). A leading `-` disables a
built-in; `#` for comments:

```
# treat our own dispatcher as async — FQN (precise) or simple name both work
com.example.os.MyDispatcher.runLater
MyDispatcher.runLater
postToBackground
# ...and stop treating AsyncTask.execute as async
-AsyncTask.execute
```

```sh
lockdex analyze "$ANDROID_BUILD_TOP/out" --out-dir ./out --async-sinks ./async-sinks.txt
```

Adding a sink removes false edges (a post that isn't followed); removing one adds
edges back. It is a list, so add or remove freely without rebuilding.

## Tests

`tests/corpus/` holds small Java programs, each with the verdict it should
produce encoded in its header comment. The harness compiles each to dex with
`d8` and checks the analysis against the expected result:

```sh
tests/run_corpus.sh
```

The corpus covers nested and interprocedural AB-BA, getter-aliased locks,
two-instance aliasing, guard-protected non-deadlocks, async boundaries,
`java.util.concurrent` locks, read/write locks, reentrancy, lambda capture, and
try-lock.

## Scope and limits

This is a bug finder, not a proof, and it is deliberately biased toward not
reporting false deadlocks rather than toward completeness:

- Locks reached through collections, arrays, or data-dependent branches are left
  unresolved and never merged, so they cannot fabricate a cycle (they can miss
  one).
- The call graph over-approximates at megamorphic sites, which can connect locks
  that are not connected in practice. A large strongly connected component
  usually reflects a genuinely tangled lock hierarchy — the `system_server`
  AMS / ATMS / WMS global locks are the classic example — rather than one bug.
- Native (JNI) monitors and cross-process Binder reentrancy are out of scope.

Read every reported cycle against the source before changing any locking.
