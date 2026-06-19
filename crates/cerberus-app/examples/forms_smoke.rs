//! End-to-end smoke test: drive a *real* form POST over the network and show the
//! server echoing back exactly what the browser submitted. Proves the form-
//! submission path (build body → POST via the net layer → render the response).
//!
//! Run: `cargo run -p cerberus-app --example forms_smoke`
//! (Uses the OS root store for the sandbox's TLS-inspecting proxy.)

use std::thread::sleep;
use std::time::Duration;

use cerberus_app::BrowserApp;
use cerberus_headless::write_png;
use cerberus_shell::FrameApp;
use cerberus_types::Size;

fn main() {
    let size = Size::new(1000, 760);
    let mut app = BrowserApp::with_options(true); // trust OS roots (proxy)
    app.set_scale_factor(1.0);
    app.render_frame(size); // establish toolbar layout for hit-testing

    // 1) Navigate to httpbin's POST form by typing into the address bar.
    navigate(&mut app, "https://httpbin.org/forms/post");
    settle(&mut app, 10);
    println!(
        "form page  : HTTP {} ({} chars)",
        app.status(),
        app.page_text().len()
    );
    write_png("forms-1-form.png", &app.render_frame(size)).ok();

    // 2) Fill the first text field (httpbin's "custname") with a marker value.
    let marker = "Cerberus-POST-OK";
    app.render_frame(size);
    let Some(field) = first_text_field(&mut app, size) else {
        eprintln!("no text field found on the form page");
        return;
    };
    app.pointer_down(field.0, field.1);
    for c in marker.chars() {
        app.text_input(c);
    }

    // 3) Submit the enclosing form (Enter) — a POST to /post.
    app.submit();
    settle(&mut app, 10);

    // 4) httpbin echoes the submitted form as JSON; the marker proves our POST
    //    body arrived as form data (not a query, not dropped).
    let text = app.page_text();
    let echoed = text.contains(marker);
    println!("after POST : HTTP {}", app.status());
    println!("echoed back: {echoed}  (looked for {marker:?})");
    if let Some(i) = text.find("\"form\"") {
        let end = (i + 160).min(text.len());
        println!("response   : …{}…", &text[i..end]);
    }
    write_png("forms-2-response.png", &app.render_frame(size)).ok();
    println!("wrote forms-1-form.png and forms-2-response.png");
    println!(
        "RESULT: {}",
        if echoed {
            "PASS — the server received the POSTed form field"
        } else {
            "FAIL — marker not echoed"
        }
    );
}

/// Type `url` into the address bar and submit it.
fn navigate(app: &mut BrowserApp, url: &str) {
    app.pointer_down(420, 18); // focus the URL box (in the toolbar)
    for c in url.chars() {
        app.text_input(c);
    }
    app.submit();
}

/// Pump the network worker for up to `secs` seconds (the example has no event
/// loop, so we poll + sleep until the in-flight load commits and settles).
fn settle(app: &mut BrowserApp, secs: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    let mut idle = 0;
    while std::time::Instant::now() < deadline {
        let worked = app.poll();
        if !app.is_loading() && !worked {
            idle += 1;
            if idle > 5 {
                break; // committed and quiet
            }
        } else {
            idle = 0;
        }
        sleep(Duration::from_millis(100));
    }
}

/// The click point (center) of the first text field on the page, if any.
fn first_text_field(app: &mut BrowserApp, size: Size) -> Option<(i32, i32)> {
    // render_frame populates the form-field hit boxes; expose them via a render.
    let _ = app.render_frame(size);
    app.text_field_centers().into_iter().next()
}
