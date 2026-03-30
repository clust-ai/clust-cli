use std::fs::File;
use std::io;
use std::process::{Command, Stdio};

/// Spawn `clust-hub` as a detached background process.
pub fn spawn_hub() -> io::Result<()> {
    // Resolve clust-hub binary path relative to the current executable.
    // During development (cargo build/run), both binaries are in the same
    // target/{debug,release}/ directory. When installed, both are co-located
    // in the same directory (e.g. ~/.clust/bin/).
    let current_exe = std::env::current_exe()?;
    let bin_dir = current_exe
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cannot determine bin directory"))?;
    let hub_bin = bin_dir.join("clust-hub");

    if !hub_bin.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("clust-hub binary not found at {}", hub_bin.display()),
        ));
    }

    // Ensure ~/.clust/ exists before opening the log file
    std::fs::create_dir_all(clust_ipc::clust_dir())?;

    // Redirect stderr to a log file so hub errors are captured.
    // Truncates on each hub start (old session logs are stale).
    let log_file = File::create(clust_ipc::log_path())?;

    let mut cmd = Command::new(&hub_bin);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file));

    // Detach into own process group so it survives terminal close
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    cmd.spawn()?;

    Ok(())
}
