// OUT: corpus.B01_HoldAcrossOutgoing.LOCK
package corpus;
import android.os.IBinder;
import android.os.Binder;
import android.os.Parcel;
// A lock is held while an outgoing Binder transaction is made: the lock stays held
// for the whole duration of the cross-process call.
public class B01_HoldAcrossOutgoing {
    static final Object LOCK = new Object();
    final Proxy proxy = new Proxy();
    void caller() {
        synchronized (LOCK) {
            proxy.doRemote();
        }
    }
}
class Proxy {
    final IBinder remote = new Binder();
    void doRemote() {
        remote.transact(1, new Parcel(), new Parcel(), 0);
    }
}
