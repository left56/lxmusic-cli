globalThis.global = globalThis;
globalThis.window = globalThis;

globalThis.__miyinTimers = [];
globalThis.__miyinTimerSeq = 1;
globalThis.__lx_handlers = [];

function __miyinArm(fn, ms, interval) {
  if (typeof fn !== "function") {
    throw new TypeError("timer callback must be a function");
  }
  if (globalThis.__miyinTimers.length >= 32) {
    throw new Error("沙箱定时器数量超限");
  }
  var id = globalThis.__miyinTimerSeq++;
  var delay = Math.max(0, Number(ms) || 0);
  globalThis.__miyinTimers.push({
    id: id,
    fn: fn,
    delay: delay,
    interval: !!interval,
    due: Date.now() + delay,
  });
  return id;
}

globalThis.setTimeout = function (fn, ms) {
  return __miyinArm(fn, ms, false);
};
globalThis.setInterval = function (fn, ms) {
  return __miyinArm(fn, ms, true);
};
globalThis.clearTimeout = globalThis.clearInterval = function (id) {
  globalThis.__miyinTimers = globalThis.__miyinTimers.filter(function (t) {
    return t.id !== id;
  });
};

if (typeof queueMicrotask !== "function") {
  globalThis.queueMicrotask = function (fn) {
    Promise.resolve().then(fn);
  };
}

function __miyinUtf8Encode(str) {
  var s = String(str);
  var out = [];
  for (var i = 0; i < s.length; i++) {
    var c = s.charCodeAt(i);
    if (c < 128) out.push(c);
    else if (c < 2048) out.push(192 | (c >> 6), 128 | (c & 63));
    else if (c >= 0xd800 && c <= 0xdbff && i + 1 < s.length) {
      var c2 = s.charCodeAt(++i);
      var u = 65536 + ((c & 1023) << 10) + (c2 & 1023);
      out.push(240 | (u >> 18), 128 | ((u >> 12) & 63), 128 | ((u >> 6) & 63), 128 | (u & 63));
    } else out.push(224 | (c >> 12), 128 | ((c >> 6) & 63), 128 | (c & 63));
  }
  var u8 = new Uint8Array(out.length);
  for (var j = 0; j < out.length; j++) u8[j] = out[j];
  return u8;
}
function __miyinUtf8Decode(arr) {
  var out = "";
  var i = 0;
  while (i < arr.length) {
    var c = arr[i++];
    if (c < 128) out += String.fromCharCode(c);
    else if (c < 224) out += String.fromCharCode(((c & 31) << 6) | (arr[i++] & 63));
    else if (c < 240) out += String.fromCharCode(((c & 15) << 12) | ((arr[i++] & 63) << 6) | (arr[i++] & 63));
    else {
      var u = ((c & 7) << 18) | ((arr[i++] & 63) << 12) | ((arr[i++] & 63) << 6) | (arr[i++] & 63);
      u -= 65536;
      out += String.fromCharCode(0xd800 + (u >> 10), 0xdc00 + (u & 1023));
    }
  }
  return out;
}
if (typeof TextEncoder === "undefined") {
  globalThis.TextEncoder = function TextEncoder() {};
  globalThis.TextEncoder.prototype.encode = function (s) {
    return __miyinUtf8Encode(s);
  };
}
if (typeof TextDecoder === "undefined") {
  globalThis.TextDecoder = function TextDecoder() {};
  globalThis.TextDecoder.prototype.decode = function (bytes) {
    var arr = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes || []);
    return __miyinUtf8Decode(arr);
  };
}

function __miyinBytesFromString(text, encoding) {
  var enc = String(encoding || "utf8").toLowerCase();
  if (enc === "hex") {
    var clean = String(text).replace(/\s+/g, "");
    if (clean.length % 2) clean = "0" + clean;
    var out = new Uint8Array(clean.length / 2);
    for (var i = 0; i < out.length; i++) {
      out[i] = parseInt(clean.substr(i * 2, 2), 16) || 0;
    }
    return out;
  }
  if (enc === "base64") {
    var bin = atob(String(text).replace(/\s+/g, ""));
    var b = new Uint8Array(bin.length);
    for (var j = 0; j < bin.length; j++) b[j] = bin.charCodeAt(j) & 255;
    return b;
  }
  if (enc === "binary" || enc === "latin1" || enc === "ascii") {
    var s = String(text);
    var a = new Uint8Array(s.length);
    for (var k = 0; k < s.length; k++) a[k] = s.charCodeAt(k) & 255;
    return a;
  }
  return new TextEncoder().encode(String(text));
}

function __miyinToString(bytes, encoding) {
  var arr = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes || []);
  var enc = String(encoding || "utf8").toLowerCase();
  if (enc === "hex") {
    var h = "";
    for (var i = 0; i < arr.length; i++) {
      var n = arr[i].toString(16);
      h += n.length < 2 ? "0" + n : n;
    }
    return h;
  }
  if (enc === "base64") {
    var bin = "";
    for (var j = 0; j < arr.length; j++) bin += String.fromCharCode(arr[j]);
    return btoa(bin);
  }
  if (enc === "binary" || enc === "latin1" || enc === "ascii") {
    var s = "";
    for (var k = 0; k < arr.length; k++) s += String.fromCharCode(arr[k]);
    return s;
  }
  return new TextDecoder().decode(arr);
}

function __miyinAsBuffer(u8) {
  try {
    Object.setPrototypeOf(u8, BufferPoly.prototype);
  } catch (e) {}
  u8.toString = function (enc) {
    return __miyinToString(this, enc || "utf8");
  };
  return u8;
}
function BufferPoly(arg) {
  if (typeof arg === "number") return __miyinAsBuffer(new Uint8Array(arg));
  if (arg instanceof Uint8Array) return __miyinAsBuffer(new Uint8Array(arg));
  return __miyinAsBuffer(new Uint8Array(0));
}
BufferPoly.from = function (data, encoding) {
  var u8;
  if (typeof data === "string") u8 = __miyinBytesFromString(data, encoding);
  else if (data instanceof ArrayBuffer) u8 = new Uint8Array(data);
  else if (data && data.buffer instanceof ArrayBuffer && typeof data.byteLength === "number") {
    u8 = new Uint8Array(data.buffer, data.byteOffset || 0, data.byteLength);
  } else if (Array.isArray(data)) {
    u8 = new Uint8Array(data.length);
    for (var i = 0; i < data.length; i++) u8[i] = data[i] & 255;
  } else u8 = new Uint8Array(0);
  return __miyinAsBuffer(u8);
};
BufferPoly.alloc = function (n) {
  return BufferPoly.from(new Uint8Array(Number(n) || 0));
};
BufferPoly.concat = function (list) {
  var size = 0;
  for (var i = 0; i < list.length; i++) size += list[i].length || list[i].byteLength || 0;
  var out = new Uint8Array(size);
  var off = 0;
  for (var j = 0; j < list.length; j++) {
    var part = list[j] instanceof Uint8Array ? list[j] : new Uint8Array(list[j]);
    out.set(part, off);
    off += part.length;
  }
  return __miyinAsBuffer(out);
};
BufferPoly.isBuffer = function (b) {
  return b instanceof Uint8Array;
};
globalThis.Buffer = BufferPoly;

globalThis.atob = globalThis.atob || function (s) {
  var chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  var str = String(s).replace(/=+$/, "").replace(/\s+/g, "");
  var out = "";
  var buf = 0;
  var bits = 0;
  for (var i = 0; i < str.length; i++) {
    var val = chars.indexOf(str.charAt(i));
    if (val < 0) continue;
    buf = (buf << 6) | val;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out += String.fromCharCode((buf >> bits) & 255);
    }
  }
  return out;
};
globalThis.btoa = globalThis.btoa || function (s) {
  var chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  var str = String(s);
  var out = "";
  for (var i = 0; i < str.length; i += 3) {
    var a = str.charCodeAt(i) & 255;
    var b = i + 1 < str.length ? str.charCodeAt(i + 1) & 255 : 0;
    var c = i + 2 < str.length ? str.charCodeAt(i + 2) & 255 : 0;
    var triple = (a << 16) | (b << 8) | c;
    out += chars.charAt((triple >> 18) & 63);
    out += chars.charAt((triple >> 12) & 63);
    out += i + 1 < str.length ? chars.charAt((triple >> 6) & 63) : "=";
    out += i + 2 < str.length ? chars.charAt(triple & 63) : "=";
  }
  return out;
};

if (typeof URLSearchParams === "undefined") {
  globalThis.URLSearchParams = function URLSearchParams(init) {
    this._pairs = [];
    if (typeof init === "string") {
      var text = init.charAt(0) === "?" ? init.slice(1) : init;
      if (text) {
        var parts = text.split("&");
        for (var i = 0; i < parts.length; i++) {
          var kv = parts[i].split("=");
          this.append(decodeURIComponent(kv[0] || ""), decodeURIComponent(kv.slice(1).join("=") || ""));
        }
      }
    } else if (init && typeof init === "object") {
      for (var k in init) this.append(k, init[k]);
    }
  };
  globalThis.URLSearchParams.prototype.append = function (k, v) {
    this._pairs.push([String(k), String(v)]);
  };
  globalThis.URLSearchParams.prototype.get = function (k) {
    for (var i = 0; i < this._pairs.length; i++) {
      if (this._pairs[i][0] === String(k)) return this._pairs[i][1];
    }
    return null;
  };
  globalThis.URLSearchParams.prototype.toString = function () {
    return this._pairs
      .map(function (p) {
        return encodeURIComponent(p[0]) + "=" + encodeURIComponent(p[1]);
      })
      .join("&");
  };
}

globalThis.fetch = function (url, options) {
  options = options || {};
  var headers = options.headers || {};
  if (headers instanceof URLSearchParams) headers = {};
  return globalThis.lx
    .request(String(url), {
      method: options.method,
      headers: headers,
      body: options.body,
    })
    .then(function (resp) {
      var text = typeof resp.body === "string" ? resp.body : resp.body == null ? "" : JSON.stringify(resp.body);
      return {
        ok: resp.statusCode >= 200 && resp.statusCode < 300,
        status: resp.statusCode,
        headers: {
          get: function (name) {
            var want = String(name || "").toLowerCase();
            var h = resp.headers || {};
            for (var k in h) {
              if (String(k).toLowerCase() === want) return h[k];
            }
            return null;
          },
        },
        text: function () {
          return Promise.resolve(text);
        },
        json: function () {
          return Promise.resolve(typeof resp.body === "string" ? JSON.parse(resp.body) : resp.body);
        },
        arrayBuffer: function () {
          return Promise.resolve(resp.raw || new ArrayBuffer(0));
        },
      };
    });
};

function XMLHttpRequest() {
  this.readyState = 0;
  this.status = 0;
  this.responseText = "";
  this._headers = {};
  this._method = "GET";
  this._url = "";
}
XMLHttpRequest.prototype.open = function (method, url) {
  this._method = method || "GET";
  this._url = url;
  this.readyState = 1;
};
XMLHttpRequest.prototype.setRequestHeader = function (k, v) {
  this._headers[k] = v;
};
XMLHttpRequest.prototype.send = function (body) {
  var self = this;
  globalThis.lx
    .request(this._url, { method: this._method, headers: this._headers, body: body })
    .then(
      function (resp) {
        self.status = resp.statusCode;
        self.responseText = typeof resp.body === "string" ? resp.body : JSON.stringify(resp.body);
        self.readyState = 4;
        if (typeof self.onreadystatechange === "function") self.onreadystatechange();
        if (typeof self.onload === "function") self.onload();
      },
      function (err) {
        if (typeof self.onerror === "function") self.onerror(err);
      }
    );
};
globalThis.XMLHttpRequest = XMLHttpRequest;

globalThis.document = {
  cookie: "",
  referrer: "",
  querySelector: function () {
    return null;
  },
  getElementById: function () {
    return null;
  },
  createElement: function () {
    return { style: {}, setAttribute: function () {}, appendChild: function () {} };
  },
};
globalThis.navigator = {
  userAgent:
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
  platform: "MacIntel",
};
globalThis.location = { href: "https://lx-music.local/", protocol: "https:", host: "lx-music.local" };
globalThis.localStorage = {
  _m: {},
  getItem: function (k) {
    return Object.prototype.hasOwnProperty.call(this._m, k) ? this._m[k] : null;
  },
  setItem: function (k, v) {
    this._m[k] = String(v);
  },
  removeItem: function (k) {
    delete this._m[k];
  },
};

globalThis.__miyinInvoke = function (payloadJson) {
  var payload = JSON.parse(payloadJson);
  var hs = globalThis.__lx_handlers || [];
  if (!hs.length) {
    globalThis.__miyinSettle(false, "音源未注册 musicUrl 处理函数");
    return;
  }
  try {
    var ret = hs[0](payload);
    Promise.resolve(ret).then(
      function (v) {
        if (typeof v === "string") globalThis.__miyinSettle(true, "s:" + v);
        else {
          try {
            globalThis.__miyinSettle(true, "j:" + JSON.stringify(v));
          } catch (e) {
            globalThis.__miyinSettle(true, "s:" + String(v));
          }
        }
      },
      function (e) {
        globalThis.__miyinSettle(false, (e && (e.stack || e.message)) || String(e));
      }
    );
  } catch (e) {
    globalThis.__miyinSettle(false, (e && (e.stack || e.message)) || String(e));
  }
};
