//! QuickJS 里的洛雪宿主。`lx.env` 固定为 `desktop`，`lx.version` 为 `2.0.0`。
//! 请求在原生函数里阻塞完成，Promise / 定时器由外面的泵补上，这样可以等异步 `inited`。

use super::crypto::{self, aes_encrypt, md5_hex, random_bytes, rsa_encrypt_no_padding};
use super::http::{script_request, HttpResponse};
use anyhow::{anyhow, bail, Result};
use rquickjs::function::Rest;
use rquickjs::{Array, ArrayBuffer, Context, Ctx, Exception, Function, Object, Runtime, Value};
use serde_json::Value as Json;
use std::collections::{BTreeMap, HashMap};
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PRELUDE: &str = include_str!("prelude.js");
pub const MAX_SCRIPT_BYTES: usize = 2 * 1024 * 1024;
const MAX_JOBS: u32 = 100_000;
const INIT_EVAL: Duration = Duration::from_secs(5);
const INIT_WAIT: Duration = Duration::from_secs(10);
const CALL_WAIT: Duration = Duration::from_secs(20);

const FIRE_TIMER: &str = r#"
(function () {
  var timers = globalThis.__miyinTimers || [];
  var now = Date.now();
  var idx = -1;
  for (var i = 0; i < timers.length; i++) {
    if (timers[i].due <= now && (idx < 0 || timers[i].due < timers[idx].due)) idx = i;
  }
  if (idx < 0) return false;
  var t = timers[idx];
  if (t.interval) t.due = now + t.delay;
  else timers.splice(idx, 1);
  try { t.fn(); } catch (e) { throw e; }
  return true;
})()
"#;

const NEXT_TIMER_MS: &str = r#"
(function () {
  var timers = globalThis.__miyinTimers || [];
  if (!timers.length) return -1;
  var now = Date.now();
  var min = timers[0].due;
  for (var i = 1; i < timers.length; i++) if (timers[i].due < min) min = timers[i].due;
  var d = min - now;
  return d < 0 ? 0 : d;
})()
"#;

#[derive(Clone)]
pub struct ScriptMeta {
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub homepage: String,
    pub raw_script: String,
}

#[derive(Debug, Clone)]
pub struct InitInfo {
    pub platforms: Vec<String>,
    pub quality_map: BTreeMap<String, Vec<String>>,
    pub update_alerts: Vec<String>,
}

struct InitState {
    inited: bool,
    platforms: Vec<String>,
    quality_map: BTreeMap<String, Vec<String>>,
    alerts: Vec<String>,
}

struct CallSlot {
    done: bool,
    ok: bool,
    payload: String,
}

enum Wait {
    Inited,
    Call,
}

pub struct SourceSession {
    context: ManuallyDrop<Context>,
    runtime: ManuallyDrop<Runtime>,
    deadline: Arc<AtomicU64>,
    init: Arc<Mutex<InitState>>,
    slot: Arc<Mutex<CallSlot>>,
}

impl Drop for SourceSession {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.context);
            ManuallyDrop::drop(&mut self.runtime);
        }
    }
}

impl SourceSession {
    pub fn open(script: &str, meta: &ScriptMeta) -> Result<Self> {
        let script = script.strip_prefix('\u{feff}').unwrap_or(script);
        if script.len() > MAX_SCRIPT_BYTES {
            bail!("音源脚本过大，拒绝加载");
        }
        let runtime = Runtime::new().map_err(|e| anyhow!("QuickJS runtime: {e}"))?;
        runtime.set_memory_limit(256 * 1024 * 1024);
        runtime.set_max_stack_size(512 * 1024);
        let deadline = Arc::new(AtomicU64::new(u64::MAX));
        let flag = deadline.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || now_ms() > flag.load(Ordering::Relaxed))));
        let context = Context::full(&runtime).map_err(|e| anyhow!("QuickJS context: {e}"))?;
        let init = Arc::new(Mutex::new(InitState {
            inited: false,
            platforms: Vec::new(),
            quality_map: BTreeMap::new(),
            alerts: Vec::new(),
        }));
        let slot = Arc::new(Mutex::new(CallSlot {
            done: false,
            ok: false,
            payload: String::new(),
        }));
        let session = Self {
            context: ManuallyDrop::new(context),
            runtime: ManuallyDrop::new(runtime),
            deadline,
            init,
            slot,
        };
        session.arm_store(INIT_EVAL);
        session.with_ctx(|ctx| install(ctx, meta, &session.deadline, &session.init, &session.slot))?;
        session.with_ctx(|ctx| {
            let _: Value = ctx.eval(script.to_owned())?;
            Ok(())
        })?;
        Ok(session)
    }

    pub fn init(&self) -> Result<InitInfo> {
        self.arm_store(INIT_WAIT);
        self.pump(Wait::Inited, INIT_WAIT)?;
        if !self.init.lock().unwrap().inited {
            bail!("音源未完成初始化（未发送 inited）");
        }
        let n = self.with_ctx(|ctx| {
            let arr: Array = ctx.globals().get("__lx_handlers")?;
            Ok(arr.len())
        })?;
        if n == 0 {
            bail!("音源未注册 musicUrl 处理函数");
        }
        let g = self.init.lock().unwrap();
        Ok(InitInfo {
            platforms: g.platforms.clone(),
            quality_map: g.quality_map.clone(),
            update_alerts: g.alerts.clone(),
        })
    }

    /// `musicUrl`、`lyric`、`pic` 共用这一次调用。播放地址仍然只从 `musicUrl` 取。
    pub fn call_action(&self, action: &str, platform: &str, info: Json) -> Result<(bool, String)> {
        if !self.init.lock().unwrap().inited {
            bail!("音源尚未初始化");
        }
        {
            let mut slot = self.slot.lock().unwrap();
            slot.done = false;
            slot.ok = false;
            slot.payload.clear();
        }
        let payload = serde_json::json!({
            "action": action,
            "source": platform,
            "info": info,
        });
        let text = serde_json::to_string(&payload)?;
        self.arm_store(CALL_WAIT);
        self.with_ctx(|ctx| {
            let invoke: Function = ctx.globals().get("__miyinInvoke")?;
            invoke.call::<_, ()>((text,))?;
            Ok(())
        })?;
        self.pump(Wait::Call, CALL_WAIT)?;
        let slot = self.slot.lock().unwrap();
        if !slot.done {
            bail!("取链超时");
        }
        Ok((slot.ok, slot.payload.clone()))
    }

    pub fn music_url(&self, platform: &str, quality: &str, music_info: &Json) -> Result<String> {
        let (ok, payload) = self.call_action(
            "musicUrl",
            platform,
            serde_json::json!({ "type": quality, "musicInfo": music_info }),
        )?;
        extract_url(ok, &payload)
    }

    #[cfg(test)]
    pub fn eval_to_string(&self, expr: &str) -> Result<String> {
        self.with_ctx(|ctx| {
            let v: Value = ctx.eval(expr.to_owned())?;
            if let Some(s) = v.as_string() {
                return Ok(s.to_string()?);
            }
            if v.is_null() || v.is_undefined() {
                return Ok(String::new());
            }
            if let Some(b) = v.as_bool() {
                return Ok(b.to_string());
            }
            if let Some(i) = v.as_int() {
                return Ok(i.to_string());
            }
            json_stringify(ctx, v)
        })
    }

    fn pump(&self, wait_for: Wait, budget: Duration) -> Result<()> {
        let deadline = Instant::now() + budget;
        let mut jobs = 0u32;
        loop {
            if Instant::now() > deadline {
                return self.wait_error(&wait_for, true, budget);
            }
            if self.satisfied(&wait_for) {
                return Ok(());
            }
            match self.runtime.execute_pending_job() {
                Ok(true) => {
                    jobs += 1;
                    if jobs > MAX_JOBS {
                        bail!("微任务数量超限");
                    }
                    continue;
                }
                Ok(false) => {}
                Err(err) => bail!("脚本任务失败: {err:?}"),
            }
            let fired = self.with_ctx(|ctx| ctx.eval::<bool, _>(FIRE_TIMER.to_owned()))?;
            if fired {
                continue;
            }
            if self.satisfied(&wait_for) {
                return Ok(());
            }
            let wait_ms = self.with_ctx(|ctx| ctx.eval::<f64, _>(NEXT_TIMER_MS.to_owned()))?;
            if wait_ms < 0.0 {
                if self.satisfied(&wait_for) {
                    return Ok(());
                }
                return self.wait_error(&wait_for, false, budget);
            }
            let slice = Duration::from_millis(wait_ms.max(0.0) as u64).min(Duration::from_millis(50));
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                return self.wait_error(&wait_for, true, budget);
            }
            if !slice.is_zero() {
                std::thread::sleep(slice.min(remain));
            }
        }
    }

    fn satisfied(&self, wait_for: &Wait) -> bool {
        match wait_for {
            Wait::Inited => self.init.lock().unwrap().inited,
            Wait::Call => self.slot.lock().unwrap().done,
        }
    }

    fn wait_error(&self, wait_for: &Wait, timed_out: bool, budget: Duration) -> Result<()> {
        let ms = budget.as_millis();
        match (wait_for, timed_out) {
            (Wait::Inited, true) => bail!("音源初始化超时（{ms}ms 内未发送 inited）"),
            (Wait::Inited, false) => bail!("音源未完成初始化（未发送 inited）"),
            (Wait::Call, _) => bail!("取链超时"),
        }
    }

    fn arm_store(&self, dur: Duration) {
        self.deadline
            .store(now_ms().saturating_add(dur.as_millis() as u64), Ordering::Relaxed);
    }

    fn with_ctx<T>(&self, f: impl FnOnce(&Ctx<'_>) -> rquickjs::Result<T>) -> Result<T> {
        let mut message: Option<String> = None;
        let out = self.context.with(|ctx| match f(&ctx) {
            Ok(v) => Ok(v),
            Err(err) => {
                message = Some(explain(&ctx, err));
                Err(rquickjs::Error::Exception)
            }
        });
        match out {
            Ok(v) => Ok(v),
            Err(err) => Err(anyhow!(message.unwrap_or_else(|| format!("脚本执行失败: {err}")))),
        }
    }
}

fn install<'js>(
    ctx: &Ctx<'js>,
    meta: &ScriptMeta,
    deadline: &Arc<AtomicU64>,
    init: &Arc<Mutex<InitState>>,
    slot: &Arc<Mutex<CallSlot>>,
) -> rquickjs::Result<()> {
    ctx.eval::<Value, _>(PRELUDE.to_owned())?;
    let global = ctx.globals();

    let console = Object::new(ctx.clone())?;
    let log = Function::new(ctx.clone(), |args: Rest<Value<'_>>| {
        if std::env::var("LXMUSIC_CLI_DEBUG").ok().as_deref() == Some("1") {
            let mut parts = Vec::new();
            for arg in args.iter() {
                if let Some(s) = arg.as_string().and_then(|s| s.to_string().ok()) {
                    parts.push(s);
                }
            }
            eprintln!("[js] {}", parts.join(" "));
        }
        rquickjs::Result::Ok(())
    })?;
    console.set("log", log.clone())?;
    console.set("info", log.clone())?;
    console.set("warn", log.clone())?;
    console.set("error", log.clone())?;
    console.set("group", log.clone())?;
    console.set("groupEnd", Function::new(ctx.clone(), || {})?)?;
    global.set("console", console)?;

    let settle_slot = slot.clone();
    global.set(
        "__miyinSettle",
        Function::new(ctx.clone(), move |ok: bool, payload: String| {
            let mut g = settle_slot.lock().unwrap();
            g.done = true;
            g.ok = ok;
            g.payload = payload;
        })?,
    )?;

    let lx = Object::new(ctx.clone())?;
    let events = Object::new(ctx.clone())?;
    events.set("request", "request")?;
    events.set("inited", "inited")?;
    events.set("updateAlert", "updateAlert")?;
    events.set("showConfigView", "showConfigView")?;
    lx.set("EVENT_NAMES", events)?;
    lx.set("env", "desktop")?;
    lx.set("version", "2.0.0")?;

    let info = Object::new(ctx.clone())?;
    info.set("name", meta.name.as_str())?;
    info.set("description", meta.description.as_str())?;
    info.set("version", meta.version.as_str())?;
    info.set("author", meta.author.as_str())?;
    info.set("homepage", meta.homepage.as_str())?;
    info.set("rawScript", meta.raw_script.as_str())?;
    lx.set("currentScriptInfo", info)?;

    let init_send = init.clone();
    lx.set(
        "send",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, args: Rest<Value<'js>>| -> rquickjs::Result<Value<'js>> {
                let name = arg_string(&args, 0);
                if name == "inited" {
                    let payload = args
                        .get(1)
                        .cloned()
                        .unwrap_or_else(|| Value::new_undefined(ctx.clone()));
                    let text = json_stringify(&ctx, payload).unwrap_or_else(|_| "null".into());
                    let mut g = init_send.lock().unwrap();
                    if let Ok(v) = serde_json::from_str::<Json>(&text) {
                        ingest_inited(&v, &mut g);
                    }
                    g.inited = true;
                } else if name == "updateAlert" {
                    let payload = args
                        .get(1)
                        .cloned()
                        .unwrap_or_else(|| Value::new_undefined(ctx.clone()));
                    let text = json_stringify(&ctx, payload).unwrap_or_default();
                    if let Ok(v) = serde_json::from_str::<Json>(&text) {
                        let alert = alert_text(&v);
                        if !alert.is_empty() {
                            init_send.lock().unwrap().alerts.push(alert);
                        }
                    } else if !text.trim().is_empty() && text != "null" {
                        init_send.lock().unwrap().alerts.push(text);
                    }
                }
                resolved_promise(&ctx)
            },
        )?,
    )?;

    lx.set(
        "on",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, args: Rest<Value<'js>>| -> rquickjs::Result<Value<'js>> {
                let name = arg_string(&args, 0);
                if name == "request" {
                    if let Some(handler) = args.get(1) {
                        let arr: Array = ctx.globals().get("__lx_handlers")?;
                        arr.set(arr.len(), handler.clone())?;
                    }
                }
                resolved_promise(&ctx)
            },
        )?,
    )?;

    let deadline_req = deadline.clone();
    lx.set(
        "request",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, args: Rest<Value<'js>>| -> rquickjs::Result<Value<'js>> {
                lx_request(&ctx, args, &deadline_req)
            },
        )?,
    )?;

    let utils = Object::new(ctx.clone())?;
    utils.set("buffer", buffer_api(ctx)?)?;
    utils.set("crypto", crypto_api(ctx)?)?;
    utils.set("zlib", zlib_api(ctx)?)?;
    lx.set("utils", utils)?;
    global.set("lx", lx)?;
    Ok(())
}

fn buffer_api<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let buffer = Object::new(ctx.clone())?;
    buffer.set(
        "from",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, args: Rest<Value<'js>>| -> rquickjs::Result<Value<'js>> {
                let ctor: Object = ctx.globals().get("Buffer")?;
                let from: Function = ctor.get("from")?;
                let a0 = args
                    .get(0)
                    .cloned()
                    .unwrap_or_else(|| Value::new_undefined(ctx.clone()));
                if let Some(a1) = args.get(1) {
                    from.call((a0, a1.clone()))
                } else {
                    from.call((a0,))
                }
            },
        )?,
    )?;
    buffer.set(
        "bufToString",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, args: Rest<Value<'js>>| -> rquickjs::Result<String> {
                let helper: Function = ctx.globals().get("__miyinToString")?;
                let data = args
                    .get(0)
                    .cloned()
                    .unwrap_or_else(|| Value::new_undefined(ctx.clone()));
                let encoding = if args.len() > 1 {
                    arg_string(&args, 1)
                } else {
                    "utf8".into()
                };
                if let Some(s) = data.as_string() {
                    let text = s.to_string()?;
                    let ctor: Object = ctx.globals().get("Buffer")?;
                    let from: Function = ctor.get("from")?;
                    let buf: Value = from.call((text, "binary"))?;
                    return helper.call((buf, encoding));
                }
                helper.call((data, encoding))
            },
        )?,
    )?;
    Ok(buffer)
}

fn crypto_api<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let crypto = Object::new(ctx.clone())?;
    crypto.set(
        "md5",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, args: Rest<Value<'js>>| -> rquickjs::Result<String> {
            let bytes = args
                .get(0)
                .map(|v| value_material(v))
                .unwrap_or_default();
            Ok(md5_hex(&bytes).to_string())
                .or_else(|_: ()| Err(Exception::throw_message(&ctx, "md5 failed")))
        })?,
    )?;
    crypto.set(
        "randomBytes",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, args: Rest<Value<'js>>| -> rquickjs::Result<Value<'js>> {
                let size = args.get(0).and_then(|v| v.as_int()).unwrap_or(0);
                if size < 0 {
                    return Err(Exception::throw_range(&ctx, "randomBytes size"));
                }
                match random_bytes(size as usize) {
                    Ok(bytes) => bytes_to_u8(&ctx, bytes),
                    Err(err) => Err(Exception::throw_message(&ctx, &err.to_string())),
                }
            },
        )?,
    )?;
    crypto.set(
        "aesEncrypt",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, args: Rest<Value<'js>>| -> rquickjs::Result<Value<'js>> {
                if args.len() < 4 {
                    return Err(Exception::throw_message(&ctx, "aesEncrypt requires 4 arguments"));
                }
                let data = value_to_bytes(&args[0]).unwrap_or_default();
                let mode = arg_string(&args, 1);
                let key = value_material(&args[2]);
                let iv = value_material(&args[3]);
                match aes_encrypt(&mode, &key, &iv, &data) {
                    Ok(out) => bytes_to_u8(&ctx, out),
                    Err(err) => Err(Exception::throw_message(&ctx, &err.to_string())),
                }
            },
        )?,
    )?;
    crypto.set(
        "rsaEncrypt",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, args: Rest<Value<'js>>| -> rquickjs::Result<Value<'js>> {
                if args.len() < 2 {
                    return Err(Exception::throw_message(&ctx, "rsaEncrypt requires 2 arguments"));
                }
                let data = value_to_bytes(&args[0]).unwrap_or_else(|| value_material(&args[0]));
                let key = arg_string(&args, 1);
                match rsa_encrypt_no_padding(&key, &data) {
                    Ok(out) => bytes_to_u8(&ctx, out),
                    Err(err) => Err(Exception::throw_message(&ctx, &err.to_string())),
                }
            },
        )?,
    )?;
    Ok(crypto)
}

fn zlib_api<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let zlib = Object::new(ctx.clone())?;
    zlib.set(
        "inflate",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, args: Rest<Value<'js>>| -> rquickjs::Result<Value<'js>> {
                let data = args.get(0).and_then(value_to_bytes).unwrap_or_default();
                promise_bytes(&ctx, crypto::zlib_inflate(&data))
            },
        )?,
    )?;
    zlib.set(
        "deflate",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, args: Rest<Value<'js>>| -> rquickjs::Result<Value<'js>> {
                let data = args.get(0).and_then(value_to_bytes).unwrap_or_default();
                promise_bytes(&ctx, crypto::zlib_deflate(&data))
            },
        )?,
    )?;
    Ok(zlib)
}

fn lx_request<'js>(
    ctx: &Ctx<'js>,
    args: Rest<Value<'js>>,
    deadline: &AtomicU64,
) -> rquickjs::Result<Value<'js>> {
    let mut url = arg_string(&args, 0);
    if url.starts_with("//") {
        url = format!("https:{url}");
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(Exception::throw_type(ctx, "lx.request url must be http(s)"));
    }
    let mut callback: Option<Function> = None;
    let mut options: Option<Object> = None;
    if let Some(second) = args.get(1) {
        if let Ok(func) = Function::from_value(second.clone()) {
            callback = Some(func);
        } else if let Some(obj) = second.as_object() {
            options = Some(obj.clone());
        }
    }
    if let Some(third) = args.get(2) {
        if let Ok(func) = Function::from_value(third.clone()) {
            callback = Some(func);
        }
    }

    let mut method = "GET".to_string();
    let mut headers = HashMap::new();
    let mut body: Option<Vec<u8>> = None;
    let mut timeout_ms: u64 = 15_000;
    if let Some(opts) = options.as_ref() {
        if let Ok(m) = opts.get::<_, String>("method") {
            if !m.trim().is_empty() {
                method = m;
            }
        }
        if let Ok(h) = opts.get::<_, Object>("headers") {
            headers = object_string_map(&h);
        }
        if let Ok(ms) = opts.get::<_, i32>("timeout") {
            if ms > 0 {
                timeout_ms = ms as u64;
            }
        }
        if let Ok(form) = opts.get::<_, Object>("form") {
            let encoded = encode_form(&object_string_map(&form));
            if !headers.keys().any(|k| k.eq_ignore_ascii_case("content-type")) {
                headers.insert(
                    "Content-Type".into(),
                    "application/x-www-form-urlencoded".into(),
                );
            }
            body = Some(encoded.into_bytes());
        }
        if let Ok(raw) = opts.get::<_, Value>("body") {
            if !raw.is_null() && !raw.is_undefined() {
                if let Some(bytes) = value_to_bytes(&raw) {
                    body = Some(bytes);
                } else if let Some(s) = raw.as_string().and_then(|s| s.to_string().ok()) {
                    body = Some(s.into_bytes());
                } else if raw.is_object() {
                    let text = json_stringify(ctx, raw)?;
                    if !headers.keys().any(|k| k.eq_ignore_ascii_case("content-type")) {
                        headers.insert("Content-Type".into(), "application/json".into());
                    }
                    body = Some(text.into_bytes());
                }
            }
        }
    }
    timeout_ms = timeout_ms.min(60_000);
    extend_deadline(deadline, Duration::from_millis(timeout_ms + 2_000));

    let (promise, resolve, reject) = ctx.promise()?;
    match script_request(
        &method,
        &url,
        &headers,
        body.as_deref(),
        Duration::from_millis(timeout_ms),
    ) {
        Ok(resp) => {
            let obj = response_object(ctx, &resp)?;
            let _ = resolve.call::<_, Value>((obj.clone(),));
            if let Some(cb) = callback {
                let body_val: Value = obj.get("body").unwrap_or_else(|_| Value::new_null(ctx.clone()));
                let _ = cb.call::<_, Value>((Value::new_null(ctx.clone()), obj, body_val));
            }
        }
        Err(err) => {
            let obj = Object::new(ctx.clone())?;
            obj.set("message", err.to_string())?;
            let _ = reject.call::<_, Value>((obj.clone(),));
            if let Some(cb) = callback {
                let _ = cb.call::<_, Value>((obj,));
            }
        }
    }
    Ok(promise.into_value())
}

fn response_object<'js>(ctx: &Ctx<'js>, resp: &HttpResponse) -> rquickjs::Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("statusCode", i32::from(resp.status))?;
    let headers = Object::new(ctx.clone())?;
    for (k, v) in &resp.headers {
        headers.set(k.as_str(), v.as_str())?;
    }
    obj.set("headers", headers)?;
    obj.set("raw", bytes_to_u8(ctx, resp.body.clone())?)?;
    let body = match std::str::from_utf8(&resp.body) {
        Ok(text) => {
            let trim = text.trim_start();
            let ct = resp
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            if ct.contains("json") || trim.starts_with('{') || trim.starts_with('[') {
                match ctx.json_parse(text.to_owned()) {
                    Ok(v) => v,
                    Err(_) => {
                        let _ = ctx.catch();
                        rquickjs::String::from_str(ctx.clone(), text)?.into_value()
                    }
                }
            } else {
                rquickjs::String::from_str(ctx.clone(), text)?.into_value()
            }
        }
        Err(_) => Value::new_null(ctx.clone()),
    };
    obj.set("body", body)?;
    Ok(obj)
}

fn promise_bytes<'js>(ctx: &Ctx<'js>, result: Result<Vec<u8>>) -> rquickjs::Result<Value<'js>> {
    let (promise, resolve, reject) = ctx.promise()?;
    match result {
        Ok(bytes) => {
            let v = bytes_to_u8(ctx, bytes)?;
            let _ = resolve.call::<_, Value>((v,));
        }
        Err(err) => {
            let msg = rquickjs::String::from_str(ctx.clone(), &err.to_string())?;
            let _ = reject.call::<_, Value>((msg,));
        }
    }
    Ok(promise.into_value())
}

fn resolved_promise<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
    let (promise, resolve, _) = ctx.promise()?;
    let _ = resolve.call::<_, Value>((Value::new_undefined(ctx.clone()),));
    Ok(promise.into_value())
}

fn bytes_to_u8<'js>(ctx: &Ctx<'js>, bytes: Vec<u8>) -> rquickjs::Result<Value<'js>> {
    let ab = ArrayBuffer::new(ctx.clone(), bytes)?;
    let ctor: Function = ctx.globals().get("Uint8Array")?;
    ctor.call((ab,))
}

fn value_to_bytes(val: &Value<'_>) -> Option<Vec<u8>> {
    let obj = val.as_object()?;
    if let Some(ab) = ArrayBuffer::from_object(obj.clone()) {
        return Some(ab.as_bytes().unwrap_or_default().to_vec());
    }
    let buffer: Object = obj.get("buffer").ok()?;
    let offset = obj.get::<_, usize>("byteOffset").unwrap_or(0);
    let len = obj.get::<_, usize>("byteLength").unwrap_or(0);
    let ab = ArrayBuffer::from_object(buffer)?;
    let bytes = ab.as_bytes().unwrap_or_default();
    let start = offset.min(bytes.len());
    let end = start.saturating_add(len).min(bytes.len());
    Some(bytes[start..end].to_vec())
}

fn value_material(val: &Value<'_>) -> Vec<u8> {
    if let Some(s) = val.as_string().and_then(|s| s.to_string().ok()) {
        return s.into_bytes();
    }
    if let Some(i) = val.as_int() {
        return i.to_string().into_bytes();
    }
    if let Some(b) = val.as_bool() {
        return b.to_string().into_bytes();
    }
    value_to_bytes(val).unwrap_or_default()
}

fn object_string_map(obj: &Object<'_>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for key in obj.keys::<String>().flatten() {
        if let Ok(v) = obj.get::<_, String>(&key) {
            map.insert(key, v);
        } else if let Ok(v) = obj.get::<_, i32>(&key) {
            map.insert(key, v.to_string());
        } else if let Ok(v) = obj.get::<_, f64>(&key) {
            map.insert(key, v.to_string());
        } else if let Ok(v) = obj.get::<_, bool>(&key) {
            map.insert(key, v.to_string());
        }
    }
    map
}

fn encode_form(map: &HashMap<String, String>) -> String {
    let mut keys: Vec<_> = map.keys().cloned().collect();
    keys.sort();
    keys.iter()
        .map(|k| format!("{}={}", form_escape(k), form_escape(&map[k])))
        .collect::<Vec<_>>()
        .join("&")
}

fn form_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn json_stringify<'js>(ctx: &Ctx<'js>, val: Value<'js>) -> rquickjs::Result<String> {
    let json: Object = ctx.globals().get("JSON")?;
    let stringify: Function = json.get("stringify")?;
    let out: Value = stringify.call((val,))?;
    if let Some(s) = out.as_string() {
        return s.to_string();
    }
    Ok("null".into())
}

fn arg_string(args: &Rest<Value<'_>>, index: usize) -> String {
    args.get(index)
        .and_then(|v| v.as_string())
        .and_then(|s| s.to_string().ok())
        .unwrap_or_default()
}

fn explain(ctx: &Ctx<'_>, err: rquickjs::Error) -> String {
    let caught = ctx.catch();
    let msg = if let Some(s) = caught.as_string().and_then(|s| s.to_string().ok()) {
        s
    } else if let Some(obj) = caught.as_object() {
        let message = obj.get::<_, String>("message").unwrap_or_default();
        let stack = obj.get::<_, String>("stack").unwrap_or_default();
        if message.is_empty() {
            stack
        } else if stack.is_empty() {
            message
        } else {
            format!("{message}\n{stack}")
        }
    } else {
        String::new()
    };
    if msg.is_empty() {
        let text = err.to_string();
        if text.to_ascii_lowercase().contains("interrupted") {
            return "音源脚本执行超时".into();
        }
        return text;
    }
    if msg.to_ascii_lowercase().contains("interrupted") {
        "音源脚本执行超时".into()
    } else {
        msg
    }
}

fn ingest_inited(data: &Json, state: &mut InitState) {
    let sources = data
        .get("init")
        .and_then(|v| v.get("sources"))
        .or_else(|| data.get("sources"));
    let Some(sources) = sources else {
        return;
    };
    let obj = sources
        .get("sources")
        .and_then(|v| v.as_object())
        .or_else(|| sources.as_object());
    let Some(obj) = obj else {
        return;
    };
    state.platforms.clear();
    state.quality_map.clear();
    for (k, v) in obj {
        if k == "sources" || !v.is_object() {
            continue;
        }
        let qs = v
            .get("qualitys")
            .and_then(|q| q.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .filter(|list| !list.is_empty())
            .unwrap_or_else(|| vec!["128k".into()]);
        state.platforms.push(k.clone());
        state.quality_map.insert(k.clone(), qs);
    }
}

fn alert_text(v: &Json) -> String {
    match v {
        Json::String(s) => s.trim().to_string(),
        Json::Object(map) => map
            .get("log")
            .or_else(|| map.get("message"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

fn extract_url(ok: bool, payload: &str) -> Result<String> {
    if !ok {
        bail!("{}", if payload.is_empty() { "取链失败" } else { payload });
    }
    if let Some(url) = payload.strip_prefix("s:") {
        if url.starts_with("http://") || url.starts_with("https://") {
            return Ok(url.to_string());
        }
        bail!("未能获取播放地址");
    }
    if let Some(json) = payload.strip_prefix("j:") {
        if let Ok(v) = serde_json::from_str::<Json>(json) {
            if let Some(url) = v.get("url").and_then(|u| u.as_str()) {
                if url.starts_with("http://") || url.starts_with("https://") {
                    return Ok(url.to_string());
                }
            }
        }
    }
    bail!("未能获取播放地址")
}

fn extend_deadline(deadline: &AtomicU64, extra: Duration) {
    let candidate = now_ms().saturating_add(extra.as_millis() as u64);
    loop {
        let cur = deadline.load(Ordering::Relaxed);
        if candidate <= cur {
            break;
        }
        if deadline
            .compare_exchange(cur, candidate, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            break;
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(script: &str) -> ScriptMeta {
        ScriptMeta {
            name: "测试源".into(),
            version: "1.0.0".into(),
            author: "test".into(),
            description: "synthetic".into(),
            homepage: String::new(),
            raw_script: script.into(),
        }
    }

    #[test]
    fn desktop_host_async_inited_and_music_url() {
        let script = r#"
            const { EVENT_NAMES, on, send } = globalThis.lx
            if (lx.env !== 'desktop' || lx.version !== '2.0.0') throw new Error('host mismatch ' + lx.env)
            on(EVENT_NAMES.showConfigView, function () {})
            on(EVENT_NAMES.request, async (req) => {
              if (req.action !== 'musicUrl') return ''
              await lx.request('http://127.0.0.1:1/', { method: 'GET', timeout: 400 }).catch(() => null)
              return 'http://example.com/' + req.source + '/' + req.info.type + '.flac'
            })
            setTimeout(() => {
              send(EVENT_NAMES.inited, { sources: { wy: { qualitys: ['128k', '320k', 'flac'] } } })
            }, 0)
        "#;
        let session = SourceSession::open(script, &meta(script)).unwrap();
        assert_eq!(session.eval_to_string("lx.utils.crypto.md5('hello')").unwrap(), "5d41402abc4b2a76b9719d911017c592");
        let init = session.init().unwrap();
        assert_eq!(init.platforms, vec!["wy".to_string()]);
        assert_eq!(
            init.quality_map.get("wy").map(Vec::as_slice),
            Some(["128k".to_string(), "320k".into(), "flac".into()].as_slice())
        );
        let url = session
            .music_url("wy", "flac", &serde_json::json!({"name": "n", "songmid": "1"}))
            .unwrap();
        assert_eq!(url, "http://example.com/wy/flac.flac");
    }

    #[test]
    fn sync_inited_object_url_and_update_alert() {
        let script = r#"
            lx.on('request', (req) => ({ url: 'https://cdn.example/a.mp3?q=' + req.info.type }))
            lx.send('updateAlert', { log: '请更新音源' })
            lx.send('inited', { sources: { kw: { qualitys: ['128k'] } } })
        "#;
        let session = SourceSession::open(script, &meta(script)).unwrap();
        let init = session.init().unwrap();
        assert_eq!(init.update_alerts, vec!["请更新音源".to_string()]);
        let url = session
            .music_url("kw", "128k", &serde_json::json!({"songmid": "1"}))
            .unwrap();
        assert_eq!(url, "https://cdn.example/a.mp3?q=128k");
    }
}
