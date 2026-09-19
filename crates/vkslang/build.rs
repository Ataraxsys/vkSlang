fn main() {
    // Bind references to our own symbols inside libvkslang.so, so exported
    // names like vkGetInstanceProcAddr never resolve to libvulkan's copy when
    // the application links libvulkan directly.
    println!("cargo:rustc-cdylib-link-arg=-Wl,-Bsymbolic");
}
