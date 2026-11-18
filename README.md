<p align="center">
  <img src="assets/icon/well.svg" width="96" height="96" alt="The abyss mark: six nested squares descending to a dark centre">
</p>

<h1 align="center">abyss</h1>

<p align="center">Look in. Nothing looks back.</p>

A small, auditable Telegram client for macOS, written in Rust with
[GPUI](https://www.gpui.rs/) and [grammers](https://github.com/Lonami/grammers).

## Threat model

abyss reduces the client-side attack surface: less code parses what
strangers send you, and there are no secret chats, calls, stickers, browser
or Mini Apps. It does not protect your chats from Telegram itself; cloud
chats are still readable by Telegram's servers.

## Getting started

Build and run against the built-in demo backend (no network, no account):

```sh
cargo dev                   # demo account; the login code is 12345
cargo dev -- --onboarding   # demo, starting from the credential screens
```

To connect to Telegram, build and open the macOS app:

```sh
scripts/bundle.sh --debug
open target/bundle/debug/abyss.app
```

Follow
[Connecting abyss to Telegram](docs/connecting.md). You need your own
`api_id` and `api_hash` from my.telegram.org; the guide walks through it.

What is done, what is next and what is still undecided: [roadmap](docs/roadmap.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
