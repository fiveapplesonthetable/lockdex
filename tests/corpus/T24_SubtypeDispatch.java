// EXPECT: NO_DEADLOCK
package corpus;
import java.util.concurrent.Executor;
// The dispatcher is an Executor *implementation* whose own name says nothing
// ("SameThreadPool"): the name-based heuristic cannot see it, the type hierarchy
// can. The runnable posted while holding A does not run under A, so A->B must
// not be emitted — even though this same-thread executor would otherwise resolve
// run() straight to the lambda body and fabricate the edge.
public class T24_SubtypeDispatch {
    static final Object A = new Object();
    static final Object B = new Object();
    static class SameThreadPool implements Executor {
        public void execute(Runnable r) { r.run(); }
    }
    SameThreadPool pool = new SameThreadPool();
    void p1() { synchronized (A) { pool.execute(() -> { synchronized (B) { } }); } }
    void p2() { synchronized (B) { synchronized (A) { } } }
}
