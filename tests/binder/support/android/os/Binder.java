package android.os;
public class Binder implements IBinder {
    public boolean transact(int code, Parcel data, Parcel reply, int flags) { return true; }
}
