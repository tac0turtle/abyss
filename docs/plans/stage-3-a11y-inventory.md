# Stage 3 accessibility inventory

The source locations are under `crates/app/src`. A repeated row describes each instance of the same control or data item. Labels in quotes are literal or begin with the named value. Labels mentioning copy use the existing screen text.

**Keyboard notation.** Read means VoiceOver navigation by item, text, or heading. Tab runs in visual order, Shift+Tab reverses it, and Return or Space activates the focused control. Return in a single-line field submits its form. For tasks 5 and 6, the path column describes the target and calls out existing shortcuts where they already apply.

**Activation.** VoiceOver's press (VO+Space, `AXPress`) reaches a control through the AccessKit Click action. GPUI calls a registered Click listener directly, but a control with only `on_click` gets a synthetic mouse click at its centre, hit-tested against hitboxes clipped by the scroll container: scrolled out of view, the press does nothing or presses what is drawn there (review N58). So every control built in this crate attaches its activation with `a11y::on_activate` or `a11y::on_activate_in`, which register mouse, keyboard and Click together, and a unit test (`ui::a11y::tests::on_click_only_in_on_activate`) fails on any other `.on_click(` in non-test code. The native scenes in `shell/tests/native_a11y/press.rs` press `Reset colours` and `Done` below the Settings card's viewport and a folder tab below the tab strip, and assert the intended action and nothing else.

## Task 4 · Startup, onboarding and sign-in

| Screen | Element | File and line | Role | Label or spoken value | Keyboard path |
| --- | --- | --- | --- | --- | --- |
| Startup failure | Launch or core start title | `shell/view.rs:177` | Heading 1 | Launch or startup failure title | Read |
| Startup failure | Explanation, per paragraph | `shell/view.rs:185` | Alert | Startup error and any recovery instruction | Automatic on arrival, Read |
| Connecting | Connecting status | `shell/view.rs:476` | Heading 1 | Connecting… | Read |
| Auth screens | Screen container | `shell/view.rs:253` | Group | Telegram sign-in | Starting focus on screens without a field, then Tab to actions |
| Auth screens with branding | Tagline | `shell/view.rs:625` | Label | Current brand tagline | Read |
| Welcome | Title | `shell/view/onboarding.rs:36` | Heading 1 | Welcome to abyss | Read |
| Welcome | Unofficial-client disclosure | `shell/view/onboarding.rs:48` | Label | abyss is an unofficial Telegram client. It uses the Telegram API. | Read |
| Welcome | Client description | `ui/copy/onboarding.rs:20` | Label | Small, auditable Telegram client for macOS | Read |
| Welcome | Design scope | `ui/copy/onboarding.rs:21` | Label | Client-side attack surface statement | Read |
| Welcome | Cloud chat disclosure | `ui/copy/onboarding.rs:23` | Label | Cloud chats are still readable by Telegram's servers | Read |
| Welcome | Unsupported features | `ui/copy/onboarding.rs:24` | Label | Features unavailable in this version | Read |
| Welcome | Connect | `shell/view/onboarding.rs:38` | Button | Connect to Telegram | Tab, Return or Space |
| Guide | Title | `shell/view/onboarding.rs:56` | Heading 1 | Get your API credentials | Read |
| Guide | Intro | `shell/view/onboarding.rs:72` | Label | Why abyss needs Telegram API credentials | Read |
| Guide | Open Telegram developer site | `shell/view/onboarding.rs:57` | Link | Open my.telegram.org | First Tab, Return or Space, or VoiceOver activation |
| Guide | Setup instructions | `shell/view/onboarding.rs:85` | List | Credential setup steps | Read |
| Guide | Step 0 | `ui/copy/onboarding.rs:34` | List item | Existing account signed in on an official app | Read |
| Guide | Step 1 | `ui/copy/onboarding.rs:37` | List item | Log in using the code from the Telegram app | Read |
| Guide | Step 2 | `ui/copy/onboarding.rs:39` | List item | Open API development tools | Read |
| Guide | Step 3 | `ui/copy/onboarding.rs:40` | List item | Set the application title, short name and platform | Read |
| Guide | Step 4 | `ui/copy/onboarding.rs:42` | List item | Copy api_id and api_hash | Read |
| Guide | Troubleshooting heading | `shell/view/onboarding.rs:76` | Heading 2 | If something goes wrong | Read |
| Guide | Form error advice | `ui/copy/onboarding.rs:46` | Label | Retry advice for Telegram form errors | Read |
| Guide | Existing app advice | `ui/copy/onboarding.rs:48` | Label | Use existing credentials if you already registered an app | Read |
| Guide | Credential warning | `ui/copy/onboarding.rs:50` | Label | Never share your api_hash | Read |
| Guide | Anti-spam note | `ui/copy/onboarding.rs:51` | Label | Telegram anti-spam scrutiny of new API IDs | Read |
| Guide | Back | `shell/view/onboarding.rs:81` | Button | Back | Tab after the link, Return or Space |
| Guide | Advance | `shell/view/onboarding.rs:64` | Button | I have my api_id and api_hash | Tab after Back, Return or Space |
| API credential entry | Title | `shell/view/onboarding.rs:101` | Heading 1 | Enter your API credentials | Read |
| API credential entry | Instructions | `shell/view/onboarding.rs:138` | Label | Where to get the credentials and when they are saved | Read |
| API credential entry | API ID | `shell/view/onboarding.rs:139` | Text input | API ID | Initial focus, Return submits both fields |
| API credential entry | API hash | `shell/view/onboarding.rs:123` | Secure text input | API hash. Bullets only | Tab, Return submits both fields |
| API credential entry | Show or hide hash | `shell/view/onboarding.rs:114` | Toggle button | Show API hash on screen, on or off | Tab after hash, Return or Space |
| API credential entry | Inline validation or rejection | `shell/view/onboarding.rs:141` | Alert | Local validation or credential error copy | Automatic on appearance, Read |
| API credential entry | Stored pair note | `shell/view/onboarding.rs:167` | Label | A pair is saved on this Mac | Read |
| API credential entry | Use saved pair | `shell/view/onboarding.rs:153` | Button | Use saved pair | Tab when available, Return or Space |
| API credential entry | Forget saved pair | `shell/view/onboarding.rs:158` | Button | Forget saved pair | Tab when available, Return or Space |
| API credential entry | Back | `shell/view/onboarding.rs:143` | Button | Back | Tab when available, Return or Space |
| API credential entry | Submit | `shell/view/onboarding.rs:132` | Button | Continue | Tab, Return or Space |
| Environment override error | Title | `shell/view/onboarding.rs:173` | Heading 1 | Credentials from the environment | Read |
| Environment override error | Problem | `shell/view/onboarding.rs:182` | Alert | Environment variable error | Automatic on arrival, Read |
| Environment override error | Recovery instructions | `shell/view/onboarding.rs:187` | Label | Fix or unset the override and restart | Read |
| Saved credentials unavailable | Title | `shell/view/onboarding.rs:193` | Heading 1 | Local storage unavailable | Read |
| Saved credentials unavailable | Explanation | `shell/view/onboarding.rs:202` | Alert | Saved API credentials could not be read | Automatic on arrival, Read |
| Saved credentials unavailable | Retry | `shell/view/onboarding.rs:195` | Button | Retry | Tab, Return or Space |
| Credential storage banner | Save/delete failure | `shell/view.rs:308` | Alert | Credential item status | Automatic on appearance, Read |
| Credential storage banner | Retry storage operation | `shell/view.rs:312` | Button | Retry | Tab when offered during sign-in, Return or Space |
| Phone | Title | `shell/view.rs:489` | Heading 1 | Sign in to Telegram | Read |
| Phone | Instructions | `shell/view.rs:490` | Label | Phone number formatting and code delivery instructions | Read |
| Phone | Phone number | `shell/view.rs:491` | Text input | Phone number | Initial focus, type, Return submits |
| Phone | Back | `shell/view.rs:482` | Button | Back | Tab when permitted by the credential source, Return or Space |
| Phone | Submit | `shell/view.rs:494` | Button | Continue | Tab, Return or Space |
| Code | Title | `shell/view/onboarding.rs:229` | Heading 1 | Enter the code | Read |
| Code | Delivery instructions | `shell/view/onboarding.rs:230` | Label | Delivery method, masked number and code length when known | Read |
| Code | Login code | `shell/view/onboarding.rs:235` | Secure text input (bullets only; review A3) | Login code | Initial focus, type, Return submits |
| Code | Sharing warning | `shell/view/onboarding.rs:218` | Label | Keep the login code private | Read |
| Code | New login notice | `shell/view/onboarding.rs:222` | Label | Telegram reports new logins | Read |
| Code | Resend countdown or available delivery | `shell/view/onboarding.rs:256` | Label | Next delivery method and time left | Read |
| Code | Request another code | `shell/view/onboarding.rs:260` | Button | Send the code another way | Tab once available, Return or Space |
| Code | Back | `shell/view/onboarding.rs:208` | Button | Back | Tab, Return or Space |
| Code | Submit | `shell/view/onboarding.rs:238` | Button | Continue | Tab, Return or Space |
| Two-step verification | Title | `shell/view.rs:508` | Heading 1 | Two-step verification | Read |
| Two-step verification | Instructions and hint | `shell/view.rs:509` | Label | Password prompt and Telegram hint if supplied | Read |
| Two-step verification | Password | `shell/view.rs:510` | Secure text input | Two-step verification password. Bullets only | Initial focus, type, Return submits |
| Two-step verification | Back | `shell/view.rs:500` | Button | Back | Tab, Return or Space |
| Two-step verification | Submit | `shell/view.rs:513` | Button | Continue | Tab, Return or Space |
| Phone, code, password | Inline sign-in error, per screen | `shell/view.rs:661` | Alert | Sign-in error copy | Automatic on appearance, Read |
| Phone, code, password | Unofficial-client disclosure, per screen | `shell/view.rs:681` | Label | abyss is an unofficial Telegram client. It uses the Telegram API. | Read |
| Flood wait | Title | `shell/view.rs:688` | Heading 1 | Too many attempts | Read |
| Flood wait | Countdown | `shell/view.rs:701` | Label | Time to retry | Read |
| Flood wait | Back | `shell/view.rs:703` | Button | Back | Tab if allowed by the resume state, Return or Space |
| Verifying | Title | `shell/view/onboarding.rs:269` | Heading 1 | Checking your sign-in | Read |
| Verifying | Explanation | `shell/view/onboarding.rs:273` | Label | Why abyss is waiting before another attempt | Read |
| Verifying | Retry error | `shell/view/onboarding.rs:274` | Alert | Last verification error | Automatic on appearance, Read |
| Sign-up required | Title | `shell/view.rs:447` | Heading 1 | No Telegram account | Read |
| Sign-up required | Explanation | `shell/view.rs:448` | Label | Create an account in an official Telegram app | Read |
| Sign-up required | Back | `shell/view/onboarding.rs:280` | Button | Back | Tab, Return or Space |
| Official app required | Title | `shell/view.rs:455` | Heading 1 | Finish in an official app | Read |
| Official app required | Explanation | `shell/view.rs:456` | Label | Finish the step in an official app | Read |
| Official app required | Fragment URL, when provided | `shell/view/onboarding.rs:292` | Label | Sanitized Fragment address | Read |
| Official app required | Back | `shell/view/onboarding.rs:280` | Button | Back | Tab, Return or Space |
| Done / unsaved | Title | `shell/view/onboarding.rs:306` | Heading 1 | You're signed in, or Signed in, but not saved | Read |
| Done / unsaved | Session save failure | `shell/view/onboarding.rs:316` | Alert | Session could not be saved | Automatic on arrival, Read |
| Done / unsaved | Device/session tip | `shell/view/onboarding.rs:330` | Label | Where to find the session in Telegram | Read |
| Done / unsaved | Two-step verification recommendation | `shell/view/onboarding.rs:320` | Label | How to enable two-step verification when missing | Read |
| Done / unsaved | Open chats | `shell/view/onboarding.rs:322` | Button | Open chats | Tab, Return or Space |
| Startup blocked | Title | `shell/view.rs:420` | Heading 1 | Couldn't read local storage | Read |
| Startup blocked | Recovery instruction | `shell/view.rs:421` | Alert | Storage recovery copy | Automatic on arrival, Read |
| Startup blocked | Retry | `shell/view.rs:414` | Button | Retry | Tab, Return or Space |
| Signing out | Title | `shell/view/onboarding.rs:300` | Heading 1 | Signing out | Read |
| Signing out | Explanation | `shell/view/onboarding.rs:301` | Label | Ending the session and deleting local account data | Read |
| Logout incomplete | Title | `shell/view.rs:369` | Heading 1 | Logout incomplete | Read |
| Logout incomplete | Local deletion error | `shell/view.rs:370` | Alert | Local deletion failed and can be retried | Automatic on arrival, Read |
| Logout incomplete | Unconfirmed remote sign-out | `shell/view.rs:360` | Label | Telegram logout was unconfirmed | Read |
| Logout incomplete | Retry local deletion | `shell/view.rs:363` | Button | Retry | Tab, Return or Space |
| Logout could not start | Title | `shell/view.rs:404` | Heading 1 | Couldn't start logout | Read |
| Logout could not start | Reason | `shell/view.rs:405` | Alert | What stopped logout and whether the account can stay signed in | Automatic on arrival, Read |
| Logout could not start | Stay signed in | `shell/view.rs:392` | Button | Stay signed in | Tab if user-initiated, Return or Space |
| Logout could not start | Retry | `shell/view.rs:398` | Button | Retry | Tab, Return or Space |
| Sign-in | Transient status/error toast | `shell/view.rs:708` | Alert | Current notice, such as a busy command queue | Automatic on appearance, Read |

## Tasks 5 and 6 · Signed-in shell and shared overlays

Task 5 covers the rail, chat and topic lists, history, and composer. Task 6 adds context menus, folder editing, Settings, logout confirmation, and keyboard focus recovery for virtualized rows. The native test target checks representative roles and states through AppKit and drives keyboard focus through the GPUI window.

| Screen | Element | File and line | Target role | Label or spoken value | Keyboard path | Status |
| --- | --- | --- | --- | --- | --- | --- |
| Sidebar rail | Tab strip | `shell/tabs.rs:46` | Tab list | Chat tabs | Tab into strip, Ctrl+Tab and Ctrl+Shift+Tab already switch tabs | Task 5, done. Only the selected tab is a tab stop (All in the archive). |
| Sidebar rail | All | `shell/tabs.rs:185` | Tab | All, unread count, selected state | Tab or Ctrl+Tab, Return or Space | Task 5, done. The name adds the unread chats of the tab. |
| Sidebar rail | Private | `shell/tabs.rs:186` | Tab | Private, unread count, selected state | Tab or Ctrl+Tab, Return or Space | Task 5, done. The name adds the unread chats of the tab. |
| Sidebar rail | Groups | `shell/tabs.rs:187` | Tab | Groups, unread count, selected state | Tab or Ctrl+Tab, Return or Space | Task 5, done. The name adds the unread chats of the tab. |
| Sidebar rail | Forums | `shell/tabs.rs:188` | Tab | Forums, unread count, selected state | Tab or Ctrl+Tab, Return or Space | Task 5, done. The name adds the unread chats of the tab. |
| Sidebar rail | Folder tab, per folder | `shell/tabs.rs:189` | Tab | Folder name, unread count, selected state | Tab or Ctrl+Tab, Return or Space | Task 5, done. The name adds the unread chats of the tab. |
| Sidebar rail | Folder menu trigger, per folder tab | `shell/tabs.rs` | Button, with shortcut on its row or tab | Open menu for folder name | Tab to selected folder, Shift+F10, or activate the named button with VoiceOver | Task 6. Shared folders report that editing is unavailable. |
| Sidebar rail | Unread filter | `shell/tabs.rs:117` | Switch | Unread, count in active view, on or off | Tab then Space, Cmd+Shift+U already toggles | Task 5, done. A switch, "Unread, N", N the unread chats of the shown view. |
| Sidebar | Heading for active tab | `shell/chat_view.rs:170` | Heading | Active chat view | Read | Task 5, done. A level 2 heading, "Chats"; in the archive the back button takes its place. |
| Sidebar | Connection status | `shell/chat_view.rs:137` | Status | Current connection state | Read | Task 5, done. |
| Sidebar | Settings | `shell/chat_view.rs:202` | Button | Settings | Tab and Return, Cmd+Comma already opens | Task 5, done as a button. The panel is task 6. |
| Sidebar | Chat list, per tab or archive | `shell/chat_view.rs:98` | List | Active view's chats | Tab to list, keyboard rows | Task 5, done. |
| Sidebar | Empty state | `shell/chat_view.rs:86` | Label | Empty view, unread filter or partially loaded explanation | Read | Task 5, done. |
| Sidebar | Chat row, per chat | `shell/chat_view.rs` | List item | Title, unread and mentions, muted, pinned, last sender, preview and time | Return opens. Option+Cmd+Up/Down opens and reveals adjacent rows. | Task 5. Chat summaries carry no last sender. Task 6 moves focus to the first rendered row if its old row leaves the viewport. |
| Sidebar | Chat menu trigger, per chat | `shell/chat_view.rs` | Button, with shortcut on its row or tab | Open menu for chat title | Focus the row and press Shift+F10, or activate the named button with VoiceOver | Task 6. |
| Sidebar | Forum disclosure | `shell/chat_view.rs:471` | Button | Expand or collapse topics for forum, expanded state | Tab and Return. Option+Cmd+Right/Left already changes expansion | Task 5, done. |
| Sidebar | Archive entry | `shell/chat_view.rs:341` | List item | Archived chats, chat and unread counts | Focus and Return | Task 5, done. |
| Archive | Back to main list | `shell/chat_view.rs:191` | Button | Back to all chats | Tab and Return | Task 5, done. |
| Sidebar | More inline topics | `shell/chat_view.rs:329` | Button | More topics | Tab and Return | Task 5, done. |
| Sidebar | Inline topics placeholder | `shell/chat_view.rs:274` | Status | Loading, empty, rate limited, or unavailable | Read | Task 5, done. |
| Sidebar | Retry loading chats | `shell/chat_view.rs:113` | Button | Retry loading chats | Tab and Return | Task 5, done. |
| Forum pane | Forum heading | `shell/topic_view.rs:81` | Heading | Forum title and chat kind | Read | Task 5, done. |
| Forum pane | Topic list | `shell/topic_view.rs:56` | List | Topics in forum name | Tab to list, keyboard rows | Task 5, done. |
| Forum pane and inline topics | Topic row, per topic | `shell/topic_view.rs:173` | List item | Topic title, unread and mentions, closed, pinned, last sender, preview and time | Focus and Return opens. Inline rows already support Option+Cmd+Up/Down stepping | Task 5, done. No last sender: a topic carries none (plan amendment). |
| Forum pane | Empty/loading/recovery explanation | `shell/topic_view.rs:51` | Status | No topics, loading topics, or topics unavailable | Read | Task 5, done. |
| Forum pane | Rate-limit countdown | `shell/topic_view.rs:60` | Status | Time until more topics can load | Read | Task 5, done. |
| Forum pane | Retry loading topics | `shell/topic_view.rs:74` | Button | Retry loading topics | Tab and Return | Task 5, done. |
| Open topic | Back to topics | `shell/topic_view.rs:118` | Button | Back to topics | Tab and Return, Cmd+[ already goes back | Task 5, done. |
| Open topic | Chat/topic heading | `shell/topic_view.rs:102` | Heading | Forum title, topic title, closed state when relevant | Read | Task 5, done. |
| Chat pane | No chat selected | `shell/chat_view.rs:540` | Label | Select a chat | Read | Task 5, done. |
| Chat pane | Chat header | `shell/chat_view.rs:607` | Heading | Chat title and kind | Read | Task 5, done. |
| Chat or topic | History | `shell/chat_view.rs:554` | List | Messages in chat or topic name | Tab to history, read messages in order | Task 5, done. |
| Chat or topic | Empty, loading or unavailable history | `shell/chat_view.rs:525` | Status | Current history placeholder | Read | Task 5, done. |
| Chat or topic | Day boundary, per day | `shell/chat_view.rs:761` | Separator | Local date | Read | Task 5, done as a label: no separator role is read by VoiceOver. |
| Chat or topic | Unread divider | `shell/chat_view.rs:773` | Separator | New messages | Read by item | Task 5, done as an AccessKit level 3 heading. See the AppKit heading limitation below. |
| Chat or topic | Message, per message | `shell/chat_view.rs:882` | List item | Sender, visible message text, time, edited state | Read; Escape from conversation returns to originating chat row | Task 5, done. |
| Chat or topic | Service message, per item | `shell/chat_view.rs:890` | List item | Telegram service message text | Read | Task 5, done. |
| Chat or topic | Reply quote, per quote | `shell/chat_view.rs:846` | Label | Quoted sender and visible excerpt, within message context | Read | Task 5, done. |
| Chat or topic | Media placeholder, per message | `shell/chat_view.rs:835` | Label | Media kind, e.g. “Photo, not shown” | Read | Task 5, done. |
| Chat or topic | Unsupported message | `shell/chat_view.rs:852` | Label | Unsupported message | Read | Task 5, done. |
| Chat or topic | Copy link target, per visible link | `chat/rich/accessible.rs` | Button | Copy link, visible link text | Tab and Return or Space, or activate with VoiceOver | Task 6. Copies the exact target. |
| Chat or topic | Reveal spoiler, per hidden range | `chat/rich/accessible.rs` | Button | Reveal spoiler | Tab and Return or Space, or activate with VoiceOver | Task 6. The message label says “Hidden spoiler” until reveal. |
| Chat or topic | New message announcement | `shell/chat_view.rs:537` | Polite live status | New message from sender, at most once per two seconds | Automatic | Task 5, done. |
| Chat or topic | Jump to latest | `shell/chat_view.rs:623` | Button | Jump to latest | Tab and Return | Task 5, done. |
| Chat or topic | Retry loading history | `shell/chat_view.rs:585` | Button | Retry loading history | Tab and Return | Task 5, done. |
| Composer | Message editor | `shell/chat_view.rs:692` | Multiline text input | Message to chat or topic name | Focus after opening chat, Shift+Return inserts a newline | Task 5, done. |
| Composer | Length counter | `ui/text_input.rs:47` | Label | Characters used out of maximum, near limit | Read | Task 5, done. |
| Composer | Send | `shell/chats.rs:540` | Button | Send message | Return in editor already sends. No onscreen button currently | Task 5, done as an assistive-technology action with no place on screen; Return still sends. |
| Composer | Read-only block | `shell/chat_view.rs:693` | Label | Read-only chat explanation | Read | Task 5, done. |
| Composer | Closed-topic block | `shell/chat_view.rs:694` | Label | Closed topic explanation | Read | Task 5, done. |
| Pending send | Outgoing echo and state | `shell/chat_view.rs:958` | List item | Draft text and pending/failed/returned status | Read | Task 5, done. |
| Pending send | Retry | `shell/chat_view.rs:972` | Button | Retry sending | Tab and Return | Task 5, done. |
| Pending send | Discard | `shell/chat_view.rs:978` | Button | Discard unsent message | Tab and Return | Task 5, done. |
| Returned send | Put back | `shell/chat_view.rs:988` | Button | Put back in composer | Tab and Return | Task 5, done. |
| Returned send | Dismiss | `shell/chat_view.rs:989` | Button | Dismiss returned message | Tab and Return | Task 5, done. |
| Stranded send | Copy text | `shell/chat_view.rs:1000` | Button | Copy unsent text | Tab and Return | Task 5, done. |
| Stranded send | Dismiss | `shell/chat_view.rs:1001` | Button | Dismiss stranded message | Tab and Return | Task 5, done. |
| Chat menu | Context menu | `shell/folder_menu.rs` | Menu | Actions for chat title | Shift+F10 on the chat row or folder tab, then Up/Down and Return | Task 6. Focus enters the first item. |
| Chat menu | Archive or unarchive | `shell/folder_menu.rs` | Menu item | Archive chat or Unarchive chat | Up/Down, Return or Space | Task 6. |
| Chat menu | Folder membership, per editable folder | `shell/folder_menu.rs` | Switch | Include in folder title, on or off | Up/Down, Return or Space | Task 6. |
| Chat menu | Create folder | `shell/folder_menu.rs` | Menu item | New folder with this chat | Up/Down, Return or Space | Task 6. |
| Folder naming | New folder title | `shell/folder_menu.rs` | Text input | Folder name | Focus follows New folder. Type a name and press Return. | Task 6. |
| Folder menu | Context menu | `shell/folder_menu.rs` | Menu | Actions for folder title | Shift+F10 on the chat row or folder tab, then Up/Down and Return | Task 6. Focus enters the first item. |
| Folder menu | Delete | `shell/folder_menu.rs` | Menu item | Delete folder name | Return or Space | Task 6. |
| Folder menu | Confirm delete | `shell/folder_menu.rs` | Menu item | Confirm delete folder name | Return or Space again | Task 6. Focus stays on the same item as its name changes. |
| Chat/folder menu | Dismiss | `shell/folder_menu.rs` | Keyboard action | Close menu | Escape returns focus to the trigger | Task 6. |
| Settings | Panel | `shell/appearance.rs` | Dialog | Settings | Cmd+Comma opens. Tab and Shift+Tab stay in the dialog. | Task 6. |
| Settings | Title | `shell/appearance.rs` | Heading 1 | Settings | Read by item | Task 6. |
| Settings | Appearance heading | `shell/appearance.rs` | Heading 2 | Appearance | Read by item | Task 6. |
| Settings | Colourway instructions | `shell/appearance.rs` | Label | Appearance preview and saving instructions | Read | Task 6. |
| Appearance | Colourway choices | `shell/appearance.rs` | Radio group | Colourway | Tab to a swatch. Arrow keys select and wrap within the group. | Task 6. |
| Appearance | Ultramarine swatch | `shell/appearance.rs` | Radio button | Ultramarine, selected state | Tab to a choice, arrows select within its group, Return or Space selects the focused choice | Task 6. |
| Appearance | Sky swatch | `shell/appearance.rs` | Radio button | Sky, selected state | Tab to a choice, arrows select within its group, Return or Space selects the focused choice | Task 6. |
| Appearance | Fern swatch | `shell/appearance.rs` | Radio button | Fern, selected state | Tab to a choice, arrows select within its group, Return or Space selects the focused choice | Task 6. |
| Appearance | Violet swatch | `shell/appearance.rs` | Radio button | Violet, selected state | Tab to a choice, arrows select within its group, Return or Space selects the focused choice | Task 6. |
| Appearance | Ember swatch | `shell/appearance.rs` | Radio button | Ember, selected state | Tab to a choice, arrows select within its group, Return or Space selects the focused choice | Task 6. |
| Appearance | Graphite swatch | `shell/appearance.rs` | Radio button | Graphite, selected state | Tab to a choice, arrows select within its group, Return or Space selects the focused choice | Task 6. |
| Appearance | abyss swatch | `shell/appearance.rs` | Radio button | abyss, selected state | Tab to a choice, arrows select within its group, Return or Space selects the focused choice | Task 6. |
| Appearance, abyss | Hue/tint instructions | `shell/appearance.rs` | Label | Day and night colour adjustment instructions | Read | Task 6. |
| Appearance, abyss | Night heading | `shell/appearance.rs` | Heading 3 | Night, In use if active | Read by item | Task 6. |
| Appearance, abyss | Day heading | `shell/appearance.rs` | Heading 3 | Day, In use if active | Read by item | Task 6. |
| Appearance, abyss | Night hue chips, per hue | `shell/appearance.rs` | Radio button | Mode or accent, named hue and degrees, selected state | Tab to a choice, arrows select within its group, Return or Space selects the focused choice | Task 6. |
| Appearance, abyss | Night tint chips, per tint | `shell/appearance.rs` | Radio button | Mode and tint percentage, selected state | Tab to a choice, arrows select within its group, Return or Space selects the focused choice | Task 6. |
| Appearance, abyss | Day hue chips, per hue | `shell/appearance.rs` | Radio button | Mode or accent, named hue and degrees, selected state | Tab to a choice, arrows select within its group, Return or Space selects the focused choice | Task 6. |
| Appearance, abyss | Day tint chips, per tint | `shell/appearance.rs` | Radio button | Mode and tint percentage, selected state | Tab to a choice, arrows select within its group, Return or Space selects the focused choice | Task 6. |
| Appearance, accent colourways | Accent instructions | `shell/appearance.rs` | Label | Accent change and reset instructions | Read | Task 6. |
| Appearance, accent colourways | Accent chips, per hue | `shell/appearance.rs` | Radio button | Mode or accent, named hue and degrees, selected state | Tab to a choice, arrows select within its group, Return or Space selects the focused choice | Task 6. |
| Appearance | Reset | `shell/appearance.rs` | Button | Reset colours | Tab where available, Return or Space | Task 6. |
| Settings | Notifications heading | `shell/appearance/notifications.rs` | Heading 2 | Notifications | Read by item | Task 6. |
| Settings | Authorization status | `shell/appearance/notifications.rs` | Label | Current macOS permission or bundle requirement | Read | Task 6. |
| Settings | Ask for notification permission (only while macOS has not been asked) | `shell/appearance/notifications.rs` | Button | Enable notifications | Tab and Return or Space | Review A6. |
| Settings | Open macOS notification settings | `shell/appearance/notifications.rs` | Button | Open System Settings | Tab and Return or Space | Task 6. |
| Settings | Dock badge | `shell/appearance/notifications.rs` | Switch | Dock badge, on or off, or unavailable outside the app bundle and on a removal-only center | Tab and Space when available | Task 6. |
| Settings | Keychain storage switch | `shell/appearance.rs` | Switch | Store secrets in Keychain, on/off or saving | Tab and Space | Task 6. |
| Settings | Keychain storage detail | `shell/appearance.rs` | Label | File versus Keychain storage and restart behavior | Read | Task 6. |
| Settings | Keychain migration note | `shell/appearance.rs` | Label | macOS access prompt and migration instructions | Read | Task 6. |
| Settings | Keychain preference save failure | `shell/appearance.rs` | Alert | Keychain preference failed to save | Announced on appearance. Read by item. | Task 6. |
| Settings | Done | `shell/appearance.rs` | Button | Done | Tab and Return or Space | Task 6. |
| Settings | Dismiss | `shell/appearance.rs` | Keyboard action | Close settings | Escape, Done, or Cmd+Comma restores focus | Task 6. |
| Signed-in shell | Log out | `shell/view.rs` | Button | Log out | Tab and Return or Space | Task 6. |
| Logout confirmation | Modal title | `shell/view.rs` | Dialog heading | Log out? | Read by item | Task 6. |
| Logout confirmation | Consequences | `shell/view.rs` | Label | Local data deletion explanation | Read | Task 6. |
| Logout confirmation | Cancel | `shell/view.rs` | Button | Cancel | Initial focus. Return, Space, or Escape restores focus to Log out. | Task 6. |
| Logout confirmation | Confirm | `shell/view.rs` | Button | Log out | Tab from Cancel, then Return or Space | Task 6. |
| Local recovery | Banner | `shell/view.rs` (`quiet_status`) | Status | Core stopped, account retained, with logout status if relevant | Announced on arrival, Read | Task 6, round 4 (N43). |
| Stopped | Restart instruction | `shell/view.rs` (`quiet_status`) | Status | Core stopped and needs restart | Announced on arrival, Read | Task 6, round 4 (N43). |
| Drafts discarded | Notice, per displayed batch | `shell/view.rs` | Status | Which drafts were discarded | Announced on arrival, Read | Task 6, round 4 (N43). |
| Drafts discarded | Copy all discarded drafts | `shell/view.rs` (`discarded_notice`) | Button | Copy text | Tab and Return | Task 6. |
| Drafts discarded | Dismiss notice | `shell/view.rs` (`discarded_notice`) | Button | Dismiss | Tab and Return | Task 6. |
| Signed-in shell | Transient toast | `shell/view.rs` | Status or alert | Action result or error copy | Polite announcement on arrival or change. Read by item. | Task 6. |
| Shutdown | Closing dialog | `shell/view.rs` (`closing_dialog`) | Dialog (modal) | Closing… | Focus moves to it on Cmd+Q, Cmd+W or the menu, and Tab and Shift+Tab stay on it. Settings, the logout prompt and menus are closed first | Task 6, round 4 (N43). |
| Shutdown | Waiting status | `shell/view.rs` | Status | Closing… | Announced on arrival, Read | Task 6, round 4 (N43). |
| Shutdown error | Dialog | `shell/view.rs` (`closing_dialog`) | Dialog (modal) | Session save unconfirmed or logout incomplete | Focus on Quit, Tab and Shift+Tab stay on it | Task 6, round 4 (N43). |
| Shutdown error | Modal title | `shell/view.rs` (`quit_card`) | Heading 1 | Session save unconfirmed or logout incomplete | Read by item | Task 6, round 4 (N43). |
| Shutdown error | Recovery explanation | `shell/view.rs` (`quit_card`) | Alert | Shutdown error detail | Automatic on appearance, Read | Task 6, round 4 (N43). |
| Shutdown error | Quit | `shell/view.rs` (`quit_card`) | Button | Quit | Initial focus. Return or Space, or Cmd+Q | Task 6, round 4 (N43). |
| macOS app menu | Settings item | `shell/mod.rs` (`menus`) | Native menu item | Settings | macOS menu navigation, Cmd+Comma | Task 6. |
| macOS app menu | Quit item | `shell/mod.rs` (`menus`) | Native menu item | Quit abyss | macOS menu navigation, Cmd+Q | Task 6. |
| macOS window | Close | `shell/mod.rs` (`open`) | Native button | Close | macOS window navigation, Cmd+W | Task 6. |
| macOS window | Minimize | `shell/mod.rs` (`open`) | Native button | Minimize | macOS window navigation, Cmd+M | Task 6. |
| macOS window | Zoom or full screen | `shell/mod.rs` (`open`) | Native button | macOS-provided zoom/full screen title | macOS window navigation | Task 6. |

## AppKit and GPUI limits

The native tests call `accessibilityRole` on the Welcome and Appearance headings. With `accesskit_macos` 0.26.3, AppKit returns `Heading`. The adapter supplies neither the native `AXHeading` role nor `AXHeadingLevel`. The visible headings retain their AccessKit roles, labels, and levels. Use ordinary VoiceOver item navigation for section titles and the unread divider until the adapter maps heading roles and levels. Heading speech and rotor inclusion still need a listening pass. [Apple's VoiceOver rotor guide](https://support.apple.com/guide/voiceover/voiceover-rotor-mchlp2719/mac)

The AppKit adapter exports switches as `AXCheckBox` with the `AXSwitch` subrole and a Boolean value. It has no `AXShowMenu` action. Each context-menu trigger therefore has a named `AXPress` button in the tree and a Shift+F10 shortcut on its keyboard-focused row or tab.

Virtualized views expose rendered rows. If a focused chat or topic row leaves the viewport or disappears, focus advances from its surviving list to the first rendered control. An empty or removed list falls back through the surrounding shell. Escape from a conversation still reveals and focuses the row that opened it. A wrapped rich-text target uses a rectangular outline spanning its lines. Text beyond the viewport becomes reachable as history scrolls.

An OS-initiated quit (a hidden app's Dock menu, Cmd+Tab then Q, a logout or shutdown) shows no closing dialog: GPUI at this revision cannot hand it to the window (plan section 3.4).

Both display preferences update from the `NSWorkspace` display-options notification. Increase Contrast strengthens quiet text and rule tokens for every colourway in both modes. GPUI uses the Reduce Motion preference for its animation primitives. History jumps already change the scroll position directly, and abyss's overlays use static opacity. The app uses no window vibrancy.

## Manual VoiceOver checklist

Run on an unlocked Mac with VoiceOver enabled and an account or demo session that can send a message. Use a disposable folder for folder edits. Keep Quick Nav off when exercising abyss's plain arrow-key shortcuts. Press Fn with F10 if the keyboard uses media keys. VO below means the configured VoiceOver modifier. Use VO+Right/Left to move by item, VO+Shift+Down to interact with a group, and VO+Space to activate an item. Record the macOS version, which steps passed, and any missing speech or unexpected focus transitions.

1. **Sign in without a mouse.** Start at Welcome and hear the unofficial-client disclosure. Tab to Connect to Telegram, open the credential guide, and reach its link and Continue path by keyboard. Enter the API ID and hash or use a saved pair. Confirm that the spoken hash stays masked even when Show API hash on screen is active. Continue through the phone and code fields and the two-step verification screen when present. Confirm that focus follows each field and that a validation error is announced. On the signed-in confirmation, activate Open chats and reach the selected tab and chat rows.
2. **Read a chat.** Tab to a chat row and listen for its title, unread and mute state, preview, and time. Press Return and verify focus enters the named composer. Use VoiceOver to enter history and read sender, text, time, and edited state, plus a media placeholder and the unread divider when present. Try the Headings rotor and record what it includes, then use item navigation to read the heading labels. Use Up/Down and Page Up/Down in history, and verify Escape returns to the original row. Scroll a focused sidebar row off screen and verify focus moves to the first rendered row. With a link and a hidden spoiler in view, verify the link action is named and the spoiler text is spoken only after activating Reveal spoiler.
3. **Send a message.** Open a writable chat with Return. Type a short test message in its named composer. Use Shift+Return for a line break, then Return to send. Read the sent message and confirm the composer remains usable. On an incoming message, check that the polite announcement names the sender.
4. **Switch folders and open their menus.** Focus the selected tab and use Ctrl+Tab or Ctrl+Shift+Tab to reach a folder. Confirm focus follows the selection, the tab name and unread count are announced, and the rows change. Open the folder menu with Shift+F10 and verify the delete confirmation and Escape focus restoration. Open a chat's menu the same way. Use Up/Down to move between its entries, check the membership switch's state, and try the New folder entry. Type a disposable folder name, press Return, and verify the folder appears.
5. **Change the colourway.** Open Settings with Cmd+Comma. Tab and Shift+Tab through the dialog and use arrows to change the colourway, including abyss. Listen for each swatch's name and selection, the Night and Day hue names and tint percentages, and the accent hue choices on a flat colourway. Check that the Dock badge switch announces its state, and that the Open System Settings button and unavailable controls have distinct names. In a short window, Tab to Done and confirm it scrolls into view. Toggle Increase Contrast and Reduce Motion in macOS while abyss is open. Verify stronger outlines and legible quiet text, a visible two-point focus ring, and an immediate Jump to latest. Close Settings and verify focus returns to the opener.
6. **Log out.** Tab to Log out and activate it. Read the confirmation and its local-data consequences. Verify initial focus is on Cancel and Escape returns to Log out. Reopen it, Tab to the confirmation button, and activate it. Verify that the signing-out state is announced and focus returns to the appropriate sign-in control when logout finishes.
7. **Quit.** With Settings open, press Cmd+Q. Verify that Settings closes, VoiceOver says "Closing…" and names a dialog, and Tab and Shift+Tab stay on it. If core cannot confirm the save, verify that the card's title and detail are read, focus is on Quit, and Return quits. Repeat with the logout confirmation and a chat menu open. Then hide abyss with Cmd+W, quit it from the Dock menu, and open it again: it may show no closing dialog (plan section 3.4).
