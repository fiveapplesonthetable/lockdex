// EXPECT: DEADLOCK
// CYCLE: corpus.T02_InterprocABBA.A corpus.T02_InterprocABBA.B
// MINSTAGE: 0
package corpus;
// Hold A, call a helper that takes B; and hold B, call a helper that takes A.
// The edge A->B / B->A only appears if interprocedural composition works.
public class T02_InterprocABBA {
    static final Object A = new Object();
    static final Object B = new Object();
    void p1() { synchronized (A) { takeB(); } }
    void takeB() { synchronized (B) { } }
    void p2() { synchronized (B) { takeA(); } }
    void takeA() { synchronized (A) { } }
}
