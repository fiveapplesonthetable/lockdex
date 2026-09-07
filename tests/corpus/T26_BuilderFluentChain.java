// EXPECT: DEADLOCK
// CYCLE: corpus.T26_BuilderFluentChain$Ams.mProcLock corpus.T26_BuilderFluentChain.B
package corpus;
// The exact ProcessStateController shape. A fluent builder carries the real lock:
//   new Controller.Builder().setLock(this).setProcLock(this.mProcLock).build()
// Each setter returns `this`, stashing its argument into a builder field; build()
// passes that field into the Controller constructor. Crucially build() also has a
// lazy default `if (mProcLock == null) mProcLock = new Object();`, so the builder
// field `mProcLock` is written by BOTH a fresh allocation and the setter param.
// A global field-merge sees the conflict and gives up, leaving Controller.mProcLock
// stuck at the builder field. Resolving it needs object sensitivity on the builder
// allocation: on THIS chain, setProcLock was called with Ams.mProcLock, so the dead
// `new Object()` default (guarded by == null) is irrelevant. Only then does the
// Ams.mProcLock <-> B inversion appear (op: procLock then B; other: B then procLock).
public class T26_BuilderFluentChain {
    static final Object B = new Object();

    static class Ams {
        final Object mProcLock = new Object();
        Controller mController;
        void init(ActiveUids uids) {
            mController = new Controller.Builder(uids)
                    .setLock(this)
                    .setProcLock(this.mProcLock)
                    .build();
        }
        void other() { synchronized (B) { synchronized (mProcLock) { } } }   // B -> mProcLock
    }

    static class ActiveUids { }

    static class Controller {
        final Object mLock;
        final Object mProcLock;
        Controller(Object lock, Object procLock) { mLock = lock; mProcLock = procLock; }
        void op() { synchronized (mProcLock) { synchronized (B) { } } }       // procLock -> B

        static class Builder {
            private final ActiveUids mActiveUids;
            private Object mLock = null;
            private Object mProcLock = null;
            Builder(ActiveUids uids) { mActiveUids = uids; }
            Builder setLock(Object lock) { mLock = lock; return this; }
            Builder setProcLock(Object procLock) { mProcLock = procLock; return this; }
            Controller build() {
                if (mLock == null) { mLock = new Object(); }
                if (mProcLock == null) { mProcLock = new Object(); }
                return new Controller(mLock, mProcLock);
            }
        }
    }

    void driver(Ams a, ActiveUids uids) {
        a.init(uids);
        a.other();
        a.mController.op();
    }
}
