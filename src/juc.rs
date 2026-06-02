// Copyright (C) 2026 The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Recognition of `java.util.concurrent.locks` operations and async-dispatch
//! points by their (class, method) signature. Monitor enter/exit come from real
//! dex `monitor-*` instructions; juc locks are ordinary method calls we translate.

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
    /// async-dispatch point (`Handler.post`, `Executor.execute`, `Thread.start`,
    /// ...) — a lambda/Runnable argument runs detached, so held locks are severed.
    AsyncDispatch,
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

/// User adjustments to the async-dispatch list, loaded from `--async-dispatch FILE`.
/// Each entry is `SimpleClass.method` or a bare `method`; `add` treats a call as an
/// async-dispatch point, `remove` disables a built-in. Both overlay the defaults.
#[derive(Default)]
pub struct AsyncConfig {
    pub add: std::collections::HashSet<String>,
    pub remove: std::collections::HashSet<String>,
}

impl AsyncConfig {
    /// Match an entry against a call. An entry may be the fully-qualified
    /// `pkg.Class.method`, the simple `Class.method`, or a bare `method`.
    fn hit(set: &std::collections::HashSet<String>, class: &str, simple: &str, name: &str) -> bool {
        set.contains(name)
            || set.contains(&format!("{simple}.{name}"))
            || set.contains(&format!("{class}.{name}"))
    }
}

pub fn classify(class: &str, name: &str, cfg: &AsyncConfig) -> Option<LockCall> {
    let simple = class.rsplit('.').next().unwrap_or(class);

    // --- async dispatch: anything that defers a Runnable/Message to run later ---
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
    let is_dispatch = (builtin_async || AsyncConfig::hit(&cfg.add, class, simple, name))
        && !AsyncConfig::hit(&cfg.remove, class, simple, name);
    if is_dispatch {
        return Some(LockCall::AsyncDispatch);
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
