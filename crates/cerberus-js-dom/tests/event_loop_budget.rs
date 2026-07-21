//! The event loop's wall-clock budget — the guard that keeps a heavy or runaway
//! page from freezing the UI thread (the cnn.com "Not Responding" report).
//!
//! `run_event_loop` already bounded a drain by macrotask count and virtual time,
//! but neither says anything about *real* time: a self-rescheduling 0-delay
//! timer that does real work per tick can hold the thread for many seconds inside
//! one drain. `EventLoopBudget::max_wall_ms` (used by the windowed app via
//! `EventLoopBudget::interactive()`) makes the drain yield promptly so the caller
//! can re-pump with a live message pump in between.

use cerberus_js::{JsEngine, JsEngineFactory};
use cerberus_js_dom::{run_event_loop, run_scripts, EventLoopBudget};
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_types::RealmId;

fn engine_and_realm() -> (Box<dyn JsEngine>, RealmId) {
    let mut engine = QuickJsEngineFactory.instantiate().expect("instantiate");
    let realm = RealmId::from_u64_pair(0, 1);
    engine.create_realm(realm).expect("create realm");
    (engine, realm)
}

#[test]
fn wall_clock_budget_yields_before_a_runaway_page_freezes_the_thread() {
    let (mut engine, realm) = engine_and_realm();
    // A 0-delay self-rescheduling timer whose callback burns a little CPU each
    // tick — the exact shape that never advances the virtual clock, so only a
    // cap can stop it. Without the wall cap this runs to the 10k task cap
    // synchronously; that many heavy ticks in one main-thread burst is the
    // freeze.
    run_scripts(
        &mut *engine,
        realm,
        &[r#"
        function spin(){ let s=0; for(let i=0;i<20000;i++){ s+=i; } return s; }
        function tick(){ spin(); setTimeout(tick, 0); }
        setTimeout(tick, 0);
        "#
        .to_string()],
    )
    .expect("register timer");

    // A tight wall budget with an effectively unreachable task cap: the wall
    // clock MUST be what stops the drain, proving the thread is yielded promptly
    // rather than after millions of tasks.
    let budget = EventLoopBudget {
        max_tasks: 5_000_000,
        max_virtual_ms: 60_000,
        max_wall_ms: 20,
    };
    let start = std::time::Instant::now();
    let stats = run_event_loop(&mut *engine, realm, budget).expect("drain");
    let elapsed = start.elapsed();

    assert!(
        stats.hit_wall_cap,
        "drain must yield on the wall-clock budget, got {stats:?}"
    );
    assert!(
        !stats.hit_task_cap,
        "the task cap must not be what stopped it, got {stats:?}"
    );
    assert!(stats.tasks_run >= 1 && stats.tasks_run < 5_000_000);
    // Generous ceiling: the 20ms budget plus at most one in-flight task's slop —
    // nowhere near the multi-second stall that trips the OS watchdog.
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "yielded in {elapsed:?}, far from a freeze"
    );
}

#[test]
fn zero_wall_budget_preserves_full_batch_draining() {
    let (mut engine, realm) = engine_and_realm();
    // A bounded chain of five timers, then done. With no wall cap (the batch /
    // headless default) the drain empties the queue and neither cap trips — the
    // deterministic contract the WPT/probe paths rely on is unchanged.
    run_scripts(
        &mut *engine,
        realm,
        &[r#"
        let n = 0;
        function tick(){ if (++n < 5) setTimeout(tick, 0); }
        setTimeout(tick, 0);
        "#
        .to_string()],
    )
    .expect("register");

    let stats = run_event_loop(
        &mut *engine,
        realm,
        EventLoopBudget {
            max_tasks: 10_000,
            max_virtual_ms: 60_000,
            max_wall_ms: 0,
        },
    )
    .expect("drain");

    assert!(
        !stats.hit_wall_cap && !stats.hit_task_cap,
        "a bounded chain drains fully under the batch budget: {stats:?}"
    );
    assert_eq!(stats.tasks_run, 5);
}
