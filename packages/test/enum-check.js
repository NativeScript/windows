// Standalone-engine check for generics closed over an enum (IAsyncOperation<Enum>,
// IVector<Enum>, IReference<Enum>):
//   nativescript-windows packages/test/enum-check.js
// Prints "enum-check: N passed, M failed" once the async check settles.
'use strict';

var pass = 0, fail = 0;
function check(name, ok) {
  if (ok) { pass += 1; }
  else { fail += 1; console.error('FAIL ' + name); }
}
function attempt(name, fn) {
  try { return fn(); } catch (e) { fail += 1; console.error('FAIL ' + name + ': ' + e.message); }
}

var Enumeration = Windows.Devices.Enumeration;
attempt('IVector<Enum>', function () {
  var classes = new Enumeration.DevicePicker().Filter.SupportedDeviceClasses;
  classes.Append(Enumeration.DeviceClass.AudioCapture);
  classes.Append(Enumeration.DeviceClass.VideoCapture);
  check('IVector<Enum>.Size', classes.Size === 2);
  check('IVector<Enum>.GetAt', classes.GetAt(0) === Enumeration.DeviceClass.AudioCapture);
});

var Devices = Windows.Media.Devices;
attempt('IReference<Enum>', function () {
  var focus = new Devices.FocusSettings();
  check('IReference<Enum> starts null', focus.Distance == null);
  focus.Distance = Devices.ManualFocusDistance.Nearest;
  check('IReference<Enum> round-trips', focus.Distance === Devices.ManualFocusDistance.Nearest);
  focus.Distance = null;
  check('IReference<Enum> clears to null', focus.Distance == null);
});

function done() {
  console.log('enum-check: ' + pass + ' passed, ' + fail + ' failed');
}
var op = attempt('IAsyncOperation<Enum>', function () {
  return Windows.Devices.Haptics.VibrationDevice.RequestAccessAsync();
});
if (op) {
  NSWinRT.toPromise(op).then(function (status) {
    check('IAsyncOperation<Enum> resolves to a number', typeof status === 'number');
    check('IAsyncOperation<Enum> GetResults', op.GetResults() === status);
  }, function (e) {
    check('IAsyncOperation<Enum> resolves: ' + e.message, false);
  }).then(done);
} else {
  done();
}
