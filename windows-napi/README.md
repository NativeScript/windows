# @nativescript/windows-napi

Node-API WinRT interop for Windows.

## How this relates to the other packages

- `@nativescript/windows` — the classic V8-based app runtime.
- `@nativescript/windows-<engine>` (`windows-v8`, `windows-quickjs`, `windows-hermes`, `windows-jsc`) — the Node-API app-runtime variants, each embedding a standalone engine.
- `@nativescript/windows-napi` (this package) — the same Node-API WinRT bindings as a native addon, for use inside an existing Node, Bun, or Deno process rather than as a standalone app runtime.

## Usage

```js
const winrt = require('@nativescript/windows-napi');
```

Requires Windows (x64 or arm64) and Node >= 18 (or a Node-API-compatible runtime such as Bun or Deno).

## XAML windows

`XamlWindow` hosts `Windows.UI.Xaml` content (XAML Islands) in a Win32 window:

```js
const { Windows, XamlWindow } = require('@nativescript/windows-napi');

const win = new XamlWindow({ style: 'dialog', owner: browserWindow.getNativeWindowHandle() });
const button = new Windows.UI.Xaml.Controls.Button();
button.Content = 'OK';
button.Click = () => win.close();
win.content = button;
win.place({ width: 320, height: 120, over: browserWindow.getNativeWindowHandle() });
win.show();
```

- `style`: `'dialog'` (title bar, close box) or `'popup'` (borderless, not in the taskbar or Alt+Tab).
- Options: `owner`, `title`, `topmost`, `theme` (`'system'`, `'dark'`, `'light'`).
- Events: `activate`, `deactivate`, `close` (title-bar X or Alt+F4; closes when unhandled), `closed`.
- Helpers: `XamlWindow.foregroundWindow()`, `activateWindow(hwnd)`, `enableWindow(hwnd, enabled)`,
  `windowProcessId(hwnd)`.

The host executable's manifest must declare `<maxversiontested Id="10.0.18362.0"/>` or later;
`electron.exe` does and `node.exe` doesn't, so `XamlWindow` throws in plain Node. XAML events run
from the host's message loop, so a host with its own UI loop needs no `pumpMessages`.

## Win32

`win32` calls exported functions of any DLL:

```js
const { win32 } = require('@nativescript/windows-napi');

win32.call('user32.dll', 'MessageBeep', 'bool', { type: 'u32', value: 0 });

const { lstrlenW } = win32.bind('kernel32.dll'); // arguments typed from their JS values
lstrlenW('hello'); // 5

const user32 = win32.define('user32.dll', {
  GetSystemMetrics: ['i32'], // argument types; returns the default (i32)
  IsWindow: { args: ['pointer'], ret: 'bool' },
});
```

- Types: `void`, `bool`, `i8`–`i64`, `u8`–`u64`, `f32`, `f64`, `pointer`, `wstr` (UTF-16 string),
  `str` (UTF-8 string). 64-bit values beyond 2^53 come back as `BigInt`; arguments take numbers or
  `BigInt`s.
- A `pointer` argument also takes an `ArrayBuffer`, a typed array or `DataView` (its memory, for
  structs and out-parameters) or a pointer object. `null` is a null pointer, also for strings.
- `win32.import(dll)` installs every export of the DLL as a global function.
- `define` and `bind` resolve a function once and call it through the platform's argument
  registers; `bind` locks a function's argument types in from its first call. `call` parses the
  signature on every call: use it for one-offs.

### Callbacks and window messages

`win32.callback(retType, argTypes, fn)` returns `{ pointer, release() }`: a native function
pointer that calls `fn` synchronously, for APIs that take one (`EnumWindows`, a `WNDPROC`). Keep it
until native code no longer uses it, then `release()` it.

`win32.messageWindow(listener)` returns `{ hwnd, close() }`, a hidden window whose messages are
delivered to `listener(msg, wParam, lParam)`. Use it as the window of a tray icon (its callback
message), `RegisterHotKey` (`WM_HOTKEY`) or a popup menu (`WM_COMMAND`). It is a top-level window,
so it also receives broadcasts such as `TaskbarCreated`.

```js
const shell32 = win32.define('shell32.dll', { Shell_NotifyIconW: { args: ['u32', 'pointer'], ret: 'bool' } });
const user32 = win32.define('user32.dll', {
  LoadIconW: { args: ['pointer', 'pointer'], ret: 'pointer' },
  RegisterHotKey: { args: ['pointer', 'i32', 'u32', 'u32'], ret: 'bool' },
});

const WM_TRAY = 0x8001; // WM_APP + 1
const WM_HOTKEY = 0x0312, WM_LBUTTONUP = 0x0202;

const window = win32.messageWindow((msg, wParam, lParam) => {
  if (msg === WM_TRAY && lParam === WM_LBUTTONUP) console.log('tray icon clicked');
  if (msg === WM_HOTKEY && wParam === 1) console.log('Ctrl+Alt+S');
});

// NOTIFYICONDATAW, 64-bit layout
const nid = new DataView(new ArrayBuffer(976));
nid.setUint32(0, 976, true); // cbSize
nid.setBigUint64(8, BigInt(window.hwnd), true); // hWnd
nid.setUint32(16, 1, true); // uID
nid.setUint32(20, 0x1 | 0x2 | 0x4, true); // NIF_MESSAGE | NIF_ICON | NIF_TIP
nid.setUint32(24, WM_TRAY, true); // uCallbackMessage
nid.setBigUint64(32, BigInt(user32.LoadIconW(null, 32512)), true); // hIcon: IDI_APPLICATION
[...'My app'].forEach((c, i) => nid.setUint16(40 + i * 2, c.charCodeAt(0), true)); // szTip
shell32.Shell_NotifyIconW(0, nid); // NIM_ADD

user32.RegisterHotKey(window.hwnd, 1, 0x2 | 0x1 | 0x4000, 0x53); // Ctrl+Alt+S, MOD_NOREPEAT
```

Messages arrive while the thread pumps them: Electron's main process does; in plain Node call
`enableAutoPump()`. In the standalone runtimes an open message window keeps the event loop running.

## Embedding outside Node

Any host that provides Node-API can load the addon: load the `.node` file as a DLL and call its
`napi_register_module_v1(env, exports)`. The addon takes its `napi_*` functions from the host
executable, as in Node. A host whose executable doesn't export them sets `NAPI_HOST_MODULES`
before loading the addon: the modules that do, separated by `;`, in priority order. Each function
comes from the first module that exports it.

React Native for Windows is one: `@nativescript/react-native` runs the addon in a Hermes runtime,
where `hermes.dll` has the engine half of Node-API (`js_native_api.h`) and the module itself
implements Node's half (`node_api.h`: threadsafe functions, cleanup hooks, buffers). It sets
`NAPI_HOST_MODULES=<its DLL>;<hermes.dll>`.

## Development

```sh
npm run build        # release build via @napi-rs/cli
npm run build:debug  # debug build
npm test             # run the test suite (run-tests.js)
```
