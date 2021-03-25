#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    use snarkos_network::Version;

    let _ = Version::deserialize(data);
});
