// EXPECT: DEADLOCK
// CYCLE: corpus.T01_SimpleABBA.A corpus.T01_SimpleABBA.B
package corpus;
// T01: classic AB-BA deadlock. Thread 1 takes A then B; thread 2 takes B then A.
public class T01_SimpleABBA {
    static final Object A = new Object();
    static final Object B = new Object();
    void t1() { synchronized (A) { synchronized (B) { work(); } } }
    void t2() { synchronized (B) { synchronized (A) { work(); } } }
    static void work() {}
}
