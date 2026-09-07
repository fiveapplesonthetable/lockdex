// EXPECT: DEADLOCK
// CYCLE: corpus.T29_AllocPassedToSetter$Ams.mProcLock corpus.T29_AllocPassedToSetter.B
package corpus;
// R8 register-reuse shape (the real ProcessStateController case). A freshly
// allocated lock is stored into this.mProcLock and then the SAME register is
// passed straight into the builder setter — no iget reload:
//   Object procLock = new Object();
//   this.mProcLock = procLock;
//   new Controller.Builder().setProcLock(procLock).build();
// Because the argument is a bare allocation (not this.mProcLock), the resolver
// used to drop the call site and leave Controller.mProcLock a standalone
// $Builder.mProcLock. Must-alias renaming (a store into this.field names the
// register as that field) recovers it, so Controller.mProcLock resolves to the
// root Ams.mProcLock and the Ams.mProcLock <-> B inversion appears.
public class T29_AllocPassedToSetter {
    static final Object B = new Object();
    static class Ams {
        Object mProcLock;
        Controller mController;
        void init(ActiveUids uids) {
            Object procLock = new Object();
            this.mProcLock = procLock;
            mController = new Controller.Builder(uids).setProcLock(procLock).build();
        }
        void other() { synchronized (B) { synchronized (mProcLock) { } } }
    }
    static class ActiveUids { }
    static class Controller {
        final Object mProcLock;
        Controller(Object procLock) { mProcLock = procLock; }
        void op() { synchronized (mProcLock) { synchronized (B) { } } }
        static class Builder {
            private final ActiveUids mActiveUids;
            private Object mProcLock = null;
            Builder(ActiveUids uids) { mActiveUids = uids; }
            Builder setProcLock(Object procLock) { mProcLock = procLock; return this; }
            Controller build() {
                if (mProcLock == null) { mProcLock = new Object(); }
                return new Controller(mProcLock);
            }
        }
    }
    void driver(Ams a, ActiveUids u) { a.init(u); a.other(); a.mController.op(); }
}
