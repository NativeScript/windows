// NSWinRT JS helper layer (ships with the package alongside the native addon).
// Pure JS that operates on the native WinRT proxies — async→Promise, event helpers.
// Mirrors the runtime's HELPER_SOURCE `toPromise`, adapted for the napi backend where the
// host owns the loop (the consumer pumps via `pumpMessages`).
'use strict';
const native = require('./index.js');

const Windows = native.getNamespace('Windows');

function statusEnum() {
  try { return Windows.Foundation.AsyncStatus; }
  catch { return { Started: 0, Completed: 1, Canceled: 2, Error: 3 }; }
}

function normalizeStatus(status) {
  if (status == null) return NaN;
  if (typeof status === 'number') return status;
  const n = Number(status);
  return Number.isNaN(n) ? NaN : n;
}

// Pump lifecycle: while any WinRT async op is outstanding, the pump timer is ref'd so Node
// stays alive to deliver Completed; when the last settles, it is unref'd so the process can
// exit. (An unref'd-only timer would let Node exit before completion fires.)
let pending = 0;
function retainPump() {
  pending += 1;
  if (autoPumpTimer && typeof autoPumpTimer.ref === 'function') autoPumpTimer.ref();
}
function releasePump() {
  pending = Math.max(0, pending - 1);
  if (pending === 0 && autoPumpTimer && typeof autoPumpTimer.unref === 'function') {
    autoPumpTimer.unref();
  }
}

// Convert a WinRT IAsyncOperation/IAsyncAction proxy to a JS Promise via its Completed event,
// Status property, and GetResults() method — all provided by the native proxy layer.
function toPromise(op) {
  if (op == null || (typeof op !== 'object' && typeof op !== 'function')) {
    return Promise.resolve(op);
  }
  if (typeof op.then === 'function' && !('Completed' in op)) return op;

  const S = statusEnum();
  retainPump();
  return new Promise((resolve, reject) => {
    let settled = false;
    const done = (fn, arg) => { settled = true; releasePump(); fn(arg); };
    const settle = (override) => {
      if (settled) return;
      try {
        const status = normalizeStatus(override !== undefined ? override : op.Status);
        if (status === S.Completed || status === 1) {
          done(resolve, typeof op.GetResults === 'function' ? op.GetResults() : undefined);
        } else if (status === S.Canceled || status === 2) {
          done(reject, new Error('WinRT async operation was canceled'));
        } else if (status === S.Error || status === 3) {
          done(reject, op.ErrorCode || new Error('WinRT async operation failed'));
        }
      } catch (err) { done(reject, err); }
    };

    const initial = normalizeStatus(op.Status);
    if (!Number.isNaN(initial) && initial !== 0) { settle(initial); return; }
    op.Completed = (asyncInfo, asyncStatus) => settle(asyncStatus);
    // Race guard: it may have completed between the status read and handler assignment.
    const race = normalizeStatus(op.Status);
    if (!Number.isNaN(race) && race !== 0) settle(race);
  });
}

// Drive the Windows message loop until `promise` settles or `timeoutMs` elapses. WinRT STA
// async completions are delivered as messages, so the loop must be pumped for them to fire.
async function awaitWithPump(promise, timeoutMs = 5000) {
  let done = false, result, error, ok;
  promise.then((r) => { done = true; ok = true; result = r; },
              (e) => { done = true; ok = false; error = e; });
  const start = Date.now();
  while (!done && Date.now() - start < timeoutMs) {
    native.pumpMessages();
    await new Promise((r) => setImmediate(r));
  }
  if (!done) throw new Error('awaitWithPump timed out');
  if (!ok) throw error;
  return result;
}

// Pump the WinRT message loop automatically on a Node timer so awaiting WinRT Promises is
// transparent (no manual `awaitWithPump`). The timer is unref'd, so it never keeps the process
// alive on its own. Returns a stop function. Safe to call once at startup.
let autoPumpTimer = null;
function enableAutoPump(intervalMs = 4) {
  if (autoPumpTimer) return () => disableAutoPump();
  autoPumpTimer = setInterval(() => { native.pumpMessages(); }, intervalMs);
  if (typeof autoPumpTimer.unref === 'function') autoPumpTimer.unref();
  return () => disableAutoPump();
}
function disableAutoPump() {
  if (autoPumpTimer) { clearInterval(autoPumpTimer); autoPumpTimer = null; }
}

// interop.* — the full NSWinRT.interop surface (Pointer/OutParam, reference/typed-value
// boxing, buffer + DateTime utilities, uuid/winmd registration, zero-copy
// arrayBufferFromBuffer). Installed by the shared native layer so the surface is identical
// across Node/Bun/Deno and the standalone engine hosts.
native.installInterop();
const interop = globalThis.NSWinRT.interop;

// dotnet.* — the .NET/BCL bridge (NSWinRT.dotnet: invoke/get/fromHandle/registerNamespace,
// taskToPromise/asDelegate, plus NSWinRT.runOnUIThread). A no-op at the JS layer until a
// dotnet-bridge/publish/DotNetBridge.dll exists next to the app.
native.installDotnet();
const dotnet = globalThis.NSWinRT.dotnet;

// Windows.UI.Xaml content in a Win32 window (XAML Islands). Needs a thread that pumps messages.
let xamlManager = null;

function systemTheme() {
  const vm = Windows.UI.ViewManagement;
  const bg = new vm.UISettings().GetColorValue(vm.UIColorType.Background);
  return bg.R < 128 ? 'dark' : 'light';
}

class XamlWindow {
  /**
   * @param {object} [options]
   * @param {'dialog'|'popup'} [options.style] dialog: title bar and close box; popup: borderless,
   *   not in the taskbar or Alt+Tab.
   * @param {number|bigint|Buffer} [options.owner] owner window handle.
   * @param {string} [options.title]
   * @param {boolean} [options.topmost]
   * @param {'system'|'dark'|'light'} [options.theme] title bar theme; read it back from `theme`.
   */
  constructor(options = {}) {
    if (!xamlManager) {
      try {
        xamlManager = Windows.UI.Xaml.Hosting.WindowsXamlManager.InitializeForCurrentThread();
      } catch (e) {
        if (!/0x8000FFFF/i.test(String(e && e.message))) throw e;
        throw new Error('XamlWindow: XAML Islands are unavailable in this process. The host ' +
          'executable\'s manifest must declare <maxversiontested Id="10.0.18362.0"/> or later.');
      }
    }
    const { theme = 'system', ...rest } = options;
    this.theme = theme === 'system' ? systemTheme() : theme;
    this.source = new Windows.UI.Xaml.Hosting.DesktopWindowXamlSource();
    this.hwnd = native.xamlHostCreate(this.source, { ...rest, dark: this.theme === 'dark' });
    this.closed = false;
    this._handlers = { activate: [], deactivate: [], close: [], closed: [] };
    native.xamlHostSetListener(this.hwnd, (type) => this._emit(type));
  }

  get content() { return this.source.Content; }
  set content(element) { this.source.Content = element; }

  get visible() { return !this.closed && native.xamlHostIsVisible(this.hwnd); }

  /** `activate`, `deactivate`, `close` (title-bar X or Alt+F4; closes when unhandled), `closed`. */
  on(type, fn) {
    if (!this._handlers[type]) throw new TypeError(`XamlWindow: unknown event '${type}'`);
    this._handlers[type].push(fn);
    return this;
  }

  off(type, fn) {
    const list = this._handlers[type];
    if (list && list.includes(fn)) list.splice(list.indexOf(fn), 1);
    return this;
  }

  /** Client size in DIPs; centered over `over`, or on the pointer's monitor at `y` (0..1). */
  place({ width, height, over, y } = {}) {
    native.xamlHostPlace(this.hwnd, { width, height, over, y });
    return this;
  }

  /** Returns whether the window is in the foreground. */
  show({ activate = true } = {}) { return native.xamlHostShow(this.hwnd, activate); }

  hide() { if (!this.closed) native.xamlHostHide(this.hwnd); }

  close() { if (!this.closed) native.xamlHostDestroy(this.hwnd); }

  _emit(type) {
    if (type === 'closed') {
      this.closed = true;
      try { this.source.Close(); } catch {}
    }
    const handlers = this._handlers[type].slice();
    if (type === 'close' && handlers.length === 0) this.close();
    for (const fn of handlers) {
      // Rethrown outside the native callback so the host reports it as uncaught.
      try { fn(); } catch (e) { setTimeout(() => { throw e; }); }
    }
  }

  static systemTheme() { return systemTheme(); }
  /** The foreground window of any process, as a number (0 if none). */
  static foregroundWindow() { return native.foregroundWindow(); }
  /** Bring a window to the foreground; `false` if Windows refuses. */
  static activateWindow(hwnd) { return native.activateWindow(hwnd); }
  static enableWindow(hwnd, enabled) { native.enableWindow(hwnd, enabled); }
  static windowProcessId(hwnd) { return native.windowProcessId(hwnd); }
}

module.exports = {
  native,
  Windows,
  toPromise,
  awaitWithPump,
  enableAutoPump,
  disableAutoPump,
  interop,
  dotnet,
  XamlWindow,
};
