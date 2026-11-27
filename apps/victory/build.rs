fn main() {
    if std::env::var_os("CARGO_FEATURE_AGILITY").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        for symbol in ["D3D12SDKVersion", "D3D12SDKPath"] {
            println!("cargo:rustc-link-arg-bin=victory=/EXPORT:{symbol},DATA");
        }
    }
}
