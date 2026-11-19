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
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
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
    }

    /// Summary of a single observation of integer operands with difference `d`.
    pub fn of_d(d: i128) -> Obs {
        Obs {
            lt: d < 0,
            eq: d == 0,
            gt: d > 0,
            max_neg: (d < 0).then_some(d),
            min_pos: (d > 0).then_some(d),
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
            ..Obs::default()
        }
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

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
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
    /// Delete a statement.
    Delete,
}

impl Mutation {
    /// Wire format shared with the injected runtime (`TMT_MUTANT=<site>:<encoded>`).
    pub fn encode(&self) -> String {
        match self {
            Mutation::Const(c) => format!("c{}", *c as u8),
            Mutation::Negate => "neg".to_string(),
            Mutation::Op(op) => format!("op{}", op.code()),
            Mutation::Shift(d) => format!("sh{d}"),
            Mutation::Delete => "del".to_string(),
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
        Mutation::Delete => op.eval_d(d),
    }
}

/// Whether some observation in `obs` gets a different result under mutation `m`.
/// A test that is not infected executes the mutant exactly like the original.
pub fn infects(op: CmpOp, m: Mutation, obs: &Obs) -> bool {
    match m {
        Mutation::Const(c) => obs.rels().any(|r| op.eval_rel(r) != c),
        Mutation::Negate => obs.reached(),
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
        Mutation::Delete => obs.reached(),
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
#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
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
    },
    /// The operands were never equal (and carry no integer values); `Op(flipped)` survives.
    EqualNeverObserved { flipped: CmpOp },
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CmpPlan {
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
                ..
            } => {
                if *max_shift >= 1 {
                    Some(Mutation::Shift(1))
                } else if *min_shift <= -1 {
                    Some(Mutation::Shift(-1))
                } else {
                    None
                }
            }
            InputGap::EqualNeverObserved { flipped } => Some(Mutation::Op(*flipped)),
        }
    }
}

/// Plans one comparison site. `domain` is the range of `D` the operand types allow, when
/// known. It only matters for a condition with a single observed outcome: if the types
/// rule the other outcome out (`x >= 0` for unsigned `x`), there is nothing to report.
pub fn plan_cmp(op: CmpOp, obs: &Obs, domain: Option<(i128, i128)>) -> CmpPlan {
    if !obs.reached() {
        return CmpPlan {
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
            gap: other_possible.then_some(InputGap::OneSided { always }),
            run: vec![Mutation::Negate],
        };
    }

    let Some((up, t0)) = op.threshold() else {
        // `==` / `!=` with both outcomes observed: nothing to shift.
        return CmpPlan {
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
                gap: None,
                run: vec![Mutation::Op(flipped), Mutation::Negate],
            }
        } else {
            CmpPlan {
                gap: Some(InputGap::EqualNeverObserved { flipped }),
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
    });
    CmpPlan { gap, run }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                missing: vec![0]
            })
        );
        // 17 is observed, so `age >= 17` flips a test outcome and has to be run.
        assert_eq!(plan.run, vec![Mutation::Shift(-1), Mutation::Negate]);
        assert_eq!(plan.gap.unwrap().representative(), Some(Mutation::Shift(1)));
    }

    #[test]
    fn pinned_boundary_has_no_input_gap() {
        for op in [CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge] {
            let plan = plan_cmp(op, &obs_of(&[-1, 0, 1]), None);
            assert_eq!(plan.gap, None, "{op:?}");
            assert_eq!(
                plan.run,
                vec![Mutation::Shift(-1), Mutation::Shift(1), Mutation::Negate],
                "{op:?}"
            );
        }
    }

    #[test]
    fn unreached_site() {
        let plan = plan_cmp(CmpOp::Lt, &Obs::default(), None);
        assert_eq!(
            plan,
            CmpPlan {
                gap: Some(InputGap::Unreached),
                run: vec![]
            }
        );
    }

    #[test]
    fn one_sided_condition() {
        let plan = plan_cmp(CmpOp::Gt, &obs_of(&[3, 5]), None);
        assert_eq!(plan.gap, Some(InputGap::OneSided { always: true }));
        assert_eq!(plan.run, vec![Mutation::Negate]);
    }

    #[test]
    fn unsigned_compared_with_zero_is_not_reported() {
        // `x >= 0` for unsigned x: D ranges over 0..=MAX, the condition cannot be false.
        let plan = plan_cmp(CmpOp::Ge, &obs_of(&[0, 4]), Some((0, 255)));
        assert_eq!(plan.gap, None);
        // `x < 0` for unsigned x can never be true either.
        let plan = plan_cmp(CmpOp::Lt, &obs_of(&[0, 4]), Some((0, 255)));
        assert_eq!(plan.gap, None);
        // `x >= 1` for unsigned x written as `x > 0`, observed at 0 and 1: both neighbours
        // are real mutants (`x > -1` is `x >= 0`, which differs at 0).
        let plan = plan_cmp(CmpOp::Gt, &obs_of(&[0, 1]), Some((0, 255)));
        assert_eq!(plan.gap, None);
        assert_eq!(
            plan.run,
            vec![Mutation::Shift(-1), Mutation::Shift(1), Mutation::Negate]
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
            InputGap::EqualNeverObserved { flipped: CmpOp::Ge }.representative(),
            Some(Mutation::Op(CmpOp::Ge))
        );
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
            Some(InputGap::EqualNeverObserved { flipped: CmpOp::Le })
        );
        assert_eq!(plan.run, vec![Mutation::Negate]);

        obs.merge(&Obs::of_rel(Rel::Equal));
        let plan = plan_cmp(CmpOp::Lt, &obs, None);
        assert_eq!(plan.gap, None);
        assert_eq!(plan.run, vec![Mutation::Op(CmpOp::Le), Mutation::Negate]);
    }

    #[test]
    fn equality_with_both_outcomes() {
        let plan = plan_cmp(CmpOp::Eq, &obs_of(&[0, 7]), None);
        assert_eq!(
            plan,
            CmpPlan {
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
            Some(InputGap::EqualNeverObserved { flipped }) => {
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
