# Patched macOS Core

Synbad builds Deskflow v1.17.0 at commit
`44bd69fdc8df726909f35b7efeeebc554bfde1c5`, with `teardown.patch` applied.
The patch joins the socket worker and macOS event thread before screen
destruction, replaces the Carbon buffer before its owning thread exits,
makes worker shutdown idempotent, and rejects events before
the Carbon queue is initialized. Socket removal remains safe after shutdown,
including cancellation while a callback holds the worker's cleanup locks.
The patch also disables forced Ninja response files on macOS, where Apple's
archiver does not accept that Windows-oriented upstream workaround.
The Carbon-loop timeout uses a numeric constant compatible with current
Clang's C++20 checks. The macOS deployment target is upstream's 12.0 baseline.
Core builds select Xcode 16.4 / macOS SDK 15.5 and verify the SDK stamp in
both executables. The older deployment target alone does not prevent the
macOS 26 SDK's Carbon input-dispatch regression
([upstream report](https://github.com/input-leap/input-leap/issues/2367)).

`dist/macos/build-core.sh` builds native arm64 or x86_64 split executables.
The Core and its GPL license ship beside the daemon; the release also includes
the complete patched source archive. Deskflow remains GPL-2.0-or-later;
Synbad communicates with it as a separate process.

Apple Silicon selects the native Core and updater archive even if the current
Synbad process runs under Rosetta. Old version-only upstream Core caches are
ignored on macOS. An upgrade from an older updater that replaces only Synbad's
Rust executables downloads the versioned, checksummed patched Core release asset.
