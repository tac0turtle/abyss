# Stage 3 task 0: spike results

Status: **done except the three checks that need a person at an unlocked screen** (section 5). Evidence for [stage-3.md](stage-3.md) (draft v0.2) sections 2, 3.3, 3.4, 4.1 and task 0. The spike code is in `spikes/` and ships nothing: it is its own cargo workspace (`spikes/Cargo.toml`), so the app workspace at the repository root never builds it.

Environment: macOS 27.2.1 (build 26B131), Darwin 27.2.0, arm64, Xcode 27.2, rustc 1.98.1. Everything is ad-hoc signed (`codesign --sign -`); the machine has no Apple Developer membership. zed/gpui pinned at `917559da`, `accesskit` 0.24.1, `accesskit_macos` 0.26.3, `objc2` 0.6.5, `objc2-user-notifications` 0.3.2.

**The screen was locked for the whole session** (`CGSSessionScreenIsLocked = 1` since 16:35:38, read with `CGSessionCopyCurrentDictionary`). That blocked the notification prompt and the Dock-click reopen, and it makes GPUI treat its window as occluded. Each place it matters says so.

## Summary

| # | Question | Result |
|---|---|---|
| 1a | Ad-hoc bundle with `app-sandbox` + `network.client` launches sandboxed | **Confirmed** |
| 1b | `URLsForDirectory(ApplicationSupport)` returns the container path, also with a decoy `HOME` | **Confirmed**, by direct exec and by `open --env` |
| 1c | Add, read, update, delete a generic-password item in the login keychain from the sandbox | **Confirmed**, with the production `KeychainStore` |
| 1d | `NSWorkspace openURL` to my.telegram.org from the sandbox | **Confirmed** (returns `true`; the page itself was not inspected) |
| 1e | `network.client` is what lets TCP out | **Half confirmed**: without it a connect is denied. With it, no positive result could be shown (section 1.5) |
| 2a | `UNUserNotificationCenter` in an unbundled process | **Confirmed, and worse than the plan says**: it kills the process; the exception cannot be caught |
| 2b | `requestAuthorization` from an ad-hoc bundle completes | **Needs owner**: no completion in 60 s, screen locked. No fast failure either |
| 2c | A banner is delivered | **Needs owner** (depends on 2b) |
| 3a | `App::hide`, then reopen, brings the window back intact | **Partly confirmed**: hide and in-process `activate` do; Dock-click reopen **needs owner** |
| 3b | `debug_a11y_tree_json` exposes labelled nodes | **Confirmed in a real window. Refuted in the test platform**: it is always `None` there |
| 3c | A custom text input can expose text and selection to AccessKit | **Confirmed**, with two conditions the abyss inputs do not meet today |

## 1. Sandbox

### 1.1 Launch

```
spikes/bundle.sh        # cargo build, then three ad-hoc bundles under spikes/build/
codesign -d --entitlements - spikes/build/Spike-sandboxed.app
```
```
[Key] com.apple.security.app-sandbox          [Bool] true
[Key] com.apple.security.network.client       [Bool] true
Identifier=dev.abyss.spike   flags=0x2(adhoc)   Signature=adhoc   TeamIdentifier=not set
```

`spikes/build/Spike-sandboxed.app/Contents/MacOS/abyss-spike sandbox dev.abyss.spike` (the same executable is also signed without entitlements as `Spike-plain.app`, id `dev.abyss.spike.plain`, as the control):

| | sandboxed bundle | plain ad-hoc bundle |
|---|---|---|
| `sandbox_check(getpid(), NULL, 0)` | `1` | `0` |
| `APP_SANDBOX_CONTAINER_ID` | `dev.abyss.spike` | unset |
| `NSHomeDirectory()` | `~/Library/Containers/dev.abyss.spike/Data` | `/Users/markobaricevic` |
| `NSTemporaryDirectory()` | `.../Containers/dev.abyss.spike/Data/tmp/` | `/var/folders/...` |
| `getpwuid_r` `pw_dir` (what `storage.rs` uses today) | `/Users/markobaricevic` (the real home) | the real home |
| write under `<pw_dir>/Library/Application Support/` | **`EPERM` (Operation not permitted)** | ok |

The container directory was created on first launch (`~/Library/Containers/dev.abyss.spike`).

**Implication.** The sandbox does apply to an ad-hoc bundle, so tracks A and B do not move behind D1. The current `storage.rs` root (built from `getpwuid_r`) would fail with `EPERM` in the bundle. That confirms plan section 2.2: the root must come from Foundation.

### 1.2 The Application Support root, and `HOME`

`NSFileManager URLsForDirectory:NSApplicationSupportDirectory inDomains:NSUserDomainMask`:

| launch | `HOME` seen by the process | `URLsForDirectory` |
|---|---|---|
| sandboxed, direct exec, real `HOME` | the container `Data` dir | `~/Library/Containers/dev.abyss.spike/Data/Library/Application Support` |
| sandboxed, direct exec, `HOME=/tmp/abyss-decoy-home CFFIXED_USER_HOME=/tmp/abyss-decoy-home` | **the container `Data` dir** (the sandbox overwrites both) | the container path |
| sandboxed, `open -n -W --env HOME=/tmp/abyss-decoy-home --stdout ... Spike-sandboxed.app --args sandbox` | the container `Data` dir | the container path |
| plain, `open --env HOME=/tmp/abyss-decoy-home` (shows `--env` does reach a process) | `/tmp/abyss-decoy-home` | `/Users/markobaricevic/Library/Application Support` (decoy ignored) |

Writing, reading and deleting a file under the returned container path worked. Both launch routes gave identical results. `open` was run with `--stdout`/`--stderr` files; the bundle also logs to `<container tmp>/abyss-spike.log`.

**Implication.** Plan section 2.2 holds: `HOME` cannot redirect storage, in or out of the sandbox. One caveat, not in the plan: Foundation ignores `HOME` but **Security.framework does not**. An unsandboxed process started with a decoy `HOME` hit a `SecurityAgent` dialog on its first keychain add and hung until killed (`HOME=/tmp/abyss-decoy-home`, plain bundle, direct exec: `get` returned `None`, then `set` blocked; reproduced under `open` as well). Sandboxed, the same decoy changed nothing: the sandbox had already rewritten `HOME` to the container (1.2), and the keychain calls succeeded. So any test or dev run that sets a decoy `HOME` while using the Keychain must run sandboxed, or must not touch the Keychain.

### 1.3 Keychain

The spike uses the production `abyss_core::secret::KeychainStore` (path dependency on `crates/core`, unmodified), `KeychainLocation::File`, namespace `dev.abyss.spike`, item `Session`:

```
precondition delete: Ok(())        get (absent): Ok(None)
add (set #1): Ok(())               read back matches #1: Ok(Some(true))
update (set #2): Ok(())            read back matches #2: Ok(Some(true))
has_items: Ok(true)                delete: Ok(())      get after delete: Ok(None)
```

Same output sandboxed and unsandboxed; no prompt appeared. The sandboxed item is in the **user's login keychain**, not a container-private one: after `abyss-spike kc-set dev.abyss.spike` from the sandboxed bundle, `security find-generic-password -s dev.abyss.spike.session` from a normal shell printed `keychain: "/Users/markobaricevic/Library/Keychains/login.keychain-db"`, class `genp`, account `abyss`, service `dev.abyss.spike.session`. `kc-delete` from the sandboxed bundle then removed it (`security` found nothing).

**Implication.** Plan section 2.3 is confirmed. A consequence the plan should state: the sandboxed bundle and `cargo dev` share Keychain items (same service names), so the two-regime risk (section 8) is real, as the plan already says. The ad-hoc-signature ACL prompt on rebuild (S1 §14, risk 3) was not exercised: every item here was created and deleted by one build.

### 1.4 `NSWorkspace openURL`

`NSWorkspace.sharedWorkspace.openURL(https://my.telegram.org/apps)` returned `true` from the sandboxed bundle, by direct exec and under `open`. It opens a browser tab; I did not look at it or close it.

### 1.5 `network.client`

Not asked for, but the plan's section 2.1 depends on it. A TCP connect to a Telegram DC2 address (`149.154.167.50:443`):

- sandbox **without** `network.client` (`Spike-sandboxed-nonet.app`, entitlement `app-sandbox` only): `Err(PermissionDenied, "Operation not permitted")`, immediately.
- sandbox **with** `network.client`, and the plain ad-hoc control: the connect **hung** (no result in 30 s). A connect to `127.0.0.1:9` from the same binary returned `ConnectionRefused` at once, and `nc -z 149.154.167.50 443` and `nc -z 1.1.1.1 443` from the shell succeeded. So something outside the sandbox (the Application Firewall is on, and `nehelper` / NetworkExtension processes are running; I did not find which) stalls outbound connects from my new ad-hoc binaries. It affects the unsandboxed control equally, so it is not the sandbox.

So: the entitlement is the gate (denied without it), but I could **not** show a successful MTProto-style connection from the sandbox on this machine. Task 1 should check a real connect once, on a machine without this stall.

### Plan amendments for section 1

- Section 2.2: add "`getpwuid_r` is no longer used for the root; in the sandbox it still returns the real home, which is denied (`EPERM`, observed)". `storage.rs`'s current root must be replaced, not wrapped.
- Section 2.2 / 7: add the `HOME` caveat above: Keychain-touching runs with a decoy `HOME` block on a system dialog unless sandboxed.
- Section 2.3: state that sandboxed Keychain items land in the user's login keychain (observed), and share service names with unsandboxed runs.
- Section 7 (sandbox): `codesign -d --entitlements -` output format is as in 1.1, so the "lists exactly the two entitlements" check can parse it.
- Section 8: add "outbound connect from a new ad-hoc binary may stall behind a firewall or network filter on a developer machine (seen here, cause unknown); it is not a sandbox symptom".

## 2. Notifications

```
spikes/bundle.sh
# bare, unbundled executable:
spikes/target/debug/abyss-spike notify 5
# sandboxed ad-hoc bundle dev.abyss.spike, then the plain control dev.abyss.spike.plain:
open -n -W --stdout out.txt --stderr err.txt spikes/build/Spike-sandboxed.app --args notify 60
open -n -W --stdout out.txt --stderr err.txt spikes/build/Spike-plain.app --args notify 60
```

### 2.1 Unbundled process: confirmed, and fatal

`currentNotificationCenter` is wrapped in `objc2::exception::catch` in the spike. The bare executable still died:

```
bundle identifier = None
bundle path = .../spikes/target/debug
*** Terminating app due to uncaught exception 'NSInternalInconsistencyException',
    reason: 'bundleProxyForCurrentProcess is nil: mainBundle.bundleURL file:///.../spikes/target/debug/'
...  UserNotifications  __53+[UNUserNotificationCenter currentNotificationCenter]_block_invoke ...
     libdispatch  _dispatch_once_callout ...
exit=134
```

The exception is raised inside a `dispatch_once` block, and the catch frame above it never saw it: the process aborted with `SIGABRT` (exit 134), and the code after the `catch` never ran. This differs from the plan's wording ("throws").

**Implication.** The bundle guard in section 3.3 is the only defence, and a mistake in it kills the app. It cannot be made safe by catching. Amend section 3.3: "the guard is mandatory and must run before the first reference to `UNUserNotificationCenter`; the exception cannot be caught (observed: abort, exit 134), so there is no fallback". The guard's two inputs are as observed: `NSBundle.mainBundle.bundleIdentifier` is `nil` and `bundlePath` is the executable's directory for the bare binary. Add a test that runs the bare test binary through the guard.

### 2.2 Authorization from the ad-hoc bundle: needs the owner

For both `dev.abyss.spike` (sandboxed) and `dev.abyss.spike.plain`:

```
currentNotificationCenter returned a center
requestAuthorization(alert|badge|sound) called; waiting for the completion handler
settings (before request): authorizationStatus=0 (notDetermined), alert=0, badge=0, notificationCenter=2
no further outcome after 60 s; exiting
```

- `currentNotificationCenter` works in the ad-hoc bundle, sandboxed or not.
- `requestAuthorizationWithOptions` did **not complete in 60 s**, so I observed neither a grant, a refusal, nor an error. The screen was locked, so nobody could click the prompt; I did not try to click it by script.
- No fast failure: an ad-hoc signature did not make the call return an error straight away. That is weak evidence that ad-hoc is acceptable (a signature problem would more likely fail fast), not proof.
- A `UNUserNotificationCenterDelegate` written in Rust (`define_class!`, `willPresentNotification`) compiles and links against the pinned `objc2-user-notifications`. It was never invoked, so this is build evidence only.
- `getNotificationSettings`, `addNotificationRequest`, `getDeliveredNotifications` and `removeAllDeliveredNotifications` are wired in the spike but only the settings call ran.

**Needs owner.** Steps in section 5.1. Until then, the plan's task 0 bullet "a delivered banner from the ad-hoc bundle" is unproven and the plan's risk "if ad-hoc is not enough, tracks A and B move behind D1" stands open.

### Plan amendments for section 2

- Section 3.3: the guard wording above.
- Section 6, task 0: mark the notification bullet "authorization and banner: owner runs `spikes/notify.sh` (section 5.1 of the spike notes)"; task 3 must not start its delivery work until that is recorded.
- Section 3.3, authorization: add that the completion handler may not return until the person answers, so the request must not block any thread or startup path (observed: 60 s without a result).

## 3. GPUI

Spike: `spikes/gpui-spike` (a window with labelled elements and two custom text elements), bundled as `dev.abyss.spike.gpui`, driven by command files (`spikes/cmd.sh`) and observed from outside with the window server (`spikes/winlist.swift`, `CGWindowListCopyWindowInfo`). Scenario script: `spikes/hide-reopen.sh`.

### 3.1 (a) `App::hide`, `on_reopen`, activation

Code, at `917559da`:

- `App::hide` is `NSApplication.hide:` (`gpui_macos/src/platform.rs:697`).
- `on_reopen` is called from `applicationShouldHandleReopen:hasVisibleWindows:` **only when `has_open_windows` is false** (`platform.rs:1421`); the method returns `YES`, so AppKit still does its default handling.
- GPUI stops its display link when `NSWindow.occlusionState` has no `Visible` (hide counts, as does display sleep: `window.rs:3171`), and the `windowDidBecomeKey` handler forces one synchronous frame on later activations (`window.rs`, near line 3339).

Observed (`spikes/hide-reopen.sh`, screen locked):

| step | result |
|---|---|
| launch | `NSApp.isHidden=false`, 1 visible NSWindow, window server `onscreen=1`, bounds `640,347 520x492` |
| `App::hide` | `isHidden=true`, visible NSWindows `0`, window server `onscreen=false`; **same window number, same bounds** |
| 3 s hidden | still hidden; the tick counter kept running (ticks 11 to 19), state is held in memory |
| `open Spike-gpui.app` (Dock-click equivalent: sends the reopen Apple event) | **no change**: still hidden, `on_reopen` callback count `0`. Same for `open -b <id>` |
| `App::hide`, then in-process `cx.activate(true)` | **brought it back**: `isHidden=false`, 1 visible NSWindow, `onscreen=1`, same bounds, same state |

What this does not show. With the screen locked, LaunchServices may not deliver or act on a reopen event, so the `open` row is **inconclusive, not a refutation**. The window was also occluded the whole time, so GPUI never painted after the first frames (`renders`/`paints` stayed at 2 to 3), which means "renders correctly after reopen" is untested. Sleep and wake were not tested (it would sleep the machine).

**Implications for section 3.4.**
1. `activate` un-hides the app in the same call (observed), so the notification-click path can rely on `cx.activate(true)`.
2. `on_reopen` is documented by the code to fire only when there is no visible window. A hidden-but-open window may count as visible or not (not determined here), so do **not** build the Dock-click path on `on_reopen`. Rely on AppKit's default reopen handling (returns `YES`), and keep `on_reopen` only as a fallback that activates and opens a window if none exists. This needs the owner's check (section 5.2).
3. Because GPUI stops painting for a hidden window, a hidden but connected app costs no frames. Reopening should show the state as it was; the owner check includes seeing one correct frame.

### 3.2 (b) `Window::debug_a11y_tree_json`

The a11y tree is **only built once an assistive client has asked for it** (`a11y_active_flag`, set by the AccessKit adapter's activation callback, `window.rs:1757`). With no client, `debug_a11y_tree_json()` is `None`. So the spike plays the client itself: `ax` calls `[contentView accessibilityChildren]` (the first call of any AX client wakes the adapter), `frame` forces `Window::draw` (needed here because the occluded window would not draw), then `axdump` walks the NSAccessibility tree with the selectors VoiceOver uses and calls `debug_a11y_tree_json`.

Observed in the real window (`spikes/cmd.sh <dir> ax; frame; axdump`):

```
AXGroup/AXApplicationGroup  title "Spike root"
  Heading          title "Spike heading"
  AXStaticText     title "ticks 8 renders 3 sent 0"
  AXButton         title "Send message"
  AXCheckBox/AXSwitch  title "Show message text in banners"  value "0"
  AXList           title "Chats"
    AXGroup        title "Ada, 2 unread"
    AXGroup        title "Grace, muted"
  AXTextField      title "Plain div input"   value "plain value"
  AXTextField      title "Custom field"      value "héllo wörld"
  AXTextField/AXSecureTextField  title "Spike password"  value "••••••••••"
debug_a11y_tree_json: Some(4676 bytes)    (node_count 14, per-node label, role, element id, source location)
```

Labels come out as `AXTitle`; `accessibilityLabel` is nil. `accessibilityPerformPress` on the button returned `true` and the button's handler ran (`sent` 0 to 1), so the action path from an AX client to a GPUI handler works.

Two things to check by ear (section 5.3): `accesskit_macos` 0.26.3 maps `Role::Heading` to the string `"Heading"` (`node.rs:125`), not `"AXHeading"`. I observed `"Heading"` as the role; whether VoiceOver announces it as a heading is not known. And a list row is `AXGroup`, not `AXRow`.

**Refuted for tests.** In GPUI's test platform the tree is never built. A `#[gpui::test]` that opens a window with the demo view, runs until parked, draws once, then reads the tree gave:

```
test platform: is_a11y_enabled=true is_a11y_active=false debug_a11y_tree_json=None
```

The test platform's `a11y_init` is the trait's empty default (`platform.rs:1484`), so nothing can set the active flag, and `Window` offers no public way to. Plan section 7's "tree tests on `debug_a11y_tree_json` for every scene in the snapshot set" cannot work as written.

**Plan amendments (4.1, 7, 8):**
- Split the accessibility tests in three. (1) Labels and roles are built by pure functions (`chat_row_label(...)`, `message_label(...)`) that are unit-tested; the plan's inventory text becomes their spec. (2) A real-window harness, like this spike (wake the adapter, `Window::draw`, read the tree), runs as an `#[ignore]` test or a CI step on a macOS runner with a GUI session. (3) Ask upstream for a test-platform activation hook, or carry a patch in its own GPUI-bump commit (section 8, "GPUI pin").
- Section 8: add "the a11y tree is lazy; nothing builds it without a client, so a missing tree in a test proves nothing".

### 3.3 (c) A custom text input exposing text and selection

Reading the code (`gpui/src/_accessibility.rs` "Synthetic children", `element.rs:132`): a custom `Element` can return a role and an element id, write node info, and add synthetic `Role::TextRun` children and a `TextSelection` on its own node through `A11ySubtreeBuilder`.

Observed in the spike (`spikes/gpui-spike/src/field.rs`, an element shaped like `input.rs`'s `TextElement`; text `"héllo wörld"`, selection characters 6 to 11), read through the NSAccessibility text attributes:

| element | `AXValue` | `AXNumberOfCharacters` | `AXSelectedTextRange` | `AXSelectedText` |
|---|---|---|---|---|
| custom element, `Role::TextInput`, one `TextRun` child + `set_text_selection` | `"héllo wörld"` | **11** | **(6, 5)** | **`"wörld"`** |
| plain `div` with `.role(TextInput).aria_value(..)` | `"plain value"` | 0 | (0, 0) | none |
| custom element, `Role::PasswordInput`, `TextRun` of bullets | `"••••••••••"`, subrole **`AXSecureTextField`** | 10 | (6, 4) | `"••••"` |

So the plan's section 4.1 fallback ("a labelled `TextInput` whose value is set from the buffer") is **not needed** for the single-line case: the real text model works, including UTF-16 ranges over a multibyte string (the `é`, `ö`). The `div`-only route gives a value but no caret and no selection, which is the fallback the plan describes. Secret fields: with `PasswordInput` and a `TextRun` of bullets, nothing in the tree carries the plaintext, so invariant 6 holds.

**Conditions the abyss inputs do not meet today.**
- `TextElement::id()` (`crates/app/src/ui/text_input/element.rs:231`) and `PasswordElement::id()` (`ui/password.rs:547`) return `None`. GPUI documents that an element without an id has no node (`window/a11y.rs` module doc), and the spike only worked once the element returned an id. I did not run abyss's own element; this is from reading its code and the documented rule, plus the spike's element having the same shape. Both need `id`, `a11y_role`, `write_a11y_info` and `a11y_synthetic_children`.
- The spike sets character lengths and word starts but **not** run bounds or character positions. VoiceOver can read, but cursor highlighting and `AXFrameForRange` need geometry. Not tested.
- The multi-line composer needs one `TextRun` per visual line (AccessKit's text model is line-based); not tested.
- Writing back (`setAccessibilityValue`, `setAccessibilitySelectedTextRange` from VoiceOver or Voice Control) needs `SetValue` and `SetTextSelection` handlers on the node; not tested.

**Plan amendments (4.1, 6 task 4/5):** replace "if the last point fails, fall back" with "the inputs get an id, a role, and synthetic `TextRun` children; the spike shows this exposes text, selection and the secure subrole. The remaining work is line runs for the composer, run geometry, and write actions, and is sized in task 4/5". Add the two `id() -> None` sites to task 4.

## 4. Cleanup

- No spike process is running (`pgrep -fl 'abyss-(gpui-)?spike'` empty). I stopped the GPUI spike with its `quit` command and killed the hung processes noted above.
- Keychain: `security find-generic-password -s <ns>.session` finds nothing for `dev.abyss.spike`, `dev.abyss.spike.plain`, `dev.abyss.spike.nonet`.
- Removed my decoy `HOME` directory and the probe directories under `~/Library/Application Support`.
- **Left in place, as asked: `~/Library/Containers/dev.abyss.spike` and `~/Library/Containers/dev.abyss.spike.nonet`** (created by the sandboxed spike bundles). `~/Library/Containers/dev.abyss` also exists; it is not from these spikes (the task 1 agent's sandboxed app), and I did not touch it.
- Left in place and not mine to remove: the `dev.abyss.spike` and `dev.abyss.spike.plain` registrations with LaunchServices and the notification service (both are `notDetermined`). They may appear in System Settings, Notifications. Deleting the two `Spike-*.app` bundles should drop them; I did not verify that.
- No `SecurityAgent` process is left from the killed hung runs (`pgrep` empty at the end).

## 5. Manual steps for the owner

All steps need the Mac unlocked. Build once: `spikes/bundle.sh`.

### 5.1 Notification authorization and a banner (question 2)

1. `spikes/notify.sh sandboxed 90`. A prompt "abyss spike would like to send you notifications" appears; click **Allow**.
2. Expected output: `requestAuthorization completed: granted=true, error=none`, then `settings (after request): authorizationStatus=2`, `addNotificationRequest completed: error=none`, `delegate: willPresentNotification called` (only if the spike is frontmost) or a banner "abyss spike / New message" on screen, and `delivered notifications in Notification Center: 1`, then `delivered after removal: 0`.
3. Record any `granted=false` or `error=domain=... code=...` line verbatim. Then run `spikes/notify.sh plain 90` (unsandboxed control) and compare.
4. If the prompt never appears: System Settings, Notifications, look for "Spike-sandboxed" and report whether it is listed.

### 5.2 Hide and reopen (question 3a)

1. `spikes/hide-reopen.sh "$(mktemp -d)"`.
2. Expected, if the plan holds: after `hide` the window disappears; after `open Spike-gpui.app` (section "A") `NSApp.isHidden=false`, one visible NSWindow, and `paints=` grows again.
3. By hand, as a person: launch `open -n spikes/build/Spike-gpui.app --args --cmd-dir "$(mktemp -d)"`, press **Cmd+H**, then (a) click the Dock icon, (b) repeat and use **Cmd+Tab**. Each time check that the window is back, the `ticks` text kept counting, and nothing is blank. Read `log.txt` in the cmd dir: did `on_reopen callback fired` appear for (a)?
4. Optional, if you are willing to sleep the Mac: hide, sleep, wake, reopen, and say whether the window draws.

### 5.3 VoiceOver (question 3b and 3c, section 4.1)

1. `open -n spikes/build/Spike-gpui.app --args --cmd-dir "$(mktemp -d)"`. Turn on VoiceOver (**Cmd+F5**).
2. Move with **VO+Right** through the window. For each stop write down what VoiceOver says. Expected: "Spike heading" (does it say *heading*?), the tick line, "Send message, button", "Show message text in banners, switch, off", "Chats, list" with its two rows, "Plain div input, text field, plain value", "Custom field, text field, héllo wörld", "Spike password, secure text field".
3. In "Custom field" press **VO+Shift+Down** to interact, then arrow Left/Right: does it speak characters, and does it announce the selected "wörld"? In the secure field, does it speak bullets and never letters?
4. On "Send message" press **VO+Space**: the tick line's `sent` count should rise.
5. Report any role VoiceOver calls "group" or "unknown" (the heading is the suspect, section 3.2).

## 6. Proposed amendments to stage-3.md

1. **Section 2.2.** The root must not come from `getpwuid_r` (denied in the sandbox, observed); use `NSFileManager`. Add the Security-framework `HOME` caveat (1.2).
2. **Section 2.3.** Sandboxed items are in the user's login keychain and shared with unsandboxed runs (1.3).
3. **Section 3.3.** The bundle guard is mandatory: `currentNotificationCenter` outside a bundle aborts the process and cannot be caught (2.1). Add a test for it. The authorization completion can wait on the person, so never block on it.
4. **Section 3.4.** Do not build the Dock-click path on `on_reopen`; it fires only with no visible window. Use `cx.activate(true)` for notification clicks (observed to unhide) and AppKit's default reopen handling, keeping `on_reopen` as a fallback. Owner verifies a Dock click and Cmd+Tab (5.2). Sleep and wake stay untested.
5. **Section 4.1.** The fallback for text inputs is not needed for single-line fields: a custom element with an id, a role and a `TextRun` exposes text, selection and the secure subrole (3.3). Replace the fallback with the remaining work: ids on `TextElement` and `PasswordElement` (`id() -> None` today), per-line runs for the composer, run geometry, write actions.
6. **Section 7, accessibility tests.** Tree tests on `debug_a11y_tree_json` cannot run on the GPUI test platform (always `None`, 3.2). Replace with: pure label functions, a real-window harness (`#[ignore]`, GUI session), and an upstream or patched activation hook.
7. **Section 4.2.** Add "check that `Role::Heading` is announced as a heading": the adapter emits the string `"Heading"` instead of `"AXHeading"` (3.2).
8. **Section 6, task 0.** Mark task 0 done for the sandbox and GPUI parts, and open for three owner checks: notification authorization and banner (5.1), Dock-click reopen (5.2), VoiceOver (5.3). Record the results here before task 3 and task 5 start.
9. **Section 8.** Add the outbound-connect stall on a developer machine (1.5) and the lazy a11y tree.

## 7. What is in `spikes/`

| path | purpose |
|---|---|
| `Cargo.toml`, `Cargo.lock` | own workspace; lock copied from the root so versions match the app |
| `app-spike/` | sandbox, keychain (production `KeychainStore`), `NSWorkspace`, TCP and notification spikes, one binary |
| `gpui-spike/` | GPUI hide/reopen, the in-process AX client, the custom text elements, one headless test |
| `bundle.sh` | builds four ad-hoc bundles in `spikes/build/` (sandboxed, sandboxed without network, plain, gpui) |
| `cmd.sh`, `hide-reopen.sh`, `notify.sh`, `winlist.swift` | drivers; `hide-reopen.sh` and `notify.sh` are for the owner |
| `assets/` | `Info.plist.in` and the two entitlement files |
