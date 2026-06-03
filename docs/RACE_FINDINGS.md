# Findings — field races in `system_server`

`lockdex races` reconstructs each field's guard from behavior — the lock held on most
of its accesses — and flags the accesses that miss it. The guard is inferred
interprocedurally: a field written inside a helper counts as guarded when every
caller of that helper holds the lock, so no `@GuardedBy` annotation or `*Locked`
naming convention is read. (An earlier cut leaned on the deadlock variant of that
analysis, which treats every public method as unlocked; the result was a top-100
dominated by serialization and caller-holds-lock noise. Crediting the guard from the
callers we can see moved the real bugs to the top — this is that list.)

These are the 100 fields with the most write-conflicts on a build's `services.jar`,
each traced to AOSP source and read by hand. **14 are real data races.** The rest split
into two honest categories:

- **IDENTITY** — the access *does* hold the guard, but under a lock object the tool
  named differently: a lock passed into a constructor (`Watchdog$HandlerChecker`,
  `MagnificationConnectionManager`), an inherited `mLock` (the autofill
  `AbstractPerUserSystemService` / `Session` family), or `synchronized(this)` on a
  receiver the tool reported by type. These are fixable by better lock-identity, not
  bugs.
- **BENIGN** — genuinely lock-free but unraceable: a thread-confined local
  (`VoteSummary`, `LaunchParams`), a freshly-built object written before publication
  (the `SyncStatusInfo`/`ActivityInfo` deserialization paths), a lock-free `dump()` of
  a primitive, or a write inside a `catch`/after `wait()` that is still within the
  monitor.

The 14 real ones share recognizable shapes: a field written **after its
`synchronized` block has already closed** (`UserInfo.flags`/`.partial`,
`ActivityStarter.mLastStarter.*`), a **lock-free read-modify-write on a binder thread**
(`UsbPortManager.mTransactionId`, `AudioService.mVibrateSetting`), or a **lock-free
mutation of a published object** that concurrent lock-holding readers observe
(`NetworkPolicy.limitBytes`, `AccessibilityWindowManager.mActiveWindowId`). Each is
below with its diagram and the racing threads named.

---

### 1. `PowerManagerService.mDirty` — guarded by `mLock` (21/30 writes)
![](findings/race/race001.svg)
**Verdict: IDENTITY.** Every flagged method is a `*Locked` helper reached only from `updatePowerStateLocked`, which itself asserts `Thread.holdsLock(mLock)` at `PowerManagerService.java:2693`; the `mDirty = 0` write at line 2713 sits directly under that assert. All ~30 call sites of `updatePowerStateLocked()` and `userActivityNoUpdateLocked` are inside `synchronized(mLock)` frames. The tool couldn't propagate `mLock` through the deep helper chain.

### 2. `UserInfo.flags` — guarded by `mUsersLock` (18/25 writes)
![](findings/race/race002.svg)
**Verdict: REAL.** `setUserEphemeralUnchecked` (`UserManagerService.java:3651`) and `setUserNonEphemeralUnchecked` (3634) mutate `userData.info.flags` *after* the `synchronized(mUsersLock)` block has closed (it ends at line 3650/3633), holding only `mPackagesLock`; `convertPreCreatedUserIfPossible:6743` mutates `flags` with no `mUsersLock` at all. Concurrent readers take `mUsersLock` — e.g. `getUserInfo` (2642) feeding `LocalService.isUserInitialized:8793`, plus `UserController.startProfiles:1900` — so a binder/handler thread can read `flags` under `mUsersLock` while these writers update it under a different (or no) lock.

### 3. `BatteryStatsImpl.mDischargeScreenDozeUnplugLevel` — guarded by `BatteryStatsImpl` (6/9 writes)
![](findings/race/race003.svg)
**Verdict: IDENTITY.** The write at `BatteryStatsImpl.java:11726` and read at 11707 live in `updateNewDischargeScreenLevelLocked`/`updateOldDischargeScreenLevelLocked`, both `@GuardedBy("this")` private helpers reached only via `noteScreenStateLocked`/`prepareForDumpLocked`/`writeSummaryToParcel`/`resetAllStatsAndHistoryLocked`, all entered under `synchronized(this)`. The guard is the receiver monitor; the tool couldn't propagate it through the helper chain.

### 4. `BatteryStatsImpl.mDischargeScreenOffUnplugLevel` — guarded by `BatteryStatsImpl` (6/9 writes)
![](findings/race/race004.svg)
**Verdict: IDENTITY.** Identical structure: write at `BatteryStatsImpl.java:11734`, read at 11713, both inside `@GuardedBy("this")` discharge-level helpers reached only from `synchronized(this)` entry points. Same receiver-monitor identity the tool failed to equate.

### 5. `BatteryStatsImpl.mDischargeScreenOnUnplugLevel` — guarded by `BatteryStatsImpl` (6/9 writes)
![](findings/race/race005.svg)
**Verdict: IDENTITY.** Same as 3 and 4: write at `BatteryStatsImpl.java:11724`, read at 11701, under `@GuardedBy("this")` helpers entered via `synchronized(this)` callers. Receiver-monitor identity, not a real gap.

### 6. `WallpaperData.mBindSource` — guarded by `WallpaperManagerService.mLock` (7/10 writes)
![](findings/race/race006.svg)
**Verdict: BENIGN.** `parseWallpaperAttributes:405` writes a freshly-constructed, still-thread-local `WallpaperData` built in `loadSettingsLocked` (`WallpaperDataParser.java:157`) before publication; `initializeFallbackWallpaper:4077` writes the fresh `mFallbackWallpaper` and `connectLocked:823` writes under `mLock`. The only two reads are serialization under `mLock` and `dump()`. No concurrent unguarded read of a mutating value.

### 7. `WallpaperData.mWhich` — guarded by `WallpaperManagerService.mLock` (6/9 writes)
![](findings/race/race007.svg)
**Verdict: BENIGN.** The flagged writes set `mWhich` on freshly-constructed unpublished objects during `loadSettingsLocked` (`WallpaperDataParser.java:207`/214) or on the fresh fallback object at `initializeFallbackWallpaper:4082`. `mWhich` is set-once at load/fallback-init; the lock-free FileObserver-thread read at `WallpaperManagerService.java:281` observes a stably-published value, not a concurrent mutation.

### 8. `ZenModeConfig$ZenRule.condition` — guarded by `ZenModeHelper.mConfigLock` (9/11 writes)
![](findings/race/race008.svg)
**Verdict: BENIGN.** Both writes (`ZenModeHelper.java:1660`, 1674) clear `condition` on a freshly-parsed local `config` (`ZenModeConfig.readXml` at line 1654) and its rules, before that config is ever published into `mConfig`. The object is thread-local and unpublished at write time.

### 9. `ZenModeConfig$ZenRule.name` — guarded by `ZenModeHelper.mConfigLock` (4/6 writes)
![](findings/race/race009.svg)
**Verdict: REAL.** `updateZenRulesOnLocaleChange` calls `updateRuleStringsForCurrentLocale(mContext, mDefaultConfig)` at `ZenModeHelper.java:1060` — *outside* the `synchronized(mConfigLock)` that begins at line 1061 — mutating `name` on the long-lived shared `mDefaultConfig` rules (writes at 2260/2263). On a different thread, `readXml` reads those same `mDefaultConfig.automaticRules[].name` under `mConfigLock` (`ZenModeHelper.java:1717-1718`). Locale-change broadcast thread vs. restore/`readXml` thread: a real race.

### 10. `BroadcastQueueImpl.mRunningColdStart` — guarded by `BroadcastQueue.mService` (5/7 writes)
![](findings/race/race010.svg)
**Verdict: IDENTITY.** Every flagged method — `clearRunningColdStart:748`, `onApplicationAttachedLocked:669`, `checkPendingColdStartValidityLocked`, `clearInvalidPendingColdStart`, `isPendingColdStartValid`, `demoteFromRunningLocked`, and `dumpProcessQueues` (via `@GuardedBy("mService") dumpLocked`) — is `@GuardedBy("mService")` and runs under `synchronized(mService)`. `mService` is the constructor-injected `final ActivityManagerService` (`BroadcastQueue.java:48`); the tool couldn't equate that injected-field monitor to the lock held several frames up.

### 11. `ContentProviderConnection.waiting` — guarded by `ContentProviderHelper.mService` (5/7 writes)
![](findings/race/race011.svg)
**Verdict: IDENTITY.** The flagged writes `conn.waiting = true/false` run inside `synchronized (cpr)` after `mService` is released (`ContentProviderHelper.java:654-688`, the `synchronized(mService)` block closes at line 647), and the publish-side writes/reads run inside `synchronized (dst)` (`ContentProviderRecord.onProviderPublishStatusLocked:208`). The real guard is the ContentProviderRecord's own monitor (the field comment says "Protected by the provider lock"), not `mService` — both waiter and publisher hold the same `cpr` lock. The only lock-free read is `toClientString:178`, a dump path.

### 12. `OomAdjuster.mOomAdjUpdateOngoing` — guarded by `ActivityManagerService.mProcLock` (6/9 writes)
![](findings/race/race012.svg)
**Verdict: IDENTITY.** The field is declared `@GuardedBy("mService")` (`OomAdjuster.java:349-350`) and every access — including the flagged sites in `updateOomAdjPendingTargetsLocked` (`OomAdjuster.java:855/861/865`) — holds `mService`. The tool latched onto `mProcLock` (held additionally at the `updateOomAdjLSP:591` sites) and reported the `mService`-only sites as violations, but `mService` is the consistent guard held on every path.

### 13. `VoteSummary.height` — guarded by `DisplayModeDirector.mLock` (4/6 writes)
![](findings/race/race013.svg)
**Verdict: BENIGN.** `VoteSummary` is never a field; the only instances are method-locals created under `mLock` in `DisplayModeDirector.getDesiredDisplayModeSpecs` (`DisplayModeDirector.java:308/348`) that never escape the stack frame. `SizeVote.updateSummary` (`SizeVote.java:59-65`) mutates that thread-confined local and `VoteSummary.toString:438` reads it, all on the same thread holding `mLock`. Unpublished, so no other thread can observe it.

### 14. `VoteSummary.maxRenderFrameRate` — guarded by `DisplayModeDirector.mLock` (5/7 writes)
![](findings/race/race014.svg)
**Verdict: BENIGN.** Same thread-confined-local pattern as #13: `RefreshRateVote$PhysicalVote/RenderVote.updateSummary` (`RefreshRateVote.java:108/71`) mutate a `VoteSummary` that exists only as a stack-local in `getDesiredDisplayModeSpecs`, created and consumed under `mLock`, never stored or published.

### 15. `VoteSummary.width` — guarded by `DisplayModeDirector.mLock` (4/6 writes)
![](findings/race/race015.svg)
**Verdict: BENIGN.** Identical to #13/#14: `SizeVote.updateSummary` (`SizeVote.java:59/64`) writes a method-local `VoteSummary` confined to `getDesiredDisplayModeSpecs` under `mLock`; the object never escapes the frame, so the access is lock-free but unraceable.

### 16. `JobServiceContext.mVerb` — guarded by `JobSchedulerService.mLock` (4/6 writes)
![](findings/race/race016.svg)
**Verdict: IDENTITY.** `mLock = service.getLock()` (`JobServiceContext.java:361`) is exactly the claimed guard. The flagged writes are in `closeAndCleanupJobLocked:1777` and `sendStopMessageLocked:1588` (reached only under `mLock`), the constructor write `:373` is on an unpublished object, and the binder callback enters `synchronized (mLock)` before `doCallbackLocked` (`JobServiceContext.java:1244-1248`). Every real access holds `mLock`.

### 17. `PackageSetting.mimeGroups` — guarded by `PackageManagerServiceInjector.mLock` (4/6 writes)
![](findings/race/race017.svg)
**Verdict: BENIGN.** The flagged `copyMimeGroups` writes (`PackageSetting.java:704/708/713/715`) run from the snapshot copy-constructor (`copyPackageSetting:894`), writing the `mimeGroups` of a brand-new, not-yet-published `PackageSetting`. Live mutation (`setMimeGroup:503`) and reads (`getMimeGroups:1539`) are funneled through `commitPackageStateMutation` under the PMS write lock or served from sealed immutable snapshots.

### 18. `UsbPortManager.mTransactionId` — guarded by `UsbPortManager.mLock` (4/6 writes)
![](findings/race/race018.svg)
**Verdict: REAL.** `enableContaminantDetection` does `++mTransactionId` with no lock held (`UsbPortManager.java:365`), while `setPortRoles` and `resetUsbPort` increment the same field under `synchronized (mLock)` (`UsbPortManager.java:664/675/688`). Both are reachable from distinct binder threads — `UsbService.enableContaminantDetection:1052` and `UsbService.setPortRoles:1015` — so two binder threads race a read-modify-write, producing lost updates / duplicate transaction IDs used to correlate async HAL callbacks.

### 19. `WallpaperData.primaryColors` — guarded by `WallpaperManagerService.mLock` (4/6 writes)
![](findings/race/race019.svg)
**Verdict: BENIGN.** Both writes (`WallpaperDataParser.java:426/442`) target a `WallpaperData` freshly allocated as a local in `loadSettingsLocked` and only installed into `mWallpaperMap` after parsing completes. While `parseWallpaperAttributes` writes `primaryColors`, the object is unpublished; live color updates on installed wallpapers occur under `mLock`.

### 20. `ActivityStarter.mAddingToTask` — guarded by `ActivityTaskManagerService.mGlobalLock` (12/14 writes)
![](findings/race/race020.svg)
**Verdict: BENIGN.** `ActivityStarter` is a per-request pooled object obtained via `obtainStarter` and recycled under `synchronized (mService.mGlobalLock)` (`ActivityStartController.java:551-564`); the flagged `reset:2680` and `set:751` writes run inside the `execute()` start flow, under `mGlobalLock`. Each starter is thread-confined to a single start; the only lock-free read is `dump:3683`. (Contrast #21-25, where the same `set`/`reset` write the *shared* `mLastStarter` after the lock is released.)

### 21. `ActivityStarter.mCanMoveToFrontCode` — guarded by `mGlobalLock` (7/9 writes)
![](findings/race/race021.svg)
**Verdict: REAL.** `ActivityStarter.set` (`ActivityStarter.java:760`) and `reset` (`:2689`) run via `ActivityStartController.onExecutionComplete` → `set`/`recycle`, invoked from `execute()`'s `finally` (`ActivityStarter.java:922`) which runs *after* the `synchronized(mGlobalLock)` block (`:803`,`:838`) has been released. The `set` write targets the shared singleton `mLastStarter`, so two binder threads concurrently in their lock-released `finally` race on `mLastStarter.mCanMoveToFrontCode`. Impact is confined to dump output, but the write is genuinely unguarded and concurrent.

### 22. `ActivityStarter.mDoResume` — guarded by `mGlobalLock` (6/8 writes)
![](findings/race/race022.svg)
**Verdict: REAL.** Same off-lock path: `set` (`ActivityStarter.java:743`) writes the shared `mLastStarter` in `onExecutionComplete`, and `reset` (`:2671`) runs in the lock-released `finally` at `ActivityStarter.java:922`. Concurrent activity starts on different binder threads write `mLastStarter.mDoResume` without `mGlobalLock`.

### 23. `ActivityStarter.mInTask` — guarded by `mGlobalLock` (4/6 writes)
![](findings/race/race023.svg)
**Verdict: REAL.** `set` (`ActivityStarter.java:749`) copies into the shared `mLastStarter` and `reset` (`:2677`) clears it, both via the lock-released `onExecutionComplete`/`recycle` path off `execute()`'s `finally` (`:922`). Two simultaneous starts racing on `mLastStarter.mInTask`, unguarded by the declared `mGlobalLock`.

### 24. `ActivityStarter.mLaunchFlags` — guarded by `mGlobalLock` (11/13 writes)
![](findings/race/race024.svg)
**Verdict: REAL.** Identical mechanism: `set` (`ActivityStarter.java:737`) writes shared `mLastStarter`, `reset` (`:2665`) runs in the post-lock `finally` (`:922`). Concurrent binder threads racing on `mLastStarter.mLaunchFlags` without the lock.

### 25. `ActivityStarter.mTargetRootTask` — guarded by `mGlobalLock` (8/10 writes)
![](findings/race/race025.svg)
**Verdict: REAL.** Same off-lock `onExecutionComplete` path: `set` (`ActivityStarter.java:756`) and `reset` (`:2684`) mutate the shared `mLastStarter` after `mGlobalLock` is dropped (`:922`). Genuine unguarded concurrent write on the singleton.

### 26. `ActivityTaskSupervisor.mUserLeaving` — guarded by `mGlobalLock` (7/10 writes)
![](findings/race/race026.svg)
**Verdict: BENIGN.** The writes at `ActivityTaskSupervisor.java:1668`/`:1727` are inside `findTaskToMoveToFront`, reached only from `moveTaskToFrontLocked` under `synchronized(mGlobalLock)` (`ActivityTaskManagerService.java:2349`) and LockTaskController Locked context; all reads (`:843`, `TaskFragment.java:1635`/`:1924`) are in `*Locked` methods. A transient flag set and cleared within one lock-held critical section.

### 27. `LaunchParamsController$LaunchParams.mPreferredTaskDisplayArea` — guarded by `mGlobalLock` (8/10 writes)
![](findings/race/race027.svg)
**Verdict: BENIGN.** `LaunchParams` is a plain data holder; the `reset`/`set` writes (`LaunchParamsController.java:219`/`:230`) act on thread-confined instances — the controller's scratch fields `mTmpParams`/`mTmpCurrent`/`mTmpResult` used only inside `calculate()` under `mGlobalLock`, or the starter's own confined `mLaunchParams`. Reads in `getOrCreateRootTask` consume a lock-confined params object in the same start.

### 28. `LaunchParamsController$LaunchParams.mWindowingMode` — guarded by `mGlobalLock` (13/15 writes)
![](findings/race/race028.svg)
**Verdict: BENIGN.** Same as #27: `reset`/`set` (`LaunchParamsController.java:221`/`:232`) mutate thread-confined scratch/per-Task `LaunchParams`, and `getOrCreateRootTask` reads (`RootWindowContainer.java:3215`, `TaskDisplayArea.java:867`) operate on a lock-confined instance computed in the same `mGlobalLock`-held activity-start flow.

### 29. `Task.mReuseTask` — guarded by `mGlobalLock` (6/9 writes)
![](findings/race/race029.svg)
**Verdict: BENIGN.** A transient flag set true and cleared false within `performClearTaskForReuse` (`Task.java:1743`/`:1749`) and `performClearTop`, all under `mGlobalLock`. The real reads (`removeChild` `:1650`, `isClearingToReuseTask` `:1980`) are in lock-held Task operations; the only unguarded reads are the lock-free `dump` (`:3825`/`:3829`).

### 30. `AccessibilityServiceInfo.crashed` — guarded by `AbstractAccessibilityServiceConnection.mLock` (2/3 writes)
![](findings/race/race030.svg)
**Verdict: IDENTITY.** `AbstractAccessibilityServiceConnection.mLock` is the injected `AccessibilityManagerService.mLock` (passed at `AccessibilityManagerService.java:3114`, assigned at `AbstractAccessibilityServiceConnection.java:368`), the same lock object. The write at `AccessibilityManagerService.java:2708` is in `readInstalledAccessibilityServiceLocked`, reached only from `readConfigurationForUserStateLocked` which holds that `mLock`; the object is also a freshly-parsed, not-yet-published `AccessibilityServiceInfo`.

### 31. `ProfilerInfo.profileFd` — guarded by `AppProfiler.mProfilerLock` (4/5 writes)
![](findings/race/race031.svg)
**Verdict: BENIGN.** lockdex conflated two distinct `android.app.ProfilerInfo` instances because `profileFd` is keyed by declaring type: the flagged write at `WindowProcessController.java:1522` mutates `mAtm.mProfilerInfo` (under `mGlobalLock`), whereas the dump reads at `AppProfiler.java:2534/2544/2644` read `mProfileData.getProfilerInfo().profileFd` — a *separate* AppProfiler-owned instance, under `@GuardedBy("mService")`. The two objects are never the same reference, so there is no shared field.

### 32. `WaitResult.who` — guarded by `ActivityTaskManagerService.mGlobalLock` (2/3 writes)
![](findings/race/race032.svg)
**Verdict: BENIGN.** The flagged write at `ActivityTaskSupervisor.java:680` runs inside `reportActivityLaunched` under `mGlobalLock`, then `notifyAll()`s waiters blocked in `waitActivityVisibleOrLaunched` on `mGlobalLock.wait()`; the shell-thread read at `ActivityManagerShellCommand.java:960-961` only sees `result` after `startActivityAndWait` returns, so the write happens-before the read through the `mGlobalLock` monitor. Publication is monitor-safe.

### 33. `SyncStatusInfo$Stats.numCancels` — guarded by `SyncStorageEngine.mAuthorities` (2/3 writes)
![](findings/race/race033.svg)
**Verdict: BENIGN.** The flagged write at `SyncStorageEngine.java:2303` is in `readSyncStatusStatsLocked`, reached via `readStatusLocked` whose call sites hold `synchronized(mAuthorities)`, and it populates a not-yet-published fresh `SyncStatusInfo`. The dump read in `SyncManager.dumpSyncState` (`:2539`) operates on a deep copy returned by `getCopyOfAuthorityWithSyncStatus` (copy made under the lock), never the live object.

### 34. `SyncStatusInfo$Stats.numFailures` — guarded by `SyncStorageEngine.mAuthorities` (2/3 writes)
![](findings/race/race034.svg)
**Verdict: BENIGN.** Identical pattern: write at `SyncStorageEngine.java:2299` under `synchronized(mAuthorities)` targets a fresh, unpublished `SyncStatusInfo`. The `SyncManager.dumpSyncState` read (`:2538`) reads `stats` off a private deep copy taken under the lock (`:1448`).

### 35. `SyncStatusInfo$Stats.numSourceFeed` — guarded by `SyncStorageEngine.mAuthorities` (2/3 writes)
![](findings/race/race035.svg)
**Verdict: BENIGN.** Write at `SyncStorageEngine.java:2326` runs under `synchronized(mAuthorities)` into a freshly-constructed `SyncStatusInfo`. The dump read at `SyncManager.java:2534` reads a deep copy from `getCopyOfAuthorityWithSyncStatus`; writer and reader never share an object.

### 36. `SyncStatusInfo$Stats.numSourceLocal` — guarded by `SyncStorageEngine.mAuthorities` (2/3 writes)
![](findings/race/race036.svg)
**Verdict: BENIGN.** Deserialization write at `SyncStorageEngine.java:2310` is lock-held and on an unpublished object; the dump read at `:2531` operates on a copy produced under the lock by `getCopyOfAuthorityWithSyncStatus` (`:1448`).

### 37. `SyncStatusInfo$Stats.numSourceOther` — guarded by `SyncStorageEngine.mAuthorities` (2/3 writes)
![](findings/race/race037.svg)
**Verdict: BENIGN.** Write at `SyncStorageEngine.java:2306` is inside `readSyncStatusStatsLocked` under `synchronized(mAuthorities)` on a fresh `SyncStatusInfo`; the dump read at `SyncManager.java:2536` reads a deep copy snapshotted under the lock.

### 38. `SyncStatusInfo$Stats.numSourcePeriodic` — guarded by `SyncStorageEngine.mAuthorities` (2/3 writes)
![](findings/race/race038.svg)
**Verdict: BENIGN.** Lock-held deserialization write at `SyncStorageEngine.java:2322` into an unpublished object; dump read at `SyncManager.java:2533` against a private copy from `getCopyOfAuthorityWithSyncStatus`.

### 39. `SyncStatusInfo$Stats.numSourcePoll` — guarded by `SyncStorageEngine.mAuthorities` (2/3 writes)
![](findings/race/race039.svg)
**Verdict: BENIGN.** Write at `SyncStorageEngine.java:2314` runs under `synchronized(mAuthorities)` on a fresh `SyncStatusInfo`; dump read at `SyncManager.java:2532` reads a deep copy taken under the lock.

### 40. `SyncStatusInfo$Stats.numSourceUser` — guarded by `SyncStorageEngine.mAuthorities` (2/3 writes)
![](findings/race/race040.svg)
**Verdict: BENIGN.** Lock-held write at `SyncStorageEngine.java:2318` into a not-yet-published `SyncStatusInfo`, dump read at `:2535` on the deep copy from `getCopyOfAuthorityWithSyncStatus`. No cross-thread access to a shared field.

### 41. `SyncStatusInfo$Stats.numSyncs` — guarded by `SyncStorageEngine.mAuthorities` (2/3 writes)
![](findings/race/race041.svg)
**Verdict: BENIGN.** The write at `SyncStorageEngine.java:2296` populates a freshly-deserialized `SyncStatusInfo` during file parse, under `synchronized(mAuthorities)` (or in `init()` before publication). The dump read at `SyncManager.java:2536` operates on a deep copy (`new SyncStatusInfo(...)`, `:1534`); `writeStatusStatsLocked` is a serializer, not a concurrent reader.

### 42. `SyncStatusInfo$Stats.totalElapsedTime` — guarded by `SyncStorageEngine.mAuthorities` (2/3 writes)
![](findings/race/race042.svg)
**Verdict: BENIGN.** Identical to 41: the write at `SyncStorageEngine.java:2292` fills a fresh, unpublished `Stats` during locked file deserialization; the dump consumer at `SyncManager.java:2539` reads a private deep copy.

### 43. `ActivityInfo.name` — guarded by `PackageManagerServiceInjector.mLock` (2/3 writes)
![](findings/race/race043.svg)
**Verdict: BENIGN.** `Task.trimIneffectiveInfo` explicitly clones the object (`new ActivityInfo(info.topActivityInfo)`, `Task.java:3517`) before blanking `name` at `:3526`; the write targets a private, unpublished copy. Cross-subsystem reads operate on different ActivityInfo instances from PMS.

### 44. `ActivityInfo.packageName` — guarded by `PackageManagerServiceInjector.mLock` (2/3 writes)
![](findings/race/race044.svg)
**Verdict: BENIGN.** Same fresh-copy at `Task.java:3517`; the `packageName = ""` write at `:3523` mutates the just-cloned ActivityInfo, not shared PMS state. The stripped object is the one returned to the caller, distinct from any concurrently-read instance.

### 45. `ActivityInfo.processName` — guarded by `PackageManagerServiceInjector.mLock` (2/3 writes)
![](findings/race/race045.svg)
**Verdict: BENIGN.** The `processName = ""` write at `Task.java:3525` follows the same `new ActivityInfo(...)` clone at `:3517`. Write target is unpublished; WM/AM readers read independently-resolved ActivityInfo objects.

### 46. `PermissionInfo.protectionLevel` — guarded by `PackageManagerServiceInjector.mLock` (2/3 writes)
![](findings/race/race046.svg)
**Verdict: BENIGN.** At `PermissionService.kt:403`, `protectionLevel` is normalized on the incoming Binder argument `permissionInfo` — a per-call, parcel-deserialized object owned by the calling thread, not yet published into the registry. Registry readers read distinct `Permission`/`PermissionInfo` instances.

### 47. `ResolveInfo.activityInfo` — guarded by `PackageManagerServiceInjector.mLock` (3/4 writes)
![](findings/race/race047.svg)
**Verdict: BENIGN.** `BroadcastRecord.applySingletonPolicy` reassigns `info.activityInfo` (`BroadcastRecord.java:1160`) on a `ResolveInfo` that lives in a single BroadcastRecord's `receivers` list, with `getActivityInfoForUser` returning a freshly-allocated `new ActivityInfo(aInfo)`. The call runs under the AMS lock at enqueue; receiver ResolveInfo objects are freshly allocated per query. Other subsystems read their own per-query instances.

### 48. `UserInfo.partial` — guarded by `UserManagerService.mUsersLock` (3/4 writes)
![](findings/race/race048.svg)
**Verdict: REAL.** The write `userData.info.partial = true` at `UserManagerService.java:7186` executes after `synchronized(mUsersLock)` has already closed (line 7182), holding only `mPackagesLock`. `getUserInfo` hands the *live* `UserData.info` to callers (`userWithName` returns `orig` unchanged when name is set, `:2662`), so `TrustManagerService.refreshAgentList` (`TrustManagerService.java:809`) and `refreshDeviceLockedForUser` (`:1042`) read `userInfo.partial` on that same object on the TrustManager thread with no `mUsersLock` held.

### 49. `RecognitionEvent.data` — guarded by `FakeSoundTriggerHal.mLock` (2/3 writes)
![](findings/race/race049.svg)
**Verdict: BENIGN.** `ConversionUtil.hidl2aidlRecognitionEvent` assigns `aidlEvent.data` (`ConversionUtil.java:322`) on a `RecognitionEvent` it just constructed and has not yet returned. A stateless static HIDL→AIDL converter building a fresh local; the credited `FakeSoundTriggerHal.mLock` is incidental.

### 50. `NetworkPolicy.limitBytes` — guarded by `NetworkPolicyManagerService.mNetworkPoliciesSecondLock` (5/6 writes)
![](findings/race/race050.svg)
**Verdict: REAL.** `factoryReset` (binder thread, no lock held in its loop) mutates `policy.limitBytes = LIMIT_DISABLED` at `NetworkPolicyManagerService.java:6460` on the *live* policy objects — `getNetworkPolicies` returns `mNetworkPolicy.valueAt(i)` without copying (`:3324`). Meanwhile `updateNetworkRulesNL` (`:2390`), `updateNetworkEnabledNL` (`:2203`), and `updateNotificationsNL` (`:1711`) read `policy.limitBytes` on those same objects under `mNetworkPoliciesSecondLock` on the handler thread — the unguarded factory-reset write races the lock-held readers before the later `setNetworkPolicies` re-lock.

### 51. `WindowManager$LayoutParams.flags` — guarded by `ActivityTaskManagerService.mGlobalLock` (6/7 writes)
![](findings/race/race051.svg)
**Verdict: BENIGN.** `flags` is a public, per-instance field on the app-side parcelable `WindowManager.LayoutParams`, not a `system_server` singleton; the correlation to `mGlobalLock` is spurious aggregation across unrelated `LayoutParams`. The flagged write at `SideFpsToast.onStart:55` mutates that dialog's own freshly-fetched window attributes on the UI thread before `setAttributes`, while the WM-internal reads operate on different `WindowState.mAttrs` instances.

### 52. `DeviceIdleController.mActiveIdleOpCount` — guarded by `DeviceIdleController` (4/5 writes)
![](findings/race/race052.svg)
**Verdict: BENIGN.** The write at `DeviceIdleController.java:4039` is in `stepIdleStateLocked`, a `@GuardedBy("this")` method, and the only flagged reads are in `dump` at lines 5438-5439, inside the `synchronized (this)` block opened at `:5276`. Same `this` monitor guards both writes and reads.

### 53. `DeviceIdleController.mCurLightIdleBudget` — guarded by `DeviceIdleController` (5/6 writes)
![](findings/race/race053.svg)
**Verdict: BENIGN.** Written at `DeviceIdleController.java:3748` in `resetLightIdleManagementLocked` (`@GuardedBy("this")`), and the dump reads at 5472/5474 are inside the `synchronized (this)` block at `:5276`.

### 54. `DeviceIdleController.mNextLightIdleDelay` — guarded by `DeviceIdleController` (3/4 writes)
![](findings/race/race054.svg)
**Verdict: BENIGN.** Written at `DeviceIdleController.java:3745` under `@GuardedBy("this")`; dump reads at 5456/5458 are within the `synchronized (this)` region beginning at `:5276`.

### 55. `DeviceIdleController.mNextLightIdleDelayFlex` — guarded by `DeviceIdleController` (3/4 writes)
![](findings/race/race055.svg)
**Verdict: BENIGN.** Written at `DeviceIdleController.java:3747` under `@GuardedBy("this")`; the dump read at 5461 is inside the same `synchronized (this)` block at `:5276`.

### 56. `StorageManagerService.mPrimaryStorageUuid` — guarded by `StorageManagerService.mLock` (4/5 writes)
![](findings/race/race056.svg)
**Verdict: BENIGN.** The write at `StorageManagerService.java:1929` is in `onMoveStatusLocked` (mLock-held callers), and `writeSettingsLocked`'s read at 2299 is always called under mLock. The only lock-free access is the read at `:3113` in `setPrimaryStorageUuid` after the `synchronized (mLock)` block closes, but it is an atomic reference read serialized by the `mMoveCallback` in-progress invariant; a stale reference there is harmless.

### 57. `Watchdog$HandlerChecker.mCurrentMonitor` — guarded by `Watchdog$HandlerChecker.mLock` (2/3 writes)
![](findings/race/race057.svg)
**Verdict: IDENTITY.** `mLock` is the single shared `Watchdog.mLock` (`Watchdog.java:211`) injected into every `HandlerChecker` (constructed at 505-535). The `run()` write at 381/388 takes `synchronized (mLock)`, while the flagged write at `:328` (`scheduleCheckLocked`) and reads at 362/365 follow caller-holds-lock — their callers run inside `synchronized (mLock)` (`:881`, `:925`/`:934`). Same lock object via an injected field.

### 58. `AccessibilityWindowManager.mActiveWindowId` — guarded by `AccessibilityWindowManager.mLock` (3/4 writes)
![](findings/race/race058.svg)
**Verdict: REAL.** Every other writer of this plain `int` (`AccessibilityWindowManager.java:106`) takes `mLock` via `*Locked` paths driven by window-change callbacks (768, 815, 851, 868, 1777), but `getActiveWindowId` does an unsynchronized read-check-then-write at lines 1758/1760/1762. It is invoked off the input/touch-explorer thread without the lock through `EventDispatcher.populateAccessibilityEvent` (`EventDispatcher.java:204`), racing the locked window-update writes on the a11y handler thread — a torn read and a lost update on `mActiveWindowId`.

### 59. `MagnificationConnectionManager$WindowMagnifier.mTrackingTypingFocusStartTime` — guarded by `MagnificationController.mLock` (2/3 writes)
![](findings/race/race059.svg)
**Verdict: BENIGN.** The field is declared `volatile long` (`MagnificationConnectionManager.java:1116`), and the companion accumulator uses an `AtomicLongFieldUpdater` (line 1264) — the access at `pauseTrackingTypingFocusRecord` (1261/1262/1265) is deliberately lock-free. Atomic volatile long; the mLock correlation is incidental.

### 60. `MagnificationConnectionManager.mConnectionWrapper` — guarded by `MagnificationController.mLock` (3/4 writes)
![](findings/race/race060.svg)
**Verdict: IDENTITY.** `mLock` (`MagnificationConnectionManager.java:136`) is the injected shared `MagnificationController.mLock` (constructor param at 219, assigned 222). The flagged write at `:268` (and 247/257) lives inside the `synchronized (mLock)` block opened at line 239, the same lock all 17 reads use. Flagged only because the guard is `synchronized(mLock)` on an injected field rather than `this`.

### 61. `ActivityManagerService.mDebugApp` — guarded by `ActivityManagerService` (2/3 writes)
![](findings/race/race061.svg)
**Verdict: BENIGN.** The `setDebugApp` write (`ActivityManagerService.java:7620`) and the orig-read (`:7618`) both sit inside `synchronized (this)` at `:7616`, and the dump reads at `:11431`/`:11439` are inside `dumpOtherProcessesInfoLSP`, `@GuardedBy({"this","mProcLock"})`. Every flagged access holds the guard.

### 62. `ActivityManagerService.mWaitForDebugger` — guarded by `ActivityManagerService` (2/3 writes)
![](findings/race/race062.svg)
**Verdict: BENIGN.** Both the write (`ActivityManagerService.java:7622`) and the orig-read (`:7619`) are inside the same `synchronized (this)` block at `:7616`. No reachable concurrent access escapes the guard.

### 63. `ActivityManagerShellCommand$MyActivityController.mState` — guarded by `ActivityManagerShellCommand$MyActivityController` (2/3 writes)
![](findings/race/race063.svg)
**Verdict: REAL.** `run()` writes `mState = STATE_NORMAL` at `ActivityManagerShellCommand.java:2415` and reads it at `:2428`/`:2436` with no synchronization, while the AMS binder thread delivers controller callbacks (`appCrashed` `:2179`, `appNotResponding` `:2239`) that set `mState` under `synchronized(this)` (via `waitControllerLocked`, `:2356`). The shell-command `run` thread and the AMS binder thread race on this field.

### 64. `AppBatteryExemptionTracker$UidStateEventWithBattery.mBatteryUsage` — guarded by `AppRestrictionController.mLock` (2/3 writes)
![](findings/race/race064.svg)
**Verdict: BENIGN.** The flagged write at `AppBatteryExemptionTracker.java:456` runs only on a freshly `clone()`d, still-unpublished event built inside the local `dest` list during the pure merge in `add()` (`:357`–`:367`); the read accessors operate on locals or list-resident events reached via `getBatteryUsageSince` under `mLock`.

### 65. `ErrorDialogController.mWaitDialog` — guarded by `ActivityManagerService.mProcLock` (2/3 writes)
![](findings/race/race065.svg)
**Verdict: BENIGN.** The field is declared `@GuardedBy("mProcLock")` (`ErrorDialogController.java:62`) and `moveWaitingDialogToDefaultDisplay` is itself `@GuardedBy("mProcLock")` (`:388`), so the reads at `:390`/`:396` and write at `:398` all hold the guard; the caller at `:122` also holds `mProcLock`.

### 66. `ProcessList$ProcStateMemTracker.mPendingMemState` — guarded by `AppProfiler.mProfilerLock` (2/3 writes)
![](findings/race/race066.svg)
**Verdict: BENIGN.** The static `computeNextPssTime` write at `ProcessList.java:1520` is reachable only through `AppProfiler.updateNextPssTimeLPf` (`@GuardedBy("mProfilerLock")`), whose three call sites all hold `mProfilerLock` (`AppProfiler.java:1280`, `ActivityManagerService.java:8943`, `:19476`). The tool lost the lock across the static helper.

### 67. `ProcessRecord.mOnewayThread` — guarded by `ActivityManagerService` (2/3 writes)
![](findings/race/race067.svg)
**Verdict: BENIGN.** The `makeInactive` write at `ProcessRecord.java:710` (and `makeActive` at `:697`/`:699`) is `@GuardedBy({"mService","mProcLock"})`, and the flagged read at `:688` is the `@GuardedBy(anyOf={"mService","mProcLock"})` accessor; since every writer holds `mProcLock`, even `mProcLock`-only readers are serialized.

### 68. `ServiceRecord.mFgsNotificationShown` — guarded by `ActivityManagerService$LocalService.this$0` (2/3 writes)
![](findings/race/race068.svg)
**Verdict: BENIGN.** The `mPostDeferredFGSNotifications.run()` write at `ActiveServices.java:3269` executes inside `synchronized (mAm)` (`:3254`); the other writes (`:3232`, `:3341`) and the `*Locked` reads all run under `mAm`, the named guard. The tool failed to equate `mAm`/`this$0` with the AMS lock identity.

### 69. `ServiceRecord.pendingConnectionGroup` — guarded by `ActivityManagerService` (2/3 writes)
![](findings/race/race069.svg)
**Verdict: BENIGN.** `setProcess` is unannotated but every caller is an AM `*Locked` method holding `mAm` (e.g. `realStartServiceLocked`, `ActiveServices.java:6254`), so the write at `ServiceRecord.java:1260` is guarded; the dump reads at `:1034`/`:1036` run via `dumpServiceLocalLocked` under `synchronized(mAm)` (`:8180`).

### 70. `ServiceRecord.pendingConnectionImportance` — guarded by `ActivityManagerService` (2/3 writes)
![](findings/race/race070.svg)
**Verdict: BENIGN.** Same path as #69: the write at `ServiceRecord.java:1260` is reached only through `mAm`-holding `*Locked` callers of `setProcess`, and the dump read at `:1037` runs under `synchronized(mAm)` (`:8180`).

### 71. `ServiceRecord.tracker` — guarded by `ProcessStatsService.mLock` (3/4 writes)
![](findings/race/race071.svg)
**Verdict: IDENTITY.** Every `getTracker()` call site (the writer at `ServiceRecord.java:1184`) is inside a `synchronized(mAm.mProcessStats.mLock)` block (e.g. `ActiveServices.java:1246`, `:1543`), and all flagged reads (`:7637/7103/7044/6934/1675/1872`) are `r.tracker != null` null-checks inside `*Locked` methods under the outer AMS lock. Effectively guarded by the AMS lock as the outer monitor, with `mProcessStats.mLock` as the inner write lock.

### 72. `AppOpsService.mFastWriteScheduled` — guarded by `AppOpsService` (2/3 writes)
![](findings/race/race072.svg)
**Verdict: IDENTITY.** The flagged write at `AppOpsService.java:352` is the first statement inside `synchronized (AppOpsService.this)` opened at `:350` in `mWriteRunner.run()`. The lock object is exactly the declared guard; the tool mis-scored the synchronized-on-`this` block.

### 73. `AppOpsService.mWriteScheduled` — guarded by `AppOpsService` (3/4 writes)
![](findings/race/race073.svg)
**Verdict: IDENTITY.** The write at `AppOpsService.java:351` sits directly under `synchronized (AppOpsService.this)` (`:350`) in `mWriteRunner.run()`, holding the declared guard.

### 74. `AttributedOp.mAccessEvents` — guarded by `AppOpsService` (2/3 writes)
![](findings/race/race074.svg)
**Verdict: IDENTITY.** `finishOrPause` carries `@SuppressWarnings("GuardedBy") // Lock is held on mAppOpsService` (`AttributedOp.java:311`); every entry — `packageRemoved`→`packageRemovedLocked` (`AppOpsService.java:1440`), `onUidStateChanged` (`:1511`), the death-recipient `onClientDeath` (`AttributedOp.java:480`) — holds the AppOpsService monitor before the write at `:332`.

### 75. `AttributedOp.mInProgressEvents` — guarded by `AppOpsService` (3/4 writes)
![](findings/race/race075.svg)
**Verdict: IDENTITY.** Same helper-indirection: the write at `AttributedOp.java:354` is inside `finishOrPause`, reached only from `finished()`/`onClientDeath`/`onUidStateChanged`, all of which acquire `synchronized(mAppOpsService)` (e.g. `:480`, `AppOpsService.java:1440`).

### 76. `AttributedOp.mPausedInProgressEvents` — guarded by `AppOpsService` (3/4 writes)
![](findings/race/race076.svg)
**Verdict: IDENTITY.** `finishPossiblyPaused` is `@SuppressWarnings("GuardedBy") // Lock is held on mAppOpsService` (`AttributedOp.java:368`); the write at `:392` is reached only via `finishOrPause`←`finished`/`onClientDeath`, each under the AppOpsService monitor.

### 77. `AudioDeviceBroker.mBluetoothA2dpSuspendedApplied` — guarded by `AudioDeviceBroker.mBluetoothAudioStateLock` (3/4 writes)
![](findings/race/race077.svg)
**Verdict: BENIGN.** The write at `AudioDeviceBroker.java:1254` is in `reapplyAudioHalBluetoothState`, `@GuardedBy("mBluetoothAudioStateLock")` (`:1241`) with the lock caller-held; the in-method reads at 1244/1258 are under that guard. The only lock-free access is the diagnostic `dump` read of this `boolean` (`:1877`).

### 78. `AudioDeviceBroker.mBluetoothLeSuspendedApplied` — guarded by `AudioDeviceBroker.mBluetoothAudioStateLock` (3/4 writes)
![](findings/race/race078.svg)
**Verdict: BENIGN.** Identical to #77: the write at `AudioDeviceBroker.java:1255` and reads at 1244/1263 are inside `@GuardedBy("mBluetoothAudioStateLock")` `reapplyAudioHalBluetoothState`; the sole unguarded touch is the lock-free `dump` read of the boolean at `:1881`.

### 79. `AudioService.mVibrateSetting` — guarded by `AudioService.mSettingsLock` (2/3 writes)
![](findings/race/race079.svg)
**Verdict: REAL.** `setVibrateSetting` does a read-modify-write of the non-volatile int at `AudioService.java:6898` with no lock held, and `getVibrateSetting` reads it lock-free at `:6890`; both are public binder entry points invokable concurrently from any app's binder thread. The init/settings-reload writes at `:3411`/`:3415` happen under `synchronized(mSettingsLock)`, so a concurrent binder `setVibrateSetting` races (lost update / stale read). Low-impact (deprecated) but a genuine unguarded race.

### 80. `BtHelper.mScoAudioState` — guarded by `BtHelper` (18/19 writes)
![](findings/race/race080.svg)
**Verdict: BENIGN.** The tool inferred the monitor as guard because 18 writers are `synchronized` methods, but the field's real contract is `@GuardedBy("mDeviceBroker.mDeviceStateLock")` (`BtHelper.java:585`). The flagged write in `resetBluetoothSco` (`:587`) is reached only under `synchronized(mDeviceStateLock)`; the `isBluetoothScoRequestedInternally` read (`:501`) runs inside `@GuardedBy("mDeviceStateLock")` `setCommunicationRouteForClient`. The only lock-free access is the single-int read in `dump` (`:1474`).

### 81. `FocusRequester.mFocusLossFadeLimbo` — guarded by `MediaFocusControl.mAudioFocusLock` (3/4 writes)
![](findings/race/race081.svg)
**Verdict: BENIGN.** The flagged write at `FocusRequester.java:516` is inside `frameworkHandleFocusLoss`, `@GuardedBy("MediaFocusControl.mAudioFocusLock")` and reachable only from the guarded `handleFocusLoss`. The flagged `dump` read at `:267` is reached only via `dumpFocusStack`/`dumpExtFocusPolicyFocusOwners`, both iterating inside `synchronized(mAudioFocusLock)` (`MediaFocusControl.java:490`).

### 82. `FocusRequester.mFocusLossReceived` — guarded by `MediaFocusControl.mAudioFocusLock` (2/3 writes)
![](findings/race/race082.svg)
**Verdict: BENIGN.** The write at `FocusRequester.java:403` and reads at `:402/:411/:450/:451/:454` are all within `handleFocusLoss` (`@GuardedBy("...mAudioFocusLock")`). The other flagged reads (`focusLossToString:242`, `toAudioFocusInfo:607`) are invoked only from `dump`/guarded dispatch under `synchronized(mAudioFocusLock)`.

### 83. `MediaFocusControl.mFocusFreezeExemptUids` — guarded by `MediaFocusControl.mAudioFocusLock` (2/3 writes)
![](findings/race/race083.svg)
**Verdict: BENIGN.** The flagged write (`MediaFocusControl.java:1482`) sits in the `catch (RemoteException)` block of `enterAudioFocusFreezeForTest`, entirely inside `synchronized(mAudioFocusLock)` opened at `:1462`. The tool mis-flagged a write lexically within the synchronized block (confused by the catch clause); the guard is held.

### 84. `MediaFocusControl.mFocusFreezerForTest` — guarded by `MediaFocusControl.mAudioFocusLock` (2/3 writes)
![](findings/race/race084.svg)
**Verdict: BENIGN.** Same `enterAudioFocusFreezeForTest` body: the flagged write (`MediaFocusControl.java:1481`, the `catch` clause's `mFocusFreezerForTest = null`) is inside the `synchronized(mAudioFocusLock)` block at `:1462`. A lexical-scope false positive in the catch handler.

### 85. `SpatializerHelper.mCapableSpatLevel` — guarded by `SpatializerHelper` (2/3 writes)
![](findings/race/race085.svg)
**Verdict: BENIGN.** The write at `SpatializerHelper.java:342` (`resetCapabilities`) runs only from `init()` (`synchronized`), so writes are guarded. The flagged read at `:1631` is in the unsynchronized `dump` called on the binder dump thread — but a lock-free read of a plain `int` where a momentarily stale debug value is harmless.

### 86. `SpatializerHelper.mDynSensorCallback` — guarded by `SpatializerHelper` (2/3 writes)
![](findings/race/race086.svg)
**Verdict: BENIGN.** The only flagged violation is the write at `SpatializerHelper.java:1542`, inside `onInitSensors`, declared `synchronized void onInitSensors()` at `:1519`. The write holds the monitor.

### 87. `SpatializerHelper.mSensorManager` — guarded by `SpatializerHelper` (2/3 writes)
![](findings/race/race087.svg)
**Verdict: BENIGN.** The flagged write at `SpatializerHelper.java:1541` is in `onInitSensors`, declared `synchronized` at `:1519`, so it runs under the guard.

### 88. `AutofillInlineSuggestionsRequestSession.mImeSessionInvalidated` — guarded by `AutofillInlineSessionController.mLock` (2/3 writes)
![](findings/race/race088.svg)
**Verdict: IDENTITY.** Both the flagged write (`onCreateInlineSuggestionsRequestLocked`, `:199`) and read (`onInlineSuggestionsResponseLocked`, `:165`) are `@GuardedBy("mLock")`. The session's `mLock` is injected at construction (`:122`); `AutofillInlineSessionController` passes its own `mLock` into the session (`:94`), so the local `mLock` is the same object as the guard.

### 89. `AutofillManagerServiceImpl.mRemoteAugmentedAutofillService` — guarded by `AbstractPerUserSystemService.mLock` (2/3 writes)
![](findings/race/race089.svg)
**Verdict: IDENTITY.** The write (`AutofillManagerServiceImpl.java:1732`) and reads (`:1667`, `:1739`) are all inside `getRemoteAugmentedAutofillServiceLocked`, `@GuardedBy("mLock")`. The class extends `AbstractPerUserSystemService` (`:117`) without shadowing `mLock`, so it resolves to the inherited `public final Object mLock` — the guard object itself.

### 90. `Session$AssistDataReceiverImpl.mPendingFillRequest` — guarded by `AbstractPerUserSystemService.mLock` (5/6 writes)
![](findings/race/race090.svg)
**Verdict: IDENTITY.** The unannotated write at `Session.java:766` (`newAutofillRequestLocked`) is reached only from `requestNewFillResponseLocked` (`:1572`, `:1595`), which is `@GuardedBy("mLock")` (`:1460`). `Session.mLock` is injected (`:1699`) from `AutofillManagerServiceImpl`'s inherited `mLock` at `new Session(..., mLock, ...)` (`:712`), so it is the same object as the `AbstractPerUserSystemService.mLock` guard.

### 91. `Session$AssistDataReceiverImpl.mPendingInlineSuggestionsRequest` — guarded by `AbstractPerUserSystemService.mLock` (2/3 writes)
![](findings/race/race091.svg)
**Verdict: IDENTITY.** The flagged write at `Session.java:768` is inside `newAutofillRequestLocked`, `@GuardedBy("mLock")` (`:764`); `Session.mLock` (`:1699`) is the `lock` object `AutofillManagerServiceImpl` passes from `AbstractPerUserSystemService.mLock`. One object; the write is properly guarded.

### 92. `Session$AssistDataReceiverImpl.mWaitForInlineRequest` — guarded by `AbstractPerUserSystemService.mLock` (2/3 writes)
![](findings/race/race092.svg)
**Verdict: IDENTITY.** Same `newAutofillRequestLocked` (`Session.java:767`), same `@GuardedBy("mLock")` contract (`:764`). The other write site `handleInlineSuggestionRequest` (`:789`) takes `synchronized(mLock)` explicitly, confirming the lock object is the master-derived `AbstractPerUserSystemService.mLock`.

### 93. `Session$ClassificationState.mPendingFieldClassificationRequest` — guarded by `AbstractPerUserSystemService.mLock` (2/3 writes)
![](findings/race/race093.svg)
**Verdict: IDENTITY.** The write in `updateResponseReceived` (`Session.java:7356`) and the read in `ClassificationState.toString` (`:7384`) are both inside methods annotated `@GuardedBy("mLock")` (`:7352`, `:7377`). That `mLock` is the shared per-user/master lock; the flag is the analyzer treating the inherited field as distinct.

### 94. `Session.mClientVulture` — guarded by `AbstractMasterSystemService.mLock` (2/3 writes)
![](findings/race/race094.svg)
**Verdict: IDENTITY.** The write (`Session.java:1901`) and reads (`:1896-1897`) live entirely within `unlinkClientVultureLocked`, `@GuardedBy("mLock")` (`:1894`). `AbstractMasterSystemService.mLock` (`:139`) is the single `Object` propagated down to `Session.mLock`, so these hold exactly the named guard.

### 95. `Session.mSessionState` — guarded by `AbstractMasterSystemService.mLock` (2/3 writes)
![](findings/race/race095.svg)
**Verdict: BENIGN.** The write in `removeFromServiceLocked` (`Session.java:8007`) is under `@GuardedBy("mLock")` (`:7984`); the only unguarded access is the read at `:7406` inside the non-`@GuardedBy` `toString()` — a lock-free debug read of a plain `@SessionState int`, a benign value-tear at worst.

### 96. `Session.mWaitForImeAnimation` — guarded by `AbstractMasterSystemService.mLock` (2/3 writes)
![](findings/race/race096.svg)
**Verdict: IDENTITY.** The write in `resetImeAnimationState` is inside `synchronized(mLock)` (`Session.java:5579-5580`), and the flagged read at `:5844` sits inside the `synchronized(mLock)` block opened at `:5813` within `requestShowFillDialog`. Both touch the master-derived `mLock`.

### 97. `BackupAgentConnectionManager.mCurrentConnection` — guarded by `mAgentConnectLock` (5/6 writes)
![](findings/race/race097.svg)
**Verdict: BENIGN.** The flagged write at `BackupAgentConnectionManager.java:167` is in the `InterruptedException` catch inside the `while` loop of `bindToAgentSynchronous`, fully enclosed by `synchronized(mAgentConnectLock)` (opened `:134`, closed `:183`). It follows `mAgentConnectLock.wait(5000)`; `Object.wait` reacquires the monitor before returning, so the write executes under the named lock.

### 98. `UserBackupManagerService.mClearingData` — guarded by `mClearDataLock` (2/3 writes)
![](findings/race/race098.svg)
**Verdict: BENIGN.** Same wait/reacquire pattern: the write at `UserBackupManagerService.java:1610` is in the `InterruptedException` catch within `synchronized(mClearDataLock)` (opened `:1598`, closed `:1624`), reached after `mClearDataLock.wait(5000)`. The monitor is held when the write runs.

### 99. `UserBackupManagerService.mJournal` — guarded by `mQueueLock` (2/3 writes)
![](findings/race/race099.svg)
**Verdict: REAL.** `mJournal` (`UserBackupManagerService.java:329`) is a plain non-volatile, non-`@GuardedBy` field. The guarded writes at `:2333`/`:2337` run under `synchronized(mQueueLock)`, but `getJournal()` (`:778`) and `setJournal()` (`:782`) are unsynchronized, and `BackupHandler.java:172` reads via `getJournal()` *outside* the `mQueueLock` block it opens at `:173` (backup handler thread), while `parseLeftoverJournals` (`:1073`, posted at init) also reads it lock-free. A backup-thread read races concurrent `mQueueLock`-guarded writes with no happens-before edge.

### 100. `ProgramInfoCache.mComplete` — guarded by `RadioModule.mLock` (2/3 writes)
![](findings/race/race100.svg)
**Verdict: BENIGN.** The flagged write at `aidl/ProgramInfoCache.java:147` is in `updateFromHalProgramListChunk`, `@VisibleForTesting` with no production caller; the real production write path `filterAndApplyChunk` (`:249`) is invoked from `RadioModule.java:147` under `synchronized(mLock)`. The `toString` read (`:102`) is reached only via the dump under `synchronized(mLock)` (`:547`).

---

## What this says about the tool

Crediting the guard interprocedurally is what made this list useful: the families that
used to fill the top — `SyncStatusInfo`/`ActivityInfo` serialization, `*Locked`
helpers whose callers hold the lock — sink to BENIGN/IDENTITY, and genuine
inconsistently-guarded state rises. **14 of the top 100 are real**, and they fall into
three repeatable shapes a reviewer can scan for:

1. **Write after the lock is released** — `UserInfo.flags`/`.partial` mutated past the
   end of the `mUsersLock` block; the whole `ActivityStarter.mLastStarter` cluster
   written in `execute()`'s post-lock `finally`.
2. **Lock-free read-modify-write on a public/binder entry** —
   `UsbPortManager.mTransactionId++`, `AudioService.setVibrateSetting`.
3. **Lock-free mutation of a published object that lock-holding readers observe** —
   `NetworkPolicy.limitBytes` under `factoryReset`, `AccessibilityWindowManager.mActiveWindowId`.

The two non-bug categories each point at one more fix. IDENTITY (≈40 of the 100) is a
lock the tool *should* equate — a constructor-injected `Object`, an inherited `mLock`,
or `synchronized(this)` reported by type — and resolving constructor-arg and inherited
lock identity would clear most of them. BENIGN (≈45) is dominated by thread-confined
locals and freshly-built objects, which a stronger escape/publication filter removes.
Neither is a false alarm a human can't dismiss in seconds with the diagram and the
cited lines — but both are mechanical, and worth removing so the 14 stand alone.
