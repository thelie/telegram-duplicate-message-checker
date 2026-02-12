# Telegram Duplicate Message Checker

A daemon that monitors your Telegram account in real-time and automatically marks duplicate forwarded messages as read.

## Problem

Telegram doesn't sync read state across duplicate/forwarded messages. When the same post is forwarded to multiple channels you follow, reading it in one channel leaves the copies unread in the others. This clutters your unread count.

## How it works

1. Connects to Telegram as a user client (not a bot) via MTProto
2. Monitors all incoming messages for forward metadata (`fwd_from.from_id` + `channel_post`)
3. Tracks which messages are copies of the same original — new forwards are **never** auto-marked as read, even if you've already read another copy
4. When you **actively read** a forwarded message in any chat, detects all other copies of the same original and marks them as read
5. Logs show channel names and message previews so you can see what's happening at a glance

## Setup

### 1. Get Telegram API credentials

Go to [my.telegram.org/apps](https://my.telegram.org/apps) and create an application to get your `API_ID` and `API_HASH`.

### 2. Configure

```sh
cp .env.example .env
```

Edit `.env` and fill in your credentials:

```
TG_API_ID=12345
TG_API_HASH=abcdef1234567890
```

Optional settings:
- `TG_PHONE_NUMBER` — skip the phone number prompt
- `TG_SESSION_PATH` — custom SQLite session file location (default: `~/.telegram_dup_checker/session.sqlite`)
- `TG_STATE_PATH` — custom state file location (default: `~/.telegram_dup_checker/state.json`)

### 3. Build and run

```sh
cargo build --release
./target/release/telegram-duplicate-message-checker
```

On first run you'll be prompted to authenticate with your phone number and verification code (and 2FA password if enabled).

## Architecture

```
src/
├── main.rs      # Entry point, update loop, signal handling
├── config.rs    # Environment variable loading
├── auth.rs      # Phone + code + 2FA authentication
├── tracker.rs   # In-memory duplicate tracking with JSON persistence
├── handler.rs   # Two-phase update processing (plan then execute)
└── marker.rs    # Mark messages as read via Telegram API
```

The update handler uses a two-phase design: phase 1 computes what needs to happen (holding only the tracker lock), phase 2 executes network I/O (holding only the marker lock). This avoids blocking state persistence during slow API calls.

The marker module maintains a peer cache with display names, populated at startup from all dialogs and updated as new messages arrive. This allows log output to show human-readable channel names instead of numeric IDs.

## State persistence

- Tracker state is saved to JSON every 5 minutes and on shutdown
- Writes are atomic (write to `.tmp` then rename)
- Entries older than 30 days are automatically cleaned up daily

## Dependencies

- [grammers](https://github.com/Lonami/grammers) — pure Rust MTProto client
- tokio — async runtime
- serde — state serialization
