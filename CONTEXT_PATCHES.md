# Context Buzz patch stack

This fork keeps a small, reviewable patch stack on top of
[`block/buzz`](https://github.com/block/buzz). The official Buzz application
remains installed and untouched. Context builds install side by side so that an
upstream release is always the rollback path.

## Current base

- Upstream repository: `block/buzz`
- Fork: `contextdotbuild/buzz`
- Upstream base: `db514b153a8cb17631d6e80b91c95fdc45deb147`
- Combined branch: `integration/context-reliability`
- Combined feature head: `1507c63ccd4ae7a1291915d81a7d7a2aad747ca5`

## Context-owned patches

### Mention an existing remote agent

- Fork branch: `fix/existing-agent-mentions`
- Commit: `dcf7dbed85940d520fa5c8035b46def916c50f79`
- Combined commit: `1507c63ccd4ae7a1291915d81a7d7a2aad747ca5`

A chat-only client can mention an existing agent identity running on another
machine. It does not require or create a local managed runtime.

The current fix also discovers agents from the relay-signed participant list
of an existing DM. The viewer must be a participant, only the latest metadata
head is accepted, and the send path still revalidates ownership and response
policy. The upstream pull request is
[`block/buzz#6882`](https://github.com/block/buzz/pull/6882).

### Add an existing remote agent to a channel

- Fork branch: `fix/add-existing-channel-agent`
- Commit: `a6cc28103a509d60d864e2810dd82a19c7dfe6e7`
- Combined commit: `f85704b41d37993658b83dda6293dd7c4b2ccecf`

The channel member dialog distinguishes existing relay identities from local
personas. A chat-only client can add the existing identity without deploying a
new instance or minting another bot keypair.

### Recover messages across agent-runtime restarts

- Fork branch: `fix/restart-replay`
- Fork head: `b58426b86b991c8ff03d0d2e4055183410cf69d3`
- Combined commits: `d8eea6fa06e35c41b8128738e6eabdb77b2e2868`
  and `aa53452da454b4f378810d4edc98d4322edaeb7b`
- Related upstream pull request:
  [`block/buzz#6772`](https://github.com/block/buzz/pull/6772)

The upstream pull request introduces a persisted replay watermark. The Context
follow-up makes the state pair-specific, remembers recent exact event IDs, and
does not treat a one-second timestamp as a unique message. This prevents quick
restarts from answering the same event again without dropping distinct,
same-second or out-of-order events.

This patch runs in the desktop process that owns the managed runtime. The
MacBook chat client does not make the iMac runtime replay-safe; the patched
runtime launcher must also be installed on the iMac before claiming that
protection live.

## Separate reply-depth work

Reply-depth enforcement is maintained on the focused branches
`fix/depth-one-agent-replies` and `fix/depth-one-client-replies`. It is not part
of this reliability integration branch. Keep its implementation and Corpus
record with its current owner unless the work is deliberately reconciled.

## Verification recorded for the combined branch

- Desktop TypeScript type-check passed.
- Biome and repository check scripts passed; remaining messages were existing
  informational warnings.
- All 5,556 desktop unit tests passed.
- Focused Playwright tests passed for both cross-device mention and adding an
  existing remote agent with zero local runtimes.
- The DM discovery correction passed 56 focused native Rust tests and the
  focused Playwright DM mention test.
- A paired MacBook `Buzz Context` build offered the existing online PM Bot in
  the mention picker, sent a DM mention without a local runtime, and received
  the exact iMac-agent reply `MACBOOK-MENTION-826B`. No `No runtime available`
  notice appeared.

The side-by-side application build is a separate release proof. A successful
build alone does not prove the paired live MacBook-to-iMac workflows.

## Side-by-side build boundary

The Context build uses:

- Product name: `Buzz Context`
- Bundle identifier: `xyz.contextdotbuild.buzz.client`
- Deep-link scheme: `buzz-context`
- Identity store: the app's owner-only `identity.key` file, scoped by the
  Context bundle identifier; the build deliberately disables `system-keyring`
- Build-time relay: `wss://buildcontext.communities.buzz.xyz`
- Bundle target: macOS `.app` only
- Install path: `~/Applications/Buzz Context.app`

The reproducible overrides are `desktop/src-tauri/tauri.context.conf.json` and
`desktop/src-tauri/ContextInfo.plist` on the integration branch. Run
`./scripts/build-context-desktop.sh`; it builds the real sidecars, embeds the
Build Context WebSocket and HTTP relay addresses, disables the default
`system-keyring` feature, creates only the macOS app bundle, and ad-hoc signs
the result.

Disabling `system-keyring` is intentional for this locally maintained,
ad-hoc-signed chat client. A new ad-hoc signature changes the macOS Keychain
access-control identity and can produce another credential prompt. Buzz's
existing no-keyring path instead stores the Nostr identity in a mode-`0600`
file inside this app's own data directory, so Context updates neither prompt
for nor collide with the official Buzz Keychain entry. Pair once through the
supported flow after moving to this build; later Context rebuilds preserve the
same app data directory.

Never overwrite, re-sign, move or remove `/Applications/Buzz.app`. Never copy
the official application's identity, Keychain item or private application
state into the Context build. Pair the Context build through Buzz's supported
pairing flow.

## Updating after upstream changes

1. Fetch `block/buzz` and inspect upstream changes touching the files in each
   focused patch.
2. Rebase each focused branch independently onto the chosen upstream commit.
   Remove a patch when upstream now provides the same behaviour and its focused
   regression test passes without the Context change.
3. Recreate the combined integration branch from that exact upstream commit by
   applying only the still-needed focused commits in the order listed above.
4. Run each focused regression test, then the combined type-check, formatter,
   desktop unit suite and focused Playwright workflows.
5. Build the isolated `Buzz Context` bundle with the declared Build Context
   relay and real sidecars by running `./scripts/build-context-desktop.sh`.
   Verify its embedded relay, bundle identity, URL scheme, executable sidecars,
   local-file identity mode and signature before replacing only the prior
   Context build.
6. Pair normally and prove the real MacBook chat-client workflows. Prove
   restart replay separately on the iMac runtime owner before enabling or
   claiming that slice there.
7. Update this file with the new upstream base, focused commit IDs, combined
   head, test evidence and upstream pull-request state.

An upstream merge is not enough by itself to retire a Context patch. The patch
is retired only after the upstream tree passes the relevant focused regression
and the next official build containing it works in the real paired workflow.
