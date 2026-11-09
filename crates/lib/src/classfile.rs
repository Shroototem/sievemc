//! Minimal, allocation-light JVM class-file reader used by the dependency
//! resolver ([`crate::deps`]).
//!
//! [`parse`] answers, for one class:
//!
//! 1. Is the whole class client-only? (`@Environment(EnvType.CLIENT)`,
//!    `@OnlyIn(Dist.CLIENT)`, implements `ClientModInitializer`, or a mixin
//!    whose every target is a client class.)
//! 2. Which other classes does its *non-client* code reference, including
//!    method references and lambdas (via `BootstrapMethods`)? Methods and
//!    fields annotated client-only are skipped, as are the synthetic lambda
//!    bodies of such methods.
//! 3. Which classes does a `@Mixin` target, and which calls match a method
//!    filter (e.g. `Registry.register`)?
//!
//! Only references whose name passes the caller-supplied `keep` filter are
//! materialized as `String`s, so JDK/Minecraft references cost nothing.
//!
//! Two cheaper scans complement it: [`forge_entry`] finds Forge/NeoForge
//! `@Mod` / `@EventBusSubscriber` classes and their `dist`, and
//! [`referenced_classes`] lists every class a constant pool mentions.

use std::collections::HashSet;

const ENVIRONMENT: &str = "Lnet/fabricmc/api/Environment;";
const ONLY_IN_FORGE: &str = "Lnet/minecraftforge/api/distmarker/OnlyIn;";
const ONLY_IN_NEOFORGE: &str = "Lnet/neoforged/api/distmarker/OnlyIn;";
const MIXIN: &str = "Lorg/spongepowered/asm/mixin/Mixin;";
const CLIENT_INITIALIZER: &str = "net/fabricmc/api/ClientModInitializer";

/// Minecraft classes that only exist on the physical client.
pub(crate) fn is_client_target(internal_name: &str) -> bool {
    internal_name.starts_with("net/minecraft/client/")
        || internal_name.starts_with("com/mojang/blaze3d/")
}

#[derive(Debug, Default)]
pub(crate) struct ClassInfo {
    /// The class (or every mixin target it has) is client-only.
    pub client_only: bool,
    /// Referenced class names (internal form, `a/b/C`) from non-client code,
    /// already filtered by `keep`, deduplicated.
    pub refs: Vec<String>,
    /// `@Mixin` targets (internal names), empty for non-mixin classes.
    pub mixin_targets: Vec<String>,
    /// Calls from non-client code matching the caller's method filter, as
    /// `owner.method`.
    pub method_hooks: Vec<String>,
}

#[derive(Clone, Copy)]
enum Cp {
    None,
    Utf8(u32, u16),
    Class(u16),
    NameAndType(u16, u16),
    Ref(u16, u16),
    MethodHandle(u16),
    MethodType(u16),
    /// (bootstrap method index, NameAndType)
    Dynamic(u16, u16),
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn u1(&mut self) -> Option<u8> {
        let v = *self.b.get(self.p)?;
        self.p += 1;
        Some(v)
    }
    fn u2(&mut self) -> Option<u16> {
        let s = self.b.get(self.p..self.p + 2)?;
        self.p += 2;
        Some(u16::from_be_bytes([s[0], s[1]]))
    }
    fn u4(&mut self) -> Option<u32> {
        let s = self.b.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        if self.p + n > self.b.len() {
            return None;
        }
        self.p += n;
        Some(())
    }
}

struct Pool<'a> {
    bytes: &'a [u8],
    entries: Vec<Cp>,
}

impl<'a> Pool<'a> {
    fn utf8(&self, i: u16) -> Option<&'a str> {
        match self.entries.get(i as usize)? {
            Cp::Utf8(off, len) => {
                let s = &self.bytes[*off as usize..*off as usize + *len as usize];
                // Modified UTF-8 is plain UTF-8 for every identifier we care about.
                std::str::from_utf8(s).ok()
            }
            _ => None,
        }
    }
    fn class_name(&self, i: u16) -> Option<&'a str> {
        match self.entries.get(i as usize)? {
            Cp::Class(n) => self.utf8(*n),
            _ => None,
        }
    }
}

/// Collects class names out of a descriptor/signature-ish string: every
/// `L...;` run, or the bare name for an internal class name.
fn push_descriptor<'a>(s: &'a str, out: &mut Vec<&'a str>) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'L' {
            if let Some(end) = s[i + 1..].find(';') {
                out.push(&s[i + 1..i + 1 + end]);
                i += end + 2;
                continue;
            }
            return;
        }
        i += 1;
    }
}

fn push_class_name<'a>(name: &'a str, out: &mut Vec<&'a str>) {
    if name.starts_with('[') {
        push_descriptor(name, out);
    } else {
        out.push(name);
    }
}

/// Every class name reachable from a constant-pool entry used by bytecode.
fn push_cp_refs<'a>(pool: &Pool<'a>, idx: u16, out: &mut Vec<&'a str>, depth: u8) {
    if depth > 3 {
        return;
    }
    let Some(entry) = pool.entries.get(idx as usize).copied() else {
        return;
    };
    match entry {
        Cp::Class(n) => {
            if let Some(name) = pool.utf8(n) {
                push_class_name(name, out);
            }
        }
        Cp::Ref(class, nat) => {
            if let Some(name) = pool.class_name(class) {
                push_class_name(name, out);
            }
            push_cp_refs(pool, nat, out, depth + 1);
        }
        Cp::NameAndType(_, desc) | Cp::MethodType(desc) => {
            if let Some(d) = pool.utf8(desc) {
                push_descriptor(d, out);
            }
        }
        Cp::Dynamic(_, nat) => push_cp_refs(pool, nat, out, depth + 1),
        Cp::MethodHandle(r) => push_cp_refs(pool, r, out, depth + 1),
        Cp::Utf8(..) | Cp::None => {}
    }
}

#[derive(Default)]
struct Annotations<'a> {
    client_only: bool,
    mixin_targets: Vec<&'a str>,
}

/// Parse a `Runtime(In)VisibleAnnotations` attribute body.
fn parse_annotations<'a>(pool: &Pool<'a>, body: &'a [u8], acc: &mut Annotations<'a>) {
    let mut r = Reader { b: body, p: 0 };
    let Some(n) = r.u2() else { return };
    for _ in 0..n {
        if parse_annotation(pool, &mut r, acc, 0).is_none() {
            return;
        }
    }
}

fn parse_annotation<'a>(
    pool: &Pool<'a>,
    r: &mut Reader<'a>,
    acc: &mut Annotations<'a>,
    depth: u8,
) -> Option<()> {
    let ty = pool.utf8(r.u2()?).unwrap_or("");
    let side_marker =
        depth == 0 && (ty == ENVIRONMENT || ty == ONLY_IN_FORGE || ty == ONLY_IN_NEOFORGE);
    let mixin = depth == 0 && ty == MIXIN;
    let pairs = r.u2()?;
    for _ in 0..pairs {
        r.u2()?; // element name
        parse_element(pool, r, acc, side_marker, mixin, depth)?;
    }
    Some(())
}

fn parse_element<'a>(
    pool: &Pool<'a>,
    r: &mut Reader<'a>,
    acc: &mut Annotations<'a>,
    side_marker: bool,
    mixin: bool,
    depth: u8,
) -> Option<()> {
    match r.u1()? {
        b's' => {
            let v = r.u2()?;
            if mixin && let Some(s) = pool.utf8(v) {
                acc.mixin_targets.push(s);
            }
        }
        b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' => {
            r.u2()?;
        }
        b'e' => {
            r.u2()?;
            let constant = r.u2()?;
            if side_marker && pool.utf8(constant) == Some("CLIENT") {
                acc.client_only = true;
            }
        }
        b'c' => {
            let v = r.u2()?;
            if mixin && let Some(d) = pool.utf8(v) {
                push_descriptor(d, &mut acc.mixin_targets);
            }
        }
        b'@' => {
            let mut nested = Annotations::default();
            parse_annotation(pool, r, &mut nested, depth + 1)?;
        }
        b'[' => {
            let n = r.u2()?;
            for _ in 0..n {
                parse_element(pool, r, acc, side_marker, mixin, depth)?;
            }
        }
        _ => return None,
    }
    Some(())
}

/// Read a member's attributes: client marker + (for methods) the Code body.
fn read_attributes<'a>(
    pool: &Pool<'a>,
    r: &mut Reader<'a>,
) -> Option<(Annotations<'a>, Option<&'a [u8]>)> {
    let mut ann = Annotations::default();
    let mut code = None;
    let count = r.u2()?;
    for _ in 0..count {
        let name = pool.utf8(r.u2()?).unwrap_or("");
        let len = r.u4()? as usize;
        let start = r.p;
        r.skip(len)?;
        let body = &r.b[start..start + len];
        match name {
            "RuntimeVisibleAnnotations" | "RuntimeInvisibleAnnotations" => {
                parse_annotations(pool, body, &mut ann)
            }
            "Code" => code = Some(body),
            _ => {}
        }
    }
    Some((ann, code))
}

/// `(owner, method name)` of a method-ref constant.
fn method_ref<'a>(pool: &Pool<'a>, idx: u16) -> Option<(&'a str, &'a str)> {
    let Cp::Ref(class, nat) = *pool.entries.get(idx as usize)? else {
        return None;
    };
    let Cp::NameAndType(name, _) = *pool.entries.get(nat as usize)? else {
        return None;
    };
    Some((pool.class_name(class)?, pool.utf8(name)?))
}

/// Walk a `Code` attribute's bytecode, pushing every class it references and
/// every invoked method that passes `method_hook`.
fn push_code_refs<'a>(
    pool: &Pool<'a>,
    code_attr: &'a [u8],
    out: &mut Vec<&'a str>,
    method_hook: &dyn Fn(&str, &str) -> bool,
    calls: &mut Vec<String>,
    bootstrap: &[Vec<u16>],
) -> Option<()> {
    let mut r = Reader { b: code_attr, p: 0 };
    r.skip(4)?; // max_stack, max_locals
    let len = r.u4()? as usize;
    let code = code_attr.get(8..8 + len)?;
    let mut pc = 0usize;
    let u2 = |at: usize| -> Option<u16> {
        Some(u16::from_be_bytes([*code.get(at)?, *code.get(at + 1)?]))
    };
    let i4 = |at: usize| -> Option<i32> {
        Some(i32::from_be_bytes([
            *code.get(at)?,
            *code.get(at + 1)?,
            *code.get(at + 2)?,
            *code.get(at + 3)?,
        ]))
    };
    while pc < code.len() {
        let op = code[pc];
        let size = match op {
            0x12 => {
                push_cp_refs(pool, *code.get(pc + 1)? as u16, out, 0);
                2
            }
            0x13 | 0x14 | 0xb2..=0xb8 | 0xbb | 0xbd | 0xc0 | 0xc1 => {
                let idx = u2(pc + 1)?;
                push_cp_refs(pool, idx, out, 0);
                if (0xb6..=0xb8).contains(&op)
                    && let Some((owner, name)) = method_ref(pool, idx)
                    && method_hook(owner, name)
                    && calls.len() < 4
                {
                    calls.push(format!("{owner}.{name}"));
                }
                3
            }
            0xb9 | 0xba => {
                let idx = u2(pc + 1)?;
                push_cp_refs(pool, idx, out, 0);
                if op == 0xb9 {
                    if let Some((owner, name)) = method_ref(pool, idx)
                        && method_hook(owner, name)
                        && calls.len() < 4
                    {
                        calls.push(format!("{owner}.{name}"));
                    }
                } else if let Some(Cp::Dynamic(bsm, _)) = pool.entries.get(idx as usize).copied() {
                    // Method references (`Foo::bar`) and lambdas live in the
                    // bootstrap arguments, not in the instruction itself.
                    for &arg in bootstrap.get(bsm as usize).into_iter().flatten() {
                        push_cp_refs(pool, arg, out, 0);
                    }
                }
                5
            }
            0xc5 => {
                push_cp_refs(pool, u2(pc + 1)?, out, 0);
                4
            }
            0x10 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => 2,
            0x11 | 0x84 | 0x99..=0xa8 | 0xc6 | 0xc7 => 3,
            0xc8 | 0xc9 => 5,
            0xc4 => {
                if *code.get(pc + 1)? == 0x84 {
                    6
                } else {
                    4
                }
            }
            0xaa => {
                let base = (pc + 4) & !3;
                let low = i4(base + 4)?;
                let high = i4(base + 8)?;
                let n = (high as i64 - low as i64 + 1).max(0) as usize;
                base + 12 + n * 4 - pc
            }
            0xab => {
                let base = (pc + 4) & !3;
                let n = i4(base + 4)?.max(0) as usize;
                base + 8 + n * 8 - pc
            }
            _ => 1,
        };
        pc += size;
    }
    Some(())
}

/// Parse `bytes` as a class file. Returns `None` for malformed input.
///
/// `keep` decides which referenced class names are worth returning (typically
/// "is this class owned by some jar in the mods folder?").
/// Read the constant pool. Returns the pool and a reader positioned at the
/// class's access flags.
fn read_pool(bytes: &[u8]) -> Option<(Pool<'_>, Reader<'_>)> {
    let mut r = Reader { b: bytes, p: 0 };
    if r.u4()? != 0xCAFEBABE {
        return None;
    }
    r.skip(4)?;
    let count = r.u2()? as usize;
    let mut entries = vec![Cp::None; count.max(1)];
    let mut i = 1;
    while i < count {
        let tag = r.u1()?;
        entries[i] = match tag {
            1 => {
                let len = r.u2()?;
                let off = r.p as u32;
                r.skip(len as usize)?;
                Cp::Utf8(off, len)
            }
            3 | 4 => {
                r.skip(4)?;
                Cp::None
            }
            5 | 6 => {
                r.skip(8)?;
                i += 1;
                Cp::None
            }
            7 => Cp::Class(r.u2()?),
            8 => {
                r.u2()?;
                Cp::None
            }
            16 => Cp::MethodType(r.u2()?),
            9..=11 => Cp::Ref(r.u2()?, r.u2()?),
            12 => Cp::NameAndType(r.u2()?, r.u2()?),
            15 => {
                r.u1()?;
                Cp::MethodHandle(r.u2()?)
            }
            17 | 18 => Cp::Dynamic(r.u2()?, r.u2()?),
            19 | 20 => {
                r.u2()?;
                Cp::None
            }
            _ => return None,
        };
        i += 1;
    }
    Some((Pool { bytes, entries }, r))
}

pub(crate) fn parse(
    bytes: &[u8],
    keep: impl Fn(&str) -> bool,
    method_hook: impl Fn(&str, &str) -> bool,
) -> Option<ClassInfo> {
    let (pool, mut r) = read_pool(bytes)?;

    r.skip(2)?; // access flags
    let this = pool.class_name(r.u2()?).unwrap_or("");
    let mut refs: Vec<&str> = Vec::new();
    let super_idx = r.u2()?;
    if let Some(s) = pool.class_name(super_idx) {
        refs.push(s);
    }
    let mut client_only = false;
    let n_ifaces = r.u2()?;
    for _ in 0..n_ifaces {
        if let Some(s) = pool.class_name(r.u2()?) {
            if s == CLIENT_INITIALIZER {
                client_only = true;
            }
            refs.push(s);
        }
    }

    let n_fields = r.u2()?;
    for _ in 0..n_fields {
        r.skip(4)?; // access, name
        let desc = pool.utf8(r.u2()?).unwrap_or("");
        let (ann, _) = read_attributes(&pool, &mut r)?;
        if !ann.client_only {
            push_descriptor(desc, &mut refs);
        }
    }

    // Methods: gather first so lambdas of client-only methods can be skipped.
    struct Method<'a> {
        name: &'a str,
        desc: &'a str,
        client: bool,
        code: Option<&'a [u8]>,
    }
    let n_methods = r.u2()?;
    let mut methods = Vec::with_capacity(n_methods as usize);
    for _ in 0..n_methods {
        r.skip(2)?;
        let name = pool.utf8(r.u2()?).unwrap_or("");
        let desc = pool.utf8(r.u2()?).unwrap_or("");
        let (ann, code) = read_attributes(&pool, &mut r)?;
        methods.push(Method {
            name,
            desc,
            client: ann.client_only,
            code,
        });
    }

    let mut class_ann = Annotations::default();
    let mut bootstrap: Vec<Vec<u16>> = Vec::new();
    let n_attrs = r.u2()?;
    for _ in 0..n_attrs {
        let name = pool.utf8(r.u2()?).unwrap_or("");
        let len = r.u4()? as usize;
        let start = r.p;
        r.skip(len)?;
        let body = &bytes[start..start + len];
        match name {
            "RuntimeVisibleAnnotations" | "RuntimeInvisibleAnnotations" => {
                parse_annotations(&pool, body, &mut class_ann)
            }
            "BootstrapMethods" => {
                let mut b = Reader { b: body, p: 0 };
                let count = b.u2().unwrap_or(0);
                for _ in 0..count {
                    let (Some(_method), Some(n)) = (b.u2(), b.u2()) else {
                        break;
                    };
                    let args: Vec<u16> = (0..n).filter_map(|_| b.u2()).collect();
                    bootstrap.push(args);
                }
            }
            _ => {}
        }
    }
    client_only |= class_ann.client_only;
    if !class_ann.mixin_targets.is_empty()
        && class_ann
            .mixin_targets
            .iter()
            .all(|t| is_client_target(&t.replace('.', "/")))
    {
        client_only = true;
    }
    let mixin_targets: Vec<String> = class_ann
        .mixin_targets
        .iter()
        .map(|t| t.replace('.', "/"))
        .collect();
    if client_only {
        return Some(ClassInfo {
            client_only: true,
            refs: Vec::new(),
            mixin_targets,
            method_hooks: Vec::new(),
        });
    }

    let mut method_hooks: Vec<String> = Vec::new();
    let client_methods: HashSet<&str> = methods
        .iter()
        .filter(|m| m.client)
        .map(|m| m.name)
        .collect();
    for m in &methods {
        if m.client {
            continue;
        }
        // javac names lambda bodies `lambda$<enclosing>$<n>`.
        if let Some(rest) = m.name.strip_prefix("lambda$")
            && let Some((enclosing, _)) = rest.rsplit_once('$')
            && client_methods.contains(enclosing)
        {
            continue;
        }
        push_descriptor(m.desc, &mut refs);
        if let Some(code) = m.code {
            push_code_refs(
                &pool,
                code,
                &mut refs,
                &method_hook,
                &mut method_hooks,
                &bootstrap,
            );
        }
    }

    let mut seen: HashSet<&str> = HashSet::with_capacity(refs.len());
    let refs = refs
        .into_iter()
        .filter(|n| *n != this && seen.insert(n) && keep(n))
        .map(str::to_string)
        .collect();
    Some(ClassInfo {
        client_only: false,
        refs,
        mixin_targets,
        method_hooks,
    })
}

const FORGE_ENTRY_TYPES: &[(&str, EntryKind)] = &[
    ("Lnet/neoforged/fml/common/Mod;", EntryKind::Mod),
    ("Lnet/minecraftforge/fml/common/Mod;", EntryKind::Mod),
    (
        "Lnet/neoforged/fml/common/EventBusSubscriber;",
        EntryKind::Subscriber,
    ),
    (
        "Lnet/neoforged/fml/common/Mod$EventBusSubscriber;",
        EntryKind::Subscriber,
    ),
    (
        "Lnet/minecraftforge/fml/common/Mod$EventBusSubscriber;",
        EntryKind::Subscriber,
    ),
];

/// A Forge/NeoForge entry point: an `@Mod` class or an `@EventBusSubscriber`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    Mod,
    Subscriber,
}

/// Which physical sides an entry point is loaded on (`dist = ...`; none = both).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Entry {
    pub kind: EntryKind,
    pub client: bool,
    pub server: bool,
}

/// If this class is a Forge/NeoForge entry point, report its kind and `dist`.
///
/// Cheap for the common case: after the constant pool is read, it returns
/// unless one of the entry annotation descriptors is present, without walking
/// fields or methods.
pub(crate) fn forge_entry(bytes: &[u8]) -> Option<Entry> {
    let (pool, mut r) = read_pool(bytes)?;
    let present = pool.entries.iter().enumerate().any(|(i, e)| {
        matches!(e, Cp::Utf8(_, len) if (30..=60).contains(len))
            && pool
                .utf8(i as u16)
                .is_some_and(|s| FORGE_ENTRY_TYPES.iter().any(|(t, _)| *t == s))
    });
    if !present {
        return None;
    }
    // Skip to the class attributes.
    r.skip(6)?;
    let n = r.u2()? as usize;
    r.skip(n * 2)?;
    for _ in 0..2 {
        let members = r.u2()?;
        for _ in 0..members {
            r.skip(6)?;
            let attrs = r.u2()?;
            for _ in 0..attrs {
                r.skip(2)?;
                let len = r.u4()? as usize;
                r.skip(len)?;
            }
        }
    }
    let attrs = r.u2()?;
    let mut found: Option<Entry> = None;
    for _ in 0..attrs {
        let name = pool.utf8(r.u2()?).unwrap_or("");
        let len = r.u4()? as usize;
        let start = r.p;
        r.skip(len)?;
        if name != "RuntimeVisibleAnnotations" && name != "RuntimeInvisibleAnnotations" {
            continue;
        }
        let mut a = Reader {
            b: &bytes[start..start + len],
            p: 0,
        };
        let count = a.u2()?;
        for _ in 0..count {
            let ty = pool.utf8(a.u2()?).unwrap_or("");
            let kind = FORGE_ENTRY_TYPES
                .iter()
                .find(|(t, _)| *t == ty)
                .map(|(_, k)| *k);
            let mut dists: Vec<&str> = Vec::new();
            let pairs = a.u2()?;
            for _ in 0..pairs {
                let element = pool.utf8(a.u2()?).unwrap_or("");
                let want = kind.is_some() && (element == "dist" || element == "value");
                collect_enums(&pool, &mut a, want, &mut dists)?;
            }
            if let Some(kind) = kind {
                let (client, server) = if dists.is_empty() {
                    (true, true)
                } else {
                    (
                        dists.contains(&"CLIENT"),
                        dists.contains(&"DEDICATED_SERVER"),
                    )
                };
                // A class can carry both annotations; @Mod wins.
                if found.is_none_or(|f| f.kind == EntryKind::Subscriber) {
                    found = Some(Entry {
                        kind,
                        client,
                        server,
                    });
                }
            }
        }
    }
    found
}

/// Skip one element value, collecting enum constant names when `want`.
fn collect_enums<'a>(
    pool: &Pool<'a>,
    r: &mut Reader<'a>,
    want: bool,
    out: &mut Vec<&'a str>,
) -> Option<()> {
    match r.u1()? {
        b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' | b's' | b'c' => {
            r.u2()?;
        }
        b'e' => {
            r.u2()?;
            let c = r.u2()?;
            if want && let Some(name) = pool.utf8(c) {
                out.push(name);
            }
        }
        b'@' => {
            r.u2()?;
            let pairs = r.u2()?;
            for _ in 0..pairs {
                r.u2()?;
                collect_enums(pool, r, false, out)?;
            }
        }
        b'[' => {
            let n = r.u2()?;
            for _ in 0..n {
                collect_enums(pool, r, want, out)?;
            }
        }
        _ => return None,
    }
    Some(())
}

/// Cheap whole-class reference scan: every class name mentioned anywhere in the
/// constant pool (class entries and descriptors/signatures), *including* client
/// code, filtered by `keep`. No method walking. Used to prove that no installed
/// client mod references a library before it is reported unused.
pub(crate) fn referenced_classes(bytes: &[u8], keep: impl Fn(&str) -> bool) -> Vec<String> {
    let mut r = Reader { b: bytes, p: 0 };
    let mut names: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    let mut scan = || -> Option<()> {
        if r.u4()? != 0xCAFEBABE {
            return None;
        }
        r.skip(4)?;
        let count = r.u2()? as usize;
        let mut i = 1;
        while i < count {
            match r.u1()? {
                1 => {
                    let len = r.u2()? as usize;
                    let start = r.p;
                    r.skip(len)?;
                    if let Ok(s) = std::str::from_utf8(&bytes[start..start + len]) {
                        // Class entries point at these too, so bare names are covered.
                        names.push(s);
                        push_descriptor(s, &mut names);
                    }
                }
                3 | 4 | 9..=12 | 17 | 18 => r.skip(4)?,
                5 | 6 => {
                    r.skip(8)?;
                    i += 1;
                }
                7 | 8 | 16 | 19 | 20 => r.skip(2)?,
                15 => r.skip(3)?,
                _ => return None,
            }
            i += 1;
        }
        Some(())
    };
    let _ = scan();
    let mut seen = HashSet::new();
    for n in names {
        let n = n.strip_prefix('[').map_or(n, |x| x.trim_start_matches('['));
        if seen.insert(n) && keep(n) {
            out.push(n.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_extraction() {
        let mut out = Vec::new();
        push_descriptor("(Lfoo/Bar;I[Lbaz/Qux;)Ljava/lang/String;", &mut out);
        assert_eq!(out, vec!["foo/Bar", "baz/Qux", "java/lang/String"]);
    }

    #[test]
    fn rejects_non_class() {
        assert!(parse(b"not a class", |_| true, |_, _| false).is_none());
    }
}
