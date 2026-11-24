//! `XCL_DXC_PROCESS`: DXC runs in worker processes (`xcl.exe --dxc-worker`) with a memory cap.
//!
//! In XCL's own process a single compile can take all of the machine's commit: DXC's memory is
//! not XCL's to bound, and a translated program of a retail pack drove it past 120 GB (#1598;
//! ~1.2 GB/s while the title loaded its world, #1566); when DXC's allocations fail it may end
//! the process (`LLVM ERROR: out of memory`, 0xC0000409), and in XCL's process that is the
//! title. Each DXC thread of `dxc::pool` gets its own worker process instead, all in one job
//! object that caps each process's memory and ends them with XCL. A compile past the cap fails
//! like any other DXC error (the pipeline keeps its stand-in): DXC reports E_OUTOFMEMORY, or the
//! worker ends. Either way the next compile gets a new worker, and what the old one held goes
//! back to Windows.
//!
//! `XCL_DXC_PROCESS=1` caps each worker at `DEFAULT_CAP_MB`, `XCL_DXC_PROCESS=<MB>` at that;
//! unset or `0`: DXC in XCL's process, as before. `XCL_DXC_WORKER_EXE` names the executable
//! (tests: the `xcl` binary); otherwise it is the running one, which is `xcl.exe` for a title.
//! When no worker starts (or the program is not one), the compile runs in XCL's process.

use std::cell::RefCell;
use std::io::{BufReader, BufWriter, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::OnceLock;

/// A worker's cap. In a retail pack's compute programs, two ~30 MB sources took more than 4 GB
/// in DXC and compiled under 8 GB; only a 150 MB one (a translation `main` no longer makes,
/// #1321) went past it (#1638). The cap is per worker: what bounds all of them together is the
/// free commit each compile waits for before it starts (`dxc::wait_for_memory`).
const DEFAULT_CAP_MB: u64 = 8192;

/// The argument `xcl.exe` takes to serve compiles on stdin / stdout.
pub const WORKER_ARG: &str = "--dxc-worker";

/// What a worker writes first: an executable that is not one (another program, an older XCL)
/// is not used.
const HELLO: &[u8; 8] = b"xcldxc01";

/// The cap of each worker in bytes, or `None`: DXC in this process.
pub fn cap() -> Option<u64> {
    static CAP: OnceLock<Option<u64>> = OnceLock::new();
    *CAP.get_or_init(|| parse_cap(std::env::var("XCL_DXC_PROCESS").ok().as_deref()))
}

fn parse_cap(v: Option<&str>) -> Option<u64> {
    match v.map(str::trim) {
        None | Some("") | Some("0") => None,
        Some("1") => Some(DEFAULT_CAP_MB << 20),
        Some(n) => n.parse::<u64>().ok().filter(|&mb| mb > 0).map(|mb| mb << 20),
    }
}

struct Worker {
    child: Child,
    to: BufWriter<ChildStdin>,
    from: BufReader<ChildStdout>,
}

thread_local! {
    /// the worker of this DXC thread
    static WORKER: RefCell<Option<Worker>> = const { RefCell::new(None) };
}

/// The job object of every worker: each process capped at `cap`, all ended with XCL's process
/// (the handle is never closed).
fn job(cap: u64) -> Result<usize, String> {
    static JOB: OnceLock<Result<usize, String>> = OnceLock::new();
    JOB.get_or_init(|| unsafe {
        use windows_sys::Win32::System::JobObjects::*;
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(format!("CreateJobObject: error {}", std::io::Error::last_os_error()));
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        info.ProcessMemoryLimit = cap as usize;
        if SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const _, std::mem::size_of_val(&info) as u32) == 0 {
            return Err(format!("SetInformationJobObject: {}", std::io::Error::last_os_error()));
        }
        Ok(job as usize)
    })
    .clone()
}

/// `XCL_DXC_WORKER_EXE`, or the running executable (`xcl.exe` for a title).
fn worker_exe() -> Result<std::path::PathBuf, String> {
    std::env::var_os("XCL_DXC_WORKER_EXE").map(std::path::PathBuf::from).map_or_else(std::env::current_exe, Ok).map_err(|e| format!("DXC worker: {e}"))
}

fn spawn(cap: u64, exe: &std::path::Path) -> Result<Worker, String> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    let job = job(cap)?;
    let mut cmd = Command::new(exe);
    cmd.arg(WORKER_ARG).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    // the DXC this process found, not a search of the worker's own
    if let Ok(dir) = super::dxc::location() {
        cmd.env("XCL_DXC_DIR", dir);
    }
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut child = cmd.creation_flags(CREATE_NO_WINDOW).spawn().map_err(|e| format!("DXC worker {}: {e}", exe.display()))?;
    if unsafe { windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(job as _, child.as_raw_handle() as _) } == 0 {
        let e = std::io::Error::last_os_error();
        let _ = child.kill();
        return Err(format!("DXC worker: AssignProcessToJobObject: {e}"));
    }
    let to = BufWriter::new(child.stdin.take().ok_or("DXC worker: no stdin")?);
    let mut from = BufReader::new(child.stdout.take().ok_or("DXC worker: no stdout")?);
    let mut hello = [0u8; 8];
    if from.read_exact(&mut hello).is_err() || &hello != HELLO {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("DXC worker: {} is not one", exe.display()));
    }
    Ok(Worker { child, to, from })
}

fn put(w: &mut impl Write, b: &[u8]) -> std::io::Result<()> {
    w.write_all(&(b.len() as u64).to_le_bytes())?;
    w.write_all(b)
}

fn get(r: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut n = [0u8; 8];
    r.read_exact(&mut n)?;
    let mut b = vec![0u8; u64::from_le_bytes(n) as usize];
    r.read_exact(&mut b)?;
    Ok(b)
}

/// A request: entry, profile, the extra arguments, the source.
fn write_request(w: &mut impl Write, src: &str, entry: &str, profile: &str, extra: &[&str]) -> std::io::Result<()> {
    put(w, entry.as_bytes())?;
    put(w, profile.as_bytes())?;
    w.write_all(&(extra.len() as u32).to_le_bytes())?;
    for a in extra {
        put(w, a.as_bytes())?;
    }
    put(w, src.as_bytes())?;
    w.flush()
}

fn read_request(r: &mut impl Read) -> std::io::Result<(String, String, String, Vec<String>)> {
    let text = |b: Vec<u8>| String::from_utf8(b).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
    let entry = text(get(r)?)?;
    let profile = text(get(r)?)?;
    let mut n = [0u8; 4];
    r.read_exact(&mut n)?;
    let extra = (0..u32::from_le_bytes(n)).map(|_| text(get(r)?)).collect::<std::io::Result<Vec<_>>>()?;
    let src = text(get(r)?)?;
    Ok((src, entry, profile, extra))
}

/// A reply: 0 and the DXIL, or 1 and the error.
fn write_reply(w: &mut impl Write, r: &Result<Vec<u8>, String>) -> std::io::Result<()> {
    match r {
        Ok(b) => {
            w.write_all(&[0])?;
            put(w, b)?;
        }
        Err(e) => {
            w.write_all(&[1])?;
            put(w, e.as_bytes())?;
        }
    }
    w.flush()
}

fn read_reply(r: &mut impl Read) -> std::io::Result<Result<Vec<u8>, String>> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    let b = get(r)?;
    Ok(if tag[0] == 0 { Ok(b) } else { Err(String::from_utf8_lossy(&b).into_owned()) })
}

/// The most memory any worker has committed so far, in bytes (the job's peak per process).
pub fn peak() -> Option<u64> {
    let job = job(cap()?).ok()?;
    unsafe {
        use windows_sys::Win32::System::JobObjects::*;
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        let ok = QueryInformationJobObject(job as _, JobObjectExtendedLimitInformation, &mut info as *mut _ as *mut _, std::mem::size_of_val(&info) as u32, std::ptr::null_mut());
        (ok != 0).then_some(info.PeakProcessMemoryUsed as u64)
    }
}

/// Compile in this thread's worker process (started, or started again, as needed).
pub fn compile(cap: u64, src: &str, entry: &str, profile: &str, extra: &[&str]) -> Result<Vec<u8>, String> {
    compile_with(cap, worker_exe(), src, entry, profile, extra)
}

fn compile_with(cap: u64, exe: Result<std::path::PathBuf, String>, src: &str, entry: &str, profile: &str, extra: &[&str]) -> Result<Vec<u8>, String> {
    WORKER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            match exe.and_then(|exe| spawn(cap, &exe)) {
                Ok(w) => *slot = Some(w),
                // no worker: DXC in this process, as without the setting
                Err(e) => {
                    static ONCE: std::sync::Once = std::sync::Once::new();
                    ONCE.call_once(|| crate::xlog_warn!("{e} – compiling in XCL's process"));
                    return super::dxc::compile_here(src, entry, profile, extra);
                }
            }
        }
        let w = slot.as_mut().unwrap();
        let r = write_request(&mut w.to, src, entry, profile, extra).and_then(|_| read_reply(&mut w.from));
        match r {
            // DXC ran out of the cap and said so: a new worker for the next compile
            Ok(Err(e)) if e.contains("0x8007000E") => {
                let mut w = slot.take().unwrap();
                let _ = w.child.kill();
                let _ = w.child.wait();
                let e = format!("{e} – a {} KB source in a DXC worker process past its {} MB cap", src.len() / 1024, cap >> 20);
                crate::xlog_warn!("{e}");
                Err(e)
            }
            Ok(r) => r,
            Err(_) => {
                // the worker ended: past its cap, or it crashed in DXC; the next compile gets a new one
                let mut w = slot.take().unwrap();
                let code = w.child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
                let e = format!("DXC worker process ended ({:#x}) on a {} KB source: past its {} MB cap, or DXC failed in it", code as u32, src.len() / 1024, cap >> 20);
                crate::xlog_warn!("{e}");
                Err(e)
            }
        }
    })
}

/// `xcl.exe --dxc-worker`: compile what comes on stdin until it closes (on a thread with DXC's
/// stack).
pub fn serve() -> i32 {
    let t = std::thread::Builder::new().stack_size(super::dxc::DXC_STACK).spawn(|| {
        let (stdin, stdout) = (std::io::stdin(), std::io::stdout());
        let (mut r, mut w) = (BufReader::new(stdin.lock()), BufWriter::new(stdout.lock()));
        if w.write_all(HELLO).and_then(|_| w.flush()).is_err() {
            return 1;
        }
        loop {
            let Ok((src, entry, profile, extra)) = read_request(&mut r) else { return 0 };
            let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
            let result = super::dxc::compile_here(&src, &entry, &profile, &extra);
            if write_reply(&mut w, &result).is_err() {
                return 1;
            }
        }
    });
    t.map_or(1, |t| t.join().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_from_the_setting() {
        assert_eq!(parse_cap(None), None);
        assert_eq!(parse_cap(Some("0")), None);
        assert_eq!(parse_cap(Some("")), None);
        assert_eq!(parse_cap(Some("1")), Some(DEFAULT_CAP_MB << 20));
        assert_eq!(parse_cap(Some("4096")), Some(4096 << 20));
        assert_eq!(parse_cap(Some("lots")), None);
    }

    #[test]
    fn without_a_worker_the_compile_runs_here() {
        if super::super::dxc::location().is_err() {
            super::super::runner::skip_test("no DXC");
            return;
        }
        // a thread of its own: no worker from an earlier compile
        let exe = std::path::PathBuf::from(r"C:\nonexistent\xcl-worker.exe");
        let r = std::thread::spawn(move || compile_with(64 << 20, Ok(exe), "[numthreads(1,1,1)] void main() {}", "main", "cs_6_6", &[]))
        .join()
        .unwrap();
        assert!(r.as_ref().is_ok_and(|b| b.starts_with(b"DXBC")), "{:?}", r.err());
        // a program that is not a worker (this test binary takes `--dxc-worker` as a filter):
        // not used either
        let exe = std::env::current_exe().unwrap();
        let r = std::thread::spawn(move || compile_with(64 << 20, Ok(exe), "[numthreads(1,1,1)] void main() {}", "main", "cs_6_6", &[])).join().unwrap();
        assert!(r.as_ref().is_ok_and(|b| b.starts_with(b"DXBC")), "{:?}", r.err());
    }

    #[test]
    fn requests_and_replies_round_trip() {
        let mut buf = Vec::new();
        write_request(&mut buf, "void main() {}", "main", "cs_6_6", &["-O1", "-Zpr"]).unwrap();
        let (src, entry, profile, extra) = read_request(&mut buf.as_slice()).unwrap();
        assert_eq!((src.as_str(), entry.as_str(), profile.as_str()), ("void main() {}", "main", "cs_6_6"));
        assert_eq!(extra, ["-O1", "-Zpr"]);
        for r in [Ok(vec![1u8, 2, 3]), Err("dxc: error".to_string())] {
            let mut buf = Vec::new();
            write_reply(&mut buf, &r).unwrap();
            assert_eq!(read_reply(&mut buf.as_slice()).unwrap(), r);
        }
        // a worker that ended mid-reply reads as an error, not as a short DXIL
        let mut buf = Vec::new();
        write_reply(&mut buf, &Ok(vec![7u8; 64])).unwrap();
        assert!(read_reply(&mut &buf[..20]).is_err());
    }
}
