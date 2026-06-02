// EXPECT: NO_DEADLOCK
// MINSTAGE: 0
package corpus;
// Leaf.mLock is assigned Core.getLock() in the constructor — it is the SAME
// object as Core.mLock. coreOp(Core.mLock -> Leaf.mLock) and leafOp(Leaf.mLock ->
// Core.mLock) look like an AB-BA across two fields, but it is one reentrant lock.
// Without lock-field alias resolution this is a false cycle.
public class T18_SharedLockAlias {
    static class Core {
        final Object mLock = new Object();
        Object getLock() { return mLock; }
        void coreOp(Leaf leaf) { synchronized (mLock) { leaf.touch(); } }
    }
    static class Leaf {
        final Object mLock;
        final Core mCore;
        Leaf(Core core) { mCore = core; mLock = core.getLock(); }
        void touch() { synchronized (mLock) { } }
        void leafOp() { synchronized (mLock) { mCore.coreOp(this); } }
    }
    static void driver() {
        Core c = new Core();
        Leaf l = new Leaf(c);
        l.leafOp();
        c.coreOp(l);
    }
}
