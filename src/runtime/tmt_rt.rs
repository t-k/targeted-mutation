// Injected by targeted-mutation.
// This file is not part of the original crate. It records what the tests observe at each
// instrumented site (TMT_TRACE=<file>) and switches one mutant on (TMT_MUTANT=<site>:<mutation>).
#![allow(dead_code, unused, missing_docs, clippy::all)]

// `std` is reached through an alias declared in the crate root, so that this module also
// works in a `#![no_std]` crate (whose tests still link std).
use crate::__tmt_std as std;
use std::prelude::v1::*;
use std::{format, thread_local};

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// Numeric value of a comparison operand.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Val {
    None,
    /// `(value, MIN, MAX)` of an integer type.
    Int(i128, i128, i128),
    Float(f64),
}

// Autoref specialisation: `(&W(x)).tmt_val()` resolves to `ValNum` for primitive numbers
// and falls back to `ValAny` for every other type.
pub struct W<'a, T: ?Sized>(pub &'a T);

pub trait ValNum {
    fn tmt_val(&self) -> Val;
}

pub trait ValAny {
    fn tmt_val(&self) -> Val;
}

macro_rules! tmt_ints {
    ($($t:ty)*) => {$(
        impl<'a> ValNum for W<'a, $t> {
            fn tmt_val(&self) -> Val {
                Val::Int(*self.0 as i128, <$t>::MIN as i128, <$t>::MAX as i128)
            }
        }
        impl<'a, 'b> ValNum for W<'a, &'b $t> {
            fn tmt_val(&self) -> Val {
                Val::Int(**self.0 as i128, <$t>::MIN as i128, <$t>::MAX as i128)
            }
        }
    )*};
}
tmt_ints!(i8 i16 i32 i64 isize u8 u16 u32 u64 usize);

macro_rules! tmt_floats {
    ($($t:ty)*) => {$(
        impl<'a> ValNum for W<'a, $t> {
            fn tmt_val(&self) -> Val {
                Val::Float(*self.0 as f64)
            }
        }
        impl<'a, 'b> ValNum for W<'a, &'b $t> {
            fn tmt_val(&self) -> Val {
                Val::Float(**self.0 as f64)
            }
        }
    )*};
}
tmt_floats!(f32 f64);

impl<'a, 'r, T: ?Sized> ValAny for &'r W<'a, T> {
    fn tmt_val(&self) -> Val {
        Val::None
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

pub const ADD: u8 = 0;
pub const SUB: u8 = 1;
pub const MUL: u8 = 2;
pub const DIV: u8 = 3;
pub const REM: u8 = 4;

/// Logic observations: the left operand decided alone, the right operand was true / false.
pub const LOGIC_SHORT: u8 = 1;
pub const LOGIC_RHS_TRUE: u8 = 2;
pub const LOGIC_RHS_FALSE: u8 = 4;

/// Arithmetic observations: the swapped operator gave the same / a different result,
/// or the operands are not primitive numbers.
pub const ARITH_SAME: u8 = 1;
pub const ARITH_DIFFERS: u8 = 2;
pub const ARITH_OPAQUE: u8 = 4;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mutant {
    Const(bool),
    Negate,
    Op(u8),
    Shift(i128),
    /// Shift of a float comparison's threshold.
    ShiftF(f64),
    Delete,
    /// `&&` <-> `||`.
    LogicSwap,
    /// The right operand is evaluated but its value is ignored.
    LogicDropRhs,
    /// The left operand is evaluated but its value is ignored.
    LogicDropLhs,
    /// `+` <-> `-`, `*` <-> `/`, `%` -> `/`.
    ArithSwap,
}

pub fn parse_mutant(spec: &str) -> Option<(u32, Mutant)> {
    let (site, m) = spec.split_once(':')?;
    let site = site.parse().ok()?;
    let m = match m {
        "c0" => Mutant::Const(false),
        "c1" => Mutant::Const(true),
        "neg" => Mutant::Negate,
        "del" => Mutant::Delete,
        "lsw" => Mutant::LogicSwap,
        "ldr" => Mutant::LogicDropRhs,
        "ldl" => Mutant::LogicDropLhs,
        "asw" => Mutant::ArithSwap,
        _ if m.starts_with("op") => Mutant::Op(m[2..].parse().ok().filter(|c| *c <= NE)?),
        _ if m.starts_with("shf") => {
            Mutant::ShiftF(m[3..].parse().ok().filter(|s: &f64| s.is_finite())?)
        }
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

fn rel_of_f(d: f64) -> u8 {
    if d < 0.0 {
        REL_LT
    } else if d == 0.0 {
        REL_EQ
    } else if d > 0.0 {
        REL_GT
    } else {
        REL_UN
    }
}

/// Difference of the operands of a comparison, when they are numbers.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Diff {
    None,
    Int(i128),
    Float(f64),
}

/// Result of the comparison under a mutant. `orig` is what the real operator returned.
pub fn apply(op: u8, m: Mutant, orig: bool, rel: u8, d: Diff) -> bool {
    match (m, d) {
        (Mutant::Const(c), _) => c,
        (Mutant::Negate, _) => !orig,
        (Mutant::Op(op2), _) => eval_rel(op2, rel),
        (Mutant::Shift(s), Diff::Int(d)) => eval_rel(op, rel_of_d(d - s)),
        (Mutant::ShiftF(s), Diff::Float(d)) => eval_rel(op, rel_of_f(d - s)),
        _ => orig,
    }
}

/// Per (site, test) summary. Mirrors `plan::Obs`.
#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub struct Summary {
    pub rels: u8,
    /// Some observation had no integer values / no float values.
    pub unvalued: bool,
    pub unfloat: bool,
    pub max_neg: Option<i128>,
    pub min_pos: Option<i128>,
    pub f_max_neg: Option<f64>,
    pub f_min_pos: Option<f64>,
}

impl Summary {
    /// Folds one observation in; returns whether the summary changed.
    pub fn observe(&mut self, rel: u8, d: Diff) -> bool {
        let before = *self;
        self.rels |= rel;
        match d {
            Diff::None => {
                self.unvalued = true;
                self.unfloat = true;
            }
            Diff::Int(d) => {
                self.unfloat = true;
                if d < 0 {
                    self.max_neg = Some(self.max_neg.map_or(d, |m| m.max(d)));
                } else if d > 0 {
                    self.min_pos = Some(self.min_pos.map_or(d, |p| p.min(d)));
                }
            }
            Diff::Float(d) => {
                self.unvalued = true;
                if d < 0.0 {
                    self.f_max_neg = Some(self.f_max_neg.map_or(d, |m| m.max(d)));
                } else if d > 0.0 {
                    self.f_min_pos = Some(self.f_min_pos.map_or(d, |p| p.min(d)));
                }
            }
        }
        *self != before
    }

    pub fn line(&self, site: u32, test: &str) -> String {
        let int = |v: Option<i128>| v.map_or("-".to_string(), |v| v.to_string());
        let float = |v: Option<f64>| v.map_or("-".to_string(), |v| format!("{:?}", v));
        format!(
            "O\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            site,
            test,
            self.rels,
            self.unvalued as u8,
            int(self.max_neg),
            int(self.min_pos),
            self.unfloat as u8,
            float(self.f_max_neg),
            float(self.f_min_pos)
        )
    }
}

struct Tracer {
    out: std::fs::File,
    names: Vec<String>,
    ids: HashMap<String, u32>,
    cmp: HashMap<(u32, u32), Summary>,
    /// Bits already written per (kind, site, test) for statement, logic and arithmetic sites.
    bits: HashMap<(u8, u32, u32), u8>,
    typed: HashSet<u32>,
}

struct State {
    mutant: Option<(u32, Mutant)>,
    tracer: Option<Mutex<Tracer>>,
}

fn state() -> &'static State {
    static STATE: OnceLock<State> = OnceLock::new();
    STATE.get_or_init(|| {
        let mutant = std::env::var("TMT_MUTANT")
            .ok()
            .and_then(|s| parse_mutant(&s));
        let tracer = std::env::var_os("TMT_TRACE").and_then(|path| {
            let out = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()?;
            Some(Mutex::new(Tracer {
                out,
                names: Vec::new(),
                ids: HashMap::new(),
                cmp: HashMap::new(),
                bits: HashMap::new(),
                typed: HashSet::new(),
            }))
        });
        State { mutant, tracer }
    })
}

fn mutant_at(site: u32) -> Option<Mutant> {
    match state().mutant {
        Some((target, m)) if target == site => Some(m),
        _ => None,
    }
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
    let Some(tracer) = &state().tracer else {
        return;
    };
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

/// Records observation bits of a statement (`R`), logic (`L`) or arithmetic (`A`) site.
fn record_bits(kind: u8, site: u32, bits: u8) {
    if state().mutant.is_some() || state().tracer.is_none() {
        return;
    }
    with_tracer(|t, test| {
        let seen = t.bits.entry((kind, site, test)).or_insert(0);
        if *seen | bits != *seen {
            *seen |= bits;
            let line = format!(
                "{}\t{}\t{}\t{}\n",
                kind as char, site, t.names[test as usize], *seen
            );
            let _ = t.out.write_all(line.as_bytes());
        }
    });
}

fn record_cmp(site: u32, rel: u8, d: Diff, av: Val, bv: Val) {
    with_tracer(|t, test| {
        if let (Val::Int(_, amin, amax), Val::Int(_, bmin, bmax)) = (av, bv) {
            if t.typed.insert(site) {
                let line = format!("T\t{}\t{}\t{}\t{}\t{}\n", site, amin, amax, bmin, bmax);
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

/// `equal` is what the real operator said about equality; it wins over float rounding
/// (`inf - inf` is NaN although the operands are equal).
pub fn diff(av: Val, bv: Val, equal: bool) -> Diff {
    match (av, bv) {
        (Val::Int(a, ..), Val::Int(b, ..)) => a.checked_sub(b).map_or(Diff::None, Diff::Int),
        (Val::Float(a), Val::Float(b)) => {
            let d = if equal { 0.0 } else { a - b };
            if d.is_finite() && (equal || d != 0.0) {
                Diff::Float(d)
            } else {
                Diff::None
            }
        }
        _ => Diff::None,
    }
}

fn finish(site: u32, op: u8, orig: bool, rel: u8, av: Val, bv: Val) -> bool {
    let d = diff(av, bv, rel == REL_EQ);
    let st = state();
    if let Some((target, m)) = st.mutant {
        return if target == site {
            apply(op, m, orig, rel, d)
        } else {
            orig
        };
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
    let rel = match (equal, diff(av, bv, equal)) {
        (true, _) => REL_EQ,
        (false, Diff::Int(d)) if d != 0 => rel_of_d(d),
        (false, _) => REL_UN,
    };
    finish(site, op, orig, rel, av, bv)
}

/// Guards a statement: false when the active mutant deletes it.
pub fn live(site: u32) -> bool {
    if mutant_at(site) == Some(Mutant::Delete) {
        return false;
    }
    record_bits(b'R', site, 1);
    true
}

/// What the left operand of `a && b` / `a || b` decides: 0 or 1 when the result is
/// already false / true, 2 when the right operand has to be evaluated.
pub fn lhs_code(and: bool, a: bool, m: Option<Mutant>) -> u8 {
    let code = |and: bool| match (and, a) {
        (true, false) => 0,
        (false, true) => 1,
        _ => 2,
    };
    match m {
        Some(Mutant::LogicSwap) => code(!and),
        Some(Mutant::LogicDropLhs) => 2,
        _ => code(and),
    }
}

/// Value of the right operand as the expression sees it.
pub fn rhs_value(and: bool, b: bool, m: Option<Mutant>) -> bool {
    match m {
        // `a && true` / `a || false`: the right operand never decides.
        Some(Mutant::LogicDropRhs) => and,
        _ => b,
    }
}

/// Left operand of `a && b` / `a || b`.
pub fn lhs(site: u32, and: bool, a: bool) -> u8 {
    let m = mutant_at(site);
    let c = lhs_code(and, a, m);
    if m.is_none() && c != 2 {
        record_bits(b'L', site, LOGIC_SHORT);
    }
    c
}

/// Right operand of `a && b` / `a || b`, evaluated because `lhs` returned 2.
pub fn rhs(site: u32, and: bool, b: bool) -> bool {
    let m = mutant_at(site);
    if m.is_none() {
        record_bits(b'L', site, if b { LOGIC_RHS_TRUE } else { LOGIC_RHS_FALSE });
    }
    rhs_value(and, b, m)
}

/// Operands of an arithmetic operator, taken out once by the probe method.
pub struct Ar<L, R>(Cell<Option<(L, R)>>);

impl<L, R> Ar<L, R> {
    pub fn new(l: L, r: R) -> Self {
        Ar(Cell::new(Some((l, r))))
    }

    fn take(&self) -> (L, R) {
        self.0.take().expect("operands are taken once")
    }
}

/// Probe for primitive numbers: can compute the swapped operator.
pub trait ArithPrim {
    type O;
    fn tmt_add(&self, site: u32) -> Self::O;
    fn tmt_sub(&self, site: u32) -> Self::O;
    fn tmt_mul(&self, site: u32) -> Self::O;
    fn tmt_div(&self, site: u32) -> Self::O;
    fn tmt_rem(&self, site: u32) -> Self::O;
}

/// The operator the `ArithSwap` mutant uses.
pub fn arith_swapped(op: u8) -> u8 {
    match op {
        ADD => SUB,
        SUB => ADD,
        MUL => DIV,
        DIV => MUL,
        _ => DIV,
    }
}

pub trait ArithNum: Copy {
    fn tmt_real(op: u8, a: Self, b: Self) -> Self;
    /// The result as bits, or `None` where the real operator would overflow or divide by zero.
    fn tmt_safe(op: u8, a: Self, b: Self) -> Option<u64>;

    /// Whether the swapped operator gives the same result on these operands.
    fn tmt_same(op: u8, a: Self, b: Self) -> bool {
        Self::tmt_safe(op, a, b) == Self::tmt_safe(arith_swapped(op), a, b)
    }

    fn tmt_run(site: u32, op: u8, a: Self, b: Self) -> Self {
        if mutant_at(site) == Some(Mutant::ArithSwap) {
            return Self::tmt_real(arith_swapped(op), a, b);
        }
        let bits = if Self::tmt_same(op, a, b) { ARITH_SAME } else { ARITH_DIFFERS };
        record_bits(b'A', site, bits);
        Self::tmt_real(op, a, b)
    }
}

macro_rules! tmt_arith_prim {
    ($($t:ty)*) => {$(
        impl ArithPrim for Ar<$t, $t> {
            type O = $t;
            fn tmt_add(&self, site: u32) -> $t { let (a, b) = self.take(); <$t>::tmt_run(site, ADD, a, b) }
            fn tmt_sub(&self, site: u32) -> $t { let (a, b) = self.take(); <$t>::tmt_run(site, SUB, a, b) }
            fn tmt_mul(&self, site: u32) -> $t { let (a, b) = self.take(); <$t>::tmt_run(site, MUL, a, b) }
            fn tmt_div(&self, site: u32) -> $t { let (a, b) = self.take(); <$t>::tmt_run(site, DIV, a, b) }
            fn tmt_rem(&self, site: u32) -> $t { let (a, b) = self.take(); <$t>::tmt_run(site, REM, a, b) }
        }
    )*};
}

macro_rules! tmt_arith_ints {
    ($($t:ty)*) => {$(
        impl ArithNum for $t {
            fn tmt_real(op: u8, a: $t, b: $t) -> $t {
                match op { ADD => a + b, SUB => a - b, MUL => a * b, DIV => a / b, _ => a % b }
            }
            fn tmt_safe(op: u8, a: $t, b: $t) -> Option<u64> {
                match op {
                    ADD => a.checked_add(b),
                    SUB => a.checked_sub(b),
                    MUL => a.checked_mul(b),
                    DIV => a.checked_div(b),
                    _ => a.checked_rem(b),
                }
                .map(|v| v as u64)
            }
        }
    )*};
}

macro_rules! tmt_arith_floats {
    ($($t:ty)*) => {$(
        impl ArithNum for $t {
            fn tmt_real(op: u8, a: $t, b: $t) -> $t {
                match op { ADD => a + b, SUB => a - b, MUL => a * b, DIV => a / b, _ => a % b }
            }
            fn tmt_safe(op: u8, a: $t, b: $t) -> Option<u64> {
                Some((Self::tmt_real(op, a, b) as f64).to_bits())
            }
        }
    )*};
}

tmt_arith_prim!(i8 i16 i32 i64 isize u8 u16 u32 u64 usize f32 f64);
tmt_arith_ints!(i8 i16 i32 i64 isize u8 u16 u32 u64 usize);
tmt_arith_floats!(f32 f64);

// Fallbacks for every other pair of operand types: the original operator, untouched.
macro_rules! tmt_arith_any {
    ($($name:ident $method:ident $bound:ident $call:ident)*) => {$(
        pub trait $name {
            type O;
            fn $method(&self, site: u32) -> Self::O;
        }
        impl<'r, L: std::ops::$bound<R>, R> $name for &'r Ar<L, R> {
            type O = L::Output;
            fn $method(&self, site: u32) -> L::Output {
                let (a, b) = self.take();
                record_bits(b'A', site, ARITH_OPAQUE);
                std::ops::$bound::$call(a, b)
            }
        }
    )*};
}
tmt_arith_any!(
    AnyAdd tmt_add Add add
    AnySub tmt_sub Sub sub
    AnyMul tmt_mul Mul mul
    AnyDiv tmt_div Div div
    AnyRem tmt_rem Rem rem
);
