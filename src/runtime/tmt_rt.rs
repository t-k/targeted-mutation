// Injected by targeted-mutation.
// This file is not part of the original crate. It records what the tests observe at each
// instrumented site (TMT_TRACE=<file>) and switches one mutant on (TMT_MUTANT=<site>:<mutation>).
#![allow(dead_code, unused, missing_docs, clippy::all)]

// `std` is reached through an alias declared in the crate root, so that this module also
// works in a `#![no_std]` crate (whose tests still link std).
use crate::__tmt_std as std;
use std::prelude::v1::*;
use std::{format, thread_local};

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering as Atomic};
use std::sync::{Arc, Mutex, OnceLock};

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
            #[inline(always)]
            fn tmt_val(&self) -> Val {
                Val::Int(*self.0 as i128, <$t>::MIN as i128, <$t>::MAX as i128)
            }
        }
        impl<'a, 'b> ValNum for W<'a, &'b $t> {
            #[inline(always)]
            fn tmt_val(&self) -> Val {
                Val::Int(**self.0 as i128, <$t>::MIN as i128, <$t>::MAX as i128)
            }
        }
    )*};
}
tmt_ints!(i8 i16 i32 i64 isize u8 u16 u32 u64 usize);

/// Order-preserving bijection from Unicode scalar values (`char`) onto the
/// contiguous range `0..=0x10_D7FF`, closing the surrogate gap `0xD800..=0xDFFF`.
#[inline(always)]
fn char_ord(c: char) -> i128 {
    let u = c as u32;
    if u >= 0xE000 {
        (u - 0x800) as i128
    } else {
        u as i128
    }
}

const CHAR_ORD_MAX: i128 = 0x10_D7FF;

impl<'a> ValNum for W<'a, char> {
    #[inline(always)]
    fn tmt_val(&self) -> Val {
        Val::Int(char_ord(*self.0), 0, CHAR_ORD_MAX)
    }
}
impl<'a, 'b> ValNum for W<'a, &'b char> {
    #[inline(always)]
    fn tmt_val(&self) -> Val {
        Val::Int(char_ord(**self.0), 0, CHAR_ORD_MAX)
    }
}

macro_rules! tmt_floats {
    ($($t:ty)*) => {$(
        impl<'a> ValNum for W<'a, $t> {
            #[inline(always)]
            fn tmt_val(&self) -> Val {
                Val::Float(*self.0 as f64)
            }
        }
        impl<'a, 'b> ValNum for W<'a, &'b $t> {
            #[inline(always)]
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

/// Autoref specialisation for observing the source type bounds of a cast or
/// primitive `From::from` operand inside `|` / `^`.
pub trait CastPrim {
    fn tmt_cast_bounds(&self) -> Option<(i128, i128)>;
}

pub trait CastAny {
    fn tmt_cast_bounds(&self) -> Option<(i128, i128)>;
}

macro_rules! tmt_cast_ints {
    ($($t:ty)*) => {$(
        impl<'a> CastPrim for W<'a, $t> {
            #[inline(always)]
            fn tmt_cast_bounds(&self) -> Option<(i128, i128)> {
                Some((<$t>::MIN as i128, <$t>::MAX as i128))
            }
        }
    )*};
}
tmt_cast_ints!(i8 i16 i32 i64 i128 isize u8 u16 u32 u64 usize);

impl<'a> CastPrim for W<'a, bool> {
    #[inline(always)]
    fn tmt_cast_bounds(&self) -> Option<(i128, i128)> {
        Some((0, 1))
    }
}

impl<'a> CastPrim for W<'a, char> {
    #[inline(always)]
    fn tmt_cast_bounds(&self) -> Option<(i128, i128)> {
        Some((0, 0x1f_ffff))
    }
}

impl<'a, 'r, T: ?Sized> CastAny for &'r W<'a, T> {
    #[inline(always)]
    fn tmt_cast_bounds(&self) -> Option<(i128, i128)> {
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

pub const ADD: u8 = 0;
pub const SUB: u8 = 1;
pub const MUL: u8 = 2;
pub const DIV: u8 = 3;
pub const REM: u8 = 4;
pub const BITAND: u8 = 5;
pub const BITOR: u8 = 6;
pub const BITXOR: u8 = 7;
pub const SHL: u8 = 8;
pub const SHR: u8 = 9;

/// Observations at the exit of a `-> bool` function.
pub const RET_TRUE: u8 = 1;
pub const RET_FALSE: u8 = 2;

/// The exit status of a test run that cannot go on as the mutant it was started with.
/// The driver counts such a mutant as not evaluated: neither found out nor passed.
pub const CANNOT_EVALUATE: i32 = 86;

/// The run may be that of a program a test started, whose exit status the test takes for
/// a failure of its own: the sign is a file the driver made and named (`TMT_UNABLE`),
/// which is taken away here. Taking a file away needs no room on the disk, so a full
/// one cannot turn a run that was given up into a mutant found out.
fn cannot_evaluate() -> ! {
    if let Some(path) = std::env::var_os("TMT_UNABLE") {
        let _ = std::fs::remove_file(path);
    }
    std::process::exit(CANNOT_EVALUATE)
}

/// With the reach bit of a range `a..b`: `b` was the largest value of its type, where
/// `a..=b` cannot be written as a range of the same kind.
pub const RANGE_AT_TOP: u8 = 2;

/// Logic observations: the left operand decided alone, the right operand was true / false.
pub const LOGIC_SHORT: u8 = 1;
pub const LOGIC_RHS_TRUE: u8 = 2;
pub const LOGIC_RHS_FALSE: u8 = 4;
/// The right operand was started. It may not give a value: it can panic or leave the
/// function (`a && check()?`), and the expression was executed all the same.
pub const LOGIC_RHS_STARTED: u8 = 8;

/// Arithmetic observations: the swapped operator gave the same / a different result,
/// or the operands are not primitive numbers.
pub const ARITH_SAME: u8 = 1;
pub const ARITH_DIFFERS: u8 = 2;
pub const ARITH_OPAQUE: u8 = 4;
/// The same two for the second operator a mutant can use instead.
pub const ARITH_SAME2: u8 = 8;
pub const ARITH_DIFFERS2: u8 = 16;

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
    /// `+` <-> `-`, `*` <-> `/`, `%` -> `/`, `&` <-> `|`, `^` -> `&`, `<<` <-> `>>`.
    ArithSwap,
    /// `+` -> `*`, `-` -> `/`, `*` -> `+`, `/` -> `%`, `%` -> `+`, `&` -> `^`, `|` -> `^`, `^` -> `|`.
    ArithSwap2,
    /// `!x` becomes `x`.
    DropNot,
    /// `a..b` becomes `a..=b` and the other way round.
    RangeEnd,
    /// `a.max(b)` becomes `a.min(b)` and the other way round.
    MinMax,
    /// `v.first()` becomes `v.last()` and the other way round.
    OtherEnd,
    /// `s.starts_with(p)` / `s.ends_with(p)` becomes `s.contains(p)`.
    StrContains,
    /// `s.replace(a, b)` <-> `s.replacen(a, b, 1)`.
    StrReplaceMultiplicity,
    /// `x.clamp(lo, hi)` becomes `x.min(hi)`.
    ClampDropLower,
    /// `x.clamp(lo, hi)` becomes `x.max(lo)`.
    ClampDropUpper,
    /// First alternative rounding method.
    RoundAlt1,
    /// Second alternative rounding method.
    RoundAlt2,
    /// `iter.all(f)` <-> `iter.any(f)`.
    QuantifierSwap,
    /// `a.saturating_{add,sub,mul}(b)` <-> `a.wrapping_{add,sub,mul}(b)`.
    SaturatingSwap,
    /// First alternative trim method (`trim` -> `trim_start`, `trim_start`/`trim_end` -> `trim`).
    TrimAlt1,
    /// Second alternative trim method (`trim` -> `trim_end`).
    TrimAlt2,
    /// `a.eq_ignore_ascii_case(b)` -> `a == b`.
    StrExactEq,
    /// `x.abs()` -> `x`.
    DropAbs,
    /// `iter.take(n)` -> `iter.take(usize::MAX)` / `iter.skip(n)` -> `iter.skip(0)`.
    IterLimitDrop,
    /// `x.to_be_bytes()` <-> `x.to_le_bytes()`, `x.to_be()` <-> `x.to_le()`.
    EndianSwap,
    /// `x.leading_zeros()` <-> `x.trailing_zeros()`, `leading_ones` <-> `trailing_ones`.
    BitCountSwap,
    /// `break` <-> `continue`.
    LoopCtrlSwap,
    /// `[lit1, lit2, ...].contains(&x)`: drop element at index `0..8`.
    DropLiteralElem(u8),
    /// `iter.position(f)` <-> `iter.rposition(f)`.
    PositionSwap,
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
        "as2" => Mutant::ArithSwap2,
        "unn" => Mutant::DropNot,
        "rng" => Mutant::RangeEnd,
        "mmx" => Mutant::MinMax,
        "end" => Mutant::OtherEnd,
        "stc" => Mutant::StrContains,
        "srp" => Mutant::StrReplaceMultiplicity,
        "cll" => Mutant::ClampDropLower,
        "clu" => Mutant::ClampDropUpper,
        "rd1" => Mutant::RoundAlt1,
        "rd2" => Mutant::RoundAlt2,
        "qsw" => Mutant::QuantifierSwap,
        "sat" => Mutant::SaturatingSwap,
        "tr1" => Mutant::TrimAlt1,
        "tr2" => Mutant::TrimAlt2,
        "seq" => Mutant::StrExactEq,
        "abs" => Mutant::DropAbs,
        "ilp" => Mutant::IterLimitDrop,
        "edn" => Mutant::EndianSwap,
        "btc" => Mutant::BitCountSwap,
        "lpc" => Mutant::LoopCtrlSwap,
        "psw" => Mutant::PositionSwap,
        _ if m.starts_with("dle") => {
            Mutant::DropLiteralElem(m[3..].parse().ok().filter(|idx| *idx < 8)?)
        }
        _ if m.starts_with("op") => Mutant::Op(m[2..].parse().ok().filter(|c| *c <= NE)?),
        _ if m.starts_with("shf") => {
            Mutant::ShiftF(m[3..].parse().ok().filter(|s: &f64| s.is_finite())?)
        }
        _ if m.starts_with("sh") => Mutant::Shift(m[2..].parse().ok()?),
        _ => return None,
    };
    Some((site, m))
}

#[inline(always)]
fn eval_rel(op: u8, rel: u8) -> bool {
    // 4-bit truth table matching `formal/lean/Planner.lean` (`CmpOp.truthMask`).
    const TRUTH: [u8; 6] = [0b0001, 0b0011, 0b0100, 0b0110, 0b0010, 0b1101];
    let idx = if (op as usize) < 6 { op as usize } else { 5 };
    TRUTH[idx] & rel != 0
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

/// What an observation added to a summary.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum News {
    Nothing,
    /// Only a distance that was known came closer to zero. A loop that counts towards
    /// its bound does this in every round.
    Closer,
    Other,
}

impl Summary {
    /// Folds one observation in; returns whether the summary changed.
    pub fn observe(&mut self, rel: u8, d: Diff) -> bool {
        self.news(rel, d) != News::Nothing
    }

    /// Folds one observation in and says what kind of change that was.
    pub fn news(&mut self, rel: u8, d: Diff) -> News {
        let new_relation = self.rels & rel != rel;
        self.rels |= rel;
        let (new_kind, distance) = match d {
            Diff::None => {
                let new_kind = !self.unvalued || !self.unfloat;
                self.unvalued = true;
                self.unfloat = true;
                (new_kind, News::Nothing)
            }
            Diff::Int(d) => {
                let new_kind = !self.unfloat;
                self.unfloat = true;
                let distance = if d < 0 {
                    update_distance(&mut self.max_neg, d, |old| d > old)
                } else if d > 0 {
                    update_distance(&mut self.min_pos, d, |old| d < old)
                } else {
                    News::Nothing
                };
                (new_kind, distance)
            }
            Diff::Float(d) => {
                let new_kind = !self.unvalued;
                self.unvalued = true;
                let distance = if d < 0.0 {
                    update_distance(&mut self.f_max_neg, d, |old| d > old)
                } else if d > 0.0 {
                    update_distance(&mut self.f_min_pos, d, |old| d < old)
                } else {
                    News::Nothing
                };
                (new_kind, distance)
            }
        };
        if new_relation || new_kind {
            News::Other
        } else {
            distance
        }
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

fn update_distance<T: Copy>(
    distance: &mut Option<T>,
    value: T,
    closer: impl FnOnce(T) -> bool,
) -> News {
    match *distance {
        None => {
            *distance = Some(value);
            News::Other
        }
        Some(old) if closer(old) => {
            *distance = Some(value);
            News::Closer
        }
        Some(_) => News::Nothing,
    }
}

/// Summaries of one thread that are closer to zero than the trace says: written when
/// the thread ends, when any other does, and by the flusher. A thread shares its backlog
/// with the tracer, so that it is not lost with a thread that never ends.
#[derive(Default)]
pub struct Backlog {
    latest: Vec<Option<Summary>>,
    stale: Vec<u32>,
}

impl Backlog {
    /// Keeps `summary` as the latest of `site`, to be written later.
    pub fn keep(&mut self, site: u32, summary: Summary) {
        let slot = site as usize;
        if self.latest.len() <= slot {
            self.latest.resize(slot + 1, None);
        }
        if self.latest[slot].replace(summary).is_none() {
            self.stale.push(site);
        }
    }

    /// The site is written by other means: nothing of it is left to write.
    pub fn forget(&mut self, site: u32) {
        if let Some(slot) = self.latest.get_mut(site as usize) {
            if slot.take().is_some() {
                self.stale.retain(|s| *s != site);
            }
        }
    }

    /// Takes out everything that is to be written.
    pub fn drain(&mut self) -> Vec<(u32, Summary)> {
        let stale = std::mem::take(&mut self.stale);
        stale
            .into_iter()
            .filter_map(|site| Some((site, self.latest[site as usize].take()?)))
            .collect()
    }
}

struct Shelf {
    test: u32,
    backlog: Arc<Mutex<Backlog>>,
}

struct Tracer {
    out: std::fs::File,
    names: Vec<String>,
    ids: HashMap<String, u32>,
    shelves: Vec<Shelf>,
    /// Bits already written per (kind, site, test) for statement, logic, arithmetic and set sites.
    bits: HashMap<(u8, u32, u32), u16>,
    typed: HashSet<u32>,
    cast_typed: HashSet<(u32, u32, Option<(i128, i128)>)>,
}

struct State {
    mutant: Option<(u32, Mutant)>,
    tracer: Option<Mutex<Tracer>>,
    /// The process that read the settings and opened the trace.
    home: u32,
}

/// The tracer, for the process that opened it.
///
/// A child made by `fork` has every lock of this module as it was at that instant,
/// perhaps held by a thread the child does not have: it would wait for ever, and so would
/// the parent that waits for its `exec`. The code a child runs before `exec` (a
/// `pre_exec` hook) therefore records nothing and takes no lock here.
fn tracer() -> Option<&'static Mutex<Tracer>> {
    let state = state();
    match &state.tracer {
        Some(tracer) if std::process::id() == state.home => Some(tracer),
        _ => None,
    }
}

/// What the probes have to do in this process, kept where a probe can read it without a
/// call: nothing has looked yet, nothing at all, everything (a trace is recorded), or
/// only the site in `TARGET` (a mutant is on and no trace is recorded).
static MODE: AtomicU8 = AtomicU8::new(MODE_UNKNOWN);
static TARGET: AtomicU32 = AtomicU32::new(0);
const MODE_UNKNOWN: u8 = 0;
const MODE_IDLE: u8 = 1;
const MODE_ALL: u8 = 2;
const MODE_ONE: u8 = 3;

/// Whether the probe at `site` has nothing to do but what the code it stands for does.
/// Most probes are asked most of the time: a run with a mutant concerns one site.
#[inline(always)]
pub fn idle(site: u32) -> bool {
    match MODE.load(Atomic::Acquire) {
        MODE_IDLE => true,
        MODE_ONE => TARGET.load(Atomic::Relaxed) != site,
        MODE_ALL => false,
        _ => first_look(site),
    }
}

#[cold]
#[inline(never)]
fn first_look(site: u32) -> bool {
    state(); // sets `MODE`
    idle(site)
}

fn mode_of(state: &State) -> u8 {
    match (&state.mutant, &state.tracer) {
        (_, Some(_)) => MODE_ALL,
        (Some(_), None) => MODE_ONE,
        (None, None) => MODE_IDLE,
    }
}

fn state() -> &'static State {
    static STATE: OnceLock<State> = OnceLock::new();
    STATE.get_or_init(|| {
        let mutant = std::env::var("TMT_MUTANT")
            .ok()
            .and_then(|s| parse_mutant(&s));
        let tracer = std::env::var_os("TMT_TRACE").map(|path| {
            let out = match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                Ok(out) => out,
                Err(err) => trace_failed(&err),
            };
            Mutex::new(Tracer {
                out,
                names: Vec::new(),
                ids: HashMap::new(),
                shelves: Vec::new(),
                bits: HashMap::new(),
                typed: HashSet::new(),
                cast_typed: HashSet::new(),
            })
        });
        let state = State {
            mutant,
            tracer,
            home: std::process::id(),
        };
        if let Some((site, _)) = state.mutant {
            TARGET.store(site, Atomic::Relaxed);
        }
        MODE.store(mode_of(&state), Atomic::Release);
        if state.tracer.is_some() {
            // For the threads that never end (a pool that lives as long as the process).
            // Without this thread their backlog is still written whenever a thread ends.
            let _ = std::thread::Builder::new()
                .name("tmt-flush".to_string())
                .spawn(|| {
                    loop {
                        std::thread::sleep(FLUSH_EVERY);
                        flush_backlogs(None);
                    }
                });
        }
        state
    })
}

/// An incomplete trace would be read as "the tests never did this". Stop the test run
/// instead, so that the analysis fails rather than concluding something false.
fn trace_failed(err: &std::io::Error) -> ! {
    std::eprintln!("targeted-mutation: cannot write the trace: {err}");
    std::process::abort()
}

fn write_line(out: &mut std::fs::File, line: &str) {
    if let Err(err) = out.write_all(line.as_bytes()) {
        trace_failed(&err);
    }
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
    if let Ok(name) = std::env::var("TMT_TEST") {
        if !name.is_empty() && !name.contains(['\t', '\n', '\r']) {
            return name;
        }
    }
    match std::thread::current().name() {
        Some(name) if name != "main" && !name.contains('\t') && !name.contains('\n') => {
            name.to_string()
        }
        _ => "?".to_string(),
    }
}

thread_local! {
    static TEST_ID: Cell<Option<u32>> = const { Cell::new(None) };
    // What this thread has already handed to the tracer. A test runs on a thread of
    // its own, so a probe in a loop takes the tracer's lock once and then only when it
    // sees something new.
    static SEEN_BITS: RefCell<Vec<u16>> = const { RefCell::new(Vec::new()) };
    static SEEN_CMP: RefCell<Vec<Option<Summary>>> = const { RefCell::new(Vec::new()) };
    static SEEN_CAST: RefCell<HashSet<(u32, u32, Option<(i128, i128)>)>> = RefCell::new(HashSet::new());
}

#[inline(always)]
pub fn record_cast(site: u32, slot: u32, bounds: Option<(i128, i128)>) {
    match MODE.load(Atomic::Acquire) {
        MODE_IDLE | MODE_ONE => return,
        MODE_ALL => {}
        _ => {
            state();
            if MODE.load(Atomic::Acquire) != MODE_ALL {
                return;
            }
        }
    }
    record_cast_busy(site, slot, bounds);
}

#[inline(never)]
fn record_cast_busy(site: u32, slot: u32, bounds: Option<(i128, i128)>) {
    let key = (site, slot, bounds);
    let fresh = SEEN_CAST
        .try_with(|seen| seen.borrow_mut().insert(key))
        .unwrap_or(true);
    if !fresh {
        return;
    }
    with_tracer(|t, _| {
        if t.cast_typed.insert(key) {
            let line = match bounds {
                Some((min, max)) => format!("C\t{site}\t{slot}\t{min}\t{max}\n"),
                None => format!("C\t{site}\t{slot}\t-\t-\n"),
            };
            write_line(&mut t.out, &line);
        }
    });
}

fn bits_slot(kind: u8, site: u32) -> usize {
    let kind = match kind {
        b'R' => 0,
        b'L' => 1,
        b'A' => 2,
        b'B' => 3,
        _ => 4,
    };
    site as usize * 5 + kind
}

/// Whether this thread has recorded all of `bits` at the site before.
pub fn bits_known(kind: u8, site: u32, bits: u8) -> bool {
    bits_known_u16(kind, site, u16::from(bits))
}

fn bits_known_u16(kind: u8, site: u32, bits: u16) -> bool {
    let slot = bits_slot(kind, site);
    SEEN_BITS
        .try_with(|seen| {
            seen.borrow()
                .get(slot)
                .is_some_and(|had| had & bits == bits)
        })
        .unwrap_or(false)
}

/// Notes `bits` as recorded by this thread; returns whether that was news.
pub fn bits_are_news(kind: u8, site: u32, bits: u8) -> bool {
    bits_are_news_u16(kind, site, u16::from(bits))
}

fn bits_are_news_u16(kind: u8, site: u32, bits: u16) -> bool {
    let slot = bits_slot(kind, site);
    SEEN_BITS
        .try_with(|seen| {
            let mut seen = seen.borrow_mut();
            if seen.len() <= slot {
                seen.resize(slot + 1, 0);
            }
            let had = seen[slot];
            seen[slot] |= bits;
            had | bits != had
        })
        .unwrap_or(true)
}

const FLUSH_EVERY: std::time::Duration = std::time::Duration::from_millis(20);

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Writes what the threads have kept back; `leaving` is the backlog of a thread that ends.
/// Locks are taken in one order everywhere: the tracer, then a backlog.
fn flush_backlogs(leaving: Option<&Arc<Mutex<Backlog>>>) {
    let Some(tracer) = tracer() else {
        return;
    };
    let mut tracer = locked(tracer);
    let t = &mut *tracer;
    let mut batch = String::new();
    for shelf in &t.shelves {
        let name = &t.names[shelf.test as usize];
        for (site, summary) in locked(&shelf.backlog).drain() {
            batch.push_str(&summary.line(site, name));
        }
    }
    if !batch.is_empty() {
        write_line(&mut t.out, &batch);
    }
    if let Some(leaving) = leaving {
        t.shelves
            .retain(|shelf| !Arc::ptr_eq(&shelf.backlog, leaving));
    }
}

/// The backlog of this thread. Its end writes every backlog: what a test had other
/// threads do is in the trace when the test is over.
struct Keeper(Option<Arc<Mutex<Backlog>>>);

impl Keeper {
    fn new() -> Keeper {
        // Only a thread with an end that is waited for keeps anything back. The main
        // thread has none (and a test may run on it, with `--test-threads=1`); a thread
        // without a name is not one of the harness and may be left behind at the exit.
        // Those write at once.
        if matches!(std::thread::current().name(), None | Some("main")) {
            return Keeper(None);
        }
        let backlog = Arc::new(Mutex::new(Backlog::default()));
        let shelved = backlog.clone();
        let mut registered = false;
        with_tracer(|t, test| {
            t.shelves.push(Shelf {
                test,
                backlog: shelved,
            });
            registered = true;
        });
        Keeper(registered.then_some(backlog))
    }
}

impl Drop for Keeper {
    fn drop(&mut self) {
        if let Some(backlog) = &self.0 {
            flush_backlogs(Some(backlog));
        }
    }
}

thread_local! {
    static KEEPER: Keeper = Keeper::new();
}

fn with_tracer(f: impl FnOnce(&mut Tracer, u32)) {
    let Some(tracer) = tracer() else {
        return;
    };
    let mut tracer = locked(tracer);
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

/// Records observation bits of a statement (`R`), logic (`L`), arithmetic (`A`), or return (`B`) site.
fn record_bits(kind: u8, site: u32, bits: u8) {
    record_bits_u16(kind, site, u16::from(bits));
}

/// Records up to 16 observation bits of a site (e.g. `S` literal-set sites).
fn record_bits_u16(kind: u8, site: u32, bits: u16) {
    if state().tracer.is_none() || !bits_are_news_u16(kind, site, bits) || tracer().is_none() {
        return;
    }
    // A thread that records has a keeper, so that its end writes every backlog.
    let _ = KEEPER.try_with(|_| ());
    with_tracer(|t, test| {
        let seen = t.bits.entry((kind, site, test)).or_insert(0);
        if *seen | bits != *seen {
            *seen |= bits;
            let line = format!(
                "{}\t{}\t{}\t{}\n",
                kind as char, site, t.names[test as usize], *seen
            );
            write_line(&mut t.out, &line);
        }
    });
}

fn record_cmp(site: u32, rel: u8, d: Diff, av: Val, bv: Val) {
    // The summary this thread has built so far says whether the observation is news.
    let seen = SEEN_CMP.try_with(|seen| {
        let mut seen = seen.borrow_mut();
        let slot = site as usize;
        if seen.len() <= slot {
            seen.resize(slot + 1, None);
        }
        let summary = seen[slot].get_or_insert_with(Summary::default);
        (summary.news(rel, d), *summary)
    });
    let summary = match seen {
        Ok((News::Nothing, _)) => return,
        // Before any lock, that of this thread's backlog included.
        _ if tracer().is_none() => return,
        Ok((News::Closer, summary)) => {
            // Not worth a line each time: kept for later where there is a later.
            let kept = KEEPER.try_with(|keeper| match &keeper.0 {
                Some(backlog) => {
                    locked(backlog).keep(site, summary);
                    true
                }
                None => false,
            });
            if kept == Ok(true) {
                return;
            }
            summary
        }
        Ok((News::Other, summary)) => {
            let _ = KEEPER.try_with(|keeper| {
                if let Some(backlog) = &keeper.0 {
                    locked(backlog).forget(site);
                }
            });
            summary
        }
        // The thread is ending and its memory is gone: this one observation.
        Err(_) => {
            let mut summary = Summary::default();
            summary.observe(rel, d);
            summary
        }
    };
    with_tracer(|t, test| {
        if let (Val::Int(_, amin, amax), Val::Int(_, bmin, bmax)) = (av, bv) {
            if t.typed.insert(site) {
                let line = format!("T\t{}\t{}\t{}\t{}\t{}\n", site, amin, amax, bmin, bmax);
                write_line(&mut t.out, &line);
            }
        }
        let line = summary.line(site, &t.names[test as usize]);
        write_line(&mut t.out, &line);
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

fn finish(site: u32, op: u8, orig: bool, rel: u8, av: Val, bv: Val, d: Diff) -> bool {
    let st = state();
    // The mutated site does not record while its mutant is active. Every other site
    // does, so that a trace taken with a mutant shows what the mutant changes elsewhere.
    if let Some(m) = mutant_at(site) {
        return apply(op, m, orig, rel, d);
    }
    if st.tracer.is_some() {
        record_cmp(site, rel, d, av, bv);
    }
    orig
}

/// Fast-path ordering comparison when the probe is idle (no `Val` conversion needed).
#[inline(always)]
pub fn fast_ord<A: ?Sized + PartialOrd<B>, B: ?Sized>(op: u8, a: &A, b: &B) -> bool {
    match op {
        LT => a < b,
        LE => a <= b,
        GT => a > b,
        _ => a >= b,
    }
}

/// Fast-path equality comparison when the probe is idle (no `Val` conversion needed).
#[inline(always)]
pub fn fast_eq<A: ?Sized + PartialEq<B>, B: ?Sized>(op: u8, a: &A, b: &B) -> bool {
    if op == EQ { a == b } else { a != b }
}

/// `a < b`, `a <= b`, `a > b`, `a >= b`.
///
/// While tracing or running a mutant the operands are compared exactly once, through
/// `partial_cmp`, and the operator's result is derived from that. This is what the
/// operator itself does unless a type overrides `lt`/`le`/`gt`/`ge` inconsistently with
/// `partial_cmp`, which the `PartialOrd` contract forbids.
#[inline(always)]
pub fn ord<A: ?Sized + PartialOrd<B>, B: ?Sized>(
    site: u32,
    op: u8,
    a: &A,
    b: &B,
    av: Val,
    bv: Val,
) -> bool {
    if idle(site) {
        return fast_ord(op, a, b);
    }
    ord_busy(site, op, a, b, av, bv)
}

#[inline(never)]
pub fn ord_busy<A: ?Sized + PartialOrd<B>, B: ?Sized>(
    site: u32,
    op: u8,
    a: &A,
    b: &B,
    av: Val,
    bv: Val,
) -> bool {
    let rel = match a.partial_cmp(b) {
        Some(Ordering::Less) => REL_LT,
        Some(Ordering::Equal) => REL_EQ,
        Some(Ordering::Greater) => REL_GT,
        None => REL_UN,
    };
    let d = diff(av, bv, rel == REL_EQ);
    finish(site, op, eval_rel(op, rel), rel, av, bv, d)
}

/// `a == b`, `a != b`.
#[inline(always)]
pub fn eq<A: ?Sized + PartialEq<B>, B: ?Sized>(
    site: u32,
    op: u8,
    a: &A,
    b: &B,
    av: Val,
    bv: Val,
) -> bool {
    let orig = fast_eq(op, a, b);
    if idle(site) {
        return orig;
    }
    eq_busy(site, op, orig, av, bv)
}

#[inline(never)]
pub fn eq_busy(site: u32, op: u8, orig: bool, av: Val, bv: Val) -> bool {
    let equal = orig == (op == EQ);
    let d = diff(av, bv, equal);
    let rel = match (equal, d) {
        (true, _) => REL_EQ,
        (false, Diff::Int(d)) if d != 0 => rel_of_d(d),
        (false, _) => REL_UN,
    };
    finish(site, op, orig, rel, av, bv, d)
}

/// Guards a statement: false when the active mutant deletes it.
#[inline(always)]
pub fn live(site: u32) -> bool {
    idle(site) || live_busy(site)
}

#[inline(never)]
fn live_busy(site: u32) -> bool {
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
    if idle(site) {
        return lhs_code(and, a, None);
    }
    let m = mutant_at(site);
    let c = lhs_code(and, a, m);
    if m.is_none() {
        record_bits(
            b'L',
            site,
            if c == 2 {
                LOGIC_RHS_STARTED
            } else {
                LOGIC_SHORT
            },
        );
    }
    c
}

/// Right operand of `a && b` / `a || b`, evaluated because `lhs` returned 2.
#[inline(always)]
pub fn rhs(site: u32, and: bool, b: bool) -> bool {
    if idle(site) {
        return b;
    }
    rhs_busy(site, and, b)
}

#[inline(never)]
fn rhs_busy(site: u32, and: bool, b: bool) -> bool {
    let m = mutant_at(site);
    if m.is_none() {
        record_bits(b'L', site, if b { LOGIC_RHS_TRUE } else { LOGIC_RHS_FALSE });
    }
    rhs_value(and, b, m)
}

/// Operands of an arithmetic operator, taken out once by the probe method.
pub struct Ar<L, R>(Cell<Option<(L, R)>>);

impl<L, R> Ar<L, R> {
    #[inline(always)]
    pub fn new(l: L, r: R) -> Self {
        Ar(Cell::new(Some((l, r))))
    }

    #[inline(always)]
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
        REM => DIV,
        BITAND => BITOR,
        BITOR => BITAND,
        BITXOR => BITAND,
        SHL => SHR,
        _ => SHL,
    }
}

/// The operator the `ArithSwap2` mutant uses. The shifts have one alternative only.
pub fn arith_swapped2(op: u8) -> Option<u8> {
    Some(match op {
        ADD => MUL,
        SUB => DIV,
        MUL => ADD,
        DIV => REM,
        REM => ADD,
        BITAND => BITXOR,
        BITOR => BITXOR,
        BITXOR => BITOR,
        _ => return None,
    })
}

pub trait ArithNum: Copy {
    fn tmt_real(op: u8, a: Self, b: Self) -> Self;
    /// The result as bits, or `None` where the real operator would overflow or divide by zero.
    fn tmt_safe(op: u8, a: Self, b: Self) -> Option<u64>;
    /// Inclusive `[min, max]` bounds for primitive integers, or `None` for floats.
    fn tmt_bounds() -> Option<(i128, i128)> {
        None
    }

    /// Whether the swapped operator gives the same result on these operands.
    ///
    /// An overflow or a division by zero on either side counts as different: what such
    /// an operation does (panic or wrap, and with which message) is not modelled here, so
    /// only running the tests can tell.
    fn tmt_same(op: u8, other: u8, a: Self, b: Self) -> bool {
        match (Self::tmt_safe(op, a, b), Self::tmt_safe(other, a, b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        }
    }

    #[inline(always)]
    fn tmt_run(site: u32, op: u8, a: Self, b: Self) -> Self {
        if idle(site) {
            return Self::tmt_real(op, a, b);
        }
        Self::tmt_busy(site, op, a, b)
    }

    #[inline(never)]
    fn tmt_busy(site: u32, op: u8, a: Self, b: Self) -> Self {
        let st = state();
        let second = arith_swapped2(op);
        match (mutant_at(site), second) {
            (Some(Mutant::ArithSwap), _) => return Self::tmt_real(arith_swapped(op), a, b),
            (Some(Mutant::ArithSwap2), Some(other)) => return Self::tmt_real(other, a, b),
            _ => {}
        }
        // Nothing to learn where both answers were seen for every replacement.
        let everything = ARITH_SAME
            | ARITH_DIFFERS
            | if second.is_some() {
                ARITH_SAME2 | ARITH_DIFFERS2
            } else {
                0
            };
        if st.tracer.is_some() && !bits_known(b'A', site, everything) {
            if matches!(op, BITOR | BITXOR) {
                if let Some((min, max)) = Self::tmt_bounds() {
                    with_tracer(|t, _| {
                        if t.typed.insert(site) {
                            let line = format!("T\t{}\t{}\t{}\t{}\t{}\n", site, min, max, min, max);
                            write_line(&mut t.out, &line);
                        }
                    });
                }
            }
            let mut bits = if Self::tmt_same(op, arith_swapped(op), a, b) {
                ARITH_SAME
            } else {
                ARITH_DIFFERS
            };
            if let Some(other) = second {
                bits |= if Self::tmt_same(op, other, a, b) {
                    ARITH_SAME2
                } else {
                    ARITH_DIFFERS2
                };
            }
            record_bits(b'A', site, bits);
        }
        Self::tmt_real(op, a, b)
    }
}

/// Probe for bit operators on integers.
pub trait BitPrim {
    type O;
    fn tmt_bitand(&self, site: u32) -> Self::O;
    fn tmt_bitor(&self, site: u32) -> Self::O;
    fn tmt_bitxor(&self, site: u32) -> Self::O;
    fn tmt_shl(&self, site: u32) -> Self::O;
    fn tmt_shr(&self, site: u32) -> Self::O;
}

/// The place and the right operand of a compound assignment, taken out once.
pub struct Aa<'p, L, R>(Cell<Option<(&'p mut L, R)>>);

impl<'p, L, R> Aa<'p, L, R> {
    #[inline(always)]
    pub fn new(l: &'p mut L, r: R) -> Self {
        Aa(Cell::new(Some((l, r))))
    }

    #[inline(always)]
    fn take(&self) -> (&'p mut L, R) {
        self.0.take().expect("operands are taken once")
    }
}

pub trait ArithAssignPrim {
    fn tmt_add_assign(&self, site: u32);
    fn tmt_sub_assign(&self, site: u32);
    fn tmt_mul_assign(&self, site: u32);
    fn tmt_div_assign(&self, site: u32);
    fn tmt_rem_assign(&self, site: u32);
}

pub trait BitAssignPrim {
    fn tmt_bitand_assign(&self, site: u32);
    fn tmt_bitor_assign(&self, site: u32);
    fn tmt_bitxor_assign(&self, site: u32);
    fn tmt_shl_assign(&self, site: u32);
    fn tmt_shr_assign(&self, site: u32);
}

macro_rules! tmt_arith_prim {($($t:ty)*) => {$(
        impl ArithPrim for Ar<$t, $t> {
            type O = $t;
            #[inline(always)] fn tmt_add(&self, site: u32) -> $t { let (a, b) = self.take(); <$t>::tmt_run(site, ADD, a, b) }
            #[inline(always)] fn tmt_sub(&self, site: u32) -> $t { let (a, b) = self.take(); <$t>::tmt_run(site, SUB, a, b) }
            #[inline(always)] fn tmt_mul(&self, site: u32) -> $t { let (a, b) = self.take(); <$t>::tmt_run(site, MUL, a, b) }
            #[inline(always)] fn tmt_div(&self, site: u32) -> $t { let (a, b) = self.take(); <$t>::tmt_run(site, DIV, a, b) }
                        #[inline(always)] fn tmt_rem(&self, site: u32) -> $t { let (a, b) = self.take(); <$t>::tmt_run(site, REM, a, b) }
        }
        impl<'p> ArithAssignPrim for Aa<'p, $t, $t> {
            #[inline(always)] fn tmt_add_assign(&self, site: u32) { let (l, r) = self.take(); *l = <$t>::tmt_run(site, ADD, *l, r) }
            #[inline(always)] fn tmt_sub_assign(&self, site: u32) { let (l, r) = self.take(); *l = <$t>::tmt_run(site, SUB, *l, r) }
            #[inline(always)] fn tmt_mul_assign(&self, site: u32) { let (l, r) = self.take(); *l = <$t>::tmt_run(site, MUL, *l, r) }
            #[inline(always)] fn tmt_div_assign(&self, site: u32) { let (l, r) = self.take(); *l = <$t>::tmt_run(site, DIV, *l, r) }
            #[inline(always)] fn tmt_rem_assign(&self, site: u32) { let (l, r) = self.take(); *l = <$t>::tmt_run(site, REM, *l, r) }
        }
    )*};
}

macro_rules! tmt_arith_ints {
    ($($t:ty)*) => {$(
        impl ArithNum for $t {
            #[inline(always)]
            fn tmt_real(op: u8, a: $t, b: $t) -> $t {
                match op {
                    ADD => a + b,
                    SUB => a - b,
                    MUL => a * b,
                    DIV => a / b,
                    REM => a % b,
                    BITAND => a & b,
                    BITOR => a | b,
                    BITXOR => a ^ b,
                    SHL => a << b,
                    _ => a >> b,
                }
            }
            fn tmt_safe(op: u8, a: $t, b: $t) -> Option<u64> {
                // A shift amount that does not fit is an overflow like any other.
                // Named in full: `TryFrom` is in the prelude only since the 2021 edition.
                let amount = <u32 as std::convert::TryFrom<$t>>::try_from(b).ok();
                match op {
                    ADD => a.checked_add(b),
                    SUB => a.checked_sub(b),
                    MUL => a.checked_mul(b),
                    DIV => a.checked_div(b),
                    REM => a.checked_rem(b),
                    BITAND => Some(a & b),
                    BITOR => Some(a | b),
                    BITXOR => Some(a ^ b),
                    SHL => amount.and_then(|n| a.checked_shl(n)),
                    _ => amount.and_then(|n| a.checked_shr(n)),
                }
                .map(|v| v as u64)
            }
            fn tmt_bounds() -> Option<(i128, i128)> {
                Some((<$t>::MIN as i128, <$t>::MAX as i128))
            }
        }
        impl BitPrim for Ar<$t, $t> {
            type O = $t;
            #[inline(always)] fn tmt_bitand(&self, site: u32) -> $t { let (a, b) = self.take(); <$t>::tmt_run(site, BITAND, a, b) }
            #[inline(always)] fn tmt_bitor(&self, site: u32) -> $t { let (a, b) = self.take(); <$t>::tmt_run(site, BITOR, a, b) }
            #[inline(always)] fn tmt_bitxor(&self, site: u32) -> $t { let (a, b) = self.take(); <$t>::tmt_run(site, BITXOR, a, b) }
            #[inline(always)] fn tmt_shl(&self, site: u32) -> $t { let (a, b) = self.take(); <$t>::tmt_run(site, SHL, a, b) }
            #[inline(always)] fn tmt_shr(&self, site: u32) -> $t { let (a, b) = self.take(); <$t>::tmt_run(site, SHR, a, b) }
        }
        impl<'p> BitAssignPrim for Aa<'p, $t, $t> {
            #[inline(always)] fn tmt_bitand_assign(&self, site: u32) { let (l, r) = self.take(); *l = <$t>::tmt_run(site, BITAND, *l, r) }
            #[inline(always)] fn tmt_bitor_assign(&self, site: u32) { let (l, r) = self.take(); *l = <$t>::tmt_run(site, BITOR, *l, r) }
            #[inline(always)] fn tmt_bitxor_assign(&self, site: u32) { let (l, r) = self.take(); *l = <$t>::tmt_run(site, BITXOR, *l, r) }
            #[inline(always)] fn tmt_shl_assign(&self, site: u32) { let (l, r) = self.take(); *l = <$t>::tmt_run(site, SHL, *l, r) }
            #[inline(always)] fn tmt_shr_assign(&self, site: u32) { let (l, r) = self.take(); *l = <$t>::tmt_run(site, SHR, *l, r) }
        }
    )*};
}

macro_rules! tmt_arith_floats {
    ($($t:ty)*) => {$(
        impl ArithNum for $t {
            #[inline(always)]
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
                if !idle(site) {
                    record_bits(b'A', site, ARITH_OPAQUE);
                }
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
tmt_arith_any!(
    AnyBitAnd tmt_bitand BitAnd bitand
    AnyBitOr tmt_bitor BitOr bitor
    AnyBitXor tmt_bitxor BitXor bitxor
    AnyShl tmt_shl Shl shl
    AnyShr tmt_shr Shr shr
);

macro_rules! tmt_assign_any {
    ($($name:ident $method:ident $bound:ident $call:ident)*) => {$(
        pub trait $name {
            fn $method(&self, site: u32);
        }
        impl<'r, 'p, L: std::ops::$bound<R>, R> $name for &'r Aa<'p, L, R> {
            fn $method(&self, site: u32) {
                let (l, r) = self.take();
                if !idle(site) {
                    record_bits(b'A', site, ARITH_OPAQUE);
                }
                std::ops::$bound::$call(l, r)
            }
        }
    )*};
}
tmt_assign_any!(
    AnyAddAssign tmt_add_assign AddAssign add_assign
    AnySubAssign tmt_sub_assign SubAssign sub_assign
    AnyMulAssign tmt_mul_assign MulAssign mul_assign
    AnyDivAssign tmt_div_assign DivAssign div_assign
    AnyRemAssign tmt_rem_assign RemAssign rem_assign
    AnyBitAndAssign tmt_bitand_assign BitAndAssign bitand_assign
    AnyBitOrAssign tmt_bitor_assign BitOrAssign bitor_assign
    AnyBitXorAssign tmt_bitxor_assign BitXorAssign bitxor_assign
    AnyShlAssign tmt_shl_assign ShlAssign shl_assign
    AnyShrAssign tmt_shr_assign ShrAssign shr_assign
);

/// Operand of `!`, taken out once.
pub struct Un<T>(Cell<Option<T>>);

impl<T> Un<T> {
    pub fn new(value: T) -> Self {
        Un(Cell::new(Some(value)))
    }
}

/// `!x` on a `bool`: the `DropNot` mutant returns `x`.
pub trait NotPrim {
    fn tmt_not(&self, site: u32) -> bool;
}

impl NotPrim for Un<bool> {
    #[inline(always)]
    fn tmt_not(&self, site: u32) -> bool {
        let value = self.0.take().expect("the operand is taken once");
        if idle(site) {
            return !value;
        }
        if mutant_at(site) == Some(Mutant::DropNot) {
            return value;
        }
        record_bits(b'R', site, 1);
        !value
    }
}

/// `!x` on anything else (integers, user types): untouched.
pub trait AnyNot {
    type O;
    fn tmt_not(&self, site: u32) -> Self::O;
}

impl<'r, T: std::ops::Not> AnyNot for &'r Un<T> {
    type O = T::Output;
    fn tmt_not(&self, site: u32) -> T::Output {
        let value = self.0.take().expect("the operand is taken once");
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        !value
    }
}

/// Normal exit of a `-> bool` function: `Const` replaces the value.
pub fn ret(site: u32, value: bool) -> bool {
    if idle(site) {
        return value;
    }
    if let Some(Mutant::Const(c)) = mutant_at(site) {
        return c;
    }
    record_bits(b'B', site, if value { RET_TRUE } else { RET_FALSE });
    value
}

/// Before evaluating a match guard: returns `Some(c)` when the active mutant replaces
/// the guard with constant `c` (so the original guard expression is not evaluated),
/// or `None` when the guard should be evaluated normally (recording reachability so
/// guards that return early or panic are still attributed to the test).
#[inline(always)]
pub fn guard_override(site: u32) -> Option<bool> {
    if idle(site) {
        return None;
    }
    if let Some(Mutant::Const(c)) = mutant_at(site) {
        return Some(c);
    }
    record_bits(b'R', site, 1);
    None
}

/// Records the value of a match guard that was evaluated normally.
#[inline(always)]
pub fn guard_record(site: u32, value: bool) -> bool {
    if !idle(site) {
        record_bits(b'B', site, if value { RET_TRUE } else { RET_FALSE });
    }
    value
}

/// Normal exit of a function returning `Option<T>`: `Const(false)` replaces the value by
/// `None`. (There is no value to replace `None` with.)
pub fn ret_opt<T>(site: u32, value: Option<T>) -> Option<T> {
    if idle(site) {
        return value;
    }
    if mutant_at(site) == Some(Mutant::Const(false)) {
        return None;
    }
    record_bits(
        b'B',
        site,
        if value.is_some() { RET_TRUE } else { RET_FALSE },
    );
    value
}

/// Guard of a `match` arm: false when the active mutant removes the arm.
#[inline(always)]
pub fn arm(site: u32) -> bool {
    idle(site) || live_busy(site)
}

/// `a..b` and `a..=b` on integers: the `RangeEnd` mutant includes or excludes the end.
pub trait RangePrim {
    type T;
    fn tmt_range(&self, site: u32) -> std::ops::Range<Self::T>;
    fn tmt_range_inclusive(&self, site: u32) -> std::ops::RangeInclusive<Self::T>;
}

macro_rules! tmt_ranges {
    ($($t:ty)*) => {$(
        impl RangePrim for Ar<$t, $t> {
            type T = $t;
            #[inline(always)]
            fn tmt_range(&self, site: u32) -> std::ops::Range<$t> {
                let (a, b) = self.take();
                if idle(site) {
                    return a..b;
                }
                if mutant_at(site) == Some(Mutant::RangeEnd) {
                    // At the top of the type there is no "one more", and no range of this
                    // kind that includes the end. The driver does not run this mutant
                    // where the tests get here as they are (see `RANGE_AT_TOP`); a run
                    // that only gets here with the mutant on is given up, because going
                    // on with the range as it is would not be the mutant.
                    return match b.checked_add(1) {
                        Some(end) => a..end,
                        None => cannot_evaluate(),
                    };
                }
                record_bits(b'R', site, if b == <$t>::MAX { 1 | RANGE_AT_TOP } else { 1 });
                a..b
            }
            #[inline(always)]
            fn tmt_range_inclusive(&self, site: u32) -> std::ops::RangeInclusive<$t> {
                let (a, b) = self.take();
                if idle(site) {
                    return a..=b;
                }
                if mutant_at(site) == Some(Mutant::RangeEnd) {
                    return match b.checked_sub(1) {
                        Some(end) => a..=end,
                        // `a..0`: an empty range.
                        None => 1..=0,
                    };
                }
                record_bits(b'R', site, 1);
                a..=b
            }
        }
    )*};
}
tmt_ranges!(i8 i16 i32 i64 isize u8 u16 u32 u64 usize);

/// Ranges of anything else (`'a'..'z'`, floats, user types): untouched.
pub trait AnyRange {
    type T;
    fn tmt_range(&self, site: u32) -> std::ops::Range<Self::T>;
    fn tmt_range_inclusive(&self, site: u32) -> std::ops::RangeInclusive<Self::T>;
}

impl<'r, T> AnyRange for &'r Ar<T, T> {
    type T = T;
    fn tmt_range(&self, site: u32) -> std::ops::Range<T> {
        let (a, b) = self.take();
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        a..b
    }
    fn tmt_range_inclusive(&self, site: u32) -> std::ops::RangeInclusive<T> {
        let (a, b) = self.take();
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        a..=b
    }
}

/// The two operands of `a.max(b)` / `a.min(b)`, looked at before the call. The call
/// itself stays in the code as it was written, so that a type with a `max` of its own
/// gets that one; the probe only says whether to call the other of the two.
pub struct Mm<'a, A, B>(pub &'a A, pub &'a B);

/// On numbers: the `MinMax` mutant takes the other one. The two only differ when the
/// operands do.
pub trait MinMaxPrim {
    fn tmt_other(&self, site: u32) -> bool;
}

macro_rules! tmt_min_max_ints {
    ($($t:ty)*) => {$(
        impl<'a> MinMaxPrim for Mm<'a, $t, $t> {
            #[inline(always)]
            fn tmt_other(&self, site: u32) -> bool {
                if idle(site) {
                    return false;
                }
                if mutant_at(site) == Some(Mutant::MinMax) {
                    return true;
                }
                if !bits_known(b'A', site, ARITH_SAME | ARITH_DIFFERS) {
                    record_bits(b'A', site, if self.0 == self.1 { ARITH_SAME } else { ARITH_DIFFERS });
                }
                false
            }
        }
    )*};
}
tmt_min_max_ints!(i8 i16 i32 i64 isize u8 u16 u32 u64 usize);

pub fn float_min_max_same(a: f64, b: f64) -> bool {
    !a.is_nan() && a.to_bits() == b.to_bits()
}

macro_rules! tmt_min_max_floats {
    ($($t:ty)*) => {$(
        impl<'a> MinMaxPrim for Mm<'a, $t, $t> {
            #[inline(always)]
            fn tmt_other(&self, site: u32) -> bool {
                if idle(site) {
                    return false;
                }
                if mutant_at(site) == Some(Mutant::MinMax) {
                    return true;
                }
                if !bits_known(b'A', site, ARITH_SAME | ARITH_DIFFERS) {
                    // `-0.0.max(0.0)` and `-0.0.min(0.0)` differ in sign bit; NaN is kept on the safe side.
                    let same = float_min_max_same(*self.0 as f64, *self.1 as f64);
                    record_bits(b'A', site, if same { ARITH_SAME } else { ARITH_DIFFERS });
                }
                false
            }
        }
    )*};
}
tmt_min_max_floats!(f32 f64);

/// `max` / `min` of anything else: never the other one.
pub trait AnyMinMax {
    fn tmt_other(&self, site: u32) -> bool;
}

impl<'r, 'a, A, B> AnyMinMax for &'r Mm<'a, A, B> {
    fn tmt_other(&self, site: u32) -> bool {
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        false
    }
}

/// The receiver of `v.first()` / `v.last()`, looked at before the call. Both calls stay in
/// the code as they are written, so that a type with methods of these names of its own
/// gets those; the probe only says which of the two to make.
pub struct Ends<'a, A: ?Sized>(pub &'a A);

/// What a probe of `first()` / `last()` does: call the other method, or the one that is
/// written and say what it returned.
pub enum End<S> {
    Other,
    Same(S),
}

/// A slice, a `Vec`, an array or a `BTreeSet`, or a shared reference to one. With no
/// element both ends are `None`, and with one they are that element.
pub trait SliceEnd {
    type Elem;
    fn tmt_other_end(&self, site: u32, first: bool) -> End<EndOf<Self::Elem>>;
}

/// The end the standard library gives, kept by its address: it borrows nothing, so the
/// call as it was written is made on a receiver that is as free as it was.
pub struct EndOf<T> {
    len: usize,
    end: Option<*const T>,
    /// An array of fewer than two elements has one end, and no test could give it
    /// another: nothing is mutated there.
    fixed: bool,
}

impl<T> EndOf<T> {
    /// `got`: what the call as it was written returned. It may be another method of that
    /// name (one a trait of the crate adds to `Vec`): where it did not return the very
    /// element the standard library gives, nothing is known about its other end, and
    /// nothing is mutated.
    #[inline(always)]
    pub fn tmt_end_seen(&self, site: u32, got: &Option<&T>) {
        if idle(site) {
            return;
        }
        let same = match (self.end, got) {
            (None, None) => true,
            (Some(ours), Some(theirs)) => std::ptr::eq(ours, *theirs),
            _ => false,
        };
        if self.fixed || !same {
            record_bits(b'A', site, ARITH_OPAQUE);
        } else {
            record_bits(
                b'A',
                site,
                if self.len <= 1 {
                    ARITH_SAME
                } else {
                    ARITH_DIFFERS
                },
            );
        }
    }
}

macro_rules! tmt_ends {
    ($([$($generics:tt)*] $t:ty, $elem:ty, $fixed:expr;)*) => {$(
        impl<'a, $($generics)*> SliceEnd for Ends<'a, $t> {
            type Elem = $elem;
            #[inline(always)]
            fn tmt_other_end(&self, site: u32, first: bool) -> End<EndOf<$elem>> {
                if idle(site) {
                    return End::Same(EndOf { len: 0, end: None, fixed: false });
                }
                let all: &$t = self.0;
                let fixed: bool = $fixed;
                if !fixed && mutant_at(site) == Some(Mutant::OtherEnd) {
                    return End::Other;
                }
                let end = if first { all.first() } else { all.last() };
                End::Same(EndOf { len: all.len(), end: end.map(|end| end as *const $elem), fixed })
            }
        }
        impl<'a, 'b, $($generics)*> SliceEnd for Ends<'a, &'b $t> {
            type Elem = $elem;
            fn tmt_other_end(&self, site: u32, first: bool) -> End<EndOf<$elem>> {
                (&Ends::<$t>(*self.0)).tmt_other_end(site, first)
            }
        }
    )*};
}
tmt_ends! {
    [T] [T], T, false;
    [T] Vec<T>, T, false;
    [T, const N: usize] [T; N], T, N < 2;
    [T: Ord] std::collections::BTreeSet<T>, T, false;
}

/// Never run: makes the compiler refuse a probe whose two calls return different types.
/// Left to itself it would turn one into the other where it can (a `&[u8; 2]` into a
/// `&[u8]`), and the call as it was written would no longer return what it did.
pub fn same_type<T: ?Sized>(_: &T, _: &T) {}

/// `first` / `last` of anything else: the call as it was written.
pub trait AnyEnd {
    fn tmt_other_end(&self, site: u32, first: bool) -> End<NoEnd>;
}

impl<'r, 'a, A: ?Sized> AnyEnd for &'r Ends<'a, A> {
    fn tmt_other_end(&self, site: u32, first: bool) -> End<NoEnd> {
        End::Same(NoEnd)
    }
}

/// Nothing is known of the ends of a type that is not a sequence of the standard library.
pub struct NoEnd;

impl NoEnd {
    pub fn tmt_end_seen<R>(&self, site: u32, got: &R) {
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
    }
}

/// `s.starts_with(p)` / `s.ends_with(p)` -> `s.contains(p)`.
pub trait TmtStrRef {
    fn tmt_as_str(&self) -> &str;
}

impl TmtStrRef for str {
    #[inline(always)]
    fn tmt_as_str(&self) -> &str {
        self
    }
}

impl TmtStrRef for &str {
    #[inline(always)]
    fn tmt_as_str(&self) -> &str {
        self
    }
}

impl TmtStrRef for &&str {
    #[inline(always)]
    fn tmt_as_str(&self) -> &str {
        self
    }
}

impl TmtStrRef for String {
    #[inline(always)]
    fn tmt_as_str(&self) -> &str {
        self.as_str()
    }
}

impl TmtStrRef for &String {
    #[inline(always)]
    fn tmt_as_str(&self) -> &str {
        self.as_str()
    }
}

pub trait TmtStrPat {
    fn tmt_affix_differs(&self, s: &str, starts: bool) -> bool;
    fn tmt_str_contains(&self, s: &str) -> bool;
    fn tmt_replace_differs(&self, s: &str, to: &str) -> bool;
    fn tmt_str_replace(&self, s: &str, to: &str) -> String;
    fn tmt_str_replacen1(&self, s: &str, to: &str) -> String;
}

/// Zero-allocation equivalence check for `s.replace(pat, to) != s.replacen(pat, to, 1)`.
/// Proven equivalent in `formal/lean/Planner.lean` (`replace_ne_replacen1_iff`).
#[inline]
pub fn str_replace_differs(s: &str, pat: &str, to: &str) -> bool {
    if pat == to {
        return false;
    }
    if pat.is_empty() {
        return !s.is_empty();
    }
    match s.find(pat) {
        Some(pos) => s[pos + pat.len()..].contains(pat),
        None => false,
    }
}

impl TmtStrPat for &str {
    #[inline(always)]
    fn tmt_affix_differs(&self, s: &str, starts: bool) -> bool {
        let orig = if starts {
            s.starts_with(*self)
        } else {
            s.ends_with(*self)
        };
        if orig {
            return false;
        }
        s.contains(*self)
    }

    #[inline(always)]
    fn tmt_str_contains(&self, s: &str) -> bool {
        s.contains(*self)
    }

    #[inline(always)]
    fn tmt_replace_differs(&self, s: &str, to: &str) -> bool {
        str_replace_differs(s, *self, to)
    }

    #[inline(always)]
    fn tmt_str_replace(&self, s: &str, to: &str) -> String {
        s.replace(*self, to)
    }

    #[inline(always)]
    fn tmt_str_replacen1(&self, s: &str, to: &str) -> String {
        s.replacen(*self, to, 1)
    }
}

impl TmtStrPat for &&str {
    #[inline(always)]
    fn tmt_affix_differs(&self, s: &str, starts: bool) -> bool {
        (*self).tmt_affix_differs(s, starts)
    }

    #[inline(always)]
    fn tmt_str_contains(&self, s: &str) -> bool {
        (*self).tmt_str_contains(s)
    }

    #[inline(always)]
    fn tmt_replace_differs(&self, s: &str, to: &str) -> bool {
        (*self).tmt_replace_differs(s, to)
    }

    #[inline(always)]
    fn tmt_str_replace(&self, s: &str, to: &str) -> String {
        (*self).tmt_str_replace(s, to)
    }

    #[inline(always)]
    fn tmt_str_replacen1(&self, s: &str, to: &str) -> String {
        (*self).tmt_str_replacen1(s, to)
    }
}

impl TmtStrPat for String {
    #[inline(always)]
    fn tmt_affix_differs(&self, s: &str, starts: bool) -> bool {
        self.as_str().tmt_affix_differs(s, starts)
    }

    #[inline(always)]
    fn tmt_str_contains(&self, s: &str) -> bool {
        self.as_str().tmt_str_contains(s)
    }

    #[inline(always)]
    fn tmt_replace_differs(&self, s: &str, to: &str) -> bool {
        self.as_str().tmt_replace_differs(s, to)
    }

    #[inline(always)]
    fn tmt_str_replace(&self, s: &str, to: &str) -> String {
        self.as_str().tmt_str_replace(s, to)
    }

    #[inline(always)]
    fn tmt_str_replacen1(&self, s: &str, to: &str) -> String {
        self.as_str().tmt_str_replacen1(s, to)
    }
}

impl TmtStrPat for &String {
    #[inline(always)]
    fn tmt_affix_differs(&self, s: &str, starts: bool) -> bool {
        self.as_str().tmt_affix_differs(s, starts)
    }

    #[inline(always)]
    fn tmt_str_contains(&self, s: &str) -> bool {
        self.as_str().tmt_str_contains(s)
    }

    #[inline(always)]
    fn tmt_replace_differs(&self, s: &str, to: &str) -> bool {
        self.as_str().tmt_replace_differs(s, to)
    }

    #[inline(always)]
    fn tmt_str_replace(&self, s: &str, to: &str) -> String {
        self.as_str().tmt_str_replace(s, to)
    }

    #[inline(always)]
    fn tmt_str_replacen1(&self, s: &str, to: &str) -> String {
        self.as_str().tmt_str_replacen1(s, to)
    }
}

impl TmtStrPat for char {
    #[inline(always)]
    fn tmt_affix_differs(&self, s: &str, starts: bool) -> bool {
        let orig = if starts {
            s.starts_with(*self)
        } else {
            s.ends_with(*self)
        };
        if orig {
            return false;
        }
        s.contains(*self)
    }

    #[inline(always)]
    fn tmt_str_contains(&self, s: &str) -> bool {
        s.contains(*self)
    }

    #[inline(always)]
    fn tmt_replace_differs(&self, s: &str, to: &str) -> bool {
        let mut buf = [0u8; 4];
        let pat = self.encode_utf8(&mut buf);
        str_replace_differs(s, pat, to)
    }

    #[inline(always)]
    fn tmt_str_replace(&self, s: &str, to: &str) -> String {
        s.replace(*self, to)
    }

    #[inline(always)]
    fn tmt_str_replacen1(&self, s: &str, to: &str) -> String {
        s.replacen(*self, to, 1)
    }
}

impl TmtStrPat for &char {
    #[inline(always)]
    fn tmt_affix_differs(&self, s: &str, starts: bool) -> bool {
        (*self).tmt_affix_differs(s, starts)
    }

    #[inline(always)]
    fn tmt_str_contains(&self, s: &str) -> bool {
        (*self).tmt_str_contains(s)
    }

    #[inline(always)]
    fn tmt_replace_differs(&self, s: &str, to: &str) -> bool {
        (*self).tmt_replace_differs(s, to)
    }

    #[inline(always)]
    fn tmt_str_replace(&self, s: &str, to: &str) -> String {
        (*self).tmt_str_replace(s, to)
    }

    #[inline(always)]
    fn tmt_str_replacen1(&self, s: &str, to: &str) -> String {
        (*self).tmt_str_replacen1(s, to)
    }
}

pub struct Aff<'a, A: ?Sized, P>(Cell<Option<(&'a A, P)>>);

impl<'a, A: ?Sized, P> Aff<'a, A, P> {
    #[inline(always)]
    pub fn new(a: &'a A, p: P) -> Self {
        Aff(Cell::new(Some((a, p))))
    }

    #[inline(always)]
    fn take(&self) -> (&'a A, P) {
        self.0.take().expect("operands are taken once")
    }
}

pub trait StrAffixPrim<'a, A: ?Sized + 'a, P> {
    fn tmt_str_affix<F: FnOnce(&'a A, P) -> bool>(&self, site: u32, starts: bool, orig: F) -> bool;
}

pub trait AnyStrAffix<'a, A: ?Sized + 'a, P> {
    fn tmt_str_affix<R, F: FnOnce(&'a A, P) -> R>(&self, site: u32, starts: bool, orig: F) -> R;
}

impl<'a, A: ?Sized + TmtStrRef, P: TmtStrPat> StrAffixPrim<'a, A, P> for Aff<'a, A, P> {
    #[inline(always)]
    fn tmt_str_affix<F: FnOnce(&'a A, P) -> bool>(&self, site: u32, starts: bool, orig: F) -> bool {
        let (a, p) = self.take();
        if idle(site) {
            return orig(a, p);
        }
        if mutant_at(site) == Some(Mutant::StrContains) {
            return p.tmt_str_contains(a.tmt_as_str());
        }
        if !bits_known(b'A', site, ARITH_SAME | ARITH_DIFFERS) {
            let differs = p.tmt_affix_differs(a.tmt_as_str(), starts);
            record_bits(b'A', site, if differs { ARITH_DIFFERS } else { ARITH_SAME });
        }
        orig(a, p)
    }
}

impl<'r, 'a, A: ?Sized, P> AnyStrAffix<'a, A, P> for &'r Aff<'a, A, P> {
    fn tmt_str_affix<R, F: FnOnce(&'a A, P) -> R>(&self, site: u32, _starts: bool, orig: F) -> R {
        let (a, p) = self.take();
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        orig(a, p)
    }
}

/// `s.replace(from, to)` <-> `s.replacen(from, to, 1)`.
pub struct Rep<'a, A: ?Sized, P, T>(Cell<Option<(&'a A, P, T)>>);

impl<'a, A: ?Sized, P, T> Rep<'a, A, P, T> {
    #[inline(always)]
    pub fn new(a: &'a A, p: P, t: T) -> Self {
        Rep(Cell::new(Some((a, p, t))))
    }

    #[inline(always)]
    fn take(&self) -> (&'a A, P, T) {
        self.0.take().expect("operands are taken once")
    }
}

pub trait StrReplacePrim<'a, A: ?Sized + 'a, P, T> {
    fn tmt_str_replace<F: FnOnce(&'a A, P, T) -> String>(
        &self,
        site: u32,
        replace_all: bool,
        orig: F,
    ) -> String;
}

pub trait AnyStrReplace<'a, A: ?Sized + 'a, P, T> {
    fn tmt_str_replace<R, F: FnOnce(&'a A, P, T) -> R>(
        &self,
        site: u32,
        replace_all: bool,
        orig: F,
    ) -> R;
}

impl<'a, A: ?Sized + TmtStrRef, P: TmtStrPat, T: TmtStrRef> StrReplacePrim<'a, A, P, T>
    for Rep<'a, A, P, T>
{
    #[inline(always)]
    fn tmt_str_replace<F: FnOnce(&'a A, P, T) -> String>(
        &self,
        site: u32,
        replace_all: bool,
        orig: F,
    ) -> String {
        let (a, p, t) = self.take();
        if idle(site) {
            return orig(a, p, t);
        }
        if mutant_at(site) == Some(Mutant::StrReplaceMultiplicity) {
            return if replace_all {
                p.tmt_str_replacen1(a.tmt_as_str(), t.tmt_as_str())
            } else {
                p.tmt_str_replace(a.tmt_as_str(), t.tmt_as_str())
            };
        }
        if !bits_known(b'A', site, ARITH_SAME | ARITH_DIFFERS) {
            let differs = p.tmt_replace_differs(a.tmt_as_str(), t.tmt_as_str());
            record_bits(b'A', site, if differs { ARITH_DIFFERS } else { ARITH_SAME });
        }
        orig(a, p, t)
    }
}

impl<'r, 'a, A: ?Sized, P, T> AnyStrReplace<'a, A, P, T> for &'r Rep<'a, A, P, T> {
    fn tmt_str_replace<R, F: FnOnce(&'a A, P, T) -> R>(
        &self,
        site: u32,
        _replace_all: bool,
        orig: F,
    ) -> R {
        let (a, p, t) = self.take();
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        orig(a, p, t)
    }
}

/// `x.clamp(lo, hi)` -> `x.min(hi)` or `x.max(lo)`.
pub struct Clp<'a, A: ?Sized, L, H>(Cell<Option<(&'a A, L, H)>>);

impl<'a, A: ?Sized, L, H> Clp<'a, A, L, H> {
    #[inline(always)]
    pub fn new(a: &'a A, lo: L, hi: H) -> Self {
        Clp(Cell::new(Some((a, lo, hi))))
    }

    #[inline(always)]
    fn take(&self) -> (&'a A, L, H) {
        self.0.take().expect("operands are taken once")
    }
}

pub trait ClampPrim<'a, A: ?Sized + 'a, L, H> {
    type Out;
    fn tmt_clamp<F: FnOnce(&'a A, L, H) -> Self::Out>(&self, site: u32, orig: F) -> Self::Out;
}

pub trait AnyClamp<'a, A: ?Sized + 'a, L, H> {
    fn tmt_clamp<R, F: FnOnce(&'a A, L, H) -> R>(&self, site: u32, orig: F) -> R;
}

impl<'r, 'a, A: ?Sized, L, H> AnyClamp<'a, A, L, H> for &'r Clp<'a, A, L, H> {
    fn tmt_clamp<R, F: FnOnce(&'a A, L, H) -> R>(&self, site: u32, orig: F) -> R {
        let (a, lo, hi) = self.take();
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        orig(a, lo, hi)
    }
}

macro_rules! tmt_clamp_ints {
    ($($t:ty)*) => {$(
        impl<'a> ClampPrim<'a, $t, $t, $t> for Clp<'a, $t, $t, $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_clamp<F: FnOnce(&'a $t, $t, $t) -> $t>(&self, site: u32, orig: F) -> $t {
                let (a, lo, hi) = self.take();
                if idle(site) {
                    return orig(a, lo, hi);
                }
                let x = *a;
                match mutant_at(site) {
                    Some(Mutant::ClampDropLower) => return x.min(hi),
                    Some(Mutant::ClampDropUpper) => return x.max(lo),
                    _ => {}
                }
                if !bits_known(b'A', site, ARITH_SAME | ARITH_DIFFERS | ARITH_SAME2 | ARITH_DIFFERS2) {
                    if lo <= hi {
                        let v = x.clamp(lo, hi);
                        let mut bits = if v == x.min(hi) { ARITH_SAME } else { ARITH_DIFFERS };
                        bits |= if v == x.max(lo) { ARITH_SAME2 } else { ARITH_DIFFERS2 };
                        record_bits(b'A', site, bits);
                    } else {
                        // `clamp` panics on reversed bounds while `min` and `max` return a value.
                        record_bits(b'A', site, ARITH_DIFFERS | ARITH_DIFFERS2);
                    }
                }
                orig(a, lo, hi)
            }
        }
        impl<'a, 'b> ClampPrim<'a, &'b $t, $t, $t> for Clp<'a, &'b $t, $t, $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_clamp<F: FnOnce(&'a &'b $t, $t, $t) -> $t>(&self, site: u32, orig: F) -> $t {
                let (a, lo, hi) = self.take();
                (&Clp::new(*a, lo, hi)).tmt_clamp(site, |_, lo, hi| orig(a, lo, hi))
            }
        }
    )*};
}
tmt_clamp_ints!(i8 i16 i32 i64 i128 isize u8 u16 u32 u64 u128 usize);

macro_rules! tmt_clamp_floats {
    ($($t:ty)*) => {$(
        impl<'a> ClampPrim<'a, $t, $t, $t> for Clp<'a, $t, $t, $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_clamp<F: FnOnce(&'a $t, $t, $t) -> $t>(&self, site: u32, orig: F) -> $t {
                let (a, lo, hi) = self.take();
                if idle(site) {
                    return orig(a, lo, hi);
                }
                let x = *a;
                match mutant_at(site) {
                    Some(Mutant::ClampDropLower) => return x.min(hi),
                    Some(Mutant::ClampDropUpper) => return x.max(lo),
                    _ => {}
                }
                if !bits_known(b'A', site, ARITH_SAME | ARITH_DIFFERS | ARITH_SAME2 | ARITH_DIFFERS2) {
                    if lo <= hi {
                        let v = x.clamp(lo, hi) as f64;
                        let drop_lo = x.min(hi) as f64;
                        let drop_hi = x.max(lo) as f64;
                        let mut bits = if float_min_max_same(v, drop_lo) { ARITH_SAME } else { ARITH_DIFFERS };
                        bits |= if float_min_max_same(v, drop_hi) { ARITH_SAME2 } else { ARITH_DIFFERS2 };
                        record_bits(b'A', site, bits);
                    } else {
                        // `clamp` panics on reversed or NaN bounds while `min` and `max` do not.
                        record_bits(b'A', site, ARITH_DIFFERS | ARITH_DIFFERS2);
                    }
                }
                orig(a, lo, hi)
            }
        }
        impl<'a, 'b> ClampPrim<'a, &'b $t, $t, $t> for Clp<'a, &'b $t, $t, $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_clamp<F: FnOnce(&'a &'b $t, $t, $t) -> $t>(&self, site: u32, orig: F) -> $t {
                let (a, lo, hi) = self.take();
                (&Clp::new(*a, lo, hi)).tmt_clamp(site, |_, lo, hi| orig(a, lo, hi))
            }
        }
    )*};
}
tmt_clamp_floats!(f32 f64);

/// `x.floor()` / `.ceil()` / `.round()` / `.trunc()` on floats.
pub struct Rnd<'a, A: ?Sized>(pub &'a A);

pub trait RoundPrim {
    type Out;
    fn tmt_round<F: FnOnce() -> Self::Out>(&self, site: u32, op: u8, orig: F) -> Self::Out;
}

pub trait AnyRound {
    fn tmt_round<R, F: FnOnce() -> R>(&self, site: u32, op: u8, orig: F) -> R;
}

impl<'r, 'a, A: ?Sized> AnyRound for &'r Rnd<'a, A> {
    fn tmt_round<R, F: FnOnce() -> R>(&self, site: u32, _op: u8, orig: F) -> R {
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        orig()
    }
}

macro_rules! tmt_round_floats {
    ($($t:ty)*) => {$(
        impl<'a> RoundPrim for Rnd<'a, $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_round<F: FnOnce() -> $t>(&self, site: u32, op: u8, orig: F) -> $t {
                if idle(site) {
                    return orig();
                }
                let x = *self.0;
                match mutant_at(site) {
                    Some(Mutant::RoundAlt1) => {
                        return match op {
                            0 => x.trunc(),
                            _ => x.floor(),
                        };
                    }
                    Some(Mutant::RoundAlt2) => {
                        return match op {
                            0 => x.ceil(),
                            1 | 2 => x.trunc(),
                            _ => x.round(),
                        };
                    }
                    _ => {}
                }
                if !bits_known(b'A', site, ARITH_SAME | ARITH_DIFFERS | ARITH_SAME2 | ARITH_DIFFERS2) {
                    let (v, alt1, alt2) = match op {
                        0 => (x.floor(), x.trunc(), x.ceil()),
                        1 => (x.ceil(), x.floor(), x.trunc()),
                        2 => (x.round(), x.floor(), x.trunc()),
                        _ => (x.trunc(), x.floor(), x.round()),
                    };
                    let mut bits = if float_min_max_same(v as f64, alt1 as f64) { ARITH_SAME } else { ARITH_DIFFERS };
                    bits |= if float_min_max_same(v as f64, alt2 as f64) { ARITH_SAME2 } else { ARITH_DIFFERS2 };
                    record_bits(b'A', site, bits);
                }
                orig()
            }
        }
        impl<'a, 'b> RoundPrim for Rnd<'a, &'b $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_round<F: FnOnce() -> $t>(&self, site: u32, op: u8, orig: F) -> $t {
                (&Rnd::<$t>(*self.0)).tmt_round(site, op, orig)
            }
        }
    )*};
}
tmt_round_floats!(f32 f64);

/// `iter.all(f)` <-> `iter.any(f)`: reach-guided probe specialized to `Iterator`.
pub struct Qnt<'a, I: ?Sized>(Cell<Option<&'a mut I>>);

impl<'a, I: ?Sized> Qnt<'a, I> {
    #[inline(always)]
    pub fn new(iter: &'a mut I) -> Self {
        Qnt(Cell::new(Some(iter)))
    }

    #[inline(always)]
    fn take(&self) -> &'a mut I {
        self.0.take().expect("iterator is taken once")
    }
}

/// Reborrows any receiver (`T`, `&mut T`, or temporary) as `Qnt<'_, Self>` via method auto-ref.
pub trait TmtQuantExt {
    fn __tmt_quant_mut(&mut self) -> Qnt<'_, Self>;
}

impl<T: ?Sized> TmtQuantExt for T {
    #[inline(always)]
    fn __tmt_quant_mut(&mut self) -> Qnt<'_, Self> {
        Qnt::new(self)
    }
}

pub trait QuantPrim<'a, I: ?Sized + 'a> {
    type Item;
    fn tmt_quant<P: FnMut(Self::Item) -> bool, G: FnOnce(&'a mut I, P) -> bool>(
        &self,
        site: u32,
        all: bool,
        pred: P,
        orig: G,
    ) -> bool;
}

pub trait AnyQuant<'a, I: ?Sized + 'a> {
    fn tmt_quant<P, R, G: FnOnce(&'a mut I, P) -> R>(
        &self,
        site: u32,
        all: bool,
        pred: P,
        orig: G,
    ) -> R;
}

impl<'a, I: Iterator> QuantPrim<'a, I> for Qnt<'a, I> {
    type Item = I::Item;
    #[inline(always)]
    fn tmt_quant<P: FnMut(I::Item) -> bool, G: FnOnce(&'a mut I, P) -> bool>(
        &self,
        site: u32,
        all: bool,
        pred: P,
        orig: G,
    ) -> bool {
        let iter = self.take();
        if idle(site) {
            return orig(iter, pred);
        }
        if mutant_at(site) == Some(Mutant::QuantifierSwap) {
            return if all { iter.any(pred) } else { iter.all(pred) };
        }
        record_bits(b'R', site, 1);
        orig(iter, pred)
    }
}

impl<'r, 'a, I: ?Sized> AnyQuant<'a, I> for &'r Qnt<'a, I> {
    fn tmt_quant<P, R, G: FnOnce(&'a mut I, P) -> R>(
        &self,
        site: u32,
        _all: bool,
        pred: P,
        orig: G,
    ) -> R {
        let iter = self.take();
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        orig(iter, pred)
    }
}

/// `a.saturating_{add,sub,mul}(b)` <-> `a.wrapping_{add,sub,mul}(b)`.
pub struct Sat<'a, A: ?Sized, B>(Cell<Option<(&'a A, B)>>);

impl<'a, A: ?Sized, B> Sat<'a, A, B> {
    #[inline(always)]
    pub fn new(a: &'a A, b: B) -> Self {
        Sat(Cell::new(Some((a, b))))
    }

    #[inline(always)]
    fn take(&self) -> (&'a A, B) {
        self.0.take().expect("operands are taken once")
    }
}

pub trait SaturatingPrim<'a, A: ?Sized + 'a, B> {
    type Out;
    fn tmt_sat<F: FnOnce(&'a A, B) -> Self::Out>(&self, site: u32, op: u8, orig: F) -> Self::Out;
}

pub trait AnySaturating<'a, A: ?Sized + 'a, B> {
    fn tmt_sat<R, F: FnOnce(&'a A, B) -> R>(&self, site: u32, op: u8, orig: F) -> R;
}

impl<'r, 'a, A: ?Sized, B> AnySaturating<'a, A, B> for &'r Sat<'a, A, B> {
    fn tmt_sat<R, F: FnOnce(&'a A, B) -> R>(&self, site: u32, _op: u8, orig: F) -> R {
        let (a, b) = self.take();
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        orig(a, b)
    }
}

macro_rules! tmt_sat_ints {
    ($($t:ty)*) => {$(
        impl<'a> SaturatingPrim<'a, $t, $t> for Sat<'a, $t, $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_sat<F: FnOnce(&'a $t, $t) -> $t>(&self, site: u32, op: u8, orig: F) -> $t {
                let (a, b) = self.take();
                if idle(site) {
                    return orig(a, b);
                }
                let x = *a;
                let swapped = match op {
                    0 => x.wrapping_add(b),
                    1 => x.wrapping_sub(b),
                    2 => x.wrapping_mul(b),
                    3 => x.saturating_add(b),
                    4 => x.saturating_sub(b),
                    _ => x.saturating_mul(b),
                };
                if mutant_at(site) == Some(Mutant::SaturatingSwap) {
                    return swapped;
                }
                let original = orig(a, b);
                if !bits_known(b'A', site, ARITH_SAME | ARITH_DIFFERS) {
                    record_bits(
                        b'A',
                        site,
                        if original == swapped { ARITH_SAME } else { ARITH_DIFFERS },
                    );
                }
                original
            }
        }
        impl<'a, 'b> SaturatingPrim<'a, &'b $t, $t> for Sat<'a, &'b $t, $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_sat<F: FnOnce(&'a &'b $t, $t) -> $t>(&self, site: u32, op: u8, orig: F) -> $t {
                let (a, b) = self.take();
                (&Sat::new(*a, b)).tmt_sat(site, op, |_, b| orig(a, b))
            }
        }
        impl<'a, 'b> SaturatingPrim<'a, &'b mut $t, $t> for Sat<'a, &'b mut $t, $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_sat<F: FnOnce(&'a &'b mut $t, $t) -> $t>(&self, site: u32, op: u8, orig: F) -> $t {
                let (a, b) = self.take();
                (&Sat::new(&**a, b)).tmt_sat(site, op, |_, b| orig(a, b))
            }
        }
        impl<'a, 'b, 'c> SaturatingPrim<'a, &'b &'c $t, $t> for Sat<'a, &'b &'c $t, $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_sat<F: FnOnce(&'a &'b &'c $t, $t) -> $t>(&self, site: u32, op: u8, orig: F) -> $t {
                let (a, b) = self.take();
                (&Sat::new(**a, b)).tmt_sat(site, op, |_, b| orig(a, b))
            }
        }
    )*};
}
tmt_sat_ints!(i8 i16 i32 i64 i128 isize u8 u16 u32 u64 u128 usize);

/// `s.trim()` / `.trim_start()` / `.trim_end()` on strings.
pub struct Trm<'a, A: ?Sized>(pub &'a A);

pub trait StrTrimPrim<'a, A: ?Sized + 'a> {
    type Out;
    fn tmt_str_trim<F: FnOnce(&'a A) -> Self::Out>(&self, site: u32, op: u8, orig: F) -> Self::Out;
}

pub trait AnyStrTrim<'a, A: ?Sized + 'a> {
    fn tmt_str_trim<R, F: FnOnce(&'a A) -> R>(&self, site: u32, op: u8, orig: F) -> R;
}

#[inline(always)]
fn eval_str_trim<'s>(site: u32, op: u8, s: &'s str) -> Option<&'s str> {
    match mutant_at(site) {
        Some(Mutant::TrimAlt1) => {
            return Some(match op {
                0 => s.trim_start(),
                _ => s.trim(),
            });
        }
        Some(Mutant::TrimAlt2) => {
            return Some(s.trim_end());
        }
        _ => {}
    }
    let want = if op == 0 {
        ARITH_SAME | ARITH_DIFFERS | ARITH_SAME2 | ARITH_DIFFERS2
    } else {
        ARITH_SAME | ARITH_DIFFERS
    };
    if !bits_known(b'A', site, want) {
        let same_slice =
            |a: &str, b: &str| a.len() == b.len() && core::ptr::eq(a.as_ptr(), b.as_ptr());
        let trimmed = s.trim();
        let bits = match op {
            0 => {
                let b1 = if same_slice(trimmed, s.trim_start()) {
                    ARITH_SAME
                } else {
                    ARITH_DIFFERS
                };
                let b2 = if same_slice(trimmed, s.trim_end()) {
                    ARITH_SAME2
                } else {
                    ARITH_DIFFERS2
                };
                b1 | b2
            }
            1 => {
                if same_slice(s.trim_start(), trimmed) {
                    ARITH_SAME
                } else {
                    ARITH_DIFFERS
                }
            }
            _ => {
                if same_slice(s.trim_end(), trimmed) {
                    ARITH_SAME
                } else {
                    ARITH_DIFFERS
                }
            }
        };
        record_bits(b'A', site, bits);
    }
    None
}

impl<'a> StrTrimPrim<'a, str> for Trm<'a, str> {
    type Out = &'a str;
    #[inline(always)]
    fn tmt_str_trim<F: FnOnce(&'a str) -> &'a str>(&self, site: u32, op: u8, orig: F) -> &'a str {
        if idle(site) {
            return orig(self.0);
        }
        if let Some(m) = eval_str_trim(site, op, self.0) {
            return m;
        }
        orig(self.0)
    }
}

impl<'a> StrTrimPrim<'a, String> for Trm<'a, String> {
    type Out = &'a str;
    #[inline(always)]
    fn tmt_str_trim<F: FnOnce(&'a String) -> &'a str>(
        &self,
        site: u32,
        op: u8,
        orig: F,
    ) -> &'a str {
        if idle(site) {
            return orig(self.0);
        }
        if let Some(m) = eval_str_trim(site, op, self.0.as_str()) {
            return m;
        }
        orig(self.0)
    }
}

impl<'a, 'b> StrTrimPrim<'a, &'b str> for Trm<'a, &'b str> {
    type Out = &'b str;
    #[inline(always)]
    fn tmt_str_trim<F: FnOnce(&'a &'b str) -> &'b str>(
        &self,
        site: u32,
        op: u8,
        orig: F,
    ) -> &'b str {
        if idle(site) {
            return orig(self.0);
        }
        if let Some(m) = eval_str_trim(site, op, *self.0) {
            return m;
        }
        orig(self.0)
    }
}

impl<'a, 'b> StrTrimPrim<'a, &'b String> for Trm<'a, &'b String> {
    type Out = &'b str;
    #[inline(always)]
    fn tmt_str_trim<F: FnOnce(&'a &'b String) -> &'b str>(
        &self,
        site: u32,
        op: u8,
        orig: F,
    ) -> &'b str {
        if idle(site) {
            return orig(self.0);
        }
        if let Some(m) = eval_str_trim(site, op, (*self.0).as_str()) {
            return m;
        }
        orig(self.0)
    }
}

impl<'a, 'b> StrTrimPrim<'a, &'b mut String> for Trm<'a, &'b mut String> {
    type Out = &'a str;
    #[inline(always)]
    fn tmt_str_trim<F: FnOnce(&'a &'b mut String) -> &'a str>(
        &self,
        site: u32,
        op: u8,
        orig: F,
    ) -> &'a str {
        if idle(site) {
            return orig(self.0);
        }
        if let Some(m) = eval_str_trim(site, op, self.0.as_str()) {
            return m;
        }
        orig(self.0)
    }
}

impl<'a, 'b, 'c> StrTrimPrim<'a, &'b &'c str> for Trm<'a, &'b &'c str> {
    type Out = &'c str;
    #[inline(always)]
    fn tmt_str_trim<F: FnOnce(&'a &'b &'c str) -> &'c str>(
        &self,
        site: u32,
        op: u8,
        orig: F,
    ) -> &'c str {
        if idle(site) {
            return orig(self.0);
        }
        if let Some(m) = eval_str_trim(site, op, **self.0) {
            return m;
        }
        orig(self.0)
    }
}

impl<'r, 'a, A: ?Sized> AnyStrTrim<'a, A> for &'r Trm<'a, A> {
    fn tmt_str_trim<R, F: FnOnce(&'a A) -> R>(&self, site: u32, _op: u8, orig: F) -> R {
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        orig(self.0)
    }
}

/// `a.eq_ignore_ascii_case(b)` -> `a == b`.
pub trait TmtAsciiBytes {
    fn tmt_ascii_bytes(&self) -> &[u8];
}

impl TmtAsciiBytes for str {
    #[inline(always)]
    fn tmt_ascii_bytes(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl TmtAsciiBytes for String {
    #[inline(always)]
    fn tmt_ascii_bytes(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl TmtAsciiBytes for [u8] {
    #[inline(always)]
    fn tmt_ascii_bytes(&self) -> &[u8] {
        self
    }
}

impl TmtAsciiBytes for Vec<u8> {
    #[inline(always)]
    fn tmt_ascii_bytes(&self) -> &[u8] {
        self.as_slice()
    }
}

impl TmtAsciiBytes for u8 {
    #[inline(always)]
    fn tmt_ascii_bytes(&self) -> &[u8] {
        std::slice::from_ref(self)
    }
}

impl<const N: usize> TmtAsciiBytes for [u8; N] {
    #[inline(always)]
    fn tmt_ascii_bytes(&self) -> &[u8] {
        self.as_slice()
    }
}

impl<T: ?Sized + TmtAsciiBytes> TmtAsciiBytes for &T {
    #[inline(always)]
    fn tmt_ascii_bytes(&self) -> &[u8] {
        (*self).tmt_ascii_bytes()
    }
}

impl<T: ?Sized + TmtAsciiBytes> TmtAsciiBytes for &mut T {
    #[inline(always)]
    fn tmt_ascii_bytes(&self) -> &[u8] {
        (**self).tmt_ascii_bytes()
    }
}

pub struct Ceq<'a, A: ?Sized, B>(Cell<Option<(&'a A, B)>>);

impl<'a, A: ?Sized, B> Ceq<'a, A, B> {
    #[inline(always)]
    pub fn new(a: &'a A, b: B) -> Self {
        Ceq(Cell::new(Some((a, b))))
    }

    #[inline(always)]
    fn take(&self) -> (&'a A, B) {
        self.0.take().expect("operands are taken once")
    }
}

pub trait CaseEqPrim<'a, A: ?Sized + 'a, B> {
    fn tmt_case_eq<F: FnOnce(&'a A, B) -> bool>(&self, site: u32, orig: F) -> bool;
}

pub trait AnyCaseEq<'a, A: ?Sized + 'a, B> {
    fn tmt_case_eq<R, F: FnOnce(&'a A, B) -> R>(&self, site: u32, orig: F) -> R;
}

impl<'a, A: ?Sized + TmtAsciiBytes, B: TmtAsciiBytes> CaseEqPrim<'a, A, B> for Ceq<'a, A, B> {
    #[inline(always)]
    fn tmt_case_eq<F: FnOnce(&'a A, B) -> bool>(&self, site: u32, orig: F) -> bool {
        let (a, b) = self.take();
        if idle(site) {
            return orig(a, b);
        }
        let exact = a.tmt_ascii_bytes() == b.tmt_ascii_bytes();
        if mutant_at(site) == Some(Mutant::StrExactEq) {
            return exact;
        }
        let original = orig(a, b);
        if !bits_known(b'A', site, ARITH_SAME | ARITH_DIFFERS) {
            record_bits(
                b'A',
                site,
                if original == exact {
                    ARITH_SAME
                } else {
                    ARITH_DIFFERS
                },
            );
        }
        original
    }
}

impl<'r, 'a, A: ?Sized, B> AnyCaseEq<'a, A, B> for &'r Ceq<'a, A, B> {
    fn tmt_case_eq<R, F: FnOnce(&'a A, B) -> R>(&self, site: u32, orig: F) -> R {
        let (a, b) = self.take();
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        orig(a, b)
    }
}

/// `x.abs()` -> `x` on signed integers and floats.
pub struct AbsWrap<'a, A: ?Sized>(pub &'a A);

pub trait AbsPrim {
    type Out;
    fn tmt_abs<F: FnOnce() -> Self::Out>(&self, site: u32, orig: F) -> Self::Out;
}

pub trait AnyAbs {
    fn tmt_abs<R, F: FnOnce() -> R>(&self, site: u32, orig: F) -> R;
}

impl<'r, 'a, A: ?Sized> AnyAbs for &'r AbsWrap<'a, A> {
    fn tmt_abs<R, F: FnOnce() -> R>(&self, site: u32, orig: F) -> R {
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        orig()
    }
}

macro_rules! tmt_abs_signed {
    ($($t:ty)*) => {$(
        impl<'a> AbsPrim for AbsWrap<'a, $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_abs<F: FnOnce() -> $t>(&self, site: u32, orig: F) -> $t {
                if idle(site) {
                    return orig();
                }
                let x = *self.0;
                if mutant_at(site) == Some(Mutant::DropAbs) {
                    return x;
                }
                if !bits_known(b'A', site, ARITH_SAME | ARITH_DIFFERS) {
                    // `MIN.abs()` panics with overflow checks enabled, and wraps back to
                    // `MIN` (matching `x`) when overflow checks are disabled.
                    let differs = if x == <$t>::MIN {
                        cfg!(debug_assertions)
                    } else {
                        x < 0
                    };
                    record_bits(b'A', site, if differs { ARITH_DIFFERS } else { ARITH_SAME });
                }
                orig()
            }
        }
        impl<'a, 'b> AbsPrim for AbsWrap<'a, &'b $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_abs<F: FnOnce() -> $t>(&self, site: u32, orig: F) -> $t {
                (&AbsWrap::<$t>(*self.0)).tmt_abs(site, orig)
            }
        }
        impl<'a, 'b> AbsPrim for AbsWrap<'a, &'b mut $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_abs<F: FnOnce() -> $t>(&self, site: u32, orig: F) -> $t {
                (&AbsWrap::<$t>(&**self.0)).tmt_abs(site, orig)
            }
        }
        impl<'a, 'b, 'c> AbsPrim for AbsWrap<'a, &'b &'c $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_abs<F: FnOnce() -> $t>(&self, site: u32, orig: F) -> $t {
                (&AbsWrap::<$t>(**self.0)).tmt_abs(site, orig)
            }
        }
    )*};
}
tmt_abs_signed!(i8 i16 i32 i64 i128 isize);

macro_rules! tmt_abs_floats {
    ($($t:ty)*) => {$(
        impl<'a> AbsPrim for AbsWrap<'a, $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_abs<F: FnOnce() -> $t>(&self, site: u32, orig: F) -> $t {
                if idle(site) {
                    return orig();
                }
                let x = *self.0;
                if mutant_at(site) == Some(Mutant::DropAbs) {
                    return x;
                }
                if !bits_known(b'A', site, ARITH_SAME | ARITH_DIFFERS) {
                    // Use bitwise comparison so `-0.0` and `-NaN` (distinguishable via
                    // `to_bits()` or `1.0 / x`) are treated as differing.
                    let differs = x.abs().to_bits() != x.to_bits();
                    record_bits(b'A', site, if differs { ARITH_DIFFERS } else { ARITH_SAME });
                }
                orig()
            }
        }
        impl<'a, 'b> AbsPrim for AbsWrap<'a, &'b $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_abs<F: FnOnce() -> $t>(&self, site: u32, orig: F) -> $t {
                (&AbsWrap::<$t>(*self.0)).tmt_abs(site, orig)
            }
        }
        impl<'a, 'b> AbsPrim for AbsWrap<'a, &'b mut $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_abs<F: FnOnce() -> $t>(&self, site: u32, orig: F) -> $t {
                (&AbsWrap::<$t>(&**self.0)).tmt_abs(site, orig)
            }
        }
        impl<'a, 'b, 'c> AbsPrim for AbsWrap<'a, &'b &'c $t> {
            type Out = $t;
            #[inline(always)]
            fn tmt_abs<F: FnOnce() -> $t>(&self, site: u32, orig: F) -> $t {
                (&AbsWrap::<$t>(**self.0)).tmt_abs(site, orig)
            }
        }
    )*};
}
tmt_abs_floats!(f32 f64);

/// `iter.take(n)` -> `iter.take(usize::MAX)` / `iter.skip(n)` -> `iter.skip(0)`.
pub struct Ilm<I, N>(Cell<Option<(I, N)>>);

impl<I, N> Ilm<I, N> {
    #[inline(always)]
    pub fn new(iter: I, n: N) -> Self {
        Ilm(Cell::new(Some((iter, n))))
    }

    #[inline(always)]
    fn take(&self) -> (I, N) {
        self.0.take().expect("operands are taken once")
    }
}

pub trait IterLimitPrim<I, N> {
    fn tmt_iter_limit<R, F: FnOnce(I, N) -> R>(&self, site: u32, take: bool, orig: F) -> R;
}

pub trait AnyIterLimit<I, N> {
    fn tmt_iter_limit<R, F: FnOnce(I, N) -> R>(&self, site: u32, take: bool, orig: F) -> R;
}

impl<I: Iterator> IterLimitPrim<I, usize> for Ilm<I, usize> {
    #[inline(always)]
    fn tmt_iter_limit<R, F: FnOnce(I, usize) -> R>(&self, site: u32, take: bool, orig: F) -> R {
        let (iter, n) = self.take();
        if idle(site) {
            return orig(iter, n);
        }
        let drop_val = if take { usize::MAX } else { 0 };
        if mutant_at(site) == Some(Mutant::IterLimitDrop) {
            return orig(iter, drop_val);
        }
        if !bits_known(b'A', site, ARITH_SAME | ARITH_DIFFERS) {
            record_bits(
                b'A',
                site,
                if n == drop_val {
                    ARITH_SAME
                } else {
                    ARITH_DIFFERS
                },
            );
        }
        orig(iter, n)
    }
}

impl<'r, I, N> AnyIterLimit<I, N> for &'r Ilm<I, N> {
    fn tmt_iter_limit<R, F: FnOnce(I, N) -> R>(&self, site: u32, _take: bool, orig: F) -> R {
        let (iter, n) = self.take();
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        orig(iter, n)
    }
}

/// `x.to_be_bytes()` <-> `x.to_le_bytes()`, `x.to_be()` <-> `x.to_le()` on multi-byte ints.
pub struct Endn<'a, A: ?Sized>(pub &'a A);

pub trait EndianPrim {
    type Bytes;
    type Int;
    fn tmt_endian_bytes<F: FnOnce() -> Self::Bytes>(
        &self,
        site: u32,
        to_be: bool,
        orig: F,
    ) -> Self::Bytes;
    fn tmt_endian_int<F: FnOnce() -> Self::Int>(
        &self,
        site: u32,
        to_be: bool,
        orig: F,
    ) -> Self::Int;
}

pub trait AnyEndian {
    fn tmt_endian_bytes<R, F: FnOnce() -> R>(&self, site: u32, to_be: bool, orig: F) -> R;
    fn tmt_endian_int<R, F: FnOnce() -> R>(&self, site: u32, to_be: bool, orig: F) -> R;
}

impl<'r, 'a, A: ?Sized> AnyEndian for &'r Endn<'a, A> {
    fn tmt_endian_bytes<R, F: FnOnce() -> R>(&self, site: u32, _to_be: bool, orig: F) -> R {
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        orig()
    }

    fn tmt_endian_int<R, F: FnOnce() -> R>(&self, site: u32, _to_be: bool, orig: F) -> R {
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        orig()
    }
}

macro_rules! tmt_endian_ints {
    ($($t:ty),* $(,)?) => {$(
        impl<'a> EndianPrim for Endn<'a, $t> {
            type Bytes = [u8; core::mem::size_of::<$t>()];
            type Int = $t;
            #[inline(always)]
            fn tmt_endian_bytes<F: FnOnce() -> Self::Bytes>(
                &self,
                site: u32,
                to_be: bool,
                orig: F,
            ) -> Self::Bytes {
                if idle(site) {
                    return orig();
                }
                let x = *self.0;
                if mutant_at(site) == Some(Mutant::EndianSwap) {
                    return if to_be { x.to_le_bytes() } else { x.to_be_bytes() };
                }
                if !bits_known(b'A', site, ARITH_SAME | ARITH_DIFFERS) {
                    record_bits(
                        b'A',
                        site,
                        if x == x.swap_bytes() { ARITH_SAME } else { ARITH_DIFFERS },
                    );
                }
                orig()
            }
            #[inline(always)]
            fn tmt_endian_int<F: FnOnce() -> Self::Int>(
                &self,
                site: u32,
                to_be: bool,
                orig: F,
            ) -> Self::Int {
                if idle(site) {
                    return orig();
                }
                let x = *self.0;
                if mutant_at(site) == Some(Mutant::EndianSwap) {
                    return if to_be { x.to_le() } else { x.to_be() };
                }
                if !bits_known(b'A', site, ARITH_SAME | ARITH_DIFFERS) {
                    record_bits(
                        b'A',
                        site,
                        if x == x.swap_bytes() { ARITH_SAME } else { ARITH_DIFFERS },
                    );
                }
                orig()
            }
        }
        impl<'a, 'b> EndianPrim for Endn<'a, &'b $t> {
            type Bytes = [u8; core::mem::size_of::<$t>()];
            type Int = $t;
            #[inline(always)]
            fn tmt_endian_bytes<F: FnOnce() -> Self::Bytes>(
                &self,
                site: u32,
                to_be: bool,
                orig: F,
            ) -> Self::Bytes {
                (&Endn::<$t>(*self.0)).tmt_endian_bytes(site, to_be, orig)
            }
            #[inline(always)]
            fn tmt_endian_int<F: FnOnce() -> Self::Int>(
                &self,
                site: u32,
                to_be: bool,
                orig: F,
            ) -> Self::Int {
                (&Endn::<$t>(*self.0)).tmt_endian_int(site, to_be, orig)
            }
        }
        impl<'a, 'b> EndianPrim for Endn<'a, &'b mut $t> {
            type Bytes = [u8; core::mem::size_of::<$t>()];
            type Int = $t;
            #[inline(always)]
            fn tmt_endian_bytes<F: FnOnce() -> Self::Bytes>(
                &self,
                site: u32,
                to_be: bool,
                orig: F,
            ) -> Self::Bytes {
                (&Endn::<$t>(&**self.0)).tmt_endian_bytes(site, to_be, orig)
            }
            #[inline(always)]
            fn tmt_endian_int<F: FnOnce() -> Self::Int>(
                &self,
                site: u32,
                to_be: bool,
                orig: F,
            ) -> Self::Int {
                (&Endn::<$t>(&**self.0)).tmt_endian_int(site, to_be, orig)
            }
        }
        impl<'a, 'b, 'c> EndianPrim for Endn<'a, &'b &'c $t> {
            type Bytes = [u8; core::mem::size_of::<$t>()];
            type Int = $t;
            #[inline(always)]
            fn tmt_endian_bytes<F: FnOnce() -> Self::Bytes>(
                &self,
                site: u32,
                to_be: bool,
                orig: F,
            ) -> Self::Bytes {
                (&Endn::<$t>(**self.0)).tmt_endian_bytes(site, to_be, orig)
            }
            #[inline(always)]
            fn tmt_endian_int<F: FnOnce() -> Self::Int>(
                &self,
                site: u32,
                to_be: bool,
                orig: F,
            ) -> Self::Int {
                (&Endn::<$t>(**self.0)).tmt_endian_int(site, to_be, orig)
            }
        }
    )*};
}
tmt_endian_ints!(u16, i16, u32, i32, u64, i64, u128, i128, usize, isize);

/// `x.leading_zeros()` <-> `x.trailing_zeros()`, `leading_ones` <-> `trailing_ones`.
pub struct Btc<'a, A: ?Sized>(pub &'a A);

pub trait BitCountPrim {
    fn tmt_bit_count<F: FnOnce() -> u32>(&self, site: u32, op: u8, orig: F) -> u32;
}

pub trait AnyBitCount {
    fn tmt_bit_count<R, F: FnOnce() -> R>(&self, site: u32, op: u8, orig: F) -> R;
}

impl<'r, 'a, A: ?Sized> AnyBitCount for &'r Btc<'a, A> {
    fn tmt_bit_count<R, F: FnOnce() -> R>(&self, site: u32, _op: u8, orig: F) -> R {
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        orig()
    }
}

macro_rules! tmt_bitcount_ints {
    ($($t:ty)*) => {$(
        impl<'a> BitCountPrim for Btc<'a, $t> {
            #[inline(always)]
            fn tmt_bit_count<F: FnOnce() -> u32>(&self, site: u32, op: u8, orig: F) -> u32 {
                if idle(site) {
                    return orig();
                }
                let x = *self.0;
                let swapped = match op {
                    0 => x.trailing_zeros(),
                    1 => x.leading_zeros(),
                    2 => x.trailing_ones(),
                    _ => x.leading_ones(),
                };
                if mutant_at(site) == Some(Mutant::BitCountSwap) {
                    return swapped;
                }
                let original = orig();
                if !bits_known(b'A', site, ARITH_SAME | ARITH_DIFFERS) {
                    record_bits(
                        b'A',
                        site,
                        if original == swapped { ARITH_SAME } else { ARITH_DIFFERS },
                    );
                }
                original
            }
        }
        impl<'a, 'b> BitCountPrim for Btc<'a, &'b $t> {
            #[inline(always)]
            fn tmt_bit_count<F: FnOnce() -> u32>(&self, site: u32, op: u8, orig: F) -> u32 {
                (&Btc::<$t>(*self.0)).tmt_bit_count(site, op, orig)
            }
        }
        impl<'a, 'b> BitCountPrim for Btc<'a, &'b mut $t> {
            #[inline(always)]
            fn tmt_bit_count<F: FnOnce() -> u32>(&self, site: u32, op: u8, orig: F) -> u32 {
                (&Btc::<$t>(&**self.0)).tmt_bit_count(site, op, orig)
            }
        }
        impl<'a, 'b, 'c> BitCountPrim for Btc<'a, &'b &'c $t> {
            #[inline(always)]
            fn tmt_bit_count<F: FnOnce() -> u32>(&self, site: u32, op: u8, orig: F) -> u32 {
                (&Btc::<$t>(**self.0)).tmt_bit_count(site, op, orig)
            }
        }
    )*};
}
tmt_bitcount_ints!(i8 i16 i32 i64 i128 isize u8 u16 u32 u64 u128 usize);

/// `break` <-> `continue` loop control probe.
#[inline(always)]
pub fn loop_ctrl_swap(site: u32) -> bool {
    if idle(site) {
        return false;
    }
    if mutant_at(site) == Some(Mutant::LoopCtrlSwap) {
        return true;
    }
    record_bits(b'R', site, 1);
    false
}

/// For an indexed range `xs[a..b]` / `xs[a..=b]`: returns `true` when the `RangeEnd`
/// mutant is active so the probe switches directly between `..` and `..=`.
#[inline(always)]
pub fn range_end(site: u32) -> bool {
    if idle(site) {
        return false;
    }
    if mutant_at(site) == Some(Mutant::RangeEnd) {
        return true;
    }
    record_bits(b'R', site, 1);
    false
}

/// Literal-set observation bits (`S` trace record): bits 0..8 mark matched element indices.
pub const LITSET_REACHED: u16 = 256;
pub const LITSET_OPAQUE: u16 = 512;

/// Built-in primitive element types with deterministic, side-effect-free equality for
/// `[lit1, lit2, ...].contains(&x)` targeted probing.
pub trait LitSetElem: PartialEq {}

macro_rules! tmt_litset_elems {
    ($($t:ty)*) => {$(
        impl LitSetElem for $t {}
    )*};
}
tmt_litset_elems!(
    i8 i16 i32 i64 i128 isize
    u8 u16 u32 u64 u128 usize
    f32 f64 bool char &str &[u8] &std::ffi::CStr
);
impl<const M: usize> LitSetElem for &[u8; M] {}

pub struct Lst<'a, A: ?Sized, X>(Cell<Option<(&'a A, X)>>);

impl<'a, A: ?Sized, X> Lst<'a, A, X> {
    #[inline(always)]
    pub fn new(a: &'a A, x: X) -> Self {
        Lst(Cell::new(Some((a, x))))
    }

    #[inline(always)]
    fn take(&self) -> (&'a A, X) {
        self.0.take().expect("operands are taken once")
    }
}

pub trait LitSetPrim<'a, A: ?Sized + 'a, X> {
    fn tmt_lit_set<F: FnOnce(&'a A, X) -> bool>(&self, site: u32, orig: F) -> bool;
}

pub trait AnyLitSet<'a, A: ?Sized + 'a, X> {
    fn tmt_lit_set<R, F: FnOnce(&'a A, X) -> R>(&self, site: u32, orig: F) -> R;
}

#[inline(always)]
fn eval_lit_set<T: LitSetElem>(
    site: u32,
    slice: &[T],
    target: &T,
    orig: impl FnOnce() -> bool,
) -> bool {
    if idle(site) {
        return orig();
    }
    if let Some(Mutant::DropLiteralElem(drop_idx)) = mutant_at(site) {
        return slice
            .iter()
            .enumerate()
            .any(|(i, elem)| i != drop_idx as usize && elem == target);
    }
    let matched = slice.iter().position(|elem| elem == target);
    let actual = orig();
    if actual != matched.is_some() {
        // A trait in scope overrode `.contains(...)` with non-slice semantics.
        record_bits_u16(b'S', site, LITSET_OPAQUE);
        return actual;
    }
    match matched {
        Some(idx) if idx < 8 => {
            record_bits_u16(b'S', site, LITSET_REACHED | (1u16 << idx));
        }
        _ => {
            record_bits_u16(b'S', site, LITSET_REACHED);
        }
    }
    actual
}

impl<'a, 'b, const N: usize, T: LitSetElem> LitSetPrim<'a, [T; N], &'b T>
    for Lst<'a, [T; N], &'b T>
{
    #[inline(always)]
    fn tmt_lit_set<F: FnOnce(&'a [T; N], &'b T) -> bool>(&self, site: u32, orig: F) -> bool {
        let (a, x) = self.take();
        eval_lit_set(site, a.as_slice(), x, || orig(a, x))
    }
}

impl<'a, 'b, 'c, const N: usize, T: LitSetElem> LitSetPrim<'a, &'c [T; N], &'b T>
    for Lst<'a, &'c [T; N], &'b T>
{
    #[inline(always)]
    fn tmt_lit_set<F: FnOnce(&'a &'c [T; N], &'b T) -> bool>(&self, site: u32, orig: F) -> bool {
        let (a, x) = self.take();
        eval_lit_set(site, (*a).as_slice(), x, || orig(a, x))
    }
}

impl<'a, 'b, const N: usize, T: LitSetElem> LitSetPrim<'a, [T; N], &'b mut T>
    for Lst<'a, [T; N], &'b mut T>
{
    #[inline(always)]
    fn tmt_lit_set<F: FnOnce(&'a [T; N], &'b mut T) -> bool>(&self, site: u32, orig: F) -> bool {
        let (a, x) = self.take();
        if idle(site) {
            return orig(a, x);
        }
        if let Some(Mutant::DropLiteralElem(drop_idx)) = mutant_at(site) {
            return a
                .as_slice()
                .iter()
                .enumerate()
                .any(|(i, elem)| i != drop_idx as usize && elem == &*x);
        }
        let matched = a.as_slice().iter().position(|elem| elem == &*x);
        let actual = orig(a, x);
        if actual != matched.is_some() {
            record_bits_u16(b'S', site, LITSET_OPAQUE);
            return actual;
        }
        match matched {
            Some(idx) if idx < 8 => {
                record_bits_u16(b'S', site, LITSET_REACHED | (1u16 << idx));
            }
            _ => {
                record_bits_u16(b'S', site, LITSET_REACHED);
            }
        }
        actual
    }
}

impl<'a, 'b, 'c, const N: usize, T: LitSetElem> LitSetPrim<'a, &'c [T; N], &'b mut T>
    for Lst<'a, &'c [T; N], &'b mut T>
{
    #[inline(always)]
    fn tmt_lit_set<F: FnOnce(&'a &'c [T; N], &'b mut T) -> bool>(
        &self,
        site: u32,
        orig: F,
    ) -> bool {
        let (a, x) = self.take();
        if idle(site) {
            return orig(a, x);
        }
        if let Some(Mutant::DropLiteralElem(drop_idx)) = mutant_at(site) {
            return (*a)
                .as_slice()
                .iter()
                .enumerate()
                .any(|(i, elem)| i != drop_idx as usize && elem == &*x);
        }
        let matched = (*a).as_slice().iter().position(|elem| elem == &*x);
        let actual = orig(a, x);
        if actual != matched.is_some() {
            record_bits_u16(b'S', site, LITSET_OPAQUE);
            return actual;
        }
        match matched {
            Some(idx) if idx < 8 => {
                record_bits_u16(b'S', site, LITSET_REACHED | (1u16 << idx));
            }
            _ => {
                record_bits_u16(b'S', site, LITSET_REACHED);
            }
        }
        actual
    }
}

impl<'r, 'a, A: ?Sized, X> AnyLitSet<'a, A, X> for &'r Lst<'a, A, X> {
    fn tmt_lit_set<R, F: FnOnce(&'a A, X) -> R>(&self, site: u32, orig: F) -> R {
        let (a, x) = self.take();
        if !idle(site) {
            record_bits_u16(b'S', site, LITSET_OPAQUE);
        }
        orig(a, x)
    }
}

/// `(a..b).contains(x)` <-> `(a..=b).contains(x)` on primitive ordered types.
pub trait RangeContainsElem: PartialOrd + PartialEq {}

macro_rules! tmt_range_contains_elems {
    ($($t:ty)*) => {$(
        impl RangeContainsElem for $t {}
    )*};
}
tmt_range_contains_elems!(
    i8 i16 i32 i64 i128 isize
    u8 u16 u32 u64 u128 usize
    f32 f64 char
);

pub struct Rcn<S, E, X>(Cell<Option<(S, E, X)>>);

impl<S, E, X> Rcn<S, E, X> {
    #[inline(always)]
    pub fn new(s: S, e: E, x: X) -> Self {
        Rcn(Cell::new(Some((s, e, x))))
    }

    #[inline(always)]
    fn take(&self) -> (S, E, X) {
        self.0.take().expect("range contains operands taken once")
    }
}

pub trait RangeContainsPrim<S, E, X> {
    fn tmt_range_contains<F: FnOnce(S, E, X) -> bool>(
        &self,
        site: u32,
        inclusive: bool,
        orig: F,
    ) -> bool;
}

pub trait AnyRangeContains<S, E, X> {
    fn tmt_range_contains<R, F: FnOnce(S, E, X) -> R>(
        &self,
        site: u32,
        inclusive: bool,
        orig: F,
    ) -> R;
}

impl<'b, T: RangeContainsElem + Copy> RangeContainsPrim<T, T, &'b T> for Rcn<T, T, &'b T> {
    #[inline(always)]
    fn tmt_range_contains<F: FnOnce(T, T, &'b T) -> bool>(
        &self,
        site: u32,
        inclusive: bool,
        orig: F,
    ) -> bool {
        let (s, e, x) = self.take();
        if idle(site) {
            return orig(s, e, x);
        }
        let mutated = if inclusive {
            (s..e).contains(x)
        } else {
            (s..=e).contains(x)
        };
        if mutant_at(site) == Some(Mutant::RangeEnd) {
            return mutated;
        }
        let actual = orig(s, e, x);
        if !bits_known(b'A', site, ARITH_SAME | ARITH_DIFFERS) {
            record_bits(
                b'A',
                site,
                if actual == mutated {
                    ARITH_SAME
                } else {
                    ARITH_DIFFERS
                },
            );
        }
        actual
    }
}

impl<'r, S, E, X> AnyRangeContains<S, E, X> for &'r Rcn<S, E, X> {
    fn tmt_range_contains<R, F: FnOnce(S, E, X) -> R>(
        &self,
        site: u32,
        _inclusive: bool,
        orig: F,
    ) -> R {
        let (s, e, x) = self.take();
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        orig(s, e, x)
    }
}

/// `iter.position(f)` <-> `iter.rposition(f)`: reach-guided probe specialized to
/// `DoubleEndedIterator + ExactSizeIterator`.
pub trait PosPrim<'a, I: ?Sized + 'a> {
    type Item;
    fn tmt_pos<P: FnMut(Self::Item) -> bool, G: FnOnce(&'a mut I, P) -> Option<usize>>(
        &self,
        site: u32,
        forward: bool,
        pred: P,
        orig: G,
    ) -> Option<usize>;
}

pub trait AnyPos<'a, I: ?Sized + 'a> {
    fn tmt_pos<P, R, G: FnOnce(&'a mut I, P) -> R>(
        &self,
        site: u32,
        forward: bool,
        pred: P,
        orig: G,
    ) -> R;
}

impl<'a, I: DoubleEndedIterator + ExactSizeIterator> PosPrim<'a, I> for Qnt<'a, I> {
    type Item = I::Item;
    #[inline(always)]
    fn tmt_pos<P: FnMut(I::Item) -> bool, G: FnOnce(&'a mut I, P) -> Option<usize>>(
        &self,
        site: u32,
        forward: bool,
        pred: P,
        orig: G,
    ) -> Option<usize> {
        let iter = self.take();
        if idle(site) {
            return orig(iter, pred);
        }
        if mutant_at(site) == Some(Mutant::PositionSwap) {
            return if forward {
                iter.rposition(pred)
            } else {
                iter.position(pred)
            };
        }
        record_bits(b'R', site, 1);
        orig(iter, pred)
    }
}

impl<'r, 'a, I: ?Sized> AnyPos<'a, I> for &'r Qnt<'a, I> {
    fn tmt_pos<P, R, G: FnOnce(&'a mut I, P) -> R>(
        &self,
        site: u32,
        _forward: bool,
        pred: P,
        orig: G,
    ) -> R {
        let iter = self.take();
        if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
        orig(iter, pred)
    }
}
