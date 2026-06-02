// IN: corpus.B03_IncomingEntryAcquires$Service.mLock
package corpus;
import android.os.Binder;
// A Binder server entry acquires a lock: a remote caller blocks on it.
public class B03_IncomingEntryAcquires {
    static class Service extends Binder {
        final Object mLock = new Object();
        public void doWork() {
            synchronized (mLock) {
                compute();
            }
        }
        void compute() {}
    }
}
