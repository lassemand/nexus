use crate::{Analyze, Lang, Report, Verdict};

const FALLBACK: &[u8] =
    br#"{"decision":"abstain","reason":"verdict could not be serialized","metrics":[]}"#;

/// Runs `analyze` on `input` and writes the verdict as JSON into `out`.
///
/// `out` is cleared first and its capacity reused. `lang` is a [`Lang::code`].
/// Input that is not UTF-8 gives an abstain verdict, `"input is not UTF-8"`.
pub fn run_into(analyze: Analyze, input: &[u8], lang: u32, out: &mut Vec<u8>) {
    out.clear();
    let verdict = match std::str::from_utf8(input) {
        Ok(text) => analyze(&Report {
            text,
            lang: Lang::from_code(lang),
        }),
        Err(_) => Verdict::abstain("input is not UTF-8"),
    };
    if serde_json::to_writer(&mut *out, &verdict).is_err() {
        out.clear();
        out.extend_from_slice(FALLBACK);
    }
}

/// Exports a kernel's `analyze` function over the WASM ABI.
///
/// Expands to nothing unless the target is `wasm32`. There it exports:
///
/// * `verdict_input(len: u32) -> *mut u8`: resizes a reusable input buffer to
///   `len` bytes and returns where the host writes the release text;
/// * `verdict_run(len: u32, lang: u32) -> u64`: runs the kernel on the first
///   `len` bytes of that buffer and returns `(ptr << 32) | len` of the JSON
///   verdict, which stays valid until the next call.
///
/// The module exports `memory` and imports nothing.
///
/// ```ignore
/// verdict_sdk::export_kernel!(crate::analyze);
/// ```
#[macro_export]
macro_rules! export_kernel {
    ($analyze:path) => {
        #[cfg(target_arch = "wasm32")]
        const _: () = {
            ::std::thread_local! {
                static VERDICT_INPUT: ::core::cell::RefCell<::std::vec::Vec<u8>> =
                    const { ::core::cell::RefCell::new(::std::vec::Vec::new()) };
                static VERDICT_OUTPUT: ::core::cell::RefCell<::std::vec::Vec<u8>> =
                    const { ::core::cell::RefCell::new(::std::vec::Vec::new()) };
            }

            #[no_mangle]
            extern "C" fn verdict_input(len: u32) -> *mut u8 {
                VERDICT_INPUT.with(|input| {
                    let mut input = input.borrow_mut();
                    input.resize(len as usize, 0);
                    input.as_mut_ptr()
                })
            }

            #[no_mangle]
            extern "C" fn verdict_run(len: u32, lang: u32) -> u64 {
                VERDICT_INPUT.with(|input| {
                    VERDICT_OUTPUT.with(|output| {
                        let input = input.borrow();
                        let mut output = output.borrow_mut();
                        let len = (len as usize).min(input.len());
                        let bytes = input.get(..len).unwrap_or(&[]);
                        $crate::run_into($analyze, bytes, lang, &mut output);
                        ((output.as_ptr() as usize as u64) << 32) | output.len() as u64
                    })
                })
            }
        };
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Decision, Metric};

    fn sample(report: &Report) -> Verdict {
        let mut metric = Metric::new("len");
        metric.current = Some(report.text.len().to_string());
        Verdict {
            decision: Decision::Buy,
            reason: format!("lang {}", report.lang.code()),
            metrics: vec![metric],
        }
    }

    #[test]
    fn writes_json_and_reuses_buffer() {
        let mut out = Vec::with_capacity(1024);
        let capacity = out.capacity();
        run_into(sample, b"hello", 2, &mut out);
        assert_eq!(
            std::str::from_utf8(&out).expect("json is UTF-8"),
            r#"{"decision":"buy","reason":"lang 2","metrics":[{"key":"len","current":"5","prior":null,"expected":null,"unit":null}]}"#
        );
        run_into(sample, b"hi", 1, &mut out);
        assert!(out.starts_with(br#"{"decision":"buy","reason":"lang 1""#));
        assert_eq!(out.capacity(), capacity);
    }

    #[test]
    fn invalid_utf8_abstains() {
        let mut out = Vec::new();
        run_into(sample, &[0xff, 0xfe], 1, &mut out);
        assert_eq!(
            std::str::from_utf8(&out).expect("json is UTF-8"),
            r#"{"decision":"abstain","reason":"input is not UTF-8","metrics":[]}"#
        );
    }
}
