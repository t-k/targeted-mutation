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

end Tmt
