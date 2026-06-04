/-
  Handlers.lean — deep effect handlers with SET-discharge (D4), and
  PRESERVATION for the handle reductions, built `sorry`-free on top of the
  de Bruijn technique proved in `Substitution.lean`.

  This mechanizes the `calculus.md` (op)/(return) rules — the *deep* handler,
  whose resumption re-installs the handler (`resume ↦ λz. handle K[z] with H`)
  — under D4 set-rows: discharge removes the operation from the row entirely,
  expressed as the side-condition `Eb ⊆ op :: Eo` (i.e. `removeAll op Eb ⊆ Eo`,
  set-remove). Confirmed against source: handlers are deep
  (`docs/calculus.md:455`; the `locus-llvm/src/lower.rs` CPS transform re-runs
  the continuation under the same handler), and rows are sets
  (`docs/calculus.md:91,201`, `E ∪ {ε}`).

  Scope (honest): one operation per handler; identity return (`handle v ↦ v`);
  PRESERVATION (not progress) for the handle rules. Handler-free evaluation
  contexts `K` are binder-free (no evaluation under `lam`), so the deep-resume
  bookkeeping is one uniform de Bruijn shift.

  Toolchain: leanprover/lean4 : v4.28.0.  Lean core only.
-/
set_option autoImplicit false

namespace Locus.Handlers

/-! ## Effects, types, terms (de Bruijn) -/

inductive Label | exn | io | st
  deriving DecidableEq, Repr

abbrev Row := List Label

inductive Ty
  | base (n : String)
  | arr  (dom : Ty) (lat : Row) (cod : Ty)
  deriving Repr

/-- de Bruijn terms. `lam` binds index 0 in `body`; a `handle`'s op-clause
    `opc` binds index 0 = the operation argument `x`, index 1 = `resume`. -/
inductive Tm
  | var     (i : Nat)
  | lam     (dom : Ty) (body : Tm)
  | app     (f a : Tm)
  | perform (op : Label) (arg : Tm)
  | handle  (op : Label) (R : Ty) (body : Tm) (opc : Tm)
  deriving Repr

/-! ## Renaming and parallel substitution -/

def upR (ρ : Nat → Nat) : Nat → Nat
  | 0   => 0
  | n+1 => (ρ n) + 1

def rename (ρ : Nat → Nat) : Tm → Tm
  | .var i           => .var (ρ i)
  | .lam A b         => .lam A (rename (upR ρ) b)
  | .app f a         => .app (rename ρ f) (rename ρ a)
  | .perform op a    => .perform op (rename ρ a)
  | .handle op R b opc => .handle op R (rename ρ b) (rename (upR (upR ρ)) opc)

def upS (σ : Nat → Tm) : Nat → Tm
  | 0   => .var 0
  | n+1 => rename (· + 1) (σ n)

def subst (σ : Nat → Tm) : Tm → Tm
  | .var i           => σ i
  | .lam A b         => .lam A (subst (upS σ) b)
  | .app f a         => .app (subst σ f) (subst σ a)
  | .perform op a    => .perform op (subst σ a)
  | .handle op R b opc => .handle op R (subst σ b) (subst (upS (upS σ)) opc)

/-- single-variable substitution: index 0 ↦ `w`, the rest shift down. -/
def subst0 (w : Tm) : Tm → Tm :=
  subst (fun n => match n with | 0 => w | m+1 => .var m)

/-! ## Typing  `Γ ⊢ e : A ! E`  with an operation signature `S` -/

abbrev Ctx := Nat → Ty

def cons (A : Ty) (Γ : Ctx) : Ctx
  | 0   => A
  | n+1 => Γ n

/-- `S op = (argument type, result type)` of the operation. The handler's
    op-clause sees `x : (S op).1` and `resume : (S op).2 → A ! Eo` — *deep*, so
    `resume` returns the handled type `A` performing the output row `Eo`. The
    discharge is `Eb ⊆ op :: Eo`: every effect the body may perform is either
    `op` (caught here) or already in the output row — the set-remove of `op`. -/
inductive Typed (S : Label → Ty × Ty) : Ctx → Tm → Ty → Row → Prop
  | var {Γ i A} : Γ i = A → Typed S Γ (.var i) A []
  | lam {Γ A b B E} :
      Typed S (cons A Γ) b B E → Typed S Γ (.lam A b) (.arr A E B) []
  | app {Γ f a A B Ef E1 E2} :
      Typed S Γ f (.arr A Ef B) E1 → Typed S Γ a A E2 →
      Typed S Γ (.app f a) B (E1 ++ E2 ++ Ef)
  | perform {Γ op a E} :
      Typed S Γ a (S op).1 E → Typed S Γ (.perform op a) (S op).2 (E ++ [op])
  | handle {Γ op body A Eb opc Eo} :
      Typed S Γ body A Eb →
      Eb ⊆ op :: Eo →
      Typed S (cons (S op).1 (cons (.arr (S op).2 Eo A) Γ)) opc A Eo →
      Typed S Γ (.handle op (S op).2 body opc) A Eo

/-! ## Values are pure -/

inductive IsValue : Tm → Prop
  | lam {A b} : IsValue (.lam A b)

theorem value_pure {S Γ w A E} (hv : IsValue w) (ht : Typed S Γ w A E) : E = [] := by
  cases hv with
  | lam => cases ht with | lam _ => rfl

/-! ## Row-subset helpers -/

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

def Renames (ρ : Nat → Nat) (Γ Δ : Ctx) : Prop := ∀ i, Δ (ρ i) = Γ i

theorem upR_renames {ρ Γ Δ A} (h : Renames ρ Γ Δ) :
    Renames (upR ρ) (cons A Γ) (cons A Δ) := by
  intro i
  cases i with
  | zero   => rfl
  | succ m => exact h m

theorem renames_shift {Γ A} : Renames (· + 1) Γ (cons A Γ) := fun _ => rfl

theorem rename_typed {S Γ e A E} (ht : Typed S Γ e A E) :
    ∀ {Δ ρ}, Renames ρ Γ Δ → Typed S Δ (rename ρ e) A E := by
  induction ht with
  | var heq             => intro Δ ρ h; exact Typed.var ((h _).trans heq)
  | lam _ ih            => intro Δ ρ h; exact Typed.lam (ih (upR_renames h))
  | app _ _ ihf iha     => intro Δ ρ h; exact Typed.app (ihf h) (iha h)
  | perform _ ih        => intro Δ ρ h; exact Typed.perform (ih h)
  | handle _ hsub _ ihb ihc =>
      intro Δ ρ h
      exact Typed.handle (ihb h) hsub (ihc (upR_renames (upR_renames h)))

/-! ## Substitution preserves typing -/

def TypedSubst (S : Label → Ty × Ty) (Δ Γ : Ctx) (σ : Nat → Tm) : Prop :=
  ∀ i, Typed S Δ (σ i) (Γ i) []

theorem upS_typed {S Δ Γ σ A} (h : TypedSubst S Δ Γ σ) :
    TypedSubst S (cons A Δ) (cons A Γ) (upS σ) := by
  intro i
  cases i with
  | zero   => exact Typed.var rfl
  | succ m => exact rename_typed (h m) renames_shift

theorem subst_typed {S Γ e A E} (ht : Typed S Γ e A E) :
    ∀ {Δ σ}, TypedSubst S Δ Γ σ → Typed S Δ (subst σ e) A E := by
  induction ht with
  | var heq             => intro Δ σ h; exact heq ▸ h _
  | lam _ ih            => intro Δ σ h; exact Typed.lam (ih (upS_typed h))
  | app _ _ ihf iha     => intro Δ σ h; exact Typed.app (ihf h) (iha h)
  | perform _ ih        => intro Δ σ h; exact Typed.perform (ih h)
  | handle _ hsub _ ihb ihc =>
      intro Δ σ h
      exact Typed.handle (ihb h) hsub (ihc (upS_typed (upS_typed h)))

/-- Substituting a `Γ`-typed term for index 0 preserves typing. -/
theorem subst0_typed {S Γ A w body B E}
    (hb : Typed S (cons A Γ) body B E) (hw : Typed S Γ w A []) :
    Typed S Γ (subst0 w body) B E := by
  unfold subst0
  apply subst_typed hb
  intro i
  cases i with
  | zero   => exact hw
  | succ m => exact Typed.var rfl

/-! ## Handler-free evaluation contexts (binder-free) -/

/-- A handler-free, binder-free evaluation context `K`: the hole, a frame
    `K[·] a`, `v (K[·])`, or `perform op (K[·])`. No `handle` frame (a `perform`
    inside `K` binds to the *nearest* enclosing handler) and no `lam` frame
    (call-by-value does not reduce under λ), so `K` carries no binders. -/
inductive Ectx
  | hole
  | appL (K : Ectx) (a : Tm)
  | appR (v : Tm) (K : Ectx)
  | perf (op : Label) (K : Ectx)

def plug : Ectx → Tm → Tm
  | .hole,      e => e
  | .appL K a,  e => .app (plug K e) a
  | .appR v K,  e => .app v (plug K e)
  | .perf op K, e => .perform op (plug K e)

def renameE (ρ : Nat → Nat) : Ectx → Ectx
  | .hole      => .hole
  | .appL K a  => .appL (renameE ρ K) (rename ρ a)
  | .appR v K  => .appR (rename ρ v) (renameE ρ K)
  | .perf op K => .perf op (renameE ρ K)

theorem rename_plug (ρ : Nat → Nat) (K : Ectx) (e : Tm) :
    rename ρ (plug K e) = plug (renameE ρ K) (rename ρ e) := by
  induction K with
  | hole         => rfl
  | appL K0 a ih => simp only [plug, renameE, rename, ih]
  | appR v K0 ih => simp only [plug, renameE, rename, ih]
  | perf op0 K0 ih => simp only [plug, renameE, rename, ih]

/-- **Monotone context typing.** If `plug K e : A ! Eb`, the hole has some type
    `Bh ! Eh`; and replacing the hole with any `e' : Bh ! Eh'` whose row only
    shrinks (`Eh' ⊆ Eh`) keeps the plug typed at `A`, with a row `⊆ Eb`. This is
    what makes the deep resumption `λz. handle K[z] H` typecheck. -/
theorem plug_retype {S Δ} (K : Ectx) : ∀ {e A Eb}, Typed S Δ (plug K e) A Eb →
    ∃ Bh Eh, Typed S Δ e Bh Eh ∧
      ∀ {e' Eh'}, Typed S Δ e' Bh Eh' → Eh' ⊆ Eh →
        ∃ Ek, Ek ⊆ Eb ∧ Typed S Δ (plug K e') A Ek := by
  induction K with
  | hole =>
      intro e A Eb ht
      exact ⟨A, Eb, ht, by intro e' Eh' he' hsub; exact ⟨Eh', hsub, he'⟩⟩
  | appL K0 a ihK0 =>
      intro e A Eb ht
      cases ht with
      | app hf ha =>
          obtain ⟨Bh, Eh, hbh, hmono⟩ := ihK0 hf
          refine ⟨Bh, Eh, hbh, ?_⟩
          intro e' Eh' he' hsub
          obtain ⟨Ek0, hk0sub, hk0⟩ := hmono he' hsub
          exact ⟨_, append_subset_left (append_subset_left hk0sub), Typed.app hk0 ha⟩
  | appR v K0 ihK0 =>
      intro e A Eb ht
      cases ht with
      | app hf ha =>
          obtain ⟨Bh, Eh, hbh, hmono⟩ := ihK0 ha
          refine ⟨Bh, Eh, hbh, ?_⟩
          intro e' Eh' he' hsub
          obtain ⟨Ek0, hk0sub, hk0⟩ := hmono he' hsub
          exact ⟨_, append_subset_left (append_subset_right hk0sub), Typed.app hf hk0⟩
  | perf op0 K0 ihK0 =>
      intro e A Eb ht
      cases ht with
      | perform ha =>
          obtain ⟨Bh, Eh, hbh, hmono⟩ := ihK0 ha
          refine ⟨Bh, Eh, hbh, ?_⟩
          intro e' Eh' he' hsub
          obtain ⟨Ek0, hk0sub, hk0⟩ := hmono he' hsub
          exact ⟨_, append_subset_left hk0sub, Typed.perform hk0⟩

/-! ## Reduction (deep handlers) and preservation -/

/-- the deep resumption `λz. handle op K[z] opc`: one fresh binder `z` (index 0),
    past which `K` and `opc` are lifted. -/
def resumeTm (op : Label) (R : Ty) (K : Ectx) (opc : Tm) : Tm :=
  .lam R (.handle op R (plug (renameE (· + 1) K) (.var 0)) (rename (upR (upR (· + 1))) opc))

/-- the `(op)`-rule contractum: `opc` with `x ↦ w` (index 0) and
    `resume ↦ λz. handle K[z] H` (index 1). -/
def opContract (op : Label) (R : Ty) (w : Tm) (K : Ectx) (opc : Tm) : Tm :=
  subst (fun n => match n with
    | 0     => w
    | 1     => resumeTm op R K opc
    | m + 2 => .var m) opc

inductive Step : Tm → Tm → Prop
  | beta {A b w}   : IsValue w → Step (.app (.lam A b) w) (subst0 w b)
  | appL {f f' a}  : Step f f' → Step (.app f a) (.app f' a)
  | appR {f a a'}  : Step a a' → Step (.app f a) (.app f a')
  | perf {op a a'} : Step a a' → Step (.perform op a) (.perform op a')
  -- (return): identity return clause `handle op R v opc ↦ v`
  | handleRet {op R w opc}     : IsValue w → Step (.handle op R w opc) w
  -- congruence: evaluate the handled body
  | handleCong {op R b b' opc} : Step b b' → Step (.handle op R b opc) (.handle op R b' opc)
  -- (op): the *deep* handler — `resume` re-installs `handle … with H`
  | handleOp {op R w K opc} :
      IsValue w →
      Step (.handle op R (plug K (.perform op w)) opc) (opContract op R w K opc)

/-- **The `(op)`-case** — a handled, deep operation step preserves typing under
    D4 set-discharge. The resumption `λz. handle K[z] H` typechecks because
    `plug_retype` lets us re-fill the hole with the bound `z : (S op).2` at a row
    that only shrinks, and the discharge side-condition `Eb ⊆ op :: Eo` then
    feeds the inner `handle`. This is the crux of effect-system soundness. -/
theorem preservation_handleOp {S Γ op w K opc A Eb Eo}
    (hbody : Typed S Γ (plug K (.perform op w)) A Eb)
    (hsub : Eb ⊆ op :: Eo)
    (hopc : Typed S (cons (S op).1 (cons (.arr (S op).2 Eo A) Γ)) opc A Eo)
    (hv : IsValue w) :
    Typed S Γ (opContract op (S op).2 w K opc) A Eo := by
  obtain ⟨Bh, Eh, hhole, _⟩ := plug_retype K hbody
  cases hhole with
  | perform hw =>
      have hEw := value_pure hv hw
      subst hEw
      have hren := rename_typed hbody (renames_shift (A := (S op).2))
      rw [rename_plug] at hren
      obtain ⟨Bh', Eh', hhole', hmono'⟩ := plug_retype (renameE (· + 1) K) hren
      cases hhole' with
      | perform hw' =>
          obtain ⟨Ek, hEksub, hKz⟩ :=
            hmono' (e' := .var 0) (Typed.var rfl) (List.nil_subset _)
          have hopc' := rename_typed hopc
            (upR_renames (upR_renames (renames_shift (A := (S op).2))))
          have hres := Typed.lam (Typed.handle hKz (List.Subset.trans hEksub hsub) hopc')
          unfold opContract
          apply subst_typed hopc
          intro i
          cases i with
          | zero => exact hw
          | succ j =>
              cases j with
              | zero   => exact hres
              | succ m => exact Typed.var rfl

/-- **Preservation, including the deep-handler reductions** — types preserved,
    effect rows only shrink (`E' ⊆ E`). `handleRet`/`handleCong` are routine;
    `handleOp` is `preservation_handleOp` — type safety survives a handled
    operation under set-discharge. -/
theorem preservation {S Γ e A E} (ht : Typed S Γ e A E) :
    ∀ {e'}, Step e e' → ∃ E', E' ⊆ E ∧ Typed S Γ e' A E' := by
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
  | perform ha iha =>
      intro e' hs
      cases hs with
      | perf hst =>
          have ⟨E', hsub, ha'⟩ := iha hst
          exact ⟨_, append_subset_left hsub, Typed.perform ha'⟩
  | handle hbody hsub hopc ihbody ihopc =>
      intro e' hs
      cases hs with
      | handleRet hv =>
          have hEb := value_pure hv hbody
          subst hEb
          exact ⟨_, List.nil_subset _, hbody⟩
      | handleCong hst =>
          obtain ⟨Eb', hsub', hbody'⟩ := ihbody hst
          exact ⟨_, (by intro x hx; exact hx),
                 Typed.handle hbody' (List.Subset.trans hsub' hsub) hopc⟩
      | handleOp hv =>
          exact ⟨_, (by intro x hx; exact hx), preservation_handleOp hbody hsub hopc hv⟩

end Locus.Handlers
