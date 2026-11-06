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

## Development

```sh
npm run build        # release build via @napi-rs/cli
npm run build:debug  # debug build
npm test             # run the test suite (run-tests.js)
```
