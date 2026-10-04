use v8::{ValueDeserializerHelper, ValueSerializerHelper};

enum Token {
    Number(f64),
    String(String),
}

struct Transferred {
    kind: String,
    token: Token,
}

pub struct Message {
    bytes: Vec<u8>,
    transferred: Vec<Transferred>,
}

/// `__nsRegisterTransferable(name, { test, detach, attach, release? })`: `detach` returns a number
/// or string token; the receiving isolate's handler of the same name `attach`es it, or `release`s
/// it when the message can't be received. ArrayBuffers in a transfer list are copied, then detached.
pub fn install_transfer_runtime(scope: &mut v8::ContextScope<v8::HandleScope>) {
    let source = r#"
    (function () {
        if (typeof globalThis.__nsRegisterTransferable === 'function') {
            return;
        }
        var handlers = Object.create(null);

        function hidden(name, value) {
            Object.defineProperty(globalThis, name, { value: value, configurable: true, writable: true });
        }

        function release(kinds, tokens, from) {
            for (var i = from; i < kinds.length; i++) {
                var handler = handlers[kinds[i]];
                if (handler && typeof handler.release === 'function') {
                    try { handler.release(tokens[i]); } catch (_) {}
                }
            }
        }

        hidden('__nsRegisterTransferable', function (name, handler) {
            if (typeof name !== 'string' || !handler || typeof handler.test !== 'function' ||
                typeof handler.detach !== 'function' || typeof handler.attach !== 'function') {
                throw new TypeError('__nsRegisterTransferable(name, { test, detach, attach, release? })');
            }
            handlers[name] = handler;
        });

        hidden('__nsTransferHooks', {
            typeOf: function (value) {
                for (var name in handlers) {
                    if (handlers[name].test(value)) {
                        return name;
                    }
                }
                return undefined;
            },
            // Sender: all or none. A throw releases what was taken.
            detachAll: function (kinds, values) {
                var tokens = [];
                try {
                    for (var i = 0; i < values.length; i++) {
                        var token = handlers[kinds[i]].detach(values[i]);
                        if (typeof token !== 'number' && typeof token !== 'string') {
                            tokens.push(token);
                            throw new TypeError("A '" + kinds[i] + "' transfer handler returned neither a number nor a string.");
                        }
                        tokens.push(token);
                    }
                } catch (e) {
                    release(kinds, tokens, 0);
                    throw e;
                }
                return tokens;
            },
            // Receiver: never throws. On an error the rest are released.
            attachAll: function (kinds, tokens) {
                var objects = [];
                for (var i = 0; i < kinds.length; i++) {
                    try {
                        var handler = handlers[kinds[i]];
                        if (!handler) {
                            throw new Error("DataCloneError: nothing here receives a transferred '" + kinds[i] + "'.");
                        }
                        var object = handler.attach(tokens[i]);
                        if (object === null || (typeof object !== 'object' && typeof object !== 'function')) {
                            throw new TypeError("A '" + kinds[i] + "' transfer handler returned no object.");
                        }
                        objects.push(object);
                    } catch (e) {
                        release(kinds, tokens, i + 1);
                        return { error: String((e && e.message) || e) };
                    }
                }
                return { objects: objects };
            }
        });
    })();
    "#;
    let Some(source) = v8::String::new(scope, source) else {
        return;
    };
    if let Some(script) = v8::Script::compile(scope, source, None) {
        script.run(scope);
    }
}

fn throw_data_clone_error(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    let Some(text) = v8::String::new(scope, &format!("DataCloneError: {message}")) else {
        return;
    };
    let error = v8::Exception::error(scope, text);
    if let (Ok(object), Some(key), Some(name)) = (
        v8::Local::<v8::Object>::try_from(error),
        v8::String::new(scope, "name"),
        v8::String::new(scope, "DataCloneError"),
    ) {
        object.set(scope, key.into(), name.into());
    }
    scope.throw_exception(error);
}

fn throw_type_error(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    if let Some(text) = v8::String::new(scope, message) {
        let error = v8::Exception::type_error(scope, text);
        scope.throw_exception(error);
    }
}

fn hooks<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Object>> {
    let context = scope.get_current_context();
    let global = context.global(scope);
    let key = v8::String::new(scope, "__nsTransferHooks")?;
    global.get(scope, key.into())?.try_into().ok()
}

/// `None`: an exception is pending.
fn call_hook<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    hooks: v8::Local<'s, v8::Object>,
    name: &str,
    args: &[v8::Local<'s, v8::Value>],
) -> Option<v8::Local<'s, v8::Value>> {
    let key = v8::String::new(scope, name)?;
    let function: v8::Local<v8::Function> = hooks.get(scope, key.into())?.try_into().ok()?;
    function.call(scope, hooks.into(), args)
}

/// The transfer list: `postMessage(data, [..])` or `postMessage(data, { transfer: [..] })`.
fn transfer_list<'s>(scope: &mut v8::PinScope<'s, '_>, transfer: v8::Local<'s, v8::Value>) -> Option<Vec<v8::Local<'s, v8::Value>>> {
    if transfer.is_null_or_undefined() {
        return Some(Vec::new());
    }
    let list = if transfer.is_array() {
        transfer
    } else if let Ok(options) = v8::Local::<v8::Object>::try_from(transfer) {
        let key = v8::String::new(scope, "transfer")?;
        let list = options.get(scope, key.into())?;
        if list.is_null_or_undefined() {
            return Some(Vec::new());
        }
        list
    } else {
        throw_type_error(scope, "postMessage's transfer must be an array");
        return None;
    };
    let Ok(array) = v8::Local::<v8::Array>::try_from(list) else {
        throw_type_error(scope, "postMessage's transfer must be an array");
        return None;
    };
    (0..array.length()).map(|index| array.get_index(scope, index)).collect()
}

struct Serializer {
    objects: Vec<v8::Global<v8::Object>>,
}

impl Serializer {
    fn index_of(&self, scope: &mut v8::PinScope<'_, '_>, object: v8::Local<v8::Object>) -> Option<u32> {
        self.objects
            .iter()
            .position(|known| v8::Local::new(scope, known).strict_equals(object.into()))
            .map(|index| index as u32)
    }
}

impl v8::ValueSerializerImpl for Serializer {
    fn throw_data_clone_error<'s>(&self, scope: &mut v8::PinScope<'s, '_>, message: v8::Local<'s, v8::String>) {
        let error = v8::Exception::error(scope, message);
        scope.throw_exception(error);
    }

    fn has_custom_host_object(&self, _isolate: &v8::Isolate) -> bool {
        !self.objects.is_empty()
    }

    fn is_host_object<'s>(&self, scope: &mut v8::PinScope<'s, '_>, object: v8::Local<'s, v8::Object>) -> Option<bool> {
        Some(self.index_of(scope, object).is_some())
    }

    fn write_host_object<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        object: v8::Local<'s, v8::Object>,
        serializer: &dyn ValueSerializerHelper,
    ) -> Option<bool> {
        serializer.write_uint32(self.index_of(scope, object)?);
        Some(true)
    }
}

struct Deserializer {
    objects: Vec<v8::Global<v8::Object>>,
}

impl v8::ValueDeserializerImpl for Deserializer {
    fn read_host_object<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        deserializer: &dyn ValueDeserializerHelper,
    ) -> Option<v8::Local<'s, v8::Object>> {
        let mut index = 0;
        let object = deserializer.read_uint32(&mut index).then(|| self.objects.get(index as usize)).flatten();
        match object {
            Some(object) => Some(v8::Local::new(scope, object)),
            None => {
                throw_data_clone_error(scope, "the message names a transferred object it doesn't carry");
                None
            }
        }
    }
}

/// Clones `value` and takes what `transfer` lists. `None`: an exception is pending, and nothing
/// was taken.
pub fn serialize<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'_, v8::Value>,
    transfer: v8::Local<'_, v8::Value>,
) -> Option<Message> {
    let value = v8::Local::new(scope, value);
    let transfer = v8::Local::new(scope, transfer);
    let items = transfer_list(scope, transfer)?;
    let mut buffers = Vec::new();
    let mut objects = Vec::new();
    let mut kinds = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if items[..index].iter().any(|seen| seen.strict_equals(*item)) {
            throw_data_clone_error(scope, &format!("value at index {index} is listed twice in the transfer list"));
            return None;
        }
        if let Ok(buffer) = v8::Local::<v8::ArrayBuffer>::try_from(*item) {
            if buffer.was_detached() || !buffer.is_detachable() {
                throw_data_clone_error(scope, &format!("the ArrayBuffer at index {index} can't be transferred"));
                return None;
            }
            buffers.push(buffer);
            continue;
        }
        let kind = match (v8::Local::<v8::Object>::try_from(*item), hooks(scope)) {
            (Ok(object), Some(hooks)) => {
                let kind = call_hook(scope, hooks, "typeOf", &[object.into()])?;
                kind.is_string().then(|| (object, kind))
            }
            _ => None,
        };
        let Some((object, kind)) = kind else {
            throw_data_clone_error(scope, &format!("value at index {index} is not transferable"));
            return None;
        };
        objects.push(object);
        kinds.push(kind);
    }

    let serializer = Serializer {
        objects: objects.iter().map(|object| v8::Global::new(scope, *object)).collect(),
    };
    let context = scope.get_current_context();
    let bytes = {
        let serializer = v8::ValueSerializer::new(scope, Box::new(serializer));
        serializer.write_header();
        if !serializer.write_value(context, value).unwrap_or(false) {
            return None;
        }
        serializer.release()
    };

    let mut transferred = Vec::with_capacity(objects.len());
    if !objects.is_empty() {
        let hooks = hooks(scope)?;
        let kinds_array = v8::Array::new_with_elements(scope, &kinds);
        let values: Vec<v8::Local<v8::Value>> = objects.iter().map(|object| (*object).into()).collect();
        let values_array = v8::Array::new_with_elements(scope, &values);
        let tokens = call_hook(scope, hooks, "detachAll", &[kinds_array.into(), values_array.into()])?;
        let tokens: v8::Local<v8::Array> = tokens.try_into().ok()?;
        for (index, kind) in kinds.iter().enumerate() {
            let token = tokens.get_index(scope, index as u32)?;
            let token = if token.is_number() {
                Token::Number(token.number_value(scope)?)
            } else {
                Token::String(token.to_rust_string_lossy(scope))
            };
            transferred.push(Transferred { kind: kind.to_rust_string_lossy(scope), token });
        }
    }
    for buffer in buffers {
        buffer.detach(None);
    }
    Some(Message { bytes, transferred })
}

/// The message's value in this isolate. `Err`: why it can't be received (what it carried is
/// released).
pub fn deserialize<'s>(scope: &mut v8::PinScope<'s, '_>, message: Message) -> Result<v8::Local<'s, v8::Value>, String> {
    let mut objects = Vec::with_capacity(message.transferred.len());
    if !message.transferred.is_empty() {
        let hooks = hooks(scope).ok_or("DataCloneError: the transfer runtime is missing")?;
        let mut kinds = Vec::with_capacity(message.transferred.len());
        let mut tokens = Vec::with_capacity(message.transferred.len());
        for transferred in &message.transferred {
            let kind = v8::String::new(scope, &transferred.kind).ok_or("DataCloneError")?;
            kinds.push(kind.into());
            tokens.push(match &transferred.token {
                Token::Number(number) => v8::Number::new(scope, *number).into(),
                Token::String(string) => v8::String::new(scope, string).ok_or("DataCloneError")?.into(),
            });
        }
        let kinds = v8::Array::new_with_elements(scope, &kinds);
        let tokens = v8::Array::new_with_elements(scope, &tokens);
        let result = call_hook(scope, hooks, "attachAll", &[kinds.into(), tokens.into()])
            .and_then(|result| v8::Local::<v8::Object>::try_from(result).ok())
            .ok_or("DataCloneError: the transferred objects could not be received")?;
        let key = v8::String::new(scope, "objects").ok_or("DataCloneError")?;
        let attached = result.get(scope, key.into()).and_then(|value| v8::Local::<v8::Array>::try_from(value).ok());
        let Some(attached) = attached else {
            let key = v8::String::new(scope, "error").ok_or("DataCloneError")?;
            let error = result.get(scope, key.into()).map(|error| error.to_rust_string_lossy(scope));
            return Err(error.unwrap_or_else(|| "DataCloneError".into()));
        };
        for index in 0..attached.length() {
            let object = attached
                .get_index(scope, index)
                .and_then(|value| v8::Local::<v8::Object>::try_from(value).ok())
                .ok_or("DataCloneError")?;
            objects.push(v8::Global::new(scope, object));
        }
    }

    let context = scope.get_current_context();
    let deserializer = v8::ValueDeserializer::new(scope, Box::new(Deserializer { objects }), &message.bytes);
    if !deserializer.read_header(context).unwrap_or(false) {
        return Err("DataCloneError: the message could not be read".into());
    }
    deserializer
        .read_value(context)
        .ok_or_else(|| "DataCloneError: the message could not be read".into())
}
