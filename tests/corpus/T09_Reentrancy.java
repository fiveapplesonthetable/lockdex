// EXPECT: NO_DEADLOCK
package corpus;
// Re-acquiring the SAME lock must not produce an A->A self cycle.
public class T09_Reentrancy {
    static final Object A = new Object();
    void r() { synchronized (A) { synchronized (A) { work(); } } }
    static void work() {}
}
