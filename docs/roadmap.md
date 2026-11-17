# abyss roadmap

Status as of 2026-11-17. This is the index of what is done, what is next and what is still undecided. Each stage's detailed plan lives in its own `docs/plans/stage-N.md`, written when that stage starts.

## v1 scope

v1 covers private chats, groups, forum topics and images.

It leaves out:
- broadcast channels, secret chats and calls;
- stickers, video playback and link previews;
- Mini Apps and an in-app browser.

Other media appears as an inert placeholder. Nothing outside this scope is added without an explicit decision.

Message links are copy-only. The only URL abyss ever opens is the hardcoded `https://my.telegram.org/apps` in onboarding.

## Stages

| Stage | Scope | Status |
|---|---|---|
| 1 | Sign-in and onboarding, chat list, text history with formatting, sending, live updates, logout, encrypted message cache | Code done and reviewed against the fake backend; live verification (task 12) open |
| 2 | Forum topics | Not started; plan to write |
| 3 | Notifications, Developer ID signing and notarization, App Sandbox, accessibility | Not started; the bundle (`scripts/bundle.sh`) is signed ad hoc |
| 4 | Media download and decode: chat images, then avatars (letter avatars and stripped thumbnails included) | Not started; images come last in v1 |

Notes:
- **Stage 1** is specified in [plans/stage-1.md](plans/stage-1.md). Its review log (section 15) runs through round 37.
- **Stage 2:** the model already carries `ThreadKey { chat, topic }`, with `topic` always `None` for now. Forum chats show "Topics arrive in a later version."
- **Stage 3:** until signing lands, every rebuild of the ad-hoc-signed dev binary triggers Keychain ACL prompts (stage-1.md section 14, risk 3).
- **Archive and chat folders:** proposed in [plans/folders.md](plans/folders.md) as three PRs: the archive as its own view (A), chat folders read-only (B), then archiving and folder editing (C).
- **Risk (folders, part C):** folder edits are recovered best-effort after an unknown outcome. An uncertain write that the server applies after the next edit's fetch can lose one of the two changes, because `messages.updateDialogFilter` replaces the whole folder with no version check (folders.md, part C, "Risks"). After an uncertain write, the next folder edit waits for the instance's retirement to complete, like every other client operation under the single rebuild mechanism; a retirement that hangs holds it too.
- **Stage 4:** images decode through one hardened path: a pure-Rust decoder (not ImageIO), strict size and pixel caps, downscaling and the encrypted cache, optionally in a sandboxed process. See stage-1.md section 0.

## Task 12: live verification (Stage 1)

This is blocked on a real `api_id`/`api_hash`. Everything else in Stage 1 has run only against the fake backend.

**Onboarding** (section 17.5):
- [ ] Create a real app on my.telegram.org following only the in-app text, and record any wording fixes.
- [ ] Check the `cx.open_url` hand-off to my.telegram.org.

**Login and Keychain** (section 13, task 6b):
- [ ] Code delivered by the app and by SMS, the resend countdown, 2FA, `API_ID_INVALID`, and a flood wait.
- [ ] Keychain prompts for `dev.abyss.apicreds`, and the retry when the Keychain is locked.
- [ ] Logout against the real Keychain.

**Backend** (section 13, task 11):
- [ ] Run the ignored contract test against a real account, and against the test DC if it is usable.
- [ ] Confirm the `getHistory` After semantics (`add_offset = -limit`, `min_id`).
- [ ] Confirm the dialogs cursor at page boundaries when broadcasts are excluded.
- [ ] The baseline arrives within 10 s, both on a fresh session and on a stored one.
- [ ] `quit` stops the runner promptly.
- [ ] Watch for grammers-internal panics on unusual peers in the update path.
- [ ] Check what `iter_dialogs` returns for the archive folder (section 14, risk 8). Part A of [plans/folders.md](plans/folders.md) relies on archived dialogs arriving with `folder_id = 1`. Also check where pinned archived chats appear in that combined traversal. If pins are per folder, `merge_first_page` must become folder-aware.

**Text input** (section 13, task 8):
- [ ] Pinyin and Japanese IME in the composer:
  - the candidate window is positioned correctly;
  - Enter commits a composition instead of submitting.
- [ ] IME in the masked `api_hash` field.
- [ ] Dead keys in the password field with Secure Event Input on.
- [ ] VoiceOver does not read the password.

**Cache:**
- [ ] History survives a restart. Check that the `~/Library/Application Support/dev.abyss/cache` permissions are 0700 with no ACLs.

## Settings page (partly built, not yet specified)

An appearance panel exists (`shell/appearance.rs`). It sets hue and tint for night and day, saved to a small file in Application Support (`ui/theme/settings.rs`). A fuller settings page has no plan section yet.

Open question: should it show active sessions? If so:
- Listing them is read-only and low risk.
- Ending other sessions acts on the account and is security-sensitive.
- Proposal: list sessions in v1 and leave out ending them.

## Open decisions

- Active sessions on the settings page (above).
- **A project `api_id` for packaged releases** (stage-1.md section 17.1, amendment):
  - Packaged builds are to embed one and go straight to sign-in.
  - Source builds keep bring-your-own credentials.
  - None is registered yet, so nothing changes in code until one is.
- **Brand:** the items under "Open" in [brand/brand.md](brand/brand.md):
  - the layered icon;
  - per-network Welcome copy;
  - fonts off macOS;
  - publishing the landing page and store listing.

## Non-blocking follow-ups

These came out of review and are tracked here so they aren't lost. None blocks Stage 1.

- **Tests:**
  - A wiring test for `shell::start` proving that the real launch opens the disk cache (task 9b).
  - Strengthen the round 22 test (`review10g.rs`) with a Newer-direction case and a case that checks nothing is answered twice.
- **Code:**
  - Move `leak_lock_for_tests` behind a test-support feature.
  - Add an overlay generation check on `ViewingTail(Some)`.

## How work is done

- **Plan first.** Each task gets a plan section, which is reviewed before implementation.
- **Review loop.**
  - Implementation runs on agent branches.
  - An independent reviewer (GPT-6 Astra) re-checks every task until it finds nothing of P0–P2 severity.
  - Each fix round is recorded in the plan's review log.
- **Merging.** Branches are squash-merged into `main`, and only reviewed-clean work is pushed. Nothing in progress goes to `main`, and pushed history is never rewritten.
- **Checks before every push:**
  - `cargo fmt --all --check`.
  - clippy with `-D warnings`, with and without `--features fake`.
  - `cargo test --workspace --features fake`, also run with `ABYSS_TEST_STORE_DELAY_MS=50`.
  - `abyss-core` must not depend on GPUI.
- **Dependencies.** They are pinned to upstream git revisions: grammers from codeberg.org/Lonami/grammers, GPUI from zed-industries/zed. They are bumped deliberately, with a diff review.
