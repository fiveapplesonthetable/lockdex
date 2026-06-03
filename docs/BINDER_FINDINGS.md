# Findings — locks across Binder IPC in `system_server`

`lockdex binder` flags a hazard distinct from the lock-order cycles in
[`FINDINGS.md`](FINDINGS.md): a lock held while a thread crosses a **process**
boundary. The peer process is outside the analysis, so this is not a provable
cycle — it is the shape that causes cross-process deadlocks, priority inversion,
and ANRs (a remote call stalls while a contended lock is pinned).

On a build's `services.jar`, `lockdex binder` reports, deterministically and in a
few seconds:

- **798** outgoing hold-sites — a lock held at a call site that reaches
  `IBinder.transact` — spread across ~130 distinct locks.
- **161** incoming server entries that take a lock a remote caller can block on.
- **4** high-risk incoming entries — they hold a lock *across their own* outgoing
  transaction (the nested pattern that genuinely deadlocks across processes).

Generated AIDL proxies (`$Stub$Proxy`, which `synchronized(this)` to cache an
interface hash), alloc-site / unresolved monitors, and compiler synthetics are
excluded as noise — only shared lock identities in real service code remain.

---

## What it finds — outgoing (a lock held across an outgoing transaction)

Ranked by how often each lock is held across IPC. The global `system_server`
locks dominate, exactly as expected:

| held across N IPCs | lock |
|---|---|
| 220 | `ActivityTaskManagerService.mGlobalLock` |
| 56 | `ActivityManagerService` (the AMS instance monitor) |
| 43 | `ActivityManagerService.mProcLock` |
| 27 | `AccessibilityManagerService.mLock` |
| 24 | `ActivityManagerService$LocalService` (outer-instance monitor) |
| 21 | `WallpaperManagerService.mLock` |
| 19 | `VibratorManagerService.mLock` |
| 18 | `ImfLock.class` (inputmethod) |
| 17 | `NotificationManagerService.mNotificationLock` |

### Example — `ActivityTaskManagerService.mGlobalLock` across a client callback

![](binder-findings/atms-mGlobalLock-activityDestroyed.svg)

`ActivityClientController.activityDestroyed` holds the WM global lock and, still
holding it, calls into `ActivityRecord.destroyed` which dispatches further — a
chain that reaches an outgoing Binder transaction with `mGlobalLock` pinned the
whole time:

```text
  frameworks/base/.../server/wm/ActivityClientController.java:316
      314      final ActivityRecord r = ActivityRecord.forTokenLocked(token);
      315      if (r != null) {
  >>  316          r.destroyed("activityDestroyed");
      317      }
```

The diagram shows the held lock (red) → the call path → the **Binder IPC** node;
every one of the 220 `mGlobalLock` sites has its own. This is the single most
load-bearing lock in `system_server`, so each place it is pinned across IPC is a
latency cliff for whatever process is on the other end.

The full ranked report (every lock, every site, each with a call-path diagram and
source) is what `lockdex binder <input> --src-root <aosp> --out-dir <dir>` writes;
`--class ActivityManagerService` or `--lock mProcLock` narrows it to one service or
lock and emits the complete set of images for it.

---

## 50 representative hold-sites

Across the top locks — holder, source line, and the local call that leads to the
transaction. Each has a call-path diagram in the `--out-dir` output.

| held lock | held across N IPCs | example holder (file line) | via |
|---|---|---|---|
| `ActivityTaskManagerService.mGlobalLock` | 220 | `ActivityClientController.activityDestroyed`:316 | `ActivityRecord.destroyed` |
| `ActivityTaskManagerService.mGlobalLock` | 220 | `ActivityClientController.activityPaused`:246 | `ActivityRecord.activityPaused` |
| `ActivityTaskManagerService.mGlobalLock` | 220 | `ActivityClientController.activityRelaunched`:343 | `ActivityRecord.finishRelaunching` |
| `ActivityTaskManagerService.mGlobalLock` | 220 | `ActivityClientController.activityStopped`:279 | `ActivityRecord.setState` |
| `am.ActivityManagerService` | 56 | `ActivityManagerService.attachApplication`:4926 | `ActivityManagerService.attachApplicationLocked` |
| `am.ActivityManagerService` | 56 | `ActivityManagerService.batterySendBroadcast`:2815 | `ActivityManagerService.broadcastIntentLocked` |
| `am.ActivityManagerService` | 56 | `ActivityManagerService.bindBackupAgent`:14240 | `ActivityManagerService.startProcessLocked` |
| `am.ActivityManagerService` | 56 | `ActivityManagerService.bindServiceInstance`:14045 | `ActiveServices.bindServiceLocked` |
| `ActivityManagerService.mProcLock` | 43 | `ActivityManagerService$LocalService.updateDeviceIdleTempAllowlist`:16978 | `ActivityManagerService.setUidTempAllowlistStateLSP` |
| `ActivityManagerService.mProcLock` | 43 | `ActivityManagerService.attachApplicationLocked`:4680 | `ActivityManagerService.clearProcessForegroundLocked` |
| `ActivityManagerService.mProcLock` | 43 | `ActivityManagerService.cleanUpApplicationRecordLocked`:13573 | `ActivityManagerService.removeLruProcessLocked` |
| `ActivityManagerService.mProcLock` | 43 | `ActivityManagerService.cleanUpApplicationRecordLocked`:13579 | `ProcessRecord.onCleanupApplicationRecordLSP` |
| `AccessibilityManagerService.mLock` | 27 | `AccessibilityManagerService$1.onReceive`:1162 | `AccessibilityManagerService.restoreEnabledAccessibilityServicesLocked` |
| `AccessibilityManagerService.mLock` | 27 | `AccessibilityManagerService$1.onReceive`:1168 | `AccessibilityManagerService.-$$Nest$mrestoreLegacyDisplayMagnificationNavBarIfNeededLocked` |
| `AccessibilityManagerService.mLock` | 27 | `AccessibilityManagerService$AccessibilityContentObserver.onChange`:6129 | `AccessibilityManagerService.-$$Nest$monUserStateChangedLocked` |
| `AccessibilityManagerService.mLock` | 27 | `AccessibilityManagerService$AccessibilityContentObserver.onChange`:6133 | `AccessibilityManagerService.-$$Nest$monUserStateChangedLocked` |
| `ActivityManagerService$LocalService.this$0` | 24 | `ActivityManagerService$LocalService.broadcastCloseSystemDialogs`:17687 | `ActivityManagerService.broadcastIntentLocked` |
| `ActivityManagerService$LocalService.this$0` | 24 | `ActivityManagerService$LocalService.broadcastGlobalConfigurationChanged`:17621 | `ActivityManagerService.broadcastIntentLocked` |
| `ActivityManagerService$LocalService.this$0` | 24 | `ActivityManagerService$LocalService.broadcastGlobalConfigurationChanged`:17639 | `ActivityManagerService.broadcastIntentLocked` |
| `ActivityManagerService$LocalService.this$0` | 24 | `ActivityManagerService$LocalService.broadcastGlobalConfigurationChanged`:17655 | `ActivityManagerService.broadcastIntentLocked` |
| `WallpaperManagerService.mLock` | 21 | `WallpaperManagerService$WallpaperConnection.lambda$new$5`:1120 | `WallpaperManagerService.-$$Nest$mclearWallpaperLocked` |
| `WallpaperManagerService.mLock` | 21 | `WallpaperManagerService$WallpaperObserver.updateWallpapers`:334 | `WallpaperManagerService.-$$Nest$mloadSettingsLocked` |
| `WallpaperManagerService.mLock` | 21 | `WallpaperManagerService$WallpaperObserver.updateWallpapers`:371 | `WallpaperManagerService.bindWallpaperDescriptionLocked` |
| `WallpaperManagerService.mLock` | 21 | `WallpaperManagerService$WallpaperObserver.updateWallpapers`:374 | `WallpaperManagerService.bindWallpaperComponentLocked` |
| `VibratorManagerService.mLock` | 19 | `VibratorManagerService$1.onReceive`:213 | `VibratorManagerService.-$$Nest$mmaybeClearCurrentAndNextSessionsLocked` |
| `VibratorManagerService.mLock` | 19 | `VibratorManagerService$1.onReceive`:220 | `VibratorManagerService.-$$Nest$mmaybeClearCurrentAndNextSessionsLocked` |
| `VibratorManagerService.mLock` | 19 | `VibratorManagerService$2.onOpChanged`:237 | `VibratorManagerService.-$$Nest$mmaybeClearCurrentAndNextSessionsLocked` |
| `VibratorManagerService.mLock` | 19 | `VibratorManagerService$ExternalVibrationCallbacks.onExternalVibrationReleased`:1965 | `VibratorManagerService.-$$Nest$mmaybeStartNextSessionLocked` |
| `ImfLock.class` | 18 | `InputMethodBindingController$2.onBindingDied`:385 | `InputMethodBindingController.unbindCurrentMethod` |
| `ImfLock.class` | 18 | `InputMethodBindingController$2.onServiceConnected`:397 | `InputMethodBindingController.unbindCurrentMethod` |
| `ImfLock.class` | 18 | `InputMethodManagerService$Lifecycle.lambda$onUserStarting$1`:1097 | `InputMethodManagerService.onUserReadyLocked` |
| `ImfLock.class` | 18 | `InputMethodManagerService$LocalServiceImpl.onSwitchKeyboardLayoutShortcut`:5844 | `InputMethodManagerService.-$$Nest$mswitchKeyboardLayoutLocked` |
| `NotificationManagerService.mNotificationLock` | 17 | `NotificationAttentionHelper$3.onReceive`:1709 | `NotificationAttentionHelper.updateLightsLocked` |
| `NotificationManagerService.mNotificationLock` | 17 | `NotificationAttentionHelper$3.onReceive`:1715 | `NotificationAttentionHelper.updateLightsLocked` |
| `NotificationManagerService.mNotificationLock` | 17 | `NotificationAttentionHelper$3.onReceive`:1721 | `NotificationAttentionHelper.updateLightsLocked` |
| `NotificationManagerService.mNotificationLock` | 17 | `NotificationAttentionHelper$SettingsObserver.onChange`:1787 | `NotificationAttentionHelper.updateLightsLocked` |
| `VpnManagerService.mVpns` | 15 | `VpnManagerService.deleteVpnProfile`:340 | `Vpn.deleteVpnProfile` |
| `VpnManagerService.mVpns` | 15 | `VpnManagerService.factoryReset`:984 | `VpnManagerService.setAlwaysOnVpnPackage` |
| `VpnManagerService.mVpns` | 15 | `VpnManagerService.factoryReset`:994 | `VpnManagerService.setLockdownTracker` |
| `VpnManagerService.mVpns` | 15 | `VpnManagerService.factoryReset`:1004 | `VpnManagerService.prepareVpn` |
| `AppRestrictionController.mLock` | 15 | `AppBatteryTracker.dump`:859 | `AppRestrictionController.getUidBatteryExemptedUsageSince` |
| `AppRestrictionController.mLock` | 15 | `AppBatteryTracker.updateBatteryUsageStatsAndCheck`:429 | `AppBatteryTracker.scheduleBatteryUsageStatsUpdateIfNecessary` |
| `AppRestrictionController.mLock` | 15 | `AppFGSTracker.handleForegroundServicesChanged`:473 | `AppFGSTracker$PackageDurations.setForegroundServiceType` |
| `AppRestrictionController.mLock` | 15 | `AppFGSTracker.handleForegroundServicesChanged`:243 | `AppFGSTracker$PackageDurations.addEvent` |
| `BatteryStatsService.mStats` | 15 | `BatteryStatsService.create`:460 | `BatteryStatsImpl.readLocked` |
| `BatteryStatsService.mStats` | 15 | `BatteryStatsService.doEnableOrDisable`:3062 | `BatteryStatsImpl.setPretendScreenOff` |
| `BatteryStatsService.mStats` | 15 | `BatteryStatsService.dumpUnmonitored`:3167 | `BatteryStatsImpl.resetAllStatsAndHistoryLocked` |
| `BatteryStatsService.mStats` | 15 | `BatteryStatsService.dumpUnmonitored`:3176 | `BatteryStatsImpl.resetAllStatsAndHistoryLocked` |
| `ProcessList.mService` | 14 | `ProcessErrorStateRecord.appNotResponding`:682 | `ProcessRecordInternal.killLocked` |
| `ProcessList.mService` | 14 | `ProcessErrorStateRecord.appNotResponding`:685 | `ProcessRecordInternal.killLocked` |

---

## High-risk — incoming entries that hold a lock across their own outgoing call

These four are the dangerous case: a method a remote process can invoke acquires a
lock and *then itself* calls out over Binder while holding it. If the downstream
process calls back into a path that needs the same lock, the two processes
deadlock — and neither side can see the cycle from its own code.

![](binder-findings/high-bugreport-binderDied.svg)

All four are in `BugreportManagerServiceImpl`, holding `mLock` across an outgoing
call:

1. `BugreportManagerServiceImpl$DumpstateListener.binderDied` — `mLock`,
   `Slogf.sMessageBuilder`
2. `BugreportManagerServiceImpl.preDumpUiData` — `mLock`
3. `BugreportManagerServiceImpl.retrieveBugreport` — `mLock`,
   `BugreportFileManager.mLock`, `WatchableImpl.mObservers`
4. `BugreportManagerServiceImpl.startBugreport` — same set

`binderDied` is itself a Binder callback (a death-recipient), so it runs on a
Binder thread holding `mLock` while it reaches another transaction — the kind of
nested cross-process hold worth a second look during review.

---

## How to read a finding

A reported site is a real bytecode fact: at this instruction the lock is on the
monitor stack (or a `j.u.c` lock is held), and the call made here transitively
reaches `IBinder.transact`. The lock is therefore held for the entire duration of
the cross-process call.

What the tool does *not* decide is whether a given hold is a bug — many are
deliberate and safe (the callee is a fast `oneway`, or the protocol guarantees the
peer never re-enters). It is a ranked, source-anchored worklist: start at the
locks held across the most transactions, and at the four high-risk entries.

## Caveats

- The call graph over-approximates at megamorphic sites, so a held lock can be
  attributed to a transaction it does not actually reach at runtime.
- `oneway` vs two-way transactions are not yet distinguished; a `oneway` hold is
  lower risk (fire-and-forget) than a blocking one.
- Locks reached only through unresolved values are dropped, so this under-reports
  rather than inventing hazards.
