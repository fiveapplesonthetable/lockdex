// OUT: corpus.B05_TransitiveReach.LOCK
package corpus;
import android.os.IBinder;
import android.os.Binder;
import android.os.Parcel;
// The transaction is two calls deep from the lock holder; the lock is still held
// across it, so the boundary is found transitively.
public class B05_TransitiveReach {
    static final Object LOCK = new Object();
    final IBinder remote = new Binder();
    void caller() {
        synchronized (LOCK) {
            mid();
        }
    }
    void mid() { deep(); }
    void deep() { remote.transact(3, new Parcel(), new Parcel(), 0); }
}
