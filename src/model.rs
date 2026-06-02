//! Core data model: the parsed dex shape plus the abstract-lock identity.
//!
//! Stage 0 keeps lock identity deliberately simple (class-qualified field /
//! static / class-const / this-monitor / alloc / distinct-opaque). Stage 2 makes
//! it receiver-sensitive (access paths + alloc-site) to kill the two-instance
//! false-merge. The `Lock` enum is shaped so that refinement is additive.

use serde::Serialize;
use std::fmt;

/// A register number in a dex method (`v0`, `v12`, ...).
pub type Reg = u32;

/// A decoded dex instruction — only the opcode classes that matter for lock
/// analysis are modeled; everything else is `Other` (it still defines a register
/// so dataflow stays total, but it never produces a lock and never aliases).
#[derive(Debug, Clone)]
pub enum Op {
    MonitorEnter(Reg),
    MonitorExit(Reg),
    /// `sget-object dst, Lcls;.field:type`
    Sget { dst: Reg, class: String, field: String },
    /// `iget-object dst, base, Lcls;.field:type`
    Iget { dst: Reg, base: Reg, class: String, field: String },
    /// `iput-object src, base, Lcls;.field:type` (used for lambda capture summaries)
    Iput { src: Reg, base: Reg, class: String, field: String },
    /// `const-class dst, Lcls;`
    ConstClass { dst: Reg, class: String },
    /// `new-instance dst, Lcls;`
    NewInstance { dst: Reg, class: String },
    /// `move-object dst, src` (and width variants)
    Move { dst: Reg, src: Reg },
    /// `move-result-object dst` — picks up the previous invoke's return value.
    MoveResult { dst: Reg },
    /// any `invoke-*`
    Invoke(Invoke),
    /// `return-object src` (carries the returned register for value summaries);
    /// non-object / void returns carry `None`.
    Return(Option<Reg>),
    /// defines `dst` with an opaque value (clears any lock tracked there).
    Def(Reg),
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvokeKind {
    Direct,
    Static,
    Virtual,
    Interface,
    Super,
}

#[derive(Debug, Clone)]
pub struct Invoke {
    pub kind: InvokeKind,
    pub args: Vec<Reg>,
    /// callee declaring class in dotted form, e.g. `corpus.Foo`
    pub class: String,
    pub name: String,
    pub sig: String,
}

impl Invoke {
    /// `class.name:sig` — the global method key used to look up summaries.
    pub fn key(&self) -> String {
        format!("{}.{}:{}", self.class, self.name, self.sig)
    }
}

/// One decoded instruction with its dex code offset (used for line mapping).
#[derive(Debug, Clone)]
pub struct Insn {
    pub offset: u32,
    pub op: Op,
}

#[derive(Debug, Clone)]
pub struct Method {
    /// dotted class, e.g. `corpus.T01_SimpleABBA`
    pub class: String,
    pub name: String,
    pub sig: String,
    pub access: u32,
    pub registers: u32,
    pub ins: u32,
    pub insns: Vec<Insn>,
    /// (code_offset, source_line) sorted by offset.
    pub positions: Vec<(u32, u32)>,
    pub source_file: Option<String>,
}

impl Method {
    pub fn key(&self) -> String {
        format!("{}.{}:{}", self.class, self.name, self.sig)
    }
    pub fn is_static(&self) -> bool {
        self.access & 0x8 != 0
    }
    /// Register holding `this` for an instance method (params live in the high
    /// registers: `this` = registers - ins).
    pub fn this_reg(&self) -> Option<Reg> {
        if self.is_static() || self.ins == 0 {
            None
        } else {
            Some(self.registers - self.ins)
        }
    }
    /// Source line for a code offset (largest position <= offset).
    pub fn line_at(&self, offset: u32) -> Option<u32> {
        let mut best = None;
        for &(off, line) in &self.positions {
            if off <= offset {
                best = Some(line);
            } else {
                break;
            }
        }
        best
    }
}

#[derive(Debug, Clone)]
pub struct Class {
    pub descriptor: String, // dotted
    pub super_class: Option<String>,
    pub interfaces: Vec<String>,
    pub methods: Vec<Method>,
}

#[derive(Debug, Default)]
pub struct Dex {
    pub classes: Vec<Class>,
}

/// Read/write mode for a `ReadWriteLock`-derived lock; `Plain` for monitors and
/// ordinary `Lock`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum Mode {
    Plain,
    Read,
    Write,
}

/// The root of an access path. `This`/`Param` are *parametric* (only valid inside
/// a method summary); they are substituted at call sites and grounded to
/// `Recv`/`Opaque` when an edge is emitted into the global graph.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub enum Root {
    /// receiver formal of the method (`p0`); parametric.
    This,
    /// formal parameter index (instance methods: 0 = this); parametric.
    Param(u32),
    /// a representative instance of a class (grounded `This`).
    Recv(String),
    /// a class that owns the leading static field of the path.
    Static(String),
    /// `Cls.class` literal.
    ClassConst(String),
    /// an allocation site `method+offset:Type`.
    Alloc(String),
    /// unresolved value — *distinct per def site* so two unknowns never merge.
    Opaque(String),
}

/// Abstract lock identity as a bounded access path. The `name()` is the canonical
/// id the graph keys on, so two locks are "the same" iff their names match.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Lock {
    pub root: Root,
    pub fields: Vec<String>,
    pub mode: Mode,
}

/// Maximum access-path length (RacerD-style k=3 keeps it bounded; longer paths
/// truncate to a distinct opaque so they never spuriously merge).
pub const MAX_AP: usize = 3;

impl Lock {
    pub fn new(root: Root) -> Self {
        Lock { root, fields: Vec::new(), mode: Mode::Plain }
    }
    pub fn field(root: Root, field: impl Into<String>) -> Self {
        Lock { root, fields: vec![field.into()], mode: Mode::Plain }
    }
    /// Extend this path by additional fields (used during call-site substitution).
    pub fn append(&self, more: &[String], mode: Mode) -> Lock {
        let mut fields = self.fields.clone();
        fields.extend_from_slice(more);
        let m = if mode != Mode::Plain { mode } else { self.mode };
        if fields.len() > MAX_AP {
            // truncated -> distinct opaque keyed by what we know (sound: no merge)
            return Lock::new(Root::Opaque(format!(
                "trunc:{}",
                Lock { root: self.root.clone(), fields, mode: m }.name()
            )));
        }
        Lock { root: self.root.clone(), fields, mode: m }
    }
    pub fn with_mode(&self, mode: Mode) -> Lock {
        Lock { mode, ..self.clone() }
    }
    pub fn name(&self) -> String {
        let mut s = match &self.root {
            Root::This => "this".to_string(),
            Root::Param(i) => format!("$p{i}"),
            Root::Recv(c) => c.clone(),
            Root::Static(c) => c.clone(),
            Root::ClassConst(c) => format!("{c}.class"),
            Root::Alloc(site) => format!("new@{site}"),
            Root::Opaque(site) => format!("?@{site}"),
        };
        for f in &self.fields {
            s.push('.');
            s.push_str(f);
        }
        match self.mode {
            Mode::Read => s.push_str(".read"),
            Mode::Write => s.push_str(".write"),
            Mode::Plain => {}
        }
        s
    }
    /// Opaque locks are unresolved; they must never participate in a merge/cycle.
    pub fn is_opaque(&self) -> bool {
        matches!(self.root, Root::Opaque(_))
    }
    /// Parametric locks (rooted at This/Param) are only valid inside summaries.
    pub fn is_parametric(&self) -> bool {
        matches!(self.root, Root::This | Root::Param(_))
    }
    /// Ground a still-parametric lock for emission into the global graph of
    /// method `key` in class `class`.
    pub fn ground(&self, class: &str, key: &str) -> Lock {
        match &self.root {
            Root::This => Lock { root: Root::Recv(class.to_string()), ..self.clone() },
            Root::Param(i) => Lock::new(Root::Opaque(format!("{key}#p{i}"))),
            _ => self.clone(),
        }
    }
}

impl fmt::Display for Lock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

/// Substitute a callee-summary lock into the caller's frame given the call's
/// argument bindings (`args[0]` is the receiver for instance calls; each entry is
/// an optional resolved access path in the caller's frame). `This` and `Param(0)`
/// both bind to `args[0]`. Returns `None` when the binding is unknown (so the edge
/// is soundly dropped rather than fabricated).
pub fn subst(lock: &Lock, args: &[Option<Lock>]) -> Option<Lock> {
    let bind = |i: usize| -> Option<Lock> {
        args.get(i).and_then(|o| o.as_ref()).map(|a| a.append(&lock.fields, lock.mode))
    };
    match &lock.root {
        Root::This => bind(0),
        Root::Param(i) => bind(*i as usize),
        _ => Some(lock.clone()),
    }
}

/// Turn a dex type descriptor (`Lcom/android/Foo;`) into dotted form
/// (`com.android.Foo`). Non-object descriptors pass through unchanged.
pub fn descriptor_to_dotted(desc: &str) -> String {
    let d = desc.trim();
    if let Some(inner) = d.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
        inner.replace('/', ".")
    } else {
        d.to_string()
    }
}
