// NO_RACE: corpus.Holder.value
package corpus;
// A shared data class. The owner guards its own `Holder` instance under mLock; an
// unrelated method reads a *different* `Holder` (a parameter) without the lock. The
// guard on the owner's instance must not project onto every `Holder.value` — keyed by
// the base instance, the parameter read is a separate, unguarded-but-unraced field.
public class R11_SharedDataClassInstance {
    final Object mLock = new Object();
    final Holder mHeld = new Holder();
    void set(int v) { synchronized (mLock) { mHeld.value = v; } }
    int peek(Holder other) { return other.value; }
    static class Holder { int value; }
}
