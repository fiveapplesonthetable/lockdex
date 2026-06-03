// NO_RACE: corpus.R02_ConsistentField.mState
package corpus;
// Every write holds mLock — consistently guarded, not a race.
public class R02_ConsistentField {
    final Object mLock = new Object();
    int mState;
    void a() { synchronized (mLock) { mState = 1; } }
    void b() { synchronized (mLock) { mState = 2; } }
}
