#!/usr/bin/env python3
"""Full-fidelity offline mirror: download the HTML plus ALL stylesheets
(same- AND cross-origin), their @imports and url() assets (fonts/images), and
<img> sources, rewriting every reference to a local file. Only <script> is
dropped (we can't run page JS, and both engines are compared on the same static
DOM). curl reaches the network through the agent proxy; Chrome cannot, so this
lets Chrome render the REAL styled page as the parity reference.

Usage: mirror.py <name> <url>
Writes <name>/index.html and <name>/assets/*.
"""
import hashlib
import os
import re
import subprocess
import sys
from urllib.parse import urljoin, urlsplit

NAME, URL = sys.argv[1], sys.argv[2]
ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), NAME)
ASSETS = os.path.join(ROOT, "assets")
os.makedirs(ASSETS, exist_ok=True)

UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0 Safari/537.36"
_cache = {}


def fetch(url):
    """Fetch url bytes through curl (proxy-aware), cached. None on failure."""
    if url in _cache:
        return _cache[url]
    try:
        out = subprocess.run(
            ["curl", "-fsSL", "--max-time", "25", "-A", UA, url],
            capture_output=True, timeout=30,
        )
        data = out.stdout if out.returncode == 0 else None
    except Exception:
        data = None
    _cache[url] = data
    return data


def local_name(url, ext):
    h = hashlib.sha1(url.encode()).hexdigest()[:16]
    return f"{h}{ext}"


def guess_ext(url, default):
    path = urlsplit(url).path
    m = re.search(r"\.(css|js|png|jpe?g|gif|svg|webp|woff2?|ttf|eot|otf|ico)(?:$|[?#])", path, re.I)
    return "." + m.group(1).lower() if m else default


def save_asset(url, data, ext):
    fn = local_name(url, ext)
    with open(os.path.join(ASSETS, fn), "wb") as f:
        f.write(data)
    return "assets/" + fn


def rewrite_css(css_text, base_url, depth=0):
    """Download @import and url() targets in a stylesheet, rewrite to local
    (relative to the css file, which lives in assets/, so peers are bare names)."""
    if depth > 3:
        return css_text

    def imp(m):
        raw = m.group(1).strip().strip("'\"")
        if raw.startswith("data:"):
            return m.group(0)
        abs_url = urljoin(base_url, raw)
        data = fetch(abs_url)
        if not data:
            return ""  # drop unreachable import
        sub = rewrite_css(data.decode("utf-8", "replace"), abs_url, depth + 1)
        fn = local_name(abs_url, ".css")
        with open(os.path.join(ASSETS, fn), "w", encoding="utf-8") as f:
            f.write(sub)
        return f'@import "{fn}";'

    css_text = re.sub(r'@import\s+(?:url\()?\s*([^;]+?)\s*\)?\s*;', imp, css_text, flags=re.I)

    def url(m):
        raw = m.group(1).strip().strip("'\"")
        if raw.startswith("data:") or not raw:
            return m.group(0)
        abs_url = urljoin(base_url, raw)
        data = fetch(abs_url)
        if not data:
            return m.group(0)
        fn = save_asset(abs_url, data, guess_ext(abs_url, ".bin"))
        return f'url({os.path.basename(fn)})'

    css_text = re.sub(r'url\(\s*([^)]+?)\s*\)', url, css_text, flags=re.I)
    return css_text


def main():
    html_bytes = fetch(URL)
    if not html_bytes:
        print(f"{NAME}: fetch failed")
        return
    html = html_bytes.decode("utf-8", "replace")

    # Drop <script> (can't run JS) but keep everything else.
    html = re.sub(r'<script\b[^>]*>.*?</script>', '', html, flags=re.I | re.S)
    html = re.sub(r'<script\b[^>]*/?>', '', html, flags=re.I)

    # <link rel=stylesheet href=...> (same + cross origin) → download + rewrite.
    def link(m):
        tag = m.group(0)
        if not re.search(r'rel\s*=\s*["\']?[^"\'>]*stylesheet', tag, re.I):
            return tag
        hm = re.search(r'href\s*=\s*["\']([^"\']+)["\']', tag, re.I)
        if not hm:
            return tag
        abs_url = urljoin(URL, hm.group(1))
        data = fetch(abs_url)
        if not data:
            return ''  # drop unreachable sheet
        css = rewrite_css(data.decode("utf-8", "replace"), abs_url)
        fn = local_name(abs_url, ".css")
        with open(os.path.join(ASSETS, fn), "w", encoding="utf-8") as f:
            f.write(css)
        return tag[:hm.start(1)] + "assets/" + fn + tag[hm.end(1):]

    html = re.sub(r'<link\b[^>]*>', link, html, flags=re.I)

    # Inline <style> — rewrite its url()/@import too.
    def style(m):
        inner = rewrite_css(m.group(1), URL)
        return m.group(0)[:m.start(1) - m.start(0)] + inner + "</style>"

    html = re.sub(r'<style\b[^>]*>(.*?)</style>', style, html, flags=re.I | re.S)

    # <img src> (same + cross origin) → download + rewrite; strip srcset (keep it simple, src wins).
    def img(m):
        tag = m.group(0)
        sm = re.search(r'\bsrc\s*=\s*["\']([^"\']+)["\']', tag, re.I)
        if not sm:
            return tag
        raw = sm.group(1)
        if raw.startswith("data:"):
            tag2 = tag
        else:
            abs_url = urljoin(URL, raw)
            data = fetch(abs_url)
            if data:
                fn = save_asset(abs_url, data, guess_ext(abs_url, ".png"))
                tag2 = tag[:sm.start(1)] + fn + tag[sm.end(1):]
            else:
                tag2 = tag
        # drop srcset so the browser uses our local src
        tag2 = re.sub(r'\ssrcset\s*=\s*["\'][^"\']*["\']', '', tag2, flags=re.I)
        return tag2

    html = re.sub(r'<img\b[^>]*>', img, html, flags=re.I)

    with open(os.path.join(ROOT, "index.html"), "w", encoding="utf-8") as f:
        f.write(html)
    n_assets = len(os.listdir(ASSETS))
    print(f"{NAME}: mirrored — {len(html)} bytes html, {n_assets} assets")


main()
