# Colourways

Status: **v5.1, reviewed clean** (2026-11-19). v2 to v5 applied review rounds 1 to 4; round 5 found no P0 to P2 findings, and v5.1 applies its P3s (GPT-6 Astra and Gemini 4 Neon; section 9). Amends the colour section of [brand/brand.md](../brand/brand.md), decided in the 2026-11-16 workshop.

## 0. Decisions (owner, 2026-11-19)

1. **Revisit the brand.** abyss gains colourways in the structure Telegram and Signal use: a neutral background and a saturated fill on the user's own messages. Two brand rules are amended (section 5) rather than kept: "No Telegram visual DNA" and "No accent colour".
2. **Inspired, not copied.** Each colourway follows its source's structure with its own tuned values, and is named for its colour: **Ultramarine** (Signal-like) and **Sky** (Telegram-like). Keeping the palettes distinct is a design decision: abyss should not be mistaken for an official client at a glance. Telegram's API terms do not regulate palettes. They restrict app titles, forbid the official logo and require a prominent statement that the app uses the Telegram API, and all three stay as they are. "Telegram" and "Signal" are never used as colourway names.
3. **Age tones stay in the abyss colourway only.** The current hue-and-tint system becomes the colourway **abyss**, with its strata unchanged. Ultramarine and Sky use flat surfaces, as their sources do.
4. **Ultramarine is the default for new installs.** Existing installs keep abyss, including those that never opened Appearance (section 3).

## 1. Colourways

| Colourway | Surfaces | Own message | Incoming | Accent | User-tunable |
|---|---|---|---|---|---|
| abyss | the 7-step strata (brand.md) | `text` fill | white wash | none: contrast only | hue and tint per mode |
| Ultramarine | flat neutral | saturated blue, white text | neutral grey | the own-message blue | no |
| Sky | flat, slightly cool | pale blue by day, deep blue by night | white by day, slate by night | a sky blue | no |

The starting values below are OKLCH, with sRGB hex computed by `theme.rs`'s own conversion. The contrast tests (section 6) set the final values, and these may move. Sky's hues are kept off Telegram's `#3390EC`.

**Ultramarine**
- Day: surface 0.995 0; incoming 0.94 0; own 0.53 0.20 h265 (`#315FDD`); `on_own` white; text 0.20 0; accent = own, `on_accent` white.
- Night: surface 0.18 0; incoming 0.33 0; own, accent and their inks as by day; text 0.93 0.
- White on own is 5.53:1. Quiet text at today's 74% opacity is 3.78:1 and fails, so Ultramarine's quiet opacity inside own messages starts at 0.86 (4.53:1).
- **Own is pinned from both sides.** Inline code (white over a 10% white wash on own) is 4.57:1, so own cannot get lighter. The night rail rule on the tab hover wash is 3.003:1, so own (as the accent) cannot get darker, and the night surface cannot get lighter. Task 2 may need to split the accent from own if tuning needs room.

**Sky**
- Day: surface 0.955 0.010 h230; incoming 1.0 0 (white bubbles on a grey-blue pane, as the source does, so bubbles separate from the pane); own 0.91 0.055 h230 (1.14:1 from the pane) with dark `on_own` 0.22 0.02 h240; text 0.22 0.02 h240; accent 0.52 0.14 h230 (about 5.1:1 with white) with white `on_accent`.
- Night: surface 0.23 0.03 h245; incoming 0.29 0.035 h245; own 0.40 0.07 h245 with `on_own` 0.94 0.01 h245; text 0.94 0.01 h245; accent 0.70 0.12 h230 with dark `on_accent` 0.18 0.03 h245 (about 7.2:1).

Each colourway also defines `spoiler(side)` and `quiet_in(side)` (section 2). They are tuned in task 2 against the section 6 floors.

## 2. Theme roles

Today `Theme` exposes `text`, `on_text`, `incoming`, `hidden`, `mark`, `surface(depth)` and `text_on(depth)`, and call sites derive everything else: they swap pairs, fade by `QUIET` and draw rules in the row's ink. Section 2 replaces those derivations with named roles. Under abyss each role is defined as exactly the expression it replaces, so abyss renders unchanged by construction. Unit tests check this for every abyss choice and depth (section 6).

**Roles and their abyss definitions**

| Role | abyss (unchanged pixels) | Flat colourways |
|---|---|---|
| `surface(d)` | strata step | flat surface |
| `text_on(d)` | `text`, or `on_text` from the flip step | `text` |
| `row_mark(d)`: the selection rule on chat and inline-topic rows | `text_on(d)` | `accent` |
| `badge(d) -> (fill, ink)`: unread counts and the unread dot (fill only) | `(text_on(d), surface(d))` | `(accent, on_accent)` |
| `accent`, `on_accent`: buttons, the retry and jump buttons, the tab-rail rule | `text`, `on_text` | per colourway |
| `own`, `on_own`: the own-message fill and its text | `text`, `on_text` | per colourway |
| `incoming`, `on_incoming`: bubbles and input fields | white wash, `text` | per colourway |
| `quiet_in(side) -> f32`: the opacity of quiet text inside a bubble | `QUIET` on both sides | per colourway and side |
| `spoiler(side)` | `hidden` on both sides | per side, separated from that side's fill |
| `mark` | the ramp in the chosen hue | the public ramp at hue 215 |

There is no focus role: nothing in the app draws a focus ring today.

**Floating controls.** The jump and retry buttons float over panes of any depth and over message bubbles (`chat_view.rs:112`, the chat-list retry; `:580`; `topic_view.rs:89`). Under abyss by day they sink to 1.13:1 against the deepest surface today, and under Ultramarine the jump button can sit on an own bubble of exactly its fill. From task 2 they carry a 2-point ring in `on_accent`. Their edge then shows against anything beneath: over every surface and both bubble fills, either the fill or the ring reaches 3:1 (section 6). Under abyss this is a deliberate visual change in task 2, not part of task 1.

**Call-site inventory.** The implementation changes exactly these, and review checks the list is complete with `grep` for `theme.text`, `on_text`, `incoming`, `hidden`, `text_on`, `surface`, `QUIET`, `quiet(` and `faded(` under `crates/app/src`:

- **Rows:** `chat_view.rs` chat rows (`:401`, rule and `count_badge`), inline topics (`:255`), `topic_view.rs` (`:254`, badge and dot). These take `row_mark` and `badge`.
- **Bubbles:** `Side::fill` and `Side::ink` (`chat_view.rs:787` to `:799`) take `own`/`on_own` and `incoming`/`on_incoming`.
- **Input fields:** `view::field` (`view.rs:482`) takes `incoming` and `on_incoming`. Both `ui/text_input/element.rs` and `ui/password.rs` draw their placeholder (40%) and selection (20%) inside it. Those opacities move to shared constants in `theme.rs`, so the two widgets cannot drift apart.
- **Quiet text inside bubbles** fades by `quiet_in(side)` instead of `QUIET`. Each site keeps its current paint operation and changes only the factor, so abyss is byte-identical:
  - the `palette` `muted` colour (`chat_view.rs:53`, via `faded`; used for the target suffix of text links, `rich.rs:176`). `palette` takes the side, and both of its callers (`:959`, `:1023`) pass it;
  - the footer time and edited mark (`:949`), the echo status (`:978`) and the unsupported-message label (`:925`). These use `view::muted`, which is `.opacity(QUIET)` on an element. A sibling `muted_in(side, theme)` applies `.opacity(theme.quiet_in(side))`;
  - `reply_quote` (`:876`), which keeps `.opacity()` on the whole element so its border fades with it, as now.
- **Actions inside an own bubble** (`echo_row`, `chat_view.rs:972`: Retry, Discard, Put back, Dismiss, Copy text) take the inverse own pair `(on_own, own)` through `pill(fill, ink)`. They are not accent buttons: an accent pill would vanish on Ultramarine's blue bubble.
- **Accent:** `view.rs:494` (`button`), `chat_view.rs:628` (`jump_button`), `:648` (`retry_button`), and `tabs.rs:149` (the rail rule). `pill` (`:660`) takes its colours from its callers and reads no theme.
- **Spoilers:** `palette` takes `spoiler(side)` (`chat_view.rs:57`).
  - Task 1 keeps `hidden` on both sides under abyss. That fails the 3:1 floor on incoming bubbles today (2.58:1 at worst across the 120 choices).
  - Task 2 retunes abyss's `spoiler(Incoming)` per mode as an explicit visual change, until every choice clears 3:1.
- **Links and mentions** stay in the bubble's ink, underlined and bold. They never take the accent, which could equal the bubble fill.

`Depth` stays. Flat colourways ignore it, so rule 1 holds only under abyss.

## 3. Settings and persistence

**Files.**
- **The `theme` file is unchanged:** `abyss-theme 1` plus the night and day hue and tint lines. It holds only abyss's settings.
- **A new `colourway` file** sits beside it: `abyss-colourway 1`, then one of `abyss`, `ultramarine`, `sky`.
- **Each file is saved only when its own fields change.** Picking Ultramarine or Sky writes `colourway` and never creates or touches `theme`. Changing a hue writes only `theme`. `Themes` holds both paths and one ordered save queue, as today.
- **One root (task 2).**
  - `storage::app_dir()`, plus `app_dir_in(home)` for tests, becomes the only place that builds `Library/Application Support/dev.abyss`. It sits on `storage::home_dir()`, the `getpwuid_r` lookup.
  - `storage.rs`, `preferences.rs` (its private `app_dir`), `small_file.rs` and `Themes` all use it.
  - `SidebarFile` moves with `small_file::path`, from `$HOME` to the same lookup.
  - A run with `HOME` changed can then never read one root's `theme` and another root's `cache`. Tests inject the directory.
- **Hardened writes (task 2).**
  - `small_file::write` stops creating directories recursively and opening a predictable staging file.
  - Every small-file write (`theme`, `colourway`, the sidebar file) first calls `abyss_core::secret::private_directory` on `dev.abyss`. That call:
    - refuses a symlink, a non-directory and a directory owned by someone else;
    - narrows an owned directory's mode to `0700` and strips its ACLs, rather than refusing it, as `storage::prepare` does;
    - creates only the leaf, never `Library/Application Support`.
  - The write itself goes through `abyss_core::secret::write_private`, which stages under a random name, exclusively.
  - **A missing `Library/Application Support`** now stops user-triggered theme and sidebar saves too, not only the startup write. This matches `storage::prepare` and `Preferences::init`. It changes today's behaviour, which created the directory recursively. The choice stays in memory for the run.
  - **Errors:** any write error keeps the choice in memory and logs only the error kind. `write_private` can fail after the rename, if syncing the directory fails, so a reported failure does not mean the old file is still there. That is harmless here, because the next save rewrites the whole file.
  - **Cost:** `private_directory` runs on every save. That means a chmod, an ACL strip and a parent sync per hue or tab click, on the background executor. This is accepted rather than adding a third directory-hardening routine beside `storage::private_dir` and `secret::private_directory`. The helper is reused unchanged: the Keychain migration relies on its parent sync.
- **Hardened reads (task 2).**
  - `small_file::read` keeps its own 128-byte cap and does not switch to `read_private`, which has a 64 KiB cap, changes permissions on read and returns secret buffers.
  - The file is opened with `O_NOFOLLOW | O_NONBLOCK`, and anything that is not a regular file is an error. A dangling symlink at `colourway` is then an invalid file rather than `NotFound`, so it cannot trigger the startup write, and a FIFO cannot block `Themes::init` on the main thread.
  - `small_file::remove` checks with `symlink_metadata` that `dev.abyss` is an owned directory before unlinking, and creates nothing.
    - A missing `dev.abyss` still returns `Ok(())`: something already gone is fine, as on logout today.
    - A symlink or a non-directory in its place is refused, and nothing is unlinked.

**Reading.**
- A `colourway` file that is missing (`ErrorKind::NotFound`) is the only case that counts as "no choice yet".
- Any other read error (not UTF-8, permission, a directory), a wrong header, an unknown name or extra bytes means the default colourway, and the file is not rewritten until the user picks something.
- **Downgrade is harmless.** An older build never reads `colourway`, so it shows abyss with the user's saved hues. When it saves, it writes a valid `theme` file, which this build reads back unchanged.

**The default.**
- **Task 2:** the default is abyss, and nothing is written at launch. A missing `colourway` file just means abyss.
- **Task 3:** the first real launch with no `colourway` file decides once and writes the choice:
  - **Existing install:** a `theme` file or the real cache directory exists under the root above. abyss is chosen.
  - **Otherwise:** Ultramarine is chosen.
  - **The write is synchronous inside `Themes::init`.**
    - `Themes::init` runs before the window opens, and the window's `Root::new` starts the session and creates the cache directory (`shell/mod.rs:234`, `session.rs:93`).
    - The write is the hardened `small_file::write` above, which validates `dev.abyss` itself. `Themes::init` does not depend on `Preferences` or on the order of the other initialisers, and production and `Themes::init_in` run the same path.
    - The check therefore always sees the state from before this launch, and a crash after it cannot turn a new user into an "existing" one on the next launch.
    - The order is plain sequential code in `shell::open`, which spawns a real core and cannot run under `TestAppContext`. What is tested is that `colourway` exists on disk as soon as `Themes::init_in(dir, policy)` returns, before any executor runs.
  - If the write fails, the in-memory choice stands for the run and the check repeats next launch. The cache directory exists by then, so a new user whose first write failed comes back as abyss. This is accepted and logged at warn level, without the path.
  - **Only real launches persist at startup.**
    - `Themes::init` takes a policy, `FirstLaunch::Persist` or `FirstLaunch::MemoryOnly`. This keeps `ui::theme` free of `crate::launch` and its `fake` cfg.
    - `shell::open` passes `Persist` only for `matches!(launch, Ok(Launch::Telegram { .. }))`. A fake launch, and any failed launch including `LaunchError::FakeNotBuilt` (`--fake` on a release build), gets `MemoryOnly`.
    - Under `MemoryOnly`, a missing file means the default and nothing is written, so `cargo dev` cannot pin the real install.
- **One colourway covers both modes.** Each colourway defines night and day, and the system appearance picks between them as it does today.
- The `small_file.rs` comment about valid sizes is updated. Both files stay well under `MAX_FILE_BYTES`.

## 4. Picker

- **Colourway row:** at the top of Appearance, three swatches, each showing its own fill on its surface, labelled with the colourway's name.
  - Every swatch has a hairline (`theme::line(theme.text)`), since a swatch's surface can match the panel.
  - The selected one is marked by a ring in `text` and a heavier label weight, never by the accent alone (rule 2).
- **Notes:** a one-line note sits above the row. The current note ("Pick a hue and how much of it…") moves into the abyss-only block, which also holds the hue and tint controls.
- **Reset:** shown only under abyss. It calls a dedicated helper that resets abyss's hues and tints to the abyss defaults, keeps `Colourway::Abyss`, and writes only `theme`. It never calls `Settings::default()`, which may name another colourway. Choosing a colourway never changes the saved hues.
- **Footer layout:** the footer keeps a left slot that holds Reset or nothing, so Done stays at the right edge when Reset is hidden.
- Choices apply immediately and save as they do today.

## 5. Brand amendments (brand.md)

- **Positioning:**
  - "No Telegram visual DNA: no paper plane, no sky blue" becomes "No Telegram marks: no paper plane, no logo. Colourways take their structure from familiar messengers under their own names and values; 'Telegram' and 'Signal' are never colourway names."
  - "The brand must stretch to other networks" stays.
  - The descriptor and the API-use statement stay unchanged.
  - The terms note records decision 2 as a design decision.
- **"Rules that hold in every theme"** splits in two:
  - **Every colourway:** rule 2 as amended ("No state depends on hue alone": the accent may mark a state, but shape, weight or a rule marks it too), rule 4 (type) and rule 5 (radius and bands).
  - **The abyss colourway:** rule 1 (old things sink), rule 3 (the user's words carry the most contrast) and rule 6 (the strata).
- **"Hue carries no meaning anywhere"** becomes "No state depends on hue alone". Its examples (avatars, sender names, links, mentions, errors) still hold in every colourway.
- **The colour section** describes three colourways:
  - The existing tables and the 120-combination contrast statement become abyss's.
  - "Two numbers per mode" and "three lines of text" are scoped to abyss.
  - The colourway file is described.
  - The weakest-pair figures are given per colourway.
- **The public ramp** governs the icon, website, frames and backdrops. In-app screenshots show the app as it is, so they may show any colourway. The store set shows Ultramarine and one abyss shot.
- **The mark** is drawn in the chosen hue and tint under abyss, and in the public ramp (hue 215, default tint) under Ultramarine and Sky.
- **Leftover lines** move under abyss: "Users choose the colour, so the brand is the set of rules that survive any hue" (brand.md:27) and "Which step a thing takes" (brand.md:65).

## 6. Tests

**Which task adds what.**
- **Task 1** adds the role-equivalence tests below, rewires the existing contrast test (`weakest_pair`) to the new roles without widening it, and measures abyss's bubble-separation floors.
- **Task 0** adds the deterministic snapshot setup.
- **Task 2** adds everything else in this section once its visual retunes have landed, except the first-launch tests.
- **Task 3** adds the first-launch tests (under Settings).

**Role equivalence (task 1).** For every abyss choice (120) and every depth in both modes, each new role equals the expression it replaces.

**Contrast (`theme.rs`)** runs for every colourway in both modes, and under abyss for all 120 choices. It checks every rendered foreground and background pair, using its real opacity:
- text and quiet text on every surface, and on `incoming` over every surface;
- `on_own` at full strength and at `quiet_in(Outgoing)` on `own`, and inline code (`on_own` on a 10% wash over `own`), and the same three checks for incoming;
- `on_accent` on `accent` at full strength only (buttons and badges are never quiet), and `badge(d)` at every depth;
- the tabs: full and quiet text on the 6% hover wash; the rail rule (`accent`) on the hover wash and on the plain surface;
- the menu: text on the 8% hover wash;
- input fields: text, placeholder and selected text, all on `over(incoming, surface(NOW))`. This covers both `text_input` and `password`;
- text on the sender-header wash; avatar initials and the topic glyph on their 14% ink wash, at every depth;
- the unread-divider label (quiet text on the surface).

- **Text pairs** need 4.5:1.
- **Non-text marks** need 3:1:
  - `row_mark(d)` and the badge fill on `surface(d)`;
  - `accent` on `surface(NOW)`, where the pinned buttons sit;
  - floating controls over every surface and both bubble fills, where either the fill or its `on_accent` ring must reach 3:1.
- **`spoiler(side)`** must be opaque and reach 3:1 against its bubble fill. Hidden text has no readability requirement. Round 3 found that abyss's incoming spoiler at L 0.65 by night and L 0.55 by day clears 3:1 for every choice and depth, which is the starting point for task 2's retune.

**Bubble separation.** For each mode, `own` and `incoming` against every surface they sit on must be at least as separated as abyss's weakest bubble-against-surface pair in that mode. That is the minimum over both sides and all 120 choices, about 1.068:1 by day and 1.175:1 by night per round 2's arithmetic, re-measured in task 1. The starting values pass it: Sky's day own is 1.14:1.

**Placeholders and the counter** (pre-existing, visual changes in task 2):
- Input placeholders draw at 40% opacity, which is about 2.5:1 on the new colourways. As hints they get a 3:1 floor on `over(incoming, surface(NOW))`, which needs about 48%. The shared constant rises until every colourway and every abyss choice meets it.
- The composer's length counter draws at 60%, which is 4.24 to 4.36:1 under abyss today. It moves to `QUIET` and comes under the 4.5:1 check.

**Existing tests updated:**
- `defaults_match_the_brand_sheet` and `old_things_sink` construct abyss explicitly.
- New tests: Ultramarine and Sky have equal surfaces and a constant `text_on` at every depth.
- `appearance_choice_rethemes_the_window_and_reset_restores_it` (`shell/tests.rs:132`) runs under abyss. A new test covers switching colourways, and Reset staying in abyss.

**Settings:**
- The tests take the default colourway as a parameter: abyss in task 2, Ultramarine from task 3.
- **Task 3 first launch:**
  - a missing `colourway` file with a `theme` file present means abyss, written;
  - a missing `colourway` file with the cache directory present means abyss, written;
  - a missing `colourway` file with neither means Ultramarine, written;
  - a fake launch, then a real launch: the real launch still decides (nothing was written by the fake one);
  - `colourway` is on disk when `Themes::init_in` returns, before any executor runs;
  - `MemoryOnly` (a fake launch, or any failed launch) writes nothing;
  - a non-`NotFound` read error means the default, not written.
- An invalid `colourway` file means the default, not written.
- Picking a colourway does not create a `theme` file. Reset under abyss keeps abyss and writes only `theme`. `preferences.rs`'s `toggling_persists_and_appearance_reset_keeps_the_choice` (`:125`) moves to the abyss Reset helper too.
- **Hardening:**
  - a symlinked `dev.abyss` and a regular file in its place are refused;
  - a `0755` `dev.abyss` is narrowed to `0700` and the write succeeds (foreign ownership cannot be set up without privileges; it is enforced by `private_directory`'s `uid != getuid()` check, which no test covers);
  - a symlink at the legacy `.tmp` name or at the destination: the write succeeds through a fresh staging file and the link's target is untouched;
  - a missing parent of `dev.abyss` fails without creating anything;
  - reads: a dangling symlink and a FIFO at `colourway` are errors, not `NotFound`, and the FIFO does not block;
  - removal: a missing `dev.abyss` is `Ok`, and a symlinked `dev.abyss` is refused without unlinking anything.
- **`settings::tests::saves_privately_and_loads_back`** (`settings.rs:238`) creates its scratch directory before saving to `nested/theme`, which today's recursive creation hid. It now checks that no staging file is left in the directory, instead of checking for the old `.tmp` name.
- A v1 `theme` file round-trips unchanged under every colourway.
- An upgrade, then a downgrade save, then an upgrade again keeps the hues.
- Truncated files and files with a future header are covered.

**Snapshots:**
- **Determinism comes first, in task 0.** `format::now()` and `format::local_tz()` get snapshot-only, thread-local overrides like `theme::forced`, and the harness sets explicit abyss settings. Task 1's "before" set is taken on top of task 0, and its "after" set on the same machine, so the comparison is byte for byte.
- `list_deep` stays pinned to abyss so the strata and the flip stay covered.
- **Matrix, for Ultramarine and Sky, day and night:**
  - welcome, sign-in, chat, tabs, inline topics, unread divider, menu, archive;
  - the appearance panel with each colourway selected;
  - two composite fixtures instead of one per state, to keep the set small enough to check by eye:
    - **A group scene:** sender headers, a reply quote, a failed send with its actions, the jump button over an own bubble, and a long composer with a selection and its counter.
    - **A hover scene:** a selected and an unselected tab hovered, and a menu entry hovered.

## 7. Tasks

0. **Deterministic snapshots.**
   - Overrides for the clock and the time zone, and explicit settings in the harness. No product change.
   - Lands first, so it gives task 1 a reproducible baseline.
1. **Role split, no visual change.**
   - The roles and the call-site inventory (section 2), the role-equivalence tests, and the contrast tests rewired to the roles under abyss.
   - The snapshot set from task 0 is taken before and after and must be byte-identical.
2. **Colourways behind the abyss default.**
   - Ultramarine and Sky, the `colourway` file with independent saves, the picker and its Reset, and the full contrast matrix.
   - The single `storage::app_dir()` root and the hardened small-file reads, writes and removals (section 3).
   - The explicit abyss visual changes:
     - the incoming spoiler;
     - the ring on floating controls;
     - the placeholder and counter opacities.
   - The new snapshots, checked by eye.
   - The default stays abyss and nothing is written at launch, so main is never left with a default the picker cannot leave, and no install is pinned early.
3. **Default and brand.**
   - The first-launch decision and its write (section 3), and the new-install default of Ultramarine.
   - brand.md is updated as in section 5.
   - This is the only task that changes what a first launch does.

Each task follows the usual loop: plan review, implementation on an agent branch, independent review until it is clean of P0 to P2 findings, then a squash-merge.

## 8. Risks

- **Confusion with the official clients:** reduced by the neutral names and the moved hues, but a blue messenger look is still closer to Telegram than abyss was. The sign-in screens keep the statement "abyss is an unofficial Telegram client. It uses the Telegram API."
- **Tight pairs:**
  - Ultramarine's own fill has almost no room either way: 4.57:1 for inline code against 4.5, and 3.003:1 for the night rail-rule hover against 3.
  - Ultramarine's quiet text on own messages and Sky's pale own fill by day are also tight.
  - The tests decide the final values.
- **Existing-install detection** can only see the cache directory and the `theme` file:
  - Any earlier real launch counts as an existing install, even one that quit on the Welcome screen: the session creates the cache directory at startup. Such users keep abyss, which they saw last.
  - `--fake` launches neither create the real cache directory nor write `colourway`, so they never count as an install.
  - A first write that fails turns a new user into abyss on the next launch (section 3). This is accepted as rare and harmless.
- **Screenshots and copy:** first-run screenshots and the store listing describe the abyss look. They are redone before publishing, which is already an open item (brand.md, "Open"). Task 3 does not change them.

## 9. Review log

**Round 1** (2026-11-19, GPT-6 Astra and Gemini 4 Neon, reviewing v1). Findings marked A are Astra's, N are Neon's. All are accepted and applied in v2.

- **N1 (P0), A1 (P1): one accent pair breaks abyss's row marks and badges.** These draw in the row's own ink, so they depend on depth. A flat accent changes pixels and drops to 1.13:1 on the deepest day rows. Fixed with the depth-aware `row_mark(d)` and `badge(d)` (section 2).
- **A2 (P1), N4.3 (P1): quiet text inside own messages has five independent 74% paths.** Fixed by routing all of them through `quiet_in(side)`, listed in the inventory. `reply_quote` keeps fading the whole element.
- **A3 (P2), N4.1: `echo_row` actions are an inverted pair.** They are not accent buttons. Fixed by mapping them to `(on_own, own)`.
- **N2 (P1), A5 (P2): Sky's accents fail with white text, `on_accent` was unspecified, Sky night text had no hue, and Ultramarine's ratio was wrong.**
  - Sky's day accent is retuned to L 0.52 with a white ink. Sky's night accent gets a dark ink.
  - Sky night text gets h245.
  - Ultramarine is restated at 5.53:1, with a quiet opacity of 0.86 and an inline-code floor.
- **N3 (P1): Sky's day incoming bubbles vanished into the pane (1.03:1).** The surface is retuned to 0.955 and a bubble-separation test is added.
- **N4.2, N4.4, N4.5, A5: missed roles.**
  - Added: the jump button, the tab-rail rule, and a per-side `spoiler`.
  - Removed: the phantom focus role.
- **A4 (P2), N5.1 (P1): the contrast matrix missed real pairs.**
  - Added: inline code, hover washes, input selection, the sender wash, non-text marks at 3:1, and the spoiler check.
  - `on_accent` is checked at full strength only.
  - The placeholder (40%) and counter (60%) gaps are pre-existing and are fixed as explicit visual changes in task 2.
- **A7 (P2), N5.2: existing tests assume the defaults.** `old_things_sink`, the brand-table test, `appearance_choice_rethemes…` and `list_deep` are pinned to abyss.
- **A8 (P2): the snapshots use the wall clock, so "byte-identical" was unprovable.** The harness pins the clock, the time zone and the settings in task 1. Role-equivalence unit tests carry the main proof.
- **A6 (P2), N7 (P2): downgrade and existing users.**
  - A v1 build rejected a v2 `theme` file and overwrote it. The colourway now lives in its own file, so the `theme` file never changes format.
  - Installs that never opened Appearance were silently moved to Ultramarine. They are now detected once, by the `theme` file or the cache directory, and pinned to abyss.
- **N6 (P1): the picker's note and Reset were wrong under the flat colourways.** The note moves into the abyss block. Reset shows only under abyss and stays in abyss. The selected swatch is marked by a ring.
- **A9 (P2), N8 (P2): brand.md had contradictions left over.** Section 5 now covers:
  - the "every theme" heading;
  - "hue carries no meaning";
  - the public ramp versus screenshots;
  - how the mark is drawn;
  - "two numbers" and "three lines";
  - the weakest-pair figures;
  - "stretch to other networks".
- **A10 (P2): the Telegram-terms rationale was overstated.** It is restated as a design decision (decision 2). The naming ban covers colourway names only.
- **A11 (P2), N9 (P2): task 2 flipped the default before a picker existed.** It is resequenced: colourways land behind the abyss default, and the default flip plus the brand update form a small final task. Welcome, sign-in and day and night appearance shots are added to the matrix.
- **N10 (P3): wrong line cited for `Side::fill`.** Corrected to `:787` to `:799`.

**Round 2** (2026-11-19, GPT-6 Astra and Gemini 4 Neon, reviewing v2). Both confirmed the round 1 fixes to the roles, the quiet paths, `echo_row`, the separate file, Reset's intent and brand.md. All new findings are accepted and applied in v3.

- **A1, N1 (P1): an unchanged abyss spoiler fails the new 3:1 floor on incoming bubbles (2.58:1 at worst).** Task 1 keeps it, so equivalence holds. Task 2 retunes it as an explicit visual change, and the test is enforced from task 2.
- **A2, N2 (P1): floating jump and retry buttons fall to 1.13:1 on deep abyss day panes, and Ultramarine's jump button can match an own bubble.** They get an `on_accent` ring in task 2. A test now checks that the fill or the ring reaches 3:1 over every surface and both bubbles, and `accent`'s own check is scoped to `surface(NOW)`.
- **A4, N4 (P1): first-launch persistence.**
  - `--fake` wrote to the real directory; it now never persists at startup.
  - The write is synchronous and before `Root::new`, with an ordering test.
  - Only `NotFound` counts as "no choice".
  - The write moves to task 3. In task 2 it would have pinned every install to abyss, and the default flip would then reach no one. That is a new issue found while applying this round.
- **N3 (P1): `small_file` used `$HOME` while `storage` used `getpwuid_r`.** Everything now resolves against one root from `storage::home_dir()`.
- **N5 (P1): task 1's byte-identical claim could not pass while the clock pinning was part of task 1.** Pinning becomes task 0, landed first. The `muted` sites keep `.opacity()` and only change the factor.
- **A3, N9 (P2): matrix gaps.** Added:
  - quiet text and the rail rule on the tab hover wash;
  - the avatar and glyph washes at every depth;
  - input fields on `over(incoming, surface(NOW))` for both widgets, with shared constants;
  - default-parameterised settings tests.
- **N6 (P2): inventory gaps.** `view::field` is added. `:648` is `retry_button`. Line numbers are corrected, and both `palette` callers are listed.
- **N7 (P2): Reset and saving.**
  - Reset gets an abyss-preserving helper that never uses `Settings::default()`.
  - Each file is saved only when its own fields change.
  - The footer keeps a left slot so Done stays right.
- **N8 (P2): the bubble-separation floor was ambiguous, and Sky's day own sat at 1.07:1.** The floor is now per mode, the minimum over both sides and all choices. Sky's day own is darkened to 0.91 0.055 (1.14:1).
- **N10 (P3):**
  - Two leftover brand.md lines move under abyss.
  - The mark uses hue and tint under abyss.
  - Snapshot fixtures gain a reply quote, a group sender header and the jump button over an own bubble.

**Round 3** (2026-11-19, GPT-6 Astra and Gemini 4 Neon, reviewing v3).
- **What they confirmed:**
  - Every round 2 fix checks out against the code.
  - The ring guarantee holds over every surface and both bubbles; the weakest cases are Sky night at 3.51:1 and Sky day at 3.96:1.
  - The separation floors are 1.068 by day and 1.175 by night, and Sky's day own is 1.136.
  - Abyss's incoming spoiler can clear 3:1 (L 0.65 by night, 0.55 by day).
- All new findings are accepted and applied in v4.
- **A1, N1 (P2): the automatic startup write bypassed the `dev.abyss` hardening.**
  - Today `small_file::write` creates directories recursively and truncates a predictable staging file, following symlinks.
  - All small-file writes now validate the directory with `private_directory` and write through `write_private`. `Preferences::init` runs before `Themes::init`, and the startup write happens only if validation succeeded.
  - This also hardens today's user-triggered saves.
- **N2 (P2): "takes the launch kind" didn't fit `Result<Launch, LaunchError>`, and `--fake` on a release build is `Err(FakeNotBuilt)`.** Fixed with an explicit `FirstLaunch` policy, `Persist` only for `Ok(Launch::Telegram)`.
- **N3 (P2): an ordering test of `shell::open` cannot run under `TestAppContext`.** The order is plain sequential code. The test asserts the write is synchronous.
- **N4 (P3):** one `storage::app_dir()` for every user of the path, `SidebarFile` included, assigned to task 2.
- **N5 (P3):** the task 1 and task 2 test boundary is stated once, at the top of section 6.
- **N6 (P3):** Ultramarine's two-sided margin is recorded in sections 1 and 8.
- **N7 (P3):** the snapshot fixtures are consolidated into two composite scenes.
- **N8 (P3):**
  - `chat_view.rs:112` is added to the floating controls.
  - `preferences.rs:125` moves to the abyss Reset helper.
  - Colourway swatches get a hairline.

**Round 4** (2026-11-19, GPT-6 Astra and Gemini 4 Neon, reviewing v4).
- **What they confirmed:**
  - The `FirstLaunch` policy and the synchronous-write test fit the startup code.
  - `write_private` writes plain files that `small_file::read` can read.
  - Moving `Preferences::init` first changes nothing.
- All findings are accepted and applied in v5.
- **A1, N3 (P2): `saves_privately_and_loads_back` relied on recursive directory creation.** The fixture is fixed and a missing-parent test is added. The behaviour change for user-triggered saves is documented.
- **N1 (P2), A2 (P3): `private_directory` narrows loose permissions rather than refusing them.** The plan is corrected, and the test now expects narrowing.
- **N2 (P2), A3 (P3): gating the startup write on `Preferences` was wrong and unneeded.** Its `failed` flag also covers a malformed preference. The gate and the ordering requirement are dropped, and `small_file::write` validates its directory itself.
- **N4 (P3): the read path could follow a dangling symlink to `NotFound` (triggering a write) or block on a FIFO.** Reads now use `O_NOFOLLOW | O_NONBLOCK` and accept regular files only. `remove` checks the directory first.
- **N5 (P3): `write_private` stages under a random name, so a planted staging symlink was ill-defined.** The tests plant symlinks at the legacy `.tmp` name and at the destination.
- **N6 (P3): the per-save cost of `private_directory`.** It is recorded and accepted. `write_private`'s error-after-rename case is recorded too (Astra).

**Round 5** (2026-11-19, GPT-6 Astra and Gemini 4 Neon, reviewing v5). Both: **clean of P0 to P2.** v5.1 applies the P3s:
- section 6 states which task adds which tests, including tasks 0 and 3;
- the claimed test coverage for foreign ownership did not exist, so it is restated as an untested `uid` check;
- `small_file::remove`'s missing-directory and symlink behaviour is stated, with tests;
- section 7, task 2 lists hardened reads and removals too;
- section 8 no longer claims task 3 redoes the screenshots;
- line citations are corrected.
