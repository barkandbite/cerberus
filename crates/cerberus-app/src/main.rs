//! Cerberus command-line entry point.
//!
//! With **no arguments** (e.g. double-clicking the .exe) a desktop build opens
//! the browser window (`run`); a headless build renders the default page
//! (`render`). See `default_command`.
//!
//! Subcommands (argument parsing is hand-rolled — no `clap` until approved):
//!   run       Open the browser in a window (desktop build).
//!   render    Render a page to a PPM file and print a summary (headless).
//!   diff      Score a render against a headless-Chrome reference PNG.
//!   mem-gate  Render, then assert resident memory is within budget (CI gate).
//!   version   Print the version.
//!   help      Print usage.

use cerberus_app::{head_switch_rss, render, resident_set_kb, RenderConfig};
use cerberus_types::{Color, Size};
use std::process::ExitCode;
use zeroize::Zeroize;

fn main() -> ExitCode {
    // Fingerprint coherence: our JS `Intl`/persona presents an America/New_York
    // browser, but QuickJS's `Date` reads the host zone (UTC in a container),
    // so `Date.getTimezoneOffset()` would disagree with `Intl` — a bot tell.
    // Pin the process zone to match the persona. (Per-head time zones move to a
    // JS `Date` shim driven by the profile once profiles are wired through.)
    if std::env::var_os("TZ").is_none() {
        std::env::set_var("TZ", "America/New_York");
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = match args.split_first() {
        Some((cmd, rest)) => (cmd.as_str(), rest),
        // No arguments — e.g. double-clicking the .exe in a file manager. A
        // desktop (windowing) build opens the browser window, which is what that
        // gesture means to a user; a headless build renders the default page.
        None => (default_command(), &[][..]),
    };

    match command {
        "run" => cmd_run(rest),
        "render" => cmd_render(rest),
        "diff" => cmd_diff(rest),
        "probe" => cmd_probe(rest),
        "mem-gate" => cmd_mem_gate(rest),
        "bench" => cmd_bench(rest),
        "mirror-bench" => cmd_mirror_bench(rest),
        "cookies" => cmd_cookies(rest),
        "identities" => cmd_identities(rest),
        "profile" => cmd_profile(rest),
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
    let opts = cerberus_app::AppOptions {
        system_roots: has_flag(args, "--system-roots"),
        data_dir: flag(args, "--data-dir").map(std::path::PathBuf::from),
        proxy: flag(args, "--proxy"),
    };
    // Multi-window mirror mode: every profile identity gets its own window, all
    // driven from the master (ADR-0017/0018).
    if has_flag(args, "--mirror") {
        let shell = match cerberus_app::build_mirror_shell(opts) {
            Ok(shell) => shell,
            Err(e) => {
                eprintln!("could not build the mirror group: {e}");
                return ExitCode::FAILURE;
            }
        };
        return match cerberus_shell_winit::run_multi(shell) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("could not open windows: {e}");
                eprintln!("(a display server is required; use `render` for headless output)");
                ExitCode::FAILURE
            }
        };
    }
    let app = cerberus_app::BrowserApp::with_config(opts);
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
    config.dump_text = has_flag(args, "--dump-text");
    config.proxy = flag(args, "--proxy");
    config.timers = has_flag(args, "--timers");
    if let Some(engine) = flag(args, "--engine") {
        config.layout_engine = cerberus_app::LayoutEngineKind::parse(&engine);
    }
    if let Some(mode) = flag(args, "--images") {
        config.image_mode = cerberus_app::ImageDisplayMode::parse(&mode);
    }
    // Per-image text-only overrides: `--text-only-image <substr>` (repeatable),
    // matched against each resolved image URL.
    config.text_only_images = flags(args, "--text-only-image");
    config.background = Color::WHITE;

    let outcome = match render(&config) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("render failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Clickability audit: dump the link + form-control hit boxes as JSON lines
    // (one object per box, content coordinates), for scripts/clickcheck.sh.
    if let Some(path) = flag(args, "--dump-links") {
        let mut buf = String::new();
        for l in &outcome.links {
            buf.push_str(&format!(
                "{{\"kind\":\"link\",\"href\":{},\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}\n",
                json_str(&l.href),
                l.rect.x,
                l.rect.y,
                l.rect.w,
                l.rect.h
            ));
        }
        for f in &outcome.fields {
            buf.push_str(&format!(
                "{{\"kind\":\"field\",\"field\":\"{:?}\",\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}\n",
                f.kind, f.rect.x, f.rect.y, f.rect.w, f.rect.h
            ));
        }
        if let Err(e) = std::fs::write(&path, buf) {
            eprintln!("could not write {path}: {e}");
            return ExitCode::FAILURE;
        }
    }

    // Output format follows the file extension: .png / .pdf / anything-else=PPM.
    let write_result = match out.rsplit('.').next() {
        Some("png") => cerberus_headless::write_png(&out, &outcome.framebuffer),
        Some("pdf") => cerberus_headless::write_pdf(&out, &outcome.framebuffer),
        _ => cerberus_headless::write_ppm(&out, &outcome.framebuffer),
    };
    if let Err(e) = write_result {
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
    if !outcome.timings.is_empty() {
        println!("--- timings ---");
        for (label, ms) in &outcome.timings {
            println!("  {label:<18} {ms:>8.3} ms");
        }
    }
    if let Some(text) = &outcome.page_text {
        println!("--- page text ---");
        println!("{text}");
    }
    ExitCode::SUCCESS
}

/// `diff` — the parity yardstick (RENDERING_PARITY_PLAN.md, Workstream 0):
/// score how far a Cerberus render is from a headless-Chrome reference so
/// "closer to Chrome" is a number, not a vibe. Reads two PNGs, optionally crops
/// the leading toolbar band off one (Cerberus draws a 36px toolbar Chrome's page
/// screenshot lacks), compares over their overlap, and prints an RMSE score and
/// mismatch fraction. `--fail-over <rmse>` turns it into a regression gate
/// (nonzero exit when the score is worse than the threshold).
///
///   cerberus-app diff --ref chrome.png --cerb cerb.png [--crop-top 36]
///                     [--tolerance 8] [--fail-over 0.15]
fn cmd_diff(args: &[String]) -> ExitCode {
    use cerberus_app::parity;
    let (Some(ref_path), Some(cerb_path)) = (flag(args, "--ref"), flag(args, "--cerb")) else {
        eprintln!("diff: --ref <chrome.png> and --cerb <cerb.png> are required");
        return ExitCode::FAILURE;
    };
    let crop_top: u32 = flag(args, "--crop-top")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let tolerance: u8 = flag(args, "--tolerance")
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let reference = match parity::load_png(&ref_path) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("diff: {e}");
            return ExitCode::FAILURE;
        }
    };
    let cerb = match parity::load_png(&cerb_path) {
        Ok(i) => parity::crop_top(&i, crop_top),
        Err(e) => {
            eprintln!("diff: {e}");
            return ExitCode::FAILURE;
        }
    };
    let report = parity::diff(&reference, &cerb, tolerance);
    println!("parity diff ({ref_path} vs {cerb_path}):");
    println!(
        "  reference       : {}x{}",
        reference.size.w, reference.size.h
    );
    println!(
        "  cerberus        : {}x{}{}",
        cerb.size.w,
        cerb.size.h,
        if crop_top > 0 {
            format!(" (top {crop_top}px cropped)")
        } else {
            String::new()
        }
    );
    println!(
        "  compared region : {}x{} ({} px)",
        report.compared.w, report.compared.h, report.compared_px
    );
    println!(
        "  mismatched      : {} px ({:.2}% over tolerance {tolerance})",
        report.mismatched_px,
        report.mismatch_fraction * 100.0
    );
    println!("  RMSE (0=match)  : {:.5}", report.rmse);
    if report.is_perfect(&reference, &cerb) {
        println!("  verdict         : PERFECT (byte-identical, same size)");
    }
    if let Some(threshold) = flag(args, "--fail-over").and_then(|s| s.parse::<f64>().ok()) {
        if report.rmse > threshold {
            eprintln!(
                "diff FAIL: RMSE {:.5} > threshold {threshold:.5}",
                report.rmse
            );
            return ExitCode::FAILURE;
        }
        println!(
            "diff PASS: RMSE {:.5} <= threshold {threshold:.5}",
            report.rmse
        );
    }
    ExitCode::SUCCESS
}

/// Headless interactive probe: drive the real worker loop against a live URL and
/// print the settled result. Unlike `render` (one-shot, synchronous), this runs
/// the async pipeline — external `<script src>` (e.g. a bot-challenge sensor)
/// fetch + execute, XHR/fetch, cookie capture, and any script-driven reload — so
/// it reflects what the interactive browser actually does. `--url` is required;
/// `--proxy`/`--system-roots`/`--data-dir` and `--timeout <secs>` are optional.
fn cmd_probe(args: &[String]) -> ExitCode {
    let Some(url) = flag(args, "--url") else {
        eprintln!("probe: --url <url> is required");
        return ExitCode::FAILURE;
    };
    let opts = cerberus_app::AppOptions {
        system_roots: has_flag(args, "--system-roots"),
        data_dir: flag(args, "--data-dir").map(std::path::PathBuf::from),
        proxy: flag(args, "--proxy"),
    };
    let timeout_s: u64 = flag(args, "--timeout")
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let mut app = cerberus_app::BrowserApp::with_config(opts);
    app.open(&url);
    // Busy-poll the worker results until the page settles, then a short quiet
    // period (async JS may schedule a reload just after the response commits).
    let start = std::time::Instant::now();
    let mut quiet = 0u32;
    while start.elapsed() < std::time::Duration::from_secs(timeout_s) {
        app.drive();
        if app.is_settled() {
            quiet += 1;
            if quiet >= 40 {
                break; // ~2s of no new network work after settling
            }
        } else {
            quiet = 0;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let text = app.page_text();
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    println!("probe {url}");
    println!("  http status : {}", app.status());
    println!("  settled     : {}", app.is_settled());
    println!("  text ({} chars):", one_line.chars().count());
    println!("{}", one_line.chars().take(3000).collect::<String>());
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

/// The M9 benchmark suite: per-stage medians over the embedded fixture.
/// `--assert-total-ms <N>` turns it into a (generous) CI regression gate.
fn cmd_bench(args: &[String]) -> ExitCode {
    let iters: usize = flag(args, "--iters")
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let stages = cerberus_app::bench_pipeline(iters);
    println!("pipeline benchmark (medians over {iters} iterations):");
    let mut total = 0.0;
    for stage in &stages {
        println!("  {:<18} {:>8.2} ms", stage.name, stage.median_ms);
        total += stage.median_ms;
    }
    println!("  {:<18} {total:>8.2} ms", "total");
    if let Some(budget) = flag(args, "--assert-total-ms").and_then(|s| s.parse::<f64>().ok()) {
        if total > budget {
            eprintln!("bench FAIL: total {total:.2} ms > budget {budget:.2} ms");
            return ExitCode::FAILURE;
        }
        println!("bench PASS: total {total:.2} ms <= budget {budget:.2} ms");
    }
    ExitCode::SUCCESS
}

/// `mirror-bench` — the large-N mirror-group gate (E3/ADR-0026): build a group of
/// N sealed instances, sweep focus across all of them (cold then warm), and
/// assert resident memory after releasing dormant snapshots stays within budget.
fn cmd_mirror_bench(args: &[String]) -> ExitCode {
    let n: usize = flag(args, "--instances")
        .and_then(|s| s.parse().ok())
        .unwrap_or(256);
    let budget_mb: f64 = flag(args, "--budget-mb")
        .and_then(|s| s.parse().ok())
        .unwrap_or(64.0);

    let bench = match cerberus_app::mirror_bench(n) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("mirror-bench failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("mirror-bench ({} instances):", bench.instances);
    println!(
        "  cold focus sweep : {:>8.2} ms ({:.3} ms/instance)",
        bench.cold_sweep_ms,
        bench.cold_sweep_ms / n.max(1) as f64
    );
    println!(
        "  warm focus sweep : {:>8.2} ms (re-focus reuses converged snapshots)",
        bench.warm_sweep_ms
    );
    match bench.peak_rss_kb {
        Some(kb) => {
            let mb = kb as f64 / 1024.0;
            println!("  resident (after release_dormant): {mb:.1} MB");
            if mb > budget_mb {
                eprintln!(
                    "mirror-bench FAIL: resident {mb:.1} MB > budget {budget_mb:.1} MB (N={n})"
                );
                return ExitCode::FAILURE;
            }
            println!("mirror-bench PASS: resident {mb:.1} MB <= budget {budget_mb:.1} MB (N={n})");
        }
        None => {
            println!("  resident memory unavailable on this platform; skipping budget check");
        }
    }
    ExitCode::SUCCESS
}

/// `cookies` — inspect and retune the per-cookie disposition policy of a
/// persistent profile, headlessly.
fn cmd_cookies(args: &[String]) -> ExitCode {
    let Some(dir) = flag(args, "--data-dir") else {
        eprintln!("cookies: --data-dir <DIR> is required");
        return ExitCode::FAILURE;
    };
    let site = flag(args, "--site");
    let set = flag(args, "--set");
    match cerberus_app::cookie_admin(&dir, site.as_deref(), set.as_deref()) {
        Ok(lines) => {
            if lines.is_empty() {
                println!("(no cookies)");
            } else {
                for line in lines {
                    println!("{line}");
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("cookies: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `identities` — list, add (`--add <label>`), or remove (`--remove <index>`)
/// a persistent profile's sealed identities, and set each one's own egress
/// proxy (`--set-proxy <idx>=<host:port>` / `--clear-proxy <idx>`). A profile
/// holds arbitrary N; the mirror driver (`run --mirror`) drives every one of
/// them, each through its own proxy.
fn cmd_identities(args: &[String]) -> ExitCode {
    let Some(dir) = flag(args, "--data-dir") else {
        eprintln!("identities: --data-dir <DIR> is required");
        return ExitCode::FAILURE;
    };
    let add = flag(args, "--add");
    let remove = flag(args, "--remove").and_then(|s| s.parse::<usize>().ok());
    let set_proxy = flag(args, "--set-proxy");
    let clear_proxy = flag(args, "--clear-proxy").and_then(|s| s.parse::<usize>().ok());
    match cerberus_app::identities_admin_full(
        &dir,
        add.as_deref(),
        remove,
        set_proxy.as_deref(),
        clear_proxy,
    ) {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("identities: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Resolve a `--delimiter` value to a single char. Accepts a literal char or a
/// name (`tab`/`colon`/`comma`/`semicolon`/`pipe`/`space`), since a tab can't be
/// typed as an argument. The quote char is reserved for field quoting.
fn parse_delimiter(s: &str) -> Result<char, String> {
    let d = match s {
        "tab" => '\t',
        "colon" => ':',
        "comma" => ',',
        "semicolon" => ';',
        "pipe" => '|',
        "space" => ' ',
        _ => {
            let mut it = s.chars();
            match (it.next(), it.next()) {
                (Some(c), None) => c,
                _ => return Err(format!("--delimiter must be one char or a name, got {s:?}")),
            }
        }
    };
    if d == '"' {
        return Err("--delimiter cannot be the quote character".into());
    }
    Ok(d)
}

/// `profile` — manage identities' autofill profiles (login/address/card), sealed
/// in the encrypted vault. Modes:
///   --template <FILE|->        write a no-frills CSV template (no vault needed)
///   --export   <FILE>          export every identity's profile to CSV
///   --import   <FILE>          import profiles from CSV (creates missing ids)
///   --set "key=value;…"        update one identity's profile (--identity N)
///   (default)                  show one identity's profile (--identity N)
/// `--delimiter <CHAR|name>` selects the CSV delimiter (default `:`; import
/// auto-detects). The vault passphrase comes from `CERBERUS_VAULT_PASS` (never an
/// argument, to keep it out of shell history) for every mode except --template.
fn cmd_profile(args: &[String]) -> ExitCode {
    let delim = match flag(args, "--delimiter").as_deref() {
        Some(s) => match parse_delimiter(s) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("profile: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => ':',
    };

    // --template needs no profile dir or vault: it is pure text.
    if let Some(file) = flag(args, "--template") {
        let text = cerberus_app::profile_csv_template(delim);
        let res = if file == "-" {
            use std::io::Write;
            std::io::stdout()
                .write_all(text.as_bytes())
                .map_err(|e| e.to_string())
        } else {
            std::fs::write(&file, &text).map_err(|e| e.to_string())
        };
        return match res {
            Ok(()) => {
                if file != "-" {
                    println!("profile: wrote CSV template to {file}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("profile: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let Some(dir) = flag(args, "--data-dir") else {
        eprintln!("profile: --data-dir <DIR> is required");
        return ExitCode::FAILURE;
    };
    let Ok(mut passphrase) = std::env::var("CERBERUS_VAULT_PASS") else {
        eprintln!("profile: set CERBERUS_VAULT_PASS to the vault passphrase");
        return ExitCode::FAILURE;
    };

    // `passphrase` is a plain String for as short a time as possible: it is
    // wrapped in the zeroizing `Secret` immediately inside each call below, and
    // this original copy is wiped (not just dropped) before `cmd_profile`
    // returns on every path (issue #30).
    let code = if let Some(file) = flag(args, "--export") {
        match cerberus_app::profile_export(&dir, &file, &passphrase, delim) {
            Ok(n) => {
                println!("profile: exported {n} identities to {file}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("profile: {e}");
                ExitCode::FAILURE
            }
        }
    } else if let Some(file) = flag(args, "--import") {
        match cerberus_app::profile_import(&dir, &file, &passphrase) {
            Ok(lines) => {
                for line in lines {
                    println!("{line}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("profile: {e}");
                ExitCode::FAILURE
            }
        }
    } else {
        let identity = flag(args, "--identity")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let set = flag(args, "--set");
        match cerberus_app::profile_admin(&dir, identity, set.as_deref(), &passphrase) {
            Ok(lines) => {
                for line in lines {
                    println!("{line}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("profile: {e}");
                ExitCode::FAILURE
            }
        }
    };
    passphrase.zeroize();
    code
}

fn print_usage() {
    println!(
        "cerberus — a privacy-first, memory-lean browser\n\n\
         USAGE:\n\
         \x20 cerberus <command> [options]\n\
         \x20 cerberus                  (no command: opens the browser on a desktop build)\n\n\
         COMMANDS:\n\
         \x20 run        Open the browser in a window (needs a display)\n\
         \x20 render     Render a page to PPM and print a summary (headless)\n\
         \x20 diff       Score a render vs a Chrome reference PNG (parity yardstick)\n\
         \x20 mem-gate   Render and assert resident memory within budget\n\
         \x20 bench      Time the render pipeline stages (see --assert-total-ms)\n\
         \x20 mirror-bench  Large-N mirror gate: focus-sweep N instances, assert RSS\n\
         \x20 cookies    Inspect/retune a profile's cookie dispositions (--data-dir)\n\
         \x20 identities Manage a profile's identities (--data-dir; --add/--remove;\n\
         \x20            --set-proxy <idx>=<host:port> / --clear-proxy <idx>)\n\
         \x20 profile    Show/set an identity's autofill profile, or bulk\n\
         \x20            import/export via CSV (--data-dir; --identity N;\n\
         \x20            --set \"key=value;...\"; --template/--export/--import\n\
         \x20            <FILE>; --delimiter <CHAR|name>; CERBERUS_VAULT_PASS)\n\
         \x20 version    Print the version\n\
         \x20 help       Print this help\n\n\
         RUN OPTIONS:\n\
         \x20 --fullscreen        start borderless-fullscreen (F11 toggles)\n\
         \x20 --mirror            drive every profile identity in its own window\n\
         \x20 --system-roots      trust the OS cert store (TLS-inspecting proxies)\n\
         \x20 --data-dir <DIR>    persistent profile (cookies, vault, heads);\n\
         \x20                     omit for fully-ephemeral (default)\n\
         \x20 --proxy <HOST:PORT> single egress proxy (CONNECT tunnel, no DNS leak)\n\n\
         RENDER OPTIONS:\n\
         \x20 --url <URL>          default: cerberus:home\n\
         \x20 --out <FILE>         default: cerberus-home.ppm\n\
         \x20 --width <PX>         viewport width\n\
         \x20 --height <PX>        viewport height\n\
         \x20 --headed            enable consent prompts\n\
         \x20 --system-roots      trust the OS cert store (TLS-inspecting proxies)\n\
         \x20 --data-dir <DIR>    persistent profile (cookies survive runs)\n\
         \x20 --dump-text         print the page's text content (automation)\n\
         \x20 --dump-links FILE   write link/control hit-boxes as JSON lines (audit)\n\
         \x20 --timers            collect + print per-stage performance timings\n\
         \x20 --proxy <HOST:PORT> single egress proxy (CONNECT tunnel, no DNS leak)\n\
         \x20 --engine <ENGINE>   layout engine: block (default) | taffy\n\
         \x20 --images <MODE>     image display: graphical (default) | text-only\n\
         \x20                     (text-only renders alt/caption, skips the fetch)\n\
         \x20 --text-only-image <SUBSTR>  force just images whose URL contains\n\
         \x20                     SUBSTR to text-only (repeatable; per-image option)\n\
         \x20 (--out extension selects the format: .ppm, .png, or .pdf)\n\n\
         MEM-GATE OPTIONS:\n\
         \x20 --budget-mb <MB>     default: 64\n\
         \x20 --switches <N>       also assert RSS within +10% after N head switches\n\n\
         MIRROR-BENCH OPTIONS:\n\
         \x20 --instances <N>      number of sealed instances to drive (default: 256)\n\
         \x20 --budget-mb <MB>     resident budget after releasing dormant (default: 64)"
    );
}

/// The command used when the binary is launched with no arguments. A desktop
/// (windowing) build opens the browser — what double-clicking the .exe is meant
/// to do — while a headless build (`--no-default-features`) renders the default
/// page. Previously this was always `render`, so double-clicking the desktop
/// binary only flashed a console and wrote a `.ppm`.
const fn default_command() -> &'static str {
    if cfg!(feature = "windowing") {
        "run"
    } else {
        "render"
    }
}

/// Read `--key value` from args.
/// Minimal JSON string escaping for hrefs (quotes, backslashes, control chars).
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

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

/// Read every `--key value` occurrence (a repeatable flag).
fn flags(args: &[String], key: &str) -> Vec<String> {
    args.iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == key)
        .filter_map(|(i, _)| args.get(i + 1).cloned())
        .collect()
}
