// EXPECT: DEADLOCK
// CYCLE: corpus.T21_ThisMonitor corpus.T21_ThisMonitor.B
package corpus;
// A synchronized instance method takes the receiver's monitor (this). Here the
// this-monitor and B are acquired in both orders.
public class T21_ThisMonitor {
    static final Object B = new Object();
    synchronized void m1() { synchronized (B) { } }   // this -> B
    void m2() { synchronized (B) { m1(); } }           // B -> this (via m1)
}
