// EXPECT: DEADLOCK
// CYCLE: corpus.T20_StaticClassLock.class corpus.T20_StaticClassLock.B
package corpus;
// A static synchronized method locks the class object (Cls.class). Here Cls.class
// and B are taken in both orders.
public class T20_StaticClassLock {
    static final Object B = new Object();
    static synchronized void s1() { synchronized (B) { } }    // Cls.class -> B
    static synchronized void onClass() { }                    // locks Cls.class
    static void s2() { synchronized (B) { onClass(); } }      // B -> Cls.class
}
