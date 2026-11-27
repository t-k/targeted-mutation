/-!
# Soundness of the survivor computation in `src/plan.rs`

A comparison site observes values `D = a - b`. Integer comparisons are threshold
decisions on `D`: the "up" family (`>=`, `>`) is true iff `t ≤ D`, the "down" family
(`<=`, `<`) is true iff `D ≤ t`. Shifting the constant by `d` moves the threshold by `d`.

`S` is the set of values the test suite observed. The planner keeps only the nearest
observation on each side of the original threshold `t0` (`lo`, `hi`) and claims:

* every threshold between them agrees with the original on all observations
  (so the mutant is executed identically by every test: a guaranteed survivor);
* every threshold outside them is distinguished by an observation (so it must be run);
* a shifted threshold always has an input on which it disagrees with the original.

Checked with Lean 4 core (no Mathlib): `lean formal/lean/Planner.lean`.
-/
namespace Tmt

def up (t D : Int) : Prop := t ≤ D
def down (t D : Int) : Prop := D ≤ t

/-! ## Canonical threshold form of the four ordering operators applied to `D - d` -/

theorem ge_shift (D d : Int) : (D - d ≥ 0) ↔ up d D := by unfold up; omega
theorem gt_shift (D d : Int) : (D - d > 0) ↔ up (1 + d) D := by unfold up; omega
theorem le_shift (D d : Int) : (D - d ≤ 0) ↔ down d D := by unfold down; omega
theorem lt_shift (D d : Int) : (D - d < 0) ↔ down (-1 + d) D := by unfold down; omega

/-! ## Up family -/

theorem up_survivor (S : Int → Prop) (t0 lo hi t : Int)
    (hfalse : ∀ D, S D → D < t0 → D ≤ lo)
    (htrue : ∀ D, S D → t0 ≤ D → hi ≤ D)
    (h1 : lo < t) (h2 : t ≤ hi) :
    ∀ D, S D → (up t0 D ↔ up t D) := by
  intro D hD
  unfold up
  constructor
  · intro h
    have := htrue D hD h
    omega
  · intro h
    by_cases hc : t0 ≤ D
    · exact hc
    · have := hfalse D hD (by omega)
      omega

theorem up_infected (S : Int → Prop) (t0 lo hi t : Int)
    (hlo : S lo) (hlo' : lo < t0) (hhi : S hi) (hhi' : t0 ≤ hi)
    (h : t ≤ lo ∨ hi < t) :
    ∃ D, S D ∧ ¬ (up t0 D ↔ up t D) := by
  rcases h with h | h
  · exact ⟨lo, hlo, by unfold up; omega⟩
  · exact ⟨hi, hhi, by unfold up; omega⟩

theorem up_witness_raise (t0 t : Int) (h : t0 < t) : up t0 t0 ∧ ¬ up t t0 := by
  unfold up; omega

theorem up_witness_lower (t0 t : Int) (h : t < t0) : ¬ up t0 (t0 - 1) ∧ up t (t0 - 1) := by
  unfold up; omega

/-! ## Down family -/

theorem down_survivor (S : Int → Prop) (t0 lo hi t : Int)
    (htrue : ∀ D, S D → D ≤ t0 → D ≤ lo)
    (hfalse : ∀ D, S D → t0 < D → hi ≤ D)
    (h1 : lo ≤ t) (h2 : t < hi) :
    ∀ D, S D → (down t0 D ↔ down t D) := by
  intro D hD
  unfold down
  constructor
  · intro h
    have := htrue D hD h
    omega
  · intro h
    by_cases hc : D ≤ t0
    · exact hc
    · have := hfalse D hD (by omega)
      omega

theorem down_infected (S : Int → Prop) (t0 lo hi t : Int)
    (hlo : S lo) (hlo' : lo ≤ t0) (hhi : S hi) (hhi' : t0 < hi)
    (h : t < lo ∨ hi ≤ t) :
    ∃ D, S D ∧ ¬ (down t0 D ↔ down t D) := by
  rcases h with h | h
  · exact ⟨lo, hlo, by unfold down; omega⟩
  · exact ⟨hi, hhi, by unfold down; omega⟩

theorem down_witness_raise (t0 t : Int) (h : t0 < t) : ¬ down t0 (t0 + 1) ∧ down t (t0 + 1) := by
  unfold down; omega

theorem down_witness_lower (t0 t : Int) (h : t < t0) : down t0 t0 ∧ ¬ down t t0 := by
  unfold down; omega

/-! ## The trace summary is enough

The trace keeps `m` (largest negative observation), whether `0` was observed, and `p`
(smallest positive observation). Every interval the planner asks about touches
`{-1, 0, 1}`, and for those the summary answers "was anything observed in `[a, b]`" exactly.
-/

theorem summary_negative (S : Int → Prop) (m a : Int)
    (hm : S m) (hneg : m < 0) (hmax : ∀ D, S D → D < 0 → D ≤ m) :
    (∃ D, S D ∧ a ≤ D ∧ D ≤ -1) ↔ a ≤ m := by
  constructor
  · rintro ⟨D, hD, h1, h2⟩
    have := hmax D hD (by omega)
    omega
  · intro h
    exact ⟨m, hm, h, by omega⟩

theorem summary_positive (S : Int → Prop) (p b : Int)
    (hp : S p) (hpos : 0 < p) (hmin : ∀ D, S D → 0 < D → p ≤ D) :
    (∃ D, S D ∧ 1 ≤ D ∧ D ≤ b) ↔ p ≤ b := by
  constructor
  · rintro ⟨D, hD, h1, h2⟩
    have := hmin D hD (by omega)
    omega
  · intro h
    exact ⟨p, hp, by omega, h⟩

/-! ## Values the expression cannot be

Compared with a number, `x * 8`, `x & 0xF0` and `x % 10` are not every number. `R` is
the set of differences `D` such an expression can make (`within_reach` in `src/plan.rs`),
and the tests observed some of them.

* `u` and `o` are the nearest values of `R` under and over the original threshold.
  Every threshold between them answers like the original for all of `R`: it is no
  mutant (the range `same`). Every threshold outside is told apart by `u` or by `o`,
  which are the values the report asks a test for.
* When nothing of `R` lies strictly between the two nearest observations, every
  threshold the tests let through answers like the original for all of `R`: the gap is
  not reported.
-/

theorem up_same (R : Int → Prop) (t0 u o t : Int)
    (hunder : ∀ D, R D → D < t0 → D ≤ u)
    (hover : ∀ D, R D → t0 ≤ D → o ≤ D)
    (h1 : u < t) (h2 : t ≤ o) :
    ∀ D, R D → (up t0 D ↔ up t D) :=
  up_survivor R t0 u o t hunder hover h1 h2

theorem up_told_apart (R : Int → Prop) (t0 u o t : Int)
    (hu : R u) (hu' : u < t0) (ho : R o) (ho' : t0 ≤ o)
    (h : t ≤ u ∨ o < t) :
    ∃ D, R D ∧ ¬ (up t0 D ↔ up t D) :=
  up_infected R t0 u o t hu hu' ho ho' h

theorem down_same (R : Int → Prop) (t0 u o t : Int)
    (hunder : ∀ D, R D → D ≤ t0 → D ≤ u)
    (hover : ∀ D, R D → t0 < D → o ≤ D)
    (h1 : u ≤ t) (h2 : t < o) :
    ∀ D, R D → (down t0 D ↔ down t D) :=
  down_survivor R t0 u o t hunder hover h1 h2

theorem down_told_apart (R : Int → Prop) (t0 u o t : Int)
    (hu : R u) (hu' : u ≤ t0) (ho : R o) (ho' : t0 < o)
    (h : t < u ∨ o ≤ t) :
    ∃ D, R D ∧ ¬ (down t0 D ↔ down t D) :=
  down_infected R t0 u o t hu hu' ho ho' h

/-- The shifts called the same, as `within_reach` computes them from `u` and `o`. -/
theorem up_same_shifts (t0 u o s : Int) :
    (u < t0 + s ∧ t0 + s ≤ o) ↔ (u - t0 + 1 ≤ s ∧ s ≤ o - t0) := by omega

theorem down_same_shifts (t0 u o s : Int) :
    (u ≤ t0 + s ∧ t0 + s < o) ↔ (u - t0 ≤ s ∧ s ≤ o - t0 - 1) := by omega

/-- Nothing the expression can be lies strictly between the nearest observations `lo`
and `hi`: a threshold the tests let through (`lo < t ≤ hi`) is the original for every
value it can be. -/
theorem up_gap_unreachable (R : Int → Prop) (t0 lo hi t : Int)
    (hlo : lo < t0) (hhi : t0 ≤ hi)
    (hnone : ∀ D, R D → D ≤ lo ∨ hi ≤ D)
    (h1 : lo < t) (h2 : t ≤ hi) :
    ∀ D, R D → (up t0 D ↔ up t D) := by
  intro D hD
  unfold up
  have := hnone D hD
  omega

theorem down_gap_unreachable (R : Int → Prop) (t0 lo hi t : Int)
    (hlo : lo ≤ t0) (hhi : t0 < hi)
    (hnone : ∀ D, R D → D ≤ lo ∨ hi ≤ D)
    (h1 : lo ≤ t) (h2 : t < hi) :
    ∀ D, R D → (down t0 D ↔ down t D) := by
  intro D hD
  unfold down
  have := hnone D hD
  omega

/-! ### What is read from the last operation holds when the arithmetic wraps

An `n`-bit product wraps to its remainder by `2 ^ n`. For a factor `2 ^ k` with
`k ≤ n` the result is still a multiple of `2 ^ k`; for another factor it need not be
(`3 * 86 = 258` wraps to `2` in eight bits), which is why only powers of two are read
as "a multiple". A mask and a remainder never wrap. -/

theorem wrapped_multiple (a k n : Nat) (h : k ≤ n) : (a * 2 ^ k) % 2 ^ n % 2 ^ k = 0 := by
  rw [Nat.mod_mod_of_dvd _ (Nat.pow_dvd_pow 2 h)]
  exact Nat.mul_mod_left a (2 ^ k)

theorem other_factor_wraps_off : (86 * 3) % 2 ^ 8 % 3 ≠ 0 := by decide

theorem masked_within (a m : Nat) : a &&& m ≤ m := Nat.and_le_right

theorem remainder_below (a m : Int) (h : 0 < m) : -m < Int.tmod a m ∧ Int.tmod a m < m :=
  ⟨Int.lt_tmod_of_pos a h, Int.tmod_lt_of_pos a h⟩

/-! ## The two ends of a sequence

`v.first()` and `v.last()` (src/runtime/tmt_rt.rs, `SliceEnd`): a test that gives at most
one element cannot tell them apart, and one whose first and last element differ can. -/

/-- With at most one element both ends are the same: the other end is the original on
every such observation. -/
theorem ends_agree_upto_one {α : Type} (l : List α) (h : l.length ≤ 1) :
    l.head? = l.getLast? := by
  match l, h with
  | [], _ => rfl
  | [_], _ => rfl
  | _ :: _ :: _, h => exact absurd h (by simp)

/-- The input the finding asks for tells the two apart. -/
theorem ends_differ_when_asked {α : Type} (a b : α) (mid : List α) (h : a ≠ b) :
    (a :: (mid ++ [b])).head? ≠ (a :: (mid ++ [b])).getLast? := by
  have last : (a :: (mid ++ [b])).getLast? = some b := by
    rw [← List.cons_append, List.getLast?_append]
    simp
  rw [last]
  simp [h]

/-! ## 4-bit comparison truth table (`src/plan.rs`, `src/runtime/tmt_rt.rs`)

Relations are encoded as 4 bits: `0 = lt`, `1 = eq`, `2 = gt`, `3 = un`.
Each comparison operator is a 4-bit mask of relations on which it evaluates to `true`.
Then `op1` and `op2` disagree on some observed relation in `rels` iff
`(rels &&& (truthMask op1 ^^^ truthMask op2)) ≠ 0`, and `op` disagrees with constant `c`
iff `(if c then rels &&& ~~~(truthMask op) else rels &&& truthMask op) ≠ 0`. -/

inductive CmpOp where
  | lt | le | gt | ge | eq | ne
  deriving DecidableEq

def CmpOp.evalRel : CmpOp → Fin 4 → Bool
  | .lt, ⟨0, _⟩ => true
  | .lt, _      => false
  | .le, ⟨0, _⟩ => true
  | .le, ⟨1, _⟩ => true
  | .le, _      => false
  | .gt, ⟨2, _⟩ => true
  | .gt, _      => false
  | .ge, ⟨1, _⟩ => true
  | .ge, ⟨2, _⟩ => true
  | .ge, _      => false
  | .eq, ⟨1, _⟩ => true
  | .eq, _      => false
  | .ne, ⟨1, _⟩ => false
  | .ne, _      => true

def CmpOp.truthMask : CmpOp → BitVec 4
  | .lt => 0b0001#4
  | .le => 0b0011#4
  | .gt => 0b0100#4
  | .ge => 0b0110#4
  | .eq => 0b0010#4
  | .ne => 0b1101#4

def relIn (r : Fin 4) (rels : BitVec 4) : Bool :=
  (rels &&& (1#4 <<< r.val)) != 0#4

theorem evalRel_iff_truthMask (op : CmpOp) (r : Fin 4) :
    op.evalRel r = relIn r op.truthMask := by
  cases op <;> revert r <;> decide

theorem cmp_differs_iff_xor (op1 op2 : CmpOp) (rels : BitVec 4) :
    ((rels &&& (op1.truthMask ^^^ op2.truthMask)) != 0#4) = true ↔
      ∃ r : Fin 4, relIn r rels = true ∧ op1.evalRel r ≠ op2.evalRel r := by
  cases op1 <;> cases op2 <;> revert rels <;> decide

theorem cmp_const_differs_iff (op : CmpOp) (c : Bool) (rels : BitVec 4) :
    ((if c then rels &&& ~~~op.truthMask else rels &&& op.truthMask) != 0#4) = true ↔
      ∃ r : Fin 4, relIn r rels = true ∧ op.evalRel r ≠ c := by
  cases op <;> cases c <;> revert rels <;> decide

/-! ## Domain-universal equivalence of comparison mutations (`src/plan.rs`, `src/driver/session.rs`)

When the integer difference `D = lhs - rhs` is restricted to `[lo, hi]` by the operand
types (e.g. `x > 0` on unsigned `x` has `lo = 0`), two distinct mutations can agree on
every `D ∈ [lo, hi]`. In that case their runtime probes produce identical boolean results
for every possible input of the program, even after control flow diverges. -/

def evalCmpInt : CmpOp → Int → Bool
  | .lt, d => decide (d < 0)
  | .le, d => decide (d ≤ 0)
  | .gt, d => decide (d > 0)
  | .ge, d => decide (d ≥ 0)
  | .eq, d => decide (d = 0)
  | .ne, d => decide (d ≠ 0)

theorem gt_negate_eq_on_nonneg (D : Int) (h : 0 ≤ D) :
    (!evalCmpInt .gt D) = evalCmpInt .eq D := by
  unfold evalCmpInt
  by_cases h0 : D = 0 <;> simp [h0] <;> omega

theorem lt_negate_eq_on_nonpos (D : Int) (h : D ≤ 0) :
    (!evalCmpInt .lt D) = evalCmpInt .eq D := by
  unfold evalCmpInt
  by_cases h0 : D = 0 <;> simp [h0] <;> omega

theorem lt_negate_eq_gt_when_nonzero (D : Int) (h : D ≠ 0) :
    (!evalCmpInt .lt D) = evalCmpInt .gt D := by
  unfold evalCmpInt
  by_cases hlt : D < 0 <;> simp [hlt] <;> omega

theorem gt_negate_eq_lt_when_nonzero (D : Int) (h : D ≠ 0) :
    (!evalCmpInt .gt D) = evalCmpInt .lt D := by
  unfold evalCmpInt
  by_cases hgt : D > 0 <;> simp [hgt] <;> omega

/-- General decision principle: for any two predicates `p, q : Int → Bool` that are
constant on `(-∞, -2]`, on each point in `{-1, 0, 1}`, and on `[2, +∞)`, universal
agreement on `[lo, hi]` is equivalent to agreement on the finite sample points of
`[lo, hi]` intersecting `{-2, -1, 0, 1, 2}` (clamped to `[lo, hi]`). -/
theorem interval_equiv_from_samples (p q : Int → Bool) (lo hi : Int)
    (hlo : ∀ d, d ≤ -2 → p d = p (-2) ∧ q d = q (-2))
    (hhi : ∀ d, 2 ≤ d → p d = p 2 ∧ q d = q 2)
    (h_neg2 : lo ≤ -2 → -2 ≤ hi → p (-2) = q (-2))
    (h_neg1 : lo ≤ -1 → -1 ≤ hi → p (-1) = q (-1))
    (h_zero : lo ≤ 0 → 0 ≤ hi → p 0 = q 0)
    (h_pos1 : lo ≤ 1 → 1 ≤ hi → p 1 = q 1)
    (h_pos2 : lo ≤ 2 → 2 ≤ hi → p 2 = q 2)
    (h_left : hi < -2 → p (-2) = q (-2))
    (h_right : 2 < lo → p 2 = q 2) :
    ∀ d, lo ≤ d → d ≤ hi → p d = q d := by
  intro d hd1 hd2
  by_cases h1 : d ≤ -2
  · have ⟨hp, hq⟩ := hlo d h1
    rw [hp, hq]
    by_cases hc : -2 ≤ hi
    · exact h_neg2 (by omega) hc
    · exact h_left (by omega)
  · by_cases h2 : 2 ≤ d
    · have ⟨hp, hq⟩ := hhi d h2
      rw [hp, hq]
      by_cases hc : lo ≤ 2
      · exact h_pos2 hc (by omega)
      · exact h_right (by omega)
    · have : d = -1 ∨ d = 0 ∨ d = 1 := by omega
      rcases this with rfl | rfl | rfl
      · exact h_neg1 hd1 hd2
      · exact h_zero hd1 hd2
      · exact h_pos1 hd1 hd2

/-! ## BitVec disjoint-bit identities (`src/instrument/cfg.rs`)

When two bitvectors have disjoint possible-1 masks `(ma &&& mb) = 0`, their bitwise AND
is zero, so `a + b = a ||| b = a ^^^ b` for every bit width `w`. -/

theorem bitvec_disjoint_of_masks {w : Nat} (a b ma mb : BitVec w)
    (ha : a &&& ~~~ma = 0#w) (hb : b &&& ~~~mb = 0#w) (hdisj : ma &&& mb = 0#w) :
    a &&& b = 0#w := by
  ext i hi
  have h1 := congrArg (fun x => x[i]) ha
  have h2 := congrArg (fun x => x[i]) hb
  have h3 := congrArg (fun x => x[i]) hdisj
  simp only [BitVec.getElem_and, BitVec.getElem_not, BitVec.getElem_zero] at h1 h2 h3 ⊢
  cases hma : ma[i] <;> cases hmb : mb[i] <;> cases ha' : a[i] <;> cases hb' : b[i] <;>
    simp_all

theorem bitvec_disjoint_or_eq_xor {w : Nat} (a b : BitVec w) (h : a &&& b = 0#w) :
    a ||| b = a ^^^ b := by
  ext i hi
  have h1 := congrArg (fun x => x[i]) h
  simp only [BitVec.getElem_and, BitVec.getElem_or, BitVec.getElem_xor, BitVec.getElem_zero] at h1 ⊢
  cases ha : a[i] <;> cases hb : b[i] <;> simp_all

private theorem nat_add_eq_xor_of_and_eq_zero : ∀ a b : Nat, a &&& b = 0 → a + b = a ^^^ b := by
  intro a
  induction a using Nat.strongRecOn with
  | ind a ih =>
    intro b h
    by_cases ha : a = 0
    · subst ha; simp
    · have h0 : (a &&& b) % 2 = 0 := by simp [h]
      have hdiv : (a / 2) &&& (b / 2) = 0 := by
        apply Nat.eq_of_testBit_eq
        intro i
        have hi := congrArg (fun n => Nat.testBit n (i + 1)) h
        simpa [Nat.testBit_and, Nat.testBit_div_two] using hi
      have hrec := ih (a / 2) (Nat.div_lt_self (Nat.pos_of_ne_zero ha) (by omega)) (b / 2) hdiv
      have hbit : a % 2 + b % 2 = (a ^^^ b) % 2 := by
        have ht0 : (decide (a % 2 = 1) && decide (b % 2 = 1)) = false := by
          have := congrArg (fun n => Nat.testBit n 0) h
          simpa [Nat.testBit_and, Nat.testBit_zero] using this
        have hxt0 : decide ((a ^^^ b) % 2 = 1) = (decide (a % 2 = 1) ^^ decide (b % 2 = 1)) := by
          rw [← Nat.testBit_zero, ← Nat.testBit_zero, ← Nat.testBit_zero]
          exact Nat.testBit_xor a b 0
        have ha2 : a % 2 = 0 ∨ a % 2 = 1 := by omega
        have hb2 : b % 2 = 0 ∨ b % 2 = 1 := by omega
        have hx2 : (a ^^^ b) % 2 = 0 ∨ (a ^^^ b) % 2 = 1 := by omega
        rcases ha2 with ha2 | ha2 <;> rcases hb2 with hb2 | hb2 <;> rcases hx2 with hx2 | hx2 <;>
          simp [ha2, hb2, hx2] at ht0 hxt0 ⊢
      have hxdiv : (a ^^^ b) / 2 = (a / 2) ^^^ (b / 2) := by
        apply Nat.eq_of_testBit_eq
        intro i
        simp [Nat.testBit_div_two, Nat.testBit_xor]
      omega

theorem bitvec_disjoint_add_eq_or {w : Nat} (a b : BitVec w) (h : a &&& b = 0#w) :
    a + b = a ||| b := by
  have hx : a ||| b = a ^^^ b := bitvec_disjoint_or_eq_xor a b h
  rw [hx]
  apply BitVec.eq_of_toNat_eq
  have hand : a.toNat &&& b.toNat = 0 := by
    have := congrArg BitVec.toNat h
    simpa [BitVec.toNat_and] using this
  have hsum : a.toNat + b.toNat = a.toNat ^^^ b.toNat :=
    nat_add_eq_xor_of_and_eq_zero a.toNat b.toNat hand
  have hlt : a.toNat ^^^ b.toNat < 2 ^ w := by
    rw [← BitVec.toNat_xor]
    exact (a ^^^ b).isLt
  rw [BitVec.toNat_add, BitVec.toNat_xor, hsum, Nat.mod_eq_of_lt hlt]

/-! ## Zero-allocation `replace` vs `replacen(..., 1)` equivalence (`src/runtime/tmt_rt.rs`)

Given the greedy non-overlapping decomposition of a string into `segs = [u₀, u₁, ..., u_k]`
around `k` occurrences of pattern `p` (`segs.length = k + 1`):
* `replaceAll t segs` joins all `k` gaps with `t`;
* `replaceOne p t segs` joins the first gap with `t` and the remaining `k - 1` gaps with `p`.
They produce different strings iff `p ≠ t` and `k ≥ 2` (`segs.length ≥ 3`). -/

def joinWith {α : Type} (sep : List α) : List (List α) → List α
  | [] => []
  | [u] => u
  | u :: rest => u ++ sep ++ joinWith sep rest

def replaceOne {α : Type} (p t : List α) : List (List α) → List α
  | [] => []
  | [u] => u
  | u :: rest => u ++ t ++ joinWith p rest

private theorem joinWith_length {α : Type} (sep : List α) :
    ∀ segs : List (List α), segs ≠ [] →
      (joinWith sep segs).length = (segs.map List.length).sum + (segs.length - 1) * sep.length
  | [], h => absurd rfl h
  | [u], _ => by simp [joinWith]
  | u :: v :: rest, _ => by
    have ih := joinWith_length sep (v :: rest) (by simp)
    simp only [joinWith, List.length_append, List.map_cons, List.sum_cons, List.length_cons,
      Nat.add_sub_cancel] at ih ⊢
    rw [Nat.add_mul, Nat.one_mul]
    omega

theorem replace_ne_replacen1_iff {α : Type} [DecidableEq α] (p t : List α) (segs : List (List α)) :
    joinWith t segs ≠ replaceOne p t segs ↔ p ≠ t ∧ segs.length ≥ 3 := by
  match segs with
  | [] => simp [joinWith, replaceOne]
  | [u] => simp [joinWith, replaceOne]
  | [u, v] => simp [joinWith, replaceOne]
  | u :: v :: w :: rest =>
    simp only [joinWith, replaceOne, List.length_cons, ge_iff_le]
    constructor
    · intro hne
      refine ⟨?_, by omega⟩
      intro hpt
      subst hpt
      exact hne rfl
    · rintro ⟨hpt, _⟩ heq
      have htail : t ++ joinWith t (w :: rest) = p ++ joinWith p (w :: rest) := by
        have h1 : u ++ t ++ (v ++ (t ++ joinWith t (w :: rest))) =
            u ++ t ++ (v ++ (p ++ joinWith p (w :: rest))) := by
          simpa [List.append_assoc] using heq
        have h2 := List.append_cancel_left h1
        exact List.append_cancel_left h2
      by_cases hlen : p.length = t.length
      · have htake := congrArg (List.take p.length) htail
        rw [List.take_left' hlen.symm, List.take_left' rfl] at htake
        exact hpt htake.symm
      · have hl := congrArg List.length htail
        have l1 := joinWith_length t (w :: rest) (by simp)
        have l2 := joinWith_length p (w :: rest) (by simp)
        simp only [List.length_append, List.length_cons, Nat.add_sub_cancel] at hl l1 l2
        have hmul : (rest.length + 1) * t.length = (rest.length + 1) * p.length := by
          rw [Nat.add_mul, Nat.one_mul, Nat.add_mul, Nat.one_mul]
          omega
        have hpos : 0 < rest.length + 1 := by omega
        exact hlen ((Nat.mul_left_cancel hpos hmul).symm)

end Tmt
