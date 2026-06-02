// EXPECT: DEADLOCK
// CYCLE: corpus.T22_CtorParamAlias.mShared corpus.T22_CtorParamAlias.B
// MINSTAGE: 0
package corpus;
// Holder.mLock is the constructor argument — at the one construction site it is
// mShared. Resolving that parameter alias reveals the real mShared <-> B inversion
// (Holder.op holds mShared then B; p2 holds B then mShared).
public class T22_CtorParamAlias {
    static final Object B = new Object();
    final Object mShared = new Object();
    static class Holder {
        final Object mLock;
        Holder(Object lock) { mLock = lock; }
        void op() { synchronized (mLock) { synchronized (B) { } } }   // mLock(=mShared) -> B
    }
    void p2() { synchronized (B) { synchronized (mShared) { } } }     // B -> mShared
    void driver() {
        Holder h = new Holder(mShared);
        h.op();
        p2();
    }
}
