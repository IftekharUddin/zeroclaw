//! Harmless native kernel stand-in for desktop ownership tests and the manual
//! Quit UI harness. No network, user configuration, or real daemon is touched.

#[cfg(windows)]
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn stop_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

pub fn run_if_fixture() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if !matches!(
        args.first().map(String::as_str),
        Some("service" | "--descendant" | "--external")
    ) {
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap();
    if args.iter().any(|arg| arg == "--help") {
        if dir.join("gate-probe").exists() {
            std::fs::write(dir.join("probe-started"), b"capability probe").unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while dir.join("gate-probe").exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        std::process::exit(0);
    }
    if args[0] == "--descendant" || args[0] == "--external" {
        let deadline = Instant::now() + Duration::from_secs(600);
        while !dir.join("stop").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::process::exit(0);
    }
    let mode = args.windows(2).find(|pair| pair[0] == "--port").unwrap()[1].clone();
    #[cfg(unix)]
    // SAFETY: the handler only stores a lock-free atomic flag.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            stop_signal as *const () as libc::sighandler_t,
        );
    }
    #[cfg(windows)]
    std::thread::spawn(|| {
        let mut byte = [0];
        while std::io::stdin().read_exact(&mut byte).is_ok() {
            if byte == [0x03] {
                STOP.store(true, Ordering::Relaxed);
                break;
            }
        }
        // EOF does not stop a fixture: No and app crash must keep it alive.
    });
    let mut descendant = Command::new(&exe)
        .arg("--descendant")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::fs::write(dir.join("supervisor.pid"), std::process::id().to_string()).unwrap();
    std::fs::write(dir.join("descendant.pid"), descendant.id().to_string()).unwrap();
    if mode != "3" {
        println!("READY");
        std::io::stdout().flush().unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        if STOP.load(Ordering::Relaxed) && mode != "2" {
            std::fs::write(dir.join("graceful-stop"), b"explicit graceful stop").unwrap();
            break;
        }
        if dir.join("stop").exists() || Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = descendant.kill();
    let _ = descendant.wait();
    std::process::exit(0);
}

pub struct Fixture {
    pub dir: PathBuf,
    pub binary: PathBuf,
}

impl Fixture {
    pub fn new(parent: &Path) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = parent.join(format!("owned-kernel-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join(if cfg!(windows) {
            "kernel.exe"
        } else {
            "kernel"
        });
        std::fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
        Self { dir, binary }
    }

    pub fn wait_for(&self, name: &str) -> PathBuf {
        let file = self.dir.join(name);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !file.exists() {
            assert!(Instant::now() < deadline, "fixture did not write {file:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
        file
    }

    pub fn pid(&self, name: &str) -> u32 {
        std::fs::read_to_string(self.wait_for(name))
            .unwrap()
            .parse()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // A panic, or a keep-running test, still leaves a cooperative cleanup
        // request for this fixture alone. Preserve its evidence directory.
        let _ = std::fs::write(self.dir.join("stop"), b"fixture cleanup");
    }
}

pub fn running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // Observation only; production ownership never discovers a PID.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // Inspect only the PID written by our own fixture; this grants no stop
        // authority and is not part of the production process registry.
        let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
        else {
            return false;
        };
        let mut code = 0;
        let result = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok() && code == 259;
        let _ = unsafe { CloseHandle(handle) };
        result
    }
}

pub fn wait_stopped(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        #[cfg(unix)]
        // No/drop intentionally released the Child handle without reaping. Our
        // cooperative cleanup may leave that direct fixture child a zombie;
        // reap it here, after production has released ownership, not in running().
        unsafe {
            if libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) > 0 {
                return;
            }
        }
        if !running(pid) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "fixture process {pid} is still alive"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
