// IN: corpus.B04_NestedIncomingOutgoing$Service.mLock
// OUT: corpus.B04_NestedIncomingOutgoing$Service.mLock
// HIGH: Service.handle
package corpus;
import android.os.IBinder;
import android.os.Binder;
import android.os.Parcel;
// An incoming Binder entry holds a lock across its own outgoing transaction — the
// nested cross-process pattern that genuinely deadlocks.
public class B04_NestedIncomingOutgoing {
    static class Service extends Binder {
        final Object mLock = new Object();
        final IBinder remote = new Binder();
        public void handle() {
            synchronized (mLock) {
                remote.transact(2, new Parcel(), new Parcel(), 0);
            }
        }
    }
}
