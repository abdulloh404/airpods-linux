fn main() {
    let pipewire = pkg_config::Config::new()
        .atleast_version("1.6.8")
        .cargo_metadata(false)
        .probe("libpipewire-0.3")
        .expect("PipeWire 1.6.8 or newer development files are required");

    for path in pipewire.link_paths {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", path.display());
    }
}
