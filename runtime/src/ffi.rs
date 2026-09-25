use crate::error::generic_error;
use crate::value::NativeType;
use crate::value::NativeValue;
use libffi::middle::Arg;
use smallvec::SmallVec;

/// Which argument slots carry an HSTRING handle, resolved once per call.
///
/// WinRT passes HSTRING parameters by value (handle-sized). The `NativeValue` union keeps each
/// string argument in a `ManuallyDrop<HSTRING>` slot that stays alive until `release_string_args`
/// runs after the call, so the handle word is passed straight out of the slot: no clone and no
/// heap storage per call. Empty when the call has no string arguments.
pub struct FfiStringPrep {
    /// `true` for each argument slot whose ABI value is an HSTRING handle.
    pub string_slots: SmallVec<[bool; 12]>,
}

/// Resolve the ABI-effective string slots for a call.
///
/// A slot is a string when its ABI type is `Pointer` and its parse type is `String` (an HSTRING
/// built from a JS string), or when the ABI type is `String` outright.
pub fn prepare_string_storage(
    argument_buf: &[NativeValue],
    parameter_types: &[NativeType],
    argument_parse_types: &[Option<NativeType>],
) -> Result<FfiStringPrep, crate::error::AnyError> {
    let has_strings = argument_parse_types
        .iter()
        .any(|opt| matches!(opt, Some(NativeType::String)));
    if !has_strings {
        return Ok(FfiStringPrep {
            string_slots: SmallVec::new(),
        });
    }

    let mut string_slots: SmallVec<[bool; 12]> = SmallVec::with_capacity(argument_buf.len());
    for i in 0..argument_buf.len() {
        let abi_native = parameter_types
            .get(i)
            .ok_or_else(|| generic_error("missing abi native type for slot"))?;
        let is_string = match abi_native {
            NativeType::String => true,
            NativeType::Pointer => matches!(
                argument_parse_types.get(i),
                Some(Some(NativeType::String))
            ),
            _ => false,
        };
        string_slots.push(is_string);
    }
    Ok(FfiStringPrep { string_slots })
}

/// Construct the libffi argument list for a call.
///
/// String slots pass the HSTRING handle word held in the union; every other slot is a typed
/// reference into `argument_buf` per its ABI type. Inline storage covers the common arities, so
/// building the list does not allocate.
pub fn build_call_args<'a>(
    prep: &FfiStringPrep,
    argument_buf: &'a [NativeValue],
    parameter_types: &'a [NativeType],
) -> SmallVec<[Arg<'a>; 12]> {
    let mut call_args: SmallVec<[Arg<'a>; 12]> = SmallVec::with_capacity(argument_buf.len());
    for (i, v) in argument_buf.iter().enumerate() {
        let abi = if prep.string_slots.get(i).copied().unwrap_or(false) {
            &STRING_ABI
        } else {
            parameter_types.get(i).unwrap_or(&POINTER_FALLBACK_REF)
        };
        call_args.push(unsafe { v.as_arg(abi) });
    }
    call_args
}

static POINTER_FALLBACK_REF: NativeType = NativeType::Pointer;
static STRING_ABI: NativeType = NativeType::String;

/// Release the HSTRINGs the argument loop created for `String` in-parameters.
///
/// A WinRT callee never takes ownership of an in HSTRING (it duplicates the string when it
/// retains it), so the caller-owned handle must be deleted after the call. `NativeValue` is a
/// union, so `Vec::clear` cannot do that; only slots whose parse type is `String` hold a handle.
///
/// # Safety
/// `parse_types[i]` must describe what `buf[i]` holds, which is the invariant the argument
/// loops maintain (they push both together).
pub unsafe fn release_string_args(buf: &mut [NativeValue], parse_types: &[Option<NativeType>]) {
    for (slot, ty) in buf.iter_mut().zip(parse_types) {
        if matches!(ty, Some(NativeType::String)) {
            std::mem::ManuallyDrop::drop(&mut slot.string);
            slot.usize_value = 0;
        }
    }
}
