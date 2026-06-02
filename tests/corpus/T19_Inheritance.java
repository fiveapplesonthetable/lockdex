// EXPECT: DEADLOCK
// CYCLE: corpus.T19_Inheritance$Derived.mLock corpus.T19_Inheritance.B
// MINSTAGE: 0
package corpus;
// The deadlock only exists if the override Derived.run() is dispatched through the
// abstract Task.run() call in p2 (RTA over instantiated subtypes) — this is the
// behaviour under test. (Note: dex references the inherited field via the subclass,
// so the lock is reported as Derived.mLock, not Base.mLock — declaring-class
// canonicalization of inherited fields is a separate, future refinement.)
public class T19_Inheritance {
    static final Object B = new Object();
    abstract static class Task { abstract void run(); }
    static class Base extends Task {
        final Object mLock = new Object();
        void run() { synchronized (mLock) { } }
    }
    static class Derived extends Base {
        @Override void run() { synchronized (mLock) { synchronized (B) { } } }   // Base.mLock -> B
    }
    void p2(Task t) { synchronized (B) { t.run(); } }                            // B -> Base.mLock (via Derived.run)
    void driver() {
        Base b = new Base();
        Derived d = new Derived();
        b.run(); d.run();
        p2(b); p2(d);
    }
}
