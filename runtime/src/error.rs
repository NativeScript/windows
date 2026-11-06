use anyhow::Error;
use std::borrow::Cow;
use std::fmt;
use std::fmt::{Display, Formatter};

/// A simple error type that lets the creator specify both the error message and
/// the error class name. This type is private; externally it only ever appears
/// wrapped in an `anyhow::Error`. To retrieve the error class name from a wrapped
/// `CustomError`, use the function `get_custom_error_class()`.
#[derive(Debug)]
struct CustomError {
    class: &'static str,
    message: Cow<'static, str>,
}

impl Display for CustomError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CustomError {}

/// If this error was crated with `custom_error()`, return the specified error
/// class name. In all other cases this function returns `None`.
#[cfg(feature = "classic")]
pub fn get_custom_error_class(error: &Error) -> Option<&'static str> {
    error.downcast_ref::<CustomError>().map(|e| e.class)
}

pub type AnyError = anyhow::Error;

pub fn custom_error(class: &'static str, message: impl Into<Cow<'static, str>>) -> Error {
    CustomError {
        class,
        message: message.into(),
    }
    .into()
}

pub fn generic_error(message: impl Into<Cow<'static, str>>) -> Error {
    custom_error("Error", message)
}

pub fn type_error(message: impl Into<Cow<'static, str>>) -> Error {
    custom_error("TypeError", message)
}

/// The HRESULT a call reports when one of its JS arguments could not be marshaled.
pub(crate) const ARGUMENT_FAILURE: windows::core::HRESULT =
    windows::core::HRESULT(0x8000_4005u32 as i32);

thread_local! {
    static ARGUMENT_ERROR: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Records why an argument could not be marshaled; the call then fails with
/// `ARGUMENT_FAILURE`, and `format_hresult_message` reports this text instead.
pub(crate) fn set_argument_error(message: impl Display) {
    ARGUMENT_ERROR.with(|e| *e.borrow_mut() = Some(message.to_string()));
}

/// Drops a recorded argument error that no caller reported.
#[inline]
pub(crate) fn clear_argument_error() {
    ARGUMENT_ERROR.with(|e| {
        if e.borrow().is_some() {
            *e.borrow_mut() = None;
        }
    });
}

pub(crate) fn take_argument_error() -> Option<String> {
    ARGUMENT_ERROR.with(|e| e.borrow_mut().take())
}

/// Format an HRESULT into a human-readable message using the runtime's
/// provided message text when available. Falls back to a hex HRESULT string.
pub fn format_hresult_message(hr: windows::core::HRESULT) -> String {
    if hr == ARGUMENT_FAILURE {
        if let Some(message) = take_argument_error() {
            return message;
        }
    }
    let s = hr.message();
    if s.is_empty() {
        format!("HRESULT 0x{:08X}", hr.0 as u32)
    } else {
        format!("{} (HRESULT 0x{:08X})", s, hr.0 as u32)
    }
}
