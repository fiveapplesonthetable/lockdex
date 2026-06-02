// EXPECT: DEADLOCK
// CYCLE: corpus.T08_RWLock.rwA.write corpus.T08_RWLock.rwB.write
package corpus;
import java.util.concurrent.locks.ReentrantReadWriteLock;
import java.util.concurrent.locks.ReadWriteLock;
public class T08_RWLock {
    final ReadWriteLock rwA = new ReentrantReadWriteLock();
    final ReadWriteLock rwB = new ReentrantReadWriteLock();
    void p1() { rwA.writeLock().lock(); try { rwB.writeLock().lock(); rwB.writeLock().unlock(); } finally { rwA.writeLock().unlock(); } }
    void p2() { rwB.writeLock().lock(); try { rwA.writeLock().lock(); rwA.writeLock().unlock(); } finally { rwB.writeLock().unlock(); } }
}
