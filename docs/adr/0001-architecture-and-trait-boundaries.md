# ADR-0001: Architecture & module/trait boundaries

- Status: Proposed
- Date: 2026-06-08
- Deciders: bbarker@barkbite.org (owner), engineering

## Context

Cerberus must let us rewrite any subsystem later (rendering especially is
"undifferentiated heavy lifting") without disturbing its callers, and must keep
memory as priority #1. We need an architecture that makes module replacement a
local change and makes the privacy guarantees structural rather than policy-based.

## Decision

Adopt **ports & adapters (hexagonal architecture)** over a Cargo **workspace**
with **one crate per subsystem**.

1. **Traits are the seams.** Each subsystem crate defines the trait(s) that
   describe its capability (the "port"). The mandated minimum set: `JsEngine`,
   `LayoutEngine`, `Rasterizer`, `TextShaper`, `ImageDecoder`, `TlsProvider`,
   `Aead`, `Kdf`, `CookieStore`, `Vault`, `FarblingProvider`.

2. **Adapters are separate crates.** Every third-party dependency is wrapped in a
   dedicated adapter crate that implements a trait. **No foreign/vendor type
   crosses a module boundary** — callers depend only on our traits and our types
   (`cerberus-types`). For example, rustls types never appear in a signature;
   `TlsProvider` hands back a `Box<dyn ReadWrite>`.

3. **Composition root.** The single binary `cerberus-app` is the only place that
   names concrete adapters; it wires them by dependency injection (constructor
   parameters / boxed trait objects). Swapping an adapter is a change *there* and
   nowhere else.

4. **Foundation crate.** `cerberus-types` holds shared value types and depends
   only on `std`, so every crate can use it without coupling to policy.

5. **The modularity test (CI-able):** delete an adapter crate, write a new one
   implementing the same trait, and the rest of the workspace compiles unchanged.

### Storage sealing (structural privacy)

The headline privacy property — an instance can only ever resolve its own
cookies — is enforced **by construction, not by a check**:

- `StorageEnvironment::instance(id)` returns an `InstanceStore` handle that
  borrows *only that instance's partition*.
- No method on `InstanceStore` accepts a foreign `InstanceId`, so there is no
  code path that reads another instance's cookies. Cross-instance correlation is
  impossible because the API to do it does not exist.
- The vault is likewise partitioned by `InstanceId`, and ciphertext is bound to
  its `(instance, key)` slot via the AEAD's associated data, so a blob cannot be
  relocated across instances without failing authentication.

This is the realization of the mandated `CookieStore`/`Vault` capability. (See
PLAN §10 for the open question of also exposing it as a named `CookieStore`
trait.)

### Lints & safety posture

- `unsafe_code = "deny"` workspace-wide (overridable per adapter crate that needs
  FFI, e.g. V8/crypto — opt-in is explicit and reviewable). The scaffold has zero
  unsafe.
- `clippy::all` and `rust_2018_idioms` as warnings; CI treats warnings as errors.

## Consequences

**Easier:** swapping engines/rasterizers/TLS; testing subsystems in isolation
with fake adapters (already done — the tests inject null/stub/test adapters);
keeping memory control centralized (the composition root decides lifecycles).

**Harder / costs:** more crates and some boilerplate (trait + adapter per
dependency); a little indirection via trait objects (acceptable — these are
coarse-grained boundaries, not hot loops; where a hot path matters we can use
generics/monomorphization behind the same trait).

**Accepted trade-off:** dynamic dispatch at subsystem boundaries in exchange for
hard modularity and testability.

## Alternatives considered

- **Single crate with modules.** Less boilerplate, but nothing *enforces* the
  boundaries — foreign types leak, and "swap a subsystem" becomes a refactor.
  Rejected: the modularity mandate is non-negotiable.
- **Traits co-located with their adapters.** Makes the adapter un-deletable
  without breaking callers. Rejected: defeats the swap test.
- **Policy-checked isolation** (a guard that compares `instance_id` on read).
  Rejected: a check can be bypassed or mis-wired; we want the leak to be
  *unrepresentable*.
