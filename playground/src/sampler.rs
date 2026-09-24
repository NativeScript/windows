//! Dev-only in-process sampling profiler: `NSWIN_SAMPLE=1` samples the JS thread for the whole
//! run and prints the hottest functions (self and inclusive) to stderr at exit.
//!
//! ETW-based profilers (samply, WPR) need Administrator on Windows; this needs nothing. A sampler
//! thread suspends the target every ~1ms, unwinds its stack with `RtlVirtualUnwind`, resumes it,
//! and symbolizes afterwards. Nothing is allocated while the target is suspended. It may be
//! holding the heap lock. JIT frames without unwind info end a stack early; the native frames
//! (the interop path under study) are what matters here.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
use windows::Win32::System::Diagnostics::Debug::{
    GetThreadContext, RtlLookupFunctionEntry, RtlVirtualUnwind, CONTEXT, CONTEXT_FULL_AMD64,
    UNW_FLAG_NHANDLER,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetCurrentThreadStackLimits, ResumeThread, SuspendThread,
};

const DEPTH: usize = 32;
const MAX_SAMPLES: usize = 400_000;

type Stack = [u64; DEPTH];

pub struct Sampler {
    stop: Arc<AtomicBool>,
    worker: JoinHandle<Vec<Stack>>,
}

struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}

/// Start sampling the calling thread when `NSWIN_SAMPLE` is set.
pub fn start_if_requested() -> Option<Sampler> {
    std::env::var_os("NSWIN_SAMPLE")?;
    let mut target = HANDLE::default();
    let (mut low, mut high) = (0usize, 0usize);
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentThread(),
            GetCurrentProcess(),
            &mut target,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
        .ok()?;
        GetCurrentThreadStackLimits(&mut low, &mut high);
    }
    let target = SendHandle(target);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let worker = std::thread::spawn(move || {
        let target = target;
        let mut samples: Vec<Stack> = Vec::with_capacity(MAX_SAMPLES);
        while !stop_flag.load(Ordering::Relaxed) && samples.len() < MAX_SAMPLES {
            std::thread::sleep(std::time::Duration::from_millis(1));
            let mut stack: Stack = [0; DEPTH];
            unsafe {
                if SuspendThread(target.0) == u32::MAX {
                    break;
                }
                capture(target.0, low as u64, high as u64, &mut stack);
                ResumeThread(target.0);
            }
            if stack[0] != 0 {
                samples.push(stack);
            }
        }
        samples
    });
    Some(Sampler { stop, worker })
}

unsafe fn capture(thread: HANDLE, low: u64, high: u64, out: &mut Stack) {
    let mut ctx = CONTEXT {
        ContextFlags: CONTEXT_FULL_AMD64,
        ..Default::default()
    };
    if unsafe { GetThreadContext(thread, &mut ctx) }.is_err() {
        return;
    }
    for slot in out.iter_mut() {
        if ctx.Rip == 0 || ctx.Rsp < low || ctx.Rsp + 8 > high {
            break;
        }
        *slot = ctx.Rip;
        let mut image_base = 0u64;
        let entry = unsafe { RtlLookupFunctionEntry(ctx.Rip, &mut image_base, None) };
        if entry.is_null() {
            // Leaf function (no unwind info): the return address is at the top of the stack.
            ctx.Rip = unsafe { *(ctx.Rsp as *const u64) };
            ctx.Rsp += 8;
        } else {
            let mut handler_data = std::ptr::null_mut();
            let mut establisher = 0u64;
            unsafe {
                RtlVirtualUnwind(
                    UNW_FLAG_NHANDLER,
                    image_base,
                    ctx.Rip,
                    entry,
                    &mut ctx,
                    &mut handler_data,
                    &mut establisher,
                    None,
                )
            };
        }
    }
}

fn symbol_name(ip: u64, cache: &mut HashMap<u64, String>) -> String {
    cache
        .entry(ip)
        .or_insert_with(|| {
            let mut name = None;
            backtrace::resolve(ip as *mut std::ffi::c_void, |sym| {
                if name.is_none() {
                    name = sym.name().map(|n| format!("{n:#}"));
                }
            });
            name.unwrap_or_else(|| format!("0x{ip:x}"))
        })
        .clone()
}

impl Sampler {
    pub fn finish(self) {
        self.stop.store(true, Ordering::Relaxed);
        let samples = self.worker.join().unwrap_or_default();
        let total = samples.len().max(1) as f64;
        let mut cache = HashMap::new();
        let mut self_counts: HashMap<String, usize> = HashMap::new();
        let mut incl_counts: HashMap<String, usize> = HashMap::new();
        for stack in &samples {
            let mut seen = HashSet::new();
            for (depth, &ip) in stack.iter().take_while(|&&ip| ip != 0).enumerate() {
                // Return addresses point after the call; step back into it for symbolization.
                let name = symbol_name(if depth == 0 { ip } else { ip - 1 }, &mut cache);
                if depth == 0 {
                    *self_counts.entry(name.clone()).or_default() += 1;
                }
                if seen.insert(name.clone()) {
                    *incl_counts.entry(name).or_default() += 1;
                }
            }
        }
        let print = |title: &str, counts: &HashMap<String, usize>| {
            let mut rows: Vec<_> = counts.iter().collect();
            rows.sort_by(|a, b| b.1.cmp(a.1));
            eprintln!("\n[sampler] {title} ({} samples)", samples.len());
            for (name, n) in rows.into_iter().take(45) {
                let short: String = name.chars().take(150).collect();
                eprintln!("{:6.2}%  {short}", *n as f64 * 100.0 / total);
            }
        };
        print("self time", &self_counts);
        print("inclusive time", &incl_counts);

        // NSWIN_SAMPLE_FOCUS=<substring>: the call chains (innermost first) leading into the
        // first frame whose symbol contains it: "who calls X".
        if let Ok(focus) = std::env::var("NSWIN_SAMPLE_FOCUS") {
            let mut chains: HashMap<String, usize> = HashMap::new();
            let mut hits = 0usize;
            for stack in &samples {
                let names: Vec<String> = stack
                    .iter()
                    .take_while(|&&ip| ip != 0)
                    .enumerate()
                    .map(|(d, &ip)| symbol_name(if d == 0 { ip } else { ip - 1 }, &mut cache))
                    .collect();
                if let Some(pos) = names.iter().position(|n| n.contains(&focus)) {
                    hits += 1;
                    let chain: Vec<String> = names[pos..]
                        .iter()
                        .take(8)
                        .map(|n| n.chars().take(70).collect())
                        .collect();
                    *chains.entry(chain.join("  <-  ")).or_default() += 1;
                }
            }
            let mut rows: Vec<_> = chains.into_iter().collect();
            rows.sort_by(|a, b| b.1.cmp(&a.1));
            eprintln!("\n[sampler] callers of '{focus}' ({hits} samples)");
            for (chain, n) in rows.into_iter().take(12) {
                eprintln!("{n:5}  {chain}");
            }
        }
    }
}
