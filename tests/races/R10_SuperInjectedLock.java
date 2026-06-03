// NO_RACE: corpus.R10_SuperInjectedLock$Base.mState
package corpus;
// The owner injects its own lock into the child, which forwards it via super(lock)
// to a base-class field. A base-class field guarded by that lock is written both by
// the owner (holding the lock directly) and by the base (holding the inherited
// field) — the same object under two names. Threading the captured lock through the
// super constructor unifies them, so the field is consistently guarded.
public class R10_SuperInjectedLock {
    final Object mLock = new Object();
    Base mChild;
    void init() { mChild = new Sub(mLock); }
    void a() { synchronized (mLock) { mChild.mState = 1; } }
    static class Base {
        int mState;
        final Object mLock;
        Base(Object lock) { mLock = lock; }
        void b() { synchronized (mLock) { mState = 2; } }
    }
    static class Sub extends Base {
        Sub(Object lock) { super(lock); }
    }
}
