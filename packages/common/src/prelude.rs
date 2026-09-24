//! JS runtime prelude for the standalone hosts, evaluated after `install_globals` and the URL
//! polyfill. Provides the pieces a bare engine lacks that are pure JS over the napi natives:
//!   - `queueMicrotask` (over the engine's own promise queue) when the engine doesn't expose it,
//!   - `NSWinRT.toPromise(op)`: converts a WinRT IAsyncOperation/IAsyncAction to a Promise
//!     (same idea as nswinrt.js's `toPromise` for the Node package). It holds the event loop
//!     open via the `__nsLoopRetain`/`__nsLoopRelease` natives while an operation is
//!     outstanding, so `await NSWinRT.toPromise(op)` "just works" under `run_event_loop`.
//!
//! Kept engine-conservative (function expressions, no async/await syntax) so one source runs
//! unchanged on QuickJS, Hermes, V8, and JSC.

pub const PRELUDE: &str = r#"
(function (g) {
  'use strict';

  // Node-style `global` alias for `globalThis`: @nativescript/core and webpack `target: 'node'`
  // output both reference bare `global` (e.g. `global.foo = ...`). Classic rusty_v8 sets this via
  // `init_global` (a read-only own property); defined the same way here so it runs on every napi
  // engine.
  if (typeof g.global === 'undefined') {
    Object.defineProperty(g, 'global', { value: g, writable: false, configurable: true });
  }

  // DevTools host hooks @nativescript/core's debugger domains call at module evaluation
  // (`@DomainDispatcher` decorators; a Vite dev build imports them unconditionally). The classic
  // runtime backs them with its inspector; the napi engines have none, so dispatchers are only
  // kept for inspection and events are dropped.
  if (typeof g.__registerDomainDispatcher !== 'function') {
    var domainDispatchers = {};
    Object.defineProperty(g, '__nsInspectorDomainDispatchers', { value: domainDispatchers, configurable: true });
    g.__registerDomainDispatcher = function (domain, dispatcher) {
      domainDispatchers[String(domain)] = dispatcher;
    };
  }
  if (typeof g.__inspectorSendEvent !== 'function') {
    g.__inspectorSendEvent = function () {};
  }
  if (typeof g.__inspectorTimestamp !== 'function') {
    g.__inspectorTimestamp = function () { return Date.now(); };
  }

  // WHATWG WebSocket over the native WinHTTP socket (runtime/src/websocket.rs, bound by
  // napi_engine::websocket), as the classic runtime's HELPER_SOURCE defines it. Events are
  // delivered on the JS thread by the event loop.
  if (typeof g.WebSocket !== 'function' && typeof g.__nsWebSocketOpen === 'function') {
    var WebSocket = function WebSocket(url, protocols) {
      if (!(this instanceof WebSocket)) { throw new TypeError("Failed to construct 'WebSocket': Please use the 'new' operator"); }
      var href = String(url);
      if (!/^wss?:\/\//i.test(href)) {
        if (/^https?:\/\//i.test(href)) { href = href.replace(/^http/i, 'ws'); }
        else { throw new SyntaxError("Failed to construct 'WebSocket': The URL '" + href + "' is invalid."); }
      }
      var list = protocols === undefined ? [] : (Array.isArray(protocols) ? protocols : [protocols]).map(String);
      this.url = href;
      this.protocol = '';
      this.extensions = '';
      this.readyState = 0;
      this.bufferedAmount = 0;
      this.binaryType = 'arraybuffer';
      this.onopen = null;
      this.onmessage = null;
      this.onerror = null;
      this.onclose = null;
      this._listeners = {};
      var self = this;
      this._id = g.__nsWebSocketOpen(href, list, function (type, a, b, c) {
        if (type === 'open') {
          self.readyState = 1;
          self.protocol = a || '';
          self._emit({ type: 'open', target: self });
        } else if (type === 'message') {
          if (self.readyState === 1) { self._emit({ type: 'message', data: a, origin: self.url, target: self }); }
        } else if (type === 'error') {
          self._emit({ type: 'error', message: a, target: self });
        } else if (type === 'close') {
          self.readyState = 3;
          self._emit({ type: 'close', code: a, reason: b || '', wasClean: !!c, target: self });
        }
      });
    };
    WebSocket.CONNECTING = WebSocket.prototype.CONNECTING = 0;
    WebSocket.OPEN = WebSocket.prototype.OPEN = 1;
    WebSocket.CLOSING = WebSocket.prototype.CLOSING = 2;
    WebSocket.CLOSED = WebSocket.prototype.CLOSED = 3;
    WebSocket.prototype._emit = function (event) {
      var handler = this['on' + event.type];
      if (typeof handler === 'function') { handler.call(this, event); }
      var list = (this._listeners[event.type] || []).slice();
      for (var i = 0; i < list.length; i++) {
        try { list[i].call(this, event); } catch (e) { setTimeout(function () { throw e; }, 0); }
      }
    };
    WebSocket.prototype.addEventListener = function (type, listener) {
      if (typeof listener !== 'function') { return; }
      var list = this._listeners[type] || (this._listeners[type] = []);
      if (list.indexOf(listener) < 0) { list.push(listener); }
    };
    WebSocket.prototype.removeEventListener = function (type, listener) {
      var list = this._listeners[type];
      if (list) { var i = list.indexOf(listener); if (i >= 0) { list.splice(i, 1); } }
    };
    WebSocket.prototype.dispatchEvent = function (event) { this._emit(event); return true; };
    WebSocket.prototype.send = function (data) {
      if (this.readyState === 0) { throw new Error("Failed to execute 'send' on 'WebSocket': Still in CONNECTING state."); }
      if (this.readyState !== 1) { return; }
      g.__nsWebSocketSend(this._id, data);
    };
    WebSocket.prototype.close = function (code, reason) {
      if (this.readyState >= 2) { return; }
      this.readyState = 2;
      g.__nsWebSocketClose(this._id, code === undefined ? 1000 : code, reason === undefined ? '' : String(reason));
    };
    g.WebSocket = WebSocket;
  }

  if (typeof g.queueMicrotask !== 'function') {
    g.queueMicrotask = function (cb) {
      if (typeof cb !== 'function') { throw new TypeError('queueMicrotask expects a function'); }
      Promise.resolve().then(cb);
    };
  }

  var retain = typeof g.__nsLoopRetain === 'function' ? g.__nsLoopRetain : function () {};
  var release = typeof g.__nsLoopRelease === 'function' ? g.__nsLoopRelease : function () {};

  function statusEnum() {
    try { return g.Windows.Foundation.AsyncStatus; }
    catch (e) { return { Started: 0, Completed: 1, Canceled: 2, Error: 3 }; }
  }

  function normalizeStatus(status) {
    if (status == null) { return NaN; }
    if (typeof status === 'number') { return status; }
    var n = Number(status);
    return isNaN(n) ? NaN : n;
  }

  // Convert a WinRT IAsyncOperation/IAsyncAction proxy to a JS Promise via its Completed event,
  // Status property, and GetResults() method. Mirrors nswinrt.js (Node package); the pump there
  // is a ref-counted Node timer, here it is the standalone event loop's keep-alive counter.
  function toPromise(op) {
    if (op == null || (typeof op !== 'object' && typeof op !== 'function')) {
      return Promise.resolve(op);
    }
    if (typeof op.then === 'function' && !('Completed' in op)) { return op; }

    var S = statusEnum();
    retain();
    return new Promise(function (resolve, reject) {
      var settled = false;
      function done(fn, arg) { settled = true; release(); fn(arg); }
      function settle(override) {
        if (settled) { return; }
        try {
          var status = normalizeStatus(override !== undefined ? override : op.Status);
          if (status === S.Completed || status === 1) {
            done(resolve, typeof op.GetResults === 'function' ? op.GetResults() : undefined);
          } else if (status === S.Canceled || status === 2) {
            done(reject, new Error('WinRT async operation was canceled'));
          } else if (status === S.Error || status === 3) {
            done(reject, op.ErrorCode || new Error('WinRT async operation failed'));
          }
        } catch (err) { done(reject, err); }
      }

      var initial = normalizeStatus(op.Status);
      if (!isNaN(initial) && initial !== 0) { settle(initial); return; }
      op.Completed = function (asyncInfo, asyncStatus) { settle(asyncStatus); };
      // Race guard: it may have completed between the status read and handler assignment.
      var race = normalizeStatus(op.Status);
      if (!isNaN(race) && race !== 0) { settle(race); }
    });
  }

  g.NSWinRT = g.NSWinRT || {};
  g.NSWinRT.toPromise = toPromise;

  // core calls the non-standard `.get()` on WeakRef.
  if (typeof g.WeakRef !== 'undefined' && typeof g.WeakRef.prototype.get !== 'function') {
    g.WeakRef.prototype.get = g.WeakRef.prototype.deref;
    g.WeakRef.prototype.__hasWarnedAboutClear = false;
    g.WeakRef.prototype.clear = function () {
      if (g.WeakRef.prototype.__hasWarnedAboutClear) { return; }
      g.WeakRef.prototype.__hasWarnedAboutClear = true;
      console.warn('WeakRef.clear() is non-standard and has been deprecated. It does nothing and the call can be safely removed.');
    };
  }
})(globalThis);

// CommonJS shim: webpack `target: 'node'` bundles (what NativeScript apps are built as) expect
// `require`/`module`/`exports`/`__dirname`/`__filename` as globals: normally supplied by Node's
// own module wrapper. This runtime is not Node, so we supply them here the same way the classic
// rusty_v8 runtime does (`global_fns::HELPER_SOURCE`), backed by the `__nsResolveModulePath` /
// `__nsReadTextFile` / `__nsAppRoot` natives `host_abi::initialize_runtime` installs. Without this, every
// chunk (runtime.js/vendor.js/the app bundle) throws `ReferenceError: require is not defined` on
// evaluation.
(function (g) {
  'use strict';

  if (typeof g.require === 'function' && typeof g.module !== 'undefined') {
    return;
  }
  if (typeof g.__nsResolveModulePath !== 'function' || typeof g.__nsReadTextFile !== 'function') {
    return;
  }

  var cjsCache = new Map();

  function resolveSpecifier(specifier, callerFile) {
    if (typeof specifier !== 'string' || specifier.length === 0) {
      throw new Error('Cannot find module: ' + String(specifier));
    }
    var appRoot = (g.__nsAppRoot || '').replace(/[\\\/]+$/, '');

    // NativeScript tilde alias: ~/foo -> {appRoot}/app/foo
    if (specifier.charAt(0) === '~' && specifier.charAt(1) === '/') {
      var abs = appRoot + '\\app\\' + specifier.substring(2).replace(/\//g, '\\');
      return g.__nsResolveModulePath(abs, '', appRoot) || abs;
    }

    // Relative (./foo, ../foo) or bare name: use native resolver with caller context.
    // Fall back to app/bundle.js as parent so top-level require('./chunk.js') works.
    var parent = callerFile || (appRoot + '\\app\\bundle.js');
    return g.__nsResolveModulePath(specifier, parent, appRoot);
  }

  function makeRequire(callerFile) {
    return function require(specifier) {
      if (specifier === 'ns:module' && g.__nsModuleBuiltin) { return g.__nsModuleBuiltin; }
      var resolved = resolveSpecifier(specifier, callerFile);
      if (!resolved) { throw new Error('Cannot find module: ' + specifier); }

      var key = resolved.replace(/\\/g, '/').toLowerCase();
      if (cjsCache.has(key)) { return cjsCache.get(key).exports; }

      var mod = { id: resolved, filename: resolved, exports: {} };
      cjsCache.set(key, mod); // set before eval to break circular deps

      var isJson = key.slice(-5) === '.json';
      var content = g.__nsReadTextFile(resolved);

      if (isJson) {
        try { mod.exports = JSON.parse(content || '{}'); } catch (_e) { mod.exports = {}; }
        return mod.exports;
      }

      var dirName = resolved.replace(/\//g, '\\').replace(/\\[^\\]*$/, '');
      var childRequire = makeRequire(resolved);

      try {
        var factory = new Function('module', 'exports', 'require', '__filename', '__dirname', content);
        factory(mod, mod.exports, childRequire, resolved, dirName);
      } catch (e) {
        cjsCache.delete(key);
        throw e;
      }
      cjsCache.set(key, mod);
      return mod.exports;
    };
  }

  g.require = makeRequire(null);

  // The `ns:module` builtin (require/import/import()): the dev-loader control surface
  // @nativescript/vite's HMR client drives, as on iOS/Android and the classic runtime. Present
  // only on engines with a module loader (the natives come from the engine package). Members are
  // frozen and non-throwing to feature-detect.
  if (!g.__nsModuleBuiltin && typeof g.__nsModuleConfigureLoader === 'function') {
    var nsModule = {
      configureLoader: function configureLoader(config) {
        if (!config || typeof config !== 'object') { throw new TypeError('configureLoader expects a config object'); }
        g.__nsModuleConfigureLoader(JSON.stringify(config));
      },
      invalidateModules: function invalidateModules(urls) {
        return g.__nsModuleInvalidate(urls);
      },
      getLoadedModuleUrls: function getLoadedModuleUrls() {
        return g.__nsModuleLoadedUrls();
      },
      createRequire: function createRequire(filenameOrURL) {
        var value = filenameOrURL && typeof filenameOrURL === 'object' ? filenameOrURL.href : filenameOrURL;
        if (typeof value !== 'string') {
          throw new TypeError("The argument 'filename' must be a file URL object, file URL string, or absolute path string.");
        }
        if (/^https?:/.test(value)) {
          throw new TypeError('createRequire() cannot take an http(s) URL (' + value + '): use import() for remote modules.');
        }
        if (value.indexOf('file:') === 0) {
          value = decodeURIComponent(value.replace(/^file:\/\/(localhost)?/, '').replace(/[?#].*$/, '')).replace(/^\/([A-Za-z]:)/, '$1');
        }
        return makeRequire(value);
      }
    };
    Object.defineProperty(g, '__nsModuleBuiltin', { value: Object.freeze(nsModule), enumerable: false });
  }

  // Top-level CJS globals for scripts executed outside a factory wrapper (e.g. when the host
  // calls runtime_runscript directly with a CJS file: runtime.js/vendor.js/the app bundle).
  if (typeof g.module === 'undefined') {
    var _topMod = { id: '<main>', exports: {} };
    Object.defineProperty(g, 'module',  { value: _topMod, writable: true, configurable: true });
    Object.defineProperty(g, 'exports', { value: _topMod.exports, writable: true, configurable: true });
  }

  // Provide __dirname / __filename globals for webpack target:'node' bundles. webpack leaves
  // these undefined when building for node (expects Node.js to provide them via its module
  // wrapper); this runtime supplies the app directory as a reasonable fallback value.
  if (typeof g.__dirname === 'undefined') {
    var _appRoot2 = (g.__nsAppRoot || '').replace(/[\\\/]+$/, '');
    var _appDir = _appRoot2 + '\\app';
    Object.defineProperty(g, '__dirname',  { value: _appDir, writable: true, configurable: true });
    Object.defineProperty(g, '__filename', { value: _appDir + '\\bundle.js', writable: true, configurable: true });
  }
})(globalThis);
'prelude-ok'
"#;
