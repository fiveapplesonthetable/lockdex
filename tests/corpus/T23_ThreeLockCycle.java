// EXPECT: DEADLOCK
// CYCLE: corpus.T23_ThreeLockCycle.A corpus.T23_ThreeLockCycle.B corpus.T23_ThreeLockCycle.C
package corpus;
// A genuine three-lock cycle: A->B, B->C, C->A. The SCC {A,B,C} is the deadlock.
public class T23_ThreeLockCycle {
    static final Object A = new Object();
    static final Object B = new Object();
    static final Object C = new Object();
    void p1() { synchronized (A) { synchronized (B) { } } }
    void p2() { synchronized (B) { synchronized (C) { } } }
    void p3() { synchronized (C) { synchronized (A) { } } }
}
