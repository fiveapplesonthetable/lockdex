// RACE: corpus.R01_InconsistentField.mState
package corpus;
// mState is written under mLock twice and once without — the bare write is the race.
public class R01_InconsistentField {
    final Object mLock = new Object();
    int mState;
    void guarded() { synchronized (mLock) { mState = 1; } }
    void alsoGuarded() { synchronized (mLock) { mState = 2; } }
    void unguarded() { mState = 3; }
}
