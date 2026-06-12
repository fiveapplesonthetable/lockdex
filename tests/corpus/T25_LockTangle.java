// EXPECT: DEADLOCK
// CYCLE: corpus.T25_LockTangle.L00 corpus.T25_LockTangle.L01 corpus.T25_LockTangle.L02 corpus.T25_LockTangle.L03 corpus.T25_LockTangle.L04 corpus.T25_LockTangle.L05 corpus.T25_LockTangle.L06 corpus.T25_LockTangle.L07 corpus.T25_LockTangle.L08 corpus.T25_LockTangle.L09 corpus.T25_LockTangle.L10 corpus.T25_LockTangle.L11 corpus.T25_LockTangle.L12
// INVERSION: corpus.T25_LockTangle.L00 corpus.T25_LockTangle.L01
package corpus;
// 13 locks in an order ring (one above the TANGLE threshold of 12) plus one
// reverse edge L01->L00. The SCC is a "lock tangle", but it must still be
// reported with ALL members and decomposed into its minimal inversions: the
// tight L00<->L01 AB-BA (asserted above) and the full ring.
public class T25_LockTangle {
    static final Object L00 = new Object();
    static final Object L01 = new Object();
    static final Object L02 = new Object();
    static final Object L03 = new Object();
    static final Object L04 = new Object();
    static final Object L05 = new Object();
    static final Object L06 = new Object();
    static final Object L07 = new Object();
    static final Object L08 = new Object();
    static final Object L09 = new Object();
    static final Object L10 = new Object();
    static final Object L11 = new Object();
    static final Object L12 = new Object();
    void m00() { synchronized (L00) { synchronized (L01) { } } }
    void m01() { synchronized (L01) { synchronized (L02) { } } }
    void m02() { synchronized (L02) { synchronized (L03) { } } }
    void m03() { synchronized (L03) { synchronized (L04) { } } }
    void m04() { synchronized (L04) { synchronized (L05) { } } }
    void m05() { synchronized (L05) { synchronized (L06) { } } }
    void m06() { synchronized (L06) { synchronized (L07) { } } }
    void m07() { synchronized (L07) { synchronized (L08) { } } }
    void m08() { synchronized (L08) { synchronized (L09) { } } }
    void m09() { synchronized (L09) { synchronized (L10) { } } }
    void m10() { synchronized (L10) { synchronized (L11) { } } }
    void m11() { synchronized (L11) { synchronized (L12) { } } }
    void m12() { synchronized (L12) { synchronized (L00) { } } }
    void rev() { synchronized (L01) { synchronized (L00) { } } }
}
