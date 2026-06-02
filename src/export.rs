//! Artifact exporters: pprof (profile.proto, gzipped) and HPROF (Android heap
//! dump for Perfetto's `art_hprof` importer). The byte layouts mirror the
//! validated Python emitters so `go tool pprof` and Perfetto accept them.
//!
//!  * pprof of the LOCK-ORDER graph — each lock is a Function/Location; each order
//!    edge holder->acquired is a Sample with stack [acquired, holder] and value =
//!    evidence count, tagged `cycle=yes|no`.
//!  * HPROF of the METHOD/LOCK graph — each method is an object whose class is its
//!    FQN; an outgoing reference named after a lock means "calls the target while
//!    holding that lock".

use crate::analyze::Analysis;
use crate::graph::LockGraph;
use std::collections::{HashMap, HashSet};
use std::io::Write;

// --------------------------------------------------------------------------
// protobuf wire helpers
// --------------------------------------------------------------------------

fn varint(mut n: u64, out: &mut Vec<u8>) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            out.push(b | 0x80);
        } else {
            out.push(b);
            break;
        }
    }
}

fn tag(field: u64, wt: u64, out: &mut Vec<u8>) {
    varint((field << 3) | wt, out);
}

/// length-delimited field
fn ld(field: u64, body: &[u8], out: &mut Vec<u8>) {
    tag(field, 2, out);
    varint(body.len() as u64, out);
    out.extend_from_slice(body);
}

/// varint field
fn vint(field: u64, n: u64, out: &mut Vec<u8>) {
    tag(field, 0, out);
    varint(n, out);
}

fn packed(field: u64, nums: &[u64], out: &mut Vec<u8>) {
    let mut body = Vec::new();
    for &x in nums {
        varint(x, &mut body);
    }
    ld(field, &body, out);
}

// --------------------------------------------------------------------------
// pprof — lock-order graph
// --------------------------------------------------------------------------

pub fn pprof_lock_order(g: &LockGraph) -> Vec<u8> {
    let in_cycle: HashSet<usize> = {
        let mut s = HashSet::new();
        for comp in g.deadlock_sccs() {
            if g.common_guard(&comp).is_empty() {
                s.extend(comp);
            }
        }
        s
    };

    // string table (index 0 must be "")
    let mut strtab: Vec<String> = vec![String::new()];
    let mut idx: HashMap<String, u64> = HashMap::new();
    idx.insert(String::new(), 0);
    let mut intern = |x: &str, strtab: &mut Vec<String>, idx: &mut HashMap<String, u64>| -> u64 {
        if let Some(&i) = idx.get(x) {
            return i;
        }
        let i = strtab.len() as u64;
        strtab.push(x.to_string());
        idx.insert(x.to_string(), i);
        i
    };
    let st_edges = intern("edges", &mut strtab, &mut idx);
    let st_count = intern("count", &mut strtab, &mut idx);
    let st_cycle = intern("cycle", &mut strtab, &mut idx);
    let st_yes = intern("yes", &mut strtab, &mut idx);
    let st_no = intern("no", &mut strtab, &mut idx);

    // function + location per lock node (id = index+1)
    let mut name_sid: Vec<u64> = Vec::with_capacity(g.nodes.len());
    for nd in &g.nodes {
        let sid = intern(&nd.name(), &mut strtab, &mut idx);
        name_sid.push(sid);
    }

    let mut prof: Vec<u8> = Vec::new();
    // sample_type ValueType{type=edges, unit=count}  (field 1)
    {
        let mut vt = Vec::new();
        vint(1, st_edges, &mut vt);
        vint(2, st_count, &mut vt);
        ld(1, &vt, &mut prof);
    }
    // samples (field 2): stack leaf-first [acquired, holder]
    for (a, b, ev) in g.sorted_evidence() {
        // only real (blocking-or-not) order edges; skip self
        if a == b {
            continue;
        }
        let cnt = ev.len() as u64;
        let mut sm = Vec::new();
        packed(1, &[(a as u64) + 1, (b as u64) + 1], &mut sm); // location_id [to, from]
        packed(2, &[cnt], &mut sm); // value
        let incyc = in_cycle.contains(&a) && in_cycle.contains(&b);
        let mut lab = Vec::new();
        vint(1, st_cycle, &mut lab);
        vint(2, if incyc { st_yes } else { st_no }, &mut lab);
        ld(3, &lab, &mut sm);
        ld(2, &sm, &mut prof);
    }
    // locations (field 4)
    for i in 0..g.nodes.len() as u64 {
        let mut line = Vec::new();
        vint(1, i + 1, &mut line); // function_id
        vint(2, 0, &mut line); // line
        let mut loc = Vec::new();
        vint(1, i + 1, &mut loc); // id
        ld(4, &line, &mut loc); // line
        ld(4, &loc, &mut prof);
    }
    // functions (field 5)
    for i in 0..g.nodes.len() as u64 {
        let mut fnc = Vec::new();
        vint(1, i + 1, &mut fnc); // id
        vint(2, name_sid[i as usize], &mut fnc); // name
        vint(3, name_sid[i as usize], &mut fnc); // system_name
        vint(4, name_sid[i as usize], &mut fnc); // filename
        ld(5, &fnc, &mut prof);
    }
    // string table (field 6)
    for s in &strtab {
        ld(6, s.as_bytes(), &mut prof);
    }
    vint(9, 0, &mut prof); // time_nanos
    {
        let mut pt = Vec::new();
        vint(1, st_edges, &mut pt);
        vint(2, st_count, &mut pt);
        ld(11, &pt, &mut prof); // period_type
    }
    vint(12, 1, &mut prof); // period

    gzip(&prof)
}

fn gzip(data: &[u8]) -> Vec<u8> {
    // minimal gzip via flate2 is not a dep; use a tiny stored-block gzip.
    // gzip header + DEFLATE stored blocks + CRC32 + ISIZE.
    let mut out = Vec::new();
    out.extend_from_slice(&[0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0xff]);
    // stored deflate blocks (max 65535 each)
    let mut i = 0;
    while i < data.len() {
        let chunk = &data[i..(i + 65535).min(data.len())];
        let last = i + chunk.len() >= data.len();
        out.push(if last { 1 } else { 0 }); // BFINAL, BTYPE=00
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
        i += chunk.len();
    }
    if data.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    let crc = crc32(data);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

// --------------------------------------------------------------------------
// HPROF — method/lock graph
// --------------------------------------------------------------------------

struct Hprof {
    next_id: u32,
    strings: Vec<u8>,
    string_ids: HashMap<String, u32>,
    load: Vec<u8>,
    class_dumps: Vec<u8>,
    instances: Vec<u8>,
    arrays: Vec<u8>,
    roots: Vec<u8>,
}

impl Hprof {
    const T_OBJECT: u8 = 2;
    const TAG_STRING: u8 = 0x01;
    const TAG_LOAD_CLASS: u8 = 0x02;
    const TAG_HEAP_DUMP_SEG: u8 = 0x1C;
    const TAG_HEAP_DUMP_END: u8 = 0x2C;
    const SUB_CLASS_DUMP: u8 = 0x20;
    const SUB_INSTANCE_DUMP: u8 = 0x21;
    const SUB_OBJ_ARRAY: u8 = 0x22;
    const ROOT_UNKNOWN: u8 = 0xFF;

    fn new() -> Self {
        Hprof {
            next_id: 1,
            strings: Vec::new(),
            string_ids: HashMap::new(),
            load: Vec::new(),
            class_dumps: Vec::new(),
            instances: Vec::new(),
            arrays: Vec::new(),
            roots: Vec::new(),
        }
    }
    fn id(&mut self) -> u32 {
        let i = self.next_id;
        self.next_id += 1;
        i
    }
    fn intern(&mut self, text: &str) -> u32 {
        if let Some(&s) = self.string_ids.get(text) {
            return s;
        }
        let sid = self.id();
        self.string_ids.insert(text.to_string(), sid);
        let mut body = Vec::new();
        body.extend_from_slice(&sid.to_be_bytes());
        body.extend_from_slice(text.as_bytes());
        self.strings.push(Self::TAG_STRING);
        self.strings.extend_from_slice(&0u32.to_be_bytes());
        self.strings.extend_from_slice(&(body.len() as u32).to_be_bytes());
        self.strings.extend_from_slice(&body);
        sid
    }
    fn add_class(&mut self, name: &str, field_names: &[String]) -> u32 {
        let name_sid = self.intern(name);
        let cid = self.id();
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&cid.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&name_sid.to_be_bytes());
        self.load.push(Self::TAG_LOAD_CLASS);
        self.load.extend_from_slice(&0u32.to_be_bytes());
        self.load.extend_from_slice(&(body.len() as u32).to_be_bytes());
        self.load.extend_from_slice(&body);

        let mut dump = Vec::new();
        dump.extend_from_slice(&cid.to_be_bytes());
        dump.extend_from_slice(&0u32.to_be_bytes()); // stack serial
        for _ in 0..6 {
            dump.extend_from_slice(&0u32.to_be_bytes()); // super, loader, ...
        }
        dump.extend_from_slice(&((field_names.len() * 4) as u32).to_be_bytes()); // instance size
        dump.extend_from_slice(&0u16.to_be_bytes()); // constant pool
        dump.extend_from_slice(&0u16.to_be_bytes()); // static fields
        dump.extend_from_slice(&(field_names.len() as u16).to_be_bytes());
        for fname in field_names {
            let fsid = self.intern(fname);
            dump.extend_from_slice(&fsid.to_be_bytes());
            dump.push(Self::T_OBJECT);
        }
        self.class_dumps.push(Self::SUB_CLASS_DUMP);
        self.class_dumps.extend_from_slice(&dump);
        cid
    }
    fn reserve(&mut self) -> u32 {
        self.id()
    }
    fn add_instance(&mut self, obj_id: u32, class_id: u32, targets: &[u32]) {
        let mut data = Vec::new();
        for t in targets {
            data.extend_from_slice(&t.to_be_bytes());
        }
        self.instances.push(Self::SUB_INSTANCE_DUMP);
        self.instances.extend_from_slice(&obj_id.to_be_bytes());
        self.instances.extend_from_slice(&0u32.to_be_bytes());
        self.instances.extend_from_slice(&class_id.to_be_bytes());
        self.instances.extend_from_slice(&(data.len() as u32).to_be_bytes());
        self.instances.extend_from_slice(&data);
    }
    fn add_object_array(&mut self, class_id: u32, elements: &[u32]) -> u32 {
        let aid = self.id();
        self.arrays.push(Self::SUB_OBJ_ARRAY);
        self.arrays.extend_from_slice(&aid.to_be_bytes());
        self.arrays.extend_from_slice(&0u32.to_be_bytes());
        self.arrays.extend_from_slice(&(elements.len() as u32).to_be_bytes());
        self.arrays.extend_from_slice(&class_id.to_be_bytes());
        for e in elements {
            self.arrays.extend_from_slice(&e.to_be_bytes());
        }
        aid
    }
    fn add_root(&mut self, obj_id: u32) {
        self.roots.push(Self::ROOT_UNKNOWN);
        self.roots.extend_from_slice(&obj_id.to_be_bytes());
    }
    fn finish(&self) -> Vec<u8> {
        let mut segment = Vec::new();
        segment.extend_from_slice(&self.class_dumps);
        segment.extend_from_slice(&self.instances);
        segment.extend_from_slice(&self.arrays);
        segment.extend_from_slice(&self.roots);
        let mut out = Vec::new();
        out.extend_from_slice(b"JAVA PROFILE 1.0.3\x00");
        out.extend_from_slice(&4u32.to_be_bytes()); // id size
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&self.strings);
        out.extend_from_slice(&self.load);
        out.push(Self::TAG_HEAP_DUMP_SEG);
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&(segment.len() as u32).to_be_bytes());
        out.extend_from_slice(&segment);
        out.push(Self::TAG_HEAP_DUMP_END);
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out
    }
}

/// Build a method/lock-graph HPROF: object per method, references named by the
/// lock held at each call edge.
pub fn hprof_method_graph(an: &Analysis) -> Vec<u8> {
    // group edges by caller, preserving (lock, callee) order.
    let mut by_caller: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
    let mut nodes: HashSet<&str> = HashSet::new();
    for (caller, lock, callee) in &an.method_edges {
        by_caller.entry(caller).or_default().push((lock, callee));
        nodes.insert(caller);
        nodes.insert(callee);
    }
    let mut node_list: Vec<&str> = nodes.into_iter().collect();
    node_list.sort_unstable();

    let mut w = Hprof::new();
    let mut class_id: HashMap<&str, u32> = HashMap::new();
    for &m in &node_list {
        let fields: Vec<String> = by_caller
            .get(m)
            .map(|es| es.iter().map(|(l, _)| (*l).to_string()).collect())
            .unwrap_or_default();
        class_id.insert(m, w.add_class(m, &fields));
    }
    let root_array_class = w.add_class("LockGraphRoot[]", &[]);
    let obj_id: HashMap<&str, u32> = node_list.iter().map(|&m| (m, w.reserve())).collect();
    for &m in &node_list {
        let targets: Vec<u32> = by_caller
            .get(m)
            .map(|es| es.iter().map(|(_, callee)| obj_id[*callee]).collect())
            .unwrap_or_default();
        w.add_instance(obj_id[m], class_id[m], &targets);
    }
    let elements: Vec<u32> = node_list.iter().map(|&m| obj_id[m]).collect();
    let root = w.add_object_array(root_array_class, &elements);
    w.add_root(root);
    w.finish()
}

pub fn write_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)
}
