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
    ) || class.contains("locks.") // any other type under java.util.concurrent.locks
}

/// Dispatcher base types and the methods on them that *defer* their
/// Runnable/Message argument to run later, usually on another thread. A call to
/// one of these methods — on the base itself or on **any subtype** (resolved
/// against the dex type hierarchy in the global phase) — severs the caller's
/// held locks from the deferred body: `synchronized (lock) { handler.post(r) }`
/// does *not* run `r` under `lock`, so no order edge or guard credit may flow
/// through the dispatch. Adjust with `--async-dispatch` `extends` entries.
pub const ASYNC_BASES: &[(&str, &[&str])] = &[
    (
        "android.os.Handler",
        &[
            "post", "postDelayed", "postAtTime", "postAtFrontOfQueue",
            "sendMessage", "sendMessageDelayed", "sendMessageAtTime",
            "sendMessageAtFrontOfQueue", "sendEmptyMessage",
            "sendEmptyMessageDelayed", "sendEmptyMessageAtTime",
        ],
    ),
    // Each method sits on the type that introduces it; concrete classes
    // (ThreadPoolExecutor, HandlerThread, ...) reach these through
    // [`KNOWN_SUPERS`].
    ("java.util.concurrent.Executor", &["execute"]),
    ("java.util.concurrent.ExecutorService", &["submit"]),
    (
        "java.util.concurrent.ScheduledExecutorService",
        &["schedule", "scheduleAtFixedRate", "scheduleWithFixedDelay"],
    ),
    ("java.lang.Thread", &["start"]),
    ("android.os.AsyncTask", &["execute", "executeOnExecutor"]),
    // `msg.sendToTarget()` defers to the message's Handler — same severing as
    // `handler.sendMessage(msg)`.
    ("android.os.Message", &["sendToTarget"]),
    // View.post* enqueue on the UI thread; runOnUiThread defers unless already
    // on it (severing is the sound default for lock analysis: the runnable may
    // run without the caller's locks).
    ("android.view.View", &["post", "postDelayed", "postOnAnimation", "postOnAnimationDelayed"]),
    ("android.app.Activity", &["runOnUiThread"]),
    ("java.util.Timer", &["schedule", "scheduleAtFixedRate"]),
    (
        "java.util.concurrent.CompletableFuture",
        &[
            "runAsync", "supplyAsync", "thenApplyAsync", "thenAcceptAsync",
            "thenRunAsync", "thenCombineAsync", "thenComposeAsync",
            "whenCompleteAsync", "handleAsync", "exceptionallyAsync",
            "acceptEitherAsync", "applyToEitherAsync", "thenAcceptBothAsync",
            "runAfterBothAsync", "runAfterEitherAsync",
        ],
    ),
];

/// Direct supertypes of well-known JDK/Android dispatcher types. The dex
/// supertype closure stops at any class that is *not* in the analyzed dex — and
/// the framework/JDK usually isn't — so `MyPool extends ThreadPoolExecutor`
/// would never reach `Executor` on its own. These edges let the base match walk
/// through the standard chain regardless of what is in the dex. Duplicate keys
/// are allowed (a class can have several relevant supertypes).
pub const KNOWN_SUPERS: &[(&str, &str)] = &[
    ("android.os.HandlerThread", "java.lang.Thread"),
    ("android.os.HandlerExecutor", "java.util.concurrent.Executor"),
    ("java.util.concurrent.ExecutorService", "java.util.concurrent.Executor"),
    ("java.util.concurrent.ScheduledExecutorService", "java.util.concurrent.ExecutorService"),
    ("java.util.concurrent.AbstractExecutorService", "java.util.concurrent.ExecutorService"),
    ("java.util.concurrent.ThreadPoolExecutor", "java.util.concurrent.AbstractExecutorService"),
    ("java.util.concurrent.ScheduledThreadPoolExecutor", "java.util.concurrent.ThreadPoolExecutor"),
    ("java.util.concurrent.ScheduledThreadPoolExecutor", "java.util.concurrent.ScheduledExecutorService"),
    ("java.util.concurrent.ForkJoinPool", "java.util.concurrent.AbstractExecutorService"),
];

/// User adjustments to the async-dispatch list, loaded from `--async-dispatch FILE`.
/// Each entry is `SimpleClass.method` or a bare `method`; `add` treats a call as an
/// async-dispatch point, `remove` disables a built-in. `add_base` / `remove_base`
/// adjust the *hierarchy* table ([`ASYNC_BASES`]): a base entry applies to the named
/// class and everything that inherits from it. All overlay the defaults.
#[derive(Default)]
pub struct AsyncConfig {
    pub add: std::collections::HashSet<String>,
    pub remove: std::collections::HashSet<String>,
    /// `extends pkg.Base: m1 m2` — base class -> dispatch methods on it.
    pub add_base: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// `-extends pkg.Base` — disable a built-in base entirely.
    pub remove_base: std::collections::HashSet<String>,
}

impl AsyncConfig {
    /// Match an entry against a call. An entry may be the fully-qualified
    /// `pkg.Class.method`, the simple `Class.method`, or a bare `method`.
    pub(crate) fn hit(set: &std::collections::HashSet<String>, class: &str, simple: &str, name: &str) -> bool {
        set.contains(name)
            || set.contains(&format!("{simple}.{name}"))
            || set.contains(&format!("{class}.{name}"))
    }

    /// Does a call to `name` on declared class `class` hit a dispatcher base —
    /// the class itself, or anything it reaches through [`KNOWN_SUPERS`] —
    /// honoring `add_base` / `remove_base`? The global phase widens this with
    /// the dex supertype closure; here, at extraction time, only the known
    /// (non-dex) chain is walked. No allocation beyond a few stack slots: this
    /// runs once per invoke instruction.
    pub fn base_hit(&self, class: &str, name: &str) -> bool {
        let mut stack: Vec<&str> = vec![class];
        let mut seen: Vec<&str> = Vec::new();
        while let Some(c) = stack.pop() {
            if seen.contains(&c) {
                continue;
            }
            seen.push(c);
            if !self.remove_base.contains(c) {
                if ASYNC_BASES.iter().any(|(b, ms)| *b == c && ms.contains(&name)) {
                    return true;
                }
                if self.add_base.get(c).is_some_and(|ms| ms.contains(name)) {
                    return true;
                }
            }
            for (sub, sup) in KNOWN_SUPERS {
                if *sub == c {
                    stack.push(sup);
                }
            }
        }
        false
    }
}

pub fn classify(class: &str, name: &str, cfg: &AsyncConfig) -> Option<LockCall> {
    let simple = class.rsplit('.').next().unwrap_or(class);

    // --- async dispatch: anything that defers a Runnable/Message to run later ---
    // Built-ins match *fully-qualified* base types (plus the known JDK/Android
    // chain above them) — a class merely named `*Handler*`/`*Executor*` is not
    // severed. Subtypes of the bases are reclassified in the global phase
    // against the dex type hierarchy (`analyze::mark_async_by_hierarchy`);
    // anything else is opt-in via `--async-dispatch` (whose name entries may be
    // FQ, `Class.method`, or bare).
    let is_dispatch = (cfg.base_hit(class, name) || AsyncConfig::hit(&cfg.add, class, simple, name))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn is_async(class: &str, name: &str) -> bool {
        matches!(classify(class, name, &AsyncConfig::default()), Some(LockCall::AsyncDispatch))
    }

    #[test]
    fn name_based_dispatch_covers_the_full_handler_surface() {
        for m in [
            "post", "postDelayed", "postAtTime", "postAtFrontOfQueue",
            "sendMessage", "sendMessageDelayed", "sendMessageAtTime",
            "sendMessageAtFrontOfQueue", "sendEmptyMessage",
            "sendEmptyMessageDelayed", "sendEmptyMessageAtTime",
        ] {
            assert!(is_async("android.os.Handler", m), "Handler.{m} must be async dispatch");
        }
        // runWithScissors blocks until the runnable completes — NOT async.
        assert!(!is_async("android.os.Handler", "runWithScissors"));
    }

    #[test]
    fn known_chain_reaches_the_introducing_base() {
        let cfg = AsyncConfig::default();
        // concrete JDK classes are not listed in ASYNC_BASES — KNOWN_SUPERS
        // walks them up to the type that introduces each method.
        assert!(cfg.base_hit("java.util.concurrent.ThreadPoolExecutor", "execute"));
        assert!(cfg.base_hit("java.util.concurrent.ThreadPoolExecutor", "submit"));
        assert!(cfg.base_hit("java.util.concurrent.ScheduledThreadPoolExecutor", "schedule"));
        assert!(cfg.base_hit("android.os.HandlerExecutor", "execute"));
        assert!(cfg.base_hit("android.os.HandlerThread", "start"));
        // `invoke` blocks for its result — never severed.
        assert!(!cfg.base_hit("java.util.concurrent.ForkJoinPool", "invoke"));
        assert!(!cfg.base_hit("com.example.RandomClass", "execute"));
    }

    #[test]
    fn bases_overlay_built_ins() {
        let cfg = AsyncConfig {
            add_base: std::collections::HashMap::from([(
                "com.example.Queue".to_string(),
                std::collections::HashSet::from(["enqueue".to_string()]),
            )]),
            remove_base: std::collections::HashSet::from(["java.lang.Thread".to_string()]),
            ..Default::default()
        };
        assert!(cfg.base_hit("android.os.Handler", "post"));
        assert!(cfg.base_hit("com.example.Queue", "enqueue"));
        // removed base: neither Thread itself nor a known subtype severs.
        assert!(!cfg.base_hit("java.lang.Thread", "start"));
        assert!(!cfg.base_hit("android.os.HandlerThread", "start"));
        // classify() honors remove_base at extraction time too.
        assert!(classify("java.lang.Thread", "start", &cfg).is_none());
    }
}
