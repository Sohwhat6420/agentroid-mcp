//! The child side: QuickJS with a memory limit, a stack limit and an interrupt handler, `eval` and
//! the `Function` constructors removed, and only the `agentroid.*` bindings (plus `console.log`).
//! It never touches a file, a socket or a device: every binding is a JSON line to the parent.

use crate::proto::{ChildLimits, FromChild, ToChild};
use rquickjs::{Context, Ctx, Function, Runtime, Value};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::io::{BufRead, Write};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Exit codes of the child.
pub const EXIT_OK: i32 = 0;
pub const EXIT_NOT_IN_JOB: i32 = 3;
pub const EXIT_PARENT_GONE: i32 = 4;
pub const EXIT_PROTOCOL: i32 = 5;

const STACK_BYTES: usize = 256 * 1024;

/// The hardening and binding prelude. It is called with the four raw Rust functions, which it
/// keeps in a closure (the script never sees them), and leaves only `agentroid` and `console`.
pub const PRELUDE: &str = r#"(function (rawCall, rawShell, rawSleep, rawLog) {
  'use strict';
  const parse = JSON.parse, stringify = JSON.stringify, E = Error, TE = TypeError, Str = String, Num = Number;
  const freeze = Object.freeze, define = Object.defineProperty, getProto = Object.getPrototypeOf;
  const unwrap = (s) => { const r = parse(s); if (!r.ok) throw new E(r.error); return r.value; };
  const show = (x) => typeof x === 'string' ? x : (x === undefined ? 'undefined' : stringify(x));
  const agentroid = freeze({
    call(tool, args) { const a = args === undefined ? undefined : stringify(args); return unwrap(rawCall(Str(tool), a === undefined ? 'null' : a)); },
    shell(cmd) { return unwrap(rawShell(Str(cmd))); },
    sleep(ms) { rawSleep(Num(ms)); },
    log(...xs) { rawLog(xs.map(show).join(' ')); },
  });
  define(globalThis, 'agentroid', { value: agentroid, writable: false, configurable: false, enumerable: false });
  const log = agentroid.log;
  define(globalThis, 'console', { value: freeze({ log, info: log, warn: log, error: log }), writable: false, configurable: false, enumerable: false });
  // No eval, no Function constructor: every route to the compiler is closed.
  const refuse = function () { throw new TE('eval and the Function constructor are disabled in run_script'); };
  for (const f of [function () {}, async function () {}, function* () {}, async function* () {}]) {
    define(getProto(f), 'constructor', { value: refuse, writable: false, configurable: false, enumerable: false });
  }
  if (!(delete globalThis.eval)) throw new E('could not remove eval');
  if (!(delete globalThis.Function)) throw new E('could not remove Function');
  // No scheduling beyond what a Promise already gives; the only timer is the capped agentroid.sleep.
  delete globalThis.queueMicrotask;
})"#;

struct Io {
    input: std::io::StdinLock<'static>,
    out: std::io::Stdout,
    next_id: u64,
    /// Time spent waiting on the parent or sleeping; not counted against the CPU deadline.
    blocked: Rc<Cell<Duration>>,
    logs: usize,
    limits: ChildLimits,
}

impl Io {
    fn send(&mut self, m: &FromChild) {
        let mut line = serde_json::to_string(m)
            .unwrap_or_else(|_| r#"{"t":"log","line":"(unserialisable)"}"#.into());
        line.push('\n');
        if self
            .out
            .write_all(line.as_bytes())
            .and_then(|_| self.out.flush())
            .is_err()
        {
            std::process::exit(EXIT_PARENT_GONE);
        }
    }

    fn read(&mut self) -> ToChild {
        let mut line = String::new();
        match self.input.read_line(&mut line) {
            Ok(0) | Err(_) => std::process::exit(EXIT_PARENT_GONE),
            Ok(_) => {
                serde_json::from_str(&line).unwrap_or_else(|_| std::process::exit(EXIT_PROTOCOL))
            }
        }
    }

    /// One binding request; returns the `{ok, value|error}` envelope the prelude unwraps.
    fn request(&mut self, make: impl FnOnce(u64) -> FromChild) -> String {
        self.next_id += 1;
        let id = self.next_id;
        let t = Instant::now();
        self.send(&make(id));
        let reply = self.read();
        self.blocked.set(self.blocked.get() + t.elapsed());
        match reply {
            ToChild::Reply {
                id: rid,
                ok: true,
                value,
                ..
            } if rid == id => json!({"ok": true, "value": value.unwrap_or_default()}).to_string(),
            ToChild::Reply {
                id: rid,
                ok: false,
                error,
                ..
            } if rid == id => {
                json!({"ok": false, "error": error.unwrap_or_else(|| "error".into())}).to_string()
            }
            _ => std::process::exit(EXIT_PROTOCOL),
        }
    }
}

fn harden_error_mode() {
    #[cfg(windows)]
    // SAFETY: process-wide flag; no crash dialog or WER UI for this process.
    unsafe {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX,
        };
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
    }
}

fn in_a_job() -> bool {
    #[cfg(windows)]
    {
        crate::job::current_process_in_a_job()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// The child binary's `main`. Returns the exit code.
pub fn main() -> i32 {
    harden_error_mode();
    let stdin = std::io::stdin().lock();
    let blocked = Rc::new(Cell::new(Duration::ZERO));
    let mut io = Io {
        input: stdin,
        out: std::io::stdout(),
        next_id: 0,
        blocked: blocked.clone(),
        logs: 0,
        limits: ChildLimits {
            js_mem_bytes: 0,
            cpu_ms: 0,
            max_sleep_ms: 0,
            max_stdout_lines: 0,
            max_line_chars: 0,
            max_value_bytes: 0,
        },
    };
    // Nothing runs before `start`, and the parent sends it only after the job assignment.
    let (script, limits) = match io.read() {
        ToChild::Start { script, limits } => (script, limits),
        _ => return EXIT_PROTOCOL,
    };
    io.limits = limits.clone();
    let in_job = in_a_job();
    io.send(&FromChild::Hello { in_job });
    if !in_job {
        io.send(&FromChild::Done {
            ok: false,
            kind: Some("sandbox".into()),
            value: None,
            error: Some("not inside a job object; refusing to run".into()),
        });
        return EXIT_NOT_IN_JOB;
    }
    let io = Rc::new(RefCell::new(io));
    let done = run_js(&script, &limits, io.clone(), blocked);
    io.borrow_mut().send(&done);
    EXIT_OK
}

fn sandbox_err(e: impl std::fmt::Display) -> FromChild {
    FromChild::Done {
        ok: false,
        kind: Some("sandbox".into()),
        value: None,
        error: Some(format!("QuickJS setup: {e}")),
    }
}

fn run_js(
    script: &str,
    limits: &ChildLimits,
    io: Rc<RefCell<Io>>,
    blocked: Rc<Cell<Duration>>,
) -> FromChild {
    let rt = match Runtime::new() {
        Ok(rt) => rt,
        Err(e) => return sandbox_err(e),
    };
    rt.set_memory_limit(limits.js_mem_bytes);
    rt.set_max_stack_size(STACK_BYTES);
    let fired = Rc::new(Cell::new(false));
    let started = Instant::now();
    let cpu = Duration::from_millis(limits.cpu_ms);
    {
        let fired = fired.clone();
        let blocked = blocked.clone();
        rt.set_interrupt_handler(Some(Box::new(move || {
            // active time: wall since start minus time blocked on the parent or in sleep
            let over = started.elapsed().saturating_sub(blocked.get()) > cpu;
            if over {
                fired.set(true);
            }
            over
        })));
    }
    let ctx = match Context::full(&rt) {
        Ok(c) => c,
        Err(e) => return sandbox_err(e),
    };
    let done = ctx.with(|ctx| {
        if let Err(e) = install(&ctx, io.clone(), blocked.clone(), limits) {
            let msg = describe_err(&ctx, e);
            return sandbox_err(msg);
        }
        let result = ctx
            .eval::<Value, _>(script)
            .and_then(|v| match v.as_promise() {
                Some(p) => p.finish::<Value>(),
                None => Ok(v),
            });
        let failed = |e: rquickjs::Error| {
            let msg = describe_err(&ctx, e);
            let kind = if fired.get() {
                "cpu_deadline"
            } else if msg.contains("out of memory") || msg.contains("Allocation failed") {
                "memory"
            } else {
                "exception"
            };
            let msg = if fired.get() {
                format!("CPU deadline of {} ms: {msg}", cpu.as_millis())
            } else {
                msg
            };
            FromChild::Done {
                ok: false,
                kind: Some(kind.into()),
                value: None,
                error: Some(truncate(&msg, 2000)),
            }
        };
        match result {
            Err(e) => failed(e),
            Ok(v) => match ctx.json_stringify(v) {
                Err(e) => failed(e),
                Ok(None) => FromChild::Done {
                    ok: true,
                    kind: None,
                    value: None,
                    error: None,
                },
                Ok(Some(s)) => match s.to_string() {
                    Err(e) => failed(e),
                    Ok(s) if s.len() > limits.max_value_bytes => FromChild::Done {
                        ok: false,
                        kind: Some("exception".into()),
                        value: None,
                        error: Some(format!(
                            "return value is {} bytes of JSON; the limit is {}",
                            s.len(),
                            limits.max_value_bytes
                        )),
                    },
                    Ok(s) => FromChild::Done {
                        ok: true,
                        kind: None,
                        value: Some(s),
                        error: None,
                    },
                },
            },
        }
    });
    // Deep in an out-of-memory state QuickJS cannot even allocate the error object and throws
    // `null`; the heap still being at the limit tells us what happened.
    match done {
        FromChild::Done {
            ok: false,
            kind: Some(k),
            error,
            ..
        } if k == "exception" && at_limit(&rt, limits.js_mem_bytes) => FromChild::Done {
            ok: false,
            kind: Some("memory".into()),
            value: None,
            error: Some(format!(
                "out of memory: QuickJS limit of {} bytes reached ({})",
                limits.js_mem_bytes,
                error.unwrap_or_default()
            )),
        },
        other => other,
    }
}

fn at_limit(rt: &Runtime, limit: usize) -> bool {
    let used = rt.memory_usage().malloc_size.max(0) as usize;
    limit > 0 && used + 1024 * 1024 >= limit
}

fn describe_err(ctx: &Ctx<'_>, e: rquickjs::Error) -> String {
    if !matches!(e, rquickjs::Error::Exception) {
        return e.to_string();
    }
    let v = ctx.catch();
    if let Some(ex) = v.as_exception() {
        let name: String = ex.get("name").unwrap_or_else(|_| "Error".into());
        return format!("{name}: {}", ex.message().unwrap_or_default());
    }
    if let Some(s) = v.as_string() {
        return s.to_string().unwrap_or_default();
    }
    match ctx.json_stringify(v) {
        Ok(Some(s)) => s.to_string().unwrap_or_default(),
        _ => "a non-JSON value was thrown".into(),
    }
}

fn install<'js>(
    ctx: &Ctx<'js>,
    io: Rc<RefCell<Io>>,
    blocked: Rc<Cell<Duration>>,
    limits: &ChildLimits,
) -> rquickjs::Result<()> {
    let raw_call = {
        let io = io.clone();
        Function::new(ctx.clone(), move |tool: String, args: String| -> String {
            let args: serde_json::Value = match serde_json::from_str(&args) {
                Ok(v) => v,
                Err(e) => {
                    return json!({"ok": false, "error": format!("args are not JSON: {e}")})
                        .to_string()
                }
            };
            io.borrow_mut()
                .request(|id| FromChild::Call { id, tool, args })
        })?
    };
    let raw_shell = {
        let io = io.clone();
        Function::new(ctx.clone(), move |cmd: String| -> String {
            io.borrow_mut().request(|id| FromChild::Shell { id, cmd })
        })?
    };
    let max_sleep = limits.max_sleep_ms;
    let raw_sleep = Function::new(ctx.clone(), move |ms: f64| {
        let ms = if ms.is_finite() {
            ms.clamp(0.0, max_sleep as f64)
        } else {
            0.0
        };
        let d = Duration::from_millis(ms as u64);
        std::thread::sleep(d);
        blocked.set(blocked.get() + d);
    })?;
    let raw_log = {
        let max_lines = limits.max_stdout_lines;
        let max_chars = limits.max_line_chars;
        Function::new(ctx.clone(), move |line: String| {
            let mut io = io.borrow_mut();
            // one past the cap, so the parent can mark the output truncated
            if io.logs <= max_lines {
                io.logs += 1;
                io.send(&FromChild::Log {
                    line: truncate(&line, max_chars),
                });
            }
        })?
    };
    let prelude: Function = ctx.eval(PRELUDE)?;
    prelude.call::<_, ()>((raw_call, raw_shell, raw_sleep, raw_log))?;
    Ok(())
}
