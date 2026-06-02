// EXPECT: NO_DEADLOCK
// MINSTAGE: 5
package corpus;
// Both orderings of A,B happen only while holding G. A common outer guard makes
// the two acquisitions mutually exclusive -> not a real deadlock.
public class T05_GuardProtected {
    static final Object G = new Object();
    static final Object A = new Object();
    static final Object B = new Object();
    void p1() { synchronized (G) { synchronized (A) { synchronized (B) { } } } }
    void p2() { synchronized (G) { synchronized (B) { synchronized (A) { } } } }
}
