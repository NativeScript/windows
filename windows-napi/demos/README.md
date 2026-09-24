# windows-napi demos

Small, runnable proof that `@nativescript/windows-napi` is a normal Node-API addon: the exact
same WinRT calls (`Windows.Globalization.Calendar`, `Windows.Security.Cryptography`,
`Windows.Data.Json`, `Windows.System.Threading.ThreadPool` async → `Promise`) run unmodified
under Node, Bun, and Deno. Only the loading shim per runtime differs. See
[`../../docs/napi-consumption.md`](../../docs/napi-consumption.md) for why.

```
demos/
  shared/run-demo.mjs         console demo: WinRT calls, identical on every runtime
  shared/run-ui-demo.mjs      UI demo: opens a real OS window via WinRT, identical on every runtime
  shared/run-window-demo.mjs  window demo: interactive WinRT Composition scene in our own window
  node/index.js               Node console-demo entry point (CommonJS require + dynamic import)
  node/ui-demo.js             Node UI-demo entry point
  node/window-demo.js         Node window-demo entry point
  bun/index.js                Bun console-demo entry point
  bun/ui-demo.js              Bun UI-demo entry point
  bun/window-demo.js          Bun window-demo entry point
  deno/main.js                Deno console-demo entry point (createRequire, package isn't on npm here)
  deno/ui-demo.js             Deno UI-demo entry point
  deno/window-demo.js         Deno window-demo entry point
```

Every entry point imports the *built package* directly (`../../nswinrt.js` /
`../../index.js`), so they need the native addon already built once:

```powershell
cd ..
npm run build          # or build:debug. Produces windows.win32-<arch>-msvc.node
```

## Bun

Requires Bun on Windows (JavaScriptCore + Node-API). Install: `winget install Oven-sh.Bun` (or
see bun.sh), then restart your shell so `bun` is on `PATH`. Then:

```powershell
cd bun
bun run index.js
```

Verified working (Bun 1.3.14, 2026-08-07): every step below ran against real WinRT, no flags
needed:

```
@nativescript/windows-napi demo: running under Bun 1.3.14 (JavaScriptCore)
------------------------------------------------------------
WinRT clock (Windows.Globalization.Calendar): 2026-08-07 06:59:53
Random session token (CryptographicBuffer.GenerateRandom): 5fc004ac305ec3730f11ef00d28a0795
JSON round-trip (Windows.Data.Json.JsonObject.Stringify): {"runtime":"Bun 1.3.14 (JavaScriptCore)",...}
Async round-trip (ThreadPool.RunAsync → Promise): completed=true in 46.42ms
------------------------------------------------------------
(table of the four steps)
uuid (interop.nsUuid via installInterop): 529fd632-65e9-4069-b726-6359ace78de7
------------------------------------------------------------
Done: Bun 1.3.14 (JavaScriptCore) drove real Windows Runtime APIs through the same napi-rs addon Node uses.
```

## Deno

Requires Deno on Windows (V8 + Node-API via the Node-compat layer). Install:
`winget install DenoLand.Deno` (or see deno.com), then restart your shell so `deno` is on
`PATH`. Then:

```powershell
cd deno
deno run --allow-ffi --allow-read --allow-env --allow-write main.js
```

Verified working (Deno 2.9.5, 2026-08-07): same shared demo body, same output shape, no
`PermissionDenied` even though all four flags were granted up front:

```
@nativescript/windows-napi demo: running under Deno 2.9.5 (V8)
------------------------------------------------------------
WinRT clock (Windows.Globalization.Calendar): 2026-08-07 07:00:03
...
Done: Deno 2.9.5 (V8) drove real Windows Runtime APIs through the same napi-rs addon Node uses.
```

- `--allow-ffi`: required to load any native (`.node`) addon.
- `--allow-read` / `--allow-env` / `--allow-write`: cover the WinRT interop layer's winmd
  scanning and timer bookkeeping; nothing in this particular demo touches the filesystem, but
  leaving them off will surface as a `PermissionDenied` the moment a future demo step does.

## Node (baseline)

The fully-tested path, for comparison. Same shared demo bodies:

```powershell
cd node
node index.js        # console demo
node ui-demo.js      # UI demo
node window-demo.js  # window demo
```

## UI demo (real OS window)

`shared/run-ui-demo.mjs` calls `Windows.System.Launcher.LaunchUriAsync` to open a URL in the
default browser: a real, visible OS window, driven entirely through WinRT. It's one of the few
WinRT UI-adjacent APIs that works unmodified from an unpackaged console host: it hands the URI to
the shell instead of needing package identity or a XAML-initialized thread the way
`Windows.UI.Xaml` / `Windows.UI.Notifications` do (see `../test/composable-test.js` for why XAML
types don't work headless).

```powershell
cd bun
bun run ui-demo.js
# or: cd deno && deno run --allow-ffi --allow-read --allow-env --allow-write ui-demo.js
# or: cd node && node <demo>.js
```

Verified working (Bun 1.3.14 / Deno 2.9.5 / Node, 2026-08-07):

```
@nativescript/windows-napi UI demo: running under Bun 1.3.14 (JavaScriptCore)
------------------------------------------------------------
Built URI (Windows.Foundation.Uri): https://github.com/NativeScript/windows-runtime
Launched via the shell (Windows.System.Launcher.LaunchUriAsync): true
------------------------------------------------------------
Done: Bun 1.3.14 (JavaScriptCore) opened a real OS window through the same napi-rs addon Node uses.
```

This demo is also what surfaced (and led to fixing) a real crash: `LaunchUriAsync` shares its
public name with two other `Launcher` overloads (`LaunchUriAsync(uri, options)` /
`(uri, options, data)`) declared on sibling statics interfaces (`ILauncherStatics2`/`3`). The
runtime's static-method resolver had a name-only fallback that, for methods whose signature
bytes didn't byte-match across interfaces, picked whichever same-named overload metadata
enumerated first, regardless of arity. That silently computed a real, in-bounds vtable slot on
the *wrong* interface and called it with too few arguments: a null-pointer access violation
inside `Windows.System.Launcher.dll` itself, not a catchable JS error. Fixed by requiring the
fallback to also match parameter count (`metadata/src/declaring_interface_for_method.rs`); a
related but separate bug. Same "first name match wins" pattern, this time in the fast
host-object call path (`runtime/src/napi_engine/ns_hostobject.rs`): meant *any* WinRT class with
same-named overloads across arities could resolve to the wrong one, not just `Launcher`.

## Window demo (interactive WinRT Composition scene in our own window)

`shared/run-window-demo.mjs` goes one step further than the UI demo above: instead of opening
*someone else's* window (the browser), it creates its own Win32 window and renders a live
`Windows.UI.Composition` scene into it, driven entirely from Node/Bun/Deno:

- a background that tracks the window size (resize the window)
- a spinning square animated on the compositor thread by a `ScalarKeyFrameAnimation`: JS starts
  it once and never touches it again
- a ring that follows the pointer
- click anywhere: a square pops in at the pointer (`Vector3KeyFrameAnimation` on `Scale`), fades out
  (`ScalarKeyFrameAnimation` on `Opacity`) and is removed from the tree; the title bar counts clicks

It runs until you close the window. Set `NSWIN_DEMO_AUTOCLOSE_MS` for unattended runs.

```powershell
cd node && node window-demo.js
# or: cd bun && bun run window-demo.js
# or: cd deno && deno run --allow-ffi --allow-read --allow-env --allow-write window-demo.js
```

`Windows.UI.Xaml` needs a XAML-initialized thread the napi engine doesn't have (see the UI demo
section above), so the runtime exposes a few thin Win32 helpers for this:

- **`native.createWindow(title, width, height)`**: plain Win32 window creation, with
  `WS_EX_NOREDIRECTIONBITMAP` (no GDI redirection bitmap; without it the window's own backing
  surface covers the Composition tree) and the window forced to the foreground (a window created
  by a non-foreground process can otherwise render correctly and still show nothing on screen).
- **`native.attachCompositorToWindow(compositor, hwnd)`**: `ICompositorDesktopInterop::CreateDesktopWindowTarget`,
  returned as a normal WinRT proxy; set its `.Root` to a visual.
- **`native.pollWindowEvents(hwnd)`**: drains the `pointerdown`/`pointerup`/`pointermove`/`resize`/`close`
  events the window queued since the last poll (client-area pixels, moves coalesced). Events are
  polled rather than delivered as callbacks because the window procedure runs nested inside the
  message pump, and calling back into JS from there is re-entrancy each engine handles differently.
- **`native.getWindowSize(hwnd)`** / **`native.setWindowTitle(hwnd, title)`**.

`native.pumpMessages()` dispatches the window's messages. It peeks messages for the whole calling
thread, so the same pump that drives WinRT async completions drives the window.

Verified (Node 24.15 / Bun 1.3.14 / Deno 2.9.5, 2026-09-23) by driving the window with posted
pointer messages, a resize and a `WM_CLOSE`, and screenshotting it:

```
@nativescript/windows-napi window demo: running under Bun 1.3.14 (JavaScriptCore)
------------------------------------------------------------
Window created (native.createWindow): client area 784x561
Compositor attached (native.attachCompositorToWindow → Windows.UI.Composition.Desktop.DesktopWindowTarget)
Spinner started (ScalarKeyFrameAnimation → RotationAngleInDegrees, runs on the compositor thread)
------------------------------------------------------------
Look for "windows-napi Composition demo: Bun 1.3.14 (JavaScriptCore)". Click inside it, move the pointer, resize it.
Close the window to exit.
------------------------------------------------------------
Session ended (window closed) after 3.1s: 3 click(s), 2 resize(s).
Done: Bun 1.3.14 (JavaScriptCore) drove an interactive WinRT Composition scene in a native window through the same napi-rs addon Node uses.
```

Things this demo's build-out surfaced, worth knowing if you extend it:

- **Overloads are callable by their public name.** WinRT tags same-interface overloads with an
  `[Overload]` metadata name: `Compositor.CreateColorBrush(Color)` is `CreateColorBrushWithColor`.
  The napi engine used to expose each overload *only* under that name, so `CreateColorBrush(color)`
  silently resolved to the 0-arg overload and dropped the argument (transparent black). The public
  name now dispatches on argument count across every overload; the `[Overload]` names still work.
- **`CW_USEDEFAULT` window positioning cascades with every window a process creates in a
  session**, eventually off-screen. `createWindow` uses a fixed position instead.

## Not included: React Native Windows

RN Windows runs its JS on Hermes/JSI, which is not a Node-API host the way Node/Bun/Deno are.
So the `.node` addon can't be `require()`-d from RN's JS thread the way it is here. Consuming
`windows-napi` from RN Windows needs either a sidecar Node/Bun process (IPC over stdio/socket)
or a native C++/JSI TurboModule that calls the underlying Rust WinRT crate directly, bypassing
napi-rs entirely. That's a separate, bigger piece of work than these demos and isn't attempted
here.
