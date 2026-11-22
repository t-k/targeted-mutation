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
    /// Bits already written per (kind, site, test) for statement, logic and arithmetic sites.
    bits: HashMap<(u8, u32, u32), u8>,
    typed: HashSet<u32>,
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
    match MODE.load(Atomic::Relaxed) {
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
        MODE.store(mode_of(&state), Atomic::Relaxed);
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
    static SEEN_BITS: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static SEEN_CMP: RefCell<Vec<Option<Summary>>> = const { RefCell::new(Vec::new()) };
}

fn bits_slot(kind: u8, site: u32) -> usize {
    let kind = match kind {
        b'R' => 0,
        b'L' => 1,
        b'A' => 2,
        _ => 3,
    };
    site as usize * 4 + kind
}

/// Whether this thread has recorded all of `bits` at the site before.
pub fn bits_known(kind: u8, site: u32, bits: u8) -> bool {
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
    for shelf in &t.shelves {
        for (site, summary) in locked(&shelf.backlog).drain() {
            let line = summary.line(site, &t.names[shelf.test as usize]);
            write_line(&mut t.out, &line);
        }
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

/// Records observation bits of a statement (`R`), logic (`L`) or arithmetic (`A`) site.
fn record_bits(kind: u8, site: u32, bits: u8) {
    if state().tracer.is_none() || !bits_are_news(kind, site, bits) || tracer().is_none() {
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
        return match op {
            LT => a < b,
            LE => a <= b,
            GT => a > b,
            _ => a >= b,
        };
    }
    ord_busy(site, op, a, b, av, bv)
}

#[inline(never)]
fn ord_busy<A: ?Sized + PartialOrd<B>, B: ?Sized>(
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
    let orig = if op == EQ { a == b } else { a != b };
    if idle(site) {
        return orig;
    }
    eq_busy(site, op, orig, av, bv)
}

#[inline(never)]
fn eq_busy(site: u32, op: u8, orig: bool, av: Val, bv: Val) -> bool {
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
                let amount = u32::try_from(b).ok();
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
                    if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
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
                    if !idle(site) {
            record_bits(b'A', site, ARITH_OPAQUE);
        }
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
    fn tmt_not(&self, site: u32) -> bool {
        let value = self.0.take().expect("the operand is taken once");
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
            fn tmt_range(&self, site: u32) -> std::ops::Range<$t> {
                let (a, b) = self.take();
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
            fn tmt_range_inclusive(&self, site: u32) -> std::ops::RangeInclusive<$t> {
                let (a, b) = self.take();
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

macro_rules! tmt_min_max {
    ($($t:ty)*) => {$(
        impl<'a> MinMaxPrim for Mm<'a, $t, $t> {
            fn tmt_other(&self, site: u32) -> bool {
                if mutant_at(site) == Some(Mutant::MinMax) {
                    return true;
                }
                record_bits(b'A', site, if self.0 == self.1 { ARITH_SAME } else { ARITH_DIFFERS });
                false
            }
        }
    )*};
}
tmt_min_max!(i8 i16 i32 i64 isize u8 u16 u32 u64 usize f32 f64);

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
