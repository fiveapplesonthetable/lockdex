# Findings — lock-order analysis of `system_server`

`lockdex` flagged 20 candidate lock-order cycles on a build's `services.jar`;
`lockdex verify` traced each to source. The candidates below were then read by
hand against AOSP `frameworks/base` — the call paths followed hop by hop, the
locks checked for object identity, the `@GuardedBy`/threading annotations and any
documented lock ordering taken into account.

This is the result of that review, not raw tool output. The confident inversions
are listed first with a suggested fix; the candidates that did not survive review
are listed at the end with the reason, because several are instructive about
where static lock-order analysis over-reports.

A reported cycle is a real pair of opposite-order acquisitions in the bytecode.
Whether it can actually deadlock further requires the two sites to run on
different threads concurrently — established per finding below, never assumed.

---

## Confident inversions

### 1. `LockSettingsService.mSeparateChallengeLock` ⇄ `mSpManager`

![](findings/cand04.png)

`LockSettingsService` declares a canonical order in a class comment:
`mSeparateChallengeLock -> mSpManager` (`LockSettingsService.java:262-263`). One
path honors it; another inverts it, on a different thread.

- **`mSeparateChallengeLock` → `mSpManager`** — `setLockCredential` takes
  `mSeparateChallengeLock` (`:1921`) then `mSpManager` via `setLockCredentialInternal`
  (`:1980`). Binder thread; matches the documented order.
- **`mSpManager` → `mSeparateChallengeLock`** — the runnable posted by
  `onUserUnlocking` takes `mSpManager` (`:953`) then `mSeparateChallengeLock` via
  `tieProfileLockIfNecessary` → `getSeparateProfileChallengeEnabledInternal`
  (`:1368`). `mHandler` thread; **inverts** the documented order.

The locks are distinct objects (`:267`, `:289`) and the two sites run on a Binder
thread and the `mHandler` thread, so they can interleave. The maintainers treat
this order as load-bearing — that's why it's written down — and the unlock path
breaks it.

**Fix.** On the unlock path, take `mSeparateChallengeLock` before `mSpManager`
(the documented order), or read the separate-challenge flag — the only thing that
needs `mSeparateChallengeLock` inside that block — before entering
`synchronized (mSpManager)`.

### 2. `RemoteTaskStore.mRemoteDeviceTaskLists` ⇄ `mRemoteTaskListeners`

![](findings/cand06.png)

Two monitors in one class, taken in both orders, with no `@GuardedBy` or ordering
comment anywhere in the file — the nesting discipline is implicit and the code
breaks it.

- **`mRemoteDeviceTaskLists` → `mRemoteTaskListeners`** — `removeDevice` holds the
  map lock (`:173`) then calls `notifyListeners()` → `synchronized (mRemoteTaskListeners)`
  (`:188`). Runs on the transport-teardown thread (`onAssociationDisconnected`,
  no Handler hop).
- **`mRemoteTaskListeners` → `mRemoteDeviceTaskLists`** — `addListener` holds the
  listener lock (`:128`) then calls `getMostRecentTasks()` → `synchronized
  (mRemoteDeviceTaskLists)` (`:52`). Reached over Binder
  (`ITaskContinuityManager.Stub.registerRemoteTaskListener`).

Distinct objects, genuinely inverted, different threads. A Binder client
registering a listener while a device disconnects is the textbook AB-BA.
(`notifyListeners` even nests lock-2→lock-1 within one call chain, so the design
is intrinsically order-mixed.)

**Fix.** Establish one order and keep `getMostRecentTasks()` (which takes
`mRemoteDeviceTaskLists`) out of any `synchronized (mRemoteTaskListeners)` block —
snapshot the tasks before taking the listener lock. Collapsing both to a single
lock is the robust option for a class this small.

### 3. `OneTimePermissionUserManager.mLock` ⇄ `PackageInactivityListener.mInnerLock`

![](findings/cand08.png)

The manager-wide `mLock` guards the uid→listener map (`@GuardedBy("mLock")`,
`:98`); the per-listener `mInnerLock` guards that listener's alarm/uid-observer
state. They are acquired in both orders across different threads.

- **`mInnerLock` → `mLock`** — the uid-observer callback
  `onUidGone`/`onUidStateChanged` → `updateUidState` takes `mInnerLock` (`:347`) →
  `onPackageInactiveLocked` takes `mLock` via `mListeners.remove(mUid)` (`:471`).
  Oneway Binder thread.
- **`mLock` → `mInnerLock`** — `mUninstallListener.onReceive` takes `mLock` (`:82`)
  → `listener.cancel()` takes `mInnerLock` (`:394`). Main thread (receiver
  registered with no Handler).

Distinct objects, opposite order, concurrent threads (Binder observer vs. main-
thread broadcast), on the same listener instance during an uninstall-while-active
session.

**Fix.** Pick a global order — `mLock → mInnerLock` is the natural one — and hold
to it both ways. `onPackageInactiveLocked` takes `mInnerLock` then `mLock`; hoist
the `mListeners.remove(mUid)` out of the `mInnerLock` region (or take `mLock`
first) so it matches the `onReceive` side.

### 4. `BatteryController.mLock` ⇄ `LocalBluetoothBatteryManager.mBroadcastReceiver`

![](findings/cand03.png)

`BatteryController`'s class javadoc notes it is touched from Binder threads, the
UEventObserver thread, and its own Handler. One lock is a `BroadcastReceiver`
object used as a monitor; the other is the controller's `mLock`. Both `@GuardedBy`
sets are internally consistent, but the two are nested in opposite orders on
different threads.

- **`mBroadcastReceiver` → `mLock`** — `onReceive` holds `mBroadcastReceiver`
  (`:964`) then invokes the listener `handleBluetoothBatteryLevelChange`, which
  takes `mLock` (`:387`). Main Looper (receiver registered without a scheduler).
- **`mLock` → `mBroadcastReceiver`** — `onInputDeviceAdded` holds `mLock` (`:478`),
  constructs a `UsiDeviceMonitor` → `addBatteryListener` → `synchronized
  (mBroadcastReceiver)` (`:981`). DisplayThread (input listener on `mHandler`).

Distinct objects; the main-thread leg fires on a BT battery-level broadcast, the
DisplayThread leg on adding/changing an input device — both normal operation,
narrow window, nothing forbids the nesting.

**Fix.** Never take `mBroadcastReceiver` while holding `mLock`: register the BT
listener (`addBatteryListener`) outside the `synchronized (mLock)` region.
Symmetrically, have `onReceive` copy the level and release `mBroadcastReceiver`
before calling the listener that takes `mLock`.

### 5. `UserController.mLock` ⇄ `UserManagerService.mUsersLock`

![](findings/cand01.png)

Two service monitors across the `am`/`pm` boundary: `UserController.mLock` guards
started-user state, `UserManagerService.mUsersLock` guards the user list. No
documented order exists between them.

- **`mLock` → `mUsersLock`** — `finishUserStopped` holds `mLock` (`:1563`) and via
  `updateUserToLockLU` → `getUserPropertiesInternal`/`hasUserRestriction` takes
  `mUsersLock` (`UserManagerService.java:2732`). UserController `mHandler` thread.
  (Reachability is gated by the delayed-locking config branch.)
- **`mUsersLock` → `mLock`** — `removeUserState` holds `mUsersLock` (`:7430`) and
  calls `getActivityManagerInternal().onUserRemoved` → `UserController.onUserRemoved`,
  `synchronized (mLock)` (`UserController.java:3927`). Removal/Binder thread.

Distinct objects, opposite order, different threads. The forward leg is config-
dependent, which is why this sits below the four above.

**Fix.** Don't call across the service boundary while holding the other service's
lock. On the removal side, move `getActivityManagerInternal().onUserRemoved(...)`
out of `synchronized (mUsersLock)` (`UserManagerService.java:7430-7433`); on the
stop side, hoist the UMS property/restriction queries in `updateUserToLockLU` out
of the `mLock` region. Either break is sufficient.

---

## Lower confidence — worth a runtime/lockdep check

### `BatteryStatsService.mStats` ⇄ `SYSTEM_CLOCK` (`BatteryHistoryStepDetailsProvider.mClock`)

![](findings/cand12.png)

`mClock` is `Clock.SYSTEM_CLOCK`, a process-global singleton shared into the
step-details provider — so the 3-lock SCC is really a two-lock pair, `mStats` vs.
that clock monitor. Forward `mStats → mClock` is synchronous in
`setBatteryStateLocked` → `requestUpdate` `else`-branch
(`BatteryHistoryStepDetailsProvider.java:106`), on a Binder thread; reverse
`mClock → mStats` is on the `mHandler` thread inside the `requestUpdate`
`if`-branch `postDelayed` runnable (`:99` → `AppProfiler.java:2181`). Distinct
monitors, opposite order, different threads — genuine — but whether both orders
are ever simultaneously in flight depends on which threads drive each, and the
boot-gated `if`-branch (`!mSystemReady || mFirstUpdate`) muddies it. Confirm with
lockdep/trace rather than dismiss.

### `ThermalManagerService.mLock` ⇄ `ThermalHalWrapper.mHalLock`

![](findings/cand16.png)

Distinct monitors. `mLock → mHalLock` runs once at boot (`onActivityManagerReady`,
`:253` → `connectToHal`, `:1413`); `mHalLock → mLock` runs in the HAL death
recipient (`serviceDied` holds `mHalLock` at `:1216`, then `resendCurrentTemperatures`
→ service `onTemperatureChanged` takes `mLock` at `:482`). A real cross-thread
inversion, but reachable only if the thermal HAL dies during boot bring-up — low
probability, not impossible.

**Fix (if confirmed).** Release `mHalLock` before the listener callout in
`serviceDied` (post the resend), so HAL-handle state and the service lock never
nest.

---

## Surfaced by the tool, set aside on review

These were reported and then dismissed. Most are the documented limitation —
lock identity is keyed by the field's declaring class, so two fields that alias
**one** `Object` (shared via a getter) look like two locks. A couple follow a
documented total order; a couple have the back-edge severed by an async hop.

| candidate | why it's not a deadlock |
|---|---|
| `HdmiControlService.mLock` ⇄ `HdmiLocalDevice.mLock` | same object — `mLock = service.getServiceLock()`; reentrant, not AB-BA |
| `PinnedSliceState.mLock` ⇄ `SliceManagerService.mLock` | same object — `mLock = mService.getLock()` |
| `JobSchedulerService.mLock` ⇄ `JobServiceContext.mLock` ⇄ `StateController.mLock` | one shared lock — all `= service.getLock()` (the global scheduler lock) |
| `LocationTimeZoneProvider/Controller/Proxy.mSharedLock` | one shared lock — all `= threadingDomain.getLockObject()`, pinned to one thread |
| `MockableLocationProvider.mOwnerLock` ⇄ `ListenerMultiplexer.mMultiplexerLock` ⇄ … | `mOwnerLock` **is** `mMultiplexerLock`; the remaining real arc is unreachable (null-listener guard during construct-then-install) |
| `AudioService.mSettingsLock`/`mHdmiClientLock`/`mVolumeStateLock` | documented total order `mSettingsLock ⊃ mHdmiClientLock ⊃ mVolumeStateLock`; the closing edge is a reentrant re-lock under `mSettingsLock` |
| `AudioDeviceBroker.mDeviceStateLock`/`mDevicesLock`/`mBluetoothAudioStateLock` | documented hierarchy (`mDeviceStateLock` outermost), single `BrokerHandler` thread; reverse edges are reentrant `@GuardedBy("mDeviceStateLock")` re-locks |
| `DisplayPowerController.mLock` ⇄ `DisplayBrightnessController.mLock` | back-edge severed: `switchMode(sendUpdate=false)` never re-enters DPC while DBC's lock is held |
| `DeviceIdleController.this` ⇄ `AlarmManagerService.mLock` | reverse edges severed: a `REPORT_ALARMS_ACTIVE` Handler hop and an explicit "must not be called with mLock held" contract (+ `holdsLock` wtf assert) |
| `RemotePrintService.mLock` ⇄ `UserState.mLock` | the A→B path is spurious (re-enters its own lock); only the one-directional dump path is real |

The first four rows are the headline reason to run `verify` and read the source:
field-granular lock identity over-reports exactly when a singleton lock is handed
around by reference. lockdex never *invents* a lock, but it will split one shared
monitor into several — which a short source read collapses.
