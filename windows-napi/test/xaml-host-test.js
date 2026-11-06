// Agile delegates (DispatcherQueueTimer.Tick) and XamlWindow: XAML Islands in a Win32 window.
const { Windows, XamlWindow, native } = require('../nswinrt.js');

let pass = 0, fail = 0;
function check(name, got, expected) {
  const ok = typeof expected === 'function' ? expected(got) : Object.is(got, expected);
  if (ok) pass++;
  else { console.log(`FAIL ${name}: got ${String(got)}, expected ${String(expected)}`); fail++; }
}

async function pumpUntil(cond, ms = 3000) {
  const start = Date.now();
  while (!cond() && Date.now() - start < ms) {
    native.pumpMessages();
    await new Promise((r) => setImmediate(r));
  }
  return cond();
}

(async () => {
  // DispatcherQueueTimer only accepts agile handlers.
  const timer = Windows.System.DispatcherQueue.GetForCurrentThread().CreateTimer();
  timer.Interval = { Duration: 200000 }; // 20ms
  timer.IsRepeating = false;
  let ticks = 0;
  let added = true;
  try { timer.Tick = () => { ticks++; }; } catch (e) { added = false; console.log(e.message); }
  check('Tick accepts a JS handler', added, true);
  timer.Start();
  check('Tick fires', await pumpUntil(() => ticks > 0), true);

  let win;
  try {
    win = new XamlWindow({ style: 'popup', topmost: true, theme: 'dark' });
  } catch (e) {
    // XAML Islands need maxversiontested in the host manifest; without it, check the error.
    check('clear error without island support', /maxversiontested/.test(e.message), true);
    console.log('XamlWindow checks skipped: this host cannot run XAML Islands');
    console.log(`\n${pass} passed, ${fail} failed`);
    process.exit(fail ? 1 : 0);
  }
  check('hwnd is a number', typeof win.hwnd === 'number' && win.hwnd > 0, true);
  check('theme resolved', win.theme, 'dark');
  const text = new Windows.UI.Xaml.Controls.TextBlock();
  text.Text = 'island';
  win.content = text;
  check('content set', win.content.Text, 'island');

  const events = [];
  win.on('closed', () => events.push('closed'));
  win.place({ width: 320, height: 80, y: 0.25 });
  check('show without activation', win.show({ activate: false }), true);
  check('visible after show', win.visible, true);
  win.hide();
  check('hidden after hide', win.visible, false);

  let threw = false;
  try { native.xamlHostShow(12345, true); } catch { threw = true; }
  check('unknown handle rejected', threw, true);
  check('foregroundWindow is a number', typeof XamlWindow.foregroundWindow(), 'number');
  check('own pid', XamlWindow.windowProcessId(win.hwnd), process.pid);

  win.close();
  check('closed event', await pumpUntil(() => events.includes('closed')), true);
  check('closed flag', win.closed, true);
  check('not visible once closed', win.visible, false);

  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})();
