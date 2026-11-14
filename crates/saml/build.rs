//! Which names xmlsec1's app calls go by (src/xmlenc.rs): built to load its crypto library at run
//! time (Debian, Ubuntu), they are the generic `xmlSecCrypto…`; else OpenSSL's own. The same test
//! samael's build makes.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(xmlsec_dynamic)");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_XMLSEC").is_none() {
        return;
    }
    let flags = std::process::Command::new("xmlsec1-config").arg("--cflags").output().expect("xmlsec1-config (libxmlsec1's development files)").stdout;
    if String::from_utf8_lossy(&flags).split_whitespace().any(|f| f == "-DXMLSEC_CRYPTO_DYNAMIC_LOADING=1") {
        println!("cargo:rustc-cfg=xmlsec_dynamic");
    }
}
