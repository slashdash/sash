use sash::ffi;

#[test]
fn links_against_libghostty_vt() {
    unsafe {
        let v = std::ffi::CStr::from_ptr(ffi::ghostty_version());
        let v = v.to_str().unwrap();
        assert!(!v.is_empty(), "version string is empty");
        println!("libghostty-vt {v}");
    }
}
