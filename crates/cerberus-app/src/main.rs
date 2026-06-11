//! Cerberus command-line entry point.
//!
//! Subcommands (argument parsing is hand-rolled — no `clap` until approved):
//!   render    Render a page to a PPM file and print a summary.
//!   mem-gate  Render, then assert resident memory is within budget (CI gate).
//!   version   Print the version.
//!   help      Print usage.

use cerberus_app::{head_switch_rss, render, resident_set_kb, RenderConfig};
use cerberus_types::{Color, Size};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = match args.split_first() {
        Some((cmd, rest)) => (cmd.as_str(), rest),
        None => ("render", &[][..]),
    };

    match command {
        "run" => cmd_run(rest),
        "render" => cmd_render(rest),
        "mem-gate" => cmd_mem_gate(rest),
        "version" | "--version" | "-V" => {
            println!("cerberus {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "help" | "--help" | "-h" => {
            print_usage();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown command: {other}\n");
            print_usage();
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "windowing")]
fn cmd_run(args: &[String]) -> ExitCode {
    let fullscreen = has_flag(args, "--fullscreen");
    let app = cerberus_app::BrowserApp::with_config(cerberus_app::AppOptions {
        system_roots: has_flag(args, "--system-roots"),
        data_dir: flag(args, "--data-dir").map(std::path::PathBuf::from),
    });
    match cerberus_shell_winit::run(app, fullscreen) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("could not open a window: {e}");
            eprintln!("(a display server is required; use `render` for headless output)");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(feature = "windowing"))]
fn cmd_run(_args: &[String]) -> ExitCode {
    eprintln!("this build has no windowing support; rebuild with --features windowing");
    ExitCode::FAILURE
}

fn cmd_render(args: &[String]) -> ExitCode {
    let out = flag(args, "--out").unwrap_or_else(|| "cerberus-home.ppm".to_string());
    let mut config = RenderConfig::default();
    if let Some(url) = flag(args, "--url") {
        config.url = url;
    }
    if let (Some(w), Some(h)) = (
        flag(args, "--width").and_then(|s| s.parse().ok()),
        flag(args, "--height").and_then(|s| s.parse().ok()),
    ) {
        config.viewport = Size::new(w, h);
    }
    config.headed = has_flag(args, "--headed");
    config.system_roots = has_flag(args, "--system-roots");
    config.data_dir = flag(args, "--data-dir");
    config.background = Color::WHITE;

    let outcome = match render(&config) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("render failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = cerberus_headless::write_ppm(&out, &outcome.framebuffer) {
        eprintln!("could not write {out}: {e}");
        return ExitCode::FAILURE;
    }

    println!(
        "rendered {} ({}x{})",
        outcome.url, outcome.viewport.w, outcome.viewport.h
    );
    println!("  http status     : {}", outcome.status);
    println!(
        "  toolbar + page  : 36px toolbar + {}x{} content",
        outcome.content_size.w, outcome.content_size.h
    );
    println!("  active head     : {}", outcome.active_head);
    println!(
        "  js engine       : {} (engines live: {}, realms: {})",
        outcome.engine_name, outcome.engines_live, outcome.realms_live
    );
    println!("  page scripts    : {} executed", outcome.scripts_ran);
    println!("  active cookies  : {}", outcome.active_cookies);
    println!(
        "  images          : {}/{} decoded",
        outcome.images_decoded, outcome.images_requested
    );
    println!("  3rd-party access : {:?}", outcome.third_party_decision);
    println!("  blocked subres  : {}", outcome.subresources_blocked);
    println!("  wrote           : {out}");
    if let Some(kb) = resident_set_kb() {
        println!("  resident memory : {:.1} MB", kb as f64 / 1024.0);
    }
    ExitCode::SUCCESS
}

fn cmd_mem_gate(args: &[String]) -> ExitCode {
    let budget_mb: f64 = flag(args, "--budget-mb")
        .and_then(|s| s.parse().ok())
        .unwrap_or(64.0);

    // Render the built-in page; this exercises the whole pipeline including a
    // live JS engine instance for the active head.
    if let Err(e) = render(&RenderConfig::default()) {
        eprintln!("mem-gate render failed: {e}");
        return ExitCode::FAILURE;
    }

    let Some(kb) = resident_set_kb() else {
        eprintln!("mem-gate: resident memory unavailable on this platform; skipping");
        return ExitCode::SUCCESS;
    };

    let mb = kb as f64 / 1024.0;
    if mb > budget_mb {
        eprintln!("mem-gate FAIL: resident {mb:.1} MB > budget {budget_mb:.1} MB");
        return ExitCode::FAILURE;
    }
    println!("mem-gate PASS: resident {mb:.1} MB <= budget {budget_mb:.1} MB");

    // Head-switch leak gate (PLAN §5): RSS after N switches stays within +10%
    // of the pre-switch idle (with a 2 MB absolute floor so tiny baselines
    // aren't judged by allocator noise). Proves engine teardown leaks neither
    // realms nor heap.
    let switches: usize = flag(args, "--switches")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if switches > 0 {
        let Some((before, after)) = head_switch_rss(switches) else {
            eprintln!("mem-gate: switch leak check unavailable on this platform; skipping");
            return ExitCode::SUCCESS;
        };
        let (before_mb, after_mb) = (before as f64 / 1024.0, after as f64 / 1024.0);
        let allowed_mb = (before_mb * 1.10).max(before_mb + 2.0);
        if after_mb > allowed_mb {
            eprintln!(
                "mem-gate FAIL: {after_mb:.1} MB after {switches} head switches (pre-switch {before_mb:.1} MB, allowed {allowed_mb:.1} MB)"
            );
            return ExitCode::FAILURE;
        }
        println!(
            "mem-gate PASS: {after_mb:.1} MB after {switches} head switches (pre-switch {before_mb:.1} MB, allowed {allowed_mb:.1} MB)"
        );
    }
    ExitCode::SUCCESS
}

fn print_usage() {
    println!(
        "cerberus — a privacy-first, memory-lean browser (M0 scaffold)\n\n\
         USAGE:\n\
         \x20 cerberus <command> [options]\n\n\
         COMMANDS:\n\
         \x20 run        Open the browser in a window (needs a display)\n\
         \x20 render     Render a page to PPM and print a summary (headless)\n\
         \x20 mem-gate   Render and assert resident memory within budget\n\
         \x20 version    Print the version\n\
         \x20 help       Print this help\n\n\
         RUN OPTIONS:\n\
         \x20 --fullscreen        start borderless-fullscreen (F11 toggles)\n\
         \x20 --system-roots      trust the OS cert store (TLS-inspecting proxies)\n\
         \x20 --data-dir <DIR>    persistent profile (cookies, vault, heads);\n\
         \x20                     omit for fully-ephemeral (default)\n\n\
         RENDER OPTIONS:\n\
         \x20 --url <URL>          default: cerberus:home\n\
         \x20 --out <FILE>         default: cerberus-home.ppm\n\
         \x20 --width <PX>         viewport width\n\
         \x20 --height <PX>        viewport height\n\
         \x20 --headed            enable consent prompts\n\
         \x20 --system-roots      trust the OS cert store (TLS-inspecting proxies)\n\
         \x20 --data-dir <DIR>    persistent profile (cookies survive runs)\n\n\
         MEM-GATE OPTIONS:\n\
         \x20 --budget-mb <MB>     default: 64\n\
         \x20 --switches <N>       also assert RSS within +10% after N head switches"
    );
}

/// Read `--key value` from args.
fn flag(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Whether a boolean `--flag` is present.
fn has_flag(args: &[String], key: &str) -> bool {
    args.iter().any(|a| a == key)
}
