use std::{env, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let repository_root = manifest_dir.join("../..");
    let fdk_source = repository_root.join("third_party/fdk-aac");
    if !fdk_source.join("CMakeLists.txt").is_file() {
        panic!(
            "vendored FDK-AAC source is missing at {}",
            fdk_source.display()
        );
    }

    let fdk_install = cmake::Config::new(&fdk_source)
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("BUILD_PROGRAMS", "OFF")
        .define("FDK_AAC_INSTALL_CMAKE_CONFIG_MODULE", "OFF")
        .define("FDK_AAC_INSTALL_PKGCONFIG_MODULE", "OFF")
        .build();
    let mut pipewire_config = pkg_config::Config::new();
    pipewire_config.atleast_version("0.3").cargo_metadata(false);
    let pipewire = pipewire_config
        .probe("libpipewire-0.3")
        .expect("PipeWire development files are required (Ubuntu: libpipewire-0.3-dev)");

    let mut native = cc::Build::new();
    native
        .cpp(true)
        .std("c++20")
        .flag("-pthread")
        .warnings(true)
        .file(manifest_dir.join("native/audio_engine.cpp"))
        .include(manifest_dir.join("native"))
        .include(fdk_source.join("libAACdec/include"));
    for include in &pipewire.include_paths {
        native.include(include);
    }
    native.compile("airpods_audio_native");

    let fdk_lib = if fdk_install.join("lib64").is_dir() {
        fdk_install.join("lib64")
    } else {
        fdk_install.join("lib")
    };
    println!("cargo:rustc-link-search=native={}", fdk_lib.display());
    println!("cargo:rustc-link-lib=static=fdk-aac");
    println!("cargo:rustc-link-lib=m");
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-link-lib=stdc++");
    for path in pipewire.link_paths {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    for library in pipewire.libs {
        println!("cargo:rustc-link-lib={library}");
    }
    println!("cargo:rerun-if-changed=native/audio_engine.cpp");
    println!("cargo:rerun-if-changed=native/audio_engine.h");
    println!("cargo:rerun-if-changed={}", fdk_source.display());
}
