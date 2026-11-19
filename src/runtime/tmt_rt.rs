// Injected by targeted-mutation.
// This file is not part of the original crate. It records what the tests observe at each
// instrumented site (TMT_TRACE=<file>) and switches one mutant on (TMT_MUTANT=<site>:<mutation>).
#![allow(dead_code, unused, missing_docs, clippy::all)]

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// Integer value of an operand with the bounds of its type: `(value, MIN, MAX)`.
pub type Val = Option<(i128, i128, i128)>;

// Autoref specialisation: `(&W(x)).tmt_val()` resolves to `ValInt` for integers
// and falls back to `ValAny` for every other type.
pub struct W<'a, T: ?Sized>(pub &'a T);

pub trait ValInt {
    fn tmt_val(&self) -> Val;
}

pub trait ValAny {
    fn tmt_val(&self) -> Val;
}

macro_rules! tmt_ints {
    ($($t:ty)*) => {$(
        impl<'a> ValInt for W<'a, $t> {
            fn tmt_val(&self) -> Val {
                Some((*self.0 as i128, <$t>::MIN as i128, <$t>::MAX as i128))
            }
        }
        impl<'a, 'b> ValInt for W<'a, &'b $t> {
            fn tmt_val(&self) -> Val {
                Some((**self.0 as i128, <$t>::MIN as i128, <$t>::MAX as i128))
            }
        }
    )*};
}
tmt_ints!(i8 i16 i32 i64 isize u8 u16 u32 u64 usize);

impl<'a, 'r, T: ?Sized> ValAny for &'r W<'a, T> {
    fn tmt_val(&self) -> Val {
        None
    }
}

pub const LT: u8 = 0;
pub const LE: u8 = 1;
pub const GT: u8 = 2;
pub const GE: u8 = 3;
pub const EQ: u8 = 4;
pub const NE: u8 = 5;

const REL_LT: u8 = 1;
const REL_EQ: u8 = 2;
const REL_GT: u8 = 4;
const REL_UN: u8 = 8;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mutant {
    Const(bool),
    Negate,
    Op(u8),
    Shift(i128),
    Delete,
}

pub fn parse_mutant(spec: &str) -> Option<(u32, Mutant)> {
    let (site, m) = spec.split_once(':')?;
    let site = site.parse().ok()?;
    let m = match m {
        "c0" => Mutant::Const(false),
        "c1" => Mutant::Const(true),
        "neg" => Mutant::Negate,
        "del" => Mutant::Delete,
        _ if m.starts_with("op") => Mutant::Op(m[2..].parse().ok().filter(|c| *c <= NE)?),
        _ if m.starts_with("sh") => Mutant::Shift(m[2..].parse().ok()?),
        _ => return None,
    };
    Some((site, m))
}

fn eval_rel(op: u8, rel: u8) -> bool {
    match (op, rel) {
        (NE, REL_UN) => true,
        (_, REL_UN) => false,
        (LT, r) => r == REL_LT,
        (LE, r) => r != REL_GT,
        (GT, r) => r == REL_GT,
        (GE, r) => r != REL_LT,
        (EQ, r) => r == REL_EQ,
        (_, r) => r != REL_EQ,
    }
}

fn rel_of_d(d: i128) -> u8 {
    if d < 0 {
        REL_LT
    } else if d == 0 {
        REL_EQ
    } else {
        REL_GT
    }
}

/// Result of the comparison under a mutant. `orig` is what the real operator returned.
pub fn apply(op: u8, m: Mutant, orig: bool, rel: u8, d: Option<i128>) -> bool {
    match m {
        Mutant::Const(c) => c,
        Mutant::Negate => !orig,
        Mutant::Op(op2) => eval_rel(op2, rel),
        Mutant::Shift(s) => match d {
            Some(d) => eval_rel(op, rel_of_d(d - s)),
            None => orig,
        },
        Mutant::Delete => orig,
    }
}

/// Per (site, test) summary. Mirrors `plan::Obs`.
#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub struct Summary {
    pub rels: u8,
    pub unvalued: bool,
    pub max_neg: Option<i128>,
    pub min_pos: Option<i128>,
}

impl Summary {
    /// Folds one observation in; returns whether the summary changed.
    pub fn observe(&mut self, rel: u8, d: Option<i128>) -> bool {
        let before = *self;
        self.rels |= rel;
        match d {
            None => self.unvalued = true,
            Some(d) if d < 0 => self.max_neg = Some(self.max_neg.map_or(d, |m| m.max(d))),
            Some(d) if d > 0 => self.min_pos = Some(self.min_pos.map_or(d, |p| p.min(d))),
            Some(_) => {}
        }
        *self != before
    }

    pub fn line(&self, site: u32, test: &str) -> String {
        let opt = |v: Option<i128>| v.map_or("-".to_string(), |v| v.to_string());
        format!(
            "O\t{}\t{}\t{}\t{}\t{}\t{}\n",
            site,
            test,
            self.rels,
            self.unvalued as u8,
            opt(self.max_neg),
            opt(self.min_pos)
        )
    }
}

struct Tracer {
    out: std::fs::File,
    names: Vec<String>,
    ids: HashMap<String, u32>,
    cmp: HashMap<(u32, u32), Summary>,
    reach: HashSet<(u32, u32)>,
    typed: HashSet<u32>,
}

struct State {
    mutant: Option<(u32, Mutant)>,
    tracer: Option<Mutex<Tracer>>,
}

fn state() -> &'static State {
    static STATE: OnceLock<State> = OnceLock::new();
    STATE.get_or_init(|| {
        let mutant = std::env::var("TMT_MUTANT").ok().and_then(|s| parse_mutant(&s));
        let tracer = std::env::var_os("TMT_TRACE").and_then(|path| {
            let out = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
            Some(Mutex::new(Tracer {
                out,
                names: Vec::new(),
                ids: HashMap::new(),
                cmp: HashMap::new(),
                reach: HashSet::new(),
                typed: HashSet::new(),
            }))
        });
        State { mutant, tracer }
    })
}

/// libtest names each test thread after the test. Anything else is reported as `?`,
/// which the driver treats as "any test of this binary".
fn test_name() -> String {
    match std::thread::current().name() {
        Some(name) if name != "main" && !name.contains('\t') && !name.contains('\n') => {
            name.to_string()
        }
        _ => "?".to_string(),
    }
}

thread_local! {
    static TEST_ID: Cell<Option<u32>> = const { Cell::new(None) };
}

fn with_tracer(f: impl FnOnce(&mut Tracer, u32)) {
    let Some(tracer) = &state().tracer else { return };
    let mut tracer = match tracer.lock() {
        Ok(t) => t,
        Err(poisoned) => poisoned.into_inner(),
    };
    let cached = TEST_ID.try_with(|c| c.get()).ok().flatten();
    let id = match cached {
        Some(id) => id,
        None => {
            let name = test_name();
            let id = match tracer.ids.get(&name) {
                Some(id) => *id,
                None => {
                    let id = tracer.names.len() as u32;
                    tracer.names.push(name.clone());
                    tracer.ids.insert(name, id);
                    id
                }
            };
            let _ = TEST_ID.try_with(|c| c.set(Some(id)));
            id
        }
    };
    f(&mut tracer, id);
}

fn record_cmp(site: u32, rel: u8, d: Option<i128>, av: Val, bv: Val) {
    with_tracer(|t, test| {
        if let (Some(a), Some(b)) = (av, bv) {
            if t.typed.insert(site) {
                let line = format!("T\t{}\t{}\t{}\t{}\t{}\n", site, a.1, a.2, b.1, b.2);
                let _ = t.out.write_all(line.as_bytes());
            }
        }
        let summary = t.cmp.entry((site, test)).or_default();
        if summary.observe(rel, d) {
            let line = summary.line(site, &t.names[test as usize]);
            let _ = t.out.write_all(line.as_bytes());
        }
    });
}

fn diff(av: Val, bv: Val) -> Option<i128> {
    match (av, bv) {
        (Some(a), Some(b)) => a.0.checked_sub(b.0),
        _ => None,
    }
}

fn finish(site: u32, op: u8, orig: bool, rel: u8, av: Val, bv: Val) -> bool {
    let d = diff(av, bv);
    let st = state();
    if let Some((target, m)) = st.mutant {
        return if target == site { apply(op, m, orig, rel, d) } else { orig };
    }
    if st.tracer.is_some() {
        record_cmp(site, rel, d, av, bv);
    }
    orig
}

/// `a < b`, `a <= b`, `a > b`, `a >= b`.
pub fn ord<A: ?Sized + PartialOrd<B>, B: ?Sized>(
    site: u32,
    op: u8,
    a: &A,
    b: &B,
    av: Val,
    bv: Val,
) -> bool {
    let orig = match op {
        LT => a < b,
        LE => a <= b,
        GT => a > b,
        _ => a >= b,
    };
    let st = state();
    if st.mutant.is_none() && st.tracer.is_none() {
        return orig;
    }
    let rel = match a.partial_cmp(b) {
        Some(Ordering::Less) => REL_LT,
        Some(Ordering::Equal) => REL_EQ,
        Some(Ordering::Greater) => REL_GT,
        None => REL_UN,
    };
    finish(site, op, orig, rel, av, bv)
}

/// `a == b`, `a != b`.
pub fn eq<A: ?Sized + PartialEq<B>, B: ?Sized>(
    site: u32,
    op: u8,
    a: &A,
    b: &B,
    av: Val,
    bv: Val,
) -> bool {
    let orig = if op == EQ { a == b } else { a != b };
    let equal = orig == (op == EQ);
    let rel = match (equal, diff(av, bv)) {
        (true, _) => REL_EQ,
        (false, Some(d)) if d != 0 => rel_of_d(d),
        (false, _) => REL_UN,
    };
    finish(site, op, orig, rel, av, bv)
}

/// Guards a statement: false when the active mutant deletes it.
pub fn live(site: u32) -> bool {
    let st = state();
    if let Some((target, m)) = st.mutant {
        return !(target == site && m == Mutant::Delete);
    }
    if st.tracer.is_some() {
        with_tracer(|t, test| {
            if t.reach.insert((site, test)) {
                let line = format!("R\t{}\t{}\n", site, t.names[test as usize]);
                let _ = t.out.write_all(line.as_bytes());
            }
        });
    }
    true
}
