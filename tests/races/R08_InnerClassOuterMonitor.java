// NO_RACE: corpus.R08_InnerClassOuterMonitor.mState
package corpus;
// An inner class synchronizes on the enclosing instance (`Outer.this`, compiled to
// the synthetic `this$0` field). That names the same monitor as the outer class's
// `synchronized(this)`, so `mState` — written under both — is consistently guarded.
public class R08_InnerClassOuterMonitor {
    int mState;
    synchronized void a() { mState = 1; }
    class Inner {
        void run() {
            synchronized (R08_InnerClassOuterMonitor.this) {
                mState = 2;
            }
        }
    }
}
