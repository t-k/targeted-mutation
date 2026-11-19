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

end Tmt
