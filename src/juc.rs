//! Recognition of `java.util.concurrent.locks` operations and async sinks by
//! their (class, method) signature. Monitor enter/exit come from real dex
//! `monitor-*` instructions; juc locks are ordinary method calls we translate.

/// What an invoke means for lock analysis, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockCall {
    /// `Lock.lock()` / `lockInterruptibly()` — blocking acquire of the receiver.
    Acquire,
    /// `Lock.tryLock()` — non-blocking acquire (cannot deadlock).
    TryAcquire,
    /// `Lock.unlock()` — release of the receiver.
    Release,
    /// `ReadWriteLock.readLock()` — returns the receiver tagged read-mode.
    ReadView,
    /// `ReadWriteLock.writeLock()` — returns the receiver tagged write-mode.
    WriteView,
    /// async sink (`Handler.post`, `Executor.execute`, `Thread.start`, ...) —
    /// any lambda/Runnable argument runs detached, so held locks are severed.
    AsyncSink,
}

fn is_lock_type(class: &str) -> bool {
    // Match by simple name suffix so we don't depend on the exact j.u.c package.
    let c = class.rsplit('.').next().unwrap_or(class);
    matches!(
        c,
        "Lock" | "ReentrantLock" | "ReentrantReadWriteLock"
            | "ReadLock" | "WriteLock"
    ) || class.contains("locks.")
}

pub fn classify(class: &str, name: &str) -> Option<LockCall> {
    let simple = class.rsplit('.').next().unwrap_or(class);

    // --- async sinks: anything that defers a Runnable/Message to run later ---
    let async_sink = matches!(
        (simple, name),
        ("Handler", "post")
            | ("Handler", "postDelayed")
            | ("Handler", "postAtTime")
            | ("Handler", "sendMessage")
            | ("Handler", "sendMessageDelayed")
            | ("Handler", "sendMessageAtTime")
            | ("Executor", "execute")
            | ("ExecutorService", "submit")
            | ("ScheduledExecutorService", "schedule")
            | ("Thread", "start")
            | ("HandlerThread", "start")
            | ("AsyncTask", "execute")
            | ("AsyncTask", "executeOnExecutor")
    ) || (name == "execute" && simple.contains("Executor"))
        || (name == "post" && simple.contains("Handler"));
    if async_sink {
        return Some(LockCall::AsyncSink);
    }

    // --- ReadWriteLock views ---
    if name == "readLock" {
        return Some(LockCall::ReadView);
    }
    if name == "writeLock" {
        return Some(LockCall::WriteView);
    }

    // --- juc Lock acquire/release ---
    if is_lock_type(class) {
        match name {
            "lock" | "lockInterruptibly" | "acquire" | "acquireUninterruptibly" => {
                return Some(LockCall::Acquire)
            }
            "tryLock" | "tryAcquire" => return Some(LockCall::TryAcquire),
            "unlock" | "release" => return Some(LockCall::Release),
            _ => {}
        }
    }
    None
}
