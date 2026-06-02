// EXPECT: NO_DEADLOCK
package corpus;
import java.util.concurrent.Executor;
// While holding A we post a runnable that takes B. A is NOT held when it runs,
// so A->B must NOT be emitted from the post path.
public class T06_AsyncBoundary {
    static final Object A = new Object();
    static final Object B = new Object();
    Executor exec;
    void p1() { synchronized (A) { exec.execute(() -> { synchronized (B) { } }); } }
    void p2() { synchronized (B) { synchronized (A) { } } }
}
