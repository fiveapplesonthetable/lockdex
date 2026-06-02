// NO_OUT: corpus.B02_ReleaseBeforeOutgoing.LOCK
package corpus;
import android.os.IBinder;
import android.os.Binder;
import android.os.Parcel;
// The lock is released before the outgoing transaction — not held across IPC.
public class B02_ReleaseBeforeOutgoing {
    static final Object LOCK = new Object();
    final IBinder remote = new Binder();
    int state;
    void caller() {
        synchronized (LOCK) {
            state++;
        }
        remote.transact(1, new Parcel(), new Parcel(), 0);
    }
}
