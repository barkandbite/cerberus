# ADR-0065: Parallel image fetch in the one-shot render path

- Status: Accepted
- Date: 2026-06-24
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

The multi-site goal asks that pages render **efficiently (fast)**. Measuring real
renders showed the engine is *not* the cost — `bench` is ~33 ms and cascade is
indexed (ADR-0047). The wall-time is **network**, and specifically **image
sub-resource fetching**, which `fetch_images_sync` did **serially**: one blocking
GET per image, in a loop. An image-dense page paid a round-trip per image in series
— Slickdeals (324 images) took ~15.6 s, Wikipedia ~9.6 s, almost all of it waiting
on image GETs one at a time.

The interactive browser already fetches subresources on a 4-worker `NetLoader`
pool; only the one-shot/headless `render()` path was serial.

## Decision

Fetch (and decode) images on a **bounded worker pool** (`IMAGE_FETCH_CONCURRENCY =
8`) using `std::thread::scope`, so the independent image GETs overlap instead of
serializing. The network client (`Router`) is already `Send + Sync` (the
`NetLoader` shares it across threads), so no new abstraction is needed.

Behavior preserved exactly except for the overlap:
- **Consent gate unchanged** — applied up front in one lock pass, in document
  order; unruled third-party images are recorded `Blocked` and never fetched.
- **Decode-memory budget unchanged** (`IMAGE_DECODE_BUDGET_BYTES = 16 MiB`) —
  workers check a shared atomic before starting a new image; once spent, no new
  image is started and the remainder lay out as their reserved placeholder box
  (`Pending`). The only relaxation is a tiny, bounded overshoot (≤ pool-size
  images may already be decoding when the budget is reached), well within the
  64 MB budget.
- Work is pulled in **document order** (an atomic cursor), so the
  earliest/most-likely-visible images win the budget.

## Consequences

- **~2× faster on image-heavy pages**, network-bound as before but overlapped:
  Slickdeals **15.6 s → 7.7 s**, Wikipedia **9.6 s → 4.0 s** (GitHub is
  image-light, so little change). No engine logic changed.
- Memory unchanged in practice: the 16 MiB decode budget still bounds resident
  bitmaps; `mem-gate` stays at 7.4 MB (it renders the image-less builtin page),
  and image-heavy renders stay far under 64 MB.
- All gates green: `fmt`, `clippy -D warnings`, `cargo test --workspace`,
  `mem-gate --budget-mb 64`, `bench` (32.5 ms). No new third-party deps
  (`std::thread::scope`, stable since Rust 1.63).
- Remaining wall-time is the rest of the network path (the HTML GET, render-
  blocking CSS, and — on client-rendered SPAs — the JS/chunk fetches). Those are
  separate, smaller follow-ups; image fetching was the dominant term.

## Alternatives considered

- **Cap the image count (e.g. first N):** faster but drops visible content on
  dense above-the-fold layouts; the decode-memory budget already bounds resident
  memory, so a hard count cap is unnecessary.
- **Reuse the interactive `NetLoader` pool:** it's built around the live event
  loop and channel plumbing; for the synchronous one-shot path a scoped pool is
  simpler and borrows the existing client/consent by reference with no `'static`
  or `Arc` churn.
- **Async runtime (tokio):** a large dependency and architectural shift for a
  problem a bounded thread pool solves; rejected (no foreign deps, memory-first).
