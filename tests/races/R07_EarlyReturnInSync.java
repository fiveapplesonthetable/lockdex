// NO_RACE: corpus.R07_EarlyReturnInSync.mState
package corpus;
// mState is written under mLock, but the synchronized block has an early return.
// d8 emits a monitor-exit on that branch *before* the fall-through write, so a
// linear scan would drop mLock and call the write unguarded. The CFG held-set must
// still see mLock held at `mState = 2`.
public class R07_EarlyReturnInSync {
    final Object mLock = new Object();
    int mState;
    void a() { synchronized (mLock) { mState = 1; } }
    void update(boolean c) {
        synchronized (mLock) {
            if (c) {
                return;
            }
            mState = 2;
        }
    }
}
