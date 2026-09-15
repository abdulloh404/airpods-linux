//! ตรวจสอบ PipeWire ที่ใช้ตอน link และฝัง runtime search path ให้ daemon พบ library ชุดเดียวกับ build

/// ตรวจสอบว่า development files ของ PipeWire ใหม่พอและส่ง link path ให้ rustc
fn main() {
    let pipewire = pkg_config::Config::new()
        .atleast_version("1.6.8")
        .cargo_metadata(false)
        .probe("libpipewire-0.3")
        .expect("PipeWire 1.6.8 or newer development files are required");

    for path in pipewire.link_paths {
        // ฝังแต่ละ directory เป็น RPATH เพื่อให้ runtime loader ใช้ library ที่ pkg-config เลือก
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", path.display());
    }
}
