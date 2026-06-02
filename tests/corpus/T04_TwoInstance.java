// EXPECT: NO_DEADLOCK
package corpus;
// a.mLock and b.mLock are different objects. a.foo() locks a.mLock then calls
// b.bar() which locks b.mLock. Field-name identity would FALSELY merge them into
// a self-cycle; receiver-sensitive identity must not.
public class T04_TwoInstance {
    final Object mLock = new Object();
    T04_TwoInstance other;
    void foo() { synchronized (mLock) { if (other != null) other.bar(); } }
    void bar() { synchronized (mLock) { } }
}
