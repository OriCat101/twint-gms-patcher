# CLAUDE.md

Implementation notes for this repo. See `README.org` for the full rationale.

## What this is

A single Rust binary (std only, no crates) that strips the Google Play services **launch
gate** from `ch.twint.payment` so TWINT runs without GMS, then re-signs it. `src/main.rs`
holds everything. Stages: `pull → decode → patch → build → sign → install`; `all` runs
decode→sign and stops before the destructive install. `pull`/`install` need the phone
connected over USB.

## Build & run

```
nix develop        # flake devshell: apktool, apksigner, zip, keytool, adb, cargo
cargo run -- <stage>
```

## The patch (what `patch` edits)

Two smali inserts in the obfuscated start-activity class
(`build/base/smali_classes4/en.smali`):
- after `iget vN, …, Lit1;->a:I` → `const/4 vN, 0x0` (kills the `ProviderInstaller` pre-check)
- after the `isGooglePlayServicesAvailable(...)I` `move-result vN` → `const/4 vN, 0x0`

`patch` matches on the **stable GMS signatures** + structural shape, requires an exact hit
count, and **aborts rather than emit a half-patched APK** if TWINT changes. It only touches app
smali; `com/google/…` (bundled GMS) is excluded. It is idempotent and backs edits up to
`build/backups/`. If a TWINT update moves the gate, re-run `pull`/`decode`/`patch`; touch the
matchers in `patch_gate` / `patch_precheck` only if the instruction shape genuinely changed.

## Constraints / gotchas (don't "fix" these)

- **No aapt2 on aarch64.** Google ships no aarch64 `aapt2` and apktool's bundled one is x86_64,
  so apktool's resource repack can't run. `build` deliberately **never recompiles resources**:
  it assembles only the changed dex via a minimal apktool project (lone `smali/` folder, no
  res/manifest ⇒ no aapt2) and injects it into the original APK. Keep it that way.
- **Dex must be injected uncompressed** (`zip -0`) — the originals are Stored; the manifest may
  rely on it. Don't switch to default compression.
- **JVM temp dir**: apktool/keytool use `java.io.tmpdir`; `main()` points `_JAVA_OPTIONS` at a
  writable `build/tmp` unless already set.
- **Split signing**: `base` and `split_config.tvdpi` must be signed with the **same** key and
  installed together (`install-multiple`).
- **Re-signing = data loss** on install (can't update over the Play-signed app); `install`
  uninstalls first and never runs without `--install`.

## Not committed

`in/ build/ out/ target/` and all `*.apk` / `*.jks` are gitignored — never commit APK binaries
or signing keys.
