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

/// User adjustments to the async-sink list, loaded from `--async-sinks FILE`.
/// Each entry is `SimpleClass.method` or a bare `method`; `add` makes something a
/// sink, `remove` disables a built-in. Both are additive to the defaults.
#[derive(Default)]
pub struct SinkConfig {
    pub add: std::collections::HashSet<String>,
    pub remove: std::collections::HashSet<String>,
}

impl SinkConfig {
    fn hit(set: &std::collections::HashSet<String>, simple: &str, name: &str) -> bool {
        set.contains(name) || set.contains(&format!("{simple}.{name}"))
    }
}

pub fn classify(class: &str, name: &str, cfg: &SinkConfig) -> Option<LockCall> {
    let simple = class.rsplit('.').next().unwrap_or(class);

    // --- async sinks: anything that defers a Runnable/Message to run later ---
    let builtin_async = matches!(
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
    // user add/remove overlays the defaults.
    let async_sink = (builtin_async || SinkConfig::hit(&cfg.add, simple, name))
        && !SinkConfig::hit(&cfg.remove, simple, name);
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
