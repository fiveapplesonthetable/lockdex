// NO_RACE: corpus.R05_VolatileExcluded.mState
package corpus;
// mState is volatile (lock-free by design) — excluded from race analysis even
// though it is written with and without the lock.
public class R05_VolatileExcluded {
    final Object mLock = new Object();
    volatile int mState;
    void a() { synchronized (mLock) { mState = 1; } }
    void b() { mState = 2; }
}
