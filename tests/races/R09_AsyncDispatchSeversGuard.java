// RACE: corpus.R09_AsyncDispatchSeversGuard.mData
package corpus;
import java.util.concurrent.Executor;
// The dispatch runs through an Executor *implementation* ("Inline"), so the
// runnable does NOT run under mLock even though post() holds it at the execute()
// call. Without hierarchy-aware dispatch the analysis credits mLock to bump()
// (post -> execute -> run -> bump) and wrongly treats the bump() write as
// guarded; it must instead sever at execute() and flag the write.
public class R09_AsyncDispatchSeversGuard {
    static class Inline implements Executor {
        public void execute(Runnable r) { r.run(); }
    }
    final Object mLock = new Object();
    final Inline mExec = new Inline();
    int mData;
    void w1() { synchronized (mLock) { mData = 1; } }
    void w2() { synchronized (mLock) { mData = 2; } }
    void w3() { synchronized (mLock) { mData = 3; } }
    void post() { synchronized (mLock) { mExec.execute(() -> bump()); } }
    private void bump() { mData = 4; }
}
