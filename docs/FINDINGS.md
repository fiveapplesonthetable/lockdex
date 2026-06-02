# Example findings — `system_server`

Candidate lock-order (AB-BA) inversions `lockdex` reported on a build's
`services.jar`, each traced back to source with `lockdex verify`.

> **These are candidates, not confirmed bugs.** A reported cycle is a real
> pair of opposite-order lock acquisitions in the bytecode; whether it can
> actually deadlock further depends on the two sites running on different
> threads concurrently, which lockdex deliberately does not guess. Read each
> against the source before drawing conclusions. Generated, not hand-picked.

Reproduce:

```sh
lockdex verify "$ANDROID_BUILD_TOP/out" \
    --src-root "$ANDROID_BUILD_TOP/frameworks/base" --max-locks 3 --out-dir ./cycles
```

| # | locks | diagram |
|---|-------|---------|
| 1 | `UserController.mLock` ⇄ `UserManagerService.mUsersLock` | [cand01](#1-usercontrollermlock-usermanagerservicemuserslock) |
| 2 | `DisplayPowerController.mLock` ⇄ `DisplayBrightnessController.mLock` | [cand02](#2-displaypowercontrollermlock-displaybrightnesscontrollermlock) |
| 3 | `BatteryController$LocalBluetoothBatteryManager.mBroadcastReceiver` ⇄ `BatteryController.mLock` | [cand03](#3-batterycontrollerlocalbluetoothbatterymanagermbroadcastreceiver-batterycontrollermlock) |
| 4 | `LockSettingsService.mSeparateChallengeLock` ⇄ `LockSettingsService.mSpManager` | [cand04](#4-locksettingsservicemseparatechallengelock-locksettingsservicemspmanager) |
| 5 | `DeviceIdleController$LocalService.this$0` ⇄ `AlarmManagerService.mLock` | [cand05](#5-deviceidlecontrollerlocalservicethis0-alarmmanagerservicemlock) |
| 6 | `RemoteTaskStore.mRemoteDeviceTaskLists` ⇄ `RemoteTaskStore.mRemoteTaskListeners` | [cand06](#6-remotetaskstoremremotedevicetasklists-remotetaskstoremremotetasklisteners) |
| 7 | `HdmiControlService.mLock` ⇄ `HdmiLocalDevice.mLock` | [cand07](#7-hdmicontrolservicemlock-hdmilocaldevicemlock) |
| 8 | `OneTimePermissionUserManager$PackageInactivityListener.mInnerLock` ⇄ `OneTimePermissionUserManager.mLock` | [cand08](#8-onetimepermissionusermanagerpackageinactivitylistenerminnerlock-onetimepermissionusermanagermlock) |
| 9 | `RemotePrintService.mLock` ⇄ `UserState.mLock` | [cand09](#9-remoteprintservicemlock-userstatemlock) |
| 10 | `PinnedSliceState.mLock` ⇄ `SliceManagerService.mLock` | [cand10](#10-pinnedslicestatemlock-slicemanagerservicemlock) |
| 11 | `JobSchedulerService.mLock` ⇄ `JobServiceContext.mLock` ⇄ `StateController.mLock` | [cand11](#11-jobschedulerservicemlock-jobservicecontextmlock-statecontrollermlock) |
| 12 | `AppProfiler.mProcessCpuTracker` ⇄ `BatteryStatsService.mStats` ⇄ `BatteryHistoryStepDetailsProvider.mClock` | [cand12](#12-appprofilermprocesscputracker-batterystatsservicemstats-batteryhistorystepdetailsprovidermclock) |
| 13 | `AudioService.mHdmiClientLock` ⇄ `AudioService.mSettingsLock` ⇄ `AudioService.mVolumeStateLock` | [cand13](#13-audioservicemhdmiclientlock-audioservicemsettingslock-audioservicemvolumestatelock) |
| 14 | `AudioDeviceBroker.mBluetoothAudioStateLock` ⇄ `AudioDeviceBroker.mDeviceStateLock` ⇄ `AudioDeviceInventory.mDevicesLock` | [cand14](#14-audiodevicebrokermbluetoothaudiostatelock-audiodevicebrokermdevicestatelock-audiodeviceinventorymdeviceslock) |
| 15 | `ListenerMultiplexer.mMultiplexerLock` ⇄ `DelegateLocationProvider.mInitializationLock` ⇄ `MockableLocationProvider.mOwnerLock` | [cand15](#15-listenermultiplexermmultiplexerlock-delegatelocationproviderminitializationlock-mockablelocationprovidermownerlock) |
| 16 | `ThermalManagerService$TemperatureWatcher.mSamples` ⇄ `ThermalManagerService$ThermalHalWrapper.mHalLock` ⇄ `ThermalManagerService.mLock` | [cand16](#16-thermalmanagerservicetemperaturewatchermsamples-thermalmanagerservicethermalhalwrappermhallock-thermalmanagerservicemlock) |
| 17 | `LocationTimeZoneProvider.mSharedLock` ⇄ `LocationTimeZoneProviderController.mSharedLock` ⇄ `LocationTimeZoneProviderProxy.mSharedLock` | [cand17](#17-locationtimezoneprovidermsharedlock-locationtimezoneprovidercontrollermsharedlock-locationtimezoneproviderproxymsharedlock) |

## 1. `UserController.mLock` ⇄ `UserManagerService.mUsersLock`

![candidate 1](findings/cand01.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`UserController.mLock` → `UserManagerService.mUsersLock`** (7×)  
- holds `UserController.mLock` at `services/core/java/com/android/server/am/UserController.java:1584`  
- path: `UserController.finishUserStopped  ->  UserController.getUserInfo  ->  UserManagerService.getUserInfo`  
- acquires `UserManagerService.mUsersLock` at `services/core/java/com/android/server/pm/UserManagerService.java:1057` — `synchronized (mUms.mUsersLock) {`  

**`UserManagerService.mUsersLock` → `UserController.mLock`** (1×)  
- holds `UserManagerService.mUsersLock` at `services/core/java/com/android/server/pm/UserManagerService.java:7432`  
- path: `UserManagerService.removeUserState  ->  ActivityManagerService$LocalService.onUserRemoved  ->  UserController.onUserRemoved`  
- acquires `UserController.mLock` at `services/core/java/com/android/server/am/UserController.java:520` — `synchronized (mLock) {`  

---

## 2. `DisplayPowerController.mLock` ⇄ `DisplayBrightnessController.mLock`

![candidate 2](findings/cand02.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`DisplayPowerController.mLock` → `DisplayBrightnessController.mLock`** (1×)  
- holds `DisplayPowerController.mLock` at `services/core/java/com/android/server/display/DisplayPowerController.java:976`  
- path: `DisplayPowerController.stop  ->  DisplayBrightnessController.stop  ->  DisplayBrightnessController.getAutoBrightnessFallbackStrategy`  
- acquires `DisplayBrightnessController.mLock` at `services/core/java/com/android/server/display/brightness/DisplayBrightnessController.java:178` — `synchronized (mLock) {`  

**`DisplayBrightnessController.mLock` → `DisplayPowerController.mLock`** (1×)  
- holds `DisplayBrightnessController.mLock` at `services/core/java/com/android/server/display/brightness/DisplayBrightnessController.java:179`  
- path: `DisplayBrightnessController.updateBrightness  ->  DisplayBrightnessStrategySelector.selectStrategy  ->  DisplayBrightnessStrategySelector.isAutomaticBrightnessStrategyValid  ->  AutomaticBrightnessStrategy.setAutoBrightnessState  ->  AutomaticBrightnessStrategy.switchMode  ->  AutomaticBrightnessController.switchMode  ->  AutomaticBrightnessController.updateAutoBrightness  ->  DisplayPowerController.updateBrightness  ->  DisplayPowerController.sendUpdatePowerState`  
- acquires `DisplayPowerController.mLock` at `services/core/java/com/android/server/display/DisplayPowerController.java:813` — `synchronized (mLock) {`  

---

## 3. `BatteryController$LocalBluetoothBatteryManager.mBroadcastReceiver` ⇄ `BatteryController.mLock`

![candidate 3](findings/cand03.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`BatteryController$LocalBluetoothBatteryManager.mBroadcastReceiver` → `BatteryController.mLock`** (1×)  
- holds `BatteryController$LocalBluetoothBatteryManager.mBroadcastReceiver` at `services/core/java/com/android/server/input/BatteryController.java:967`  
- path: `BatteryController$LocalBluetoothBatteryManager$1.onReceive  ->  BatteryController$$ExternalSyntheticLambda4.onBluetoothBatteryChanged  ->  BatteryController.$r8$lambda$kHxElP6jGL2CI2h9-PGs0oeXj6g  ->  BatteryController.handleBluetoothBatteryLevelChange`  
- acquires `BatteryController.mLock` at `services/core/java/com/android/server/input/BatteryController.java:137` — `synchronized (mLock) {`  

**`BatteryController.mLock` → `BatteryController$LocalBluetoothBatteryManager.mBroadcastReceiver`** (4×)  
- holds `BatteryController.mLock` at `services/core/java/com/android/server/input/BatteryController.java:481`  
- path: `BatteryController$1.onInputDeviceAdded  ->  BatteryController$UsiDeviceMonitor.<init>  ->  BatteryController$DeviceMonitor.<init>  ->  BatteryController$DeviceMonitor.configureDeviceMonitor  ->  BatteryController.-$$Nest$mupdateBluetoothBatteryMonitoring  ->  BatteryController.updateBluetoothBatteryMonitoring  ->  BatteryController$LocalBluetoothBatteryManager.addBatteryListener`  
- acquires `BatteryController$LocalBluetoothBatteryManager.mBroadcastReceiver` at `services/core/java/com/android/server/input/BatteryController.java:964` — `synchronized (mBroadcastReceiver) {`  

---

## 4. `LockSettingsService.mSeparateChallengeLock` ⇄ `LockSettingsService.mSpManager`

![candidate 4](findings/cand04.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`LockSettingsService.mSeparateChallengeLock` → `LockSettingsService.mSpManager`** (3×)  
- holds `LockSettingsService.mSeparateChallengeLock` at `services/core/java/com/android/server/locksettings/LockSettingsService.java:1922`  
- path: `LockSettingsService.setLockCredential  ->  LockSettingsService.doVerifyCredential`  
- acquires `LockSettingsService.mSpManager` at `services/core/java/com/android/server/locksettings/LockSettingsService.java:953` — `synchronized (mSpManager) {`  

**`LockSettingsService.mSpManager` → `LockSettingsService.mSeparateChallengeLock`** (3×)  
- holds `LockSettingsService.mSpManager` at `services/core/java/com/android/server/locksettings/LockSettingsService.java:954`  
- path: `LockSettingsService$1.run  ->  LockSettingsService.-$$Nest$mtieProfileLockIfNecessary  ->  LockSettingsService.tieProfileLockIfNecessary  ->  LockSettingsService.getSeparateProfileChallengeEnabledInternal`  
- acquires `LockSettingsService.mSeparateChallengeLock` at `services/core/java/com/android/server/locksettings/LockSettingsService.java:1368` — `synchronized (mSeparateChallengeLock) {`  

---

## 5. `DeviceIdleController$LocalService.this$0` ⇄ `AlarmManagerService.mLock`

![candidate 5](findings/cand05.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`DeviceIdleController$LocalService.this$0` → `AlarmManagerService.mLock`** (1×)  
- holds `DeviceIdleController$LocalService.this$0` at `apex/jobscheduler/service/java/com/android/server/DeviceIdleController.java:2323`  
- path: `DeviceIdleController$LocalService.onConstraintStateChanged  ->  DeviceIdleController.-$$Nest$monConstraintStateChangedLocked  ->  DeviceIdleController.onConstraintStateChangedLocked  ->  DeviceIdleController.becomeInactiveIfAppropriateLocked  ->  DeviceIdleController.verifyAlarmStateLocked  ->  AlarmManagerService$LocalService.isIdling  ->  AlarmManagerService.-$$Nest$misIdlingImpl  ->  AlarmManagerService.isIdlingImpl`  
- acquires `AlarmManagerService.mLock` at `apex/jobscheduler/service/java/com/android/server/alarm/AlarmManagerService.java:866` — `synchronized (mLock) {`  

**`AlarmManagerService.mLock` → `DeviceIdleController$LocalService.this$0`** (2×)  
- holds `AlarmManagerService.mLock` at `apex/jobscheduler/service/java/com/android/server/alarm/AlarmManagerService.java:1990`  
- path: `(not reconstructed — chain too deep or via an over-approximated call)`  

---

## 6. `RemoteTaskStore.mRemoteDeviceTaskLists` ⇄ `RemoteTaskStore.mRemoteTaskListeners`

![candidate 6](findings/cand06.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`RemoteTaskStore.mRemoteDeviceTaskLists` → `RemoteTaskStore.mRemoteTaskListeners`** (1×)  
- holds `RemoteTaskStore.mRemoteDeviceTaskLists` at `services/companion/java/com/android/server/companion/datatransfer/continuity/tasks/RemoteTaskStore.java:179`  
- path: `RemoteTaskStore.removeDevice  ->  RemoteTaskStore.notifyListeners`  
- acquires `RemoteTaskStore.mRemoteTaskListeners` at `services/companion/java/com/android/server/companion/datatransfer/continuity/tasks/RemoteTaskStore.java:128` — `synchronized (mRemoteTaskListeners) {`  

**`RemoteTaskStore.mRemoteTaskListeners` → `RemoteTaskStore.mRemoteDeviceTaskLists`** (2×)  
- holds `RemoteTaskStore.mRemoteTaskListeners` at `services/companion/java/com/android/server/companion/datatransfer/continuity/tasks/RemoteTaskStore.java:130`  
- path: `RemoteTaskStore.addListener  ->  RemoteTaskStore.getMostRecentTasks`  
- acquires `RemoteTaskStore.mRemoteDeviceTaskLists` at `services/companion/java/com/android/server/companion/datatransfer/continuity/tasks/RemoteTaskStore.java:52` — `synchronized (mRemoteDeviceTaskLists) {`  

---

## 7. `HdmiControlService.mLock` ⇄ `HdmiLocalDevice.mLock`

![candidate 7](findings/cand07.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`HdmiControlService.mLock` → `HdmiLocalDevice.mLock`** (3×)  
- holds `HdmiControlService.mLock` at `services/core/java/com/android/server/hdmi/HdmiControlService.java:1507`  
- path: `HdmiControlService$22.onAllocated  ->  HdmiCecLocalDevice.setDeviceInfo`  
- acquires `HdmiLocalDevice.mLock` at `services/core/java/com/android/server/hdmi/HdmiCecLocalDevice.java:716` — `synchronized (mLock) {`  

**`HdmiLocalDevice.mLock` → `HdmiControlService.mLock`** (12×)  
- holds `HdmiLocalDevice.mLock` at `services/core/java/com/android/server/hdmi/HdmiCecLocalDeviceAudioSystem.java:260`  
- path: `HdmiCecLocalDeviceAudioSystem.onStandby  ->  HdmiControlService.setActiveSource`  
- acquires `HdmiControlService.mLock` at `services/core/java/com/android/server/hdmi/HdmiControlService.java:771` — `synchronized (mLock) {`  

---

## 8. `OneTimePermissionUserManager$PackageInactivityListener.mInnerLock` ⇄ `OneTimePermissionUserManager.mLock`

![candidate 8](findings/cand08.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`OneTimePermissionUserManager$PackageInactivityListener.mInnerLock` → `OneTimePermissionUserManager.mLock`** (4×)  
- holds `OneTimePermissionUserManager$PackageInactivityListener.mInnerLock` at `services/core/java/com/android/server/pm/permission/OneTimePermissionUserManager.java:264`  
- path: `OneTimePermissionUserManager$PackageInactivityListener.<init>  ->  OneTimePermissionUserManager$PackageInactivityListener.onPackageInactiveLocked`  
- acquires `OneTimePermissionUserManager.mLock` at `services/core/java/com/android/server/pm/permission/OneTimePermissionUserManager.java:82` — `synchronized (mLock) {`  

**`OneTimePermissionUserManager.mLock` → `OneTimePermissionUserManager$PackageInactivityListener.mInnerLock`** (3×)  
- holds `OneTimePermissionUserManager.mLock` at `services/core/java/com/android/server/pm/permission/OneTimePermissionUserManager.java:88`  
- path: `OneTimePermissionUserManager$1.onReceive  ->  OneTimePermissionUserManager$PackageInactivityListener.-$$Nest$mcancel  ->  OneTimePermissionUserManager$PackageInactivityListener.cancel`  
- acquires `OneTimePermissionUserManager$PackageInactivityListener.mInnerLock` at `services/core/java/com/android/server/pm/permission/OneTimePermissionUserManager.java:263` — `synchronized (mInnerLock) {`  

---

## 9. `RemotePrintService.mLock` ⇄ `UserState.mLock`

![candidate 9](findings/cand09.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`RemotePrintService.mLock` → `UserState.mLock`** (1×)  
- holds `RemotePrintService.mLock` at `services/print/java/com/android/server/print/RemotePrintService.java:654`  
- path: `RemotePrintService$RemoteServiceConneciton.onServiceConnected  ->  RemotePrintService.-$$Nest$mhandleBinderDied  ->  RemotePrintService.handleBinderDied  ->  UserState.onServiceDied`  
- acquires `UserState.mLock` at `services/print/java/com/android/server/print/UserState.java:171` — `synchronized (mLock) {`  

**`UserState.mLock` → `RemotePrintService.mLock`** (1×)  
- holds `UserState.mLock` at `services/print/java/com/android/server/print/UserState.java:865`  
- path: `UserState.dump  ->  RemotePrintService.dump`  
- acquires `RemotePrintService.mLock` at `services/print/java/com/android/server/print/RemotePrintService.java:457` — `synchronized (mLock) {`  

---

## 10. `PinnedSliceState.mLock` ⇄ `SliceManagerService.mLock`

![candidate 10](findings/cand10.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`PinnedSliceState.mLock` → `SliceManagerService.mLock`** (1×)  
- holds `PinnedSliceState.mLock` at `services/core/java/com/android/server/slice/PinnedSliceState.java:180`  
- path: `PinnedSliceState.handleRecheckListeners  ->  PinnedSliceState.checkSelfRemove  ->  SliceManagerService.removePinnedSlice`  
- acquires `SliceManagerService.mLock` at `services/core/java/com/android/server/slice/SliceManagerService.java:138` — `synchronized (mLock) {`  

**`SliceManagerService.mLock` → `PinnedSliceState.mLock`** (1×)  
- holds `SliceManagerService.mLock` at `services/core/java/com/android/server/slice/SliceManagerService.java:359`  
- path: `SliceManagerService.removePinnedSlice  ->  PinnedSliceState.destroy  ->  PinnedSliceState.setSlicePinned`  
- acquires `PinnedSliceState.mLock` at `services/core/java/com/android/server/slice/PinnedSliceState.java:76` — `synchronized (mLock) {`  

---

## 11. `JobSchedulerService.mLock` ⇄ `JobServiceContext.mLock` ⇄ `StateController.mLock`

![candidate 11](findings/cand11.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`JobSchedulerService.mLock` → `JobServiceContext.mLock`** (10×)  
- holds `JobSchedulerService.mLock` at `apex/jobscheduler/service/java/com/android/server/job/JobSchedulerService.java:1439`  
- path: `JobSchedulerService$3.onReceive  ->  JobSchedulerService.-$$Nest$mcancelJobsForPackageAndUidLocked  ->  JobSchedulerService.cancelJobsForPackageAndUidLocked  ->  JobSchedulerService.cancelJobImplLocked  ->  JobConcurrencyManager.stopJobOnServiceContextLocked  ->  JobServiceContext.cancelExecutingJobLocked  ->  JobServiceContext.doCancelLocked  ->  JobServiceContext.handleCancelLocked  ->  JobServiceContext.sendStopMessageLocked  ->  JobServiceContext.closeAndCleanupJobLocked  ->  JobConcurrencyManager.onJobCompletedLocked  ->  JobConcurrencyManager.startJobLocked  ->  JobServiceContext.executeRunnableJob`  
- acquires `JobServiceContext.mLock` at `apex/jobscheduler/service/java/com/android/server/job/JobServiceContext.java:387` — `synchronized (mLock) {`  

**`JobSchedulerService.mLock` → `StateController.mLock`** (20×)  
- holds `JobSchedulerService.mLock` at `apex/jobscheduler/service/java/com/android/server/job/JobSchedulerService.java:1439`  
- path: `JobSchedulerService$3.onReceive  ->  TimeController.reevaluateStateLocked  ->  TimeController.checkExpiredDeadlinesAndResetAlarm`  
- acquires `StateController.mLock` at `apex/jobscheduler/service/java/com/android/server/job/controllers/TimeController.java:238` — `synchronized (mLock) {`  

**`JobServiceContext.mLock` → `JobSchedulerService.mLock`** (5×)  
- holds `JobServiceContext.mLock` at `apex/jobscheduler/service/java/com/android/server/job/JobServiceContext.java:1213`  
- path: `JobServiceContext$JobServiceHandler.handleMessage  ->  JobServiceContext.handleOpTimeoutLocked  ->  JobServiceContext.closeAndCleanupJobLocked  ->  JobSchedulerService.getUidProcState`  
- acquires `JobSchedulerService.mLock` at `apex/jobscheduler/service/java/com/android/server/job/JobSchedulerService.java:532` — `synchronized (mLock) {`  

**`JobServiceContext.mLock` → `StateController.mLock`** (5×)  
- holds `JobServiceContext.mLock` at `apex/jobscheduler/service/java/com/android/server/job/JobServiceContext.java:1213`  
- path: `JobServiceContext$JobServiceHandler.handleMessage  ->  JobServiceContext.handleOpTimeoutLocked  ->  JobSchedulerService.isReadyToBeExecutedLocked  ->  JobSchedulerService.evaluateControllerStatesLocked  ->  ConnectivityController.evaluateStateLocked  ->  ConnectivityController.isNetworkAvailable`  
- acquires `StateController.mLock` at `apex/jobscheduler/service/java/com/android/server/job/controllers/ConnectivityController.java:437` — `synchronized (mLock) {`  

**`StateController.mLock` → `JobSchedulerService.mLock`** (28×)  
- holds `StateController.mLock` at `apex/jobscheduler/service/java/com/android/server/job/controllers/BatteryController.java:145`  
- path: `BatteryController.lambda$onBatteryStateChangedLocked$0  ->  BatteryController.maybeReportNewChargingStateLocked  ->  JobSchedulerService.isPowerConnected`  
- acquires `JobSchedulerService.mLock` at `apex/jobscheduler/service/java/com/android/server/job/JobSchedulerService.java:532` — `synchronized (mLock) {`  

---

## 12. `AppProfiler.mProcessCpuTracker` ⇄ `BatteryStatsService.mStats` ⇄ `BatteryHistoryStepDetailsProvider.mClock`

![candidate 12](findings/cand12.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`AppProfiler.mProcessCpuTracker` → `BatteryStatsService.mStats`** (2×)  
- holds `AppProfiler.mProcessCpuTracker` at `services/core/java/com/android/server/am/AppProfiler.java:2681`  
- path: `AppProfiler.getAppProfileStatsForDebugging  ->  AppProfiler.updateCpuStatsNow`  

**`BatteryStatsService.mStats` → `BatteryHistoryStepDetailsProvider.mClock`** (1×)  
- holds `BatteryStatsService.mStats` at `services/core/java/com/android/server/am/BatteryStatsService.java:2818`  
- path: `BatteryStatsService.lambda$setBatteryState$95  ->  BatteryStatsImpl.setBatteryStateLocked  ->  BatteryHistoryStepDetailsProvider.requestUpdate`  
- acquires `BatteryHistoryStepDetailsProvider.mClock` at `services/core/java/com/android/server/power/stats/BatteryHistoryStepDetailsProvider.java:99` — `synchronized (mClock) {`  

**`BatteryHistoryStepDetailsProvider.mClock` → `AppProfiler.mProcessCpuTracker`** (1×)  
- holds `BatteryHistoryStepDetailsProvider.mClock` at `services/core/java/com/android/server/power/stats/BatteryHistoryStepDetailsProvider.java:102`  
- path: `BatteryHistoryStepDetailsProvider.lambda$requestUpdate$0  ->  BatteryHistoryStepDetailsProvider.update  ->  BatteryStatsImpl.updateCpuDetails  ->  ActivityManagerService.batteryNeedsCpuUpdate  ->  ActivityManagerService.updateCpuStatsNow  ->  AppProfiler.updateCpuStatsNow`  
- acquires `AppProfiler.mProcessCpuTracker` at `services/core/java/com/android/server/am/AppProfiler.java:623` — `synchronized (mProcessCpuTracker) {`  

**`BatteryHistoryStepDetailsProvider.mClock` → `BatteryStatsService.mStats`** (1×)  
- holds `BatteryHistoryStepDetailsProvider.mClock` at `services/core/java/com/android/server/power/stats/BatteryHistoryStepDetailsProvider.java:102`  
- path: `BatteryHistoryStepDetailsProvider.lambda$requestUpdate$0  ->  BatteryHistoryStepDetailsProvider.update  ->  BatteryStatsImpl.updateCpuDetails  ->  ActivityManagerService.batteryNeedsCpuUpdate  ->  ActivityManagerService.updateCpuStatsNow  ->  AppProfiler.updateCpuStatsNow`  

---

## 13. `AudioService.mHdmiClientLock` ⇄ `AudioService.mSettingsLock` ⇄ `AudioService.mVolumeStateLock`

![candidate 13](findings/cand13.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`AudioService.mHdmiClientLock` → `AudioService.mVolumeStateLock`** (4×)  
- holds `AudioService.mHdmiClientLock` at `services/core/java/com/android/server/audio/AudioService.java:4419`  
- path: `AudioService.adjustStreamVolume  ->  AudioService$VolumeStreamState.getIndex`  
- acquires `AudioService.mVolumeStateLock` at `services/core/java/com/android/server/audio/AudioService.java:1743` — `synchronized (mVolumeStateLock) {`  

**`AudioService.mSettingsLock` → `AudioService.mHdmiClientLock`** (1×)  
- holds `AudioService.mSettingsLock` at `services/core/java/com/android/server/audio/AudioSystemAdapter.java:4571`  
- path: `(not reconstructed — chain too deep or via an over-approximated call)`  

**`AudioService.mSettingsLock` → `AudioService.mVolumeStateLock`** (48×)  
- holds `AudioService.mSettingsLock` at `services/core/java/com/android/server/audio/AudioService.java:14005`  
- path: `AudioService$AudioServiceInternal.updateRingerModeAffectedStreamsInternal  ->  AudioService.-$$Nest$msetRingerModeInt  ->  AudioService.setRingerModeInt  ->  AudioService.updateStreamMuteFromRingerMode`  
- acquires `AudioService.mVolumeStateLock` at `services/core/java/com/android/server/audio/AudioService.java:1743` — `synchronized (mVolumeStateLock) {`  

**`AudioService.mVolumeStateLock` → `AudioService.mSettingsLock`** (21×)  
- holds `AudioService.mVolumeStateLock` at `services/core/java/com/android/server/audio/AudioService.java:9707`  
- path: `AudioService$VolumeGroupState.applyAllVolumes  ->  AudioService$VolumeStreamState.setIndex`  
- acquires `AudioService.mSettingsLock` at `services/core/java/com/android/server/audio/AudioService.java:2605` — `synchronized (mSettingsLock) {`  

---

## 14. `AudioDeviceBroker.mBluetoothAudioStateLock` ⇄ `AudioDeviceBroker.mDeviceStateLock` ⇄ `AudioDeviceInventory.mDevicesLock`

![candidate 14](findings/cand14.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`AudioDeviceBroker.mBluetoothAudioStateLock` → `AudioDeviceBroker.mDeviceStateLock`** (1×)  
- holds `AudioDeviceBroker.mBluetoothAudioStateLock` at `services/core/java/com/android/server/audio/AudioDeviceBroker.java:1274`  
- path: `AudioDeviceBroker.setBluetoothScoOn  ->  AudioDeviceBroker.bluetoothScoRequestOwnerAttributionSource  ->  AudioDeviceBroker.isBluetoothScoRequested  ->  AudioDeviceBroker.isDeviceRequestedForCommunication`  
- acquires `AudioDeviceBroker.mDeviceStateLock` at `services/core/java/com/android/server/audio/AudioDeviceBroker.java:264` — `synchronized (mDeviceStateLock) {`  

**`AudioDeviceBroker.mDeviceStateLock` → `AudioDeviceBroker.mBluetoothAudioStateLock`** (18×)  
- holds `AudioDeviceBroker.mDeviceStateLock` at `services/core/java/com/android/server/audio/AudioDeviceBroker.java:1983`  
- path: `(not reconstructed — chain too deep or via an over-approximated call)`  

**`AudioDeviceBroker.mDeviceStateLock` → `AudioDeviceInventory.mDevicesLock`** (30×)  
- holds `AudioDeviceBroker.mDeviceStateLock` at `services/core/java/com/android/server/audio/AudioDeviceBroker.java:2086`  
- path: `AudioDeviceBroker$BrokerHandler.handleMessage  ->  AudioDeviceInventory.onMakeHearingAidDeviceUnavailableNow`  
- acquires `AudioDeviceInventory.mDevicesLock` at `services/core/java/com/android/server/audio/AudioDeviceInventory.java:299` — `synchronized (mDevicesLock) {`  

**`AudioDeviceInventory.mDevicesLock` → `AudioDeviceBroker.mBluetoothAudioStateLock`** (4×)  
- holds `AudioDeviceInventory.mDevicesLock` at `services/core/java/com/android/server/audio/AudioDeviceInventory.java:1060`  
- path: `AudioDeviceInventory.onSetBtActiveDevice  ->  AudioDeviceInventory.makeLeAudioDeviceAvailable  ->  AudioDeviceBroker.clearLeAudioSuspended`  
- acquires `AudioDeviceBroker.mBluetoothAudioStateLock` at `services/core/java/com/android/server/audio/AudioDeviceBroker.java:1174` — `synchronized (mBluetoothAudioStateLock) {`  

**`AudioDeviceInventory.mDevicesLock` → `AudioDeviceBroker.mDeviceStateLock`** (4×)  
- holds `AudioDeviceInventory.mDevicesLock` at `services/core/java/com/android/server/audio/AudioDeviceInventory.java:1197`  
- path: `AudioDeviceInventory.onMakeA2dpDeviceUnavailableNow  ->  AudioDeviceInventory.makeA2dpDeviceUnavailableNow  ->  AudioDeviceBroker.clearAvrcpAbsoluteVolumeSupported  ->  AudioDeviceBroker.setAvrcpAbsoluteVolumeSupported`  
- acquires `AudioDeviceBroker.mDeviceStateLock` at `services/core/java/com/android/server/audio/AudioDeviceBroker.java:264` — `synchronized (mDeviceStateLock) {`  

---

## 15. `ListenerMultiplexer.mMultiplexerLock` ⇄ `DelegateLocationProvider.mInitializationLock` ⇄ `MockableLocationProvider.mOwnerLock`

![candidate 15](findings/cand15.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`ListenerMultiplexer.mMultiplexerLock` → `DelegateLocationProvider.mInitializationLock`** (5×)  
- holds `ListenerMultiplexer.mMultiplexerLock` at `services/core/java/com/android/server/location/provider/LocationProviderManager.java:1776`  
- path: `LocationProviderManager.setMockProvider  ->  MockableLocationProvider.setMockProvider  ->  MockableLocationProvider.setProviderLocked  ->  AbstractLocationProvider.setState  ->  StationaryThrottlingLocationProvider.onStateChanged  ->  DelegateLocationProvider.onStateChanged  ->  DelegateLocationProvider.waitForInitialization`  
- acquires `DelegateLocationProvider.mInitializationLock` at `services/core/java/com/android/server/location/provider/DelegateLocationProvider.java:55` — `synchronized (mInitializationLock) {`  

**`ListenerMultiplexer.mMultiplexerLock` → `MockableLocationProvider.mOwnerLock`** (15×)  
- holds `ListenerMultiplexer.mMultiplexerLock` at `services/core/java/com/android/server/location/listeners/ListenerMultiplexer.java:642`  
- path: `ListenerMultiplexer.dump  ->  LocationProviderManager.getServiceState  ->  MockableLocationProvider.getCurrentRequest`  
- acquires `MockableLocationProvider.mOwnerLock` at `services/core/java/com/android/server/location/provider/MockableLocationProvider.java:88` — `synchronized (mOwnerLock) {`  

**`DelegateLocationProvider.mInitializationLock` → `ListenerMultiplexer.mMultiplexerLock`** (1×)  
- holds `DelegateLocationProvider.mInitializationLock` at `services/core/java/com/android/server/location/provider/DelegateLocationProvider.java:57`  
- path: `DelegateLocationProvider.initializeDelegate  ->  AbstractLocationProvider.setState  ->  LocationProviderManager.onStateChanged  ->  ListenerMultiplexer.updateRegistrations`  
- acquires `ListenerMultiplexer.mMultiplexerLock` at `services/core/java/com/android/server/location/listeners/ListenerMultiplexer.java:266` — `synchronized (mMultiplexerLock) {`  

**`DelegateLocationProvider.mInitializationLock` → `MockableLocationProvider.mOwnerLock`** (1×)  
- holds `DelegateLocationProvider.mInitializationLock` at `services/core/java/com/android/server/location/provider/DelegateLocationProvider.java:57`  
- path: `DelegateLocationProvider.initializeDelegate  ->  AbstractLocationProvider.setState  ->  MockableLocationProvider$ListenerWrapper.onStateChanged`  
- acquires `MockableLocationProvider.mOwnerLock` at `services/core/java/com/android/server/location/provider/MockableLocationProvider.java:88` — `synchronized (mOwnerLock) {`  

**`MockableLocationProvider.mOwnerLock` → `ListenerMultiplexer.mMultiplexerLock`** (2×)  
- holds `MockableLocationProvider.mOwnerLock` at `services/core/java/com/android/server/location/provider/MockableLocationProvider.java:189`  
- path: `MockableLocationProvider.setMockProviderAllowed  ->  MockLocationProvider.setProviderAllowed  ->  AbstractLocationProvider.setAllowed  ->  AbstractLocationProvider.setState  ->  LocationProviderManager.onStateChanged  ->  ListenerMultiplexer.updateRegistrations`  
- acquires `ListenerMultiplexer.mMultiplexerLock` at `services/core/java/com/android/server/location/listeners/ListenerMultiplexer.java:266` — `synchronized (mMultiplexerLock) {`  

**`MockableLocationProvider.mOwnerLock` → `DelegateLocationProvider.mInitializationLock`** (2×)  
- holds `MockableLocationProvider.mOwnerLock` at `services/core/java/com/android/server/location/provider/MockableLocationProvider.java:189`  
- path: `MockableLocationProvider.setMockProviderAllowed  ->  MockLocationProvider.setProviderAllowed  ->  AbstractLocationProvider.setAllowed  ->  AbstractLocationProvider.setState  ->  StationaryThrottlingLocationProvider.onStateChanged  ->  DelegateLocationProvider.onStateChanged  ->  DelegateLocationProvider.waitForInitialization`  
- acquires `DelegateLocationProvider.mInitializationLock` at `services/core/java/com/android/server/location/provider/DelegateLocationProvider.java:55` — `synchronized (mInitializationLock) {`  

---

## 16. `ThermalManagerService$TemperatureWatcher.mSamples` ⇄ `ThermalManagerService$ThermalHalWrapper.mHalLock` ⇄ `ThermalManagerService.mLock`

![candidate 16](findings/cand16.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`ThermalManagerService$TemperatureWatcher.mSamples` → `ThermalManagerService$ThermalHalWrapper.mHalLock`** (9×)  
- holds `ThermalManagerService$TemperatureWatcher.mSamples` at `services/core/java/com/android/server/power/thermal/ThermalManagerService.java:204`  
- path: `ThermalManagerService$1.onThresholdChanged  ->  ThermalManagerService$TemperatureWatcher.getHeadroomCallbackDataLocked  ->  ThermalManagerService$TemperatureWatcher.getForecast  ->  ThermalManagerService$ThermalHalAidlWrapper.forecastSkinTemperature`  
- acquires `ThermalManagerService$ThermalHalWrapper.mHalLock` at `services/core/java/com/android/server/power/thermal/ThermalManagerService.java:1202` — `synchronized (mHalLock) {`  

**`ThermalManagerService$ThermalHalWrapper.mHalLock` → `ThermalManagerService$TemperatureWatcher.mSamples`** (2×)  
- holds `ThermalManagerService$ThermalHalWrapper.mHalLock` at `services/core/java/com/android/server/power/thermal/ThermalManagerService.java:1219`  
- path: `ThermalManagerService$ThermalHalWrapper$DeathRecipient.serviceDied  ->  ThermalManagerService$ThermalHalWrapper.resendCurrentTemperatures  ->  ThermalManagerService$1.onTemperatureChanged  ->  ThermalManagerService.-$$Nest$monTemperatureChanged  ->  ThermalManagerService.onTemperatureChanged`  
- acquires `ThermalManagerService$TemperatureWatcher.mSamples` at `services/core/java/com/android/server/power/thermal/ThermalManagerService.java:191` — `synchronized (mTemperatureWatcher.mSamples) {`  

**`ThermalManagerService$ThermalHalWrapper.mHalLock` → `ThermalManagerService.mLock`** (2×)  
- holds `ThermalManagerService$ThermalHalWrapper.mHalLock` at `services/core/java/com/android/server/power/thermal/ThermalManagerService.java:1219`  
- path: `ThermalManagerService$ThermalHalWrapper$DeathRecipient.serviceDied  ->  ThermalManagerService$ThermalHalWrapper.resendCurrentTemperatures  ->  ThermalManagerService$1.onTemperatureChanged  ->  ThermalManagerService.-$$Nest$monTemperatureChanged  ->  ThermalManagerService.onTemperatureChanged`  
- acquires `ThermalManagerService.mLock` at `services/core/java/com/android/server/power/thermal/ThermalManagerService.java:197` — `synchronized (mLock) {`  

**`ThermalManagerService.mLock` → `ThermalManagerService$ThermalHalWrapper.mHalLock`** (4×)  
- holds `ThermalManagerService.mLock` at `services/core/java/com/android/server/power/thermal/ThermalManagerService.java:258`  
- path: `ThermalManagerService.onActivityManagerReady  ->  ThermalManagerService$ThermalHalAidlWrapper.connectToHal`  
- acquires `ThermalManagerService$ThermalHalWrapper.mHalLock` at `services/core/java/com/android/server/power/thermal/ThermalManagerService.java:1202` — `synchronized (mHalLock) {`  

---

## 17. `LocationTimeZoneProvider.mSharedLock` ⇄ `LocationTimeZoneProviderController.mSharedLock` ⇄ `LocationTimeZoneProviderProxy.mSharedLock`

![candidate 17](findings/cand17.png)

Opposite-order acquisitions of the locks above. Each edge below is "hold the first lock, then reach an acquisition of the second":

**`LocationTimeZoneProvider.mSharedLock` → `LocationTimeZoneProviderController.mSharedLock`** (8×)  
- holds `LocationTimeZoneProvider.mSharedLock` at `services/core/java/com/android/server/timezonedetector/location/LocationTimeZoneProvider.java:492`  
- path: `LocationTimeZoneProvider.destroy  ->  LocationTimeZoneProvider.setCurrentState  ->  LocationTimeZoneProviderController$$ExternalSyntheticLambda0.onProviderStateChange  ->  LocationTimeZoneProviderController.onProviderStateChange`  
- acquires `LocationTimeZoneProviderController.mSharedLock` at `services/core/java/com/android/server/timezonedetector/location/LocationTimeZoneProviderController.java:197` — `synchronized (mSharedLock) {`  

**`LocationTimeZoneProvider.mSharedLock` → `LocationTimeZoneProviderProxy.mSharedLock`** (12×)  
- holds `LocationTimeZoneProvider.mSharedLock` at `services/core/java/com/android/server/timezonedetector/location/LocationTimeZoneProvider.java:492`  
- path: `LocationTimeZoneProvider.destroy  ->  BinderLocationTimeZoneProvider.onDestroy  ->  LocationTimeZoneProviderProxy.destroy`  
- acquires `LocationTimeZoneProviderProxy.mSharedLock` at `services/core/java/com/android/server/timezonedetector/location/LocationTimeZoneProviderProxy.java:78` — `synchronized (mSharedLock) {`  

**`LocationTimeZoneProviderController.mSharedLock` → `LocationTimeZoneProvider.mSharedLock`** (16×)  
- holds `LocationTimeZoneProviderController.mSharedLock` at `services/core/java/com/android/server/timezonedetector/location/LocationTimeZoneProviderController.java:851`  
- path: `LocationTimeZoneProviderController.clearRecordedStates  ->  LocationTimeZoneProvider.clearRecordedStates`  
- acquires `LocationTimeZoneProvider.mSharedLock` at `services/core/java/com/android/server/timezonedetector/location/LocationTimeZoneProvider.java:442` — `synchronized (mSharedLock) {`  

**`LocationTimeZoneProviderController.mSharedLock` → `LocationTimeZoneProviderProxy.mSharedLock`** (9×)  
- holds `LocationTimeZoneProviderController.mSharedLock` at `services/core/java/com/android/server/timezonedetector/location/LocationTimeZoneProviderController.java:275`  
- path: `LocationTimeZoneProviderController.destroy  ->  LocationTimeZoneProvider.destroy  ->  BinderLocationTimeZoneProvider.onDestroy  ->  LocationTimeZoneProviderProxy.destroy`  
- acquires `LocationTimeZoneProviderProxy.mSharedLock` at `services/core/java/com/android/server/timezonedetector/location/LocationTimeZoneProviderProxy.java:78` — `synchronized (mSharedLock) {`  

**`LocationTimeZoneProviderProxy.mSharedLock` → `LocationTimeZoneProvider.mSharedLock`** (2×)  
- holds `LocationTimeZoneProviderProxy.mSharedLock` at `services/core/java/com/android/server/timezonedetector/location/RealLocationTimeZoneProviderProxy.java:113`  
- path: `RealLocationTimeZoneProviderProxy.onBind  ->  BinderLocationTimeZoneProvider$1.onProviderBound  ->  BinderLocationTimeZoneProvider.-$$Nest$mhandleOnProviderBound  ->  BinderLocationTimeZoneProvider.handleOnProviderBound`  
- acquires `LocationTimeZoneProvider.mSharedLock` at `services/core/java/com/android/server/timezonedetector/location/BinderLocationTimeZoneProvider.java:85` — `synchronized (mSharedLock) {`  

**`LocationTimeZoneProviderProxy.mSharedLock` → `LocationTimeZoneProviderController.mSharedLock`** (1×)  
- holds `LocationTimeZoneProviderProxy.mSharedLock` at `services/core/java/com/android/server/timezonedetector/location/RealLocationTimeZoneProviderProxy.java:128`  
- path: `RealLocationTimeZoneProviderProxy.onUnbind  ->  BinderLocationTimeZoneProvider$1.onProviderUnbound  ->  LocationTimeZoneProvider.handleTemporaryFailure  ->  LocationTimeZoneProvider.setCurrentState  ->  LocationTimeZoneProviderController$$ExternalSyntheticLambda0.onProviderStateChange  ->  LocationTimeZoneProviderController.onProviderStateChange`  
- acquires `LocationTimeZoneProviderController.mSharedLock` at `services/core/java/com/android/server/timezonedetector/location/LocationTimeZoneProviderController.java:197` — `synchronized (mSharedLock) {`  

---
