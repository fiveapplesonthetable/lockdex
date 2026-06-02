// EXPECT: DEADLOCK
// CYCLE: corpus.T10_LambdaCapture.mLock corpus.T10_LambdaCapture.B
package corpus;
// A Runnable captures this.mLock and locks it when run synchronously (run() is
// called directly, not posted). Held under B in one path; reverse in the other.
public class T10_LambdaCapture {
    final Object mLock = new Object();
    static final Object B = new Object();
    void runNow(Runnable r) { r.run(); }
    void p1() { synchronized (B) { runNow(() -> { synchronized (mLock) { } }); } }
    void p2() { synchronized (mLock) { synchronized (B) { } } }
}
