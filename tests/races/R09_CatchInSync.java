// NO_RACE: corpus.R09_CatchInSync.mState
package corpus;
// mState is written in a catch block that is still inside the synchronized region.
// The handler is reached by an exception edge from the try body, so the held-set
// dataflow must carry mLock into the catch — otherwise the write looks unguarded.
public class R09_CatchInSync {
    final Object mLock = new Object();
    int mState;
    void a() { synchronized (mLock) { mState = 1; } }
    void update() {
        synchronized (mLock) {
            try {
                risky();
            } catch (Exception e) {
                mState = 2;
            }
        }
    }
    void risky() throws Exception { throw new Exception(); }
}
