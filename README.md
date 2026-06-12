# lockdex

Static lock-order and deadlock analysis for Android, working directly from DEX
bytecode.

`lockdex` reads the compiled `classes.dex` of a component — typically
`system_server` — reconstructs which locks are acquired while other locks are
already held, builds the global lock-order graph, and reports the cycles in it:
the AB-BA inversions that are candidate deadlocks. It also exports the graph as a
pprof profile and an HPROF heap dump so it can be explored in existing viewers.

No annotations needed, no instrumented build, no runtime
trace. One dex is the whole program for analysis purposes, so lock identity and
the call graph are resolved across the entire component.

Two related queries run on the same data: `lockdex binder` finds locks held across
Binder IPC boundaries (a cross-process hazard rather than a same-process cycle),
and `lockdex races` infers each field's guard lock and flags inconsistently-guarded
fields. See [Locks across Binder IPC](#locks-across-binder-ipc) and
[Inconsistently-guarded fields](#inconsistently-guarded-fields).

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
| `report.txt` | the deadlock report — small actionable cycles first, then large "lock tangle" SCCs with every member and their minimal inversions (`file:line` on each edge) |
| `lockgraph.json` | the full graph and findings (incl. per-SCC `inversions`), for tooling |
| `cycles.dot` / `cycles.svg` | the deadlock picture: small cycles (red) + tangle inversions (amber); the SVG is rendered automatically when Graphviz `dot` is installed |
| `lockgraph.dot` | the *full* lock-order graph for tooling — written but not rendered (too many edges) |
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

## Generating diagrams (SVGs) and code paths

All diagrams come from `--out-dir`. Prerequisites:

- **Graphviz `dot`** on `PATH` — the only extra tool. Without it lockdex still
  writes every `.dot` file and says so; render later with
  `dot -Tsvg file.dot > file.svg`.
- **`--src-root <checkout>`** wherever you want actual *code* in the output —
  the source tree the dex was built from (for AOSP, `$ANDROID_BUILD_TOP` or
  `frameworks/base`). Required by `verify`, optional for `binder` / `races`.

What each command draws:

| command | diagram | what it shows |
|---|---|---|
| `analyze --out-dir D` | `D/cycles.svg` | every deadlock cycle: small SCCs (red) and tangle inversions (amber), edge labels = number of inducing sites |
| `verify --src-root S --out-dir D` | `D/candNN.svg` per candidate | the **call-path DAG** of one deadlock: lock nodes (red) joined by the real call chain of each order edge (`held in → calls… → acquires`); `verify.txt` inlines the source lines of both orderings |
| `binder --src-root S --out-dir D` | per-finding `*.svg` | the chain from the lock holder to the Binder boundary; `binder.md` inlines the holding site's source |
| `races --src-root S --out-dir D` | per-field `*.svg` | the field, its inferred guard (green), and the unguarded accessors (red); `races.md` inlines the offending lines |

The curated reports under [`docs/`](docs/) (`FINDINGS.md`, `BINDER_FINDINGS.md`,
`RACE_FINDINGS.md`) were produced exactly this way — run the command with
`--out-dir`, copy the per-finding SVGs next to the markdown, then audit each
finding against source and write up the ones that survive:

```sh
# deadlocks: analyze, then verify each candidate with code + per-candidate SVGs
lockdex verify out/soong/system_server_dexjars/services.jar \
    --src-root "$ANDROID_BUILD_TOP" --max-locks 4 --out-dir ./verify-out
# binder hazards, filtered to one service => every matching site gets an SVG
lockdex binder services.jar --class ActivityManagerService \
    --src-root "$ANDROID_BUILD_TOP" --out-dir ./binder-out
# field races, filtered to one guard lock
lockdex races services.jar --guard mProcLock \
    --src-root "$ANDROID_BUILD_TOP" --out-dir ./races-out
```

`--lock` / `--class` / `--field` / `--guard` filters switch binder/races from
"sample the top findings" to "emit a diagram + source for *every* match", which
is how you get the complete image set for one service or lock.

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
component — many locks mutually out-of-order (the `system_server` AMS/ATMS/WMS
locks are the classic case). A tangle is **not** summarized away: the block lists
*all* of its member locks, then decomposes the tangle into its **minimal
inversions** — for every order edge, the shortest cycle through it — each printed
like a small `DEADLOCK` (locks, edges, `file:line`). Inversions whose every
acquisition shares a common outer lock are kept but marked `[gated by …]` (they
cannot interleave); nothing is silently dropped, and every order edge in the
tangle appears in at least one listed inversion. The same decomposition is in
`lockgraph.json` under `cycles[].inversions`.

Open `cycles.svg` for the picture — small cycles in red, tangle inversions in
amber — and use `verify` to pull the exact source for any of them (tangles are
verified piecewise, one inversion at a time).

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

## Locks across Binder IPC

`lockdex binder` is a separate query over the same data. It is *not* deadlock
detection: it flags places where a lock is held while a thread crosses a process
boundary. The peer process is outside the graph, so this is a cross-process
deadlock / priority-inversion / ANR hazard rather than a provable cycle.

```sh
lockdex binder "$ANDROID_BUILD_TOP/out/soong/system_server_dexjars/services.jar" \
    --src-root "$ANDROID_BUILD_TOP" --out-dir ./binder-out
```

Two directions, selectable with `--direction out|in|both` (default `both`):

- **Outgoing** — a lock held at a call site that (transitively) reaches
  `IBinder.transact`. The report groups these by lock and ranks them by how often
  each lock is held across IPC, so the global locks rise to the top (on
  `system_server`, `ActivityTaskManagerService.mGlobalLock` leads by a wide margin).
- **Incoming** — a public method of a Binder server (a class extending
  `android.os.Binder`) that acquires a lock a remote caller can therefore block on.
  An entry that *also* holds a lock across its own outgoing transaction is flagged
  **high-risk** — that is the nested pattern that genuinely deadlocks across
  processes.

`--out-dir` writes `binder.md` (the report), a call-path diagram per finding
(`*.dot` and, with Graphviz, `*.svg`) showing the chain from the lock holder to the
Binder boundary, `binder.json` (the full data), and `binder.pb.gz` / `binder.hprof`
for `go tool pprof` / Perfetto. With `--src-root` the holding site is inlined from
source. Locks with no cross-thread identity (a fresh `new` object, an unresolved
monitor) are excluded — only shared lock identities can be a cross-process hazard.

To focus on one service or lock, pass `--class <substr>` (matches the holding
method, e.g. `--class ActivityManagerService`) and/or `--lock <substr>` (matches the
lock name, e.g. `--lock mProcLock`). A filtered run emits the source and a diagram
for *every* matching site, not just the top ones — so it's the way to get the full
set of images for a given service or lock.

Output is deterministic, so comparing two builds needs no special mode: diff the
`binder.json`, or use `go tool pprof -diff_base=old.pb.gz new.pb.gz` to see which
hold-sites a change added or removed.

See [`docs/BINDER_FINDINGS.md`](docs/BINDER_FINDINGS.md) for what this finds on a
real `system_server` — the ranked locks held across IPC and the four high-risk
incoming entries, with diagrams.

## Inconsistently-guarded fields

`lockdex races` infers each field's guarding lock from the bytecode: for each field it
looks at the locks held on its reads and writes, and flags the fields that are
guarded by one lock *almost* always — the remaining unguarded accesses are the
suspected races.

```sh
lockdex races "$ANDROID_BUILD_TOP/out/soong/system_server_dexjars/services.jar" \
    --src-root "$ANDROID_BUILD_TOP" --out-dir ./races-out
```

The held-set is interprocedural: a field written inside a helper counts as guarded
when the helper is *always* reached under the lock (a meet over the method's
callers). `final`/`volatile` fields, constructor writes, and compiler-synthesized
methods are excluded. A field is only reported when a clear majority (≥2/3) of its
writes hold one lock and some access still misses it — below that, the "guard" is
usually a call-graph artifact rather than a real contract.

`--out-dir` writes `races.md`, `races.json`, and a per-field diagram (the field,
its guard in green, and the unguarded accessors in red); `--src-root` inlines the
offending lines. `--field <substr>` / `--guard <substr>` narrow the report and emit
every matching diagram. See [`docs/RACE_FINDINGS.md`](docs/RACE_FINDINGS.md).

The must-hold reasoning is sound given the call graph: a flagged access is
genuinely reachable without the guard along a modeled path. Because direct and
private calls are resolved exactly, a flag on a private call chain is guaranteed —
the only way it is a false positive is if a *virtual* call on the path was resolved
by RTA to a target the runtime never dispatches there (a spurious caller that
poisons the intersection). So treat it as a ranked worklist whose precision is
exactly the call graph's: start at the top and check the public methods on the
path.

## Tuning the async-dispatch list

Held locks are *severed* at calls that defer work to another thread, so a lock
held when work is posted is not treated as held when it runs:
`synchronized (lock) { handler.post(() -> foo()) }` does **not** run `foo()`
under `lock`, and no order edge or guard credit flows through the dispatch.

Dispatch points are recognized two ways, both on by default:

- **By name** — `Handler.post*` / `sendMessage*`, `Executor.execute`,
  `ExecutorService.submit`, `Thread.start`, `AsyncTask.execute`, etc.
- **By type hierarchy** — the same methods on **anything that inherits from** a
  dispatcher base: `android.os.Handler`, the `java.util.concurrent` executor
  types, `java.lang.Thread`, `android.os.AsyncTask`, `View.post*`,
  `Message.sendToTarget`, `Timer`, `CompletableFuture.*Async`. A custom `Handler`
  subclass or an `Executor` implementation with an arbitrary name is recognized
  by what it *is*, not what it's called (see `ASYNC_BASES` in `src/juc.rs`).
  The standard JDK/Android chains (`ThreadPoolExecutor → … → Executor`,
  `HandlerThread → Thread`) are known to the tool even when those classes are
  not in the analyzed dex (`KNOWN_SUPERS`). The hierarchy is otherwise read from
  the dex itself, so a subclass chain that passes through a class *outside* the
  dex (and outside `KNOWN_SUPERS`) cannot be followed — add an `extends` entry
  for such a dispatcher.

`--async-dispatch FILE` adjusts both lists on top of the built-ins. One entry per
line, `#` for comments, a leading `-` disables a built-in:

```
# name entries: FQN (precise), simple Class.method, or bare method
com.example.os.MyDispatcher.runLater
MyDispatcher.runLater
postToBackground
# hierarchy entries: the base class (fully qualified) and everything
# inheriting from it
extends com.example.os.WorkQueue: enqueue enqueueFront
# stop treating AsyncTask.execute as async (name entry — matches the class
# declared at the call site, not its subtypes)
-AsyncTask.execute
# disable a built-in hierarchy base entirely (covers its subtypes)
-extends java.lang.Thread
```

```sh
lockdex analyze "$ANDROID_BUILD_TOP/out" --out-dir ./out --async-dispatch ./async.txt
```

Adding a point removes false edges (a post that isn't followed); removing one adds
edges back. It is a list, so add or remove freely without rebuilding.

## Tests

`tests/corpus/` holds small Java programs, each with the verdict it should
produce encoded in its header comment (`// EXPECT:`, `// CYCLE:`, and
`// INVERSION:` for the minimal-inversion decomposition of a tangle). Each fixture
is committed alongside a prebuilt `.dex`, so the suite runs in-process with no Java
toolchain:

```sh
cargo test
```

`dexdump` must be reachable (see Install). After adding or editing a fixture,
rebuild the dex inputs (needs `javac` and `d8`):

```sh
cargo run --example regen_dex
```

The corpus covers nested and interprocedural AB-BA, three-lock cycles,
getter-aliased and constructor-parameter-aliased locks, a shared singleton lock
split across fields, two-instance aliasing, guard-protected non-deadlocks, async
boundaries (both by name and via an Executor *subtype* with an unrelated name),
a 13-lock tangle with its minimal-inversion decomposition, `java.util.concurrent`
locks, read/write locks, reentrancy, lambda capture, try-lock, inheritance with
override (RTA) dispatch, static-synchronized class locks, and
instance-synchronized `this` monitors.

`tests/binder/` is a second corpus for the Binder analysis, with `// OUT:`,
`// NO_OUT:`, `// IN:`, and `// HIGH:` contracts (lock held across an outgoing
transaction, the negative, an incoming entry that takes a lock, and the high-risk
nested case). Those fixtures compile against a fake `android.os` package under
`tests/binder/support` so they dex without the Android SDK; regenerate them with
`cargo run --example regen_dex -- binder`.

`tests/races/` is a third corpus for the field-race analysis, with `// RACE:` /
`// NO_RACE:` contracts — covering an inconsistently-guarded field, a consistent
one, the interprocedural guarded and unguarded cases (which exercise the must-hold
propagation), and the `volatile` and constructor exclusions. Regenerate with
`cargo run --example regen_dex -- races`.

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

## License

Apache 2.0 — see [`LICENSE`](LICENSE). The crate is laid out as a library
(`src/lib.rs`), a CLI binary (`src/main.rs`), and an in-process corpus test
(`tests/corpus.rs`); an `Android.bp` is included so it can be hosted in an AOSP
tree as `rust_library_host` / `rust_binary_host` / `rust_test_host` without
restructuring.
