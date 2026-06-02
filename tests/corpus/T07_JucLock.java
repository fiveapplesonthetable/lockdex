// EXPECT: DEADLOCK
// CYCLE: corpus.T07_JucLock.l1 corpus.T07_JucLock.l2
// MINSTAGE: 4
package corpus;
import java.util.concurrent.locks.ReentrantLock;
import java.util.concurrent.locks.Lock;
// lock1 then lock2 in one path; lock2 then lock1 in the other.
public class T07_JucLock {
    final Lock l1 = new ReentrantLock();
    final Lock l2 = new ReentrantLock();
    void p1() { l1.lock(); try { l2.lock(); try {} finally { l2.unlock(); } } finally { l1.unlock(); } }
    void p2() { l2.lock(); try { l1.lock(); try {} finally { l1.unlock(); } } finally { l2.unlock(); } }
}
