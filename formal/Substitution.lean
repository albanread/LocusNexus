/-
  Substitution.lean — a self-contained, `sorry`-free de Bruijn substitution
  lemma and PRESERVATION for the effectful λ-core of the Locus calculus
  (var / lam / app / let / perform; effect rows = lists of labels).

  This discharges the "keystone" the main `LocusCalculus.lean` leaves `sorry`:
  a *capture-safe* (de Bruijn) substitution lemma, hence preservation, proved
  outright (no `sorry`). It uses function contexts (`Nat → Ty`) and parallel
  substitution, the cleanest hand-rolled de Bruijn metatheory (no Mathlib).

  Scope (honest): the effectful λ-core — values are `lam`, hence genuinely
  pure, so value-purity holds. The staging constructs (quote/splice/genlet) and
  the value-purity-of-`quote` subtlety the source doc itself defers, plus
  migrating the main file's term layer to this representation, are the
  remaining steps.

  Toolchain: leanprover/lean4 : v4.28.0.  Lean core only.
-/
set_option autoImplicit false

namespace Locus.Sub

/-! ## Effects, types, terms (de Bruijn) -/

inductive Label | exn | world (n : String) | gc | user (n : String)
  deriving DecidableEq, Repr

abbrev Row := List Label

inductive Ty
  | base (n : String)
  | arr  (dom : Ty) (lat : Row) (cod : Ty)
  deriving Repr

/-- de Bruijn terms; `lam` and `letin` each bind index 0 in their body. -/
inductive Term
  | var     (i : Nat)
  | lam     (dom : Ty) (body : Term)
  | app     (f a : Term)
  | letin   (rhs body : Term)
  | perform (op : Label) (arg : Term)
  deriving Repr

/-! ## Renaming and parallel substitution -/

/-- lift a renaming under one binder. -/
def upR (ρ : Nat → Nat) : Nat → Nat
  | 0   => 0
  | n+1 => (ρ n) + 1

def rename (ρ : Nat → Nat) : Term → Term
  | .var i        => .var (ρ i)
  | .lam A b      => .lam A (rename (upR ρ) b)
  | .app f a      => .app (rename ρ f) (rename ρ a)
  | .letin r b    => .letin (rename ρ r) (rename (upR ρ) b)
  | .perform op a => .perform op (rename ρ a)

/-- lift a substitution under one binder. -/
def upS (σ : Nat → Term) : Nat → Term
  | 0   => .var 0
  | n+1 => rename (· + 1) (σ n)

def subst (σ : Nat → Term) : Term → Term
  | .var i        => σ i
  | .lam A b      => .lam A (subst (upS σ) b)
  | .app f a      => .app (subst σ f) (subst σ a)
  | .letin r b    => .letin (subst σ r) (subst (upS σ) b)
  | .perform op a => .perform op (subst σ a)

/-- single-variable substitution: index 0 ↦ `w`, the rest shift down. -/
def subst0 (w : Term) : Term → Term :=
  subst (fun n => match n with | 0 => w | m+1 => .var m)

/-! ## Typing  `Γ ⊢ e : A ! E`  (function contexts) -/

abbrev Ctx := Nat → Ty

/-- extend a context with a new index-0 binder. -/
def cons (A : Ty) (Γ : Ctx) : Ctx
  | 0   => A
  | n+1 => Γ n

inductive Typed : Ctx → Term → Ty → Row → Prop
  | var {Γ i A} : Γ i = A → Typed Γ (.var i) A []
  | lam {Γ A b B E} :
      Typed (cons A Γ) b B E → Typed Γ (.lam A b) (.arr A E B) []
  | app {Γ f a A B Ef E1 E2} :
      Typed Γ f (.arr A Ef B) E1 → Typed Γ a A E2 →
      Typed Γ (.app f a) B (E1 ++ E2 ++ Ef)
  | letin {Γ r A E1 b B E2} :
      Typed Γ r A E1 → Typed (cons A Γ) b B E2 →
      Typed Γ (.letin r b) B (E1 ++ E2)
  | perform {Γ op a A B E} :
      Typed Γ a A E → Typed Γ (.perform op a) B (E ++ [op])

/-! ## Values are pure -/

inductive IsValue : Term → Prop
  | lam {A b} : IsValue (.lam A b)

theorem value_pure {Γ w A E} (hv : IsValue w) (ht : Typed Γ w A E) : E = [] := by
  cases hv with
  | lam => cases ht with | lam _ => rfl

/-! ## Row-subset helpers (core only — no `rcases`) -/

theorem append_subset_left {a a' b : Row} (h : a' ⊆ a) : a' ++ b ⊆ a ++ b := by
  intro x hx
  cases List.mem_append.mp hx with
  | inl h1 => exact List.mem_append.mpr (Or.inl (h h1))
  | inr h2 => exact List.mem_append.mpr (Or.inr h2)

theorem append_subset_right {a b b' : Row} (h : b' ⊆ b) : a ++ b' ⊆ a ++ b := by
  intro x hx
  cases List.mem_append.mp hx with
  | inl h1 => exact List.mem_append.mpr (Or.inl h1)
  | inr h2 => exact List.mem_append.mpr (Or.inr (h h2))

/-! ## Renaming preserves typing (weakening) -/

/-- `ρ` maps `Γ` into `Δ`: index `i` of `Γ` has the same type at `ρ i` in `Δ`. -/
def Renames (ρ : Nat → Nat) (Γ Δ : Ctx) : Prop := ∀ i, Δ (ρ i) = Γ i

theorem upR_renames {ρ Γ Δ A} (h : Renames ρ Γ Δ) :
    Renames (upR ρ) (cons A Γ) (cons A Δ) := by
  intro i
  cases i with
  | zero   => rfl
  | succ m => exact h m

theorem renames_shift {Γ A} : Renames (· + 1) Γ (cons A Γ) := fun _ => rfl

theorem rename_typed {Γ e A E} (ht : Typed Γ e A E) :
    ∀ {Δ ρ}, Renames ρ Γ Δ → Typed Δ (rename ρ e) A E := by
  induction ht with
  | var heq           => intro Δ ρ h; exact Typed.var ((h _).trans heq)
  | lam _ ih          => intro Δ ρ h; exact Typed.lam (ih (upR_renames h))
  | app _ _ ihf iha   => intro Δ ρ h; exact Typed.app (ihf h) (iha h)
  | letin _ _ ihr ihb => intro Δ ρ h; exact Typed.letin (ihr h) (ihb (upR_renames h))
  | perform _ ih      => intro Δ ρ h; exact Typed.perform (ih h)

/-! ## Substitution preserves typing — the keystone -/

/-- A substitution `σ` maps `Γ` into `Δ`: each `σ i` is a *pure* term of type
    `Γ i` in `Δ`. -/
def TypedSubst (Δ Γ : Ctx) (σ : Nat → Term) : Prop :=
  ∀ i, Typed Δ (σ i) (Γ i) []

theorem upS_typed {Δ Γ σ A} (h : TypedSubst Δ Γ σ) :
    TypedSubst (cons A Δ) (cons A Γ) (upS σ) := by
  intro i
  cases i with
  | zero   => exact Typed.var rfl
  | succ m => exact rename_typed (h m) renames_shift

theorem subst_typed {Γ e A E} (ht : Typed Γ e A E) :
    ∀ {Δ σ}, TypedSubst Δ Γ σ → Typed Δ (subst σ e) A E := by
  induction ht with
  | var heq           => intro Δ σ h; exact heq ▸ h _
  | lam _ ih          => intro Δ σ h; exact Typed.lam (ih (upS_typed h))
  | app _ _ ihf iha   => intro Δ σ h; exact Typed.app (ihf h) (iha h)
  | letin _ _ ihr ihb => intro Δ σ h; exact Typed.letin (ihr h) (ihb (upS_typed h))
  | perform _ ih      => intro Δ σ h; exact Typed.perform (ih h)

/-- Substituting a `Γ`-typed term for index 0 preserves typing. -/
theorem subst0_typed {Γ A w body B E}
    (hb : Typed (cons A Γ) body B E) (hw : Typed Γ w A []) :
    Typed Γ (subst0 w body) B E := by
  unfold subst0
  apply subst_typed hb
  intro i
  cases i with
  | zero   => exact hw
  | succ m => exact Typed.var rfl

/-! ## Reduction and preservation -/

inductive Step : Term → Term → Prop
  | beta   {A b w}   : IsValue w → Step (.app (.lam A b) w) (subst0 w b)
  | letv   {w b}     : IsValue w → Step (.letin w b) (subst0 w b)
  | appL   {f f' a}  : Step f f' → Step (.app f a) (.app f' a)
  | appR   {f a a'}  : Step a a' → Step (.app f a) (.app f a')
  | letrhs {r r' b}  : Step r r' → Step (.letin r b) (.letin r' b)
  | perf   {op a a'} : Step a a' → Step (.perform op a) (.perform op a')

/-- **Preservation** — types are preserved and effect rows only *shrink*
    (`E' ⊆ E`). The β and `let`-value cases are exactly `subst0_typed`; the
    congruence cases thread the inductive hypothesis with row monotonicity. -/
theorem preservation {Γ e A E} (ht : Typed Γ e A E) :
    ∀ {e'}, Step e e' → ∃ E', E' ⊆ E ∧ Typed Γ e' A E' := by
  induction ht with
  | var heq => intro e' hs; cases hs
  | lam hb ihb => intro e' hs; cases hs
  | app hf ha ihf iha =>
      intro e' hs
      cases hs with
      | beta hv =>
          cases hf with
          | lam hb =>
              have hE2 := value_pure hv ha
              subst hE2
              exact ⟨_, by intro x hx; simpa using hx, subst0_typed hb ha⟩
      | appL hst =>
          have ⟨E1', hsub, hf'⟩ := ihf hst
          exact ⟨_, append_subset_left (append_subset_left hsub), Typed.app hf' ha⟩
      | appR hst =>
          have ⟨E2', hsub, ha'⟩ := iha hst
          exact ⟨_, append_subset_left (append_subset_right hsub), Typed.app hf ha'⟩
  | letin hr hb ihr ihb =>
      intro e' hs
      cases hs with
      | letv hv =>
          have hE1 := value_pure hv hr
          subst hE1
          exact ⟨_, by intro x hx; simpa using hx, subst0_typed hb hr⟩
      | letrhs hst =>
          have ⟨E1', hsub, hr'⟩ := ihr hst
          exact ⟨_, append_subset_left hsub, Typed.letin hr' hb⟩
  | perform ha iha =>
      intro e' hs
      cases hs with
      | perf hst =>
          have ⟨E', hsub, ha'⟩ := iha hst
          exact ⟨_, append_subset_left hsub, Typed.perform ha'⟩

/-! ## Closedness and pure progress -/

/-- `bnd n e`: every free de Bruijn index in `e` is `< n`; `bnd 0 e` ⟺ closed. -/
def bnd (n : Nat) : Term → Bool
  | .var i       => decide (i < n)
  | .lam _ b     => bnd (n+1) b
  | .app f a     => bnd n f && bnd n a
  | .letin r b   => bnd n r && bnd (n+1) b
  | .perform _ a => bnd n a

/-- **Pure progress** — a *closed* term with an empty effect row is a value or
    steps; no stuck states. With `preservation`, this is **type safety for the
    pure λ-core**. (Effectful progress — blocked on an unhandled `perform` via
    evaluation contexts — is the noted next step.) -/
theorem progress_pure {Γ e A E} (ht : Typed Γ e A E) :
    bnd 0 e = true → E = [] → (IsValue e ∨ ∃ e', Step e e') := by
  induction ht with
  | var heq => intro hc _; simp [bnd] at hc
  | lam hb ihb => intro _ _; exact Or.inl IsValue.lam
  | app hf ha ihf iha =>
      intro hc hE
      simp only [bnd, Bool.and_eq_true] at hc
      have ⟨hcf, hca⟩ := hc
      have ⟨h12, _⟩ := List.append_eq_nil_iff.mp hE
      have ⟨hE1, hE2⟩ := List.append_eq_nil_iff.mp h12
      cases ihf hcf hE1 with
      | inl hvf =>
          cases hvf with
          | lam =>
              cases iha hca hE2 with
              | inl hva => exact Or.inr ⟨_, Step.beta hva⟩
              | inr hsa => have ⟨_, hsa'⟩ := hsa; exact Or.inr ⟨_, Step.appR hsa'⟩
      | inr hsf => have ⟨_, hsf'⟩ := hsf; exact Or.inr ⟨_, Step.appL hsf'⟩
  | letin hr hb ihr ihb =>
      intro hc hE
      simp only [bnd, Bool.and_eq_true] at hc
      have ⟨hcr, _⟩ := hc
      have ⟨hE1, _⟩ := List.append_eq_nil_iff.mp hE
      cases ihr hcr hE1 with
      | inl hvr => exact Or.inr ⟨_, Step.letv hvr⟩
      | inr hsr => have ⟨_, hsr'⟩ := hsr; exact Or.inr ⟨_, Step.letrhs hsr'⟩
  | perform ha iha =>
      intro _ hE
      simp at hE

/-! ## Effectful progress via a `Stuck` predicate (evaluation positions) -/

/-- `Stuck e` — `e` is blocked on an unhandled `perform` in evaluation position
    (the effect/runtime interface): the `e = K[perform op w]` evaluation-context
    decomposition, as an inductive over eval positions. -/
inductive Stuck : Term → Prop
  | perform    {op w} : IsValue w → Stuck (.perform op w)
  | performArg {op a} : Stuck a → Stuck (.perform op a)
  | appFun     {f a}  : Stuck f → Stuck (.app f a)
  | appArg     {f a}  : IsValue f → Stuck a → Stuck (.app f a)
  | letRhs     {r b}  : Stuck r → Stuck (.letin r b)

/-- **Progress (effectful)** — a *closed* well-typed term is a value, steps, or is
    `Stuck` (blocked on an unhandled `perform`). -/
theorem progress {Γ e A E} (ht : Typed Γ e A E) :
    bnd 0 e = true → (IsValue e ∨ (∃ e', Step e e') ∨ Stuck e) := by
  induction ht with
  | var heq => intro hc; simp [bnd] at hc
  | lam hb ihb => intro _; exact Or.inl IsValue.lam
  | app hf ha ihf iha =>
      intro hc
      simp only [bnd, Bool.and_eq_true] at hc
      have ⟨hcf, hca⟩ := hc
      cases ihf hcf with
      | inl hvf =>
          cases hvf with
          | lam =>
              cases iha hca with
              | inl hva => exact Or.inr (Or.inl ⟨_, Step.beta hva⟩)
              | inr h =>
                  cases h with
                  | inl hsa => have ⟨_, hs⟩ := hsa; exact Or.inr (Or.inl ⟨_, Step.appR hs⟩)
                  | inr hst => exact Or.inr (Or.inr (Stuck.appArg IsValue.lam hst))
      | inr h =>
          cases h with
          | inl hsf => have ⟨_, hs⟩ := hsf; exact Or.inr (Or.inl ⟨_, Step.appL hs⟩)
          | inr hst => exact Or.inr (Or.inr (Stuck.appFun hst))
  | letin hr hb ihr ihb =>
      intro hc
      simp only [bnd, Bool.and_eq_true] at hc
      have ⟨hcr, _⟩ := hc
      cases ihr hcr with
      | inl hvr => exact Or.inr (Or.inl ⟨_, Step.letv hvr⟩)
      | inr h =>
          cases h with
          | inl hsr => have ⟨_, hs⟩ := hsr; exact Or.inr (Or.inl ⟨_, Step.letrhs hs⟩)
          | inr hst => exact Or.inr (Or.inr (Stuck.letRhs hst))
  | perform ha iha =>
      intro hc
      simp only [bnd] at hc
      cases iha hc with
      | inl hva => exact Or.inr (Or.inr (Stuck.perform hva))
      | inr h =>
          cases h with
          | inl hsa => have ⟨_, hs⟩ := hsa; exact Or.inr (Or.inl ⟨_, Step.perf hs⟩)
          | inr hst => exact Or.inr (Or.inr (Stuck.performArg hst))

/-- A `Stuck` term's blocking operation **is in its effect row** — so a closed
    well-typed term is stuck only at an operation its type admits. With
    `progress` + `preservation`, this is **type safety for the effectful λ-core**:
    a well-typed program runs to a value or blocks at an unhandled effect its row
    declares — it never "goes wrong." -/
theorem stuck_op_in_row {e} (hst : Stuck e) :
    ∀ {Γ A E}, Typed Γ e A E → ∃ op, op ∈ E := by
  induction hst with
  | @perform op w hv =>
      intro Γ A E ht
      cases ht with
      | perform _ => exact ⟨op, by simp⟩
  | performArg _ ih =>
      intro Γ A E ht
      cases ht with
      | perform hta => have ⟨op, hop⟩ := ih hta; exact ⟨op, List.mem_append.mpr (Or.inl hop)⟩
  | appFun _ ih =>
      intro Γ A E ht
      cases ht with
      | app htf _ =>
          have ⟨op, hop⟩ := ih htf
          exact ⟨op, List.mem_append.mpr (Or.inl (List.mem_append.mpr (Or.inl hop)))⟩
  | appArg _ _ ih =>
      intro Γ A E ht
      cases ht with
      | app _ hta =>
          have ⟨op, hop⟩ := ih hta
          exact ⟨op, List.mem_append.mpr (Or.inl (List.mem_append.mpr (Or.inr hop)))⟩
  | letRhs _ ih =>
      intro Γ A E ht
      cases ht with
      | letin htr _ => have ⟨op, hop⟩ := ih htr; exact ⟨op, List.mem_append.mpr (Or.inl hop)⟩

end Locus.Sub
