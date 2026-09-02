# agents.md

## Purpose

This document defines how LLM agents should reason about design and implementation decisions in this codebase.

The default expectation is correct, minimal, readable code that solves the task actually in front of you — not maximal flexibility, extensibility, or architectural elegance.

---

## Existence Rule (Hard Gate)

No component may be introduced without both:

1. A **spec reference** that requires it
2. A clearly defined **role**

If either is missing, the component is invalid and must not be created.

This rule applies to:

* classes, structs, modules
* interfaces and abstractions
* utilities and helpers
* services, managers, handlers, etc.

---

### Spec Requirement

Every component must trace directly to a concrete requirement.

At minimum:

* what behavior requires this component
* what inputs/outputs it participates in
* what invariant or rule it enforces

If removing the component does not break a requirement, it should not exist.

---

### Role Requirement

Every component must have a **single, clear responsibility**.

It must be expressible as:

* one sentence
* one job
* one reason to change

Avoid vague roles like:

* Manager
* Service
* Helper
* Base

Prefer explicit behavioral roles:

* Validator
* Policy
* Adapter
* Strategy
* Planner
* Formatter

If a component cannot be described without "and", it likely contains multiple roles and should be split.

---

### Enforcement Heuristics

Before adding a component, answer:

* What spec requires this?
* What role does this fulfill?
* What breaks if this is removed?
* Why is this not part of an existing role?

If these cannot be answered clearly, do not proceed.

---

## Priority Order

When principles conflict, resolve them in this order:

1. **YAGNI** — You Aren't Gonna Need It
2. **KISS** — Keep It Simple
3. **DRY** — Don't Repeat Yourself
4. **SOLID**

These principles apply only after a component has passed the Existence Rule.

Do not treat lower-priority principles as justification for violating higher-priority ones.

---

## Principle Definitions

**YAGNI** — Solve the current problem only. Do not add abstractions, extension points, or generic machinery unless required by real, present use cases.

Watch for:

* interfaces with one implementation
* parameters with one known value
* virtual/overridable behavior with one override
* speculative hooks
* comments about hypothetical future reuse

---

**KISS** — Choose the simplest solution that fully satisfies the requirement.

Prefer code that is easy to read, debug, and modify over code that is clever or over-engineered.

Watch for:

* excessive indirection
* abstraction hiding simple logic
* compressed one-liners that need explanation

---

**DRY** — Keep each piece of knowledge in one authoritative place.

Deduplicate when two things represent the same rule or invariant — not merely when they look similar.

Watch for:

* copy-pasted algorithms
* duplicated constants
* repeated validation rules
* parallel conditionals that must evolve together

---

**SOLID** — Apply when it materially improves correctness, changeability, or testability.

Interpretation:

* **Single Responsibility**: each unit owns one role
* **Open/Closed**: extend via composition, not mutation
* **Liskov Substitution**: only promise substitutability when it is real
* **Interface Segregation**: expose minimal role contracts
* **Dependency Inversion**: depend on roles, not concretions

---

## Conflict Resolution

* **YAGNI over SOLID**: Do not introduce abstractions without real need
* **KISS over DRY**: Prefer clarity over forced deduplication
* **DRY for knowledge, not shape**: Similar code is not necessarily duplication
* **SOLID selectively**: Use it when it pays for itself

---

## Decision Rules

### Abstraction

Do not introduce a new abstraction unless:

* It is required by a current spec AND has a clearly defined role
* AND at least one of:

  * Two real use cases already share meaningfully common behavior
  * It removes repeated knowledge, not just repeated syntax
  * It materially improves correctness, testability, or change isolation

---

### Polymorphism and Interfaces

Do not create interfaces for single implementations.

An interface without multiple real implementations is a role without evidence.

Introduce polymorphism only when interchangeable behavior is required by the spec.

---

### Generics and Templates

Use generics only when real call sites already require them.

Do not generalize for theoretical reuse.

---

### Dependency Injection

Inject dependencies when it clearly improves testability, isolation, or substitution.

Do not inject purely for style.

---

### Duplication

Remove duplication when the same rule must remain consistent.

Keep duplication when merging would reduce clarity.

---

### Error Handling

Handle real failure modes required by the spec.

Do not add defensive behavior for out-of-scope scenarios.

---

### Compatibility and Fallbacks

Do not add backward-compatibility shims by default.

Do not add fallback code paths by default.

Only add either when a current spec explicitly requires them for a named path.

If required, document:

* the exact spec reference
* the removal condition
* the owner and review date

---

## Behavioral Defaults

**Do:**

* Implement only what the task requires
* Prefer concrete types and straightforward control flow
* Keep units narrowly focused
* Consolidate repeated knowledge when the rule is real
* Inject dependencies when it clearly helps
* Stay consistent with surrounding code

**Do not:**

* Add scaffolding for imagined future work
* Introduce interfaces or abstractions without need
* Add fallback paths unless required
* Generalize APIs prematurely
* Replace clear duplication with confusing indirection
* Introduce components without a clear spec and role
* Keep stale stubs, compatibility shims, or dead files after feature migration
* Leave temporary debug logs/artifacts in repo root or tracked paths

---

## Review Heuristics

Look for and call out:

* Speculative generalization
* Unnecessary complexity
* Duplicated knowledge
* Oversized responsibilities
* Hard-coded dependencies
* Premature polymorphism
* Ceremonial architecture
* Missing role: unclear responsibility
* Missing spec: no traceable requirement

---

## Unstaged Change Hygiene

Before finalizing work, scan unstaged changes for:

* stub files (`*.stub.cpp`) and temporary bridge scaffolding
* legacy/compat naming (`legacy`, `compat`, `fallback`) that no longer maps to a spec requirement
* temporary logs and ad-hoc output files (`*.log`, scratch outputs) accidentally left in repo root

Remove anything that does not satisfy a current spec + role.

---

## Assumptions and Missing Details

When requirements are incomplete:

* choose the simplest valid assumption
* keep scope narrow
* document meaningful assumptions briefly

Do not expand scope due to ambiguity.

---

## Output Expectations

When proposing changes, explain:

* What requirement they satisfy
* What spec requires each new component
* What role each new component fulfills
* What complexity they avoid
* What tradeoff was chosen and why

Do not justify designs based on hypothetical future needs.

---

## Default Bias

When in doubt, choose the solution that is:

1. Sufficient
2. Simple
3. Clear
4. Hard to misuse

Remove abstraction, narrow scope, and keep the code boring.

---

## If a component cannot justify its existence through a spec and a role, it is not incomplete — it is incorrect.

---