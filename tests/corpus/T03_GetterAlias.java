// EXPECT: DEADLOCK
// CYCLE: corpus.T03_GetterAlias.mLock corpus.T03_GetterAlias.B
package corpus;
// getLock() trivially returns this.mLock. synchronized(getLock()) must resolve
// to this.mLock (NOT a fresh opaque), so it can form an AB-BA with B.
public class T03_GetterAlias {
    final Object mLock = new Object();
    static final Object B = new Object();
    Object getLock() { return mLock; }
    void p1() { synchronized (getLock()) { synchronized (B) { } } }
    void p2() { synchronized (B) { synchronized (getLock()) { } } }
}
