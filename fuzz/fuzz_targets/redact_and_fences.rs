#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    agentdesk::input_fuzz::redact_and_fences(data);
});
