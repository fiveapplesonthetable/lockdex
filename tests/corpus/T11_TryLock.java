// EXPECT: NO_DEADLOCK
package corpus;
import java.util.concurrent.locks.ReentrantLock;
import java.util.concurrent.locks.Lock;
// p1 takes l1 then TRYLOCKs l2 (non-blocking). p2 takes l2 then l1 (blocking).
// A failed tryLock returns instead of blocking, so this pair cannot deadlock.
public class T11_TryLock {
    final Lock l1 = new ReentrantLock();
    final Lock l2 = new ReentrantLock();
    void p1() { l1.lock(); try { if (l2.tryLock()) { l2.unlock(); } } finally { l1.unlock(); } }
    void p2() { l2.lock(); try { l1.lock(); l1.unlock(); } finally { l2.unlock(); } }
}
