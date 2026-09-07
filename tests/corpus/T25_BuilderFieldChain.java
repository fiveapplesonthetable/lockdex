// EXPECT: DEADLOCK
// CYCLE: corpus.T25_BuilderFieldChain$Ams.mProcLock corpus.T25_BuilderFieldChain.B
package corpus;
// A lock reaches Controller.mProcLock only through an intermediate Builder field
// that is itself assigned `ams.mProcLock` (field = parameter.subfield), and
// build() passes that Builder field into the Controller constructor. Recovering
// the real lock needs (1) keeping the `.mProcLock` sub-path when a field is
// aliased to a parameter's field, and (2) chaining
// Controller.mProcLock -> Builder.mProcLock -> the root Ams.mProcLock. Only then
// does the Ams.mProcLock <-> B inversion appear (op: procLock then B; other: B
// then Ams.mProcLock).
public class T25_BuilderFieldChain {
    static class Ams { final Object mProcLock = new Object(); }
    static final Ams sAms = new Ams();
    static final Object B = new Object();

    static class Controller {
        final Object mProcLock;
        Controller(Object procLock) { mProcLock = procLock; }
        void op() { synchronized (mProcLock) { synchronized (B) { } } }
    }
    static class Builder {
        final Ams mAms;
        Object mProcLock;
        Builder(Ams ams) { mAms = ams; mProcLock = ams.mProcLock; }
        Controller build() { return new Controller(mProcLock); }
    }
    static void other() { synchronized (B) { synchronized (sAms.mProcLock) { } } }
    static void driver() {
        Controller c = new Builder(sAms).build();
        c.op();
        other();
    }
}
