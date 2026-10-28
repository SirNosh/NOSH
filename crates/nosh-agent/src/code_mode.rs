//! nosh_code, as `code-mode.ts` and `code-mode-worker.ts`: the model writes a short JavaScript
//! program that calls its own tools, so independent calls run together, dependent calls chain,
//! and large results are filtered before they reach the model's context. The program runs in a
//! QuickJS interpreter on its own thread and can reach nothing but the tool functions passed in
//! here, each of which the caller runs through the normal, checked tool implementation.
//!
//! The engine is quickjs-ng (rquickjs), not Bellard QuickJS compiled to WASM, so engine-written
//! error texts (syntax errors, module loading) can differ from TypeScript's; host-written texts
//! are identical. One deliberate difference: a program whose value has no JSON form (a function,
//! a symbol) returns null, where TypeScript's daemon crashed on `JSON.parse("undefined")`.

use crate::async_calls::{AbortSignal, BoxFuture};
use crate::js_json;
use nosh_wire::{canonical_json, js};
use rquickjs::convert::Coerced;
use rquickjs::function::This;
use rquickjs::promise::PromiseState;
use rquickjs::{Context, Ctx, Function, Persistent, Promise, Runtime, Value as JsValue};
use serde::Serialize;
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc as channel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeLimits {
    pub max_calls: usize,
    pub cpu_ms: u64,
    pub memory_bytes: usize,
    pub stack_bytes: usize,
    pub max_log_chars: usize,
}

pub const CODE_LIMITS: CodeLimits = CodeLimits {
    max_calls: 50,
    cpu_ms: 5_000,
    memory_bytes: 64 * 1024 * 1024,
    stack_bytes: 1024 * 1024,
    max_log_chars: 12_000,
};
pub const MAX_CODE_CHARS: usize = 20_000;
/// The interpreter's native frames sit below its 1 MB JS stack limit; Windows' default 1 MB
/// thread stack overflows natively first.
const GUEST_THREAD_STACK: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProgramResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub logs: Vec<String>,
    pub calls: usize,
}

/// Runs one tool call for the program; `index` is the call's position, for event ids. It is
/// called when the program makes the call, in program order, so synchronous checks (the worktree
/// lock) see calls as TypeScript does; the returned future runs the rest. An error fails the call.
pub type ProgramHost =
    Arc<dyn Fn(&str, Vec<Value>, usize) -> BoxFuture<Result<Value, String>> + Send + Sync>;

enum GuestMessage {
    Call {
        id: u64,
        name: String,
        args_json: String,
    },
    Log(String),
    Done(Result<String, String>),
}

enum Inbound {
    Reply { id: u64, ok: bool, json: String },
    Cancel,
}

pub async fn run_program(
    code: &str,
    functions: &[&str],
    host: ProgramHost,
    signal: &AbortSignal,
    limits: CodeLimits,
) -> ProgramResult {
    let failed = |error: String| ProgramResult {
        ok: false,
        value: None,
        error: Some(error),
        logs: Vec::new(),
        calls: 0,
    };
    if js::length(code) > MAX_CODE_CHARS {
        return failed(format!("Program exceeds {MAX_CODE_CHARS} characters"));
    }
    if signal.aborted() {
        return failed("Program cancelled".into());
    }
    let (outbound, mut messages) = channel::unbounded_channel();
    let (replies, inbound) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let input = GuestInput {
        code: code.to_owned(),
        functions: functions.iter().map(|name| (*name).to_owned()).collect(),
        limits,
        cancelled: Arc::clone(&cancelled),
    };
    let spawned = std::thread::Builder::new()
        .name("nosh-code".into())
        .stack_size(GUEST_THREAD_STACK)
        .spawn(move || {
            if let Some(result) = evaluate(&input, &outbound, &inbound) {
                let _ = outbound.send(GuestMessage::Done(result));
            }
        });
    if let Err(error) = spawned {
        return failed(format!("Program failed: {error}"));
    }

    let (settled, mut completions) = channel::unbounded_channel::<(u64, Result<Value, String>)>();
    let (mut logs, mut log_chars, mut calls) = (Vec::new(), 0, 0);
    let outcome: Result<Value, String> = loop {
        tokio::select! {
            biased;
            () = signal.cancelled() => break Err("Program cancelled".into()),
            message = messages.recv() => match message {
                None => break Err("Program stopped unexpectedly".into()),
                Some(GuestMessage::Done(result)) => break result.and_then(|json| js_json::parse(&well_formed_escapes(&json)).map_err(|error| error.message)),
                Some(GuestMessage::Log(text)) => if log_chars < limits.max_log_chars {
                    let text = js::slice_to(&text, limits.max_log_chars - log_chars);
                    log_chars += js::length(&text);
                    logs.push(text);
                },
                Some(GuestMessage::Call { id, name, args_json }) => {
                    let refuse = |json: String| { let _ = replies.send(Inbound::Reply { id, ok: false, json }); };
                    if !functions.contains(&name.as_str()) { refuse(format!("Unknown tool function {name}")); continue; }
                    if calls >= limits.max_calls { refuse(format!("Program exceeded {} tool calls", limits.max_calls)); continue; }
                    let index = calls;
                    calls += 1;
                    let args = match js_json::parse(&well_formed_escapes(&args_json)) {
                        Ok(Value::Array(args)) => args,
                        Ok(_) => { refuse("Invalid arguments: arguments must be a list".into()); continue; }
                        Err(error) => { refuse(format!("Invalid arguments: {}", error.message)); continue; }
                    };
                    // Host calls outlive the program, as unawaited promises do in TypeScript.
                    let work = tokio::spawn(host(&name, args, index));
                    let settled = settled.clone();
                    tokio::spawn(async move {
                        let result = work.await.unwrap_or_else(|error| Err(error.to_string()));
                        let _ = settled.send((id, result));
                    });
                }
            },
            Some((id, result)) = completions.recv() => {
                let (ok, json) = match result { Ok(value) => (true, canonical_json(&value)), Err(error) => (false, error) };
                let _ = replies.send(Inbound::Reply { id, ok, json });
            }
        }
    };
    // Stops a spinning interpreter (the interrupt handler) and a waiting one (the channel).
    cancelled.store(true, Ordering::SeqCst);
    let _ = replies.send(Inbound::Cancel);
    match outcome {
        Ok(value) => ProgramResult {
            ok: true,
            value: Some(value),
            error: None,
            logs,
            calls,
        },
        Err(error) => ProgramResult {
            ok: false,
            value: None,
            error: Some(error),
            logs,
            calls,
        },
    }
}

struct GuestInput {
    code: String,
    functions: Vec<String>,
    limits: CodeLimits,
    cancelled: Arc<AtomicBool>,
}

/// Only time spent executing inside the interpreter counts against the CPU limit; waiting on
/// tools does not.
#[derive(Default)]
struct CpuClock {
    spent: Cell<Duration>,
    entered: Cell<Option<Instant>>,
}

impl CpuClock {
    fn enter(&self) {
        self.entered.set(Some(Instant::now()));
    }
    fn leave(&self) {
        if let Some(at) = self.entered.take() {
            self.spent.set(self.spent.get() + at.elapsed());
        }
    }
    fn exceeded(&self, limit: Duration) -> bool {
        self.entered
            .get()
            .is_some_and(|at| self.spent.get() + at.elapsed() > limit)
    }
}

type Settlers = (Persistent<Function<'static>>, Persistent<Function<'static>>);

/// Handles the guest holds into the runtime; every one is released before the runtime drops.
#[derive(Default)]
struct Handles {
    waiting: RefCell<HashMap<u64, Settlers>>,
    /// `String.prototype.toWellFormed` as it was before the program ran.
    well_formed: RefCell<Option<Persistent<Function<'static>>>>,
    next_id: Cell<u64>,
}

/// Runs the program to its end; `None` when it was cancelled or the host went away.
fn evaluate(
    input: &GuestInput,
    outbound: &channel::UnboundedSender<GuestMessage>,
    inbound: &mpsc::Receiver<Inbound>,
) -> Option<Result<String, String>> {
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => return Some(Err(format!("Program failed: {error}"))),
    };
    runtime.set_memory_limit(input.limits.memory_bytes);
    runtime.set_max_stack_size(input.limits.stack_bytes);
    let clock = Rc::new(CpuClock::default());
    {
        let (clock, cancelled, limit) = (
            Rc::clone(&clock),
            Arc::clone(&input.cancelled),
            Duration::from_millis(input.limits.cpu_ms),
        );
        runtime.set_interrupt_handler(Some(Box::new(move || {
            cancelled.load(Ordering::SeqCst) || clock.exceeded(limit)
        })));
    }
    let context = match Context::full(&runtime) {
        Ok(context) => context,
        Err(error) => return Some(Err(format!("Program failed: {error}"))),
    };
    let handles = Rc::new(Handles::default());
    let outcome = run_guest(
        input, &runtime, &context, &clock, &handles, outbound, inbound,
    );
    handles.waiting.borrow_mut().clear();
    handles.well_formed.borrow_mut().take();
    outcome
}

fn run_guest(
    input: &GuestInput,
    runtime: &Runtime,
    context: &Context,
    clock: &CpuClock,
    handles: &Rc<Handles>,
    outbound: &channel::UnboundedSender<GuestMessage>,
    inbound: &mpsc::Receiver<Inbound>,
) -> Option<Result<String, String>> {
    let program = context.with(|ctx| -> Result<Persistent<Promise<'static>>, String> {
        let well_formed: Function = ctx.eval("String.prototype.toWellFormed").map_err(|error| describe_error(&ctx, handles, error))?;
        *handles.well_formed.borrow_mut() = Some(Persistent::save(&ctx, well_formed));
        let call = call_function(&ctx, handles, outbound.clone()).map_err(|error| describe_error(&ctx, handles, error))?;
        let log = log_function(&ctx, handles, outbound.clone()).map_err(|error| describe_error(&ctx, handles, error))?;
        ctx.globals().set("__call", call).map_err(|error| describe_error(&ctx, handles, error))?;
        ctx.globals().set("__log", log).map_err(|error| describe_error(&ctx, handles, error))?;
        let names = canonical_json(&Value::from(input.functions.clone()));
        // The prelude closes over the host functions, then removes them from the global object.
        let prelude = format!(
            r#"const {{ tools, console }} = ((call, log) => {{
  const invoke = async (name, args) => {{ let json; try {{ json = await call(name, JSON.stringify(args)); }} catch (message) {{ throw new Error(String(message)); }} return JSON.parse(json); }};
  const format = (value) => typeof value === "string" ? value : JSON.stringify(value);
  return {{
    tools: Object.freeze(Object.fromEntries({names}.map((name) => [name, (...args) => invoke(name, args)]))),
    console: Object.freeze({{ log: (...values) => log(values.map(format).join(" ")) }}),
  }};
}})(__call, __log);
delete globalThis.__call; delete globalThis.__log;"#
        );
        let source = format!(
            "{prelude}\n(async () => {{ const __value = await (async () => {{\n{}\n}})(); const __json = JSON.stringify(__value); return __json === undefined ? \"null\" : __json; }})()",
            input.code
        );
        clock.enter();
        let evaluated: rquickjs::Result<Promise> = ctx.eval(source);
        clock.leave();
        match evaluated {
            Ok(promise) => Ok(Persistent::save(&ctx, promise)),
            Err(error) => Err(describe_error(&ctx, handles, error)),
        }
    });
    let program = match program {
        Ok(program) => program,
        Err(error) => return (!input.cancelled.load(Ordering::SeqCst)).then_some(Err(error)),
    };
    loop {
        clock.enter();
        let jobs = loop {
            match runtime.execute_pending_job() {
                Ok(true) => {}
                Ok(false) => break Ok(()),
                Err(_) => break Err(()),
            }
        };
        clock.leave();
        if input.cancelled.load(Ordering::SeqCst) {
            return None;
        }
        if jobs.is_err() {
            return Some(Err(context.with(|ctx| {
                let error = ctx.catch();
                describe(&ctx, handles, &error)
            })));
        }
        let settled = context.with(|ctx| -> Option<Result<String, String>> {
            let promise = program.clone().restore(&ctx).ok()?;
            match promise.state() {
                PromiseState::Pending => None,
                PromiseState::Resolved => Some(Ok(match promise.result::<JsValue>() {
                    Some(Ok(value)) => value.as_string().map_or_else(
                        || "null".to_owned(),
                        |text| rust_string(&ctx, handles, text.clone()),
                    ),
                    _ => "null".to_owned(),
                })),
                PromiseState::Rejected => Some(Err(match promise.result::<JsValue>() {
                    Some(Err(error)) => describe_error(&ctx, handles, error),
                    _ => "Error: rejected".to_owned(),
                })),
            }
        });
        if let Some(result) = settled {
            return Some(result);
        }
        // The guest has no timers: with no tool call outstanding, an unsettled program can never
        // make progress.
        if handles.waiting.borrow().is_empty() {
            return Some(Err("The program is waiting on something that can never finish (no tool call is outstanding).".into()));
        }
        match inbound.recv() {
            Ok(Inbound::Reply { id, ok, json }) => {
                let Some((resolve, reject)) = handles.waiting.borrow_mut().remove(&id) else {
                    continue;
                };
                context.with(|ctx| {
                    let settle = if ok { resolve } else { reject };
                    if let (Ok(settle), Ok(text)) = (
                        settle.restore(&ctx),
                        rquickjs::String::from_str(ctx.clone(), &json),
                    ) {
                        let _ = settle.call::<_, ()>((text,));
                    }
                });
            }
            Ok(Inbound::Cancel) | Err(_) => return None,
        }
    }
}

fn call_function<'js>(
    ctx: &Ctx<'js>,
    handles: &Rc<Handles>,
    outbound: channel::UnboundedSender<GuestMessage>,
) -> rquickjs::Result<Function<'js>> {
    let handles = Rc::clone(handles);
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>,
              name: rquickjs::String<'js>,
              args: rquickjs::String<'js>|
              -> rquickjs::Result<Promise<'js>> {
            let id = handles.next_id.get();
            handles.next_id.set(id + 1);
            let (promise, resolve, reject) = ctx.promise()?;
            handles.waiting.borrow_mut().insert(
                id,
                (
                    Persistent::save(&ctx, resolve),
                    Persistent::save(&ctx, reject),
                ),
            );
            let (name, args_json) = (
                rust_string(&ctx, &handles, name),
                rust_string(&ctx, &handles, args),
            );
            let _ = outbound.send(GuestMessage::Call {
                id,
                name,
                args_json,
            });
            Ok(promise)
        },
    )
}

fn log_function<'js>(
    ctx: &Ctx<'js>,
    handles: &Rc<Handles>,
    outbound: channel::UnboundedSender<GuestMessage>,
) -> rquickjs::Result<Function<'js>> {
    let handles = Rc::clone(handles);
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, text: Coerced<rquickjs::String<'js>>| {
            let _ = outbound.send(GuestMessage::Log(rust_string(&ctx, &handles, text.0)));
        },
    )
}

/// A guest string as Rust text; lone surrogates, which Rust strings cannot hold, become U+FFFD.
fn rust_string<'js>(ctx: &Ctx<'js>, handles: &Handles, text: rquickjs::String<'js>) -> String {
    if let Ok(text) = text.to_string() {
        return text;
    }
    let well_formed = handles
        .well_formed
        .borrow()
        .clone()
        .and_then(|function| function.restore(ctx).ok());
    well_formed
        .and_then(|function| function.call::<_, rquickjs::String>((This(text),)).ok())
        .and_then(|text| text.to_string().ok())
        .unwrap_or_else(|| "\u{FFFD}".to_owned())
}

fn describe_error(ctx: &Ctx<'_>, handles: &Handles, error: rquickjs::Error) -> String {
    if matches!(error, rquickjs::Error::Exception) {
        let exception = ctx.catch();
        return describe(ctx, handles, &exception);
    }
    error.to_string()
}

/// TypeScript's `describe(vm.dump(error))`: `name: message` for an error or an object with a
/// message, else `String(error)`.
fn describe<'js>(ctx: &Ctx<'js>, handles: &Handles, error: &JsValue<'js>) -> String {
    let text = |value: JsValue<'js>| -> String {
        match value.get::<Coerced<rquickjs::String>>() {
            Ok(text) => rust_string(ctx, handles, text.0),
            Err(_) => {
                let _ = ctx.catch();
                if value.is_symbol() {
                    "Symbol()".to_owned()
                } else {
                    "[object Object]".to_owned()
                }
            }
        }
    };
    if let Some(object) = error.as_object() {
        let message: JsValue = object
            .get("message")
            .unwrap_or_else(|_| JsValue::new_undefined(ctx.clone()));
        if error.is_error() || !message.is_undefined() {
            let name: JsValue = object
                .get("name")
                .unwrap_or_else(|_| JsValue::new_undefined(ctx.clone()));
            let name = if name.is_undefined() || name.is_null() {
                "Error".to_owned()
            } else {
                text(name)
            };
            return format!("{name}: {}", text(message));
        }
    }
    text(error.clone())
}

/// JSON text from the guest's `JSON.stringify`, with lone-surrogate escapes (which `serde_json`
/// rejects) replaced by `�`, as the lone surrogates themselves become U+FFFD elsewhere.
fn well_formed_escapes(json: &str) -> std::borrow::Cow<'_, str> {
    if !json.contains("\\u") {
        return json.into();
    }
    let bytes = json.as_bytes();
    let unit = |at: usize| -> Option<u16> {
        (bytes.get(at) == Some(&b'\\') && bytes.get(at + 1) == Some(&b'u'))
            .then(|| {
                json.get(at + 2..at + 6)
                    .and_then(|hex| u16::from_str_radix(hex, 16).ok())
            })
            .flatten()
    };
    let mut out = String::with_capacity(json.len());
    let mut index = 0;
    while index < json.len() {
        if bytes[index] != b'\\' {
            let next = json[index..]
                .find('\\')
                .map_or(json.len(), |offset| index + offset);
            out.push_str(&json[index..next]);
            index = next;
            continue;
        }
        match unit(index) {
            Some(high @ 0xD800..=0xDBFF) if matches!(unit(index + 6), Some(0xDC00..=0xDFFF)) => {
                let _ = high;
                out.push_str(&json[index..index + 12]);
                index += 12;
            }
            Some(0xD800..=0xDFFF) => {
                out.push_str("\\ufffd");
                index += 6;
            }
            _ => {
                // Any other escape is two characters, or six for \u; copy it whole.
                let end = if bytes.get(index + 1) == Some(&b'u') {
                    index + 6
                } else {
                    index + 2
                };
                out.push_str(&json[index..end.min(json.len())]);
                index = end;
            }
        }
    }
    out.into()
}
