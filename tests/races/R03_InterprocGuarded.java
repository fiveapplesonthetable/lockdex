// NO_RACE: corpus.R03_InterprocGuarded.mState
package corpus;
// inc() writes mState but is only ever called under mLock, so the write is guarded
// interprocedurally (must-hold-on-entry == {mLock}). Must NOT be flagged.
public class R03_InterprocGuarded {
    final Object mLock = new Object();
    int mState;
    void direct() { synchronized (mLock) { mState = 0; } }
    void caller1() { synchronized (mLock) { inc(); } }
    void caller2() { synchronized (mLock) { inc(); } }
    private void inc() { mState = mState + 1; }
}
