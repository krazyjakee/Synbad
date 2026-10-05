# macOS builds and signing

macOS 12 or newer is required. The source installer builds Synbad and the patched Deskflow Core natively,
including when launched from a Rosetta terminal. Install Xcode command-line
tools, Rust/rustup, and native Homebrew dependencies first:

```sh
brew install cmake ninja qt openssl@3 opus pkg-config librsvg
bash dist/macos/install.sh
```

For local installs, `sign.sh` defaults to ad-hoc signing with fixed
identifiers. To use a certificate already in the keychain, set
`SYNBAD_SIGN_IDENTITY` to its Developer ID Application identity before
running the installer. Keep the same certificate/team across upgrades.

Configure these repository secrets to enable Developer ID signing:

| Secret | Value |
| --- | --- |
| `MACOS_SIGN_IDENTITY` | Developer ID Application identity (including team ID) |
| `MACOS_CERT_P12_BASE64` | Base64-encoded exported signing certificate and private key |
| `MACOS_CERT_PASSWORD` | Password protecting the exported PKCS#12 file |

The workflow imports the certificate into a temporary keychain and removes
it after packaging. It signs all four executables before signing and
verifying `Synbad.app`, with the stable app identifier `dev.synbad.synbad`.
The daemon and GUI carry the [audio-input entitlement](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.device.audio-input)
so signing with the hardened runtime preserves Core Audio capture.
Native arm64 and Intel runners build separate artifacts and run the patched
Core's teardown regressions in CI. Releases without signing credentials use
ad-hoc signatures with the same fixed identifiers. If a Developer ID identity
is configured, signing failures stop packaging rather than silently falling
back. Notarization is not configured by this workflow.

Both the DMG and update archive contain the same complete signed app.
The separate signed Core archive repairs installs updated by older
updaters that copied only the Rust executables. The runtime rejects an
Intel-only Core on Apple Silicon and bypasses the old upstream cache.
