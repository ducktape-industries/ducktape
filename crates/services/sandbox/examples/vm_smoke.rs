//! boot ONE microVM end to end and prove the whole contract on this host:
//! image build, VMM spawn, guest dial-back, stdio frames, exit code, and the
//! workspace read-back. The smallest thing that fails if the sandbox breaks —
//! and the first thing to run when bringing the backend up on a new OS.
//!
//!   cargo run -p sandbox-host --example vm_smoke -- \
//!       --kernel <workspace>/guest/vmlinux \
//!       --rootfs <workspace>/guest/rootfs.ext4 \
//!       [--vmm firecracker|vz] [--exec '<shell command>']
//!
//! `--vmm` defaults to this OS's flavor (vz on macOS, Firecracker elsewhere).
//! `--exec` runs a command of your own inside the guest FIRST — what an image
//! carries is a question you ask by running something against it — and the
//! probe still follows it. Exit 0 means a guest booted, ran `/bin/sh`, wrote a
//! file into its workspace, and the file came back to the host.

use std::path::PathBuf;

use sandbox_host::firecracker_api::{self, VmConfig};
use sandbox_host::guest_manifest::RunManifest;
use sandbox_host::guest_paths::{GUEST_HOME, GUEST_PATH, GUEST_WORKSPACE};
use sandbox_host::microvm::ScratchDir;
use sandbox_host::{MicroVm, Vmm};

/// what the guest runs before the probe when `--exec` names nothing: read the
/// one file the host put in the workspace.
const DEFAULT_EXEC: &str = "cat input.txt";

fn parse_args() -> Result<(PathBuf, PathBuf, Vmm, String), String> {
    let mut kernel = None;
    let mut rootfs = None;
    let mut vmm = Vmm::platform_default();
    let mut exec = DEFAULT_EXEC.to_string();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--kernel" => kernel = Some(PathBuf::from(value)),
            "--rootfs" => rootfs = Some(PathBuf::from(value)),
            "--exec" => exec = value,
            "--vmm" => {
                vmm = match value.as_str() {
                    "firecracker" => Vmm::Firecracker,
                    "vz" => Vmm::Vz,
                    other => return Err(format!("unknown vmm {other:?}")),
                }
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    let kernel = kernel.ok_or("--kernel is required")?;
    let rootfs = rootfs.ok_or("--rootfs is required")?;
    Ok((kernel, rootfs, vmm, exec))
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    if let Err(error) = smoke().await {
        eprintln!("vm_smoke: {error}");
        std::process::exit(1);
    }
    println!("vm_smoke: OK");
}

async fn smoke() -> Result<(), String> {
    let (kernel, rootfs, vmm, exec) = parse_args()?;

    // scratch: a workspace with one input file, a run dir, and a SHORT socket
    // dir (`SUN_LEN` caps a unix socket path near 104 bytes on macOS).
    let slot = format!("smoke-{}", std::process::id());
    let workdir = std::env::temp_dir().join(format!("dt-smoke-ws-{}", std::process::id()));
    std::fs::create_dir_all(&workdir).map_err(|e| e.to_string())?;
    std::fs::write(workdir.join("input.txt"), b"from the host\n").map_err(|e| e.to_string())?;
    // owned by guards until the VM exists: a smoke run that fails to boot
    // must leave no scratch behind either.
    let run_scratch = ScratchDir::create(std::env::temp_dir().join(format!("dt-vm-{slot}")))?;
    let socket_base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let socket_scratch = ScratchDir::create(socket_base.join(format!("dt-vm-{slot}")))?;
    let run_dir = run_scratch.path().to_path_buf();
    let socket_dir = socket_scratch.path().to_path_buf();

    let cfg = VmConfig {
        vmm,
        kernel,
        rootfs,
        manifest: run_dir.join("manifest.bin"),
        agent_volume: None,
        assets: run_dir.join("assets.ext4"),
        // this run execs `/bin/sh` from the rootfs — there is no agent CLI to lend.
        executors: None,
        workspace: run_dir.join("workspace.ext4"),
        vcpus: 1,
        mem_mib: 512,
        vsock_uds: socket_dir.join("v.sock"),
        tap: None,
    };
    // `--exec` is a PREFIX, never a replacement: whatever the run does, it ends
    // by writing the probe file and printing the probe line, because those two
    // are what tell "the guest ran to completion" apart from "the VM died
    // somewhere in the middle" — and every assertion below rests on them.
    // by ABSOLUTE path: a command that ends somewhere else (a build runs from
    // the directory it cloned into) would otherwise drop the probe file there
    // and fail the read-back for a reason that has nothing to do with the run.
    let script =
        format!("{exec} && echo smoke > {GUEST_WORKSPACE}/vm-smoke.txt && echo guest-side-ok");
    let manifest = RunManifest {
        argv: vec!["/bin/sh".into(), "-c".into(), script],
        // the REAL run environment, not a hand-written subset of it: this
        // example is what answers "does this image work", and it can only
        // answer for the paths a run actually searches. A narrower PATH here
        // hides everything the image installs into /usr/local/bin.
        env: vec![
            ("PATH".into(), GUEST_PATH.into()),
            ("HOME".into(), GUEST_HOME.into()),
        ],
        cwd: GUEST_WORKSPACE.into(),
        mounts: firecracker_api::manifest_mounts(&cfg),
        tunnel_ports: vec![],
        pty: false,
    };

    let (vm, mut io) = MicroVm::boot(&run_dir, &workdir, &[], &cfg, &manifest).await?;
    // the VM's own Drop removes both directories from here.
    run_scratch.disarm();
    socket_scratch.disarm();

    // no stdin for this run; dropping the handle sends the EOF frame.
    drop(io.stdin);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    tokio::io::copy(&mut io.stdout, &mut stdout)
        .await
        .map_err(|e| format!("read guest stdout: {e}"))?;
    tokio::io::copy(&mut io.stderr, &mut stderr)
        .await
        .map_err(|e| format!("read guest stderr: {e}"))?;
    // On a post-boot failure the console is the only witness, and the VM's
    // Drop removes it with the run dir — so read it BEFORE bailing out.
    let console_tail = || {
        let raw = std::fs::read_to_string(vm.run_dir().join("console.log")).unwrap_or_default();
        let tail: Vec<&str> = raw.lines().rev().take(25).collect();
        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    };
    let exit = io.exit.await.map_err(|_| {
        format!(
            "the guest halted without reporting an exit code\nguest console:\n{}",
            console_tail()
        )
    })?;

    print!("{}", String::from_utf8_lossy(&stdout));
    eprint!("{}", String::from_utf8_lossy(&stderr));
    if exit != 0 {
        return Err(format!("guest exit code {exit}"));
    }
    if !stdout.ends_with(b"guest-side-ok\n") {
        return Err("guest stdout did not end with the probe line".into());
    }

    vm.collect(&workdir).await?;
    let echoed = std::fs::read_to_string(workdir.join("vm-smoke.txt"))
        .map_err(|e| format!("the guest's write never came back: {e}"))?;
    if echoed.trim() != "smoke" {
        return Err(format!("read back {echoed:?}, expected \"smoke\""));
    }

    let _ = std::fs::remove_dir_all(&workdir);
    Ok(())
}
