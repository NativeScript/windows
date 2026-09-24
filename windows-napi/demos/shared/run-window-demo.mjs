// Shared window demo body. A real, interactive Win32 window with a WinRT Composition visual tree
// rendered into it, driven identically from Node, Bun, and Deno.
//
// Windows.UI.Xaml (buttons, layout, …) needs a XAML-initialized thread that headless hosts don't
// have (see ../../test/composable-test.js). Windows.UI.Composition has no such requirement. The
// only missing piece is an HWND to render onto, which `native.createWindow` +
// `native.attachCompositorToWindow` (thin wrappers around plain Win32 window creation and
// `ICompositorDesktopInterop`) supply. `native.pumpMessages()` pumps the window's message queue
// (it peeks messages for the whole calling thread), and `native.pollWindowEvents(hwnd)` drains the
// pointer/resize/close events the window collected while pumping.
//
// What's on screen:
//   - a background that tracks the window size (resize the window)
//   - a spinning square, animated on the compositor thread by a ScalarKeyFrameAnimation: JS
//     starts it once and never touches it again
//   - a ring that follows the pointer
//   - click anywhere: a square pops in at the pointer (Vector3KeyFrameAnimation on Scale) and
//     fades out (ScalarKeyFrameAnimation on Opacity), then is removed from the tree
//
// Runs until the window is closed. Set NSWIN_DEMO_AUTOCLOSE_MS for unattended runs (CI, scripts).

const TICKS_PER_MS = 10_000; // Windows.Foundation.TimeSpan is in 100ns ticks

export async function runWindowDemo(runtimeName, api, options = {}) {
  const { Windows, native } = api;
  const env = options.env ?? {};
  const autoCloseMs = Number(env.NSWIN_DEMO_AUTOCLOSE_MS) || 0;

  const line = (s = '') => console.log(s);
  const rule = () => line('-'.repeat(60));

  line(`@nativescript/windows-napi window demo: running under ${runtimeName}`);
  rule();

  const baseTitle = `windows-napi Composition demo: ${runtimeName}`;
  const hwnd = native.createWindow(baseTitle, 800, 600);
  let { width, height } = native.getWindowSize(hwnd);
  line(`Window created (native.createWindow): client area ${width}x${height}`);

  const compositor = new Windows.UI.Composition.Compositor();
  const target = native.attachCompositorToWindow(compositor, hwnd);
  line(`Compositor attached (native.attachCompositorToWindow → ${target.__typeName__})`);

  const brush = (A, R, G, B) => compositor.CreateColorBrush({ A, R, G, B });
  const linear = compositor.CreateLinearEasingFunction();

  const root = compositor.CreateSpriteVisual();
  root.Brush = brush(255, 32, 34, 46);
  root.Size = { X: width, Y: height };
  target.Root = root;

  // Spinner: one ScalarKeyFrameAnimation on RotationAngleInDegrees, looping forever. After
  // StartAnimation the compositor runs it on its own thread. No JS per frame.
  const spinnerSize = 160;
  const spinner = compositor.CreateSpriteVisual();
  spinner.Size = { X: spinnerSize, Y: spinnerSize };
  spinner.CenterPoint = { X: spinnerSize / 2, Y: spinnerSize / 2, Z: 0 };
  spinner.Brush = brush(255, 40, 120, 220);
  const placeSpinner = () => {
    spinner.Offset = { X: (width - spinnerSize) / 2, Y: (height - spinnerSize) / 2, Z: 0 };
  };
  placeSpinner();
  const spin = compositor.CreateScalarKeyFrameAnimation();
  spin.InsertKeyFrame(0, 0);
  spin.InsertKeyFrame(1, 360, linear);
  spin.Duration = { Duration: 4000 * TICKS_PER_MS };
  spin.IterationBehavior = Windows.UI.Composition.AnimationIterationBehavior.Forever;
  spinner.StartAnimation('RotationAngleInDegrees', spin);
  root.Children.InsertAtTop(spinner);
  line('Spinner started (ScalarKeyFrameAnimation → RotationAngleInDegrees, runs on the compositor thread)');

  // Pointer follower: a ring built from two nested squares.
  const ringSize = 28;
  const ring = compositor.CreateSpriteVisual();
  ring.Size = { X: ringSize, Y: ringSize };
  ring.Brush = brush(255, 250, 200, 60);
  const ringHole = compositor.CreateSpriteVisual();
  ringHole.Size = { X: ringSize - 8, Y: ringSize - 8 };
  ringHole.Offset = { X: 4, Y: 4, Z: 0 };
  ringHole.Brush = brush(255, 32, 34, 46);
  ring.Children.InsertAtTop(ringHole);
  ring.Opacity = 0;
  root.Children.InsertAtTop(ring);

  const palette = [
    [255, 239, 83, 80],
    [255, 102, 187, 106],
    [255, 255, 202, 40],
    [255, 171, 71, 188],
    [255, 38, 198, 218],
    [255, 255, 138, 101],
  ];
  let clicks = 0;
  const spawn = (x, y) => {
    const size = 72;
    const [A, R, G, B] = palette[clicks % palette.length];
    const v = compositor.CreateSpriteVisual();
    v.Size = { X: size, Y: size };
    v.CenterPoint = { X: size / 2, Y: size / 2, Z: 0 };
    v.Offset = { X: x - size / 2, Y: y - size / 2, Z: 0 };
    v.Brush = brush(A, R, G, B);

    const pop = compositor.CreateVector3KeyFrameAnimation();
    pop.InsertKeyFrame(0, { X: 0.2, Y: 0.2, Z: 1 });
    pop.InsertKeyFrame(0.3, { X: 1.15, Y: 1.15, Z: 1 });
    pop.InsertKeyFrame(1, { X: 1, Y: 1, Z: 1 });
    pop.Duration = { Duration: 450 * TICKS_PER_MS };
    v.StartAnimation('Scale', pop);

    const fade = compositor.CreateScalarKeyFrameAnimation();
    fade.InsertKeyFrame(0, 1);
    fade.InsertKeyFrame(0.6, 1);
    fade.InsertKeyFrame(1, 0);
    fade.Duration = { Duration: 1600 * TICKS_PER_MS };
    v.StartAnimation('Opacity', fade);

    root.Children.InsertBelow(v, ring);
    setTimeout(() => root.Children.Remove(v), 1700);
    clicks++;
    native.setWindowTitle(hwnd, `${baseTitle} (${clicks} click${clicks === 1 ? '' : 's'})`);
  };

  rule();
  line(`Look for "${baseTitle}". Click inside it, move the pointer, resize it.`);
  line(autoCloseMs ? `Closing automatically in ${autoCloseMs}ms (NSWIN_DEMO_AUTOCLOSE_MS).` : 'Close the window to exit.');

  const start = Date.now();
  let resizes = 0;
  const reason = await new Promise((resolve) => {
    const iv = setInterval(() => {
      native.pumpMessages();
      for (const e of native.pollWindowEvents(hwnd)) {
        switch (e.type) {
          case 'pointermove':
            ring.Opacity = 1;
            ring.Offset = { X: e.x - ringSize / 2, Y: e.y - ringSize / 2, Z: 0 };
            break;
          case 'pointerdown':
            spawn(e.x, e.y);
            break;
          case 'resize':
            if (e.width > 0 && e.height > 0) {
              width = e.width;
              height = e.height;
              root.Size = { X: width, Y: height };
              placeSpinner();
              resizes++;
            }
            break;
          case 'close':
            clearInterval(iv);
            resolve('window closed');
            return;
        }
      }
      if (autoCloseMs && Date.now() - start > autoCloseMs) {
        clearInterval(iv);
        resolve('auto-close timeout');
      }
    }, 16);
  });

  rule();
  line(`Session ended (${reason}) after ${((Date.now() - start) / 1000).toFixed(1)}s: ${clicks} click(s), ${resizes} resize(s).`);
  line(`Done: ${runtimeName} drove an interactive WinRT Composition scene in a native window through the same napi-rs addon Node uses.`);
  return { clicks, resizes, reason };
}
