// ============================================================================
//  flipkart-price-bot v2.0  —  production-grade price monitor
//
//  ARCHITECTURE:
//    Layer 0 – Config & telemetry bootstrap
//    Layer 1 – Price extraction pipeline (3-tier fallback strategy)
//    Layer 2 – Anti-bot evasion engine
//    Layer 3 – Retry / circuit-breaker
//    Layer 4 – Telegram notifier with rate-limiter
//    Layer 5 – Main orchestration loop with adaptive polling
// ============================================================================

use anyhow::{bail, Context, Result};
use chrono::Local;
use rand::Rng;
use regex::Regex;
use reqwest::{header, Client};
use scraper::{Html, Selector};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::Mutex,
    time::{sleep, Instant},
};
use tracing::{debug, error, info, warn};

// ─────────────────────────────────────────────────────────────────────────────
//  LAYER 0 — CONFIG
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Config {
    product_url: String,
    poll_secs: u64,
    jitter_ms: u64,
    bot_token: String,
    chat_id: String,
    /// Alert when price drops below this (0 = always notify on any change)
    target_price: f64,
}

impl Config {
    fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        let url = std::env::var("FLIPKART_URL").context("FLIPKART_URL not set")?;
        // Validate it's actually a Flipkart URL at startup, not at first poll
        if !url.contains("flipkart.com") {
            bail!("FLIPKART_URL must be a flipkart.com URL, got: {}", url);
        }

        Ok(Self {
            product_url: url,
            poll_secs: env_u64("POLL_INTERVAL_SECS", 10),
            jitter_ms: env_u64("JITTER_MS", 3000),
            bot_token: std::env::var("TELEGRAM_BOT_TOKEN").context("TELEGRAM_BOT_TOKEN not set")?,
            chat_id: std::env::var("TELEGRAM_CHAT_ID").context("TELEGRAM_CHAT_ID not set")?,
            target_price: std::env::var("TARGET_PRICE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0),
        })
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ─────────────────────────────────────────────────────────────────────────────
//  LAYER 1 — PRICE EXTRACTION PIPELINE  (3-tier fallback)
// ─────────────────────────────────────────────────────────────────────────────
//
//  Tier A: Internal JSON-LD / structured data   ← most stable, format = schema.org
//  Tier B: Internal __INITIAL_STATE__ JSON blob ← stable, format = Flipkart's own
//  Tier C: CSS class selectors on rendered HTML ← least stable, changes ~monthly
//
//  We hash the page to detect Flipkart A/B variants and bot-detection pages.

#[derive(Debug, Clone)]
struct PriceResult {
    raw: String,  // "₹12,499"
    numeric: f64, // 12499.0
    tier: u8,     // which extraction tier succeeded
}

fn extract_price(html: &str) -> Option<PriceResult> {
    tier_a_json_ld(html)
        .or_else(|| tier_b_initial_state(html))
        .or_else(|| tier_c_css(html))
}

/// Tier A — JSON-LD schema.org/Product (most stable across site redesigns)
fn tier_a_json_ld(html: &str) -> Option<PriceResult> {
    // Flipkart embeds <script type="application/ld+json"> blocks with offers.price
    let re = Regex::new(
        r#"application/ld\+json[^>]*>\s*\{[^}]*"@type"\s*:\s*"Product".*?"price"\s*:\s*"?(\d[\d,\.]*)"?"#,
    ).ok()?;

    // Use DOTALL-equivalent by replacing newlines for the regex engine
    let flat = html.replace('\n', " ");
    let caps = re.captures(&flat)?;
    let raw_num = caps[1].replace(',', "");
    let numeric = raw_num.parse::<f64>().ok()?;

    Some(PriceResult {
        raw: format!("₹{}", &caps[1]),
        numeric,
        tier: 1,
    })
}

/// Tier B — window.__INITIAL_STATE__ finalPrice key
fn tier_b_initial_state(html: &str) -> Option<PriceResult> {
    // Flipkart's React SSR embeds the full product state as JSON.
    // finalPrice is always an integer (paise or rupees depending on product).
    let patterns = [
        r#""finalPrice"\s*:\s*\{"value"\s*:\s*(\d+)"#, // new format
        r#""finalPrice"\s*:\s*(\d+)"#,                 // old flat format
        r#""sellingPrice"\s*:\s*\{"value"\s*:\s*(\d+)"#, // alternate key
        r#""price"\s*:\s*\{"value"\s*:\s*(\d+)"#,
    ];

    for pat in &patterns {
        if let Ok(re) = Regex::new(pat) {
            if let Some(caps) = re.captures(html) {
                if let Ok(n) = caps[1].parse::<f64>() {
                    return Some(PriceResult {
                        raw: format!("₹{}", format_price(n)),
                        numeric: n,
                        tier: 2,
                    });
                }
            }
        }
    }
    None
}

/// Tier C — CSS class selectors (fragile, last resort)
fn tier_c_css(html: &str) -> Option<PriceResult> {
    // Known Flipkart price class names (update when they rotate the obfuscation)
    let selectors = [
        "._30jeq3._16Jk6d", // primary price + discount badge combo
        "._30jeq3",         // standalone price
        ".Nx9bqj.CxhGGd",   // 2024 product page variant
        ".CxhGGd",          // minimal variant
        "._16Jk6d",         // sometimes used standalone
    ];

    let doc = Html::parse_document(html);
    for sel_str in &selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(elem) = doc.select(&sel).next() {
                let text: String = elem.text().collect::<String>().trim().to_owned();
                if !text.is_empty() && text.contains('₹') {
                    let numeric = parse_rupee(&text)?;
                    return Some(PriceResult {
                        raw: text,
                        numeric,
                        tier: 3,
                    });
                }
            }
        }
    }
    None
}

fn parse_rupee(s: &str) -> Option<f64> {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    cleaned.parse().ok()
}

fn format_price(n: f64) -> String {
    // Indian number formatting: X,XX,XXX
    let s = format!("{:.0}", n);
    let bytes = s.as_bytes();
    let len = bytes.len();
    if len <= 3 {
        return s;
    }
    let mut out = String::new();
    let first = len % 2; // how many digits before first comma in Indian system
    let first = if first == 0 { 2 } else { first };
    out.push_str(&s[..first]);
    let mut i = first;
    while i < len {
        out.push(',');
        out.push_str(&s[i..i + 2.min(len - i)]);
        i += 2;
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
//  LAYER 2 — ANTI-BOT EVASION ENGINE
// ─────────────────────────────────────────────────────────────────────────────

const USER_AGENTS: &[&str] = &[
    // Chrome Windows
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36",
    // Chrome macOS
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36",
    // Firefox Linux
    "Mozilla/5.0 (X11; Linux x86_64; rv:126.0) Gecko/20100101 Firefox/126.0",
    // Edge Windows
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36 Edg/125.0.0.0",
    // Safari macOS
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_4_1) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4.1 Safari/605.1.15",
    // Chrome Android (mobile — sometimes gets a lighter page with less bot scrutiny)
    "Mozilla/5.0 (Linux; Android 13; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.6422.165 Mobile Safari/537.36",
];

fn build_client(ua_index: usize) -> Result<Client> {
    let ua = USER_AGENTS[ua_index % USER_AGENTS.len()];

    let mut headers = header::HeaderMap::new();
    headers.insert(header::USER_AGENT, ua.parse()?);
    headers.insert(
        header::ACCEPT_LANGUAGE,
        "en-IN,en-GB;q=0.9,en;q=0.8".parse()?,
    );
    headers.insert(
        header::ACCEPT,
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8"
            .parse()?,
    );
    headers.insert(header::ACCEPT_ENCODING, "gzip, deflate, br".parse()?);
    headers.insert("DNT", "1".parse()?);
    headers.insert("Sec-Fetch-Dest", "document".parse()?);
    headers.insert("Sec-Fetch-Mode", "navigate".parse()?);
    headers.insert("Sec-Fetch-Site", "none".parse()?);
    headers.insert("Sec-Fetch-User", "?1".parse()?);
    // Upgrade-Insecure-Requests: signals we're a real browser
    headers.insert("Upgrade-Insecure-Requests", "1".parse()?);

    Client::builder()
        .default_headers(headers)
        .cookie_store(true) // session cookies = less bot suspicion
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .timeout(Duration::from_secs(25))
        .tcp_keepalive(Duration::from_secs(30))
        .connection_verbose(false)
        .build()
        .context("Failed to build HTTP client")
}

/// Detects soft-block pages (200 OK but CAPTCHA / empty / anti-bot wall)
fn is_bot_detected(html: &str) -> bool {
    let signals = [
        "Please verify you are human",
        "captcha",
        "cf-browser-verification",
        "Access Denied",
        "unusual traffic",
        "security check",
        "bot detected",
    ];
    let html_lower = html.to_lowercase();
    let too_small = html.len() < 8_000; // real Flipkart pages are >100KB
    too_small || signals.iter().any(|s| html_lower.contains(s))
}

// ─────────────────────────────────────────────────────────────────────────────
//  LAYER 3 — RETRY + CIRCUIT BREAKER
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum FetchOutcome {
    Price(PriceResult),
    BotDetected,
    ParseFailure(String), // page loaded but price not found — selector may need update
    NetworkError(String),
}

struct CircuitBreaker {
    failures: u32,
    open_until: Option<Instant>,
    open_threshold: u32,
    open_duration: Duration,
}

impl CircuitBreaker {
    fn new() -> Self {
        Self {
            failures: 0,
            open_until: None,
            open_threshold: 5,
            open_duration: Duration::from_secs(120), // 2 min cool-off
        }
    }

    fn is_open(&self) -> bool {
        self.open_until.map(|t| Instant::now() < t).unwrap_or(false)
    }

    fn record_success(&mut self) {
        self.failures = 0;
        self.open_until = None;
    }

    fn record_failure(&mut self) {
        self.failures += 1;
        if self.failures >= self.open_threshold {
            self.open_until = Some(Instant::now() + self.open_duration);
            warn!(
                failures = self.failures,
                "Circuit breaker OPEN — pausing fetches for {}s",
                self.open_duration.as_secs()
            );
        }
    }
}

async fn fetch_with_retry(cfg: &Config, ua_idx: &mut usize) -> FetchOutcome {
    const MAX_ATTEMPTS: u32 = 4;
    let mut delay_ms: u64 = 1_500;

    for attempt in 1..=MAX_ATTEMPTS {
        let client = match build_client(*ua_idx) {
            Ok(c) => c,
            Err(e) => return FetchOutcome::NetworkError(e.to_string()),
        };

        match client.get(&cfg.product_url).send().await {
            Err(e) => {
                warn!(attempt, "Network error: {}", e);
                // Rotate UA on network failures too (sometimes IP-level block)
                *ua_idx = (*ua_idx + 1) % USER_AGENTS.len();
                if attempt < MAX_ATTEMPTS {
                    sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms = (delay_ms * 2).min(30_000); // exponential, cap 30s
                }
            }
            Ok(resp) => {
                let status = resp.status();
                let html = match resp.text().await {
                    Ok(h) => h,
                    Err(e) => return FetchOutcome::NetworkError(e.to_string()),
                };

                if status == 429 || status == 403 {
                    warn!(attempt, %status, "Rate-limited / forbidden — rotating UA");
                    *ua_idx = (*ua_idx + 1) % USER_AGENTS.len();
                    if attempt < MAX_ATTEMPTS {
                        // Longer back-off when explicitly rate-limited
                        sleep(Duration::from_millis(delay_ms * 3)).await;
                        delay_ms = (delay_ms * 2).min(60_000);
                    }
                    continue;
                }

                if is_bot_detected(&html) {
                    warn!(attempt, "Bot-detection page received");
                    *ua_idx = (*ua_idx + 1) % USER_AGENTS.len();
                    if attempt < MAX_ATTEMPTS {
                        sleep(Duration::from_millis(delay_ms * 2)).await;
                        delay_ms = (delay_ms * 2).min(30_000);
                    }
                    continue;
                }

                // Page loaded cleanly — try extraction
                return match extract_price(&html) {
                    Some(p) => {
                        debug!(tier = p.tier, price = %p.raw, "Price extracted");
                        FetchOutcome::Price(p)
                    }
                    None => {
                        // Log a hash of the page so you can compare across runs
                        let hash = hex::encode(&Sha256::digest(html.as_bytes())[..6]);
                        warn!(page_hash = %hash, len = html.len(),
                              "Price not found — possible selector drift");
                        FetchOutcome::ParseFailure(hash)
                    }
                };
            }
        }
    }

    FetchOutcome::NetworkError(format!("All {} attempts failed", MAX_ATTEMPTS))
}

// ─────────────────────────────────────────────────────────────────────────────
//  LAYER 4 — TELEGRAM NOTIFIER  (with simple rate-limit guard)
// ─────────────────────────────────────────────────────────────────────────────

struct Telegram {
    client: Client,
    token: String,
    chat_id: String,
    last_sent: Option<Instant>,
}

impl Telegram {
    fn new(token: String, chat_id: String) -> Result<Self> {
        Ok(Self {
            client: Client::builder().timeout(Duration::from_secs(15)).build()?,
            token,
            chat_id,
            last_sent: None,
        })
    }

    async fn send(&mut self, msg: &str) -> Result<()> {
        // Debounce: never send more than 1 msg per 3 seconds (Telegram limit = 30/min)
        if let Some(t) = self.last_sent {
            let elapsed = t.elapsed();
            if elapsed < Duration::from_secs(3) {
                sleep(Duration::from_secs(3) - elapsed).await;
            }
        }

        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let payload = serde_json::json!({
            "chat_id":                  self.chat_id,
            "text":                     msg,
            "parse_mode":               "HTML",
            "disable_web_page_preview": false,
        });

        let resp = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .context("Telegram HTTP request failed")?;

        self.last_sent = Some(Instant::now());

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("Telegram API rejected message: {}", body);
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  LAYER 5 — ORCHESTRATION LOOP
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("flipkart_price_bot=debug".parse()?),
        )
        .with_target(false)
        .with_thread_ids(false)
        .compact()
        .init();

    let cfg = Arc::new(Config::from_env()?);

    info!("═══════════════════════════════════════════");
    info!("  Flipkart Price Bot v2.0 — Starting up");
    info!("═══════════════════════════════════════════");
    info!(url = %cfg.product_url);
    info!(poll_secs = cfg.poll_secs, jitter_ms = cfg.jitter_ms);
    if cfg.target_price > 0.0 {
        info!(
            target = cfg.target_price,
            "Alert only when price drops below target"
        );
    }

    let tg = Arc::new(Mutex::new(Telegram::new(
        cfg.bot_token.clone(),
        cfg.chat_id.clone(),
    )?));

    let mut cb = CircuitBreaker::new();
    let mut last_price: Option<PriceResult> = None;
    let mut ua_idx: usize = 0;
    let mut cycle: u64 = 0;
    // Track consecutive parse failures separately (selector drift signal)
    let mut parse_fail_streak: u32 = 0;

    // Startup notification
    {
        let mut tg = tg.lock().await;
        let _ = tg
            .send(&format!(
                "🟢 <b>Flipkart Price Bot started</b>\n\
             🕐 Polling every {}s\n\
             🛒 <a href=\"{}\">Product link</a>",
                cfg.poll_secs, cfg.product_url
            ))
            .await;
    }

    loop {
        cycle += 1;
        debug!(cycle, "── Tick ──");

        // ── Circuit breaker check ────────────────────────────────────────────
        if cb.is_open() {
            debug!("Circuit open — skipping fetch");
            sleep(Duration::from_secs(10)).await;
            continue;
        }

        // ── Jitter: randomize delay to avoid clockwork request patterns ──────
        let jitter = rand::thread_rng().gen_range(0..=cfg.jitter_ms);
        let poll_duration = Duration::from_secs(cfg.poll_secs) + Duration::from_millis(jitter);

        // ── Fetch ────────────────────────────────────────────────────────────
        match fetch_with_retry(&cfg, &mut ua_idx).await {
            FetchOutcome::Price(price) => {
                cb.record_success();
                parse_fail_streak = 0;
                info!(
                    price = %price.raw,
                    numeric = price.numeric,
                    tier = price.tier,
                    cycle,
                    "✓ Price fetched"
                );

                // ── Determine if we should alert ─────────────────────────────
                let should_alert = match &last_price {
                    None => {
                        // First successful fetch — always announce
                        true
                    }
                    Some(prev) => {
                        let changed = (prev.numeric - price.numeric).abs() > 0.01;
                        if !changed {
                            false
                        } else if cfg.target_price > 0.0 {
                            // Only alert if new price is at or below target
                            price.numeric <= cfg.target_price
                        } else {
                            true // alert on any change
                        }
                    }
                };

                if should_alert {
                    let msg = build_alert_message(&cfg, &last_price, &price);
                    info!("Sending Telegram alert: {}", price.raw);
                    let mut tg = tg.lock().await;
                    match tg.send(&msg).await {
                        Ok(_) => info!("✓ Telegram alert sent"),
                        Err(e) => error!("✗ Telegram send failed: {}", e),
                    }
                }

                last_price = Some(price);
            }

            FetchOutcome::BotDetected => {
                cb.record_failure();
                warn!(cycle, "Bot-detection page — circuit failure recorded");
                // Extra back-off beyond the poll interval
                sleep(Duration::from_secs(30)).await;
            }

            FetchOutcome::ParseFailure(hash) => {
                parse_fail_streak += 1;
                warn!(
                    cycle,
                    streak = parse_fail_streak,
                    page_hash = %hash,
                    "Price extraction failed"
                );

                // After 3 consecutive parse failures, alert for manual intervention
                if parse_fail_streak == 3 {
                    let msg = format!(
                        "⚠️ <b>Selector drift detected</b>\n\
                         Price extraction has failed {} consecutive times.\n\
                         Page hash: <code>{}</code>\n\
                         Action needed: update CSS selectors in source.\n\
                         🛒 <a href=\"{}\">Inspect product page</a>",
                        parse_fail_streak, hash, cfg.product_url
                    );
                    let mut tg = tg.lock().await;
                    let _ = tg.send(&msg).await;
                }
            }

            FetchOutcome::NetworkError(e) => {
                cb.record_failure();
                error!(cycle, error = %e, "Network failure");
            }
        }

        sleep(poll_duration).await;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  HELPERS
// ─────────────────────────────────────────────────────────────────────────────

fn build_alert_message(cfg: &Config, prev: &Option<PriceResult>, current: &PriceResult) -> String {
    let timestamp = Local::now().format("%d %b %Y, %I:%M %p IST");

    match prev {
        None => format!(
            "🟢 <b>Price Monitor Active</b>\n\
             💰 Current price: <b>{}</b>\n\
             🕐 {}\n\
             🛒 <a href=\"{}\">View on Flipkart</a>",
            current.raw, timestamp, cfg.product_url
        ),
        Some(p) => {
            let diff = p.numeric - current.numeric;
            let pct = (diff / p.numeric * 100.0).abs();
            let dir = if diff > 0.0 {
                "📉 Price dropped"
            } else {
                "📈 Price rose"
            };
            let diff_str = format!("₹{:.0}", diff.abs());

            let target_line = if cfg.target_price > 0.0 && current.numeric <= cfg.target_price {
                format!(
                    "\n🎯 <b>TARGET PRICE REACHED!</b> (target: ₹{:.0})",
                    cfg.target_price
                )
            } else {
                String::new()
            };

            format!(
                "{} <b>by {} ({:.1}%)</b>{}\n\
                 ├ Was: <s>{}</s>\n\
                 └ Now: <b>{}</b>\n\
                 🕐 {}\n\
                 🛒 <a href=\"{}\">Buy now</a>",
                dir, diff_str, pct, target_line, p.raw, current.raw, timestamp, cfg.product_url
            )
        }
    }
}
