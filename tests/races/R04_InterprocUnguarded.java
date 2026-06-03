// RACE: corpus.R04_InterprocUnguarded.mState
package corpus;
// inc() is called under mLock from one site and without it from another, so its
// write to mState is not always guarded — a race against the guarded writes.
public class R04_InterprocUnguarded {
    final Object mLock = new Object();
    int mState;
    void direct() { synchronized (mLock) { mState = 0; } }
    void direct2() { synchronized (mLock) { mState = 9; } }
    void guardedCaller() { synchronized (mLock) { inc(); } }
    void unguardedCaller() { inc(); }
    private void inc() { mState = mState + 1; }
}
