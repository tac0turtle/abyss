# Connecting abyss to Telegram

This guide follows the screens abyss shows on first launch. The app's text
lives in `crates/app/src/ui/copy/onboarding.rs`; a unit test checks that
each step's key phrase below matches it.

## 1. Welcome

abyss is a small, auditable Telegram client for macOS. It reduces the
client-side attack surface: there is less code that handles what strangers
send you.

It does not hide your chats from Telegram. Cloud chats are still readable by
Telegram's servers. This version has no secret chats, calls, stickers,
browser or Mini Apps.

Press **Connect to Telegram**.

## 2. Get your API credentials

abyss connects with your own Telegram API credentials (`api_id` and
`api_hash`). **Open my.telegram.org** opens
<https://my.telegram.org/apps> in your browser. abyss opens no other URL
during onboarding.

0. You need an existing Telegram account signed in on an official app
   (phone or desktop), and that session must stay available. Login codes
   for my.telegram.org and for abyss arrive there.
1. Log in with your phone number. The login code arrives in your Telegram
   app, not by SMS.
2. Open "API development tools".
3. Fill App title `abyss`, a Short name of 5–32 letters or digits, and
   Platform "Desktop". Leave URL empty.
4. Copy `api_id` and `api_hash`.

If something goes wrong:

- If the form says "ERROR", retry later with VPN and ad blockers off and a
  letters-only short name.
- There is one app per phone number. If you already registered one, copy
  its existing `api_id` and `api_hash`; don't try to create a second.
- Never share your api_hash. Anyone who has it can pose as your app.
- New `api_id`s can draw extra anti-spam scrutiny from Telegram.

Press **I have my api_id and api_hash**.

## 3. Enter your API credentials

- `api_id` takes digits only, at most 10.
- `api_hash` takes exactly 32 hex characters. It is masked; **Show** reveals
  it. Pasted spaces and line breaks are dropped.

**Continue** checks both fields and takes you to the phone screen. It does
not contact Telegram yet: the pair is first used when you enter your phone
number and abyss asks for a login code. abyss saves the pair on this Mac
only once Telegram has accepted it, i.e.
after a code request with this pair succeeds. If Telegram rejects the pair,
you come back to this screen with the reason.

If saving fails, abyss says so and offers **Retry**; until a save succeeds
you will need to enter the pair again next time.

If a pair is already saved, you can use it, enter a new pair to replace it,
or forget it. Forgetting is refused while you are signed in: log out first.
If local storage refuses to delete it, abyss says the pair is still stored
and offers **Retry**; it never claims the pair is gone before it is.

### Environment override

`ABYSS_API_ID` and `ABYSS_API_HASH` override the saved pair. Both must be
set. They are never saved. While they are set, the entry
screen is disabled and abyss explains what is wrong with them; fix them, or
unset both and restart abyss.

If saved credentials cannot be read, abyss shows
**Retry** instead of asking for a new pair.

## 4. Phone

Enter your phone number in international format with a leading `+`.
Telegram sends the code to your other Telegram sessions first.

## 5. Code

The screen says where the code went (your Telegram app, SMS, a call, …) and,
when Telegram offers another way, counts down until you can ask for it. For
a flash call, enter the calling number; for a missed call, enter the last
digits of the calling number, as the screen says.

If your number is a Fragment number, abyss cannot receive its code: it
shows the Fragment address as text (it never opens it) and asks you to
finish in an official app.

Telegram and abyss will never ask for this code anywhere else.

Your other devices will show a "new login" notice. That's expected.

If Telegram asks for a step abyss does not support (for example, setting up
a login email), it says: "Finish this step in an official Telegram app, then
try again." If the number has no account, create one in an official app
first.

## 6. Password

If your account has two-step verification, enter its password. The hint you
set is shown.

## 7. Done

- The session shows as "abyss" under Settings › Devices in official apps,
  where you can end it remotely.
- If your account has no two-step verification password, abyss recommends
  turning it on in an official app. abyss doesn't set it.

## Going back and logging out

**Back** works on every screen before you are signed in, except the welcome
screen and the screens where abyss is waiting on something outside the form
(checking a lost sign-in answer, an unusable environment override, unavailable local
storage). From the phone screen, Back returns to the credential screens
unless the pair comes from the environment.

Once you are signed in, the only way back is **Log out** (top right of the
chat list). It ends the session with Telegram, then deletes this account's
session key and message cache from this Mac. The chat list, open chat and
any unsent draft are cleared as soon as the logout starts.

If Telegram cannot be reached, the logout still deletes the local data, and
abyss tells you to end the session from Settings › Devices on another
device. If the local deletion fails (for example, local storage refuses),
abyss shows **Logout incomplete** with **Retry**, and accepts no new login
until Retry succeeds.

abyss records in local storage that a logout is in progress, and does nothing
else until that record is saved. Meanwhile it shows **Logging out**: your
chats stay, but nothing new is sent. A message you write then is kept as
failed, and automatic resends (after a Telegram flood wait, say) wait.
A message already handed to Telegram before you logged out cannot be
taken back. If the record cannot be saved (for example, the
storage is unavailable), abyss shows **Couldn't start logout**: nothing was
ended or deleted, you are still signed in on this Mac, and you can **Retry**
or **Stay signed in**, which resumes the waiting resends; a message kept
as failed is sent with its **Retry**. A logout Telegram started (it ended
the session) offers only Retry.

Once the record is saved, a logout that is interrupted (abyss quits or
crashes before the deletion finishes) or whose deletion failed is finished
at the next start, before anything else: abyss never signs that account
back in. This holds only for a logout whose record was saved; a logout that
could not start leaves the account signed in, as the screen says.

If abyss cannot read that record at startup, it cannot tell whether a
logout is unfinished, so it shows **Couldn't read local storage** and waits. It
loads no session, opens no message cache (opening can replace a cache it
cannot read) and deletes nothing. **Retry** reads the record again: if there is
none, abyss starts normally with your account; if there is one, it finishes
that logout; if it still cannot read it, it keeps waiting.

## Secret storage and Keychain prompts

Keychain storage is **off by default**. Open **Settings…** from the abyss
menu (Cmd+,) and turn on **Store secrets in Keychain** to opt in. Changes
apply on the next launch. Resetting appearance does not change this setting.

By default, the session, API credentials, message-cache key and unfinished
logout record are stored in
`~/Library/Application Support/dev.abyss/private/secrets`. The directory is
0700, the file is 0600, and the directory is excluded from Time Machine
backups. These secrets are unencrypted in that file and accessible to
processes running as your macOS user. The message cache remains encrypted,
but its key is in the local file. Enable Keychain storage if you want the
secrets protected by macOS Keychain access controls.

The first launch after upgrading imports existing Keychain secrets. This
may ask for access to each existing item. Switching storage preserves the
session, cache key, API credentials and any unfinished logout. The app
commits the destination before removing the source copies; failed migration
or cleanup blocks startup and can be retried. Later launches in local mode
do not access Keychain.

With Keychain enabled, a locked login keychain or an access decision can
still cause a macOS prompt. Selecting **Always Allow** trusts that signed
app identity. Ad-hoc signed builds change identity when rebuilt, so they
can ask again. For builds with an installed signing certificate, use the
same identity across rebuilds:

```sh
ABYSS_SIGNING_IDENTITY='Apple Development: Your Name (TEAMID)' scripts/bundle.sh
```

The bundle script retains `dev.abyss` as the signing identifier. It defaults
to ad-hoc signing when the variable is unset. Keeping the certificate and
identifier stable lets Keychain recognize subsequent builds; it does not
bypass a locked keychain or a user's access decision. See Apple's
[code signing requirements](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements).
