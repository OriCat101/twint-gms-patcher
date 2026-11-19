//! twint-gms-patcher
//!
//! Removes ch.twint.payment's Google Play services *launch gate* so the app opens
//! on a de-Googled phone (no GMS, no microG). It does NOT add any Google functionality;
//! push (FCM) and anything else that genuinely needs GMS will still not work.
//!
//! Stages (`cargo run -- <stage>`):
//!   pull    adb-pull the installed base + split APKs into in/
//!   decode  apktool d  in/base.apk -> build/base
//!   patch   rewrite the two smali gate instructions (pure Rust, self-validating)
//!   build   apktool b  build/base -> out/twint-patched-unsigned.apk
//!   sign    zipalign + apksigner (base + split) with a throwaway key in out/twint.jks
//!   all     decode -> patch -> build -> sign   (stops before the destructive install)
//!   install adb uninstall + install-multiple   (requires --install; WIPES app data)
//!
//! The patcher only edits app code. GMS library smali under com/google/ is never touched.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{exit, Command};

const PKG: &str = "ch.twint.payment";
const GMS_AVAIL_SIG: &str = "->isGooglePlayServicesAvailable(Landroid/content/Context;)I";
const EXPECTED_SPLITS: &[&str] = &["base.apk", "split_config.tvdpi.apk"];

type R<T> = Result<T, String>;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let stage = args.get(1).map(String::as_str).unwrap_or("help");
    let root = match project_root() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            exit(1);
        }
    };
    // apktool/keytool/apksigner are JVM tools that use java.io.tmpdir (defaults to /tmp,
    // which can be read-only in sandboxes). Point it at a writable project dir unless the
    // environment already set JVM options.
    if std::env::var_os("_JAVA_OPTIONS").is_none() {
        let tmp = root.join("build/tmp");
        let _ = fs::create_dir_all(&tmp);
        std::env::set_var("_JAVA_OPTIONS", format!("-Djava.io.tmpdir={}", tmp.display()));
    }
    let res = match stage {
        "pull" => pull(&root),
        "decode" => decode(&root),
        "patch" => patch(&root),
        "build" => build(&root),
        "sign" => sign(&root),
        "all" => all(&root),
        "install" => install(&root, args.iter().any(|a| a == "--install")),
        _ => {
            help();
            Ok(())
        }
    };
    if let Err(e) = res {
        eprintln!("\n\x1b[31merror:\x1b[0m {e}");
        exit(1);
    }
}

fn help() {
    eprintln!(
        "twint-gms-patcher — strip the Play-services launch gate from {PKG}\n\
         \n\
         stages: pull | decode | patch | build | sign | all | install --install\n\
         typical: cargo run -- pull && cargo run -- all   (then test, then `install --install`)\n"
    );
}

// ---------------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------------

fn project_root() -> R<PathBuf> {
    // Walk up from CWD until we find Cargo.toml (the project root).
    let mut d = std::env::current_dir().map_err(|e| e.to_string())?;
    loop {
        if d.join("Cargo.toml").is_file() {
            return Ok(d);
        }
        if !d.pop() {
            return Err("could not locate project root (no Cargo.toml above CWD)".into());
        }
    }
}

fn ensure_dir(p: &Path) -> R<()> {
    fs::create_dir_all(p).map_err(|e| format!("mkdir {}: {e}", p.display()))
}

// ---------------------------------------------------------------------------
// subprocess helper
// ---------------------------------------------------------------------------

fn run<I, S>(bin: &str, args: I, cwd: &Path) -> R<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let argv: Vec<String> = args
        .into_iter()
        .map(|a| a.as_ref().to_string_lossy().into_owned())
        .collect();
    eprintln!("  $ {bin} {}", argv.join(" "));
    let status = Command::new(bin)
        .args(&argv)
        .current_dir(cwd)
        .status()
        .map_err(|e| format!("spawn {bin}: {e} (is it on PATH? use `nix develop`)"))?;
    if !status.success() {
        return Err(format!("{bin} exited with {status}"));
    }
    Ok(())
}

/// Run a command, returning Ok even if it exits non-zero (caller verifies side effects).
fn run_allow_fail(bin: &str, args: &[&str], cwd: &Path) -> bool {
    eprintln!("  $ {bin} {}", args.join(" "));
    Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_capture(bin: &str, args: &[&str], cwd: &Path) -> R<String> {
    let out = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("spawn {bin}: {e} (is it on PATH? use `nix develop`)"))?;
    if !out.status.success() {
        return Err(format!(
            "{bin} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// stages
// ---------------------------------------------------------------------------

fn pull(root: &Path) -> R<()> {
    let in_dir = root.join("in");
    ensure_dir(&in_dir)?;
    println!("[pull] querying device for {PKG} APK paths");
    let out = run_capture("adb", &["shell", "pm", "path", PKG], root)?;
    let remotes: Vec<String> = out
        .lines()
        .filter_map(|l| l.trim().strip_prefix("package:"))
        .map(|s| s.trim().to_string())
        .collect();
    if remotes.is_empty() {
        return Err(format!("device reports no paths for {PKG} (is it installed / phone plugged in?)"));
    }
    let mut names: Vec<String> = remotes
        .iter()
        .map(|r| basename(r).to_string())
        .collect();
    names.sort();
    let mut expected: Vec<String> = EXPECTED_SPLITS.iter().map(|s| s.to_string()).collect();
    expected.sort();
    if names != expected {
        return Err(format!(
            "unexpected APK split set on device: {names:?}\n\
             this patcher was built for {expected:?}. Signing/installing a different split set\n\
             needs the code updated — aborting rather than guessing."
        ));
    }
    for r in &remotes {
        let dst = in_dir.join(basename(r));
        run("adb", ["pull", r.as_str(), dst.to_str().unwrap()], root)?;
    }
    println!("[pull] ok -> {}", in_dir.display());
    Ok(())
}

fn decode(root: &Path) -> R<()> {
    let base = root.join("in/base.apk");
    if !base.is_file() {
        return Err(format!("missing {} — run `pull` first", base.display()));
    }
    ensure_dir(&root.join("build"))?;
    println!("[decode] apktool d in/base.apk -> build/base");
    run(
        "apktool",
        ["d", "-f", "-o", "build/base", "in/base.apk"],
        root,
    )?;
    println!("[decode] ok");
    Ok(())
}

/// Build strategy on aarch64: Google ships no aarch64 Linux aapt2, and apktool's bundled
/// aapt2 is x86_64, so apktool's *resource* repack cannot run here. But we only changed
/// smali/dex, never resources — so we never recompile resources at all. For each smali
/// folder we edited, we assemble just that one dex via a minimal apktool project (a project
/// with a lone `smali/` folder and no res/manifest triggers no aapt2), then inject the
/// rebuilt dex into a copy of the ORIGINAL base.apk — keeping its exact resources, manifest,
/// libs and assets byte-for-byte — and strip the now-void v1 signature. apksigner adds a
/// fresh v2/v3 signature in `sign`. (This is why the aapt2-FEX setup isn't needed here.)
fn build(root: &Path) -> R<()> {
    let base = root.join("build/base");
    if !base.is_dir() {
        return Err("build/base missing — run `decode` then `patch` first".into());
    }
    let orig = root.join("in/base.apk");
    if !orig.is_file() {
        return Err("in/base.apk missing — run `pull` first".into());
    }
    ensure_dir(&root.join("out"))?;

    let changed = changed_dex_folders(root)?;
    if changed.is_empty() {
        return Err("no patched smali recorded in build/backups — run `patch` first".into());
    }

    // Assemble each changed folder -> its dex.
    let yml = base.join("apktool.yml");
    let mut built: Vec<(String, PathBuf)> = Vec::new();
    for (folder, dexname) in &changed {
        println!("[build] assembling {dexname} from {folder} (no resources, no aapt2)");
        let proj = root.join("build/dexasm").join(folder);
        let _ = fs::remove_dir_all(&proj);
        ensure_dir(&proj)?;
        // Point the project's `smali` folder at the real one via symlink (no 12 MB copy).
        let src = base.join(folder).canonicalize().map_err(|e| e.to_string())?;
        std::os::unix::fs::symlink(&src, proj.join("smali"))
            .map_err(|e| format!("symlink smali: {e}"))?;
        fs::copy(&yml, proj.join("apktool.yml")).map_err(|e| format!("copy apktool.yml: {e}"))?;

        let proj_rel = format!("build/dexasm/{folder}");
        let discard = format!("{proj_rel}/out.apk");
        run("apktool", ["b", &proj_rel, "-o", &discard], root)?;

        let dex = proj.join("build/apk/classes.dex");
        if !dex.is_file() {
            return Err(format!("apktool did not produce a dex for {folder}"));
        }
        built.push((dexname.clone(), dex));
    }

    // Inject the rebuilt dex(es) into a copy of the original APK.
    let out_apk = root.join("out/twint-patched-unsigned.apk");
    fs::copy(&orig, &out_apk).map_err(|e| format!("copy base.apk: {e}"))?;
    for (dexname, dex) in &built {
        let staged = root.join("out").join(dexname);
        fs::copy(dex, &staged).map_err(|e| format!("stage {dexname}: {e}"))?;
        println!("[build] injecting {dexname} (uncompressed) into base.apk copy");
        // `zip -0` stores uncompressed (dex are Stored in the original); run from out/ so the
        // entry name is exactly `<dexname>`.
        run("zip", ["-0", "-X", "twint-patched-unsigned.apk", dexname], &root.join("out"))?;
        let _ = fs::remove_file(&staged);
    }

    println!("[build] stripping old v1 signature files");
    let _ = run_allow_fail(
        "zip",
        &[
            "-d", "twint-patched-unsigned.apk",
            "META-INF/MANIFEST.MF", "META-INF/*.SF", "META-INF/*.RSA",
            "META-INF/*.DSA", "META-INF/*.EC",
        ],
        &root.join("out"),
    );
    println!("[build] ok -> out/twint-patched-unsigned.apk");
    Ok(())
}

/// Returns the (smali-folder, dex-filename) pairs we edited, derived from build/backups
/// (every backed-up .smali marks an edited top-level smali folder).
fn changed_dex_folders(root: &Path) -> R<Vec<(String, String)>> {
    let backups = root.join("build/backups");
    let mut folders: Vec<String> = Vec::new();
    if backups.is_dir() {
        for entry in fs::read_dir(&backups).map_err(|e| e.to_string())? {
            let p = entry.map_err(|e| e.to_string())?.path();
            let name = p.file_name().and_then(OsStr::to_str).unwrap_or("");
            if p.is_dir() && (name == "smali" || name.starts_with("smali_classes")) {
                // Only count it if it actually holds a backed-up .smali.
                let mut any = Vec::new();
                collect_by_ext(&p, "smali", &mut any)?;
                if !any.is_empty() && !folders.contains(&name.to_string()) {
                    folders.push(name.to_string());
                }
            }
        }
    }
    folders.sort();
    Ok(folders
        .into_iter()
        .map(|f| {
            let dex = dex_name_for_folder(&f);
            (f, dex)
        })
        .collect())
}

fn dex_name_for_folder(folder: &str) -> String {
    if folder == "smali" {
        "classes.dex".to_string()
    } else if let Some(n) = folder.strip_prefix("smali_classes") {
        format!("classes{n}.dex")
    } else {
        format!("{folder}.dex")
    }
}

fn collect_by_ext(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) -> R<()> {
    for entry in fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))? {
        let p = entry.map_err(|e| e.to_string())?.path();
        if p.is_dir() {
            collect_by_ext(&p, ext, out)?;
        } else if p.extension().and_then(OsStr::to_str) == Some(ext) {
            out.push(p);
        }
    }
    Ok(())
}

fn sign(root: &Path) -> R<()> {
    let out = root.join("out");
    ensure_dir(&out)?;
    let ks = out.join("twint.jks");
    let storepass = "twintpatch";
    if !ks.is_file() {
        println!("[sign] generating throwaway signing key out/twint.jks");
        run(
            "keytool",
            [
                "-genkeypair", "-keystore", ks.to_str().unwrap(),
                "-storepass", storepass, "-keypass", storepass,
                "-alias", "twint", "-keyalg", "RSA", "-keysize", "2048",
                "-validity", "10000", "-dname", "CN=twint-gms-patch",
            ],
            root,
        )?;
    }

    // (unsigned-or-source, final-output-name)
    let jobs = [
        ("out/twint-patched-unsigned.apk", "out/twint-patched.apk"),
        ("in/split_config.tvdpi.apk", "out/split_config.tvdpi.apk"),
    ];
    for (src, dst) in jobs {
        if !root.join(src).is_file() {
            return Err(format!("missing {src} — run the earlier stages first"));
        }
        // zipalign is a nice-to-have (mmap perf); it lives only in the unfree Android
        // build-tools. The manifest has extractNativeLibs="true", so page alignment is NOT
        // required to install. Use zipalign if present, otherwise just copy through.
        if zipalign_available(root) {
            println!("[sign] zipalign + apksigner {src} -> {dst}");
            run("zipalign", ["-p", "-f", "4", src, dst], root)?;
        } else {
            println!("[sign] (zipalign absent, skipping) apksigner {src} -> {dst}");
            fs::copy(root.join(src), root.join(dst))
                .map_err(|e| format!("copy {src} -> {dst}: {e}"))?;
        }
        run(
            "apksigner",
            [
                "sign",
                "--ks", ks.to_str().unwrap(),
                "--ks-pass", &format!("pass:{storepass}"),
                "--key-pass", &format!("pass:{storepass}"),
                "--v2-signing-enabled", "true",
                "--v3-signing-enabled", "true",
                dst,
            ],
            root,
        )?;
    }
    // Sanity-check the signatures we just produced.
    for apk in ["out/twint-patched.apk", "out/split_config.tvdpi.apk"] {
        println!("[sign] verifying {apk}");
        run("apksigner", ["verify", "--print-certs", apk], root)?;
    }
    println!("[sign] ok -> out/twint-patched.apk + out/split_config.tvdpi.apk (signer: out/twint.jks)");
    Ok(())
}

fn zipalign_available(root: &Path) -> bool {
    Command::new("zipalign")
        .arg("-h")
        .current_dir(root)
        .output()
        .is_ok()
}

fn all(root: &Path) -> R<()> {
    decode(root)?;
    patch(root)?;
    build(root)?;
    sign(root)?;
    println!(
        "\n[all] done. Signed set in out/.\n\
         Test without wiping yet: `cargo run -- install --install` will\n\
         UNINSTALL (wiping TWINT data) then install-multiple. Confirm before running."
    );
    Ok(())
}

fn install(root: &Path, confirmed: bool) -> R<()> {
    if !confirmed {
        return Err(
            "refusing to install without --install.\n\
             This UNINSTALLS ch.twint.payment first (wipes its data -> you must re-enrol),\n\
             then installs the re-signed set. Re-run: `cargo run -- install --install`"
                .into(),
        );
    }
    let base = root.join("out/twint-patched.apk");
    let split = root.join("out/split_config.tvdpi.apk");
    if !base.is_file() || !split.is_file() {
        return Err("signed APKs missing in out/ — run `all` first".into());
    }
    println!("[install] uninstalling {PKG} (data will be wiped)");
    let _ = run("adb", ["uninstall", PKG], root); // ok if not currently installed
    println!("[install] install-multiple");
    run(
        "adb",
        [
            "install-multiple",
            base.to_str().unwrap(),
            split.to_str().unwrap(),
        ],
        root,
    )?;
    println!("[install] ok — launch TWINT and watch it stay open");
    Ok(())
}

// ---------------------------------------------------------------------------
// patch core
// ---------------------------------------------------------------------------

fn patch(root: &Path) -> R<()> {
    let base = root.join("build/base");
    if !base.is_dir() {
        return Err("build/base missing — run `decode` first".into());
    }
    let files = app_smali_files(&base)?;
    if files.is_empty() {
        return Err("no app smali files found under build/base".into());
    }

    let mut gate_hits = 0usize; // Patch A: isGooglePlayServicesAvailable result forced to 0
    let mut prechk_hits = 0usize; // Patch B: ProviderInstaller it1.a pre-check forced to 0
    let mut edited_files: Vec<String> = Vec::new();

    for f in &files {
        let text = fs::read_to_string(f).map_err(|e| format!("read {}: {e}", f.display()))?;
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

        let a = patch_gate(&mut lines);
        let b = patch_precheck(&mut lines);
        gate_hits += a.found;
        prechk_hits += b.found;

        if a.changed || b.changed {
            // Back up outside the smali tree so apktool doesn't try to read the .orig file.
            let relp = f.strip_prefix(&base).unwrap_or(f);
            let orig = root.join("build/backups").join(relp);
            if !orig.exists() {
                if let Some(parent) = orig.parent() {
                    ensure_dir(parent)?;
                }
                fs::copy(f, &orig).map_err(|e| format!("backup {}: {e}", orig.display()))?;
            }
            fs::write(f, lines.join("\n") + "\n")
                .map_err(|e| format!("write {}: {e}", f.display()))?;
            edited_files.push(rel(root, f));
        }
    }

    println!("[patch] gate (isGooglePlayServicesAvailable -> 0) sites : {gate_hits}");
    println!("[patch] pre-check (ProviderInstaller it1.a -> 0) sites  : {prechk_hits}");
    if edited_files.is_empty() {
        println!("[patch] no edits needed (already patched)");
    } else {
        for e in &edited_files {
            println!("[patch] edited {e}");
        }
    }

    if gate_hits == 0 {
        return Err(
            "did not find the GoogleApiAvailability->isGooglePlayServicesAvailable gate.\n\
             The app likely changed; refusing to produce an unpatched APK."
                .into(),
        );
    }
    if prechk_hits == 0 {
        return Err(
            "did not find the ProviderInstaller it1.a pre-check (iget ...->a:I / if-eqz / ->w1(I)V).\n\
             On this phone that pre-check shows the Play-services dialog and closes the app, so a\n\
             patch without it would still fail. Refusing to continue; the app likely changed."
                .into(),
        );
    }
    println!("[patch] ok");
    Ok(())
}

struct PatchResult {
    found: usize,   // matched anchors (fresh + already-patched)
    changed: bool,  // did we actually insert anything
}

/// Patch A — after `invoke-* ...isGooglePlayServicesAvailable(...)I` + `move-result vN`,
/// force vN to 0 so the "services available" branch is always taken.
fn patch_gate(lines: &mut Vec<String>) -> PatchResult {
    let mut found = 0;
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        if t.starts_with("invoke-") && t.contains(GMS_AVAIL_SIG) {
            if let Some(j) = next_instr(lines, i + 1) {
                if let Some(reg) = after_prefix(lines[j].trim(), "move-result ") {
                    let reg = reg.trim().to_string();
                    found += 1;
                    // already patched?
                    let already = next_instr(lines, j + 1)
                        .map(|k| is_const_zero(lines[k].trim(), &reg))
                        .unwrap_or(false);
                    if !already {
                        let indent = indent_of(&lines[j]);
                        lines.insert(j + 1, format!("{indent}{}", const_zero(&reg)));
                        changed = true;
                    }
                }
            }
        }
        i += 1;
    }
    PatchResult { found, changed }
}

/// Patch B — the ProviderInstaller gate. Shape in onCreate:
///   iget vN, vM, L..;->a:I
///   if-eqz vN, :cond_x
///   invoke-virtual {p0, vN}, L..;->w1(I)V
/// Force vN to 0 after the iget so the error dialog is never shown.
fn patch_precheck(lines: &mut Vec<String>) -> PatchResult {
    let mut found = 0;
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        if let Some(reg) = iget_int_field_a(t) {
            // Look at the following real instructions.
            let n1 = next_instr(lines, i + 1);
            let is_already = n1
                .map(|k| is_const_zero(lines[k].trim(), &reg))
                .unwrap_or(false);
            if is_already {
                // iget / const/4 0 / if-eqz / invoke(p0,reg)(I)V
                let n2 = n1.and_then(|k| next_instr(lines, k + 1));
                let n3 = n2.and_then(|k| next_instr(lines, k + 1));
                if seq_ifeqz_then_p0call(lines, &reg, n2, n3) {
                    found += 1;
                }
            } else if seq_ifeqz_then_p0call(lines, &reg, n1, n1.and_then(|k| next_instr(lines, k + 1))) {
                found += 1;
                let indent = indent_of(&lines[i]);
                lines.insert(i + 1, format!("{indent}{}", const_zero(&reg)));
                changed = true;
            }
        }
        i += 1;
    }
    PatchResult { found, changed }
}

/// True if `a` is `if-eqz reg, :cond_*` and `b` is `invoke-virtual {p0, reg}, L..;->name(I)V`.
fn seq_ifeqz_then_p0call(
    lines: &[String],
    reg: &str,
    a: Option<usize>,
    b: Option<usize>,
) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return false;
    };
    let ta = lines[a].trim();
    let tb = lines[b].trim();
    let ifeqz_ok = ta.starts_with("if-eqz ")
        && ta
            .after_if_reg(reg); // reg followed by , :cond
    let call_ok = tb.starts_with("invoke-virtual ")
        && tb.contains(&format!("{{p0, {reg}}}"))
        && tb.ends_with("(I)V");
    ifeqz_ok && call_ok
}

trait IfReg {
    fn after_if_reg(&self, reg: &str) -> bool;
}
impl IfReg for str {
    fn after_if_reg(&self, reg: &str) -> bool {
        // "if-eqz v1, :cond_0"
        if let Some(rest) = self.strip_prefix("if-eqz ") {
            let rest = rest.trim();
            return rest.starts_with(&format!("{reg},")) || rest == reg;
        }
        false
    }
}

// ---------------------------------------------------------------------------
// smali line helpers
// ---------------------------------------------------------------------------

/// Index of the next real instruction at/after `from`, skipping blanks, `.`-directives
/// and `#` comments.
fn next_instr(lines: &[String], from: usize) -> Option<usize> {
    (from..lines.len()).find(|&k| {
        let t = lines[k].trim();
        !t.is_empty() && !t.starts_with('.') && !t.starts_with('#')
    })
}

fn after_prefix<'a>(s: &'a str, p: &str) -> Option<&'a str> {
    s.strip_prefix(p)
}

/// `iget vDst, vObj, L..;->a:I` -> Some("vDst")
fn iget_int_field_a(t: &str) -> Option<String> {
    if !t.starts_with("iget ") {
        return None;
    }
    if !t.ends_with(";->a:I") {
        return None;
    }
    // iget v1, v0, Lit1;->a:I
    let dst = t["iget ".len()..].split(',').next()?.trim();
    if dst.starts_with('v') {
        Some(dst.to_string())
    } else {
        None
    }
}

fn is_const_zero(t: &str, reg: &str) -> bool {
    t == format!("const/4 {reg}, 0x0") || t == format!("const/16 {reg}, 0x0")
}

fn const_zero(reg: &str) -> String {
    // const/4 holds v0..v15; use const/16 for higher registers.
    let n: u32 = reg.trim_start_matches('v').parse().unwrap_or(0);
    if n <= 15 {
        format!("const/4 {reg}, 0x0")
    } else {
        format!("const/16 {reg}, 0x0")
    }
}

fn indent_of(line: &str) -> String {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').collect()
}

// ---------------------------------------------------------------------------
// file discovery
// ---------------------------------------------------------------------------

/// All app smali files under build/base/smali*, excluding bundled library namespaces
/// (GMS/Firebase/AndroidX/Kotlin) so we never rewrite the real GMS implementation.
fn app_smali_files(base: &Path) -> R<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(base).map_err(|e| format!("read {}: {e}", base.display()))? {
        let p = entry.map_err(|e| e.to_string())?.path();
        let name = p.file_name().and_then(OsStr::to_str).unwrap_or("");
        if p.is_dir() && name.starts_with("smali") {
            collect_smali(&p, &mut out)?;
        }
    }
    Ok(out)
}

fn is_library_path(p: &Path) -> bool {
    let s = p.to_string_lossy().replace('\\', "/");
    const LIB_PREFIXES: &[&str] = &[
        "/com/google/",
        "/com/android/",
        "/androidx/",
        "/android/support/",
        "/kotlin/",
        "/kotlinx/",
    ];
    LIB_PREFIXES.iter().any(|lp| s.contains(lp))
}

fn collect_smali(dir: &Path, out: &mut Vec<PathBuf>) -> R<()> {
    for entry in fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))? {
        let p = entry.map_err(|e| e.to_string())?.path();
        if p.is_dir() {
            collect_smali(&p, out)?;
        } else if p.extension().and_then(OsStr::to_str) == Some("smali") && !is_library_path(&p) {
            out.push(p);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// misc
// ---------------------------------------------------------------------------

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().into_owned()
}
