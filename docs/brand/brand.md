# abyss brand

Status: decided in the branding workshop on 2026-11-16, amended for colourways on 2026-11-19 (`docs/plans/colourways.md`). The colour system, the hue and tint picker, type, the mark and rules 1 to 5 are implemented in `crates/app/src/ui/theme.rs`, `ui/mark.rs` and the shell views. The icon is drawn (`assets/icon/`) and attached by the bundle script. A first landing page (`site/index.html`) and store listing copy (`docs/brand/store-listing.md`) exist; neither is published. What remains is under "Open".

**Checking the look.** The views can be rendered to PNG without opening a window:

```sh
ABYSS_SNAPSHOT_DIR=/path/to/shots cargo test -p abyss --features snapshots shell::tests::snapshots
```

It writes the Welcome and sign-in screens, an open chat, the appearance panel, tabs, topics, menus, a busy group chat and hover states in every colourway, the group chat in Sky with a chosen accent, and the chat list at three depths under abyss, in day and night. Shots other than abyss's are prefixed with the colourway's name.

## Positioning

- **Audience:** privacy-curious mainstream users.
- **What it is:** the quiet, hardened window onto every network you message on. Telegram first.
- **Lead claim:** calm. Safety is the reason the calm can be trusted.
- **Idea:** the stream is the abyss; the app is the glass you read it through.
- **Personality:** austere and exact, calm and quiet. Not menacing, not wry.
- **Name:** `abyss`, lowercase, no article. "The abyss" means the endless stream, never the product.
- **No Telegram marks:** no paper plane, no logo. Colourways take their structure from familiar messengers under their own names and values; "Telegram" and "Signal" are never colourway names. The brand must stretch to other networks.
- **Terms note:** colourways that echo familiar messengers are a design decision (owner, 2026-11-19, decision 2 of the colourways plan). They copy structure, not marks, names or exact values.
- **Tagline:** Look in. Nothing looks back.
- **Descriptor:** "abyss, an unofficial Telegram client" as a store title, until a second network ships. In the app: "abyss is an unofficial Telegram client. It uses the Telegram API." Telegram's API terms allow "Telegram" in a title only after "Unofficial", forbid its logo, and require stating prominently that the app uses the Telegram API. The sign-in screens carry the wordmark, the tagline and that sentence.

## Rules

Rules keep their workshop numbers, so a reference such as "rule 3" still holds.

**In every colourway:**

- **Rule 2: No state depends on hue alone.** The accent may mark a state, but shape, weight or a rule marks it too.
- **Rule 4: People are set in Charter.** Names and the wordmark use the serif. Messages, times and states use Avenir Next.
- **Rule 5: One radius, full-width bands.** Messages and the composer share a 12-point corner. Chat rows touch edge to edge.

**In the abyss colourway:**

- **Rule 1: Old things sink.** Tone tracks age in the chat list and in history. Older is darker, in both modes.
- **Rule 3: The user's words carry the most contrast on screen.** Everything that arrives is quieter.
- **Rule 6: One graphic device: the strata.** The ramp used in order, lightest first. No second colour, no decorative gradient.

## Colour

**Seven colourways**, picked at the top of Appearance. Each defines a night and a day theme, and the system appearance picks between them. Six are flat: one surface at every depth, a coloured fill for the user's own messages, and an accent.

- **Ultramarine:** flat neutrals and a saturated blue (oklch 0.51 0.20 265) for the user's own messages and the accent. The default.
- **Sky:** flat, slightly cool surfaces, white incoming messages by day, and a sky-blue accent.
- **Fern:** a warm pane, soft green messages and a deep green accent; dark teal by night.
- **Violet:** flat neutrals and a saturated violet; by night the accent is a light lilac inked dark.
- **Ember:** warm paper and a terracotta accent; brown-black by night.
- **Graphite:** no hue at all. The user's own messages take the strongest grey on screen.
- **abyss:** the strata below. The user chooses its hue and tint; the rest of this section up to the public ramp describes it.

**Choosing colour.** Every colourway but Graphite lets the user choose its colour. Under abyss that is its hue and tint per mode. Elsewhere it is one accent hue, from the same 24 on offer, that recolours the own-message fill and the accent (badges, row marks, buttons, the tab rail) and leaves surfaces and text alone. A chosen hue is drawn at the WCAG luminance of the colourway's own colour, so it reads exactly as well. Reset puts back the showing colourway's own and stays in it.

**Files.** The colourway is saved as two lines of text in `~/Library/Application Support/dev.abyss/colourway` (`abyss-colourway 1`, then its name in lowercase), and the colour choices as four lines in `theme` beside it (`abyss-theme 2`, abyss's night and day hue and tint, then `accent default` or a hue). Both are readable only by the user. Nothing is written until the user picks; a missing or damaged file means the defaults.

**Contrast in the flat colourways**, the weakest text pair over every accent on offer:

| Colourway | Day | Night |
| --- | --- | --- |
| Ultramarine | 4.75 | 4.75 |
| Sky | 5.11 | 4.97 |
| Fern | 5.66 | 4.99 |
| Violet | 5.17 | 5.17 |
| Ember | 5.16 | 5.11 |
| Graphite | 7.30 | 6.93 |

The flat colourways' values live in `crates/app/src/ui/theme/flat.rs`.

### abyss

Users choose the colour, so abyss's brand is the set of rules that survive any hue.

A theme is two numbers per mode: a hue and a tint amount. Lightness is fixed by the tables below and is not user-editable, which is what keeps text readable and rule 1 working.

**The picker** (Appearance, in the app menu, on Cmd+comma and above the chat list) offers 24 hues, 15 degrees apart, and five tint amounts from 0% (grey) to 200%, separately for night and day. A choice applies as it is made. abyss's hues are saved in the `theme` file described above.

Values are `oklch(lightness chroma hue)`. Chroma is multiplied by the tint amount (default 1). Hex values are the defaults converted to sRGB; the darkest steps clip slightly.

**Defaults:** night hue 215, day hue 200.

| Step | Night | Hex | Day | Hex |
| --- | --- | --- | --- | --- |
| 1 (now) | 0.300 0.045 | `#0C333C` | 0.970 0.006 | `#F1F6F7` |
| 2 | 0.275 0.044 | `#062D35` | 0.925 0.012 | `#DEE9E9` |
| 3 | 0.250 0.042 | `#02272E` | 0.860 0.020 | `#C3D5D6` |
| 4 | 0.225 0.040 | `#002027` | 0.790 0.028 | `#A7C0C2` |
| 5 | 0.200 0.037 | `#001A20` | 0.430 0.045 | `#30575A` |
| 6 | 0.175 0.034 | `#00141A` | 0.355 0.045 | `#1B4345` |
| 7 (long ago) | 0.150 0.030 | `#000E13` | 0.280 0.040 | `#0B2F31` |

| Role | Night | Hex | Day | Hex |
| --- | --- | --- | --- | --- |
| Text and own-message fill | 0.93 0.015 | `#DDEBEE` | 0.24 0.03 | `#0B2425` |
| Text on own-message fill | 0.18 0.03 | `#01151A` | 0.97 0.006 | `#F1F6F7` |
| Incoming-message fill | white at 8% | | white at 80% | |
| Quiet text | text at 74% opacity | | text at 74% opacity | |

In day mode, steps 1 to 4 take dark text and steps 5 to 7 take light text.

**Which step a thing takes** is its age in local calendar days: today is step 1, yesterday 2, within a week 3, within 30 days 4, within 90 days 5, within a year 6, older 7.

**Contrast** (WCAG ratio). Every way text can sit on the theme is checked: on each step, inside an incoming message over each step, and inside the user's own message, at full and at quiet strength. Across all 120 combinations the picker offers, the weakest pair is 4.94 in night mode and 4.59 in day mode, against a minimum of 4.5. The tests in `theme/tests.rs` hold this for every combination and for both flat colourways, in both modes, so no choice can make text unreadable.

The workshop boards used day steps 4 to 6 at 0.760, 0.520 and 0.400, an incoming fill of 62% and quiet text at 68%. Those failed the minimum for quiet text and were tuned to the values above.

**No state depends on hue alone**, in every colourway. Avatars are initials on a faint disc of the row's text colour, sender names take the message's text colour, a link is marked by its underline, a mention by its weight, and an error by a rule and a heavier weight.

**Public ramp.** Everything outside the app (icon, website, frames, backdrops) uses hue 215 and these six tones only. In-app screenshots show the app as it is, in any colourway; the store set shows Ultramarine and one abyss shot.

| Name | oklch | Hex |
| --- | --- | --- |
| Surface | 0.93 0.015 | `#DDEBEE` |
| Shallows | 0.74 0.035 | `#93B1B9` |
| Mid water | 0.56 0.050 | `#517C87` |
| Deep | 0.40 0.050 | `#244F59` |
| Trench | 0.27 0.045 | `#032C34` |
| Floor | 0.17 0.032 | `#001318` |

What is ownable is the low saturation, not the hue. Keep the tint low so 215 never reads as Telegram blue.

Mid water is the one tone that cannot carry body text in ramp colours at 4.5:1. The landing page sets text in black on that band; prefer headings or no text there.

## Type

- **Charter:** names and the wordmark.
- **Avenir Next:** everything else. It is licensed only as part of macOS, so it cannot ship on another platform or be embedded on a website.
- **On macOS nothing is bundled.** Both faces come from the system, which keeps font files out of the app's supply chain.

**For other platforms and the web** (research by a delegated model on 2026-11-16; licences and metrics not re-checked by hand, so confirm before shipping any font file):

- **Charter:** Matthew Butterick's free Charter package, under the Bitstream Charter licence. Regular, italic, bold and bold italic only, and no Cyrillic, so names in other scripts fall back to another face.
- **Sans:** Lato (SIL Open Font License 1.1) is the recommended stand-in: regular, medium and semibold with italics, tabular figures, Latin, Cyrillic, Greek and Vietnamese. It runs about 3% narrower than Avenir Next and is a little warmer, so line heights and truncation need retuning. Source Sans 3 and Montserrat were the alternatives considered.
- **If fonts are ever bundled:** static cuts only, pinned to an upstream revision with a checksum verified in the build. This is a small but real addition to the attack surface, since the system's font parser then reads files the app supplies.
- **Fallback order with nothing bundled:** names in Charter, Bitstream Charter, Charis SIL, Georgia, Noto Serif; everything else in Avenir Next, Lato, Segoe UI, Noto Sans.

## Icon

"Well": the tile shape repeated inward, each step darker, drifting slightly downward.

- Built only from the public ramp, so it survives system tinting.
- The centre stays empty and dark. Never add a highlight; that is the "nothing looks back" part.
- **The mark** is the shape full bleed: [well.svg](../../assets/icon/well.svg). The app draws it live beside the wordmark (`crates/app/src/ui/mark.rs`): in the chosen hue and tint under abyss, and in the public ramp (hue 215, default tint) under every other colourway.
- **The app icon** is the shape on Apple's macOS icon grid (824-point body on a 1024-point canvas, continuous corners, soft shadow): `assets/icon/abyss.icns`, generated by `assets/icon/render.swift`.
- **Building the app bundle:** `scripts/bundle.sh` builds the release binary and writes `target/bundle/abyss.app` (binary, `abyss.icns`, `assets/macos/Info.plist`), then signs it ad hoc and verifies the seal. `ABYSS_BUNDLE_PRECOMPILED_SHADERS=1` builds with `--no-default-features`, as CI does. `scripts/icon.sh` regenerates `abyss.icns` from `render.swift`.
- **Development launches:** `scripts/bundle.sh --debug` writes `target/bundle/debug/abyss.app` for Finder and Dock use. Direct `cargo run` and `cargo dev` launches set the Dock icon from the same embedded `abyss.icns`.
- **Small sizes** (32 pixels and under) draw four levels instead of six. Checked by eye at 16 points on light and dark grounds: it reads as nested squares; six levels blurred together.

## Voice

- Say what happened and what abyss did. Plain words, sentence case.
- No exclamation marks. No adjectives such as "secure" or "military-grade". Numbers over adjectives.
- The app's text is always shorter and quieter than the user's.
- State uncertainty as a fact ("Not sure it was sent").
- Water vocabulary is for marketing only. Interface labels stay plain ("Chats", not "Depths").
- The link claim is "never opens links from messages". Onboarding opens one fixed URL, so the unqualified claim is false.

## Open

- **Layered icon, not yet used:** `assets/icon/abyss.icon` is an Icon Composer document that Xcode 27.2's `actool` compiles without errors, rendered and checked in the default and dark appearances only. Open it in Icon Composer to check clear and tinted before the bundle switches to it; the system adds a faint rim sheen in the default appearance that the file cannot turn off. The bundle still uses `abyss.icns`.
- **Signing:** the bundle is signed ad hoc. Developer ID signing, notarization and the App Sandbox are Stage 3.
- **A project `api_id`:** packaged releases are to embed one (plan section 17.1, amended), and none is registered yet.
- **Per-network Welcome copy:** onboarding (task 6b) is built and themed, with the brand header on the Welcome and sign-in screens. The Welcome copy is one fixed statement for Telegram (`crates/app/src/ui/copy/onboarding.rs`); the per-network threat model of plan section 17.3 waits for a second network.
- **Type off macOS:** the stand-ins above are a recommendation, not adopted. No font is bundled anywhere, so the landing page shows system faces on non-Apple devices.
- **Publishing:** `site/index.html` and the store listing are drafts. Check every claim on them against the app before publishing.
