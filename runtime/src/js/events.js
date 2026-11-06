// DOMException, Event, CustomEvent, EventTarget, AbortSignal and AbortController, each
// installed only when the global is missing. Plain functions and WeakMap state, so it runs on
// every engine.
(function () {
    var g = globalThis;
    var defineProperty = Object.defineProperty;

    function hidden(target, name, value) {
        defineProperty(target, name, { value: value, writable: true, configurable: true });
    }

    function scheduleTimer(fn, delay) {
        var setTimer = g.setTimeout || g.__ns__setTimeout;
        return setTimer(fn, delay);
    }

    function clearTimer(id) {
        var clear = g.clearTimeout || g.__ns__clearTimeout;
        if (typeof clear === 'function') clear(id);
    }

    // A throwing listener must not stop the others; report it as an uncaught error.
    function reportError(error) {
        if (typeof g.reportError === 'function') {
            try { g.reportError(error); return; } catch (_e) {}
        }
        scheduleTimer(function () { throw error; }, 0);
    }

    function finishInterface(ctor, tag) {
        defineProperty(ctor.prototype, Symbol.toStringTag, { value: tag, configurable: true });
    }

    function accessor(proto, name, get) {
        defineProperty(proto, name, { get: get, enumerable: true, configurable: true });
    }

    if (typeof g.DOMException !== 'function') {
        var codes = {
            IndexSizeError: 1, HierarchyRequestError: 3, WrongDocumentError: 4,
            InvalidCharacterError: 5, NoModificationAllowedError: 7, NotFoundError: 8,
            NotSupportedError: 9, InvalidStateError: 11, SyntaxError: 12,
            InvalidModificationError: 13, NamespaceError: 14, InvalidAccessError: 15,
            TypeMismatchError: 17, SecurityError: 18, NetworkError: 19, AbortError: 20,
            URLMismatchError: 21, QuotaExceededError: 22, TimeoutError: 23,
            InvalidNodeTypeError: 24, DataCloneError: 25
        };
        var exceptionState = new WeakMap();

        var DOMException = function DOMException(message, name) {
            if (!(this instanceof DOMException)) {
                throw new TypeError("Class constructor DOMException cannot be invoked without 'new'");
            }
            exceptionState.set(this, {
                message: message === undefined ? '' : String(message),
                name: name === undefined ? 'Error' : String(name)
            });
            if (typeof Error.captureStackTrace === 'function') {
                Error.captureStackTrace(this, DOMException);
            } else {
                hidden(this, 'stack', new Error(message).stack);
            }
        };
        DOMException.prototype = Object.create(Error.prototype);
        hidden(DOMException.prototype, 'constructor', DOMException);
        function exceptionField(self, field) {
            var state = exceptionState.get(self);
            if (!state) throw new TypeError('Illegal invocation');
            return state[field];
        }
        accessor(DOMException.prototype, 'name', function () { return exceptionField(this, 'name'); });
        accessor(DOMException.prototype, 'message', function () { return exceptionField(this, 'message'); });
        accessor(DOMException.prototype, 'code', function () {
            return codes[exceptionField(this, 'name')] || 0;
        });
        var constants = {
            INDEX_SIZE_ERR: 1, DOMSTRING_SIZE_ERR: 2, HIERARCHY_REQUEST_ERR: 3,
            WRONG_DOCUMENT_ERR: 4, INVALID_CHARACTER_ERR: 5, NO_DATA_ALLOWED_ERR: 6,
            NO_MODIFICATION_ALLOWED_ERR: 7, NOT_FOUND_ERR: 8, NOT_SUPPORTED_ERR: 9,
            INUSE_ATTRIBUTE_ERR: 10, INVALID_STATE_ERR: 11, SYNTAX_ERR: 12,
            INVALID_MODIFICATION_ERR: 13, NAMESPACE_ERR: 14, INVALID_ACCESS_ERR: 15,
            VALIDATION_ERR: 16, TYPE_MISMATCH_ERR: 17, SECURITY_ERR: 18, NETWORK_ERR: 19,
            ABORT_ERR: 20, URL_MISMATCH_ERR: 21, QUOTA_EXCEEDED_ERR: 22, TIMEOUT_ERR: 23,
            INVALID_NODE_TYPE_ERR: 24, DATA_CLONE_ERR: 25
        };
        Object.keys(constants).forEach(function (key) {
            var descriptor = { value: constants[key], enumerable: true };
            defineProperty(DOMException, key, descriptor);
            defineProperty(DOMException.prototype, key, descriptor);
        });
        finishInterface(DOMException, 'DOMException');
        hidden(g, 'DOMException', DOMException);
    }

    var hasEvent = typeof g.Event === 'function' && typeof g.EventTarget === 'function';
    var Event = g.Event;
    var EventTarget = g.EventTarget;
    // Set by AbortSignal below: called with (target, type, listenerCount) on every change.
    var listenerHooks = new WeakMap();

    if (!hasEvent) {
        var eventState = new WeakMap();
        var NONE = 0, AT_TARGET = 2;

        Event = function Event(type, init) {
            if (!(this instanceof Event)) {
                throw new TypeError("Class constructor Event cannot be invoked without 'new'");
            }
            if (arguments.length === 0) {
                throw new TypeError("Failed to construct 'Event': 1 argument required, but only 0 present.");
            }
            init = init || {};
            eventState.set(this, {
                type: String(type),
                bubbles: !!init.bubbles,
                cancelable: !!init.cancelable,
                composed: !!init.composed,
                defaultPrevented: false,
                target: null,
                currentTarget: null,
                phase: NONE,
                stop: false,
                stopImmediate: false,
                inPassiveListener: false,
                timeStamp: typeof g.performance === 'object' ? g.performance.now() : Date.now()
            });
        };
        function eventField(self) {
            var state = eventState.get(self);
            if (!state) throw new TypeError('Illegal invocation');
            return state;
        }
        ['type', 'bubbles', 'cancelable', 'composed', 'defaultPrevented', 'target',
            'currentTarget', 'timeStamp'].forEach(function (name) {
            accessor(Event.prototype, name, function () { return eventField(this)[name]; });
        });
        accessor(Event.prototype, 'srcElement', function () { return eventField(this).target; });
        accessor(Event.prototype, 'eventPhase', function () { return eventField(this).phase; });
        accessor(Event.prototype, 'isTrusted', function () { eventField(this); return false; });
        accessor(Event.prototype, 'returnValue', function () { return !eventField(this).defaultPrevented; });
        accessor(Event.prototype, 'cancelBubble', function () { return eventField(this).stop; });
        Event.prototype.preventDefault = function preventDefault() {
            var state = eventField(this);
            if (state.cancelable && !state.inPassiveListener) state.defaultPrevented = true;
        };
        Event.prototype.stopPropagation = function stopPropagation() {
            eventField(this).stop = true;
        };
        Event.prototype.stopImmediatePropagation = function stopImmediatePropagation() {
            var state = eventField(this);
            state.stop = true;
            state.stopImmediate = true;
        };
        Event.prototype.composedPath = function composedPath() {
            var state = eventField(this);
            return state.currentTarget ? [state.currentTarget] : [];
        };
        [['NONE', 0], ['CAPTURING_PHASE', 1], ['AT_TARGET', 2], ['BUBBLING_PHASE', 3]].forEach(function (c) {
            var descriptor = { value: c[1], enumerable: true };
            defineProperty(Event, c[0], descriptor);
            defineProperty(Event.prototype, c[0], descriptor);
        });
        finishInterface(Event, 'Event');

        var listenerState = new WeakMap();
        function listenersOf(target) {
            var map = listenerState.get(target);
            if (!map) {
                map = Object.create(null);
                listenerState.set(target, map);
            }
            return map;
        }
        function notifyListeners(target, type, list) {
            var hook = listenerHooks.get(Object.getPrototypeOf(target)) || listenerHooks.get(target);
            if (hook) hook(target, type, list.length);
        }
        function captureOf(options) {
            if (typeof options === 'boolean') return options;
            return !!(options && typeof options === 'object' && options.capture);
        }

        EventTarget = function EventTarget() {
            if (!(this instanceof EventTarget)) {
                throw new TypeError("Class constructor EventTarget cannot be invoked without 'new'");
            }
            listenersOf(this);
        };
        EventTarget.prototype.addEventListener = function addEventListener(type, callback, options) {
            if (callback === null || callback === undefined) return;
            type = String(type);
            var capture = captureOf(options);
            var once = false, passive = false, signal;
            if (options && typeof options === 'object') {
                once = !!options.once;
                passive = !!options.passive;
                signal = options.signal;
            }
            if (signal && signal.aborted) return;
            var map = listenersOf(this);
            var list = map[type] || (map[type] = []);
            for (var i = 0; i < list.length; i++) {
                if (list[i].callback === callback && list[i].capture === capture) return;
            }
            var entry = { callback: callback, capture: capture, once: once, passive: passive, removed: false };
            list.push(entry);
            notifyListeners(this, type, list);
            if (signal && typeof signal.addEventListener === 'function') {
                var self = this;
                signal.addEventListener('abort', function () {
                    self.removeEventListener(type, callback, { capture: capture });
                }, { once: true });
            }
        };
        EventTarget.prototype.removeEventListener = function removeEventListener(type, callback, options) {
            type = String(type);
            var capture = captureOf(options);
            var map = listenersOf(this);
            var list = map[type];
            if (!list) return;
            for (var i = 0; i < list.length; i++) {
                if (list[i].callback === callback && list[i].capture === capture) {
                    list[i].removed = true;
                    list.splice(i, 1);
                    notifyListeners(this, type, list);
                    return;
                }
            }
        };
        EventTarget.prototype.dispatchEvent = function dispatchEvent(event) {
            var state = eventState.get(event);
            if (!state) {
                throw new TypeError("Failed to execute 'dispatchEvent' on 'EventTarget': parameter 1 is not of type 'Event'.");
            }
            if (state.phase !== NONE) {
                throw new g.DOMException('The event is already being dispatched.', 'InvalidStateError');
            }
            state.target = this;
            state.currentTarget = this;
            state.phase = AT_TARGET;
            var map = listenersOf(this);
            var list = map[state.type];
            if (list) {
                var snapshot = list.slice();
                for (var i = 0; i < snapshot.length; i++) {
                    var entry = snapshot[i];
                    if (entry.removed) continue;
                    if (entry.once) this.removeEventListener(state.type, entry.callback, { capture: entry.capture });
                    state.inPassiveListener = entry.passive;
                    try {
                        var callback = entry.callback;
                        if (typeof callback === 'function') {
                            callback.call(this, event);
                        } else if (callback && typeof callback.handleEvent === 'function') {
                            callback.handleEvent(event);
                        }
                    } catch (error) {
                        reportError(error);
                    }
                    state.inPassiveListener = false;
                    if (state.stopImmediate) break;
                }
            }
            state.currentTarget = null;
            state.phase = NONE;
            state.stop = false;
            state.stopImmediate = false;
            return !state.defaultPrevented;
        };
        finishInterface(EventTarget, 'EventTarget');

        hidden(g, 'Event', Event);
        hidden(g, 'EventTarget', EventTarget);
    }

    if (typeof g.CustomEvent !== 'function') {
        var details = new WeakMap();
        var CustomEvent = function CustomEvent(type, init) {
            if (!(this instanceof CustomEvent)) {
                throw new TypeError("Class constructor CustomEvent cannot be invoked without 'new'");
            }
            var event = Reflect.construct(Event, [type, init], new.target || CustomEvent);
            details.set(event, init && init.detail !== undefined ? init.detail : null);
            return event;
        };
        CustomEvent.prototype = Object.create(Event.prototype);
        hidden(CustomEvent.prototype, 'constructor', CustomEvent);
        Object.setPrototypeOf(CustomEvent, Event);
        accessor(CustomEvent.prototype, 'detail', function () {
            if (!details.has(this)) throw new TypeError('Illegal invocation');
            return details.get(this);
        });
        finishInterface(CustomEvent, 'CustomEvent');
        hidden(g, 'CustomEvent', CustomEvent);
    }

    if (typeof g.AbortController === 'function' && typeof g.AbortSignal === 'function') return;

    var makeWeakRef = typeof g.WeakRef === 'function'
        ? function (value) { return new g.WeakRef(value); }
        : function (value) { return { deref: function () { return value; } }; };

    var signalState = new WeakMap();
    var internal = {};
    // Timeout signals and composites stay reachable while someone listens for their abort.
    var observed = new Set();

    var timers;
    // Cancels the pending timer of a collected timeout signal.
    function timerRegistry() {
        if (timers === undefined) {
            timers = typeof g.FinalizationRegistry === 'function'
                ? new g.FinalizationRegistry(function (id) { clearTimer(id); })
                : null;
        }
        return timers;
    }

    function abortError() {
        return new g.DOMException('This operation was aborted', 'AbortError');
    }

    function stateOf(signal) {
        var state = signalState.get(signal);
        if (!state) throw new TypeError('Illegal invocation');
        return state;
    }

    var AbortSignal = function AbortSignal(token) {
        if (token !== internal) throw new TypeError('Illegal constructor');
        var self = Reflect.construct(EventTarget, [], new.target || AbortSignal);
        signalState.set(self, {
            aborted: false,
            reason: undefined,
            timeout: false,
            composite: false,
            sources: null,
            dependents: null,
            selfRef: null,
            onabort: null,
            onabortListener: null
        });
        return self;
    };
    AbortSignal.prototype = Object.create(EventTarget.prototype);
    hidden(AbortSignal.prototype, 'constructor', AbortSignal);
    Object.setPrototypeOf(AbortSignal, EventTarget);

    function createSignal(aborted, reason) {
        var signal = new AbortSignal(internal);
        var state = stateOf(signal);
        state.aborted = aborted;
        state.reason = reason;
        return signal;
    }

    function refOf(signal) {
        var state = stateOf(signal);
        return state.selfRef || (state.selfRef = makeWeakRef(signal));
    }

    function signalAbort(signal, reason) {
        var state = stateOf(signal);
        if (state.aborted) return;
        state.aborted = true;
        state.reason = reason;
        observed.delete(signal);
        var dependents = state.dependents || [];
        state.dependents = null;
        state.sources = null;
        // Every affected signal is aborted before the first abort event fires.
        var toFire = [signal];
        for (var i = 0; i < dependents.length; i++) {
            var dependent = dependents[i].deref();
            if (!dependent) continue;
            var dependentState = stateOf(dependent);
            if (dependentState.aborted) continue;
            dependentState.aborted = true;
            dependentState.reason = reason;
            dependentState.sources = null;
            observed.delete(dependent);
            toFire.push(dependent);
        }
        for (var j = 0; j < toFire.length; j++) {
            EventTarget.prototype.dispatchEvent.call(toFire[j], new Event('abort'));
        }
    }

    listenerHooks.set(AbortSignal.prototype, function (signal, type, count) {
        if (type !== 'abort') return;
        var state = signalState.get(signal);
        if (!state || state.aborted) return;
        var live = state.timeout || (state.composite && state.sources && state.sources.length > 0);
        if (!live) return;
        if (count > 0) observed.add(signal); else observed.delete(signal);
    });

    accessor(AbortSignal.prototype, 'aborted', function () { return stateOf(this).aborted; });
    accessor(AbortSignal.prototype, 'reason', function () { return stateOf(this).reason; });
    defineProperty(AbortSignal.prototype, 'onabort', {
        enumerable: true,
        configurable: true,
        get: function () { return stateOf(this).onabort; },
        set: function (value) {
            var state = stateOf(this);
            if (typeof value !== 'function' && (typeof value !== 'object' || value === null)) value = null;
            state.onabort = value;
            if (!state.onabortListener) {
                state.onabortListener = function (event) {
                    var handler = stateOf(this).onabort;
                    if (typeof handler === 'function') return handler.call(this, event);
                };
                this.addEventListener('abort', state.onabortListener);
            }
        }
    });
    AbortSignal.prototype.throwIfAborted = function throwIfAborted() {
        var state = stateOf(this);
        if (state.aborted) throw state.reason;
    };

    AbortSignal.abort = function abort(reason) {
        return createSignal(true, reason === undefined ? abortError() : reason);
    };

    AbortSignal.timeout = function timeout(delay) {
        if (typeof delay !== 'number') {
            throw new TypeError('The "delay" argument must be of type number');
        }
        if (!Number.isInteger(delay) || delay < 0 || delay > 4294967295) {
            throw new RangeError('The value of "delay" is out of range. It must be an integer >= 0 and <= 4294967295. Received ' + delay);
        }
        var signal = createSignal(false, undefined);
        stateOf(signal).timeout = true;
        // The timer holds the signal weakly: an unobserved signal can be collected first.
        var ref = makeWeakRef(signal);
        var registry = timerRegistry();
        var id = scheduleTimer(function () {
            var target = ref.deref();
            if (!target) return;
            if (registry) registry.unregister(target);
            signalAbort(target, new g.DOMException('The operation was aborted due to timeout', 'TimeoutError'));
        }, delay);
        if (registry) registry.register(signal, id, signal);
        return signal;
    };

    AbortSignal.any = function any(signals) {
        if (signals === null || (typeof signals !== 'object' && typeof signals !== 'function') ||
            typeof signals[Symbol.iterator] !== 'function') {
            throw new TypeError('signals is not iterable');
        }
        var list = [];
        var iterator = signals[Symbol.iterator]();
        for (var step = iterator.next(); !step.done; step = iterator.next()) {
            if (!signalState.has(step.value)) {
                throw new TypeError('signals must contain only AbortSignal instances');
            }
            list.push(step.value);
        }
        var result = createSignal(false, undefined);
        var resultState = stateOf(result);
        resultState.composite = true;
        for (var i = 0; i < list.length; i++) {
            var sourceState = stateOf(list[i]);
            if (sourceState.aborted) {
                resultState.aborted = true;
                resultState.reason = sourceState.reason;
                return result;
            }
        }
        resultState.sources = [];
        var resultRef = refOf(result);
        function link(source) {
            var ref = refOf(source);
            if (resultState.sources.indexOf(ref) !== -1) return;
            resultState.sources.push(ref);
            var state = stateOf(source);
            (state.dependents || (state.dependents = [])).push(resultRef);
            // A followed timeout signal stays alive until it fires.
            if (state.timeout) observed.add(source);
        }
        for (var j = 0; j < list.length; j++) {
            var state = stateOf(list[j]);
            if (state.composite) {
                // Composites are flattened: follow their sources directly.
                var sources = state.sources || [];
                for (var k = 0; k < sources.length; k++) {
                    var source = sources[k].deref();
                    if (source) link(source);
                }
            } else {
                link(list[j]);
            }
        }
        return result;
    };

    ['aborted', 'reason', 'onabort', 'throwIfAborted'].forEach(function (name) {
        var descriptor = Object.getOwnPropertyDescriptor(AbortSignal.prototype, name);
        descriptor.enumerable = true;
        defineProperty(AbortSignal.prototype, name, descriptor);
    });
    finishInterface(AbortSignal, 'AbortSignal');

    var controllerSignals = new WeakMap();
    var AbortController = function AbortController() {
        if (!(this instanceof AbortController)) {
            throw new TypeError("Class constructor AbortController cannot be invoked without 'new'");
        }
        controllerSignals.set(this, createSignal(false, undefined));
    };
    function signalOf(controller) {
        var signal = controllerSignals.get(controller);
        if (!signal) throw new TypeError('Illegal invocation');
        return signal;
    }
    accessor(AbortController.prototype, 'signal', function () { return signalOf(this); });
    AbortController.prototype.abort = function abort(reason) {
        signalAbort(signalOf(this), reason === undefined ? abortError() : reason);
    };
    finishInterface(AbortController, 'AbortController');

    hidden(g, 'AbortSignal', AbortSignal);
    hidden(g, 'AbortController', AbortController);
})();
