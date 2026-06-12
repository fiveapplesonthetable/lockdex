// NO_RACE: corpus.R10_SyncCallKeepsGuard.mData
package corpus;
// Control for R09: the same shape, but the helper is a plain synchronous
// callback runner — NOT an Executor/Handler subtype. The lock held at call()
// genuinely covers the runnable body, so the interprocedural guard credit must
// survive and mData must NOT be flagged. Guards against over-eager severing.
public class R10_SyncCallKeepsGuard {
    static class Caller {
        void call(Runnable r) { r.run(); }
    }
    final Object mLock = new Object();
    final Caller mCaller = new Caller();
    int mData;
    void w1() { synchronized (mLock) { mData = 1; } }
    void w2() { synchronized (mLock) { mData = 2; } }
    void post() { synchronized (mLock) { mCaller.call(() -> bump()); } }
    private void bump() { mData = 3; }
}
