// NO_RACE: corpus.R06_ConstructorExcluded.mState
package corpus;
// The only unguarded write is in the constructor (pre-publication) — excluded.
public class R06_ConstructorExcluded {
    final Object mLock = new Object();
    int mState;
    R06_ConstructorExcluded() { mState = 0; }
    void a() { synchronized (mLock) { mState = 1; } }
    void b() { synchronized (mLock) { mState = 2; } }
}
