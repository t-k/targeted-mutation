//! The planner: decides, from what the tests observed at a comparison, which mutations
//! are guaranteed to survive (input gaps) and which ones the tests must be run against.
//!
//! Everything here is pure and works on `D = lhs - rhs`. Integer comparisons are threshold
//! decisions on `D`, and the trace keeps only the observations nearest to zero. The proofs
//! that this summary is enough, and that the survivor range is exact, are in
//! `formal/lean/Planner.lean`.

use serde::Serialize;

/// Outcome of `partial_cmp`, as a bit in [`Obs`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rel {
    Less,
    Equal,
    Greater,
    Unordered,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

impl CmpOp {
    pub const ALL: [CmpOp; 6] = [
        CmpOp::Lt,
        CmpOp::Le,
        CmpOp::Gt,
        CmpOp::Ge,
        CmpOp::Eq,
        CmpOp::Ne,
    ];

    pub fn code(self) -> u8 {
        match self {
            CmpOp::Lt => 0,
            CmpOp::Le => 1,
            CmpOp::Gt => 2,
            CmpOp::Ge => 3,
            CmpOp::Eq => 4,
            CmpOp::Ne => 5,
        }
    }

    pub fn from_code(code: u8) -> Option<CmpOp> {
        CmpOp::ALL.into_iter().find(|op| op.code() == code)
    }

    pub fn symbol(self) -> &'static str {
        match self {
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
        }
    }

    pub fn is_ordering(self) -> bool {
        !matches!(self, CmpOp::Eq | CmpOp::Ne)
    }

    pub fn eval_rel(self, rel: Rel) -> bool {
        match (self, rel) {
            (CmpOp::Ne, Rel::Unordered) => true,
            (_, Rel::Unordered) => false,
            (CmpOp::Lt, r) => r == Rel::Less,
            (CmpOp::Le, r) => r != Rel::Greater,
            (CmpOp::Gt, r) => r == Rel::Greater,
            (CmpOp::Ge, r) => r != Rel::Less,
            (CmpOp::Eq, r) => r == Rel::Equal,
            (CmpOp::Ne, r) => r != Rel::Equal,
        }
    }

    /// The operator applied to integer operands whose difference is `d`.
    pub fn eval_d(self, d: i128) -> bool {
        self.eval_rel(match d {
            d if d < 0 => Rel::Less,
            0 => Rel::Equal,
            _ => Rel::Greater,
        })
    }

    /// `<` and `>` as each other and as `==`.
    pub fn mistaken_for(self) -> &'static [CmpOp] {
        match self {
            CmpOp::Lt => &[CmpOp::Eq, CmpOp::Gt],
            CmpOp::Gt => &[CmpOp::Eq, CmpOp::Lt],
            _ => &[],
        }
    }

    pub fn negated(self) -> CmpOp {
        match self {
            CmpOp::Lt => CmpOp::Ge,
            CmpOp::Le => CmpOp::Gt,
            CmpOp::Gt => CmpOp::Le,
            CmpOp::Ge => CmpOp::Lt,
            CmpOp::Eq => CmpOp::Ne,
            CmpOp::Ne => CmpOp::Eq,
        }
    }

    /// `<` <-> `<=`, `>` <-> `>=`: the classic off-by-one.
    pub fn flip_eq(self) -> Option<CmpOp> {
        match self {
            CmpOp::Lt => Some(CmpOp::Le),
            CmpOp::Le => Some(CmpOp::Lt),
            CmpOp::Gt => Some(CmpOp::Ge),
            CmpOp::Ge => Some(CmpOp::Gt),
            CmpOp::Eq | CmpOp::Ne => None,
        }
    }

    /// Threshold form over integers: `(up, t0)`. Up family is true iff `D >= t0`,
    /// down family is true iff `D <= t0`.
    fn threshold(self) -> Option<(bool, i128)> {
        match self {
            CmpOp::Ge => Some((true, 0)),
            CmpOp::Gt => Some((true, 1)),
            CmpOp::Le => Some((false, 0)),
            CmpOp::Lt => Some((false, -1)),
            CmpOp::Eq | CmpOp::Ne => None,
        }
    }
}

/// What the tests observed at one comparison, summarised. Merging is commutative,
/// associative and idempotent, so trace lines can be folded in any order.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Obs {
    pub lt: bool,
    pub eq: bool,
    pub gt: bool,
    pub un: bool,
    /// Some observation did not carry integer values, so `max_neg`/`min_pos` are incomplete.
    pub unvalued: bool,
    /// Largest observed `D < 0`.
    pub max_neg: Option<i128>,
    /// Smallest observed `D > 0`.
    pub min_pos: Option<i128>,
    /// Some observation did not carry float values, so the two fields below are incomplete.
    pub unfloat: bool,
    /// Largest observed negative and smallest observed positive float difference.
    pub f_max_neg: Option<f64>,
    pub f_min_pos: Option<f64>,
}

impl Obs {
    pub fn reached(&self) -> bool {
        self.lt || self.eq || self.gt || self.un
    }

    pub fn valued(&self) -> bool {
        self.reached() && !self.unvalued
    }

    pub fn merge(&mut self, other: &Obs) {
        self.lt |= other.lt;
        self.eq |= other.eq;
        self.gt |= other.gt;
        self.un |= other.un;
        self.unvalued |= other.unvalued;
        self.max_neg = match (self.max_neg, other.max_neg) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        self.min_pos = match (self.min_pos, other.min_pos) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.unfloat |= other.unfloat;
        self.f_max_neg = match (self.f_max_neg, other.f_max_neg) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        self.f_min_pos = match (self.f_min_pos, other.f_min_pos) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }

    /// Summary of a single observation of integer operands with difference `d`.
    pub fn of_d(d: i128) -> Obs {
        Obs {
            lt: d < 0,
            eq: d == 0,
            gt: d > 0,
            max_neg: (d < 0).then_some(d),
            min_pos: (d > 0).then_some(d),
            unfloat: true,
            ..Obs::default()
        }
    }

    /// Summary of a single observation without integer values.
    pub fn of_rel(rel: Rel) -> Obs {
        Obs {
            lt: rel == Rel::Less,
            eq: rel == Rel::Equal,
            gt: rel == Rel::Greater,
            un: rel == Rel::Unordered,
            unvalued: true,
            unfloat: true,
            ..Obs::default()
        }
    }

    /// Summary of a single observation of float operands with difference `d`.
    pub fn of_f(d: f64) -> Obs {
        Obs {
            lt: d < 0.0,
            eq: d == 0.0,
            gt: d > 0.0,
            unvalued: true,
            f_max_neg: (d < 0.0).then_some(d),
            f_min_pos: (d > 0.0).then_some(d),
            ..Obs::default()
        }
    }

    /// The gap around zero that no float observation fell into, when every observation
    /// was a float and both sides were seen.
    pub fn float_gap(&self) -> Option<(f64, f64)> {
        // Nothing reached leaves both sides of the gap unset.
        if self.unfloat || self.un {
            return None;
        }
        Some((self.f_max_neg?, self.f_min_pos?))
    }

    fn rels(&self) -> impl Iterator<Item = Rel> {
        [
            (self.lt, Rel::Less),
            (self.eq, Rel::Equal),
            (self.gt, Rel::Greater),
            (self.un, Rel::Unordered),
        ]
        .into_iter()
        .filter_map(|(seen, rel)| seen.then_some(rel))
    }

    /// Whether `d` itself was observed. Exact only for `d` in `-1..=1` on valued sites.
    fn saw(&self, d: i128) -> bool {
        match d {
            0 => self.eq,
            -1 => self.max_neg == Some(-1),
            1 => self.min_pos == Some(1),
            _ => unreachable!("only the values next to zero can be answered from the summary"),
        }
    }

    /// Largest observation `<= x`, for `x` in `{-1, 0}`.
    fn max_le(&self, x: i128) -> Option<i128> {
        debug_assert!(x == -1 || x == 0);
        if x == 0 && self.eq {
            Some(0)
        } else {
            self.max_neg
        }
    }

    /// Smallest observation `>= x`, for `x` in `{0, 1}`.
    fn min_ge(&self, x: i128) -> Option<i128> {
        debug_assert!(x == 0 || x == 1);
        if x == 0 && self.eq {
            Some(0)
        } else {
            self.min_pos
        }
    }

    /// Whether any observation lies in `lo..=hi`. The interval must touch `-1..=1`
    /// from the inside (no gap between it and zero), which is all the planner needs.
    fn saw_any_in(&self, lo: i128, hi: i128) -> bool {
        if lo > hi {
            return false;
        }
        debug_assert!(
            lo <= 1 && hi >= -1,
            "interval {lo}..={hi} is not anchored at zero"
        );
        let zero = self.eq && lo <= 0 && 0 <= hi;
        let neg = lo <= -1 && self.max_neg.is_some_and(|m| m >= lo);
        let pos = hi >= 1 && self.min_pos.is_some_and(|p| p <= hi);
        zero || neg || pos
    }
}

#[derive(Clone, Copy, PartialEq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mutation {
    /// Replace the comparison by a constant.
    Const(bool),
    /// Negate the comparison.
    Negate,
    /// Replace the operator.
    Op(CmpOp),
    /// `lhs op rhs + d` (integer operands only).
    Shift(i128),
    /// `lhs op rhs + d` for float operands.
    ShiftF(f64),
    /// Delete a statement.
    Delete,
    /// `&&` <-> `||`.
    LogicSwap,
    /// Ignore the value of the right operand of `&&` / `||` (`a && true`, `a || false`).
    LogicDropRhs,
    /// Ignore the value of the left operand of `&&` / `||`.
    LogicDropLhs,
    /// `+` <-> `-`, `*` <-> `/`, `%` -> `/`, `&` <-> `|`, `^` -> `&`, `<<` <-> `>>`.
    ArithSwap,
    /// The second replacement of an operator: `+` -> `*`, `-` -> `/`, `*` -> `+`,
    /// `/` -> `%`, `%` -> `+`, `&` -> `^`, `|` -> `^`, `^` -> `|`.
    ArithSwap2,
    /// `!x` becomes `x`.
    DropNot,
    /// `a..b` becomes `a..=b` and the other way round.
    RangeEnd,
    /// `a.max(b)` becomes `a.min(b)` and the other way round.
    MinMax,
}

impl Mutation {
    /// Wire format shared with the injected runtime (`TMT_MUTANT=<site>:<encoded>`).
    pub fn encode(&self) -> String {
        match self {
            Mutation::Const(c) => format!("c{}", *c as u8),
            Mutation::Negate => "neg".to_string(),
            Mutation::Op(op) => format!("op{}", op.code()),
            Mutation::Shift(d) => format!("sh{d}"),
            Mutation::ShiftF(d) => format!("shf{d:?}"),
            Mutation::Delete => "del".to_string(),
            Mutation::LogicSwap => "lsw".to_string(),
            Mutation::LogicDropRhs => "ldr".to_string(),
            Mutation::LogicDropLhs => "ldl".to_string(),
            Mutation::ArithSwap => "asw".to_string(),
            Mutation::ArithSwap2 => "as2".to_string(),
            Mutation::DropNot => "unn".to_string(),
            Mutation::RangeEnd => "rng".to_string(),
            Mutation::MinMax => "mmx".to_string(),
        }
    }
}

/// Result of the mutated comparison on integer operands with difference `d`.
pub fn eval_mutated_d(op: CmpOp, m: Mutation, d: i128) -> bool {
    match m {
        Mutation::Const(c) => c,
        Mutation::Negate => !op.eval_d(d),
        Mutation::Op(op2) => op2.eval_d(d),
        Mutation::Shift(s) => op.eval_d(d - s),
        _ => op.eval_d(d),
    }
}

/// Whether some observation in `obs` gets a different result under mutation `m`.
/// A test that is not infected executes the mutant exactly like the original.
pub fn infects(op: CmpOp, m: Mutation, obs: &Obs) -> bool {
    match m {
        Mutation::Const(c) => obs.rels().any(|r| op.eval_rel(r) != c),
        Mutation::Op(op2) => obs.rels().any(|r| op.eval_rel(r) != op2.eval_rel(r)),
        Mutation::Shift(s) => {
            let Some((lo, hi)) = shift_diff(op, s) else {
                return false;
            };
            if !obs.valued() {
                // Without values we cannot rule the difference out.
                return obs.reached();
            }
            obs.saw_any_in(lo, hi)
        }
        // A float shift is only ever proposed inside the unobserved gap.
        Mutation::ShiftF(s) => match obs.float_gap() {
            Some((lo, hi)) => !(lo < s && s < hi) || obs.eq,
            None => obs.reached(),
        },
        // Negation, and what is not a mutation of a comparison: every execution differs.
        _ => obs.reached(),
    }
}

/// The values of `D` on which `lhs op rhs` and `lhs op rhs + s` disagree.
fn shift_diff(op: CmpOp, s: i128) -> Option<(i128, i128)> {
    let (up, t0) = op.threshold()?;
    let t = t0 + s;
    let (a, b) = (t0.min(t), t0.max(t));
    // Up: true iff D >= threshold, so the two disagree on [a, b - 1].
    // Down: true iff D <= threshold, so they disagree on [a + 1, b].
    Some(if up { (a, b - 1) } else { (a + 1, b) })
}

/// A hole in the test inputs at one comparison: mutations that no test can tell apart
/// from the original, because no test ever feeds the comparison a distinguishing value.
#[derive(Clone, PartialEq, Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum InputGap {
    /// No test executes the comparison.
    Unreached,
    /// The comparison has the same result in every test; `Const(always)` survives.
    OneSided { always: bool },
    /// Every `Shift(d)` with `min_shift <= d <= max_shift`, `d != 0`, survives.
    /// `missing` lists the unobserved values of `D` next to the boundary that would pin it.
    Boundary {
        min_shift: i128,
        max_shift: i128,
        missing: Vec<i128>,
        /// The shifts from the first to the second of these answer like the original
        /// for every value the expression can be (see [`within_reach`]): they are no
        /// mutants. `(0, 0)` when it can be every number.
        #[serde(skip_serializing_if = "no_shift")]
        same: (i128, i128),
    },
    /// The operands were never equal (and carry no integer values); `Op(flipped)` survives.
    /// For floats, `float_gap` is the range of differences no test observed: every
    /// `ShiftF(d)` strictly inside it survives too.
    EqualNeverObserved {
        flipped: CmpOp,
        float_gap: Option<(f64, f64)>,
    },
}

fn no_shift(range: &(i128, i128)) -> bool {
    *range == (0, 0)
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct CmpPlan {
    /// The comparison had one outcome in every test and its operand types allow no other
    /// (`x >= 0` for unsigned `x`): whatever encloses it is constant for the same reason.
    pub cannot_differ: bool,
    pub gap: Option<InputGap>,
    /// Mutations that change the outcome of at least one test's observation. Only running
    /// the tests can tell whether an assertion notices.
    pub run: Vec<Mutation>,
}

impl InputGap {
    /// One concrete mutation that the gap lets through, nearest to the original.
    pub fn representative(&self) -> Option<Mutation> {
        match self {
            InputGap::Unreached => None,
            InputGap::OneSided { always } => Some(Mutation::Const(*always)),
            InputGap::Boundary {
                min_shift,
                max_shift,
                same,
                ..
            } => {
                // The nearest shift that is a mutant.
                if *max_shift > same.1 {
                    Some(Mutation::Shift(same.1 + 1))
                } else if *min_shift < same.0 {
                    Some(Mutation::Shift(same.0 - 1))
                } else {
                    None
                }
            }
            InputGap::EqualNeverObserved { flipped, .. } => Some(Mutation::Op(*flipped)),
        }
    }

    /// Up to three concrete mutations the gap lets through, nearest to the original first.
    pub fn survivors(&self) -> Vec<Mutation> {
        match self {
            InputGap::Boundary {
                min_shift,
                max_shift,
                same,
                ..
            } => (1..=3)
                .flat_map(|n| [same.1 + n, same.0 - n])
                .filter(|s| min_shift <= s && s <= max_shift)
                .take(3)
                .map(Mutation::Shift)
                .collect(),
            InputGap::EqualNeverObserved { flipped, float_gap } => {
                let mut all = vec![Mutation::Op(*flipped)];
                if let Some((lo, hi)) = float_gap {
                    all.extend([Mutation::ShiftF(hi / 2.0), Mutation::ShiftF(lo / 2.0)]);
                }
                all
            }
            other => other.representative().into_iter().collect(),
        }
    }
}

/// Plans one comparison site. `domain` is the range of `D` the operand types allow, when
/// known. It only matters for a condition with a single observed outcome: if the types
/// rule the other outcome out (`x >= 0` for unsigned `x`), there is nothing to report.
pub fn plan_cmp(op: CmpOp, obs: &Obs, domain: Option<(i128, i128)>) -> CmpPlan {
    let mut plan = plan_boundary(op, obs, domain);
    // The other operators a `<` or `>` is commonly mistaken for (the replacements
    // cargo-mutants makes). One that answers like the original for every relation the
    // tests saw runs like the original from the first step to the last and passes them
    // all, which the gap already says. Every other one is run, also where it answers
    // like another mutant for everything that was seen: once a mutant has changed the
    // way the code goes, the comparison meets values the tests never gave it, and there
    // the two may part.
    let answers =
        |other: CmpOp| -> Vec<bool> { obs.rels().map(|rel| other.eval_rel(rel)).collect() };
    for other in op.mistaken_for() {
        if answers(*other) != answers(op) {
            plan.run.push(Mutation::Op(*other));
        }
    }
    // `>` for `<=` and `<` for `>=` are the negation, except for operands that have no
    // order (NaN): there the negation is true and the other operator false. Only
    // integers are known to always have an order. For anything else the two are
    // different mutants and both are run, whether or not the tests gave the comparison
    // such operands: a mutant that changes the way the code goes can be the first to.
    if matches!(op, CmpOp::Le | CmpOp::Ge) && !obs.valued() && answers(op.negated()) != answers(op)
    {
        plan.run.push(Mutation::Op(op.negated()));
    }
    plan
}

fn plan_boundary(op: CmpOp, obs: &Obs, domain: Option<(i128, i128)>) -> CmpPlan {
    if !obs.reached() {
        return CmpPlan {
            cannot_differ: false,
            gap: Some(InputGap::Unreached),
            run: vec![],
        };
    }
    let saw_true = obs.rels().any(|r| op.eval_rel(r));
    let saw_false = obs.rels().any(|r| !op.eval_rel(r));
    if !(saw_true && saw_false) {
        let always = saw_true;
        // Can the other outcome happen at all? Only checkable on integers with known types.
        let other_possible = match domain {
            // The domain is an interval that contains the observations, so if the other
            // outcome is possible at all, it is possible next to zero.
            Some((lo, hi)) if obs.valued() => [-1, 0, 1]
                .into_iter()
                .any(|d| lo <= d && d <= hi && op.eval_d(d) != always),
            _ => true,
        };
        return CmpPlan {
            cannot_differ: !other_possible,
            gap: other_possible.then_some(InputGap::OneSided { always }),
            run: vec![Mutation::Negate],
        };
    }

    let Some((up, t0)) = op.threshold() else {
        // `==` / `!=` with both outcomes observed: nothing to shift.
        return CmpPlan {
            cannot_differ: false,
            gap: None,
            run: vec![Mutation::Negate],
        };
    };

    if !obs.valued() {
        let flipped = op
            .flip_eq()
            .expect("ordering operators have a flipped form");
        return if obs.eq {
            CmpPlan {
                cannot_differ: false,
                gap: None,
                run: vec![Mutation::Op(flipped), Mutation::Negate],
            }
        } else {
            CmpPlan {
                cannot_differ: false,
                gap: Some(InputGap::EqualNeverObserved {
                    flipped,
                    float_gap: obs.float_gap(),
                }),
                run: vec![Mutation::Negate],
            }
        };
    }

    // Nearest observations on each side of the boundary, and the two values of D that
    // straddle it: `w_minus` separates the original from Shift(-1), `w_plus` from Shift(+1).
    let (below, above, w_minus, w_plus) = if up {
        (obs.max_le(t0 - 1), obs.min_ge(t0), t0 - 1, t0)
    } else {
        (obs.max_le(t0), obs.min_ge(t0 + 1), t0, t0 + 1)
    };
    let below = below.expect("both outcomes were observed");
    let above = above.expect("both outcomes were observed");
    let (min_shift, max_shift) = if up {
        (below + 1 - t0, above - t0)
    } else {
        (below - t0, above - 1 - t0)
    };

    // Both outcomes were observed, so both `w_minus` and `w_plus` lie between two observed
    // values: they are inside the operand types' range and the mutants are not equivalent.
    let mut missing = vec![];
    let mut run = vec![];
    for (w, shift) in [(w_minus, -1), (w_plus, 1)] {
        if obs.saw(w) {
            run.push(Mutation::Shift(shift));
        } else {
            missing.push(w);
        }
    }
    run.push(Mutation::Negate);
    let gap = (!missing.is_empty()).then_some(InputGap::Boundary {
        min_shift,
        max_shift,
        missing,
        same: (0, 0),
    });
    CmpPlan {
        cannot_differ: false,
        gap,
        run,
    }
}

/// What an integer expression can be whatever its inputs are, read from how it is
/// written. Each holds also when the arithmetic wraps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reach {
    /// A multiple of this power of two (`x * 8`, `x << 3`).
    Multiples(i128),
    /// Not negative, with no bit outside of this mask (`x & 0xF0`).
    Bits(i128),
    /// Smaller in size than this (`x % 10`).
    Below(i128),
}

impl Reach {
    pub fn holds(self, value: i128) -> bool {
        match self {
            Reach::Multiples(step) => value % step == 0,
            Reach::Bits(mask) => value >= 0 && value & !mask == 0,
            Reach::Below(limit) => -limit < value && value < limit,
        }
    }

    /// The least value above `value` that the expression can be.
    fn next_above(self, value: i128) -> Option<i128> {
        match self {
            Reach::Multiples(step) => value.div_euclid(step).checked_add(1)?.checked_mul(step),
            Reach::Bits(_) if value < 0 => Some(0),
            Reach::Bits(mask) => {
                let over = value.checked_add(1)?;
                let outside = over & !mask;
                if outside == 0 {
                    return Some(over);
                }
                // The bits from the highest one that is not of the mask down cannot
                // stay: fill them and what else is not of the mask, so that adding one
                // carries into the next bit of the mask above.
                let top = 127 - outside.leading_zeros();
                let low = 1_i128.checked_shl(top + 1)?.checked_sub(1)?;
                let next = (over | !mask | low).wrapping_add(1) & mask;
                (next > value).then_some(next)
            }
            Reach::Below(limit) => {
                let next = value.checked_add(1)?.max(1 - limit);
                (next < limit).then_some(next)
            }
        }
    }

    /// The greatest value below `value` that the expression can be.
    fn next_below(self, value: i128) -> Option<i128> {
        match self {
            Reach::Multiples(step) => value.checked_sub(1)?.div_euclid(step).checked_mul(step),
            Reach::Bits(_) if value <= 0 => None,
            Reach::Bits(mask) => {
                let under = value - 1;
                let outside = under & !mask;
                Some(if outside == 0 {
                    under
                } else {
                    // Below the highest bit that is not of the mask, all of the mask.
                    let top = 127 - outside.leading_zeros();
                    let low = (1_i128 << top) - 1;
                    (under & !low & !(1_i128 << top)) | (mask & low)
                })
            }
            Reach::Below(limit) => {
                let next = value.checked_sub(1)?.min(limit - 1);
                (next > -limit).then_some(next)
            }
        }
    }
}

/// The boundary gap of `expr op literal` (or `literal op expr`), given what the
/// expression can be at all. `None` when no value it can be lies between the values
/// the tests gave it: every shifted threshold then answers like the original, always.
/// Otherwise the values to add are ones it can be, and a side that has none is left out.
pub fn within_reach(
    gap: InputGap,
    op: CmpOp,
    obs: &Obs,
    reach: Reach,
    literal: i128,
    literal_is_rhs: bool,
) -> Option<InputGap> {
    let InputGap::Boundary {
        min_shift,
        max_shift,
        missing,
        same,
    } = gap
    else {
        return Some(gap);
    };
    let Some((up, t0)) = op.threshold() else {
        return Some(InputGap::Boundary {
            min_shift,
            max_shift,
            missing,
            same,
        });
    };
    // As in `plan_boundary`: the nearest differences seen on each side, and the two
    // next to the boundary.
    let (below, above, w_minus, w_plus) = if up {
        (obs.max_le(t0 - 1), obs.min_ge(t0), t0 - 1, t0)
    } else {
        (obs.max_le(t0), obs.min_ge(t0 + 1), t0, t0 + 1)
    };
    let (below, above) = (below?, above?);
    // The expression's value for a difference, and back.
    let value = |d: i128| {
        if literal_is_rhs {
            literal.checked_add(d)
        } else {
            literal.checked_sub(d)
        }
    };
    let difference = |v: i128| {
        if literal_is_rhs {
            v.checked_sub(literal)
        } else {
            literal.checked_sub(v)
        }
    };
    // The nearest difference the expression can make, from `start` towards `bound`
    // (which it stops short of).
    let nearest = |start: i128, bound: i128| -> Option<i128> {
        if (start < bound) != (start <= bound) || start == bound {
            return None;
        }
        let (from, to) = (value(start)?, value(bound)?);
        let found = if reach.holds(from) {
            from
        } else if to > from {
            reach.next_above(from)?
        } else {
            reach.next_below(from)?
        };
        let inside = if to > from { found < to } else { found > to };
        inside.then(|| difference(found)).flatten()
    };
    let down = nearest(w_minus, below);
    let up_side = nearest(w_plus, above);
    let missing: Vec<i128> = [(w_minus, down), (w_plus, up_side)]
        .into_iter()
        .filter(|(w, _)| missing.contains(w))
        .filter_map(|(_, nearest)| nearest)
        .collect();
    if missing.is_empty() {
        return None;
    }
    // A threshold moved by less than it takes to reach the nearest value the expression
    // can be, on either side, answers like the original for all of them. Where there is
    // no such value short of what the tests gave, that is the whole side.
    let (first_under, first_over) = (down.unwrap_or(below), up_side.unwrap_or(above));
    let same = if up {
        (first_under - t0 + 1, first_over - t0)
    } else {
        (first_under - t0, first_over - t0 - 1)
    };
    Some(InputGap::Boundary {
        min_shift,
        max_shift,
        missing,
        same: (same.0.max(min_shift).min(0), same.1.min(max_shift).max(0)),
    })
}

/// What the tests observed at `a && b` / `a || b`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LogicObs {
    /// The left operand decided alone (false for `&&`, true for `||`).
    pub short: bool,
    pub rhs_true: bool,
    pub rhs_false: bool,
    /// The right operand was started. Where it gave no value (a panic, a `?`, a
    /// `return`) this is all there is to see of the expression having been executed.
    pub rhs_started: bool,
}

impl LogicObs {
    pub fn from_bits(bits: u8) -> LogicObs {
        LogicObs {
            short: bits & 1 != 0,
            rhs_true: bits & 2 != 0,
            rhs_false: bits & 4 != 0,
            rhs_started: bits & 8 != 0,
        }
    }

    pub fn merge(&mut self, other: &LogicObs) {
        self.short |= other.short;
        self.rhs_true |= other.rhs_true;
        self.rhs_false |= other.rhs_false;
        self.rhs_started |= other.rhs_started;
    }

    pub fn reached(&self) -> bool {
        self.short || self.rhs_true || self.rhs_false || self.rhs_started
    }

    /// The value of the whole expression, if it was the same in every observation.
    pub fn constant_value(&self, and: bool) -> Option<bool> {
        // A short circuit yields false for `&&` and true for `||`.
        let saw_true = self.rhs_true || (self.short && !and);
        let saw_false = self.rhs_false || (self.short && and);
        match (saw_true, saw_false) {
            (true, false) => Some(true),
            (false, true) => Some(false),
            _ => None,
        }
    }

    /// The right operand was evaluated and its value decided the result.
    fn rhs_decided(&self, and: bool) -> bool {
        if and { self.rhs_false } else { self.rhs_true }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicGap {
    Unreached,
    /// No test has the right operand decide the result: `LogicDropRhs` survives.
    RhsNeverDecides,
    /// No test has the left operand decide the result: `LogicDropLhs` survives.
    LhsNeverDecides,
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct LogicPlan {
    pub gaps: Vec<LogicGap>,
    pub run: Vec<Mutation>,
}

/// Whether a test with these observations can execute differently under `m`.
/// Swapping the operator changes when the right operand is evaluated, so it is treated as
/// infecting every test that reaches the expression.
pub fn logic_infects(and: bool, m: Mutation, obs: &LogicObs) -> bool {
    match m {
        Mutation::LogicDropRhs => obs.rhs_decided(and),
        Mutation::LogicDropLhs => obs.short,
        _ => obs.reached(),
    }
}

pub fn plan_logic(and: bool, obs: &LogicObs) -> LogicPlan {
    if !obs.reached() {
        return LogicPlan {
            gaps: vec![LogicGap::Unreached],
            run: vec![],
        };
    }
    let mut plan = LogicPlan::default();
    for (m, gap) in [
        (Mutation::LogicDropRhs, LogicGap::RhsNeverDecides),
        (Mutation::LogicDropLhs, LogicGap::LhsNeverDecides),
    ] {
        if logic_infects(and, m, obs) {
            plan.run.push(m);
        } else {
            plan.gaps.push(gap);
        }
    }
    plan.run.push(Mutation::LogicSwap);
    plan
}

/// What the tests observed at an arithmetic operator on primitive numbers.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ArithObs {
    /// The swapped operator gave the same result.
    pub same: bool,
    /// The swapped operator gave a different result.
    pub differs: bool,
    /// The operands were not primitive numbers: nothing can be said.
    pub opaque: bool,
    /// The same for the operator's second replacement, where it has one.
    pub same2: bool,
    pub differs2: bool,
}

impl ArithObs {
    pub fn from_bits(bits: u8) -> ArithObs {
        ArithObs {
            same: bits & 1 != 0,
            differs: bits & 2 != 0,
            opaque: bits & 4 != 0,
            same2: bits & 8 != 0,
            differs2: bits & 16 != 0,
        }
    }

    pub fn merge(&mut self, other: &ArithObs) {
        self.same |= other.same;
        self.differs |= other.differs;
        self.opaque |= other.opaque;
        self.same2 |= other.same2;
        self.differs2 |= other.differs2;
    }

    /// The observations as they are for the second replacement.
    pub fn second(&self) -> ArithObs {
        ArithObs {
            same: self.same2,
            differs: self.differs2,
            opaque: self.opaque,
            same2: false,
            differs2: false,
        }
    }

    /// Whether mutation `m` computes another value in this test.
    pub fn infected_by(&self, m: Mutation) -> bool {
        if m == Mutation::ArithSwap2 {
            self.differs2
        } else {
            self.differs
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArithPlan {
    Unreached,
    /// Not primitive numbers: not mutated.
    Opaque,
    /// The swapped operator agrees on every observed pair of operands: it survives.
    NeverDiffers,
    /// Some test computes a different value: run `ArithSwap` against those tests.
    Run,
}

pub fn plan_arith(obs: &ArithObs) -> ArithPlan {
    if obs.differs {
        ArithPlan::Run
    } else if obs.same {
        ArithPlan::NeverDiffers
    } else if obs.opaque {
        ArithPlan::Opaque
    } else {
        ArithPlan::Unreached
    }
}

/// What a `-> bool` function returned at its normal exit.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ReturnObs {
    pub saw_true: bool,
    pub saw_false: bool,
}

impl ReturnObs {
    pub fn from_bits(bits: u8) -> ReturnObs {
        ReturnObs {
            saw_true: bits & 1 != 0,
            saw_false: bits & 2 != 0,
        }
    }

    pub fn merge(&mut self, other: &ReturnObs) {
        self.saw_true |= other.saw_true;
        self.saw_false |= other.saw_false;
    }

    pub fn reached(&self) -> bool {
        self.saw_true || self.saw_false
    }

    /// Whether replacing the returned value by `value` changes anything for this test.
    pub fn infected_by_const(&self, value: bool) -> bool {
        if value { self.saw_false } else { self.saw_true }
    }
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct ReturnPlan {
    pub unreached: bool,
    /// The function returned only this value: `Const(value)` survives.
    pub always: Option<bool>,
    pub run: Vec<Mutation>,
}

/// Plans a function returning `Option<T>`, where `saw_true` stands for `Some` and
/// `saw_false` for `None`. The result can be replaced by `None` but not by a `Some`.
pub fn plan_option_return(obs: &ReturnObs) -> ReturnPlan {
    match (obs.saw_true, obs.saw_false) {
        (false, false) => ReturnPlan {
            unreached: true,
            ..ReturnPlan::default()
        },
        // Never `Some`: always returning `None` passes every test.
        (false, true) => ReturnPlan {
            unreached: false,
            always: Some(false),
            run: vec![],
        },
        (true, _) => ReturnPlan {
            unreached: false,
            always: None,
            run: vec![Mutation::Const(false)],
        },
    }
}

pub fn plan_return(obs: &ReturnObs) -> ReturnPlan {
    match (obs.saw_true, obs.saw_false) {
        (false, false) => ReturnPlan {
            unreached: true,
            ..ReturnPlan::default()
        },
        (true, true) => ReturnPlan {
            run: vec![Mutation::Const(true), Mutation::Const(false)],
            ..ReturnPlan::default()
        },
        (always, _) => ReturnPlan {
            unreached: false,
            always: Some(always),
            run: vec![Mutation::Const(!always)],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REACHES: [Reach; 5] = [
        Reach::Multiples(4),
        Reach::Multiples(8),
        Reach::Bits(0b1010),
        Reach::Bits(0b0110),
        Reach::Below(5),
    ];

    #[test]
    fn the_next_value_an_expression_can_be() {
        for reach in REACHES {
            for value in -40..=40_i128 {
                let above = (value + 1..=80).find(|v| reach.holds(*v));
                let below = (-80..value).rev().find(|v| reach.holds(*v));
                assert_eq!(reach.next_above(value), above, "{reach:?} above {value}");
                assert_eq!(reach.next_below(value), below, "{reach:?} below {value}");
            }
        }
        // At the ends of the numbers there is no next one.
        assert_eq!(Reach::Multiples(8).next_above(i128::MAX), None);
        assert_eq!(Reach::Multiples(8).next_below(i128::MIN), None);
        assert_eq!(Reach::Below(5).next_above(i128::MAX), None);
        assert_eq!(Reach::Below(5).next_below(i128::MIN), None);
        assert_eq!(Reach::Bits(i128::MAX).next_above(i128::MAX), None);
        assert!(Reach::Bits(0xF0).holds(0x30) && !Reach::Bits(0xF0).holds(0x31));
        assert!(!Reach::Bits(0xF0).holds(-16));
    }

    /// Every pair of values the expression can be, one on each side of the boundary,
    /// as what the tests saw: the gap that is left is checked against all the values
    /// the expression can be.
    #[test]
    fn a_gap_is_kept_only_for_values_the_expression_can_be() {
        let mut dropped = 0;
        let mut kept = 0;
        for reach in REACHES {
            let can_be: Vec<i128> = (-24..=24).filter(|v| reach.holds(*v)).collect();
            for op in [CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge] {
                for literal in -6..=14_i128 {
                    for literal_is_rhs in [true, false] {
                        let d_of = |v: i128| {
                            if literal_is_rhs {
                                v - literal
                            } else {
                                literal - v
                            }
                        };
                        for &low in &can_be {
                            for &high in can_be.iter().filter(|v| **v > low) {
                                let mut obs = Obs::of_d(d_of(low));
                                obs.merge(&Obs::of_d(d_of(high)));
                                let Some(gap @ InputGap::Boundary { .. }) =
                                    plan_boundary(op, &obs, None).gap
                                else {
                                    continue;
                                };
                                let InputGap::Boundary {
                                    min_shift,
                                    max_shift,
                                    ..
                                } = gap.clone()
                                else {
                                    unreachable!()
                                };
                                // A threshold moved by `shift` answers `op` for the
                                // difference less the shift.
                                let differs = |shift: i128, v: i128| {
                                    op.eval_d(d_of(v)) != op.eval_d(d_of(v) - shift)
                                };
                                let told_apart =
                                    |shift: i128| can_be.iter().any(|v| differs(shift, *v));
                                let case = format!(
                                    "{reach:?} {op:?} literal {literal} rhs {literal_is_rhs}, saw {low} and {high}"
                                );
                                match within_reach(gap, op, &obs, reach, literal, literal_is_rhs) {
                                    None => {
                                        // Nothing it can be tells any of them apart.
                                        for shift in min_shift..=max_shift {
                                            assert!(!told_apart(shift), "{case}: shift {shift}");
                                        }
                                        dropped += 1;
                                    }
                                    Some(InputGap::Boundary {
                                        min_shift: least,
                                        max_shift: most,
                                        missing,
                                        same,
                                    }) => {
                                        assert!(!missing.is_empty(), "{case}");
                                        assert_eq!((least, most), (min_shift, max_shift), "{case}");
                                        for d in &missing {
                                            let v = if literal_is_rhs {
                                                literal + d
                                            } else {
                                                literal - d
                                            };
                                            assert!(reach.holds(v), "{case}: {v}");
                                            assert!(low < v && v < high, "{case}: {v}");
                                        }
                                        // The shifts called the same are exactly those that
                                        // nothing the expression can be tells apart.
                                        assert!(same.0 <= 0 && 0 <= same.1, "{case}: {same:?}");
                                        for shift in least..=most {
                                            assert_eq!(
                                                same.0 <= shift && shift <= same.1,
                                                !told_apart(shift),
                                                "{case}: shift {shift}, same {same:?}"
                                            );
                                        }
                                        // The values to add find out every other one of a
                                        // side that has a value to add.
                                        let gap = InputGap::Boundary {
                                            min_shift: least,
                                            max_shift: most,
                                            missing: missing.clone(),
                                            same,
                                        };
                                        for mutation in gap.survivors() {
                                            let Mutation::Shift(shift) = mutation else {
                                                panic!("{case}: {mutation:?}")
                                            };
                                            assert!(told_apart(shift), "{case}: shift {shift}");
                                        }
                                        assert!(gap.representative().is_some(), "{case}");
                                        kept += 1;
                                    }
                                    Some(other) => panic!("{case}: {other:?}"),
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(
            dropped > 100 && kept > 1000,
            "{dropped} dropped, {kept} kept"
        );
    }

    #[test]
    fn gaps_of_another_kind_are_not_touched() {
        let obs = Obs::of_d(3);
        for gap in [
            InputGap::Unreached,
            InputGap::OneSided { always: true },
            InputGap::EqualNeverObserved {
                flipped: CmpOp::Le,
                float_gap: None,
            },
        ] {
            assert_eq!(
                within_reach(gap.clone(), CmpOp::Lt, &obs, Reach::Multiples(8), 20, true),
                Some(gap)
            );
        }
        // `blocks * 8 > 20` seen at 16 and 24: nothing between them is a multiple of 8.
        let mut obs = Obs::of_d(16 - 20);
        obs.merge(&Obs::of_d(24 - 20));
        let gap = plan_boundary(CmpOp::Gt, &obs, None).gap.unwrap();
        assert_eq!(
            within_reach(gap, CmpOp::Gt, &obs, Reach::Multiples(8), 20, true),
            None
        );
        // Seen at 8 and 40: 16 and 24 are the values next to the boundary.
        let mut obs = Obs::of_d(8 - 20);
        obs.merge(&Obs::of_d(40 - 20));
        let gap = plan_boundary(CmpOp::Gt, &obs, None).gap.unwrap();
        assert_eq!(
            within_reach(gap, CmpOp::Gt, &obs, Reach::Multiples(8), 20, true),
            Some(InputGap::Boundary {
                min_shift: -12,
                max_shift: 19,
                missing: vec![-4, 4],
                // `> 16` to `> 23` answer like `> 20` for 16 and 24, and all between.
                same: (-4, 3),
            })
        );
    }

    fn obs_of(ds: &[i128]) -> Obs {
        let mut obs = Obs::default();
        for d in ds {
            obs.merge(&Obs::of_d(*d));
        }
        obs
    }

    fn rel_of(d: i128) -> Rel {
        match d {
            d if d < 0 => Rel::Less,
            0 => Rel::Equal,
            _ => Rel::Greater,
        }
    }

    /// Ground truth: does any concrete observation change under the mutation?
    fn brute_infects(op: CmpOp, m: Mutation, ds: &[i128]) -> bool {
        ds.iter()
            .any(|d| op.eval_d(*d) != eval_mutated_d(op, m, *d))
    }

    #[test]
    fn is_eligible_example_from_the_readme() {
        // `age >= 18`, tests pass 17 and 20: D = -1 and 2.
        let plan = plan_cmp(CmpOp::Ge, &obs_of(&[-1, 2]), None);
        assert_eq!(
            plan.gap,
            Some(InputGap::Boundary {
                min_shift: 0,
                max_shift: 2,
                missing: vec![0],
                same: (0, 0),
            })
        );
        // 17 is observed, so `age >= 17` flips a test outcome and has to be run.
        assert_eq!(plan.run, vec![Mutation::Shift(-1), Mutation::Negate]);
        assert_eq!(plan.gap.unwrap().representative(), Some(Mutation::Shift(1)));
    }

    /// Every operator `<` and `>` are mistaken for is accounted for, whatever the tests
    /// saw: it answers like the original for all of it and passes (a gap says so), or
    /// it is run. It is never left to another mutant that answers alike: two mutants
    /// that both change the way the code goes meet different values afterwards.
    #[test]
    fn the_operators_a_comparison_is_mistaken_for() {
        let rels = [Rel::Less, Rel::Equal, Rel::Greater, Rel::Unordered];
        for op in [CmpOp::Lt, CmpOp::Gt] {
            for seen in 1u8..16 {
                let mut obs = Obs::default();
                let seen: Vec<Rel> = (0..4)
                    .filter(|i| seen & (1 << i) != 0)
                    .map(|i| rels[i])
                    .collect();
                for rel in &seen {
                    obs.merge(&Obs::of_rel(*rel));
                }
                let plan = plan_cmp(op, &obs, None);
                for other in op.mistaken_for() {
                    let like_the_original = seen
                        .iter()
                        .all(|rel| other.eval_rel(*rel) == op.eval_rel(*rel));
                    let what = format!("{op:?} as {other:?} with {seen:?}: {plan:?}");
                    let run = plan
                        .run
                        .iter()
                        .filter(|m| **m == Mutation::Op(*other))
                        .count();
                    if like_the_original {
                        assert!(plan.gap.is_some(), "{what}");
                        assert_eq!(run, 0, "{what}");
                    } else {
                        assert_eq!(run, 1, "{what}");
                    }
                }
            }
        }
        // The other operators get nothing added.
        for op in [CmpOp::Le, CmpOp::Ge, CmpOp::Eq, CmpOp::Ne] {
            assert!(op.mistaken_for().is_empty());
        }
        // `>` for `<=` and `<` for `>=` are the negation unless the operands have no
        // order, and only integers are known to always have one. For anything else the
        // operator is run beside the negation wherever it answers otherwise than the
        // original, whether or not the tests gave operands without an order.
        let seen = |rels: &[Rel]| {
            let mut obs = Obs::default();
            for rel in rels {
                obs.merge(&Obs::of_rel(*rel));
            }
            obs
        };
        for (op, reverse) in [(CmpOp::Ge, CmpOp::Lt), (CmpOp::Le, CmpOp::Gt)] {
            let run = |rels: &[Rel]| plan_cmp(op, &seen(rels), None).run;
            let reversed = |rels: &[Rel]| run(rels).contains(&Mutation::Op(reverse));
            assert!(reversed(&[Rel::Unordered, Rel::Less]), "{op:?}");
            assert!(reversed(&[Rel::Less, Rel::Greater]), "{op:?}");
            assert!(reversed(&[Rel::Less, Rel::Equal, Rel::Greater]), "{op:?}");
            // Where the operator answers like the original for all that was seen, it
            // passes and is not run: only operands without an order, or only the side
            // on which both are false.
            assert_eq!(run(&[Rel::Unordered]), vec![Mutation::Negate], "{op:?}");
            // Integers: the negation is the operator, at every distance.
            for ds in [&[-1i128, 1][..], &[-2, 0, 3], &[0, 1]] {
                let plan = plan_cmp(op, &obs_of(ds), None);
                assert!(!plan.run.contains(&Mutation::Op(reverse)), "{op:?} {ds:?}");
            }
            for d in -3..=3 {
                assert_eq!(eval_mutated_d(op, Mutation::Negate, d), reverse.eval_d(d));
            }
        }
        // `!=` for `==` is the negation with and without an order.
        let run = plan_cmp(CmpOp::Eq, &seen(&[Rel::Unordered, Rel::Equal]), None).run;
        assert_eq!(run, vec![Mutation::Negate]);
        // Seen above the bound only, `>=` and `>` in place of `<` answer alike, and
        // both are run: the first step they change, they go different ways.
        let plan = plan_cmp(CmpOp::Lt, &Obs::of_rel(Rel::Greater), None);
        assert_eq!(plan.run, vec![Mutation::Negate, Mutation::Op(CmpOp::Gt)]);
    }

    #[test]
    fn pinned_boundary_has_no_input_gap() {
        for op in [CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge] {
            let plan = plan_cmp(op, &obs_of(&[-1, 0, 1]), None);
            assert_eq!(plan.gap, None, "{op:?}");
            // With all three relations seen, no two operators answer alike.
            let mut run = vec![Mutation::Shift(-1), Mutation::Shift(1), Mutation::Negate];
            run.extend(op.mistaken_for().iter().map(|other| Mutation::Op(*other)));
            assert_eq!(plan.run, run, "{op:?}");
        }
    }

    #[test]
    fn unreached_site() {
        let plan = plan_cmp(CmpOp::Lt, &Obs::default(), None);
        assert_eq!(
            plan,
            CmpPlan {
                cannot_differ: false,
                gap: Some(InputGap::Unreached),
                run: vec![]
            }
        );
    }

    #[test]
    fn one_sided_condition() {
        let plan = plan_cmp(CmpOp::Gt, &obs_of(&[3, 5]), None);
        assert_eq!(plan.gap, Some(InputGap::OneSided { always: true }));
        // Always true: the negation, `==` and `<` are all false here, and each is run.
        assert_eq!(
            plan.run,
            vec![
                Mutation::Negate,
                Mutation::Op(CmpOp::Eq),
                Mutation::Op(CmpOp::Lt)
            ]
        );
    }

    #[test]
    fn unsigned_compared_with_zero_is_not_reported() {
        // `x >= 0` for unsigned x: D ranges over 0..=MAX, the condition cannot be false.
        let plan = plan_cmp(CmpOp::Ge, &obs_of(&[0, 4]), Some((0, 255)));
        assert_eq!(plan.gap, None);
        assert!(plan.cannot_differ);
        assert!(!plan_cmp(CmpOp::Ge, &obs_of(&[0, 4]), Some((-1, 255))).cannot_differ);
        assert!(!plan_cmp(CmpOp::Ge, &obs_of(&[-1, 4]), Some((-1, 255))).cannot_differ);
        // `x < 0` for unsigned x can never be true either.
        let plan = plan_cmp(CmpOp::Lt, &obs_of(&[0, 4]), Some((0, 255)));
        assert_eq!(plan.gap, None);
        // `x >= 1` for unsigned x written as `x > 0`, observed at 0 and 1: both neighbours
        // are real mutants (`x > -1` is `x >= 0`, which differs at 0).
        let plan = plan_cmp(CmpOp::Gt, &obs_of(&[0, 1]), Some((0, 255)));
        assert_eq!(plan.gap, None);
        // `x == 0` answers like the negation at 0 and 1, and is run all the same.
        assert_eq!(
            plan.run,
            vec![
                Mutation::Shift(-1),
                Mutation::Shift(1),
                Mutation::Negate,
                Mutation::Op(CmpOp::Eq),
                Mutation::Op(CmpOp::Lt)
            ]
        );
        // `x <= 0` for unsigned x, observed at 0 and 1: `x <= -1` is a real mutant too,
        // but `x < 0`-style thresholds below the domain are not counted twice.
        let plan = plan_cmp(CmpOp::Le, &obs_of(&[0, 1]), Some((0, 255)));
        assert_eq!(plan.gap, None);
    }

    #[test]
    fn one_sided_condition_at_the_edge_of_the_type() {
        // Always true, and the only value that makes it false is the type's minimum.
        let plan = plan_cmp(CmpOp::Ge, &obs_of(&[0, 4]), Some((-1, 10)));
        assert_eq!(plan.gap, Some(InputGap::OneSided { always: true }));
        // ... or its maximum.
        let plan = plan_cmp(CmpOp::Le, &obs_of(&[-4, 0]), Some((-10, 1)));
        assert_eq!(plan.gap, Some(InputGap::OneSided { always: true }));
        let plan = plan_cmp(CmpOp::Le, &obs_of(&[-4, 0]), Some((-10, 0)));
        assert_eq!(plan.gap, None);
        // Without values the types are unknown: report.
        let plan = plan_cmp(CmpOp::Ge, &Obs::of_rel(Rel::Greater), Some((0, 10)));
        assert_eq!(plan.gap, Some(InputGap::OneSided { always: true }));
    }

    #[test]
    fn representative_is_the_nearest_survivor() {
        let boundary = |min_shift, max_shift| InputGap::Boundary {
            min_shift,
            max_shift,
            missing: vec![0],
            same: (0, 0),
        };
        assert_eq!(boundary(-3, 1).representative(), Some(Mutation::Shift(1)));
        assert_eq!(boundary(-1, 0).representative(), Some(Mutation::Shift(-1)));
        assert_eq!(boundary(0, 0).representative(), None);
        assert_eq!(InputGap::Unreached.representative(), None);
        assert_eq!(
            InputGap::OneSided { always: false }.representative(),
            Some(Mutation::Const(false))
        );
        assert_eq!(
            InputGap::EqualNeverObserved {
                flipped: CmpOp::Ge,
                float_gap: None
            }
            .representative(),
            Some(Mutation::Op(CmpOp::Ge))
        );
    }

    /// `a && b` / `a || b` with the tool's mutants, as a reference evaluator. Returns the
    /// result and whether the right operand was evaluated.
    fn eval_logic(and: bool, m: Option<Mutation>, a: bool, b: bool) -> (bool, bool) {
        let decided = |and: bool| if and { !a } else { a };
        match m {
            None => {
                if decided(and) {
                    (a, false)
                } else {
                    (b, true)
                }
            }
            Some(Mutation::LogicSwap) => {
                if decided(!and) {
                    (a, false)
                } else {
                    (b, true)
                }
            }
            // The right operand is evaluated as in the original, but its value is ignored.
            Some(Mutation::LogicDropRhs) => (a, !decided(and)),
            Some(Mutation::LogicDropLhs) => (b, true),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn logic_plans_match_a_reference_evaluator() {
        let cases = [(false, false), (false, true), (true, false), (true, true)];
        for and in [true, false] {
            for mask in 0u32..16 {
                let seen: Vec<(bool, bool)> = cases
                    .iter()
                    .enumerate()
                    .filter_map(|(i, c)| (mask & (1 << i) != 0).then_some(*c))
                    .collect();
                let mut obs = LogicObs::default();
                for (a, b) in &seen {
                    let (_, evaluated) = eval_logic(and, None, *a, *b);
                    obs.short |= !evaluated;
                    obs.rhs_true |= evaluated && *b;
                    obs.rhs_false |= evaluated && !*b;
                }
                let ctx = format!("and={and} seen={seen:?}");
                let differs = |m: Mutation| {
                    seen.iter().any(|(a, b)| {
                        eval_logic(and, None, *a, *b) != eval_logic(and, Some(m), *a, *b)
                    })
                };
                let plan = plan_logic(and, &obs);
                if seen.is_empty() {
                    assert_eq!(
                        plan,
                        LogicPlan {
                            gaps: vec![LogicGap::Unreached],
                            run: vec![]
                        }
                    );
                    continue;
                }
                for (m, gap) in [
                    (Mutation::LogicDropRhs, LogicGap::RhsNeverDecides),
                    (Mutation::LogicDropLhs, LogicGap::LhsNeverDecides),
                ] {
                    // Exact: a gap iff no observed case evaluates differently.
                    assert_eq!(plan.gaps.contains(&gap), !differs(m), "{gap:?} {ctx}");
                    assert_eq!(plan.run.contains(&m), differs(m), "{m:?} {ctx}");
                    assert_eq!(logic_infects(and, m, &obs), differs(m), "{m:?} {ctx}");
                }
                // The swap is always run: never claimed safe.
                assert!(plan.run.contains(&Mutation::LogicSwap), "{ctx}");
                assert!(logic_infects(and, Mutation::LogicSwap, &obs), "{ctx}");
            }
        }
        // A right operand that was started and gave no value (it panicked, or left the
        // function): the expression was executed, the operator swapped is run against
        // this test, and the other two change nothing in it.
        let started = LogicObs::from_bits(8);
        assert!(started.rhs_started && started.reached());
        for and in [true, false] {
            let plan = plan_logic(and, &started);
            assert_eq!(plan.run, vec![Mutation::LogicSwap]);
            assert_eq!(
                plan.gaps,
                vec![LogicGap::RhsNeverDecides, LogicGap::LhsNeverDecides]
            );
            assert!(logic_infects(and, Mutation::LogicSwap, &started));
            assert!(!logic_infects(and, Mutation::LogicDropRhs, &started));
            assert!(!logic_infects(and, Mutation::LogicDropLhs, &started));
        }
        let mut merged = LogicObs::from_bits(2);
        merged.merge(&started);
        assert_eq!(merged, LogicObs::from_bits(10));
        assert_eq!(
            LogicObs::from_bits(5),
            LogicObs {
                short: true,
                rhs_true: false,
                rhs_false: true,
                rhs_started: false,
            }
        );
        assert_eq!(
            LogicObs::from_bits(2),
            LogicObs {
                short: false,
                rhs_true: true,
                rhs_false: false,
                rhs_started: false,
            }
        );
    }

    #[test]
    fn return_plans() {
        assert_eq!(
            plan_return(&ReturnObs::from_bits(0)),
            ReturnPlan {
                unreached: true,
                always: None,
                run: vec![]
            }
        );
        assert_eq!(
            plan_return(&ReturnObs::from_bits(1)),
            ReturnPlan {
                unreached: false,
                always: Some(true),
                run: vec![Mutation::Const(false)]
            }
        );
        assert_eq!(
            plan_return(&ReturnObs::from_bits(2)),
            ReturnPlan {
                unreached: false,
                always: Some(false),
                run: vec![Mutation::Const(true)]
            }
        );
        assert_eq!(
            plan_return(&ReturnObs::from_bits(3)),
            ReturnPlan {
                unreached: false,
                always: None,
                run: vec![Mutation::Const(true), Mutation::Const(false)]
            }
        );
        // Forcing the value a test already sees changes nothing for that test.
        for bits in 0u8..4 {
            let obs = ReturnObs::from_bits(bits);
            assert_eq!(obs.infected_by_const(true), obs.saw_false);
            assert_eq!(obs.infected_by_const(false), obs.saw_true);
            assert_eq!(obs.reached(), bits != 0);
            let plan = plan_return(&obs);
            for m in &plan.run {
                let Mutation::Const(value) = m else { panic!() };
                assert!(
                    obs.infected_by_const(*value),
                    "a run mutant must change some test"
                );
            }
            if let Some(always) = plan.always {
                assert!(
                    !obs.infected_by_const(always),
                    "the survivor changes no test"
                );
            }
        }
        let mut merged = ReturnObs::from_bits(1);
        merged.merge(&ReturnObs::from_bits(2));
        assert_eq!(
            merged,
            ReturnObs {
                saw_true: true,
                saw_false: true
            }
        );
    }

    #[test]
    fn logic_constant_value() {
        let cases = [(false, false), (false, true), (true, false), (true, true)];
        for and in [true, false] {
            for mask in 0u32..16 {
                let seen: Vec<(bool, bool)> = cases
                    .iter()
                    .enumerate()
                    .filter_map(|(i, c)| (mask & (1 << i) != 0).then_some(*c))
                    .collect();
                let mut obs = LogicObs::default();
                let mut values = std::collections::BTreeSet::new();
                for (a, b) in &seen {
                    let (value, evaluated) = eval_logic(and, None, *a, *b);
                    values.insert(value);
                    obs.short |= !evaluated;
                    obs.rhs_true |= evaluated && *b;
                    obs.rhs_false |= evaluated && !*b;
                }
                let expected = if values.len() == 1 {
                    values.first().copied()
                } else {
                    None
                };
                assert_eq!(obs.constant_value(and), expected, "and={and} seen={seen:?}");
            }
        }
    }

    #[test]
    fn option_return_plans() {
        let plan = |bits| plan_option_return(&ReturnObs::from_bits(bits));
        assert_eq!(
            plan(0),
            ReturnPlan {
                unreached: true,
                always: None,
                run: vec![]
            }
        );
        // Only `None` was ever returned: "always None" changes nothing.
        assert_eq!(
            plan(2),
            ReturnPlan {
                unreached: false,
                always: Some(false),
                run: vec![]
            }
        );
        // `Some` was returned: replacing it by `None` has to be run, against those tests.
        for bits in [1, 3] {
            assert_eq!(
                plan(bits),
                ReturnPlan {
                    unreached: false,
                    always: None,
                    run: vec![Mutation::Const(false)]
                }
            );
            assert!(ReturnObs::from_bits(bits).infected_by_const(false));
        }
        assert!(!ReturnObs::from_bits(2).infected_by_const(false));
    }

    #[test]
    fn arithmetic_plans() {
        for bits in 0u8..8 {
            let obs = ArithObs::from_bits(bits);
            let expected = if bits & 2 != 0 {
                ArithPlan::Run
            } else if bits & 1 != 0 {
                ArithPlan::NeverDiffers
            } else if bits & 4 != 0 {
                ArithPlan::Opaque
            } else {
                ArithPlan::Unreached
            };
            assert_eq!(plan_arith(&obs), expected, "bits={bits}");
        }
        let mut merged = ArithObs::from_bits(1);
        merged.merge(&ArithObs::from_bits(4));
        assert_eq!(
            merged,
            ArithObs {
                same: true,
                differs: false,
                opaque: true,
                ..Default::default()
            }
        );
    }

    #[test]
    fn float_comparison_without_equal_operands() {
        // `celsius > 30.5` with 10.0 and 40.0: differences -20.5 and 9.5.
        let mut obs = Obs::of_f(-20.5);
        obs.merge(&Obs::of_f(9.5));
        let plan = plan_cmp(CmpOp::Gt, &obs, None);
        let gap = plan.gap.expect("a gap");
        assert_eq!(
            gap,
            InputGap::EqualNeverObserved {
                flipped: CmpOp::Ge,
                float_gap: Some((-20.5, 9.5))
            }
        );
        assert_eq!(
            gap.survivors(),
            vec![
                Mutation::Op(CmpOp::Ge),
                Mutation::ShiftF(4.75),
                Mutation::ShiftF(-10.25)
            ]
        );
        // Shifts inside the unobserved gap infect nothing; outside it, or once equality
        // has been observed, they do.
        for op in [CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge] {
            assert!(!infects(op, Mutation::ShiftF(4.75), &obs));
            assert!(!infects(op, Mutation::ShiftF(-10.25), &obs));
            assert!(infects(op, Mutation::ShiftF(9.5), &obs));
            assert!(infects(op, Mutation::ShiftF(-20.5), &obs));
        }
        let mut with_eq = obs;
        with_eq.merge(&Obs::of_f(0.0));
        assert!(infects(CmpOp::Gt, Mutation::ShiftF(4.75), &with_eq));
        assert_eq!(plan_cmp(CmpOp::Gt, &with_eq, None).gap, None);
        // Mixed with a non-float observation there is no float gap to speak of.
        let mut mixed = obs;
        mixed.merge(&Obs::of_rel(Rel::Less));
        assert_eq!(mixed.float_gap(), None);
        assert!(infects(CmpOp::Gt, Mutation::ShiftF(4.75), &mixed));
        assert_eq!(Obs::of_d(3).float_gap(), None);
    }

    #[test]
    fn survivors_are_listed_nearest_first() {
        let boundary = |min_shift, max_shift| InputGap::Boundary {
            min_shift,
            max_shift,
            missing: vec![0],
            same: (0, 0),
        };
        assert_eq!(
            boundary(-9, 9).survivors(),
            vec![Mutation::Shift(1), Mutation::Shift(-1), Mutation::Shift(2)]
        );
        assert_eq!(boundary(-1, 0).survivors(), vec![Mutation::Shift(-1)]);
        assert_eq!(
            boundary(0, 2).survivors(),
            vec![Mutation::Shift(1), Mutation::Shift(2)]
        );
        assert_eq!(
            InputGap::OneSided { always: true }.survivors(),
            vec![Mutation::Const(true)]
        );
        assert_eq!(InputGap::Unreached.survivors(), vec![]);
    }

    #[test]
    fn single_observation_summaries() {
        let bits = |o: Obs| (o.lt, o.eq, o.gt, o.un);
        assert_eq!(bits(Obs::of_f(-0.5)), (true, false, false, false));
        assert_eq!(bits(Obs::of_f(0.0)), (false, true, false, false));
        assert_eq!(bits(Obs::of_f(0.5)), (false, false, true, false));
        assert_eq!(
            (Obs::of_f(-0.5).f_max_neg, Obs::of_f(-0.5).f_min_pos),
            (Some(-0.5), None)
        );
        assert_eq!(
            (Obs::of_f(0.5).f_max_neg, Obs::of_f(0.5).f_min_pos),
            (None, Some(0.5))
        );
        assert_eq!(
            (Obs::of_f(0.0).f_max_neg, Obs::of_f(0.0).f_min_pos),
            (None, None)
        );
        assert!(Obs::of_f(1.0).unvalued && !Obs::of_f(1.0).unfloat);
        assert!(Obs::of_d(1).unfloat && !Obs::of_d(1).unvalued);

        // Reached through any one relation, including "unordered" alone.
        assert!(Obs::of_rel(Rel::Unordered).reached());
        assert!(!Obs::default().reached());
        // An unreached site has no values, even though nothing contradicts having them.
        assert!(!Obs::default().valued());
        assert!(Obs::of_d(0).valued());
        assert!(!Obs::of_rel(Rel::Equal).valued());

        // A NaN comparison among floats spoils the float gap.
        let mut nan = Obs::of_f(-1.0);
        nan.merge(&Obs::of_f(1.0));
        assert_eq!(nan.float_gap(), Some((-1.0, 1.0)));
        nan.un = true;
        assert_eq!(nan.float_gap(), None);
        assert_eq!(Obs::default().float_gap(), None);
        assert_eq!(Obs::of_f(-1.0).float_gap(), None, "one side only");
    }

    #[test]
    fn interval_queries_look_at_the_right_side_of_zero() {
        // Only positives were observed: an interval of negatives is empty, and vice versa.
        assert!(!obs_of(&[2]).saw_any_in(-3, -1));
        assert!(!obs_of(&[-2]).saw_any_in(1, 3));
        assert!(obs_of(&[-2]).saw_any_in(-3, -1));
        assert!(obs_of(&[2]).saw_any_in(1, 3));
        assert!(!obs_of(&[-4]).saw_any_in(-3, 0));
        assert!(!obs_of(&[4]).saw_any_in(0, 3));
        // A stray value on the far side must not leak into the answer.
        let mut odd = obs_of(&[3]);
        odd.max_neg = Some(5);
        assert!(!odd.saw_any_in(0, 2));
        let mut odd = obs_of(&[-3]);
        odd.min_pos = Some(-5);
        assert!(!odd.saw_any_in(-2, 0));
    }

    #[test]
    fn empty_interval_has_no_observations() {
        assert!(!obs_of(&[-1, 0, 1]).saw_any_in(1, 0));
        assert!(obs_of(&[0]).saw_any_in(0, 0));
    }

    #[test]
    fn unvalued_ordering_without_equal_operands() {
        let mut obs = Obs::of_rel(Rel::Less);
        obs.merge(&Obs::of_rel(Rel::Greater));
        let plan = plan_cmp(CmpOp::Lt, &obs, None);
        assert_eq!(
            plan.gap,
            Some(InputGap::EqualNeverObserved {
                flipped: CmpOp::Le,
                float_gap: None
            })
        );
        // Without equal operands `>` answers like the negation; it is run as well.
        assert_eq!(
            plan.run,
            vec![
                Mutation::Negate,
                Mutation::Op(CmpOp::Eq),
                Mutation::Op(CmpOp::Gt)
            ]
        );

        obs.merge(&Obs::of_rel(Rel::Equal));
        let plan = plan_cmp(CmpOp::Lt, &obs, None);
        assert_eq!(plan.gap, None);
        assert_eq!(
            plan.run,
            vec![
                Mutation::Op(CmpOp::Le),
                Mutation::Negate,
                Mutation::Op(CmpOp::Eq),
                Mutation::Op(CmpOp::Gt)
            ]
        );
    }

    #[test]
    fn equality_with_both_outcomes() {
        let plan = plan_cmp(CmpOp::Eq, &obs_of(&[0, 7]), None);
        assert_eq!(
            plan,
            CmpPlan {
                cannot_differ: false,
                gap: None,
                run: vec![Mutation::Negate]
            }
        );
    }

    #[test]
    fn unordered_counts_as_not_equal() {
        let obs = Obs::of_rel(Rel::Unordered);
        assert!(infects(CmpOp::Ne, Mutation::Const(false), &obs));
        assert!(!infects(CmpOp::Lt, Mutation::Op(CmpOp::Ge), &obs));
        assert!(infects(CmpOp::Lt, Mutation::Op(CmpOp::Ne), &obs));
    }

    #[test]
    fn merge_is_a_semilattice() {
        let samples: Vec<Obs> = [-3, -1, 0, 1, 4]
            .into_iter()
            .map(Obs::of_d)
            .chain([
                Obs::of_rel(Rel::Unordered),
                Obs::of_rel(Rel::Less),
                Obs::default(),
            ])
            .collect();
        for a in &samples {
            let mut aa = *a;
            aa.merge(a);
            assert_eq!(aa, *a, "idempotent");
            for b in &samples {
                let (mut ab, mut ba) = (*a, *b);
                ab.merge(b);
                ba.merge(a);
                assert_eq!(ab, ba, "commutative");
                for c in &samples {
                    let mut ab_c = ab;
                    ab_c.merge(c);
                    let mut bc = *b;
                    bc.merge(c);
                    let mut a_bc = *a;
                    a_bc.merge(&bc);
                    assert_eq!(ab_c, a_bc, "associative");
                }
            }
        }
    }

    #[test]
    fn mutation_wire_format() {
        assert_eq!(Mutation::Const(true).encode(), "c1");
        assert_eq!(Mutation::Const(false).encode(), "c0");
        assert_eq!(Mutation::Negate.encode(), "neg");
        assert_eq!(Mutation::Op(CmpOp::Ge).encode(), "op3");
        assert_eq!(Mutation::Shift(-2).encode(), "sh-2");
        assert_eq!(Mutation::Delete.encode(), "del");
        assert_eq!(Mutation::ShiftF(4.75).encode(), "shf4.75");
        assert_eq!(Mutation::ShiftF(-10.0).encode(), "shf-10.0");
        assert_eq!(Mutation::LogicSwap.encode(), "lsw");
        assert_eq!(Mutation::LogicDropRhs.encode(), "ldr");
        assert_eq!(Mutation::LogicDropLhs.encode(), "ldl");
        assert_eq!(Mutation::ArithSwap.encode(), "asw");
        assert_eq!(Mutation::DropNot.encode(), "unn");
        assert_eq!(Mutation::RangeEnd.encode(), "rng");
        assert_eq!(Mutation::MinMax.encode(), "mmx");
    }

    fn candidate_mutations(op: CmpOp) -> Vec<Mutation> {
        let mut ms = vec![
            Mutation::Const(true),
            Mutation::Const(false),
            Mutation::Negate,
        ];
        ms.extend(CmpOp::ALL.into_iter().map(Mutation::Op));
        if op.is_ordering() {
            ms.extend((-5..=5).filter(|s| *s != 0).map(Mutation::Shift));
        }
        ms
    }

    /// Checks a plan against concrete observations by brute force.
    fn check_plan(op: CmpOp, ds: &[i128], domain: Option<(i128, i128)>) {
        let obs = obs_of(ds);
        let ctx = format!("{op:?} ds={ds:?} domain={domain:?}");
        let (dom_lo, dom_hi) = domain.unwrap_or((-50, 50));
        let differs_somewhere =
            |m: Mutation| (dom_lo..=dom_hi).any(|d| op.eval_d(d) != eval_mutated_d(op, m, d));

        for m in candidate_mutations(op) {
            assert_eq!(
                infects(op, m, &obs),
                brute_infects(op, m, ds),
                "infects {m:?} {ctx}"
            );
        }

        let plan = plan_cmp(op, &obs, domain);
        for m in &plan.run {
            assert!(
                brute_infects(op, *m, ds),
                "run {m:?} must be infected: {ctx}"
            );
            assert!(differs_somewhere(*m), "run {m:?} is equivalent: {ctx}");
        }
        match &plan.gap {
            None => {}
            Some(InputGap::Unreached) => assert!(ds.is_empty(), "{ctx}"),
            Some(InputGap::OneSided { always }) => {
                assert!(ds.iter().all(|d| op.eval_d(*d) == *always), "{ctx}");
                assert!(differs_somewhere(Mutation::Const(*always)), "{ctx}");
            }
            Some(InputGap::Boundary {
                min_shift,
                max_shift,
                missing,
                same: _,
            }) => {
                assert!(*min_shift <= 0 && 0 <= *max_shift, "{ctx}");
                for s in (*min_shift..=*max_shift).filter(|s| *s != 0) {
                    let m = Mutation::Shift(s);
                    assert!(
                        !brute_infects(op, m, ds),
                        "survivor {m:?} is infected: {ctx}"
                    );
                    assert!(differs_somewhere(m), "survivor {m:?} is equivalent: {ctx}");
                }
                if domain.is_none() {
                    // Exactness: one step outside the range is caught by some observation.
                    for s in [min_shift - 1, max_shift + 1] {
                        assert!(
                            brute_infects(op, Mutation::Shift(s), ds),
                            "range not maximal: {ctx}"
                        );
                    }
                }
                assert!(!missing.is_empty(), "{ctx}");
                for w in missing {
                    assert!(!ds.contains(w), "missing value {w} was observed: {ctx}");
                    assert!(
                        dom_lo <= *w && *w <= dom_hi,
                        "missing value {w} out of domain: {ctx}"
                    );
                    let s = if op.eval_d(*w) != eval_mutated_d(op, Mutation::Shift(1), *w) {
                        1
                    } else {
                        -1
                    };
                    assert!(
                        op.eval_d(*w) != eval_mutated_d(op, Mutation::Shift(s), *w),
                        "missing value {w} separates nothing: {ctx}"
                    );
                    assert!(*min_shift <= s && s <= *max_shift, "{ctx}");
                }
            }
            Some(InputGap::EqualNeverObserved { .. }) => panic!("valued site: {ctx}"),
        }
        if let Some(m) = plan.gap.as_ref().and_then(InputGap::representative) {
            assert!(
                !brute_infects(op, m, ds),
                "representative {m:?} is infected: {ctx}"
            );
        }

        // Completeness for the off-by-one mutants: each is a survivor, is run, or is equivalent.
        if op.is_ordering() && ds.iter().any(|d| op.eval_d(*d)) && ds.iter().any(|d| !op.eval_d(*d))
        {
            for s in [-1, 1] {
                let m = Mutation::Shift(s);
                let survivor = matches!(&plan.gap, Some(InputGap::Boundary { min_shift, max_shift, .. })
                    if *min_shift <= s && s <= *max_shift);
                let ran = plan.run.contains(&m);
                assert!(
                    survivor ^ ran || !differs_somewhere(m),
                    "Shift({s}) unaccounted for: survivor={survivor} ran={ran} {ctx}"
                );
            }
        }
    }

    #[test]
    fn exhaustive_small_observation_sets() {
        let universe: Vec<i128> = (-3..=3).collect();
        for op in CmpOp::ALL {
            for mask in 0u32..(1 << universe.len()) {
                let ds: Vec<i128> = universe
                    .iter()
                    .enumerate()
                    .filter_map(|(i, d)| (mask & (1 << i) != 0).then_some(*d))
                    .collect();
                check_plan(op, &ds, None);
                // Domains that contain every observation.
                for (lo, hi) in [(0, 10), (-10, 0), (-2, 2), (1, 9), (-9, -1)] {
                    if ds.iter().all(|d| lo <= *d && *d <= hi) {
                        check_plan(op, &ds, Some((lo, hi)));
                    }
                }
            }
        }
    }

    /// Rows generated by `pict formal/pict/planner.pict /o:3 /e:formal/pict/planner.seed.tsv`.
    #[test]
    fn pict_table() {
        let table = include_str!("../tests/data/planner.pict.tsv");
        let mut rows = 0;
        for line in table.lines().skip(1) {
            let cols: Vec<&str> = line.split('\t').collect();
            let [op, below, equal, above, valued, literal, domain] = cols[..] else {
                panic!("bad row: {line}");
            };
            let op = match op {
                "lt" => CmpOp::Lt,
                "le" => CmpOp::Le,
                "gt" => CmpOp::Gt,
                "ge" => CmpOp::Ge,
                "eq" => CmpOp::Eq,
                "ne" => CmpOp::Ne,
                other => panic!("op {other}"),
            };
            let mut ds: Vec<i128> = vec![];
            match below {
                "none" => {}
                "adjacent" => ds.extend([-1, -4]),
                "far" => ds.extend([-3, -6]),
                other => panic!("below {other}"),
            }
            if equal == "yes" {
                ds.push(0);
            }
            match above {
                "none" => {}
                "adjacent" => ds.extend([1, 5]),
                "far" => ds.extend([3, 7]),
                other => panic!("above {other}"),
            }
            rows += 1;
            if valued == "no" {
                let mut obs = Obs::default();
                for d in &ds {
                    obs.merge(&Obs::of_rel(rel_of(*d)));
                }
                check_unvalued(op, &obs, &ds, line);
                continue;
            }
            // D = lhs - rhs. With the literal on the right, a tight lower bound on the other
            // operand is a lower bound on D; with the literal on the left it is an upper bound.
            let domain = match (literal, domain) {
                ("none", _) => None,
                (_, "wide") => Some((-40, 40)),
                ("rhs", "tight_low") | ("lhs", "tight_high") => Some((0, 40)),
                ("rhs", "tight_high") | ("lhs", "tight_low") => Some((-40, 0)),
                other => panic!("domain {other:?}"),
            };
            check_plan(op, &ds, domain);
        }
        assert_eq!(rows, table.lines().count() - 1);
        assert!(rows > 50, "the PICT table looks truncated: {rows} rows");
    }

    fn check_unvalued(op: CmpOp, obs: &Obs, ds: &[i128], ctx: &str) {
        let plan = plan_cmp(op, obs, None);
        for m in [
            Mutation::Const(true),
            Mutation::Const(false),
            Mutation::Negate,
        ]
        .into_iter()
        .chain(CmpOp::ALL.into_iter().map(Mutation::Op))
        {
            assert_eq!(
                infects(op, m, obs),
                brute_infects(op, m, ds),
                "infects {m:?} {ctx}"
            );
        }
        for m in &plan.run {
            assert!(
                brute_infects(op, *m, ds),
                "run {m:?} must be infected: {ctx}"
            );
        }
        if let Some(m) = plan.gap.as_ref().and_then(InputGap::representative) {
            assert!(
                !brute_infects(op, m, ds),
                "representative {m:?} is infected: {ctx}"
            );
        }
        match &plan.gap {
            Some(InputGap::Boundary { .. }) => panic!("unvalued site cannot shift: {ctx}"),
            Some(InputGap::EqualNeverObserved { flipped, .. }) => {
                assert!(!ds.contains(&0), "{ctx}");
                assert_eq!(op.flip_eq(), Some(*flipped), "{ctx}");
            }
            Some(InputGap::Unreached) => assert!(ds.is_empty(), "{ctx}"),
            _ => {}
        }
        // A shift on a reached site without values is never claimed to be safe.
        if op.is_ordering() && obs.reached() {
            assert!(infects(op, Mutation::Shift(1), obs), "{ctx}");
        }
    }
}
