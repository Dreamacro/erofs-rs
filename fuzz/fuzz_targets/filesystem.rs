#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| erofs_fuzz::filesystem(data));
