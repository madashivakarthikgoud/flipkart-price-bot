# Flipkart Price Bot v2.0

Production-grade Flipkart price monitor in Rust. Sends a Telegram notification
the moment a price changes (or drops below your target).

## Architecture: 3-Tier Price Extraction Pipeline

```
Tier A  JSON-LD schema.org/Product  ←  most stable
  ↓ (fallback)
Tier B  window.__INITIAL_STATE__    ←  stable
  ↓ (fallback)
Tier C  CSS class selectors         ←  least stable (update when Flipkart rotates classes)
```

The bot tells you which tier extracted the price in logs (`tier=1/2/3`).
If you see tier 3 consistently, it means Tier A/B broke and you should investigate.

## Quick Start

```bash
git clone https://github.com/yourname/flipkart-price-bot
cd flipkart-price-bot
cp .env.example .env   # fill in values
cargo run --release
```

## Environment Variables

| Variable              | Required | Default | Description                                      |
|-----------------------|----------|---------|--------------------------------------------------|
| `FLIPKART_URL`        | ✅        | —       | Full product URL from Flipkart                   |
| `TELEGRAM_BOT_TOKEN`  | ✅        | —       | Token from @BotFather                            |
| `TELEGRAM_CHAT_ID`    | ✅        | —       | Your personal or group chat ID                   |
| `TARGET_PRICE`        | ❌        | 0       | Alert only when price ≤ this. 0 = any change     |
| `POLL_INTERVAL_SECS`  | ❌        | 10      | Seconds between polls                            |
| `JITTER_MS`           | ❌        | 3000    | Max random jitter per poll (prevents patterns)   |
| `RUST_LOG`            | ❌        | info    | Log level: debug / info / warn / error           |

## Setup: Telegram

### Step 1 — Create bot

1. Open Telegram, search `@BotFather`
2. Send `/newbot`
3. Enter a name: `My Flipkart Monitor`
4. Enter a username (must end in `bot`): `my_flipkart_bot`
5. BotFather gives you a token: `1234567890:ABCDef...` → this is `TELEGRAM_BOT_TOKEN`

### Step 2 — Get your Chat ID

**Personal chat (simplest):**
1. Send any message to your new bot in Telegram
2. Open in browser: `https://api.telegram.org/bot<YOUR_TOKEN>/getUpdates`
3. Find `"chat":{"id":XXXXXXX}` → that number is your `TELEGRAM_CHAT_ID`

**Group chat (to share alerts with a group):**
1. Add your bot to the group
2. Send a message in the group
3. Same URL above → the chat id will be a negative number like `-1001234567890`

## Deploy: Render.com (Free, 24/7)

> ⚠️ Use **Worker** type, not Web Service. Web services spin down; workers don't.

### Option A — Blueprint (one click)

1. Push this repo to GitHub
2. Go to [render.com](https://render.com) → New → Blueprint
3. Connect your GitHub repo — Render reads `render.yaml` automatically
4. In the Environment tab, add the 3 secret env vars (URL, token, chat ID)
5. Deploy

### Option B — Manual

1. render.com → New → Background Worker
2. Connect repo, runtime = Docker
3. Region: Singapore (closest to Flipkart's servers)
4. Plan: Free
5. Add env vars in Environment tab
6. Deploy

### Alternative: Koyeb (also free, also 24/7)

1. [koyeb.com](https://koyeb.com) → Create App → Docker
2. Connect GitHub repo
3. Set env vars in the Environment section
4. Instance: Free nano
5. Deploy

Both platforms give you persistent logs, auto-redeploy on git push, and zero cost.

## Docker (local)

```bash
# Build
docker build -t flipkart-bot .

# Run
docker run --env-file .env flipkart-bot

# Check logs
docker logs -f <container_id>
```

## What the alerts look like

**On first start:**
```
🟢 Price Monitor Active
💰 Current price: ₹12,499
🕐 15 Jun 2025, 10:32 AM IST
🛒 View on Flipkart
```

**On price drop:**
```
📉 Price dropped by ₹500 (3.8%)
├ Was: ₹13,000
└ Now: ₹12,500
🕐 16 Jun 2025, 02:15 PM IST
🛒 Buy now
```

**On selector drift (action needed):**
```
⚠️ Selector drift detected
Price extraction has failed 3 consecutive times.
Page hash: a3f92b
Action needed: update CSS selectors in source.
🛒 Inspect product page
```

## Updating CSS Selectors (when Flipkart rotates class names)

1. Open the product page in Chrome
2. Right-click the price → Inspect
3. Note the class names on the price element (e.g. `._30jeq3`)
4. Update the `selectors` array in `tier_c_css()` in `src/main.rs`
5. Commit and push — Render auto-redeploys

The page hash in the drift alert helps you confirm if the page structure
genuinely changed vs. a transient network issue.

## Resilience Features

| Feature                  | Behaviour                                                  |
|--------------------------|------------------------------------------------------------|
| 3-tier extraction        | Falls back across JSON-LD → JSON state → CSS               |
| UA rotation              | Cycles through 6 browser fingerprints on failure           |
| Exponential backoff      | 1.5s → 3s → 6s → 12s between retry attempts               |
| Circuit breaker          | Opens after 5 failures, rests 2 min, then retries          |
| Jitter                   | ±3s random per poll to avoid clockwork patterns            |
| Bot detection            | Detects CAPTCHA/soft-block pages before they count as hits |
| Parse failure alerts     | Telegram alert after 3 consecutive extraction failures     |
| Telegram rate limiter    | Debounces sends to stay under API's 30 msg/min limit       |
