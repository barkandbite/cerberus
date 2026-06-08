//! Cerberus command-line entry point.
//!
//! Subcommands (argument parsing is hand-rolled — no `clap` until approved):
//!   render    Render a page to a PPM file and print a summary.
//!   mem-gate  Render, then assert resident memory is within budget (CI gate).
//!   version   Print the version.
//!   help      Print usage.

use cerberus_app::{render, resident_set_kb, RenderConfig};
use cerberus_types::{Color, Size};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = match args.split_first() {
        Some((cmd, rest)) => (cmd.as_str(), rest),
        None => ("render", &[][..]),
    };

    match command {
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
    println!("  active head     : {}", outcome.active_head);
    println!(
        "  js engine       : {} (engines live: {}, realms: {})",
        outcome.engine_name, outcome.engines_live, outcome.realms_live
    );
    println!("  active cookies  : {}", outcome.active_cookies);
    println!("  3rd-party access : {:?}", outcome.third_party_decision);
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
    if mb <= budget_mb {
        println!("mem-gate PASS: resident {mb:.1} MB <= budget {budget_mb:.1} MB");
        ExitCode::SUCCESS
    } else {
        eprintln!("mem-gate FAIL: resident {mb:.1} MB > budget {budget_mb:.1} MB");
        ExitCode::FAILURE
    }
}

fn print_usage() {
    println!(
        "cerberus — a privacy-first, memory-lean browser (M0 scaffold)\n\n\
         USAGE:\n\
         \x20 cerberus <command> [options]\n\n\
         COMMANDS:\n\
         \x20 render     Render a page to PPM and print a summary\n\
         \x20 mem-gate   Render and assert resident memory within budget\n\
         \x20 version    Print the version\n\
         \x20 help       Print this help\n\n\
         RENDER OPTIONS:\n\
         \x20 --url <URL>          default: cerberus:home\n\
         \x20 --out <FILE>         default: cerberus-home.ppm\n\
         \x20 --width <PX>         viewport width\n\
         \x20 --height <PX>        viewport height\n\
         \x20 --headed            enable consent prompts\n\n\
         MEM-GATE OPTIONS:\n\
         \x20 --budget-mb <MB>     default: 64"
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
