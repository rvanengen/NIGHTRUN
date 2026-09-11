//! NightRun build/run automation.
//!
//! Commands:
//!   cargo xtask build                       build BOOTX64.EFI and stage the ESP dir
//!   cargo xtask run [opts]                  boot the staged ESP in QEMU + OVMF
//!     --window            show a display window (default: headless)
//!     --network           enable Ethernet/ARP/IPv4/ICMP/UDP only
//!     --mcp               enable networking and the MCP bridge
//!     --mem <size>        guest RAM (default 2G)
//!     --secs <n>          quit QEMU after n seconds
//!     --shot <t>:<path>   screendump PNG at t seconds (repeatable)
//!     --keys <t>:<text>   type text at t seconds (repeatable; "\n" = Enter)

mod image;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("build") => {
            build_and_stage(arch_flag(&args), stack_mode(&args));
        }
        Some("image") => {
            let model = args
                .iter()
                .position(|a| a == "--model")
                .map(|i| args[i + 1].clone());
            build_image(true, model.as_deref(), arch_flag(&args), stack_mode(&args));
        }
        Some("run") => run(parse_run_opts(&args[1..])),
        Some("bench") => bench(),
        Some("pi-image") => {
            let model = args
                .iter()
                .position(|a| a == "--model")
                .map(|i| args[i + 1].clone());
            pi_image(model.as_deref(), stack_mode(&args));
        }
        _ => {
            eprintln!("usage: cargo xtask <build|image|run|bench|pi-image> [options]");
            std::process::exit(2);
        }
    }
}

/// Boot the image in QEMU, run a scripted prompt, and record measured
/// numbers from the serial log into docs/benchmarks.md.
fn bench() {
    let root = root();
    let opts = parse_run_opts(
        &[
            "--img",
            "--mem",
            "4G",
            "--smp",
            "8",
            "--secs",
            "150",
            "--keys",
            "35:Explain what a rotary positional embedding is in two sentences.\\n",
        ]
        .map(String::from),
    );
    run(opts);

    let log = std::fs::read_to_string(root.join("target/serial.log")).expect("serial log");
    let grab = |pat: &str| -> Option<String> {
        log.lines()
            .find(|l| l.contains(pat))
            .map(|l| l.trim().to_string())
    };
    let mut out = String::from("## Bench snapshot (cargo xtask bench)\n\n");
    out.push_str("Environment: QEMU q35, KVM, `-cpu max -smp 8 -m 4G`, OVMF; ");
    out.push_str(&format!(
        "host: {} hardware threads.\nDate: {}\n\n```\n",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        String::from_utf8_lossy(
            &Command::new("date")
                .arg("+%Y-%m-%d")
                .output()
                .unwrap()
                .stdout
        )
        .trim(),
    ));
    for pat in [
        "[smp]",
        "workers active",
        // One line since streaming verification landed:
        // "[boot] model loaded + verified (streaming CRC) in N ms"
        "model loaded",
        "chat-ready in",
        "prefill",
        "milli-tok/s",
    ] {
        if let Some(line) = grab(pat) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out.push_str("```\n");
    // benchmarks.md is a curated measurement log (Pi rows, comparisons,
    // context-decay notes). Append a dated snapshot; never clobber it.
    std::fs::create_dir_all(root.join("docs")).unwrap();
    let path = root.join("docs/benchmarks.md");
    let mut existing = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| String::from("# NightRun benchmarks (measured)\n"));
    if !existing.ends_with('\n') {
        existing.push('\n');
    }
    existing.push_str("\n");
    existing.push_str(&out);
    std::fs::write(&path, &existing).unwrap();
    println!("--- appended snapshot to docs/benchmarks.md:\n{out}");
}

/// Build nightrun.img with the given model (default models/model.nrm).
/// When `fresh` is false and the image already contains this exact model,
/// only BOOTX64.EFI is refreshed.
fn build_image(fresh: bool, model_arg: Option<&str>, arch: Arch, mode: StackMode) -> PathBuf {
    let root = root();
    let (_, efi) = build_and_stage(arch, mode);
    let img = root.join(match arch {
        Arch::X86 => "nightrun.img",
        Arch::Aarch64 => "nightrun-aarch64.img",
    });
    let model = root.join(model_arg.unwrap_or("models/model.nrm"));
    let model = model.exists().then_some(model);
    if model.is_none() {
        println!("note: model file missing - building image without model");
    }

    // Sidecar records which model the image holds, so switching models
    // forces a full rebuild instead of a stale EFI-only update.
    let sidecar = root.join(match arch {
        Arch::X86 => "target/image-model.txt",
        Arch::Aarch64 => "target/image-model-aarch64.txt",
    });
    let stamp = model
        .as_ref()
        .map(|m| {
            let sz = std::fs::metadata(m).map(|md| md.len()).unwrap_or(0);
            format!("{}|{sz}", m.display())
        })
        .unwrap_or_default()
        + mode.stamp();
    let same_model = std::fs::read_to_string(&sidecar)
        .map(|s| s == stamp)
        .unwrap_or(false);

    if !fresh && same_model && img.exists() && image::update_efi(&img, &efi, arch.boot_file()) {
        println!("updated {} in {}", arch.boot_file(), img.display());
        return img;
    }
    image::build(&img, &efi, model.as_deref(), arch.boot_file());
    let _ = std::fs::write(&sidecar, stamp);
    img
}

/// Build the flashable Raspberry Pi 5 SD image (MBR + FAT32: firmware
/// payload, BOOTAA64.EFI, model). Default model: Granite 3B (fits the
/// 4 GB board; Qwen3 4B needs 8 GB+).
fn pi_image(model_arg: Option<&str>, mode: StackMode) {
    let root = root();
    let (_, efi) = build_and_stage(Arch::Aarch64, mode);
    let model = root.join(model_arg.unwrap_or("models/granite-4.1-3b-q4km.nrm"));
    let model = model.exists().then_some(model);
    if model.is_none() {
        println!("note: model file missing - building image without model");
    }
    let firmware = root.join("vendor/rpi5-uefi");
    image::build_pi(
        &root.join("nightrun-pi5.img"),
        &efi,
        model.as_deref(),
        &firmware,
    );
}

/// --arch flag for the simple subcommands (run parses its own).
fn arch_flag(args: &[String]) -> Arch {
    args.iter()
        .position(|a| a == "--arch")
        .map(|i| Arch::parse(&args[i + 1]))
        .unwrap_or_default()
}

fn stack_mode(args: &[String]) -> StackMode {
    if args.iter().any(|arg| arg == "--mcp") {
        StackMode::Mcp
    } else if args.iter().any(|arg| arg == "--network") {
        StackMode::Network
    } else {
        StackMode::Offline
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn build_and_stage(arch: Arch, mode: StackMode) -> (PathBuf, PathBuf) {
    let root = root();
    let efi = match arch {
        Arch::X86 => {
            // Custom hard-float UEFI target (the builtin one is soft-float,
            // which both breaks AVX intrinsics and would cripple f32 math),
            // so build core/alloc from source on nightly.
            let mut command = Command::new("cargo");
            command.current_dir(&root).env_remove("CARGO").args([
                "+nightly",
                "build",
                "--release",
                "-p",
                "nr-boot",
                "-Zbuild-std=core,alloc",
                "-Zjson-target-spec",
                "--target",
                "x86_64-nightrun-uefi.json",
            ]);
            if let Some(feature) = mode.cargo_feature() {
                command.args(["--features", feature]);
            }
            let status = command
                .status()
                .expect("run cargo (is nightly installed? rustup toolchain install nightly --component rust-src)");
            assert!(status.success(), "nr-boot build failed");
            root.join("target/x86_64-nightrun-uefi/release/nr-boot.efi")
        }
        Arch::Aarch64 => {
            // Stock tier-2 target: hard-float + NEON baseline, stable
            // toolchain, no build-std.
            let mut command = Command::new("cargo");
            command.current_dir(&root).args([
                "build",
                "--release",
                "-p",
                "nr-boot",
                "--target",
                "aarch64-unknown-uefi",
            ]);
            if let Some(feature) = mode.cargo_feature() {
                command.args(["--features", feature]);
            }
            let status = command
                .status()
                .expect("run cargo (rustup target add aarch64-unknown-uefi)");
            assert!(status.success(), "nr-boot aarch64 build failed");
            root.join("target/aarch64-unknown-uefi/release/nr-boot.efi")
        }
    };

    let esp = root.join(match arch {
        Arch::X86 => "target/esp",
        Arch::Aarch64 => "target/esp-aarch64",
    });
    let boot_dir = esp.join("EFI/BOOT");
    std::fs::create_dir_all(&boot_dir).unwrap();
    std::fs::copy(&efi, boot_dir.join(arch.boot_file())).unwrap();
    println!("staged {}", esp.display());
    (esp, efi)
}

/// Target architecture for build/image/run. x86_64 keeps its custom
/// hard-float target + nightly build-std; aarch64 uses the stock
/// hard-float `aarch64-unknown-uefi` target on stable.
#[derive(Clone, Copy, PartialEq, Default)]
enum Arch {
    #[default]
    X86,
    Aarch64,
}

impl Arch {
    fn parse(v: &str) -> Arch {
        match v {
            "x86_64" | "x86" => Arch::X86,
            "aarch64" | "arm64" => Arch::Aarch64,
            other => panic!("unknown --arch {other} (x86_64|aarch64)"),
        }
    }

    fn boot_file(self) -> &'static str {
        match self {
            Arch::X86 => "BOOTX64.EFI",
            Arch::Aarch64 => "BOOTAA64.EFI",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Default)]
enum StackMode {
    #[default]
    Offline,
    Network,
    Mcp,
}

impl StackMode {
    fn cargo_feature(self) -> Option<&'static str> {
        match self {
            Self::Offline => None,
            Self::Network => Some("network"),
            Self::Mcp => Some("mcp"),
        }
    }

    fn stamp(self) -> &'static str {
        match self {
            Self::Offline => "|offline",
            Self::Network => "|network",
            Self::Mcp => "|network+mcp",
        }
    }

    fn has_network(self) -> bool {
        !matches!(self, Self::Offline)
    }
}

#[derive(Default)]
struct RunOpts {
    arch: Arch,
    window: bool,
    img: bool,
    mode: StackMode,
    model: Option<String>,
    mem: Option<String>,
    smp: Option<String>,
    secs: Option<u64>,
    shots: Vec<(u64, String)>,
    keys: Vec<(u64, String)>,
}

fn parse_run_opts(args: &[String]) -> RunOpts {
    let mut o = RunOpts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().expect("missing value").clone();
        match a.as_str() {
            "--arch" => o.arch = Arch::parse(&val()),
            "--window" => o.window = true,
            "--img" => o.img = true,
            "--network" => {
                if o.mode == StackMode::Offline {
                    o.mode = StackMode::Network;
                }
            }
            "--mcp" => o.mode = StackMode::Mcp,
            "--model" => o.model = Some(val()),
            "--mem" => o.mem = Some(val()),
            "--smp" => o.smp = Some(val()),
            "--secs" => o.secs = Some(val().parse().unwrap()),
            "--shot" => {
                let v = val();
                let (t, p) = v.split_once(':').expect("--shot t:path");
                o.shots.push((t.parse().unwrap(), p.into()));
            }
            "--keys" => {
                let v = val();
                let (t, k) = v.split_once(':').expect("--keys t:text");
                o.keys.push((t.parse().unwrap(), k.replace("\\n", "\n")));
            }
            other => panic!("unknown option {other}"),
        }
    }
    o
}

fn run(opts: RunOpts) {
    let root = root();
    // --img boots the real GPT/FAT32 image (required for the model, which
    // exceeds QEMU's virtual-FAT limits); the default boots the staged ESP
    // directory for a fast dev loop.
    let boot_drive = if opts.img {
        let img = build_image(false, opts.model.as_deref(), opts.arch, opts.mode);
        format!("format=raw,file={}", img.display())
    } else {
        let (esp, _) = build_and_stage(opts.arch, opts.mode);
        format!("format=raw,file=fat:rw:{}", esp.display())
    };
    let target = root.join("target");

    let qmp_sock = target.join("qmp.sock");
    let _ = std::fs::remove_file(&qmp_sock);
    let serial_log = target.join("serial.log");

    let mut cmd;
    match opts.arch {
        Arch::X86 => {
            let ovmf_code = "/usr/share/OVMF/OVMF_CODE_4M.fd";
            // Fresh vars every run: stale boot entries (e.g. from a run
            // with different media attached) can send the firmware down
            // PXE instead of our drive.
            let vars = target.join("OVMF_VARS.fd");
            std::fs::copy("/usr/share/OVMF/OVMF_VARS_4M.fd", &vars).expect("copy OVMF vars");
            cmd = Command::new("qemu-system-x86_64");
            cmd.current_dir(&root)
                .args(["-machine", "q35"])
                .args(["-accel", "kvm", "-accel", "tcg"])
                .args(["-cpu", "max"])
                .args(["-m", opts.mem.as_deref().unwrap_or("2G")])
                .args(["-smp", opts.smp.as_deref().unwrap_or("8")])
                .args([
                    "-drive",
                    &format!("if=pflash,format=raw,readonly=on,file={ovmf_code}"),
                ])
                .args([
                    "-drive",
                    &format!("if=pflash,format=raw,file={}", vars.display()),
                ])
                .args(["-drive", &boot_drive]);
        }
        Arch::Aarch64 => {
            // Generic aarch64 UEFI machine under TCG (the host is x86;
            // QEMU cannot emulate a Pi 5 — this validates arch
            // correctness, not Pi hardware). AAVMF is Ubuntu's packaged
            // ARM64 EDK2.
            let vars = target.join("AAVMF_VARS.fd");
            std::fs::copy("/usr/share/AAVMF/AAVMF_VARS.fd", &vars).expect("copy AAVMF vars");
            cmd = Command::new("qemu-system-aarch64");
            cmd.current_dir(&root)
                .args(["-machine", "virt"])
                .args(["-accel", "tcg,thread=multi"])
                .args(["-cpu", "cortex-a76"])
                .args(["-m", opts.mem.as_deref().unwrap_or("3G")])
                .args(["-smp", opts.smp.as_deref().unwrap_or("4")])
                .args([
                    "-drive",
                    "if=pflash,format=raw,readonly=on,file=/usr/share/AAVMF/AAVMF_CODE.fd",
                ])
                .args([
                    "-drive",
                    &format!("if=pflash,format=raw,file={}", vars.display()),
                ])
                // ramfb: plain linear framebuffer through AAVMF's GOP
                // (virtio-gpu is Blt-only, which NightRun's direct-write
                // renderer rejects; the Pi's real GOP is linear).
                .args(["-device", "ramfb"])
                .args(["-device", "qemu-xhci", "-device", "usb-kbd"])
                .args(["-drive", &format!("if=none,id=boot,{boot_drive}")])
                .args(["-device", "virtio-blk-pci,drive=boot"]);
        }
    }
    if opts.mode.has_network() {
        cmd.args(["-netdev", "user,id=nightrun"]);
        match opts.arch {
            Arch::X86 => {
                cmd.args(["-device", "e1000,netdev=nightrun"]);
            }
            Arch::Aarch64 => {
                cmd.args(["-device", "virtio-net-pci,netdev=nightrun"]);
            }
        }
    }
    cmd.args(["-serial", &format!("file:{}", serial_log.display())])
        .args([
            "-qmp",
            &format!("unix:{},server=on,wait=off", qmp_sock.display()),
        ])
        .args(["-monitor", "none"]);
    if !opts.window {
        cmd.arg("-display").arg("none");
    }
    println!(
        "qemu: {:?}",
        cmd.get_args().collect::<Vec<_>>().join(" ".as_ref())
    );
    let mut child = cmd
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn qemu");

    let mut qmp = Qmp::connect(&qmp_sock, Duration::from_secs(10));

    // Timeline of scheduled actions.
    let mut events: Vec<(u64, Event)> = Vec::new();
    for (t, p) in &opts.shots {
        events.push((*t, Event::Shot(p.clone())));
    }
    for (t, k) in &opts.keys {
        events.push((*t, Event::Keys(k.clone())));
    }
    events.sort_by_key(|(t, _)| *t);

    let start = Instant::now();
    for (t, ev) in events {
        let due = Duration::from_secs(t);
        if let Some(rem) = due.checked_sub(start.elapsed()) {
            std::thread::sleep(rem);
        }
        match ev {
            Event::Shot(path) => {
                if let Some(qmp) = qmp.as_mut() {
                    let abs = root.join(&path);
                    qmp.screendump(abs.to_str().unwrap());
                    println!("screendump -> {path}");
                }
            }
            Event::Keys(text) => {
                if let Some(qmp) = qmp.as_mut() {
                    qmp.type_text(&text);
                    println!("typed {text:?}");
                }
            }
        }
    }

    if let Some(secs) = opts.secs {
        if let Some(rem) = Duration::from_secs(secs).checked_sub(start.elapsed()) {
            std::thread::sleep(rem);
        }
        if let Some(qmp) = qmp.as_mut() {
            qmp.cmd(r#"{"execute":"quit"}"#);
        }
        wait_or_kill(&mut child, Duration::from_secs(5));
    } else {
        let _ = child.wait();
    }

    if let Ok(log) = std::fs::read_to_string(&serial_log) {
        let tail: Vec<&str> = log.lines().rev().take(30).collect();
        println!("--- serial tail ---");
        for line in tail.iter().rev() {
            println!("{line}");
        }
    }
}

enum Event {
    Shot(String),
    Keys(String),
}

fn wait_or_kill(child: &mut Child, timeout: Duration) {
    let start = Instant::now();
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct Qmp {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Qmp {
    fn connect(path: &Path, timeout: Duration) -> Option<Qmp> {
        let start = Instant::now();
        let stream = loop {
            match UnixStream::connect(path) {
                Ok(s) => break s,
                Err(_) if start.elapsed() < timeout => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    eprintln!("qmp connect failed: {e}");
                    return None;
                }
            }
        };
        let writer = stream.try_clone().unwrap();
        let mut qmp = Qmp {
            reader: BufReader::new(stream),
            writer,
        };
        qmp.read_line(); // greeting
        qmp.cmd(r#"{"execute":"qmp_capabilities"}"#);
        Some(qmp)
    }

    fn read_line(&mut self) -> String {
        let mut line = String::new();
        let _ = self.reader.read_line(&mut line);
        line
    }

    /// Send a command and read until its response (skipping async events).
    fn cmd(&mut self, json: &str) -> String {
        let _ = writeln!(self.writer, "{json}");
        loop {
            let line = self.read_line();
            if line.is_empty() || line.contains("\"return\"") || line.contains("\"error\"") {
                return line;
            }
        }
    }

    fn screendump(&mut self, path: &str) {
        let resp = self.cmd(&format!(
            r#"{{"execute":"screendump","arguments":{{"filename":"{path}","format":"png"}}}}"#
        ));
        if resp.contains("\"error\"") {
            eprintln!("screendump failed: {resp}");
        }
    }

    fn sendkey(&mut self, key: &str) {
        self.cmd(&format!(
            r#"{{"execute":"human-monitor-command","arguments":{{"command-line":"sendkey {key}"}}}}"#
        ));
        std::thread::sleep(Duration::from_millis(35));
    }

    fn type_text(&mut self, text: &str) {
        // Special keys spelled as tokens, e.g. "<up><up><down>".
        let mut rest = text;
        while let Some(i) = rest.find('<') {
            let (head, tail) = rest.split_at(i);
            self.type_plain(head);
            if let Some(end) = tail.find('>') {
                let key = match &tail[1..end] {
                    "up" => "up",
                    "down" => "down",
                    "pgup" => "pgup",
                    "pgdn" => "pgdn",
                    "esc" => "esc",
                    "left" => "left",
                    "right" => "right",
                    "del" => "delete",
                    other => {
                        eprintln!("unknown key token <{other}>");
                        ""
                    }
                };
                if !key.is_empty() {
                    self.sendkey(key);
                }
                rest = &tail[end + 1..];
            } else {
                self.type_plain(tail);
                return;
            }
        }
        self.type_plain(rest);
    }

    fn type_plain(&mut self, text: &str) {
        for ch in text.chars() {
            let key = match ch {
                'a'..='z' | '0'..='9' => ch.to_string(),
                'A'..='Z' => format!("shift-{}", ch.to_ascii_lowercase()),
                ' ' => "spc".into(),
                '\n' => "ret".into(),
                '.' => "dot".into(),
                ',' => "comma".into(),
                '-' => "minus".into(),
                '_' => "shift-minus".into(),
                '/' => "slash".into(),
                '?' => "shift-slash".into(),
                '!' => "shift-1".into(),
                ':' => "shift-semicolon".into(),
                ';' => "semicolon".into(),
                '\'' => "apostrophe".into(),
                _ => continue,
            };
            self.sendkey(&key);
        }
    }
}
