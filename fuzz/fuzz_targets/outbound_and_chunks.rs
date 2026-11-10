#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    agentdesk::input_fuzz::outbound_and_chunks(data);
});
