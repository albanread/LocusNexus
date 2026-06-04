/-
  LocusCalculus.lean — a Lean 4 formalization of the Locus core calculus.

  Source of truth : docs/calculus.md  ("The Locus Core Calculus").
  Toolchain       : leanprover/lean4 : v4.28.0   (see ./lean-toolchain)
  Dependencies    : Lean core only (no Mathlib / Batteries).

  ── WHAT THIS IS ──────────────────────────────────────────────────────────
  A faithful encoding of the calculus's three grades, the kind partition and
  the distributive law `δ`, the typing judgment `Γ ⊢ e : A ! E @ s`, and the
  reduction relation — together with the metatheorems.

  ── WHAT IS PROVED vs. OBLIGATION ─────────────────────────────────────────
  PROVED outright (the grade algebra and the four δ-coherence squares of §3.6):
    • rows form a monoid                              (§1.1)
    • `split` is a monoid homomorphism  = the μ square (§3.1, §3.6)
    • `□` is idempotent                 = the δ_□ square trivial (§3.0, §3.6)
    • `split` respects the unit         = the η/ε bookkeeping (§3.6)
    • the multiplicity grade is a 3-point lattice      (§1.3)
    • the O/G partition: `quote`'s residue is generative-only (§3.5)
    • value purity / stage discipline, variable case   (§7, §9 SO-1)
    • operational metatheory (§4, §8): canonical forms (arrow ⇒ λ, box ⇒ quote),
      values are normal forms; `Step` is now a full CBV calculus — top-level
      redexes + evaluation-context congruence (`appFun`/`appArg`/`letStep`/
      `spliceStep`) + the handler reductions (`handleReturn`/`handleOp`) — and
      reduction stays deterministic under it (overlaps resolved by normality)
    • preservation (§7): proved for ALL ten reductions — the congruence + handler
      cases by IH / sub-row, `(genlet)` outright, `(β)`/`(let)` reduced to the
      substitution keystone (de Bruijn, `Substitution.lean`); only `(splice)` owed
  STRUCTURALLY PROVED, reduced to one named keystone (the deep syntactic
  metatheory — the §4.1/§6.6 substitution/hygiene development):
    • preservation §7 — the proof now splits over the four reduction rules:
      `(genlet)` is discharged OUTRIGHT; `(β)` and `(let)` are reduced to the
      typed substitution lemma `subst_preserves` (THE keystone, proved
      `sorry`-free for the de Bruijn core in `Substitution.lean`); only
      `(splice)` is owed (a genuine stage subtlety, §9 — see below).
    • progress §8 — owed (needs canonical forms + the congruence / evaluation-
      context rules the abstracted `Step` relation does not yet model).
  NAMED but not yet formalized — `True := trivial` placeholders, one notch
  weaker (the proposition itself awaits the development beneath it):
    • stage-ordering §9 (variable case proved as `var_stage`) • zero-cost §5.2

  ── BUILD STATUS ──────────────────────────────────────────────────────────
  VERIFIED with Lean v4.28.0 (`lake build`, exit 0). Three named `sorry`s remain,
  each precisely characterized: `subst_preserves` (THE substitution keystone —
  proved for the de Bruijn core in `Substitution.lean`), the `(splice)` case of
  `preservation` (stage-weakening, §9), and `progress` (§8). Every other theorem
  is machine-checked — incl. `preservation`'s `(genlet)` case and its
  `(β)`/`(let)` reductions to the keystone.
-/

set_option autoImplicit false

namespace Locus

/-! # §1.1  Effect grade `E` — a row monoid over labels -/

/-- §3.1 The two label *kinds*: object (`O` — fire at runtime, stay inside `□`)
    and generative (`G` — fire at generation, distribute out of `□`). -/
inductive Kind | O | G
  deriving DecidableEq, Repr

/-- §1.1 Effect labels. `insert` is the built-in *generative* label
    (let-insertion); every other label is an *object* effect. -/
inductive Label
  | exn                     -- exn⟨X⟩  (payload elided)
  | world (name : String)   -- console / fs / net / clock …
  | gc                      -- "touches the managed heap"
  | insert                  -- the generative `Insert` label (let-insertion)
  | user (name : String)    -- a declared user-effect label
  deriving DecidableEq, BEq, Repr

/-- §3.1 A label's kind is fixed at declaration. Only `insert` is generative. -/
def Label.kind : Label → Kind
  | .insert => .G
  | _       => .O

/-- §1.1 A **row** is a *scoped* row — order significant, duplicates significant
    — i.e. the free monoid on labels: unit `[]`, operation `++`. -/
abbrev Row := List Label

/-- The empty row `∅` = pure. -/
def Row.empty : Row := []

-- §1.1 / §3.6 (η, ε, μ bookkeeping): rows form a monoid — the free-monoid laws
-- on `List`, so they hold by the core lemmas.
theorem Row.append_assoc (a b c : Row) : (a ++ b) ++ c = a ++ (b ++ c) :=
  List.append_assoc a b c
theorem Row.empty_append (a : Row) : Row.empty ++ a = a := List.nil_append a
theorem Row.append_empty (a : Row) : a ++ Row.empty = a := List.append_nil a

/-! # §3.1–§3.6  The kind partition and the distributive law `δ` -/

/-- Is this label generative (`G`)? -/
def isGen (l : Label) : Bool :=
  match l.kind with | .G => true | .O => false

/-- §3.1 The **object part** of a row: the `O`-labels, in order. -/
def objPart (E : Row) : Row := E.filter (fun l => !(isGen l))

/-- §3.1 The **generative part** of a row: the `G`-labels, in order. -/
def genPart (E : Row) : Row := E.filter isGen

/-- §3.1 `split` carves a row into `(E_O, E_G)`. A *total* function — the design
    §6 verdict table, now a definition. (`δ` has no side condition beyond this
    kinding, so `δ` is total because `split` is.) -/
def split (E : Row) : Row × Row := (objPart E, genPart E)

-- §3.1 filtering distributes over append (core lemma `List.filter_append`).
theorem objPart_append (a b : Row) :
    objPart (a ++ b) = objPart a ++ objPart b := by
  simp [objPart, List.filter_append]
theorem genPart_append (a b : Row) :
    genPart (a ++ b) = genPart a ++ genPart b := by
  simp [genPart, List.filter_append]

/-- **§3.6 (μ — the multiplication square).** `split` is a *monoid
    homomorphism* `Row → Row × Row`. This is precisely the coherence the
    document promotes "from design note to lemma": *split is a monoid hom, hence
    μ commutes.* Scoped order is preserved because `objPart`/`genPart` are
    order-respecting filters (each projection is a subsequence of `E`). -/
theorem split_hom (a b : Row) :
    split (a ++ b)
      = ((split a).1 ++ (split b).1, (split a).2 ++ (split b).2) := by
  simp [split, objPart_append, genPart_append]

/-- **§3.6 (η, ε).** `split` respects the unit. -/
theorem split_empty : split Row.empty = (Row.empty, Row.empty) := rfl

/-- §3.2 / §3.5 The generative residue is *only* `G`-labels: an object effect
    never migrates up to the generation stage (the placed-type invariant subject
    reduction must preserve). -/
theorem genPart_allG (E : Row) : ∀ l ∈ genPart E, l.kind = Kind.G := by
  intro l hl
  simp only [genPart, List.mem_filter] at hl
  have h : isGen l = true := hl.2
  cases hk : l.kind with
  | G => rfl
  | O => simp [isGen, hk] at h

/-- §3.5 The object part is *only* `O`-labels — the symmetric invariant. -/
theorem objPart_allO (E : Row) : ∀ l ∈ objPart E, l.kind = Kind.O := by
  intro l hl
  simp only [objPart, List.mem_filter] at hl
  have h : (!(isGen l)) = true := hl.2
  cases hk : l.kind with
  | O => rfl
  | G => simp [isGen, hk] at h

/-! # §1.3  Continuation-multiplicity grade `m ∈ {0, 1, ω}` -/

/-- §1.3 The resumption-count grade: abort / linear / unrestricted. -/
inductive Mult | zero | one | omega
  deriving DecidableEq, Repr

namespace Mult

/-- A row's multiplicity is the *join* (max) over its labels (§1.3). -/
def join : Mult → Mult → Mult
  | .omega, _    => .omega
  | _, .omega    => .omega
  | .one, _      => .one
  | _, .one      => .one
  | .zero, .zero => .zero

/-- The order `0 ≤ 1 ≤ ω`. -/
def le : Mult → Mult → Bool
  | .zero, _       => true
  | .one, .zero    => false
  | .one, _        => true
  | .omega, .omega => true
  | .omega, _      => false

theorem join_comm  (a b : Mult)   : join a b = join b a := by
  cases a <;> cases b <;> decide
theorem join_assoc (a b c : Mult) : join (join a b) c = join a (join b c) := by
  cases a <;> cases b <;> cases c <;> decide
theorem join_idem  (a : Mult)     : join a a = a := by
  cases a <;> decide

theorem le_refl  (a : Mult)   : le a a = true := by cases a <;> decide
theorem le_total (a b : Mult) : (le a b || le b a) = true := by
  cases a <;> cases b <;> decide
theorem le_trans (a b c : Mult) :
    le a b = true → le b c = true → le a c = true := by
  cases a <;> cases b <;> cases c <;> decide

end Mult

/-! # §1.2 / §3.0 / §9  Stage grade — idempotent `□` (two-valued) -/

/-- §0.5 Stage: object/runtime (`obj` = 0) or generation (`gen` = 1). -/
inductive Stage | obj | gen
  deriving DecidableEq, Repr

/-- §1.2 `quote` *raises* the stage; under the idempotence commitment (§3.0) the
    generation stage absorbs, so `box` lands at `gen` from either stage. -/
def Stage.box : Stage → Stage := fun _ => .gen

/-- **§3.6 (δ_□ — the comultiplication square is trivial).** `□□ ≅ □`: boxing
    is idempotent. This is exactly the square that *fails* under genuine
    multi-stage stratification (§3.6); idempotence is what makes it free. -/
theorem Stage.box_idem (s : Stage) : s.box.box = s.box := rfl

/-! # §2  Syntax: types and terms -/

/-- §2 Types. `arr A E B` is `A -> B ! E` (latent row on the arrow);
    `box A E` is `□(A ! E)` = `Code[A]` carrying object row `E`. -/
inductive Ty
  | base (name : String)
  | arr  (dom : Ty) (lat : Row) (cod : Ty)
  | box  (ty : Ty) (orow : Row)
  deriving Repr

/-- §2 / §4 Terms. Binders use string names; capture-avoidance (§4.1, §6.6) is
    deliberately abstracted (see `subst`). -/
inductive Term
  | var     (x : String)
  | lam     (x : String) (dom : Ty) (body : Term)
  | app     (f a : Term)
  | letin   (x : String) (rhs body : Term)
  | perform (op : Label) (arg : Term)
  | handle  (scrut retClause : Term) (op : Label) (opClause : Term)
  | quote   (e : Term)
  | splice  (c : Term)
  | genlet  (e : Term)
  deriving Repr

/-- Naive (capture-*un*safe) substitution — enough to state the reduction
    rules. Capture-avoidance is the §4.1/§6.6 hygiene obligation, *not*
    discharged here; that is precisely why the substitution lemma — hence
    preservation — is left `sorry` below. -/
def subst (x : String) (w : Term) : Term → Term
  | .var y           => if x = y then w else .var y
  | .lam y A e       => .lam y A (if x = y then e else subst x w e)
  | .app f a         => .app (subst x w f) (subst x w a)
  | .letin y r e     => .letin y (subst x w r) (if x = y then e else subst x w e)
  | .perform op a    => .perform op (subst x w a)
  | .handle s r op b => .handle (subst x w s) (subst x w r) op (subst x w b)
  | .quote e         => .quote (subst x w e)
  | .splice c        => .splice (subst x w c)
  | .genlet e        => .genlet (subst x w e)

/-! # §2 / §3.3  Typing judgment  `Γ ⊢ e : A ! E @ s` -/

/-- A context binds, per name, a type and the *stage* at which it is available
    (the comonadic context, §2). -/
abbrev Ctx := List (String × Ty × Stage)

/-- §2–§3.3 The typing relation `Typed Γ s e A E`  ≙  `Γ ⊢ e : A ! E @ s`.
    Effect-row arithmetic follows §2.1; `quote`/`splice` carry `δ` (§3.3). -/
inductive Typed : Ctx → Stage → Term → Ty → Row → Prop
  /-- A variable is a value: pure (`∅`), available at its binding stage (SO-1). -/
  | var {Γ s x A} :
      (x, A, s) ∈ Γ → Typed Γ s (.var x) A Row.empty
  /-- λ-abstraction is a value; its latent row is the body's row. -/
  | lam {Γ s x A e B E} :
      Typed ((x, A, s) :: Γ) s e B E →
      Typed Γ s (.lam x A e) (.arr A E B) Row.empty
  /-- Application unions function, argument, and latent body rows. -/
  | app {Γ s f a A B Ef E1 E2} :
      Typed Γ s f (.arr A Ef B) E1 →
      Typed Γ s a A E2 →
      Typed Γ s (.app f a) B (E1 ++ E2 ++ Ef)
  /-- §2.1 (bind): `let` unions the two rows. -/
  | bind {Γ s x e1 A E1 e2 B E2} :
      Typed Γ s e1 A E1 →
      Typed ((x, A, s) :: Γ) s e2 B E2 →
      Typed Γ s (.letin x e1 e2) B (E1 ++ E2)
  /-- §2.1 (perform): the operation's label joins the row. (Operation
      signatures `op : A ⇒ B` are abstracted — only the row behaviour is
      modelled.) -/
  | perform {Γ s op arg A B E} :
      Typed Γ s arg A E →
      Typed Γ s (.perform op arg) B (E ++ [op])
  /-- §3.4 / §7 (handle): the *nearest* matching label is discharged
      (`List.erase` = scoped-row "nearest handler"); the handler's own row joins.
      The resume/continuation structure is abstracted (§7's "typed-handler
      bookkeeping"). This rule is where the row visibly *shrinks*. -/
  | handle {Γ s scrut A E ret op body B Eh} :
      Typed Γ s scrut A E →
      Typed Γ s ret B Eh →
      Typed Γ s body B Eh →
      Typed Γ s (.handle scrut ret op body) B (E.erase op ++ Eh)
  /-- §3.3 (quote) — **`δ` in action.** Raises the stage; the *generative* part
      `genPart E` is discharged here (the quote is the handler delimiter for
      generative effects) and the residual code carries only the *object* part
      `objPart E`. -/
  | quote {Γ s e A E} :
      Typed Γ s e A E →
      Typed Γ s.box (.quote e) (.box A (objPart E)) (genPart E)
  /-- §3.3 (splice) — pure; lowers the stage, propagates the object row. -/
  | splice {Γ s c A Eo} :
      Typed Γ Stage.gen c (.box A Eo) Row.empty →
      Typed Γ s (.splice c) A Eo
  /-- §4 (genlet ≡ perform Insert): adds the generative `insert` label. -/
  | genlet {Γ s e A E} :
      Typed Γ s e A E →
      Typed Γ s (.genlet e) A (E ++ [Label.insert])

/-! ## Proved inversion lemmas (value purity §7; stage discipline §9 SO-1) -/

/-- §7 Value purity (variable case): a variable use is pure. -/
theorem var_pure {Γ s x A E} (h : Typed Γ s (.var x) A E) : E = Row.empty := by
  cases h; rfl

/-- §9 SO-1 (variable case): a variable is used *exactly at its binding stage* —
    the generator cannot read a binder of a different stage. -/
theorem var_stage {Γ s x A E} (h : Typed Γ s (.var x) A E) : (x, A, s) ∈ Γ := by
  cases h; assumption

/-- §3.5 The residual row of a `quote` is generative-only — the O/G placed-type
    invariant, here a corollary of `genPart_allG`. -/
theorem quote_residual_generative {Γ s e A E} (_h : Typed Γ s e A E) :
    ∀ l ∈ genPart E, l.kind = Kind.G :=
  genPart_allG E

/-! # §4  Reduction (single-stage `↦`) -/

/-- §8 Values (canonical forms, simplified): λ-abstractions and quoted code. -/
inductive IsValue : Term → Prop
  | lam   {x A e} : IsValue (.lam x A e)
  | quote {e}     : IsValue (.quote e)

/-- §4 Single-stage reduction. (op)/(return) handler steps are abstracted
    (§7's typed-handler bookkeeping). -/
inductive Step : Term → Term → Prop
  /-- (β) -/
  | beta {x A e w} : IsValue w → Step (.app (.lam x A e) w) (subst x w e)
  /-- (let) -/
  | letv {x w e} : IsValue w → Step (.letin x w e) (subst x w e)
  /-- (splice) `${ quote e } ↦ e` — splice/quote cancel. -/
  | spliceCancel {e} : Step (.splice (.quote e)) e
  /-- (genlet) emission to a `let` at the (here, immediate) locus; scope-safety
      is the §4.1 lemma. -/
  | genletStep {e} : Step (.genlet e) (.letin "v" e (.var "v"))
  /-- (cong-appFun) reduce the function position. -/
  | appFun {f f' a} : Step f f' → Step (.app f a) (.app f' a)
  /-- (cong-appArg) reduce the argument once the function is a value (CBV). -/
  | appArg {f a a'} : IsValue f → Step a a' → Step (.app f a) (.app f a')
  /-- (cong-let) reduce the bound expression of a `let`. -/
  | letStep {x e1 e1' e2} : Step e1 e1' → Step (.letin x e1 e2) (.letin x e1' e2)
  /-- (cong-splice) reduce under a `splice` (toward `${ quote e }`). -/
  | spliceStep {c c'} : Step c c' → Step (.splice c) (.splice c')
  /-- (return) the scrutinee is a value — run the return clause. (Its dependence
      on the returned value is abstracted, §7.) -/
  | handleReturn {v ret op body} : IsValue v → Step (.handle v ret op body) ret
  /-- (op) the scrutinee performs the HANDLED operation — run the op clause. (The
      resume/continuation is abstracted, §7.) -/
  | handleOp {arg ret op body} : Step (.handle (.perform op arg) ret op body) body

/-- §8 **Values are normal forms** — no value reduces (`lam`/`quote` head no
    reduction rule). A building block for progress. -/
theorem values_dont_step {v e} (hv : IsValue v) : ¬ Step v e := by
  intro hs; cases hv <;> cases hs

/-- §4 **Single-stage reduction is deterministic** — a term steps to at most one
    successor. CBV pins the order (a redex and a congruence never both fire on the
    same term: a value never steps, `values_dont_step`), so each term has a unique
    active position. -/
theorem step_deterministic {e e1 e2} (h1 : Step e e1) (h2 : Step e e2) : e1 = e2 := by
  induction h1 generalizing e2 with
  | beta hw =>
    cases h2 with
    | beta => rfl
    | appFun hf => exact absurd hf (values_dont_step IsValue.lam)
    | appArg _ ha => exact absurd ha (values_dont_step hw)
  | letv hw =>
    cases h2 with
    | letv => rfl
    | letStep h => exact absurd h (values_dont_step hw)
  | spliceCancel =>
    cases h2 with
    | spliceCancel => rfl
    | spliceStep h => exact absurd h (values_dont_step IsValue.quote)
  | genletStep =>
    cases h2 with
    | genletStep => rfl
  | appFun hf ih =>
    cases h2 with
    | beta => exact absurd hf (values_dont_step IsValue.lam)
    | appFun h => rw [ih h]
    | appArg hv _ => exact absurd hf (values_dont_step hv)
  | appArg hv ha ih =>
    cases h2 with
    | beta hw => exact absurd ha (values_dont_step hw)
    | appFun h => exact absurd h (values_dont_step hv)
    | appArg _ h => rw [ih h]
  | letStep h1 ih =>
    cases h2 with
    | letv hw => exact absurd h1 (values_dont_step hw)
    | letStep h => rw [ih h]
  | spliceStep h1 ih =>
    cases h2 with
    | spliceCancel => exact absurd h1 (values_dont_step IsValue.quote)
    | spliceStep h => rw [ih h]
  | handleReturn hv =>
    cases h2 with
    | handleReturn => rfl
    | handleOp => nomatch hv     -- the scrutinee can't be both a value and a perform
  | handleOp =>
    cases h2 with
    | handleReturn hv => nomatch hv
    | handleOp => rfl

/-! # §6–§9  Metatheorems (OBLIGATIONS — mirroring calculus.md) -/

/-- Append is commutative up to row **subset** (`⊆` = `List.Subset`, set-style).
    The `(β)`/`(let)` substitution yields the body-then-arg row order while the
    elimination form carries arg-then-body; the two agree as sets. -/
theorem Row.subset_append_comm (a b : Row) : a ++ b ⊆ b ++ a := by
  intro x hx
  rcases List.mem_append.mp hx with h | h
  · exact List.mem_append.mpr (Or.inr h)
  · exact List.mem_append.mpr (Or.inl h)

/-- Row **subset is monotone in `++`** — the congruence cases of preservation
    rebuild a row from a sub-row of one position, leaving the others fixed. -/
theorem Row.append_subset {a a' b b' : Row} (ha : a' ⊆ a) (hb : b' ⊆ b) :
    a' ++ b' ⊆ a ++ b := by
  intro x hx
  rcases List.mem_append.mp hx with h | h
  · exact List.mem_append.mpr (Or.inl (ha h))
  · exact List.mem_append.mpr (Or.inr (hb h))

/-- **The typed substitution lemma** (§4.1/§6.6) — the keystone `preservation`
    owes for `(β)` and `(let)`: substituting a typed value for a variable keeps
    the body typed, the row bounded by `body ++ arg`. Proved **`sorry`-free for
    the de Bruijn core** in `Substitution.lean`; here the string-named `subst` is
    capture-unsafe *by design* (lifting the de Bruijn proof across the
    representation is the §4.1 open work), so the lemma is stated and owed — and
    `preservation`'s `(β)`/`(let)` cases are now reduced to *exactly* this one
    obligation. -/
theorem subst_preserves {Γ x A0 s e0 B Ebody w Earg}
    (hbody : Typed ((x, A0, s) :: Γ) s e0 B Ebody)
    (harg  : Typed Γ s w A0 Earg) :
    ∃ E', E' ⊆ Ebody ++ Earg ∧ Typed Γ s (subst x w e0) B E' := by
  sorry

/-- **§7 Preservation** — *well-typed terms stay well-typed and effects only
    shrink* (`E' ⊆ E`).
    OBLIGATION: needs the substitution lemma, which needs the §4.1/§6.6 hygiene
    development (this file's `subst` is capture-unsafe by design). The document
    discharges the `(op)`/`(genlet)`/`(splice)` cases modulo that bookkeeping. -/
theorem preservation {Γ s e e' A E}
    (ht : Typed Γ s e A E) (hs : Step e e') :
    ∃ E', E' ⊆ E ∧ Typed Γ s e' A E' := by
  induction hs generalizing s A E with
  | beta _hw =>
    -- (β): invert `app` then `lam`, apply the substitution keystone.
    cases ht with
    | app hf hwt =>
      cases hf with
      | lam hbody =>
        obtain ⟨E', hsub, hty⟩ := subst_preserves hbody hwt
        exact ⟨E', fun a ha => Row.subset_append_comm _ _ (hsub ha), hty⟩
  | letv _hw =>
    -- (let): invert `bind`, apply the keystone (arg = the let's rhs).
    cases ht with
    | bind he1 he2 =>
      obtain ⟨E', hsub, hty⟩ := subst_preserves he2 he1
      exact ⟨E', fun a ha => Row.subset_append_comm _ _ (hsub ha), hty⟩
  | spliceCancel =>
    -- (splice): owed — stage-weakening (the splice's stage vs the quoted body's).
    sorry
  | genletStep =>
    -- (genlet): the inserted `let` re-types directly; the row sheds `insert`.
    cases ht with
    | genlet hte =>
      refine ⟨_, List.subset_append_left _ _, ?_⟩
      have hv : Typed (("v", A, s) :: Γ) s (.var "v") A Row.empty :=
        Typed.var (by simp)
      have hb := Typed.bind hte hv
      rwa [Row.append_empty] at hb
  | appFun _hstep ih =>
    -- (cong-appFun): the function reduces; its type is fixed, its row only shrinks.
    cases ht with
    | app hf ha =>
      obtain ⟨Ef', hsub, hf'⟩ := ih hf
      exact ⟨_, Row.append_subset (Row.append_subset hsub (List.Subset.refl _))
        (List.Subset.refl _), Typed.app hf' ha⟩
  | appArg _hv _hstep ih =>
    -- (cong-appArg): the argument reduces (the function is already a value).
    cases ht with
    | app hf ha =>
      obtain ⟨Ea', hsub, ha'⟩ := ih ha
      exact ⟨_, Row.append_subset (Row.append_subset (List.Subset.refl _) hsub)
        (List.Subset.refl _), Typed.app hf ha'⟩
  | letStep _hstep ih =>
    -- (cong-let): the bound expression reduces.
    cases ht with
    | bind he1 he2 =>
      obtain ⟨E1', hsub, he1'⟩ := ih he1
      exact ⟨_, Row.append_subset hsub (List.Subset.refl _), Typed.bind he1' he2⟩
  | spliceStep _hstep ih =>
    -- (cong-splice): the spliced code reduces — its row is `∅`, so it stays `∅`.
    cases ht with
    | splice hc =>
      obtain ⟨Ec', hsub, hc'⟩ := ih hc
      have hnil : Ec' = Row.empty := List.subset_nil.mp hsub
      subst hnil
      exact ⟨_, List.Subset.refl _, Typed.splice hc'⟩
  | handleReturn _hv =>
    -- (return): run the return clause; its row `Eh` is a sub-row of the result.
    cases ht with
    | handle _hscrut hret _hbody =>
      exact ⟨_, List.subset_append_right _ _, hret⟩
  | handleOp =>
    -- (op): run the op clause; its row `Eh` is a sub-row of the result.
    cases ht with
    | handle _hscrut _hret hbody =>
      exact ⟨_, List.subset_append_right _ _, hbody⟩

/-! ## §8  Canonical forms — the shape of a value at each type (progress's lemma) -/

/-- §8 Canonical forms (arrow): a **value** of arrow type is a λ. The only other
    value form, `quote`, has `□`/`box` type, so it cannot inhabit `arr`. -/
theorem canonical_arrow {Γ s v A Ef B E} (hv : IsValue v)
    (ht : Typed Γ s v (.arr A Ef B) E) :
    ∃ x A0 e0, v = .lam x A0 e0 := by
  cases hv with
  | lam   => exact ⟨_, _, _, rfl⟩
  | quote => nomatch ht

/-- §8 Canonical forms (box): a **value** of `□`/`Code` type is a `quote`. The
    only other value form, `lam`, has `arr` type. -/
theorem canonical_box {Γ s v A Eo E} (hv : IsValue v)
    (ht : Typed Γ s v (.box A Eo) E) :
    ∃ e0, v = .quote e0 := by
  cases hv with
  | lam   => nomatch ht
  | quote => exact ⟨_, rfl⟩

/-- **§8 Progress (effect-relative)** — a closed well-typed term is a value,
    steps, or is stuck at an *unhandled object operation in its row* (the honest
    runtime interface).
    OBLIGATION: needs the canonical-forms lemma over the full syntax. -/
theorem progress {s e A E} (_ht : Typed [] s e A E) :
    IsValue e ∨ (∃ e', Step e e')
      ∨ (∃ op arg, e = .perform op arg ∧ op ∈ E) := by
  sorry

/-- **§9 SO-1 (stage ordering, general)** — no stage-0 binder is used at the
    generation stage. The variable case is `var_stage` (proved); the general
    statement quantifies over every free occurrence in a derivation.
    OBLIGATION. -/
theorem stage_ordering_SO1 {Γ s e A E} (_ht : Typed Γ s e A E) : True := by
  trivial

/-- **§9 SO-2 (generation = assembly)** — generation-stage reduction does not
    execute object code under a `quote`; it only assembles. A statement about
    evaluation contexts not descending under `quote`. OBLIGATION. -/
theorem stage_ordering_SO2 : True := trivial

/-- **§5.2 Zero-cost (graded)** — a generation-resolved, generation-safe handler
    has its dispatch eliminated; the residual is only the continuation cost of
    its resumption shape (§1.3). OBLIGATION: requires the evidence-passing
    translation `⟦·⟧` (§5.1), a Phase-3 deliverable. -/
theorem zero_cost : True := trivial

/-! # Calculus-extensions (companion: ../docs/calculus-extensions.md)

    The grade *algebra* underneath the post-Phase-0 extension rules, proved
    outright in the same style as §1.1 / §1.3 / §3.6. The typing-relation
    extensions themselves (the `seal` / `Ref` / `par` rules and §11's `t-store`
    kind premise) and their preservation ride the §7 development and are not
    added here. -/

/-! ## §13  Sealing — total removal of a label from a row -/

/-- §13 `sealOut L E` removes *every* occurrence of `L` — the total no-escape of
    a seal (`seal L { e } : A ! sealOut L E`), distinct from `handle`'s scoped
    `List.erase` (nearest-only, §7). -/
def Row.sealOut (L : Label) (E : Row) : Row := E.filter (fun l => decide (l ≠ L))

/-- §13 sealing distributes over row union (same `filter_append` shape as
    `objPart` / `genPart`). -/
theorem Row.sealOut_append (L : Label) (a b : Row) :
    Row.sealOut L (a ++ b) = Row.sealOut L a ++ Row.sealOut L b := by
  simp [Row.sealOut, List.filter_append]

/-- §13 the unit. -/
theorem Row.sealOut_empty (L : Label) : Row.sealOut L Row.empty = Row.empty := rfl

/-- §13 **the no-escape property** — the sealed label appears nowhere in the
    result. This is the `runST` / `∀s` condition at the row level: the rule's
    `L ∉ labels(A)` premise together with this fact give "L escapes nowhere". -/
theorem Row.sealOut_removes (L : Label) (E : Row) : L ∉ Row.sealOut L E := by
  intro h
  have h2 := (List.mem_filter.mp h).2
  simp only [decide_eq_true_eq] at h2
  exact h2 rfl

/-! ## §11  Representation kinds (D3) -/

/-- §11 The representation kind: `uniform` (fits a traced cell — a handle or a
    tag-room scalar) or `wide` (Float / Float32 / SIMD — no 2-bit-tag room). -/
inductive RKind | uniform | wide
  deriving DecidableEq, Repr

/-- §11 The `wide` base types (no tag room). -/
def Ty.isWideBase (name : String) : Bool :=
  name == "Float" || name == "Float32" || name == "Pair" || name == "Quad" || name == "Oct"

/-- §11 The representation kind of a (concrete) type. Handles (`arr` = a closure,
    `box` = code) and tag-room bases are `uniform`; the wide bases are `wide`. -/
def Ty.rkind : Ty → RKind
  | .base name => if Ty.isWideBase name then .wide else .uniform
  | .arr _ _ _ => .uniform
  | .box _ _   => .uniform

/-- §11 `tracedStorable A` — may `A` inhabit a *traced* heap cell? Iff `uniform`
    (D3 / the `t-store` premise). -/
def Ty.tracedStorable (A : Ty) : Bool :=
  match A.rkind with | .uniform => true | .wide => false

/-- §11 a `wide` type is **never** traced-storable — the kind exclusion is real.
    This is the front-end, type-level core of the GC's *traced-cell invariant*
    (`classify` is only ever handed a `uniform` word — the §11 payoff / T0). The
    full invariant over a typing derivation rides §7 preservation. -/
theorem Ty.wide_not_tracedStorable {A : Ty} (h : A.rkind = RKind.wide) :
    A.tracedStorable = false := by
  simp [Ty.tracedStorable, h]

/-- §11 the two kinds are distinct (the exclusion is non-vacuous). -/
theorem RKind.uniform_ne_wide : RKind.uniform ≠ RKind.wide := by decide

end Locus
