#![no_main]
libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    if let Ok(source) = std::str::from_utf8(bytes)
        && let Ok(document) = vmlab_hv_sail_ir::parse(source)
    {
        let rendered = document.to_string();
        assert_eq!(source, rendered);
        let reparsed = vmlab_hv_sail_ir::parse(&rendered).unwrap();
        assert_eq!(document.definitions, reparsed.definitions);
    }
});
